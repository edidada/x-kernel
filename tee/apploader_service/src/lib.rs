// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! SysV shared-memory handoff for dynamic TA images.
//!
//! The protocol uses one 41-byte TIPC control message: an 8-bit command, the
//! SysV shared-memory ID. The apploader fills the segment before
//! sending the message. The kernel copies and hashes the image in a private
//! buffer before launching. This is a handoff check, not TA signature
//! verification.

#![no_std]

extern crate alloc;

use alloc::{string::String, sync::Arc, vec::Vec};
use core::{future::poll_fn, mem::size_of, task::Poll};

use kerrno::{KError, KResult};
use kpoll::{PollRegisterError, PollRegistrations};
use ktime_types::TimeSpan;
use log::warn;
use posix_ipc::read_shm_segment;
use sha2::{Digest, Sha256};
use tipc::{
    Handle, HandleEventMask, IpcChan, IpcPort, IpcPortFlags, IpcUuid, ipc_port_create,
    ipc_port_publish,
};
use zerocopy::{FromBytes, Immutable, KnownLayout, little_endian::U32};

// The reserved name of the kernel-owned port for the secure apploader service.
const PORT_NAME: &str = "com.android.trusty.apploader.secure";
// UUID of the preinstalled Trusty apploader TA.
const APPLOADER_TA_UUID: IpcUuid = IpcUuid::from_bytes([
    0x08, 0x1b, 0xa8, 0x8f, 0xf1, 0xee, 0x45, 0x2e, 0xb5, 0xe8, 0xa7, 0xe9, 0xef, 0x17, 0x3a, 0x97,
]);
// The only accepted command: load.
const CMD_LOAD: u8 = 0;
// Fixed wire length of one LOAD request, in bytes.
const LOAD_LEN: usize = 41;
// Maximum accepted TA image size, in bytes.
const MAX_IMAGE_BYTES: u32 = 16 * 1024 * 1024;
// Receive-slot size for one control message, in bytes.
const CHANNEL_BUF_LEN: usize = LOAD_LEN;
const MESSAGE_TIMEOUT: TimeSpan = TimeSpan::from_millis(30_000);

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Status {
    Ok             = 0,
    Invalid        = 1,
    TooLarge       = 2,
    NoMemory       = 3,
    DigestMismatch = 4,
    Unsupported    = 5,
    LoadFailed     = 6,
}

type LaunchTa = fn(Vec<u8>) -> KResult;

/// One validated LOAD control request and its 41-byte wire layout.
#[repr(C)]
#[derive(FromBytes, Immutable, KnownLayout)]
struct LoadRequest {
    command: u8,
    shmid: U32,
    image_len: U32,
    expected_digest: [u8; 32],
}

const _: () = assert!(size_of::<LoadRequest>() == LOAD_LEN);

impl LoadRequest {
    /// Decodes and validates one LOAD frame.
    fn parse(message: &[u8]) -> Result<Self, Status> {
        let request = Self::read_from_bytes(message).map_err(|_| Status::Invalid)?;
        if request.command != CMD_LOAD {
            return Err(Status::Invalid);
        }
        let image_len = request.image_len.get();
        if image_len == 0 || image_len > MAX_IMAGE_BYTES {
            return Err(Status::TooLarge);
        }
        Ok(request)
    }

    fn copy_and_check(&self) -> Result<Vec<u8>, Status> {
        let image_len = usize::try_from(self.image_len.get()).map_err(|_| Status::TooLarge)?;
        let shmid = i32::try_from(self.shmid.get()).map_err(|_| Status::Invalid)?;
        let mut image = Vec::new();
        image
            .try_reserve_exact(image_len)
            .map_err(|_| Status::NoMemory)?;
        image.resize(image_len, 0);
        read_shm_segment(shmid, &mut image).map_err(|_| Status::Invalid)?;
        if Sha256::digest(&image).as_slice() != self.expected_digest {
            return Err(Status::DigestMismatch);
        }
        Ok(image)
    }
}

fn wait_event(handle: &dyn Handle, wanted: HandleEventMask, timeout: Option<TimeSpan>) -> KResult {
    let mut registrations = PollRegistrations::new();
    let wait = poll_fn(|cx| {
        let events = handle.poll(true);
        if events.contains(wanted) {
            return Poll::Ready(Ok(()));
        }
        if events.intersects(HandleEventMask::HUP | HandleEventMask::ERROR) {
            return Poll::Ready(Err(KError::NotConnected));
        }
        let mut context = registrations.context(cx);
        if let Err(err) = handle.register(&mut context, wanted) {
            return Poll::Ready(Err(match err {
                PollRegisterError::NoMemory | PollRegisterError::IdExhausted => KError::NoMemory,
                PollRegisterError::InvalidState => KError::BadState,
            }));
        }
        drop(context);
        let events = handle.poll(true);
        if events.contains(wanted) {
            Poll::Ready(Ok(()))
        } else if events.intersects(HandleEventMask::HUP | HandleEventMask::ERROR) {
            Poll::Ready(Err(KError::NotConnected))
        } else {
            Poll::Pending
        }
    });
    ktask::future::block_on(ktask::future::timeout(timeout, wait)).map_err(KError::from)?
}

