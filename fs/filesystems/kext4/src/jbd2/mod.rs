// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

mod commit;
mod log;
/// Defines typed journal and target block numbers plus journal block mapping.
mod mapper;
mod replay;
/// Decodes and updates the JBD2 journal superblock.
mod superblock;
mod transaction;

pub(crate) use commit::{
    JournalBlockWriter, JournalCommitBlock, JournalPersistedCommit, finish_journal_checkpoint,
    persist_journal_commit,
};
pub(crate) use log::{
    JournalBlockReader, JournalLogScan, JournalTransaction, JournalTransactionState, scan_journal,
};
pub(crate) use mapper::{JournalBlock, JournalBlockMapper, JournalTargetBlock, TransactionId};
pub(crate) use replay::{
    JournalReplayApplied, JournalReplayBlockWriter, JournalReplayReport, replay_scanned_journal,
};
pub(crate) use superblock::{
    JournalStart, JournalSuperblock, enable_superblock_revoke, mark_superblock_empty,
};
pub(crate) use transaction::{
    JournalCredits, JournalHandle, JournalTransactions, RuntimeTransaction,
};
