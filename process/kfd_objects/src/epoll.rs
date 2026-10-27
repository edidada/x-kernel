// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Epoll instance and interest management.

use alloc::{
    collections::vec_deque::VecDeque,
    sync::{Arc, Weak},
    task::Wake,
    vec::Vec,
};
use core::{
    hash::{Hash, Hasher},
    mem,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::{Context, Waker},
};

use anon_inodefs::AnonInodeFs;
use bitflags::bitflags;
use hashbrown::HashMap;
use kcred::Cred;
use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollContext, PollRegisterError, PollRegistrations, PollSet, Pollable};
use kspin::{SpinNoIrq, SpinNoPreempt};
use ksync::Mutex;
use kvfs::{FMode, FileOperations, FilePollSource, OpenFlags, VfsFile, VfsInode};
use linux_raw_sys::general::{EPOLLET, EPOLLONESHOT};
use posix_types::k_epoll_event;

/// A ready event returned by an [`Epoll`] instance.
#[derive(Clone, Copy)]
pub struct EpollEvent {
    /// Interested I/O events.
    pub events: IoEvents,
    /// User data associated with the interest.
    pub user_data: u64,
}

bitflags! {
    /// Flags for entries in an `epoll` instance.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct EpollFlags: u32 {
        /// Rearm notification through source transitions rather than retained level readiness.
        const EDGE_TRIGGER = EPOLLET;
        /// Disable an interest after one event until MOD rearms it.
        const ONESHOT = EPOLLONESHOT;
    }
}

/// Interest trigger mode.
#[derive(Debug, Clone, Copy)]
enum TriggerMode {
    /// Level-triggered: until the condition is cleared.
    Level,
    /// Edge-triggered: consume notifications without retaining level readiness.
    Edge,
    /// One-shot: notify only once.
    OneShot { fired: bool },
}

impl TriggerMode {
    fn from_flags(flags: EpollFlags) -> Self {
        if flags.contains(EpollFlags::ONESHOT) {
            TriggerMode::OneShot { fired: false }
        } else if flags.contains(EpollFlags::EDGE_TRIGGER) {
            TriggerMode::Edge
        } else {
            TriggerMode::Level
        }
    }

    fn should_notify(&self) -> (bool, Self) {
        match self {
            TriggerMode::Level => (true, *self),
            TriggerMode::Edge => (true, TriggerMode::Edge),
            TriggerMode::OneShot { fired } => {
                if *fired {
                    (false, *self)
                } else {
                    (true, TriggerMode::OneShot { fired: true })
                }
            }
        }
    }

    fn is_enabled(&self) -> bool {
        match self {
            TriggerMode::OneShot { fired } => !fired,
            _ => true,
        }
    }
}

enum ConsumeResult {
    /// Return an event and keep the interest queued for another level-triggered poll.
    EventAndKeep(EpollEvent),
    /// Return an event without retaining its level readiness.
    EventAndRemove(EpollEvent),
    /// Return no user-visible event.
    NoEvent,
}

#[derive(Clone, Copy)]
struct EpollInterestConfig {
    event: EpollEvent,
    mode: TriggerMode,
}

impl EpollInterestConfig {
    fn new(event: EpollEvent, flags: EpollFlags) -> Self {
        Self {
            event,
            mode: TriggerMode::from_flags(flags),
        }
    }
}

fn match_ready_events(current: IoEvents, interested: IoEvents) -> IoEvents {
    (current & interested) | (current & IoEvents::ALWAYS_POLL)
}

fn register_events(interested: IoEvents) -> IoEvents {
    interested | IoEvents::ALWAYS_POLL
}

fn map_register_error(error: PollRegisterError) -> KError {
    match error {
        PollRegisterError::NoMemory | PollRegisterError::IdExhausted => KError::NoMemory,
        PollRegisterError::InvalidState => KError::InvalidInput,
    }
}

#[derive(Clone)]
struct EntryKey {
    fd: i32,
    file: Weak<VfsFile>,
}

impl EntryKey {
    fn new(fd: i32, file: &Arc<VfsFile>) -> Self {
        Self {
            fd,
            file: Arc::downgrade(file),
        }
    }

    #[inline]
    fn get_file(&self) -> Option<Arc<VfsFile>> {
        self.file.upgrade()
    }
}

impl Hash for EntryKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (self.fd, self.file.as_ptr()).hash(state);
    }
}

impl PartialEq for EntryKey {
    fn eq(&self, other: &Self) -> bool {
        self.fd == other.fd && Weak::ptr_eq(&self.file, &other.file)
    }
}

impl Eq for EntryKey {}

