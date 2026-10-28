// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-defconfig Cargo feature resolution, matching `xkmake build`.
//!
//! `cargo tree` runs deduplicated: each package's `{f}` is already the merged
//! feature set, and `--no-dedupe` output grows exponentially with the graph.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use xconfig::build_config::resolve_kernel_config;

use crate::error::{Error, IoResultExt, Result};

/// Lines kept from an inverse `cargo tree` enable path.
const ENABLE_PATH_MAX_LINES: usize = 24;

/// Resolved Cargo features of one defconfig.
#[derive(Clone, Debug, Default)]
pub(crate) struct Resolution {
    /// Defconfig path relative to the workspace root.
    pub(crate) defconfig: String,
    pub(crate) target: String,
    /// Features passed to Cargo, without the `kfeat/` prefix.
    pub(crate) kfeat_features: Vec<String>,
    /// Enabled features of every package in the target graph.
    pub(crate) packages: BTreeMap<String, BTreeSet<String>>,
}

impl Resolution {
    pub(crate) fn is_linked(&self, package: &str) -> bool {
        self.packages.contains_key(package)
    }

    pub(crate) fn cargo_feature_args(&self) -> Vec<String> {
        self.kfeat_features
            .iter()
            .map(|feature| format!("kfeat/{feature}"))
            .collect()
    }
}

/// All `platforms/**/*defconfig` files, sorted.
pub(crate) fn default_defconfigs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![root.join("platforms")];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).with_path(&directory)? {
            let path = entry.with_path(&directory)?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("defconfig"))
            {
                found.push(path);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// Resolves `defconfig` exactly as the kernel build does, then lists the
/// features Cargo enables for the kernel target.
pub(crate) fn resolve(root: &Path, app: &Path, defconfig: &Path) -> Result<Resolution> {
    // Evaluation only reads the seed; no `.config` or generated file is written.
    let config = resolve_kernel_config(defconfig, root.join("Kconfig"), root)?;
    let mut resolution = Resolution {
        defconfig: defconfig
            .strip_prefix(root)
            .unwrap_or(defconfig)
            .to_string_lossy()
            .replace('\\', "/"),
        target: config.target().to_string(),
        kfeat_features: config.cargo_features().to_vec(),
        packages: BTreeMap::new(),
    };
    let mut command = cargo_tree(root, app, &resolution);
    command.args([
        "-e",
        "normal,no-proc-macro",
        "-f",
        "{p}|{f}",
        "--prefix",
        "none",
    ]);
    let packages = &mut resolution.packages;
    stream_lines(command, |line| {
        if let Some((package, features)) = parse_tree_line(line) {
            packages.entry(package).or_default().extend(features);
        }
        true
    })?;
    Ok(resolution)
}

/// Inverse feature tree explaining why `package/feature` is enabled.
pub(crate) fn enable_path(
    root: &Path,
    app: &Path,
    resolution: &Resolution,
    package: &str,
    feature: &str,
) -> Result<Vec<String>> {
    let mut command = cargo_tree(root, app, resolution);
    command.args([
        "-e",
        "features,normal,no-proc-macro",
        "--depth",
        "6",
        "-i",
        package,
    ]);
    let marker = format!("{package} feature \"{feature}\"");
    let mut path = Vec::new();
    let mut is_inside = false;
    stream_lines(command, |line| {
        let depth = tree_depth(line);
        if depth == 1 {
            is_inside = line.contains(&marker);
        }
        if is_inside && depth >= 1 {
            path.push(line.trim_end().to_string());
        }
        path.len() < ENABLE_PATH_MAX_LINES
    })?;
    Ok(path)
}

fn cargo_tree(root: &Path, app: &Path, resolution: &Resolution) -> Command {
    let mut command = Command::new("cargo");
    command
        .current_dir(root)
        // Build-time flag variables do not affect feature resolution but
        // would make a stale environment fail the check.
        .env_remove("RUSTFLAGS")
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env("RUSTC_BOOTSTRAP", "1")
        .args(["tree", "--locked", "--manifest-path"])
        .arg(root.join(app).join("Cargo.toml"))
        .args(["--target", &resolution.target]);
    let features = resolution.cargo_feature_args();
    if !features.is_empty() {
        command.arg("--features").arg(features.join(","));
    }
    command
}

/// Runs `command` and feeds stdout lines to `consume` until it returns false.
fn stream_lines(mut command: Command, mut consume: impl FnMut(&str) -> bool) -> Result<()> {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| Error::Spawn {
            program: "cargo".into(),
            source,
        })?;
    let mut stderr = child.stderr.take();
    // Drain stderr concurrently so a chatty child cannot block on a full pipe.
    let stderr_reader = std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(stderr) = stderr.as_mut() {
            let _ = stderr.read_to_string(&mut text);
        }
        text
    });
    let mut is_consuming = true;
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines() {
            let line = line.map_err(|source| Error::Spawn {
                program: "cargo tree".into(),
                source,
            })?;
            if is_consuming {
                is_consuming = consume(&line);
            }
        }
    }
    let status = child.wait().map_err(|source| Error::Spawn {
        program: "cargo tree".into(),
        source,
    })?;
    let stderr = stderr_reader.join().unwrap_or_default();
    if !status.success() {
        return Err(Error::Message(format!(
            "cargo tree failed ({status}): {}",
            stderr.trim()
        )));
    }
    Ok(())
}

/// Parses `name vX.Y (path)|f1,f2` with an optional ` (*)` suffix.
pub(crate) fn parse_tree_line(line: &str) -> Option<(String, Vec<String>)> {
    let line = line.trim_end();
    let line = line.strip_suffix(" (*)").unwrap_or(line);
    let (package, features) = line.rsplit_once('|')?;
    let name = package.split_whitespace().next()?;
    let features = features
        .split(',')
        .map(str::trim)
        .filter(|feature| !feature.is_empty())
        .map(str::to_string)
        .collect();
    Some((name.to_string(), features))
}

/// Depth of a `cargo tree` line drawn with the default indent prefix.
fn tree_depth(line: &str) -> usize {
    let prefix: String = line
        .chars()
        .take_while(|ch| matches!(ch, ' ' | '│' | '├' | '└' | '─'))
        .collect();
    prefix.chars().count() / 4
}
