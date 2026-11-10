// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! of_device_id matching semantics: a driver entry matches if it names
//! ANY of the node's compatible strings — SoC DT nodes commonly spell
//! `compatible = "vendor,soc-widget", "vendor,generic-ip"` and the generic IP
//! driver must still claim the device.

use unittest::{assert_eq, def_test};

use super::{MatchScore, OfMatch, PlatformBus, PlatformBusData, PlatformId};
use crate::bus::Bus;

fn node(compatibles: &[&str]) -> PlatformBusData {
    PlatformBusData::firmware_node(0, compatibles)
}

#[def_test(serial)]
fn of_match_hits_any_compatible_not_just_primary() {
    // Neutral fixture strings on purpose: rule L0-a says this crate
    // names no device family, and the check is mechanical.
    let data = node(&["vendor,soc-widget", "vendor,generic-ip"]);
    let row = PlatformId::Of(OfMatch::compatible("vendor,generic-ip"));
    assert_eq!(PlatformBus::match_device(&data, &row), MatchScore::Exact);
}

#[def_test(serial)]
fn of_match_misses_when_no_compatible_agrees() {
    let data = node(&["vendor,soc-uart"]);
    let row = PlatformId::Of(OfMatch::compatible("vendor,other-uart"));
    assert_eq!(PlatformBus::match_device(&data, &row), MatchScore::NoMatch);
}

#[def_test(serial)]
fn alias_match_hits_only_board_static_identity() {
    let data = PlatformBusData::static_config(0, "board-alias0");
    let hit = PlatformId::Alias(super::AliasMatch::new("board-alias0"));
    let miss = PlatformId::Of(OfMatch::compatible("serial0"));
    assert_eq!(PlatformBus::match_device(&data, &hit), MatchScore::Exact);
    assert_eq!(PlatformBus::match_device(&data, &miss), MatchScore::NoMatch);
}

#[def_test(serial)]
fn modalias_follows_first_compatible() {
    use crate::bus::BusDeviceData as _;
    let data = node(&["vendor,generic-ip"]);
    let modalias = data.modalias();
    assert_eq!(
        modalias.as_deref(),
        Some("of:NcompatT(null)Cvendor,generic-ip")
    );
    let static_data = PlatformBusData::static_config(0, "ramdisk");
    let none = static_data.modalias();
    assert_eq!(none, None);
}