struct EpollInterest {
    key: EntryKey,
    // Captured at ADD even when ready or not interested in the source's events.
    // An expired Some remains bound; MOD and rearm must never recapture it.
    poll_source: Option<FilePollSource>,
    // Serializes consume/rearm with MOD so an older registration cannot
    // replace a newer generation's owner. Source callbacks never take it.
    operation_lock: Mutex<()>,
    config: SpinNoPreempt<EpollInterestConfig>,
    // Sleepable: held only while swapping the owner, not across file poll hooks.
    // Old `PollRegistrations` must be dropped outside this lock so source
    // `SpinNoIrq` unregister work does not nest under the Mutex.
    registrations: Mutex<PollRegistrations>,
    in_ready_queue: AtomicBool,
    waker_generation: AtomicUsize,
}

impl EpollInterest {
    fn new(
        key: EntryKey,
        event: EpollEvent,
        flags: EpollFlags,
        poll_source: Option<FilePollSource>,
    ) -> Self {
        Self {
            key,
            poll_source,
            operation_lock: Mutex::new(()),
            config: SpinNoPreempt::new(EpollInterestConfig::new(event, flags)),
            registrations: Mutex::new(PollRegistrations::new()),
            in_ready_queue: AtomicBool::new(false),
            waker_generation: AtomicUsize::new(0),
        }
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        match &self.poll_source {
            Some(source) => source.register(context, events),
            None => file.register_poll(context, events),
        }
    }

    #[inline]
    fn is_enabled(&self) -> bool {
        self.config.lock().mode.is_enabled()
    }

    #[inline]
    fn event_snapshot(&self) -> EpollEvent {
        self.config.lock().event
    }

    #[inline]
    fn interested_events(&self) -> IoEvents {
        self.event_snapshot().events
    }

    #[inline]
    fn registered_events(&self) -> IoEvents {
        register_events(self.interested_events())
    }

    fn modify_config(&self, event: EpollEvent, flags: EpollFlags) -> EpollInterestConfig {
        mem::replace(
            &mut *self.config.lock(),
            EpollInterestConfig::new(event, flags),
        )
    }

    fn restore_config(&self, snapshot: EpollInterestConfig) {
        *self.config.lock() = snapshot;
    }

    #[inline]
    fn is_in_queue(&self) -> bool {
        self.in_ready_queue.load(Ordering::Acquire)
    }

    #[inline]
    fn try_mark_in_queue(&self) -> bool {
        self.in_ready_queue
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    #[inline]
    fn mark_not_in_queue(&self) {
        self.in_ready_queue.store(false, Ordering::Release);
    }

    /// Takes ownership of any live source registrations, releasing the Mutex
    /// before the returned value is dropped (and sources are unregistered).
    fn take_registrations(&self) -> PollRegistrations {
        let mut owned = self.registrations.lock();
        mem::take(&mut *owned)
    }

    /// Installs `registrations` and returns the previous owner so it can be
    /// dropped after the Mutex is released.
    fn replace_registrations(&self, registrations: PollRegistrations) -> PollRegistrations {
        let mut owned = self.registrations.lock();
        mem::replace(&mut *owned, registrations)
    }

    fn consume(&self, file: &VfsFile) -> ConsumeResult {
        let current_events = file.poll();
        let mut config = self.config.lock();
        let interested_events = config.event.events;
        let matched = match_ready_events(current_events, interested_events);
        if matched.is_empty() {
            return ConsumeResult::NoEvent;
        }

        // The ready entry represents a source notification. Two arrivals can
        // leave the same readiness mask; comparing it with the previous mask
        // would discard a real edge. ET differs from LT by not retaining this
        // entry after delivery.
        let (should_notify, new_mode) = config.mode.should_notify();
        config.mode = new_mode;
        trace!(
            "consume fd: {} matches {:?} should notify: {} ",
            self.key.fd, matched, should_notify
        );

        if !should_notify {
            return ConsumeResult::NoEvent;
        }

        let event = EpollEvent {
            events: matched,
            user_data: config.event.user_data,
        };

        match config.mode {
            TriggerMode::Level => ConsumeResult::EventAndKeep(event),
            TriggerMode::Edge | TriggerMode::OneShot { .. } => ConsumeResult::EventAndRemove(event),
        }
    }

    fn next_waker_generation(&self) -> usize {
        self.waker_generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn is_current_waker_generation(&self, generation: usize) -> bool {
        self.waker_generation.load(Ordering::Acquire) == generation
    }
}

struct InterestWaker {
    epoll: Weak<EpollInner>,
    interest: Weak<EpollInterest>,
    generation: usize,
}

impl Wake for InterestWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if let Some(interest) = self.interest.upgrade() {
            if !interest.is_current_waker_generation(self.generation) {
                return;
            }
            self.queue_interest_with(interest);
        }
    }
}

