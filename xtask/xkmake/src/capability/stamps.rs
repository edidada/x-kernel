// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `stamps.json`: what each card was last verified against.
//!
//! Written only by `xkmake capability stamp`, after a human reviewed the
//! card. Never edit by hand. A stamp records token fingerprints of the
//! items a card cites, never file contents or line numbers, so code that
//! moves or changes elsewhere in the same file leaves the card fresh.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use serde::{Deserialize, Serialize};

use super::{card::SCHEMA_VERSION, items::ItemHashes};

pub(crate) const STAMPS_FILE: &str = "stamps.json";

/// Fingerprint of one cited item.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ItemStamp {
    /// Source reference of the file the item was found in.
    pub(crate) path: String,
    /// Path of the item inside that file (modules, owner, name), joined
    /// with `::`.
    pub(crate) chain: String,
    #[serde(flatten)]
    pub(crate) hashes: ItemHashes,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct CardStamp {
    /// `HEAD` when the card was stamped; the start of the stale-age window.
    #[serde(default)]
    pub(crate) commit: String,
    /// Symbol FQN -> fingerprint.
    #[serde(default)]
    pub(crate) symbols: BTreeMap<String, ItemStamp>,
    /// Evidence item path -> fingerprint.
    #[serde(default)]
    pub(crate) items: BTreeMap<String, ItemStamp>,
}

/// Package names known when discovery was last baselined; new ones become
/// "consider a card" hints.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DiscoveryBaseline {
    #[serde(default)]
    pub(crate) registry_packages: BTreeSet<String>,
    #[serde(default)]
    pub(crate) workspace_packages: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Stamps {
    pub(crate) schema: i64,
    #[serde(default)]
    pub(crate) discovery: DiscoveryBaseline,
    #[serde(default)]
    pub(crate) cards: BTreeMap<String, CardStamp>,
}

impl Stamps {
    /// Reads `stamps.json`. With `strict`, a stamp file of another schema
    /// is an error; otherwise its card stamps are dropped (they cannot be
    /// compared) and only the discovery baseline is kept, for re-stamping.
    pub(crate) fn load(root: &Path, strict: bool) -> Result<Self, String> {
        let path = root.join(STAMPS_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    schema: SCHEMA_VERSION,
                    ..Self::default()
                });
            }
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        let value: serde_json::Value =
            serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
        let schema = value["schema"].as_i64().unwrap_or(0);
        if schema == SCHEMA_VERSION {
            return serde_json::from_value(value)
                .map_err(|error| format!("{}: {error}", path.display()));
        }
        if strict {
            return Err(format!(
                "{}: schema {schema}, expected {SCHEMA_VERSION}; re-stamp the converted cards \
                 with `cargo xkmake capability stamp --all`",
                path.display()
            ));
        }
        let discovery = serde_json::from_value(value["discovery"].clone()).unwrap_or_default();
        Ok(Self {
            schema: SCHEMA_VERSION,
            discovery,
            cards: BTreeMap::new(),
        })
    }

    pub(crate) fn to_json(&self) -> Result<String, String> {
        let mut text = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        text.push('\n');
        Ok(text)
    }
}
