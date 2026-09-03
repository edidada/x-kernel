// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux `inotify` object support.
//!
//! This implementation accepts watch registration and removal and preserves
//! the Linux `inotify_event` read ABI, but does not yet generate filesystem
//! events; watches only record their target and mask. This is sufficient for
//! user-space daemons that probe inotify availability before falling back to
//! polling.

use alloc::{collections::VecDeque, string::String, sync::Arc, vec::Vec};
use core::mem;

use kcred::Cred;
use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::Mutex;
use ktask::future::{block_on, poll_io};
use kvfs::{AnonInodeFs, FMode, FileOperations, OpenFlags, VfsFile, VfsInode};

/// A single queued `struct inotify_event` with its optional name tail.
struct QueuedEvent {
    wd: i32,
    mask: u32,
    cookie: u32,
    name: Option<Vec<u8>>,
}

/// One registered watch descriptor.
struct Watch {
    wd: i32,
    #[allow(dead_code)]
    mask: u32,
    #[allow(dead_code)]
    pathname: String,
}

/// Size of the fixed `struct inotify_event` header (without the name tail).
const INOTIFY_EVENT_HEADER_SIZE: usize = mem::size_of::<i32>() + mem::size_of::<u32>() * 3;

/// An inotify instance.
pub struct Inotify {
    watches: Mutex<Vec<Watch>>,
    next_wd: Mutex<i32>,
    events: Mutex<VecDeque<QueuedEvent>>,
    poll_rx: PollSet,
}

impl Inotify {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            watches: Mutex::new(Vec::new()),
            next_wd: Mutex::new(1),
            events: Mutex::new(VecDeque::new()),
            poll_rx: PollSet::new(),
        })
    }

    /// Creates an inotify file and captures `cred` as its open credential.
    pub fn new_file(open_flags: u32, cred: Arc<Cred>) -> KResult<Arc<VfsFile>> {
        let open_flags = OpenFlags::from_bits(open_flags).ok_or(KError::InvalidInput)?;
        AnonInodeFs::global().get_file(
            "[inotify]",
            Arc::new(InotifyFops),
            Self::new(),
            FMode::READ | FMode::STREAM,
            open_flags,
            cred,
        )
    }

    /// Returns the inotify object attached to an inotify file.
    pub fn from_file(file: &VfsFile) -> KResult<Arc<Self>> {
        file.private_data_get::<Self>()
            .ok_or(KError::BadFileDescriptor)
    }

    /// Adds a watch on `pathname` and returns its watch descriptor.
    ///
    /// A duplicate watch on the same path replaces the mask and returns the
    /// existing descriptor, matching Linux `inotify_add_watch` semantics.
    pub fn add_watch(&self, pathname: &str, mask: u32) -> KResult<i32> {
        let mut watches = self.watches.lock();
        if let Some(watch) = watches.iter_mut().find(|w| w.pathname == pathname) {
            watch.mask = mask;
            return Ok(watch.wd);
        }
        let mut next_wd = self.next_wd.lock();
        let wd = *next_wd;
        *next_wd = next_wd.wrapping_add(1);
        watches.push(Watch {
            wd,
            mask,
            pathname: String::from(pathname),
        });
        Ok(wd)
    }

    /// Removes the watch identified by `wd`.
    ///
    /// Queues an `IN_IGNORED` event first, like Linux does before releasing
    /// the watch.
    pub fn rm_watch(&self, wd: i32) -> KResult<()> {
        let mut watches = self.watches.lock();
        let Some(pos) = watches.iter().position(|w| w.wd == wd) else {
            return Err(KError::InvalidInput);
        };
        watches.remove(pos);
        drop(watches);
        self.queue_event(QueuedEvent {
            wd,
            mask: linux_raw_sys::general::IN_IGNORED,
            cookie: 0,
            name: None,
        });
        Ok(())
    }

    fn queue_event(&self, event: QueuedEvent) {
        self.events.lock().push_back(event);
        self.poll_rx.wake();
    }

    fn pop_event(&self, buf: &mut [u8]) -> KResult<usize> {
        let Some(event) = self.events.lock().pop_front() else {
            return Err(KError::WouldBlock);
        };
        let name_len = event.name.as_ref().map_or(0, |n| n.len());
        let total = INOTIFY_EVENT_HEADER_SIZE + name_len;
        if buf.len() < total {
            // Linux requires the whole event to fit; it would have been
            // reported through EINVAL at read time for a short buffer.
            return Err(KError::InvalidInput);
        }
        let (header, tail) = buf.split_at_mut(INOTIFY_EVENT_HEADER_SIZE);
        header[..4].copy_from_slice(&event.wd.to_ne_bytes());
        header[4..8].copy_from_slice(&event.mask.to_ne_bytes());
        header[8..12].copy_from_slice(&event.cookie.to_ne_bytes());
        header[12..16].copy_from_slice(&(name_len as u32).to_ne_bytes());
        if let Some(name) = &event.name {
            tail.copy_from_slice(name);
        }
        Ok(total)
    }
}

