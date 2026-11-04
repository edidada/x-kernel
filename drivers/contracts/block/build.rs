// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Build script for the `block` crate.
//!
//! When `XKERNEL_RAMDISK_IMG` is set (an absolute path to a filesystem image),
//! it is forwarded to the crate via `cargo:rustc-env` so that the
//! `ramdisk-static` feature can embed it with `include_bytes!(env!(...))`.
//!
//! The variable is optional: when the `ramdisk-static` feature is off the
//! gated module that consumes it is not compiled, so leaving the variable
//! unset is perfectly fine.
//!
//! Because `include_bytes!(env!(...))` is opaque to Cargo's dependency
//! tracking, a content fingerprint is also emitted so the crate is rebuilt
//! (and the image re-embedded) whenever the image file changes -- either by
//! regeneration at the same path or by pointing `XKERNEL_RAMDISK_IMG` at a
//! different file.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=XKERNEL_RAMDISK_IMG");

    let Ok(img) = std::env::var("XKERNEL_RAMDISK_IMG") else {
        // No image supplied. When the `ramdisk-static` feature is off that is
        // the expected state and there is nothing to embed; when it is on (a
        // bare `cargo check`, or rust-analyzer which never sees the Makefile's
        // environment) the crate still needs the two `env!()` values, so point
        // it at an empty stub instead of failing the build.
        emit_empty_image_stub();
        return;
    };

    let path = Path::new(&img);
    // `canonicalize` requires the file to exist; fall back to the raw path so
    // that a missing image produces a clear `include_bytes!` error at compile
    // time rather than a build-script failure.
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

    println!("cargo:rustc-env=XKERNEL_RAMDISK_IMG={}", abs.display());

    let Ok(meta) = std::fs::metadata(&abs) else {
        // The variable points at a path that does not exist (typically because
        // the `ramdisk-static` feature is not in use). Emitting
        // `rerun-if-changed` for a missing file would make Cargo flag this
        // build script as stale on every invocation, so we deliberately omit
        // it here. If the feature is later enabled and the image is generated,
        // the resulting `include_bytes!` error at compile time makes the
        // misconfiguration obvious.
        return;
    };

    // The image exists: track it so the crate is rebuilt (and the image
    // re-embedded) whenever it changes. The byte length is also exposed so the
    // source can size its static array without a second `include_bytes!` call.
    println!("cargo:rerun-if-changed={}", abs.display());
    println!("cargo:rustc-env=XKERNEL_RAMDISK_IMG_LEN={}", meta.len());
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| format!("{}:{}", d.as_secs(), d.subsec_nanos()))
        .unwrap_or_else(|| "0".into());
    println!(
        "cargo:rustc-env=XKERNEL_RAMDISK_IMG_FINGERPRINT={}_{}",
        meta.len(),
        mtime
    );
}

/// Points the crate at an empty image when `ramdisk-static` is enabled without
/// an image path.
///
/// This keeps `cargo check`, rust-analyzer and the other non-Makefile entry
/// points compiling. The resulting kernel has no root filesystem, which is why
/// the fallback is announced as a build warning instead of happening silently.
fn emit_empty_image_stub() {
    if std::env::var_os("CARGO_FEATURE_RAMDISK_STATIC").is_none() {
        return;
    }
    let Ok(out_dir) = std::env::var("OUT_DIR") else {
        return;
    };
    let stub = Path::new(&out_dir).join("empty-ramdisk.img");
    if std::fs::write(&stub, []).is_err() {
        return;
    }
    println!("cargo:rustc-env=XKERNEL_RAMDISK_IMG={}", stub.display());
    println!("cargo:rustc-env=XKERNEL_RAMDISK_IMG_LEN=0");
    println!("cargo:rustc-env=XKERNEL_RAMDISK_IMG_FINGERPRINT=0_0");
    println!(
        "cargo:warning=XKERNEL_RAMDISK_IMG is unset while `ramdisk-static` is enabled: embedding \
         an empty ramdisk image. Build through `make build`, or export the image path, to embed a \
         real root filesystem."
    );
}
