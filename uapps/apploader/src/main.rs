// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! QEMU acceptance apploader. It trusts the CA input and keeps the image in memory.

extern crate alloc;

use std::{cell::RefCell, ffi::CStr, rc::Rc};

use rust_libtipc::{
    ConnectResult, Handle, Manager, MessageResult, PortCfg, UnbufferedService, Uuid,
};

const PUBLIC_SERVICE: &str = "com.android.trusty.apploader";
const SECURE_SERVICE: &CStr = c"com.android.trusty.apploader.secure";

const VERSION: u32 = 1;
const SECURE_CMD_LOAD: u8 = 0;
const CMD_BEGIN: u32 = 0;
const CMD_DATA: u32 = 2;
const CMD_COMMIT: u32 = 4;

const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

// Keep one DATA message plus its 12-byte header within TIPC's 8 KiB slot.
const DATA_BYTES: usize = 4 * 1024;
const MESSAGE_BYTES: usize = DATA_BYTES + 12;

const STATUS_OK: u32 = 0;
const STATUS_INVALID: u32 = 1;
const STATUS_TOO_LARGE: u32 = 2;
const STATUS_NO_MEMORY: u32 = 3;
const STATUS_LOAD_FAILED: u32 = 6;

// 081ba88f-f1ee-452e-b5e8-a7e9ef173a97; must match the installed .ta filename.
#[used]
#[unsafe(link_section = ".ta_head")]
static TA_HEAD: [u8; 32] = [
    0x8f, 0xa8, 0x1b, 0x08, 0xee, 0xf1, 0x2e, 0x45, 0xb5, 0xe8, 0xa7, 0xe9, 0xef, 0x17, 0x3a, 0x97,
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
];

struct Upload {
    length_bytes: usize,
    digest: [u8; 32],
    image: Vec<u8>,
}

struct ApploaderService;

impl UnbufferedService for ApploaderService {
    type Connection = RefCell<Option<Upload>>;

    fn on_connect(
        &self,
        _port: &PortCfg,
        _channel: &Handle,
        _peer: &Uuid,
    ) -> rust_libtipc::Result<ConnectResult<Self::Connection>> {
        Ok(ConnectResult::Accept(RefCell::new(None)))
    }

    fn on_message(
        &self,
        session: &Self::Connection,
        channel: &Handle,
        buffer: &mut [u8],
    ) -> rust_libtipc::Result<MessageResult> {
        let count = channel.recv_into(buffer)?;
        if count < 4 {
            return Ok(MessageResult::CloseConnection);
        }
        let message = &buffer[..count];
        let command = read_u32(message, 0).unwrap();
        let (status, close) = match command {
            CMD_BEGIN => (begin(session, message), false),
            CMD_DATA => (append(session, message), false),
            CMD_COMMIT if count == 4 => {
                let upload = session.borrow_mut().take();
                let status = match upload {
                    Some(upload) if upload.image.len() == upload.length_bytes => {
                        match forward_to_kernel(&upload) {
                            Ok(status) => status,
                            Err(error) => {
                                eprintln!("apploader: secure connection failed: {error:?}");
                                STATUS_LOAD_FAILED
                            }
                        }
                    }
                    _ => STATUS_INVALID,
                };
                (status, true)
            }
            _ => (STATUS_INVALID, true),
        };
        respond(channel, command, status)?;
        Ok(if close || status != STATUS_OK {
            MessageResult::CloseConnection
        } else {
            MessageResult::MaintainConnection
        })
    }

    fn max_message_length(&self) -> usize {
        MESSAGE_BYTES
    }
}

fn begin(session: &RefCell<Option<Upload>>, message: &[u8]) -> u32 {
    if session.borrow().is_some() || message.len() != 48 || read_u32(message, 4) != Some(VERSION) {
        return STATUS_INVALID;
    }
    let Some(length) = read_u64(message, 8).and_then(|length| usize::try_from(length).ok()) else {
        return STATUS_TOO_LARGE;
    };
    if length == 0 || length > MAX_IMAGE_BYTES {
        return STATUS_TOO_LARGE;
    }
    let mut image = Vec::new();
    if image.try_reserve_exact(length).is_err() {
        return STATUS_NO_MEMORY;
    }
    let mut digest = [0; 32];
    digest.copy_from_slice(&message[16..48]);
    *session.borrow_mut() = Some(Upload {
        length_bytes: length,
        digest,
        image,
    });
    STATUS_OK
}

fn append(session: &RefCell<Option<Upload>>, message: &[u8]) -> u32 {
    if message.len() <= 12 || message.len() > MESSAGE_BYTES {
        return STATUS_INVALID;
    }
    let mut session = session.borrow_mut();
    let Some(upload) = session.as_mut() else {
        return STATUS_INVALID;
    };
    if read_u64(message, 4) != Some(upload.image.len() as u64) {
        return STATUS_INVALID;
    }
    let data = &message[12..];
    if data.len() > upload.length_bytes - upload.image.len() {
        return STATUS_TOO_LARGE;
    }
    upload.image.extend_from_slice(data);
    STATUS_OK
}

