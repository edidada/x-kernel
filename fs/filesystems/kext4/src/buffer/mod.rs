// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

/// Defines immutable snapshots and transaction-scoped metadata access tokens.
mod access;
/// Indexes one mount's metadata buffers and coordinates loading and reclamation.
///
/// The cache publishes one slot per physical block. A single loader performs
/// device I/O while concurrent readers wait; failed loads wake waiters and are
/// removed so a later read can retry.
mod cache;
/// Connects metadata buffer state to journal handles and home-block I/O.
///
/// `Ext4MetadataIo` is the entry point used by storage algorithms. It consumes
/// journal credits before granting mutable access and writes committed
/// snapshots back to their home blocks during checkpoint.
mod io;
/// Enforces the per-block loading, journal ownership, and checkpoint state machine.
///
/// A slot serializes transitions between loading, clean, transaction-owned,
/// dirty, and checkpoint states. Waiters never observe bytes until the initial
/// load has either published a complete block or failed.
mod slot;
/// Selects host or kernel wait and locking primitives for metadata buffers.
mod sync;

/// Covers metadata-buffer ownership, checkpoint, failure, and reclamation behavior.
#[cfg(test)]
mod tests;

pub(crate) use access::{MetadataBuffer, MetadataWriteAccess};
#[cfg(test)]
use cache::MetadataBlockCache;
pub(crate) use io::Ext4MetadataIo;
#[cfg(test)]
use slot::MetadataBufferState;
