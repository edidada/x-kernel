// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fixture-workspace tests for the CG-047 feature checks.

use std::{fs, path::Path};

use super::{
    checks::{self, FeatureContext, Lint},
    kconfig::KconfigModel,
    model::{Ownership, Workspace},
    resolve::{Resolution, parse_tree_line},
};
use crate::hygiene::Finding;

const KFEAT_MANIFEST: &str = r#"[package]
name = "kfeat"
version = "0.1.0"
edition = "2024"

[package.metadata.kfeat]
internal = ["smp"]

[features]
net = ["dep:knet", "knet/net"]
vsock = ["net", "knet/vsock"]
smp = ["kspin/smp"]
platform_kplat_aarch64 = ["smp"]
orphan = ["kspin/orphan"]

[dependencies]
knet = { path = "../knet", optional = true }
kspin = { path = "../kspin" }
"#;

const DEFAULT_KCONFIG: &str = "\
config MACHINE_AARCH64_QEMU
    bool \"QEMU\"

config KFEAT_NET
    bool \"Networking\"

config KFEAT_VSOCK
    bool \"vsock\"
    depends on KFEAT_NET
";

/// Files of a minimal kernel workspace; `peer` is `posix-net`'s manifest tail.
fn write_workspace(root: &Path, kconfig: &str, kfeat: &str, peer: &str, app_features: &str) {
    let files = [
        (
            "Cargo.toml",
            "[workspace]\nresolver = \"2\"\nmembers = [\"kfeat\", \"entry\", \"knet\", \"kspin\", \
             \"posix-net\"]\n"
                .to_string(),
        ),
        ("Kconfig", kconfig.to_string()),
        ("kfeat/Cargo.toml", kfeat.to_string()),
        (
            "entry/Cargo.toml",
            format!(
                "[package]\nname = \"entry\"\nversion = \"0.1.0\"\nedition = \
                 \"2024\"\n\n[features]\n{app_features}\n\n[dependencies]\nkfeat = {{ path = \
                 \"../kfeat\" }}\n"
            ),
        ),
        (
            "knet/Cargo.toml",
            "[package]\nname = \"knet\"\nversion = \"0.1.0\"\nedition = \
             \"2024\"\n\n[features]\nnet = []\nvsock = []\n"
                .to_string(),
        ),
        (
            "kspin/Cargo.toml",
            "[package]\nname = \"kspin\"\nversion = \"0.1.0\"\nedition = \
             \"2024\"\n\n[features]\nsmp = []\norphan = []\nunused = []\n"
                .to_string(),
        ),
        (
            "posix-net/Cargo.toml",
            format!(
                "[package]\nname = \"posix-net\"\nversion = \"0.1.0\"\nedition = \
                 \"2024\"\n\n{peer}\n"
            ),
        ),
    ];
    for (path, text) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, text).unwrap();
        if path.file_name().is_some_and(|name| name == "Cargo.toml") && path.parent() != Some(root)
        {
            let source = path.parent().unwrap().join("src");
            fs::create_dir_all(&source).unwrap();
            fs::write(source.join("lib.rs"), "").unwrap();
        }
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    workspace: Workspace,
    kconfig: KconfigModel,
    ownership: Ownership,
}

impl Fixture {
    fn new(kconfig: &str, peer: &str, app_features: &str) -> Self {
        Self::with_kfeat(kconfig, KFEAT_MANIFEST, peer, app_features)
    }

    fn with_kfeat(kconfig: &str, kfeat: &str, peer: &str, app_features: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        write_workspace(directory.path(), kconfig, kfeat, peer, app_features);
        let workspace = Workspace::load(directory.path()).unwrap();
        let ownership = Ownership::from_package(workspace.kfeat().unwrap()).unwrap();
        let kconfig = KconfigModel::load(directory.path()).unwrap();
        Self {
            _directory: directory,
            workspace,
            kconfig,
            ownership,
        }
    }

    fn standard() -> Self {
        Self::new(DEFAULT_KCONFIG, "", "unittest = []")
    }

    fn context(&self) -> FeatureContext<'_> {
        FeatureContext::new(
            &self.workspace,
            &self.kconfig,
            &self.ownership,
            "entry".to_string(),
        )
    }
}

fn subjects(findings: &[Finding], lint: Lint) -> Vec<String> {
    let mut subjects: Vec<String> = findings
        .iter()
        .filter(|finding| finding.check == lint.name())
        .map(|finding| finding.subject.clone())
        .collect();
    subjects.sort();
    subjects
}

