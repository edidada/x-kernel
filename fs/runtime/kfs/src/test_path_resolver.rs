// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for PathResolver.

#![cfg(unittest)]

use unittest::{assert, def_test};

use crate::{FsContext, PathResolver, kernel_fs_context};

#[def_test]
fn test_path_resolver_max_symlinks_config() {
    // Test custom max symlinks configuration
    let _resolver = PathResolver::with_max_symlinks(10);
    // Just verify it can be created

    let _resolver = PathResolver::with_max_symlinks(100);

    // Test default
    let _resolver = PathResolver::new();
}

#[def_test]
fn test_path_resolver_clone() {
    // Test that PathResolver can be cloned
    let resolver1 = PathResolver::with_max_symlinks(20);
    let _resolver2 = resolver1.clone();
}

#[def_test]
fn test_path_resolution_stays_below_context_root() {
    let global = kernel_fs_context().lock().clone();
    let jail_root = global.resolve("/tmp").unwrap();
    let jail = FsContext::new(jail_root.clone());

    assert!(jail.resolve("/").unwrap().ptr_eq(&jail_root));
    assert!(jail.resolve("/..").unwrap().ptr_eq(&jail_root));
    assert!(jail.resolve("..").unwrap().ptr_eq(&jail_root));
    assert!(jail.with_current_dir(global.root_dir().clone()).is_err());
    assert!(jail.remove_dir("/").is_err());

    let link_path = "/tmp/.kfs-root-boundary-test-link";
    let _ = global.remove_file(link_path);
    global.symlink("/", link_path).unwrap();
    assert!(
        jail.resolve("/.kfs-root-boundary-test-link")
            .unwrap()
            .ptr_eq(&jail_root)
    );
    global.remove_file(link_path).unwrap();
}
