// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::cell::{Cell, RefCell};

use unittest::{assert, assert_eq, def_test};

use super::{MappingFlags, TextPatchError, with_writable_text};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProtectError {
    EnableWrite,
    Restore,
}

#[derive(Debug, PartialEq)]
enum PatchEvent {
    Protect(MappingFlags),
    Write,
}

#[def_test]
fn text_patch_restores_protection_after_writing() {
    let old_flags = MappingFlags::READ | MappingFlags::EXECUTE;
    let events = RefCell::new(alloc::vec::Vec::new());
    let result = with_writable_text(
        old_flags,
        |flags| {
            events.borrow_mut().push(PatchEvent::Protect(flags));
            Ok::<(), ProtectError>(())
        },
        || events.borrow_mut().push(PatchEvent::Write),
    );
    assert_eq!(result, Ok(()));
    assert_eq!(
        *events.borrow(),
        alloc::vec![
            PatchEvent::Protect(old_flags | MappingFlags::WRITE),
            PatchEvent::Write,
            PatchEvent::Protect(old_flags),
        ]
    );
}

#[def_test]
fn failed_write_enable_restores_changed_protection_without_writing() {
    let old_flags = MappingFlags::READ | MappingFlags::EXECUTE;
    let current_flags = Cell::new(old_flags);
    let did_write = Cell::new(false);
    let result = with_writable_text(
        old_flags,
        |flags| {
            // Model PTE changes taking effect before completion reports failure.
            current_flags.set(flags);
            if flags.contains(MappingFlags::WRITE) {
                Err(ProtectError::EnableWrite)
            } else {
                Ok(())
            }
        },
        || did_write.set(true),
    );
    assert_eq!(result, Err(TextPatchError::EnableWrite(ProtectError::EnableWrite)));
    assert_eq!(current_flags.get(), old_flags);
    assert!(!did_write.get());
}

#[def_test]
fn failed_restore_is_reported_after_a_successful_patch() {
    let old_flags = MappingFlags::READ | MappingFlags::EXECUTE;
    let did_write = Cell::new(false);
    let result = with_writable_text(
        old_flags,
        |flags| {
            if flags.contains(MappingFlags::WRITE) {
                Ok(())
            } else {
                Err(ProtectError::Restore)
            }
        },
        || did_write.set(true),
    );
    assert_eq!(result, Err(TextPatchError::Restore(ProtectError::Restore)));
    assert!(did_write.get());
}

#[def_test]
fn failed_restore_takes_precedence_over_failed_write_enable() {
    let old_flags = MappingFlags::READ | MappingFlags::EXECUTE;
    let did_write = Cell::new(false);
    let result = with_writable_text(
        old_flags,
        |flags| {
            if flags.contains(MappingFlags::WRITE) {
                Err(ProtectError::EnableWrite)
            } else {
                Err(ProtectError::Restore)
            }
        },
        || did_write.set(true),
    );
    assert_eq!(result, Err(TextPatchError::Restore(ProtectError::Restore)));
    assert!(!did_write.get());
}
