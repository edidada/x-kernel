// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared configuration and counters for the semantic IPv4 filter table.
//!
//! Management adapters submit validated tables through this API. Wire formats,
//! entry offsets, and adapter-specific metadata remain outside the store.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

use klazy::Lazy;
use ksync::RwLock;

use super::{
    matcher::FilterPacket,
    model::{FilterHook, FilterTable, Verdict},
};

static FILTER_CONFIG: Lazy<FilterConfig> =
    Lazy::new(|| FilterConfig::new(FilterTable::accept_all()));

/// Returns the shared configuration capability used by management adapters.
pub(crate) fn global() -> &'static FilterConfig {
    &FILTER_CONFIG
}

/// Packet and byte totals for one semantic rule or chain completion.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct FilterCounter {
    pub(crate) packets: u64,
    pub(crate) bytes: u64,
}

/// Counters ordered by rule position, followed by the chain-completion counter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChainCounters {
    pub(crate) rules: Vec<FilterCounter>,
    pub(crate) end: FilterCounter,
}

/// Opaque publication identity for conflict checks and adapter metadata.
#[derive(Clone, Debug)]
pub(crate) struct FilterGeneration(Arc<()>);

impl PartialEq for FilterGeneration {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for FilterGeneration {}

/// One owned semantic table and a coherent snapshot of its counters.
pub(crate) struct FilterSnapshot {
    pub(crate) generation: FilterGeneration,
    pub(crate) table: Arc<FilterTable>,
    pub(crate) counters: Vec<ChainCounters>,
}

/// A failed conditional publication or a mismatched semantic counter shape.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FilterConfigError {
    Conflict,
    InvalidCounterShape,
}

/// Owns publication and counters without depending on a management ABI.
pub(crate) struct FilterConfig {
    current: RwLock<FilterState>,
}

struct FilterState {
    generation: FilterGeneration,
    table: Arc<FilterTable>,
    counters: Vec<ChainCounterState>,
}

struct ChainCounterState {
    rules: Vec<EntryCounter>,
    end: EntryCounter,
}

struct EntryCounter {
    packets: AtomicU64,
    bytes: AtomicU64,
}