struct InotifyFops;

impl FileOperations for InotifyFops {
    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], _offset: u64) -> KResult<usize> {
        let inotify = Inotify::from_file(file)?;
        block_on(poll_io(
            inotify.as_ref(),
            IoEvents::IN,
            file.is_nonblocking(),
            || inotify.pop_event(buf),
        ))
    }

    fn write(&self, _file: &VfsFile, _buf: &[u8], _offset: u64) -> KResult<usize> {
        Err(KError::BadFileDescriptor)
    }

    fn release(&self, _inode: &VfsInode, _file: &VfsFile) -> KResult<()> {
        Ok(())
    }

    fn poll(&self, file: &VfsFile) -> IoEvents {
        let mut events = IoEvents::OUT;
        if let Ok(inotify) = Inotify::from_file(file)
            && !inotify.events.lock().is_empty()
        {
            events |= IoEvents::IN;
        }
        events
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        let Ok(inotify) = Inotify::from_file(file) else {
            return Ok(());
        };
        if events.intersects(IoEvents::IN | IoEvents::RDNORM) {
            context.register(&inotify.poll_rx)?;
        }
        Ok(())
    }
}

impl Pollable for Inotify {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::OUT;
        events.set(IoEvents::IN, !self.events.lock().is_empty());
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.intersects(IoEvents::IN | IoEvents::RDNORM) {
            context.register(&self.poll_rx)?;
        }
        Ok(())
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn test_add_watch_returns_incrementing_wds() {
        let inotify = Inotify::new();
        let wd1 = inotify.add_watch("/tmp", 0xfff).unwrap();
        let wd2 = inotify.add_watch("/var", 0xfff).unwrap();
        assert_eq!(wd1, 1);
        assert_eq!(wd2, 2);
    }

    #[def_test]
    fn test_duplicate_watch_reuses_wd() {
        let inotify = Inotify::new();
        let wd1 = inotify.add_watch("/tmp", 0xfff).unwrap();
        let wd2 = inotify.add_watch("/tmp", 0x100).unwrap();
        assert_eq!(wd1, wd2);
    }

    #[def_test]
    fn test_rm_watch_queues_in_ignored() {
        let inotify = Inotify::new();
        let wd = inotify.add_watch("/tmp", 0xfff).unwrap();
        inotify.rm_watch(wd).unwrap();

        let mut buf = [0u8; INOTIFY_EVENT_HEADER_SIZE];
        let count = inotify.pop_event(&mut buf).unwrap();
        assert_eq!(count, INOTIFY_EVENT_HEADER_SIZE);
        let mut mask = [0u8; 4];
        mask.copy_from_slice(&buf[4..8]);
        assert_eq!(u32::from_ne_bytes(mask), linux_raw_sys::general::IN_IGNORED);
    }

    #[def_test]
    fn test_rm_watch_rejects_unknown_wd() {
        let inotify = Inotify::new();
        assert!(inotify.rm_watch(42).is_err());
    }
}
