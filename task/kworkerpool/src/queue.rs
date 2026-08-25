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

/// Bounded runnable/deferred queue storage for one worker-pool instance.
///
/// Runnable entries are globally FIFO. Deferred entries are grouped by opaque
/// owner key and can be mechanically promoted into the runnable FIFO by owner.
/// Runnable and deferred lanes share one total entry capacity.
pub struct PoolRunQueue<O, K, T, const ENTRY_CAP: usize> {
    runnable: EntryQueue<PoolEntry<O, K, T>, ENTRY_CAP>,
    deferred: EntryQueue<PoolEntry<O, K, T>, ENTRY_CAP>,
    len: usize,
}

impl<O, K, T, const ENTRY_CAP: usize> PoolRunQueue<O, K, T, ENTRY_CAP>
where
    O: Clone + Eq,
    K: Eq,
{
    /// Creates an empty pool run queue.
    pub const fn new() -> Self {
        Self {
            runnable: EntryQueue::new(),
            deferred: EntryQueue::new(),
            len: 0,
        }
    }

    /// Returns total entries across runnable and deferred lanes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Returns whether no entries are queued.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns current runnable FIFO length.
    pub fn runnable_len(&self) -> usize {
        self.runnable.len()
    }

    /// Returns runnable entries for one owner.
    pub fn runnable_len_for_owner(&self, owner: &O) -> usize {
        self.runnable.count_matching(|entry| entry.owner() == owner)
    }

    /// Returns deferred entries for one owner.
    pub fn deferred_len_for_owner(&self, owner: &O) -> usize {
        self.deferred.count_matching(|entry| entry.owner() == owner)
    }

    /// Pushes an entry directly into runnable FIFO.
    pub fn push_runnable(&mut self, entry: PoolEntry<O, K, T>) -> Result<(), PoolEntry<O, K, T>> {
        if self.len == ENTRY_CAP {
            return Err(entry);
        }
        self.runnable.push(entry)?;
        self.len += 1;
        Ok(())
    }

    /// Pushes an entry into its owner's deferred FIFO.
    pub fn push_deferred(&mut self, entry: PoolEntry<O, K, T>) -> Result<(), PoolEntry<O, K, T>> {
        if self.len == ENTRY_CAP {
            return Err(entry);
        }
        self.deferred.push(entry)?;
        self.len += 1;
        Ok(())
    }

    /// Promotes up to `budget` entries from one owner deferred FIFO.
    pub fn promote_deferred(&mut self, owner: &O, budget: usize) -> usize {
        let mut promoted = 0;
        while promoted < budget && self.runnable.len() < ENTRY_CAP {
            let Some(entry) = self.deferred.remove_first(|entry| entry.owner() == owner) else {
                break;
            };
            if self.runnable.push(entry).is_err() {
                break;
            }
            promoted += 1;
        }
        promoted
    }

    /// Moves all runnable entries for `owner` into that owner's deferred FIFO.
    ///
    /// This is a mechanical lane move; caller-owned policy decides when it is
    /// necessary.
    pub fn defer_runnable_for_owner(&mut self, owner: &O) -> usize {
        let mut moved = 0usize;
        while let Some(entry) = self.runnable.remove_first(|entry| entry.owner() == owner) {
            self.deferred
                .push(entry)
                .ok()
                .expect("moving within shared pool capacity should not fail");
            moved += 1;
        }
        moved
    }

    /// Pops the oldest runnable entry.
    pub fn pop_runnable(&mut self) -> Option<PoolEntry<O, K, T>> {
        let entry = self.runnable.pop_front()?;
        self.len = self.len.saturating_sub(1);
        Some(entry)
    }

    /// Removes an entry by owner and key from runnable or deferred lanes.
    pub fn remove(
        &mut self,
        owner: &O,
        key: &K,
    ) -> Option<(PoolEntry<O, K, T>, QueueRemoveResult)> {
        if let Some(entry) = self
            .runnable
            .remove_first(|entry| entry.owner() == owner && entry.key() == key)
        {
            self.len = self.len.saturating_sub(1);
            return Some((entry, QueueRemoveResult::Runnable));
        }
        self.deferred
            .remove_first(|entry| entry.owner() == owner && entry.key() == key)
            .map(|entry| {
                self.len = self.len.saturating_sub(1);
                (entry, QueueRemoveResult::Deferred)
            })
    }

    /// Returns a mutable payload reference by owner and key.
    pub fn get_mut(&mut self, owner: &O, key: &K) -> Option<&mut T> {
        self.runnable
            .get_mut(|entry| entry.owner() == owner && entry.key() == key)
            .map(|entry| &mut entry.payload)
            .or_else(|| {
                self.deferred
                    .get_mut(|entry| entry.owner() == owner && entry.key() == key)
                    .map(|entry| &mut entry.payload)
            })
    }
}

impl<O, K, T, const ENTRY_CAP: usize> Default for PoolRunQueue<O, K, T, ENTRY_CAP>
where
    O: Clone + Eq,
    K: Eq,
{
    fn default() -> Self {
        Self::new()
    }
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
                .is_some_and(&pred)
        })?;
        self.remove_logical(logical)
    }

    fn get_mut(&mut self, pred: impl Fn(&T) -> bool) -> Option<&mut T> {
        let logical = (0..self.len).find(|index| {
            self.entries[self.physical_index(*index)]
                .as_ref()
                .is_some_and(&pred)
        })?;
        let physical = self.physical_index(logical);
        self.entries[physical].as_mut()
    }

    fn count_matching(&self, pred: impl Fn(&T) -> bool) -> usize {
        (0..self.len)
            .filter(|index| {
                self.entries[self.physical_index(*index)]
                    .as_ref()
                    .is_some_and(&pred)
            })
            .count()
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