#[test]
fn feature_map_flags_unmapped_features_and_symbols() {
    let kconfig = format!(
        "{DEFAULT_KCONFIG}\nconfig KFEAT_DMA\n    bool \"DMA\"\n\nconfig KFEAT_FS\n    bool \"fs \
         gate\"\n"
    );
    let fixture = Fixture::new(&kconfig, "", "unittest = []");
    let findings = checks::check_feature_map(&fixture.context());
    // `smp` is internal and reachable from the MACHINE-mapped platform
    // feature; KFEAT_FS is a skip key.
    assert_eq!(
        subjects(&findings, Lint::FeatureMap),
        ["KFEAT_DMA", "orphan"]
    );
}

#[test]
fn feature_map_rejects_unreachable_and_stale_internal_entries() {
    let kfeat = KFEAT_MANIFEST.replace(
        "internal = [\"smp\"]",
        "internal = [\"smp\", \"orphan\", \"net\", \"gone\"]",
    );
    let fixture = Fixture::with_kfeat(DEFAULT_KCONFIG, &kfeat, "", "unittest = []");
    let findings = checks::check_feature_map(&fixture.context());
    // `orphan` is internal but unreachable; `net` is mapped; `gone` is unknown.
    assert_eq!(
        subjects(&findings, Lint::FeatureMap),
        ["gone", "net", "orphan"]
    );

    // The unittest entry is a legitimate root for internal features.
    let fixture = Fixture::with_kfeat(DEFAULT_KCONFIG, &kfeat, "", "unittest = [\"kfeat/orphan\"]");
    let findings = checks::check_feature_map(&fixture.context());
    assert_eq!(subjects(&findings, Lint::FeatureMap), ["gone", "net"]);
}

#[test]
fn kconfig_implication_requires_kconfig_prerequisite() {
    let fixture = Fixture::standard();
    assert!(checks::check_kconfig_implication(&fixture.context()).is_empty());

    let kconfig = DEFAULT_KCONFIG.replace("    depends on KFEAT_NET\n", "");
    let fixture = Fixture::new(&kconfig, "", "unittest = []");
    let findings = checks::check_kconfig_implication(&fixture.context());
    assert_eq!(
        subjects(&findings, Lint::KconfigImplication),
        ["vsock -> net"]
    );
}

#[test]
fn kconfig_implication_inherits_menu_if_choice_and_select() {
    for kconfig in
        [
            "config KFEAT_NET\n    bool \"net\"\n\nmenu \"m\"\n    depends on KFEAT_NET\nconfig \
             KFEAT_VSOCK\n    bool \"vsock\"\nendmenu\n",
            "config KFEAT_NET\n    bool \"net\"\n\nif KFEAT_NET && !KFEAT_OTHER\nconfig \
             KFEAT_VSOCK\n    bool \"vsock\"\nendif\n",
            "config KFEAT_NET\n    bool \"net\"\n\nchoice\n    prompt \"c\"\n    depends on \
             KFEAT_NET\nconfig KFEAT_VSOCK\n    bool \"vsock\"\nendchoice\n",
            "config KFEAT_NET\n    bool \"net\"\n\nconfig HELPER\n    bool\n    select \
             KFEAT_NET\n\nconfig KFEAT_VSOCK\n    bool \"vsock\"\n    depends on HELPER\n",
        ]
    {
        let fixture = Fixture::new(kconfig, "", "unittest = []");
        let findings = checks::check_kconfig_implication(&fixture.context());
        assert!(findings.is_empty(), "{kconfig}\n{findings:?}");
    }
    // A disjunction does not force either side.
    let kconfig =
        "config KFEAT_NET\n    bool \"net\"\n\nconfig KFEAT_VSOCK\n    bool \"vsock\"\n    \
         depends on KFEAT_NET || KFEAT_OTHER\n";
    let fixture = Fixture::new(kconfig, "", "unittest = []");
    assert_eq!(
        subjects(
            &checks::check_kconfig_implication(&fixture.context()),
            Lint::KconfigImplication
        ),
        ["vsock -> net"]
    );
}

const PEER_MANIFEST: &str = r#"[features]
vsock = ["knet/vsock"]
local = []

[dependencies]
knet = { path = "../knet", features = ["net"] }
kspin = { path = "../kspin" }

[dev-dependencies]
kspin = { path = "../kspin", features = ["smp"] }
"#;

