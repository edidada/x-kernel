// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Conversion between workqueue executor entries and worker-pool entries.

use kworkerpool::{
    EntryKey as PoolEntryKey, EntryOwner as PoolEntryOwner, EntryPayload as PoolEntryPayload,
    EntrySource as PoolEntrySource, PoolEntry,
};

pub(crate) struct PoolEntryAdapter(PoolEntry);

impl PoolEntryAdapter {
    pub(crate) const fn into_inner(self) -> PoolEntry {
        self.0
    }
}

impl From<kworkqueue::ExecutorEntry> for PoolEntryAdapter {
    fn from(entry: kworkqueue::ExecutorEntry) -> Self {
        Self(PoolEntry::new(
            PoolEntrySource::new(entry.binding.get()),
            PoolEntryOwner::new(entry.owner.get()),
            PoolEntryKey::new(entry.key.get()),
            PoolEntryPayload::new(entry.payload.get()),
        ))
    }
}

impl TryFrom<PoolEntryAdapter> for kworkqueue::ExecutorEntry {
    type Error = ();

    fn try_from(entry: PoolEntryAdapter) -> Result<Self, Self::Error> {
        let entry = entry.0;
        Ok(Self {
            binding: non_zero_pool_value(entry.source().as_usize())
                .map(kworkqueue::BindingId::new)?,
            owner: non_zero_pool_value(entry.owner().as_usize())
                .map(kworkqueue::EntryOwner::new)?,
            key: non_zero_pool_value(entry.key().as_usize()).map(kworkqueue::EntryKey::new)?,
            payload: non_zero_pool_value(entry.into_payload().as_usize())
                .map(kworkqueue::EntryPayload::new)?,
        })
    }
}

pub(crate) fn pool_entry(entry: kworkqueue::ExecutorEntry) -> PoolEntry {
    PoolEntryAdapter::from(entry).into_inner()
}

pub(crate) const fn pool_owner(owner: kworkqueue::EntryOwner) -> PoolEntryOwner {
    PoolEntryOwner::new(owner.get())
}

pub(crate) const fn pool_key(key: kworkqueue::EntryKey) -> PoolEntryKey {
    PoolEntryKey::new(key.get())
}

pub(crate) fn executor_entry(entry: PoolEntry) -> Option<kworkqueue::ExecutorEntry> {
    kworkqueue::ExecutorEntry::try_from(PoolEntryAdapter(entry)).ok()
}

fn non_zero_pool_value(value: usize) -> Result<core::num::NonZeroUsize, ()> {
    core::num::NonZeroUsize::new(value).ok_or(())
}