impl InterestWaker {
    fn new(epoll: &Arc<EpollInner>, interest: &Arc<EpollInterest>) -> Arc<Self> {
        let generation = interest.next_waker_generation();
        Arc::new(Self {
            epoll: Arc::downgrade(epoll),
            interest: Arc::downgrade(interest),
            generation,
        })
    }

    fn queue_interest_with(&self, interest: Arc<EpollInterest>) {
        let Some(epoll) = self.epoll.upgrade() else {
            return;
        };
        epoll.queue_interest(&interest);
    }
}

struct EpollInner {
    // Serializes epoll_ctl-style updates that mutate the interest table and
    // per-interest registrations. Ready consumption keeps using finer locks.
    ctl_lock: Mutex<()>,
    interests: SpinNoPreempt<HashMap<EntryKey, Arc<EpollInterest>>>,
    // Poll source wakers may run directly from timer IRQ context. Mask local
    // IRQs so an interrupt cannot recursively acquire this lock while the
    // interrupted task is holding it.
    ready_queue: SpinNoIrq<VecDeque<Weak<EpollInterest>>>,
    poll_ready: PollSet,
}

impl EpollInner {
    fn queue_interest(&self, interest: &Arc<EpollInterest>) {
        if interest.try_mark_in_queue() {
            self.ready_queue.lock().push_back(Arc::downgrade(interest));
            // Source callbacks can run in IRQ context; do not take the
            // task-context config lock here, including for tracing.
            trace!(
                "Epoll: fd={} added to ready queue; wake up poller",
                interest.key.fd
            );
            self.poll_ready.wake();
        }
    }
}

impl Default for EpollInner {
    fn default() -> Self {
        Self {
            ctl_lock: Mutex::new(()),
            interests: SpinNoPreempt::new(HashMap::new()),
            ready_queue: SpinNoIrq::new(VecDeque::new()),
            poll_ready: PollSet::new(),
        }
    }
}

/// An epoll instance.
#[derive(Default)]
pub struct Epoll {
    inner: Arc<EpollInner>,
}

impl Epoll {
    /// Creates a new epoll instance.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an epoll anonymous-inode file and captures `cred` as its open credential.
    ///
    /// # Errors
    ///
    /// Forwards errors from anonymous-inode file creation.
    pub fn new_file(cred: Arc<Cred>) -> KResult<Arc<VfsFile>> {
        AnonInodeFs::global().get_file(
            "[eventpoll]",
            Arc::new(EventpollFops),
            Arc::new(Self::new()),
            FMode::READ | FMode::WRITE | FMode::STREAM,
            OpenFlags::READ_WRITE,
            cred,
        )
    }

    /// Returns the epoll instance attached to an epoll file.
    ///
    /// # Errors
    ///
    /// Returns `KError::BadFileDescriptor` when the file has no matching private data.
    pub fn from_file(file: &VfsFile) -> KResult<Arc<Self>> {
        file.private_data_get::<Self>()
            .ok_or(KError::BadFileDescriptor)
    }

    fn register_waker(&self, interest: &Arc<EpollInterest>, file: &VfsFile) -> KResult<()> {
        // Registration precedes the readiness sample. Earlier callbacks may
        // be superseded because that sample covers their state; callbacks
        // from this generation must always leave pending queue work.
        let waker = Waker::from(InterestWaker::new(&self.inner, interest));
        let context = Context::from_waker(&waker);
        let mut registrations = PollRegistrations::new();
        {
            let mut poll_context = registrations.context(&context);
            interest
                .register_poll(file, &mut poll_context, interest.registered_events())
                .map_err(map_register_error)?;
        }
        drop(interest.replace_registrations(registrations));
        Ok(())
    }

    fn check_and_register_waker(&self, interest: &Arc<EpollInterest>) -> KResult<()> {
        let Some(file) = interest.key.get_file() else {
            drop(interest.take_registrations());
            return Ok(());
        };
        if !interest.is_enabled() {
            drop(interest.take_registrations());
            return Ok(());
        }

        if !interest.is_in_queue()
            && !match_ready_events(file.poll(), interest.interested_events()).is_empty()
        {
            // A queued check will arm the source before consuming readiness.
            interest.next_waker_generation();
            drop(interest.take_registrations());
            self.inner.queue_interest(interest);
        } else {
            self.register_waker(interest, &file)?;
            if !match_ready_events(file.poll(), interest.interested_events()).is_empty() {
                self.inner.queue_interest(interest);
            }
        }
        Ok(())
    }