fn forward_to_kernel(upload: &Upload) -> rust_libtipc::Result<u32> {
    let segment = match SharedSegment::new(upload.length_bytes) {
        Ok(segment) => segment,
        Err(status) => return Ok(status),
    };
    // SAFETY: COMMIT only calls `forward_to_kernel` after checking
    // `upload.image.len() == upload.length_bytes`, and `new` created the
    // segment with that same `length_bytes` capacity.
    unsafe { segment.copy_from_slice(&upload.image) };

    let channel = Handle::connect(SECURE_SERVICE)?;
    let mut request = [0; 41];
    // 41-byte kernel handoff: u8 command, u32 shmid, u32 image length, and
    // 32-byte SHA-256.
    request[0] = SECURE_CMD_LOAD;
    // `SharedSegment::new` rejects negative shmids, and `begin` bounds the
    // image length to 16 MiB, so both values fit their wire fields.
    request[1..5].copy_from_slice(&(segment.shmid as u32).to_le_bytes());
    request[5..9].copy_from_slice(&(upload.length_bytes as u32).to_le_bytes());
    request[9..41].copy_from_slice(&upload.digest);
    channel.send_vectored(&[&request], &[])?;
    receive_status(&channel, u32::from(SECURE_CMD_LOAD))
}

struct SharedSegment {
    shmid: i32,
    addr: *mut u8,
}

impl SharedSegment {
    fn new(image_len: usize) -> Result<Self, u32> {
        // SAFETY: `begin` rejected a zero length and values above
        // `MAX_IMAGE_BYTES`, and required `usize::try_from` to succeed for the
        // u64 wire value, so `image_len` is a non-zero, bounded, valid `size_t`
        // byte count for `shmget`. `IPC_PRIVATE` is the valid SysV key for a
        // private segment, and `IPC_CREAT | 0o600` is a valid creation flag and
        // mode combination; `shmget` reports failure through its return value.
        let shmid = unsafe { libc::shmget(libc::IPC_PRIVATE, image_len, libc::IPC_CREAT | 0o600) };
        if shmid < 0 {
            let error = std::io::Error::last_os_error();
            eprintln!("apploader: shared segment creation failed: {error}");
            return Err(if error.raw_os_error() == Some(libc::ENOMEM) {
                STATUS_NO_MEMORY
            } else {
                STATUS_LOAD_FAILED
            });
        }
        // SAFETY: `shmid` was returned by `shmget`; a null address lets the
        // kernel choose the mapping address.
        let addr = unsafe { libc::shmat(shmid, std::ptr::null(), 0) };
        if addr == (-1_isize as *mut libc::c_void) {
            eprintln!("apploader: shared segment attachment failed: {}", std::io::Error::last_os_error());
            // SAFETY: remove the segment created above after attach failed.
            unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) };
            return Err(STATUS_LOAD_FAILED);
        }
        // SAFETY: the segment remains alive while this process is attached;
        // marking it for removal ensures crash-time cleanup after detach.
        if unsafe { libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut()) } != 0 {
            eprintln!("apploader: shared segment removal failed: {}", std::io::Error::last_os_error());
            // SAFETY: `addr` is the attachment returned by `shmat` above.
            unsafe { libc::shmdt(addr) };
            return Err(STATUS_LOAD_FAILED);
        }
        Ok(Self { shmid, addr: addr.cast() })
    }

    /// Copies `src` into the mapped shared-memory region.
    ///
    /// # Safety
    ///
    /// `src.len()` must not exceed the segment size passed to
    /// [`SharedSegment::new`]. The caller is responsible for establishing that
    /// bound before copying.
    unsafe fn copy_from_slice(&self, src: &[u8]) {
        // SAFETY: `new` returned a successful `shmat` result for a segment
        // created with the size used by the caller's bound. The caller
        // establishes that bound from COMMIT's
        // `upload.image.len() == upload.length_bytes` check. The segment is
        // created with the same `upload.length_bytes` value.
        // The segment remains attached while this shared borrow of `self`
        // exists; `src` and `addr` are distinct allocations, and `u8` has
        // alignment 1.
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), self.addr, src.len());
        }
    }
}

impl Drop for SharedSegment {
    fn drop(&mut self) {
        // SAFETY: `addr` is the successful `shmat` result and is no longer
        // accessed after this guard is dropped.
        if unsafe { libc::shmdt(self.addr.cast()) } != 0 {
            eprintln!("apploader: shared segment detach failed");
        }
    }
}

fn receive_status(channel: &Handle, command: u32) -> rust_libtipc::Result<u32> {
    let mut response = [0; 8];
    let (count, handles) = channel.recv_vectored(&mut [&mut response], &mut [])?;
    if count != response.len() || handles != 0 || read_u32(&response, 0) != Some(command | 1) {
        return Ok(STATUS_LOAD_FAILED);
    }
    Ok(read_u32(&response, 4).unwrap())
}

fn respond(channel: &Handle, command: u32, status: u32) -> rust_libtipc::Result<()> {
    let mut response = [0; 8];
    response[..4].copy_from_slice(&(command | 1).to_le_bytes());
    response[4..].copy_from_slice(&status.to_le_bytes());
    channel.send_vectored(&[&response], &[])
}

fn read_u32(message: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        message.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u64(message: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        message.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn main() {
    if let Err(error) = run() {
        eprintln!("apploader: {error:?}");
        std::process::exit(1);
    }
}

fn run() -> rust_libtipc::Result<()> {
    let port = PortCfg::new(PUBLIC_SERVICE)?
        .allow_ns_connect()
        .msg_queue_len(4)
        .msg_max_size(MESSAGE_BYTES as u32);
    rust_libtipc::service_dispatcher! {
        pub(crate) enum ApploaderDispatcher {
            ApploaderService,
        }
    }
    let mut dispatcher = ApploaderDispatcher::<1>::new()?;
    dispatcher.add_service(Rc::new(ApploaderService), port)?;
    let manager =
        Manager::<_, Vec<u8>, 1, 8>::new_with_dispatcher(dispatcher, vec![0; MESSAGE_BYTES])?;
    println!("apploader: ready ({PUBLIC_SERVICE})");
    manager.run_event_loop()
}
