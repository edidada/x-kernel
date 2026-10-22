// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic device-capability publication, admission and withdrawal.
//!
//! [`Class`] owns a publication registry, subscriptions and projection hooks.
//! Its registry is private to the class module. [`PublicationSource`] supplies
//! the live device identity without exposing the device model's private state.
//!
//! [`Registration`] owns withdrawal and cannot be cloned. [`Publication`] is a
//! consumer reference whose drop never withdraws. [`InUse`] permits an operation
//! and defers class-side teardown until the last use leaves. All three roles
//! share one publication lifecycle and belong to the publication module.
//!
//! Modules remain private; consumers use the types re-exported at the crate root.

#![no_std]

extern crate alloc;

mod class;
mod owner;
mod publication;

pub use class::{
    Class, ClassAvailabilityCallback, ClassHooks, ClassRemovalCallback, PublicationNode,
};
pub use owner::PublicationSource;
pub use publication::{InUse, Publication, Registration};

unittest::path_mod_test!(tests, "../ut-tests/class.rs");