    /// Adds a file descriptor interest to the epoll instance.
    ///
    /// # Errors
    ///
    /// Returns `KError::AlreadyExists` for the same fd/file key; forwards mapped
    /// poll-source capture or registration failures and rolls back insertion.
    pub fn add(
        &self,
        fd: i32,
        file: Arc<VfsFile>,
        event: EpollEvent,
        flags: EpollFlags,
    ) -> KResult<()> {
        let _ctl_guard = self.inner.ctl_lock.lock();
        let key = EntryKey::new(fd, &file);
        if self.inner.interests.lock().contains_key(&key) {
            return Err(KError::AlreadyExists);
        }
        let poll_source = file.capture_poll_source().map_err(map_register_error)?;
        let interest = Arc::new(EpollInterest::new(key.clone(), event, flags, poll_source));
        self.inner
            .interests
            .lock()
            .insert(key.clone(), Arc::clone(&interest));
        trace!(
            "Epoll add fd: {} interest {:?} ",
            fd,
            interest.interested_events()
        );
        let _operation_guard = interest.operation_lock.lock();
        if let Err(err) = self.check_and_register_waker(&interest) {
            let removed = {
                let mut guard = self.inner.interests.lock();
                if guard
                    .get(&key)
                    .is_some_and(|current| Arc::ptr_eq(current, &interest))
                {
                    guard.remove(&key)
                } else {
                    None
                }
            };
            drop(removed);
            return Err(err);
        }
        Ok(())
    }

    /// Modifies an existing interest for the given file descriptor.
    ///
    /// # Errors
    ///
    /// Returns `KError::NotFound` for an absent fd/file key; forwards mapped
    /// registration failures after restoring the previous configuration and
    /// queueing enabled interests for a readiness check and rearm retry.
    pub fn modify(
        &self,
        fd: i32,
        file: Arc<VfsFile>,
        event: EpollEvent,
        flags: EpollFlags,
    ) -> KResult<()> {
        let _ctl_guard = self.inner.ctl_lock.lock();
        let key = EntryKey::new(fd, &file);
        let interest = self
            .inner
            .interests
            .lock()
            .get(&key)
            .cloned()
            .ok_or(KError::NotFound)?;
        let _operation_guard = interest.operation_lock.lock();
        let snapshot = interest.modify_config(event, flags);
        let registration_result = self.check_and_register_waker(&interest);
        if let Err(err) = registration_result {
            interest.restore_config(snapshot);
            // Registration already advanced the waker generation. Restoring
            // config alone leaves the old source wakers unable to notify us.
            if interest.is_enabled() {
                self.inner.queue_interest(&interest);
            }
            return Err(err);
        }
        if interest.is_in_queue() {
            self.inner.poll_ready.wake();
        }

        trace!(
            "Epoll: modify fd={}, events={:?}",
            fd,
            interest.interested_events()
        );
        Ok(())
    }

    /// Removes an existing interest for the given file descriptor.
    ///
    /// # Errors
    ///
    /// Returns `KError::NotFound` when no interest matches the fd/file key.
    pub fn delete(&self, fd: i32, file: Arc<VfsFile>) -> KResult<()> {
        let _ctl_guard = self.inner.ctl_lock.lock();
        let key = EntryKey::new(fd, &file);
        let removed = self
            .inner
            .interests
            .lock()
            .remove(&key)
            .ok_or(KError::NotFound)?;
        drop(removed);
        trace!("Epoll: delete fd={fd}");
        Ok(())
    }