fn receive_load_request(channel: &IpcChan) -> KResult<LoadRequest> {
    loop {
        match channel.ipc_get_msg() {
            Ok(info) => {
                let message = (|| {
                    if info.num_handles != 0 || info.len == 0 || info.len > CHANNEL_BUF_LEN {
                        return Err(KError::InvalidInput);
                    }
                    let mut message = Vec::new();
                    message
                        .try_reserve_exact(info.len)
                        .map_err(|_| KError::NoMemory)?;
                    message.resize(info.len, 0);
                    let copied = channel.ipc_read_msg(info.id, 0, &mut message)?;
                    if copied != info.len {
                        return Err(KError::InvalidInput);
                    }
                    Ok(message)
                })();
                channel.ipc_put_msg(info.id)?;
                let message = message?;
                let request = match LoadRequest::parse(&message) {
                    Ok(request) => request,
                    Err(status) => {
                        respond(channel, message[0], status)?;
                        return Err(KError::InvalidInput);
                    }
                };
                return Ok(request);
            }
            Err(KError::WouldBlock) => {
                wait_event(channel, HandleEventMask::MSG, Some(MESSAGE_TIMEOUT))?;
            }
            Err(err) => return Err(err),
        }
    }
}

fn send_response(channel: &IpcChan, response: &[u8]) -> KResult {
    loop {
        match channel.ipc_send_msg(response) {
            Ok(sent) if sent == response.len() => return Ok(()),
            Ok(_) => return Err(KError::BadState),
            Err(KError::WouldBlock) => {
                wait_event(
                    channel,
                    HandleEventMask::SEND_UNBLOCKED,
                    Some(MESSAGE_TIMEOUT),
                )?;
            }
            Err(err) => return Err(err),
        }
    }
}

fn respond(channel: &IpcChan, command: u8, status: Status) -> KResult {
    let mut response = [0; 8];
    response[..4].copy_from_slice(&(u32::from(command) | 1).to_le_bytes());
    response[4..].copy_from_slice(&(status as u32).to_le_bytes());
    send_response(channel, &response)
}

fn serve_channel(channel: &IpcChan, launch: LaunchTa) -> KResult {
    let request = receive_load_request(channel)?;
    let image = match request.copy_and_check() {
        Ok(image) => image,
        Err(status) => {
            respond(channel, request.command, status)?;
            return Ok(());
        }
    };
    let status = match launch(image) {
        Ok(()) => Status::Ok,
        Err(KError::InvalidExecutable | KError::InvalidData | KError::InvalidInput) => {
            Status::Invalid
        }
        Err(KError::NoMemory) => Status::NoMemory,
        Err(KError::FileTooLarge) => Status::TooLarge,
        Err(KError::Unsupported) => Status::Unsupported,
        Err(err) => {
            warn!("apploader secure TA launch failed: {err:?}");
            Status::LoadFailed
        }
    };
    respond(channel, request.command, status)?;
    Ok(())
}

fn run(port: Arc<IpcPort>, launch: LaunchTa) {
    loop {
        match port.ipc_port_accept() {
            Ok((channel, peer_uuid)) => {
                if peer_uuid != APPLOADER_TA_UUID {
                    channel.close();
                    continue;
                }
                if let Err(err) = serve_channel(&channel, launch) {
                    warn!("apploader secure session ended: {err:?}");
                }
                channel.close();
            }
            Err(KError::WouldBlock) => {
                if let Err(err) = wait_event(port.as_ref(), HandleEventMask::READY, None) {
                    warn!("apploader secure port wait failed: {err:?}");
                    ktask::sleep(TimeSpan::from_millis(10));
                }
            }
            Err(err) => warn!("apploader secure accept failed: {err:?}"),
        }
    }
}

/// Publishes the kernel service before PID 1 and starts its single receiver task.
///
/// This function may allocate and spawn a task; call it only from sleepable
/// late initialization. Connections from other TAs are closed before any
/// upload message is handled.
///
/// Without the `dynamic_apploader` feature this is a no-op: the feature is
/// forwarded from `kfeat` when the matching Kconfig symbol is selected, so
/// builds without the bridge never publish `.secure`.
///
/// # Panics
///
/// Panics if the kernel-owned `.secure` port cannot be created or published.
/// PID 1 must not start without the reserved service name, or a user-space TA
/// could publish the same name and impersonate the kernel service.
#[expect(
    clippy::panic,
    reason = "CG-021-no-panic-on-reachable-failure: Fatal boot: PID 1 must not start without the \
              reserved `.secure` port, or a user TA could publish the same name and impersonate \
              the kernel service"
)]
pub fn start(launch: LaunchTa) {
    if cfg!(feature = "dynamic_apploader")
        && let Err(err) = try_start(launch)
    {
        panic!("failed to start apploader secure receiver: {err:?}");
    }
}

fn try_start(launch: LaunchTa) -> KResult {
    let port = ipc_port_create(
        IpcUuid::default(),
        String::from(PORT_NAME),
        4,
        CHANNEL_BUF_LEN,
        IpcPortFlags::ALLOW_TA_CONNECT,
    )?;
    ipc_port_publish(&port)?;
    ktask::spawn_with_name(move || run(port, launch), String::from("apploader-secure"));
    Ok(())
}
