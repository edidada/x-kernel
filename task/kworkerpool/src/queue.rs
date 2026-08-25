// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

/// Entry stored by one worker-pool instance.
///
/// `owner` is an opaque user-defined grouping key used for deferred promotion.
/// `key` is an opaque user-defined entry key used for removal. The pool never
/// interprets either value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PoolEntry<O, K, T> {
    owner: O,
    key: K,
    payload: T,
}

impl<O, K, T> PoolEntry<O, K, T> {
    /// Creates a pool entry.
    pub const fn new(owner: O, key: K, payload: T) -> Self {
        Self {
            owner,
            key,
            payload,
        }
    }

    /// Returns the opaque owner key.
    pub const fn owner(&self) -> &O {
        &self.owner
    }

    /// Returns the opaque entry key.
    pub const fn key(&self) -> &K {
        &self.key
    }

    /// Consumes the entry and returns the payload.
    pub fn into_payload(self) -> T {
        self.payload
    }
}

/// Result of removing an entry from pool queues.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueRemoveResult {
    /// The entry came from the runnable FIFO.
    Runnable,
    /// The entry came from a deferred owner lane.
    Deferred,
}

pub(crate) struct PoolRunQueue<
    O,
    K,
    T,
    const RUNNABLE_CAP: usize,
    const OWNER_CAP: usize,
    const DEFERRED_CAP: usize,
> {
    runnable: EntryQueue<PoolEntry<O, K, T>, RUNNABLE_CAP>,
    deferred: DeferredQueues<O, K, T, OWNER_CAP, DEFERRED_CAP>,
}

impl<O, K, T, const RUNNABLE_CAP: usize, const OWNER_CAP: usize, const DEFERRED_CAP: usize>
    PoolRunQueue<O, K, T, RUNNABLE_CAP, OWNER_CAP, DEFERRED_CAP>
where
    O: Clone + Eq,
    K: Eq,
{
    pub(crate) const fn new() -> Self {
        Self {
            runnable: EntryQueue::new(),
            deferred: DeferredQueues::new(),
        }
    }

    pub(crate) fn runnable_len(&self) -> usize {
        self.runnable.len()
    }

    pub(crate) fn push_runnable(
        &mut self,
        entry: PoolEntry<O, K, T>,
    ) -> Result<(), PoolEntry<O, K, T>> {
        self.runnable.push(entry)
    }

    pub(crate) fn push_deferred(
        &mut self,
        entry: PoolEntry<O, K, T>,
    ) -> Result<(), PoolEntry<O, K, T>> {
        self.deferred.push(entry)
    }

    pub(crate) fn promote_deferred(&mut self, owner: &O, budget: usize) -> usize {
        let mut promoted = 0;
        while promoted < budget && self.runnable.len() < RUNNABLE_CAP {
            let Some(entry) = self.deferred.pop_owner_front(owner) else {
                break;
            };
            if self.runnable.push(entry).is_err() {
                break;
            }
            promoted += 1;
        }
        promoted
    }

    pub(crate) fn pop_runnable(&mut self) -> Option<PoolEntry<O, K, T>> {
        self.runnable.pop_front()
    }

    pub(crate) fn remove(
        &mut self,
        owner: &O,
        key: &K,
    ) -> Option<(PoolEntry<O, K, T>, QueueRemoveResult)> {
        if let Some(entry) = self
            .runnable
            .remove_first(|entry| entry.owner() == owner && entry.key() == key)
        {
            return Some((entry, QueueRemoveResult::Runnable));
        }
        self.deferred
            .remove(owner, key)
            .map(|entry| (entry, QueueRemoveResult::Deferred))
    }
}

struct DeferredQueues<O, K, T, const OWNER_CAP: usize, const DEFERRED_CAP: usize> {
    lanes: [Option<DeferredLane<O, K, T, DEFERRED_CAP>>; OWNER_CAP],
}

impl<O, K, T, const OWNER_CAP: usize, const DEFERRED_CAP: usize>
    DeferredQueues<O, K, T, OWNER_CAP, DEFERRED_CAP>