impl EntryCounter {
    fn new() -> Self {
        Self {
            packets: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> FilterCounter {
        FilterCounter {
            packets: self.packets.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
        }
    }

    fn add(&self, value: FilterCounter) {
        self.packets.fetch_add(value.packets, Ordering::Relaxed);
        self.bytes.fetch_add(value.bytes, Ordering::Relaxed);
    }
}

impl FilterState {
    fn new(table: FilterTable) -> Self {
        let counters = table
            .chains()
            .iter()
            .map(|chain| ChainCounterState {
                rules: chain.rules().iter().map(|_| EntryCounter::new()).collect(),
                end: EntryCounter::new(),
            })
            .collect();
        Self {
            generation: FilterGeneration(Arc::new(())),
            table: Arc::new(table),
            counters,
        }
    }

    fn snapshot(&self) -> FilterSnapshot {
        FilterSnapshot {
            generation: self.generation.clone(),
            table: Arc::clone(&self.table),
            counters: self
                .counters
                .iter()
                .map(|chain| ChainCounters {
                    rules: chain.rules.iter().map(EntryCounter::snapshot).collect(),
                    end: chain.end.snapshot(),
                })
                .collect(),
        }
    }

    fn evaluate(&self, hook: FilterHook, packet: &FilterPacket<'_>) -> Verdict {
        self.table
            .evaluate_with_counter(hook, packet, |chain_id, rule_index| {
                let chain = &self.counters[chain_id.index()];
                let counter = rule_index.map_or(&chain.end, |rule| &chain.rules[rule]);
                counter.add(FilterCounter {
                    packets: 1,
                    bytes: packet.packet_len() as u64,
                });
            })
    }
}

impl FilterConfig {
    pub(crate) fn new(table: FilterTable) -> Self {
        Self {
            current: RwLock::new(FilterState::new(table)),
        }
    }

    /// Captures a complete generation after in-flight counter updates finish.
    pub(crate) fn snapshot(&self) -> FilterSnapshot {
        // Updates hold shared guards. An exclusive guard keeps both fields of
        // each counter, and all counters in this snapshot, mutually consistent.
        self.current.write().snapshot()
    }

    /// Publishes a validated table and returns the old snapshot and new identity.
    ///
    /// Construction finishes before taking the publication lock. `check` runs
    /// against the current immutable table and identity under that lock; it must
    /// not perform I/O or acquire another configuration/adapter lock.
    ///
    /// # Errors
    ///
    /// Returns the adapter's conflict check without changing the current table.
    pub(crate) fn replace(
        &self,
        table: FilterTable,
        check: impl FnOnce(&FilterTable, &FilterGeneration) -> Result<(), FilterConfigError>,
    ) -> Result<(FilterSnapshot, FilterGeneration), FilterConfigError> {
        let replacement = FilterState::new(table);
        let generation = replacement.generation.clone();
        let mut current = self.current.write();
        check(&current.table, &current.generation)?;
        let snapshot = current.snapshot();
        let old = core::mem::replace(&mut *current, replacement);
        drop(current);
        drop(old);
        Ok((snapshot, generation))
    }

    /// Adds counters to the generation selected while holding a shared guard.
    ///
    /// `prepare` maps an adapter request to rule and chain-completion counters
    /// using the current immutable table. It follows the lock restrictions of
    /// [`Self::replace`]. Every chain shape is checked before any update applies.
    ///
    /// # Errors
    ///
    /// Returns the adapter's conflict check, or `InvalidCounterShape` if the
    /// updates do not contain exactly one counter per rule and chain completion.
    pub(crate) fn add_counters(
        &self,
        prepare: impl FnOnce(&FilterTable) -> Result<Vec<ChainCounters>, FilterConfigError>,
    ) -> Result<FilterGeneration, FilterConfigError> {
        let current = self.current.read();
        let updates = prepare(&current.table)?;
        if updates.len() != current.counters.len()
            || updates
                .iter()
                .zip(&current.counters)
                .any(|(update, chain)| update.rules.len() != chain.rules.len())
        {
            return Err(FilterConfigError::InvalidCounterShape);
        }
        for (update, chain) in updates.iter().zip(&current.counters) {
            for (value, counter) in update.rules.iter().zip(&chain.rules) {
                counter.add(*value);
            }
            chain.end.add(update.end);
        }
        Ok(current.generation.clone())
    }

    pub(crate) fn evaluate_ipv4(
        &self,
        hook: FilterHook,
        packet: &[u8],
        indev: &str,
        outdev: &str,
    ) -> Verdict {
        let parsed = if hook == FilterHook::Output {
            FilterPacket::parse_output(packet, outdev)
        } else {
            FilterPacket::parse(packet, indev, outdev)
        };
        match parsed {
            Ok(parsed) => self.current.read().evaluate(hook, &parsed),
            Err(_) => Verdict::Drop,
        }
    }
}

/// Evaluates one packet using a complete published semantic generation.
pub(crate) fn evaluate_ipv4(hook: FilterHook, packet: &[u8], indev: &str, outdev: &str) -> Verdict {
    global().evaluate_ipv4(hook, packet, indev, outdev)
}

#[cfg(unittest)]
mod tests {
    use alloc::vec;

    use unittest::{assert_eq, def_test};

    use super::*;
    use crate::{
        ip::Ipv4Address,
        ipv4::{PROTOCOL_ICMP, build_ipv4_packet},
        netfilter::filter::model::Chain,
    };

    #[def_test]
    fn semantic_publication_replaces_policy_and_checks_generation() {
        let store = FilterConfig::new(FilterTable::accept_all());
        let packet = build_ipv4_packet(
            Ipv4Address::new(192, 0, 2, 1),
            Ipv4Address::new(192, 0, 2, 2),
            PROTOCOL_ICMP,
            64,
            &[8, 0, 0, 0, 0, 0, 0, 0],
        )
        .unwrap();
        assert_eq!(
            store.evaluate_ipv4(FilterHook::Input, &packet, "lo", ""),
            Verdict::Accept
        );
        let expected = store.snapshot().generation;
        let table = FilterTable::new(vec![
            Chain::built_in(FilterHook::Input, Verdict::Drop, Vec::new()),
            Chain::built_in(FilterHook::Forward, Verdict::Accept, Vec::new()),
            Chain::built_in(FilterHook::Output, Verdict::Accept, Vec::new()),
        ])
        .unwrap();
        let (old, generation) = store
            .replace(table.clone(), |_, current| {
                if current != &expected {
                    return Err(FilterConfigError::Conflict);
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(old.generation, expected);
        assert_eq!(
            old.counters[0].end,
            FilterCounter {
                packets: 1,
                bytes: packet.len() as u64
            }
        );
        assert_eq!(generation == expected, false);
        assert_eq!(*store.snapshot().table, table);
        assert_eq!(store.snapshot().counters[0].end, FilterCounter::default());
        assert_eq!(
            store.evaluate_ipv4(FilterHook::Input, &packet, "lo", ""),
            Verdict::Drop
        );
        let result = store.replace(FilterTable::accept_all(), |_, current| {
            if current != &expected {
                return Err(FilterConfigError::Conflict);
            }
            Ok(())
        });
        assert_eq!(result.err(), Some(FilterConfigError::Conflict));
        assert_eq!(store.snapshot().generation, generation);
        assert_eq!(*store.snapshot().table, table);
    }

    #[def_test]
    fn counter_updates_validate_every_chain_before_applying() {
        let store = FilterConfig::new(FilterTable::accept_all());
        let before = store.snapshot();
        let mut updates = before.counters.clone();
        updates[0].end.packets = 7;
        updates[1].rules.push(FilterCounter::default());
        assert_eq!(
            store.add_counters(|_| Ok(updates)),
            Err(FilterConfigError::InvalidCounterShape)
        );
        assert_eq!(store.snapshot().counters, before.counters);
        let mut updates = before.counters;
        updates[0].end = FilterCounter {
            packets: 7,
            bytes: 42,
        };
        assert_eq!(
            store.add_counters(|_| Ok(updates.clone())),
            Ok(before.generation)
        );
        assert_eq!(store.snapshot().counters, updates);
    }
}