    /// Polls for ready events and writes them into `out`.
    ///
    /// # Errors
    ///
    /// Returns a mapped poll-source registration error when no event was emitted.
    /// If events were already written, returns their count and requeues retry work.
    pub fn poll_events(&self, out: &mut [k_epoll_event]) -> KResult<usize> {
        trace!("Epoll: poll_events called, out.len()={}", out.len());

        let mut count = 0;
        let mut deferred_keep = Vec::new();
        // Bound this call to the number of entries ready when it started.
        // Rearming an interest while consuming it can synchronously enqueue the
        // same interest again through a source callback. Leaving those new
        // entries for the next call prevents a persistently-ready fd from
        // keeping one `epoll_wait` call alive forever.
        let (ready_count, mut first_ready_interest) = {
            let mut queue = self.inner.ready_queue.lock();
            let ready_count = queue.len();
            (ready_count, queue.pop_front())
        };
        // The value records whether this call already retained a queue entry.
        // Stale duplicates may be discarded, but a fresh wake after an edge
        // was consumed still owns its in_ready_queue flag and must be kept.
        let mut emitted: HashMap<EntryKey, bool> = HashMap::with_capacity(ready_count);
        for index in 0..ready_count {
            let weak_interest = if index == 0 {
                first_ready_interest.take()
            } else {
                let mut queue = self.inner.ready_queue.lock();
                queue.pop_front()
            };

            let Some(weak_interest) = weak_interest else {
                break;
            };
            let Some(interest) = weak_interest.upgrade() else {
                continue;
            };
            let Some(file) = interest.key.get_file() else {
                let removed = self.inner.interests.lock().remove(&interest.key);
                drop(removed);
                interest.mark_not_in_queue();
                continue;
            };
            if let Some(has_retained_entry) = emitted.get(&interest.key) {
                if !*has_retained_entry && interest.is_in_queue() {
                    self.inner.ready_queue.lock().push_front(weak_interest);
                    // The original batch has been overtaken by a new wake.
                    // Stop before this call can pop and discard it again.
                    break;
                }
                continue;
            }
            emitted.insert(interest.key.clone(), false);
            if count >= out.len() {
                self.inner.ready_queue.lock().push_front(weak_interest);
                break;
            }

            trace!(
                "Epoll: consuming ready interest for fd={}, events={:?}",
                interest.key.fd,
                interest.interested_events()
            );

            let _operation_guard = interest.operation_lock.lock();
            // Claim only the popped entry before registering and sampling.
            // A callback during either operation owns a new queue entry and
            // must never be erased by completion of this consumption.
            interest.mark_not_in_queue();
            if !interest.is_enabled() {
                drop(interest.take_registrations());
                continue;
            }
            let registration_result = self.register_waker(&interest, &file);
            match interest.consume(file.as_ref()) {
                ConsumeResult::EventAndKeep(event) => {
                    out[count] = k_epoll_event::new(event.events.bits(), event.user_data);
                    count += 1;
                    if interest.try_mark_in_queue() {
                        deferred_keep.push(Arc::downgrade(&interest));
                        emitted.insert(interest.key.clone(), true);
                    }
                }
                ConsumeResult::EventAndRemove(event) => {
                    out[count] = k_epoll_event::new(event.events.bits(), event.user_data);
                    count += 1;
                    if !interest.is_enabled() {
                        // A fired ONESHOT no longer needs sources, including
                        // callbacks detached concurrently with cancellation.
                        interest.next_waker_generation();
                        drop(interest.take_registrations());
                        continue;
                    }
                }
                ConsumeResult::NoEvent => {}
            }
            if let Err(err) = registration_result {
                // Preserve events already copied out and retry registration
                // through the queue instead of stranding an enabled interest.
                return Self::finish_poll_events(
                    &self.inner,
                    count,
                    deferred_keep,
                    Some(Arc::downgrade(&interest)),
                    Some(err),
                );
            }
        }

        Self::finish_poll_events(&self.inner, count, deferred_keep, None, None)
    }

    fn finish_poll_events(
        inner: &EpollInner,
        count: usize,
        deferred_keep: Vec<Weak<EpollInterest>>,
        failed_rearm: Option<Weak<EpollInterest>>,
        rearm_error: Option<KError>,
    ) -> KResult<usize> {
        {
            let mut queue = inner.ready_queue.lock();
            for interest in deferred_keep {
                queue.push_back(interest);
            }
            if let Some(failed) = failed_rearm
                && let Some(interest) = failed.upgrade()
                && interest.try_mark_in_queue()
            {
                queue.push_back(failed);
            }
        }

        match rearm_error {
            Some(err) if count == 0 => Err(err),
            _ if count == 0 => Err(KError::WouldBlock),
            _ => Ok(count),
        }
    }
}

impl Pollable for Epoll {
    fn poll(&self) -> IoEvents {
        if self.inner.ready_queue.lock().is_empty() {
            IoEvents::empty()
        } else {
            IoEvents::IN
        }
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            context.register(&self.inner.poll_ready)?;
        }
        Ok(())
    }
}

struct EventpollFops;

impl FileOperations for EventpollFops {
    fn release(&self, _inode: &VfsInode, _file: &VfsFile) -> KResult<()> {
        Ok(())
    }

    fn poll(&self, file: &VfsFile) -> Option<IoEvents> {
        Some(Epoll::from_file(file).map_or(IoEvents::ERR, |epoll| epoll.poll()))
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if let Ok(epoll) = Epoll::from_file(file) {
            epoll.register(context, events)?;
        }
        Ok(())
    }
}

unittest::path_mod_test!(epoll_tests, "../ut-tests/epoll.rs");