where
    O: Clone + Eq,
    K: Eq,
{
    const fn new() -> Self {
        Self {
            lanes: [const { None }; OWNER_CAP],
        }
    }

    fn push(&mut self, entry: PoolEntry<O, K, T>) -> Result<(), PoolEntry<O, K, T>> {
        if let Some(index) = self.find_lane(entry.owner()) {
            return self.lanes[index]
                .as_mut()
                .expect("found deferred lane should exist")
                .queue
                .push(entry);
        }
        let Some(index) = self.lanes.iter().position(Option::is_none) else {
            return Err(entry);
        };
        let owner = entry.owner;
        let key = entry.key;
        let payload = entry.payload;
        self.lanes[index] = Some(DeferredLane {
            owner,
            queue: EntryQueue::new(),
        });
        let lane = self.lanes[index]
            .as_mut()
            .expect("new deferred lane should exist");
        lane.queue
            .push(PoolEntry::new(lane.owner.clone(), key, payload))
    }

    fn pop_owner_front(&mut self, owner: &O) -> Option<PoolEntry<O, K, T>> {
        let index = self.find_lane(owner)?;
        let entry = self.lanes[index]
            .as_mut()
            .expect("found deferred lane should exist")
            .queue
            .pop_front();
        self.clear_empty_lane(index);
        entry
    }

    fn remove(&mut self, owner: &O, key: &K) -> Option<PoolEntry<O, K, T>> {
        let index = self.find_lane(owner)?;
        let removed = self.lanes[index]
            .as_mut()
            .expect("found deferred lane should exist")
            .queue
            .remove_first(|entry| entry.key() == key);
        self.clear_empty_lane(index);
        removed
    }

    fn find_lane(&self, owner: &O) -> Option<usize> {
        self.lanes
            .iter()
            .position(|lane| lane.as_ref().is_some_and(|lane| &lane.owner == owner))
    }

    fn clear_empty_lane(&mut self, index: usize) {
        if self.lanes[index]
            .as_ref()
            .is_some_and(|lane| lane.queue.is_empty())
        {
            self.lanes[index] = None;
        }
    }
}

struct DeferredLane<O, K, T, const CAP: usize> {
    owner: O,
    queue: EntryQueue<PoolEntry<O, K, T>, CAP>,
}

struct EntryQueue<T, const CAP: usize> {
    entries: [Option<T>; CAP],
    head: usize,
    len: usize,
}

impl<T, const CAP: usize> EntryQueue<T, CAP> {
    const fn new() -> Self {
        Self {
            entries: [const { None }; CAP],
            head: 0,
            len: 0,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn physical_index(&self, logical_index: usize) -> usize {
        (self.head + logical_index) % CAP
    }

    fn push(&mut self, entry: T) -> Result<(), T> {
        if self.len == CAP {
            return Err(entry);
        }
        let index = self.physical_index(self.len);
        self.entries[index] = Some(entry);
        self.len += 1;
        Ok(())
    }

    fn pop_front(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        let index = self.head;
        let entry = self.entries[index].take();
        self.len -= 1;
        if self.len == 0 {
            self.head = 0;
        } else {
            self.head = self.physical_index(1);
        }
        entry
    }

    fn remove_first(&mut self, pred: impl Fn(&T) -> bool) -> Option<T> {
        let logical = (0..self.len).find(|index| {
            self.entries[self.physical_index(*index)]
                .as_ref()
                .is_some_and(|entry| pred(entry))
        })?;
        self.remove_logical(logical)
    }

    fn remove_logical(&mut self, logical_index: usize) -> Option<T> {
        let physical = self.physical_index(logical_index);
        let removed = self.entries[physical].take();

        for logical in logical_index..(self.len - 1) {
            let current = self.physical_index(logical);
            let next = self.physical_index(logical + 1);
            self.entries[current] = self.entries[next].take();
        }
        let tail = self.physical_index(self.len - 1);
        self.entries[tail] = None;
        self.len -= 1;
        if self.len == 0 {
            self.head = 0;
        }
        removed
    }
}
