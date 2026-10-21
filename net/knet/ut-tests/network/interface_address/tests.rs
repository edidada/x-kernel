// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

fn entry(address: [u8; 4], prefix: u8) -> Ipv4AddrEntry {
    Ipv4AddrEntry {
        addr: Ipv4Cidr::new(
            Ipv4Address::new(address[0], address[1], address[2], address[3]),
            prefix,
        )
        .unwrap(),
        scope: 0,
        broadcast: None,
    }
}

#[def_test]
fn replacing_primary_preserves_attributes_and_order_on_failure() {
    let mut addresses = Ipv4AddressSet::default();
    let primary = entry([192, 0, 2, 7], 24);
    let secondary = entry([192, 0, 2, 7], 16);
    assert_eq!(
        addresses.replace_primary(primary),
        Err(LinuxError::EADDRNOTAVAIL)
    );
    assert_eq!(addresses.remove_primary(), None);
    addresses.insert(primary).unwrap();
    addresses.insert(secondary).unwrap();
    for (replacement, error) in [
        (secondary, LinuxError::EEXIST),
        (entry([0, 0, 0, 0], 32), LinuxError::EINVAL),
    ] {
        assert_eq!(addresses.replace_primary(replacement), Err(error));
        assert_eq!(addresses.as_slice(), &[primary, secondary]);
    }
    let replacement = Ipv4AddrEntry {
        scope: 254,
        broadcast: Some(Ipv4Address::new(198, 51, 100, 111)),
        ..entry([198, 51, 100, 7], 24)
    };
    assert_eq!(addresses.replace_primary(replacement), Ok(primary));
    assert_eq!(addresses.as_slice(), &[replacement, secondary]);
    assert!(!addresses.contains_entry(primary.addr));
    assert_eq!(addresses.remove_primary(), Some(replacement));
    assert_eq!(addresses.primary_entry(), Some(secondary));
    assert_eq!(addresses.remove(secondary.addr), Some(secondary));
    assert_eq!(addresses.remove(secondary.addr), None);
    assert!(addresses.as_slice().is_empty());
}

#[def_test]
fn insertion_validates_unicast_and_preserves_existing_attributes() {
    let mut addresses = Ipv4AddressSet::default();
    let primary = entry([192, 0, 2, 7], 24);
    assert_eq!(
        addresses.insert(entry([0, 0, 0, 0], 32)),
        Err(LinuxError::EINVAL)
    );
    assert_eq!(
        addresses.insert(entry([255, 255, 255, 255], 32)),
        Err(LinuxError::EINVAL)
    );
    assert!(addresses.as_slice().is_empty());
    assert_eq!(addresses.insert(primary), Ok(true));
    assert_eq!(
        addresses.insert(Ipv4AddrEntry {
            scope: 254,
            ..primary
        }),
        Ok(false)
    );
    assert_eq!(addresses.as_slice(), &[primary]);
}

#[def_test]
fn source_selection_prefers_covering_then_first_scope_eligible_address() {
    let mut addresses = Ipv4AddressSet::default();
    let primary = entry([192, 0, 2, 7], 24);
    let secondary = entry([198, 51, 100, 7], 24);
    addresses.insert(primary).unwrap();
    addresses.insert(secondary).unwrap();
    let next_hop = Ipv4Address::new(198, 51, 100, 8).into();
    let unmatched = Ipv4Address::new(203, 0, 113, 8).into();
    assert_eq!(
        addresses.select_source(&next_hop, 0),
        Some(secondary.addr.address())
    );
    assert_eq!(
        addresses.select_source(&unmatched, 0),
        Some(primary.addr.address())
    );
    addresses
        .replace_primary(Ipv4AddrEntry {
            scope: 254,
            ..primary
        })
        .unwrap();
    assert_eq!(
        addresses.select_source(&unmatched, 0),
        Some(secondary.addr.address())
    );
    assert_eq!(addresses.remove(secondary.addr), Some(secondary));
    assert_eq!(addresses.select_source(&next_hop, 0), None);
    assert_eq!(
        addresses.select_source(&next_hop, 254),
        Some(primary.addr.address())
    );
}