#[test]
fn peer_forwarding_flags_peer_edges_and_unconditional_features() {
    let fixture = Fixture::new(
        DEFAULT_KCONFIG,
        PEER_MANIFEST,
        "unittest = [\"kfeat/smp\"]\ndbg = [\"kfeat/net\"]",
    );
    let findings = checks::check_peer_forwarding(&fixture.context());
    // Dev-dependency features and `entry/unittest` are allowed.
    assert_eq!(
        subjects(&findings, Lint::PeerForwarding),
        [
            "entry/dbg -> kfeat/net",
            "posix-net -> knet/net",
            "posix-net/vsock -> knet/vsock"
        ]
    );
    let edge = findings
        .iter()
        .find(|finding| finding.subject == "posix-net/vsock -> knet/vsock")
        .unwrap();
    assert_eq!(edge.file, "posix-net/Cargo.toml");
    assert_eq!(edge.line, Some(7));
    assert_eq!(edge.owners, ["vsock"]);
}

fn resolution(packages: &[(&str, &[&str])]) -> Resolution {
    Resolution {
        defconfig: "platforms/test_defconfig".to_string(),
        target: "aarch64-unknown-none-softfloat".to_string(),
        kfeat_features: Vec::new(),
        packages: packages
            .iter()
            .map(|(name, features)| {
                (
                    (*name).to_string(),
                    features
                        .iter()
                        .map(|feature| (*feature).to_string())
                        .collect(),
                )
            })
            .collect(),
    }
}

#[test]
fn capability_bypass_names_the_enabling_edge() {
    let fixture = Fixture::new(DEFAULT_KCONFIG, PEER_MANIFEST, "unittest = []");
    let context = fixture.context();
    let bypass = resolution(&[
        ("entry", &[]),
        ("kfeat", &[]),
        ("posix-net", &[]),
        ("knet", &["net"]),
        ("kspin", &[]),
    ]);
    let findings = checks::check_capability_bypass(&context, &bypass);
    assert_eq!(
        subjects(&findings, Lint::CapabilityBypass),
        ["posix-net -> knet/net"]
    );
    assert_eq!(
        findings[0].defconfig.as_deref(),
        Some("platforms/test_defconfig")
    );
    assert_eq!(
        subjects(
            &checks::report_linked_without_owner(&context, &bypass),
            Lint::LinkedWithoutOwner
        ),
        ["knet"]
    );

    let owned = resolution(&[
        ("kfeat", &["net"]),
        ("posix-net", &[]),
        ("knet", &["net"]),
        ("kspin", &[]),
    ]);
    assert!(checks::check_capability_bypass(&context, &owned).is_empty());
    assert!(checks::report_linked_without_owner(&context, &owned).is_empty());
}

#[test]
fn unused_features_ignore_tested_and_referenced_features() {
    let fixture = Fixture::new(DEFAULT_KCONFIG, PEER_MANIFEST, "unittest = []");
    let mut cfg_features = std::collections::BTreeMap::new();
    cfg_features.insert(
        "posix-net".to_string(),
        checks::source_feature_names("#[cfg(feature = \"local\")]\nfn f() {}"),
    );
    let findings = checks::report_unused_features(&fixture.workspace, &cfg_features);
    // knet/vsock and kspin/{smp,orphan} are referenced by manifests;
    // posix-net/local is tested in source; entry/unittest is the app switch.
    assert_eq!(
        subjects(&findings, Lint::UnusedFeature),
        ["entry/unittest", "kspin/unused"]
    );
}

#[test]
fn build_args_flag_kfeat_literals_and_kernel_feature_flags() {
    let fixture = Fixture::standard();
    let files = vec![(
        "Makefile".to_string(),
        "\
# cargo build --features kfeat/net is only a comment
build:
\tcargo build --manifest-path entry/Cargo.toml \\
\t\t--features kfeat/net
tools:
\tcargo run --manifest-path xtask/Cargo.toml --features x -p xkmake
\tcd tools/foo && cargo build --no-default-features
\tcargo clippy -Fsmp
\tmkfs.fat -F 32 disk.img
\tcp api/kfeat/Cargo.toml /tmp
"
        .to_string(),
    )];
    let findings = checks::check_build_args(&fixture.workspace, &files);
    let lines: Vec<(Option<usize>, &str)> = findings
        .iter()
        .map(|finding| (finding.line, finding.subject.as_str()))
        .collect();
    assert_eq!(
        lines,
        [
            (Some(3), "kfeat/net"),
            (Some(3), "cargo build --features"),
            (Some(8), "cargo clippy -F")
        ]
    );
}

#[test]
fn tree_lines_parse_deduplicated_output() {
    assert_eq!(
        parse_tree_line("kspin v0.3.0-dev (/x/kspin)|smp,stats (*)"),
        Some((
            "kspin".to_string(),
            vec!["smp".to_string(), "stats".to_string()]
        ))
    );
    assert_eq!(
        parse_tree_line("entry v0.3.0-dev (/x/entry)|"),
        Some(("entry".to_string(), Vec::new()))
    );
    assert_eq!(parse_tree_line(""), None);
}
