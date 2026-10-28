---
id: CG-047-kconfig-feature-ownership
level: MUST
paths:
  - "Cargo.toml"
  - "**/Cargo.toml"
  - "Kconfig"
  - "Kconfig.*"
  - "**/Kconfig"
  - "**/Kconfig.*"
checks:
  - backend: xkmake
    lint: [feature-map, kconfig-implication, capability-bypass, peer-forwarding, build-args]
  - backend: ai
    triggers:
      - "(^|[\\[,])\\s*[\"'][\\w-]+\\??/[\\w-]+[\"']"
      - "\\bdep:[\\w-]+"
      - "^(?!.*\\bversion\\s*=)(.*[\\s{,])?(default-)?features\\s*="
      - "\\boptional\\s*=\\s*true"
      - "^\\s*default\\s*=\\s*\\[\\s*([\"']|$)"
      - "^\\s*(menu)?config\\s+KFEAT_"
      - "^\\s*(depends on|select|imply|if)\\b.*\\bKFEAT_"
---

# CG-047-kconfig-feature-ownership: Keep system feature policy in Kconfig and assembly in kfeat

- **Rule**: Configurable kernel capabilities MUST take their selection,
  prerequisites, exclusions and default policy from Kconfig. `api/kfeat/Cargo.toml`
  MUST assemble the participating subsystem and platform Cargo features for each
  system capability. Ordinary crates may forward features required by their own
  implementation, but MUST NOT own system-wide selection of peer capabilities.
  Cargo defaults, dependency feature lists and build arguments MUST preserve the
  resolved Kconfig selection rather than enable a disabled system capability.
- **Scope**: Kernel feature declarations, forwarding edges, dependency feature
  lists, defaults, optional dependencies, `KFEAT_*` Kconfig relationships and
  kernel build arguments. A *capability feature* is the target of an edge in
  `api/kfeat/Cargo.toml` (`crate/feature`, `crate?/feature` or `dep:crate`);
  its *owners* are the kfeat features that list it. `make check_hygiene`
  (`xkmake hygiene check`) enforces the Kconfig-to-kfeat mapping, Kconfig
  prerequisites of kfeat edges, owner-less enablement in every platform
  defconfig, peer forwarding and build arguments, including deletions. AI
  review judges only a newly added feature or cross-crate edge that no kfeat
  feature owns: is it a local implementation choice or a system capability
  that belongs in Kconfig and kfeat? Triggers therefore match added lines only.
  [CG-035](CG-035-declared-cfgs.md) owns cfg name/value validity;
  this rule owns configuration policy and assembly boundaries.
- **Why**: Scattered system feature edges create a second configuration policy:
  a selected combination can miss required participants, or Cargo can enable a
  capability that the resolved kernel configuration disables.
- **How to check**:
  1. Run `make check_hygiene`, or `make check_features` for this rule only;
     it resolves every `platforms/**/*defconfig` through the xconfig engine
     and needs no `.config`. `feature-map`: each
     bool/tristate `KFEAT_*` symbol has a same-named lowercase kfeat feature,
     and each kfeat feature is mapped, `platform_kplat_*` or a declared
     internal feature. `kconfig-implication`: a kfeat feature listing another
     mapped one has a Kconfig symbol that depends on or selects the other.
     `capability-bypass`: no defconfig enables a capability feature without one
     of its owners. `peer-forwarding`: outside kfeat and `entry`, an edge to a
     capability feature comes from a capability feature whose owners imply the
     target's owner, and no normal or build dependency's `features` or
     `default` enables one unconditionally. `build-args`: the Makefile, scripts,
     CI, hooks and `.cargo` configuration pass no `kfeat/` feature or Cargo
     feature flag to kernel builds. `xkmake hygiene features --base <rev>`
     lists the enabled features, edges and `KFEAT_*` relations a change adds
     or removes; `--report` adds advisory `linked-without-owner` and
     `unused-feature` items.
  2. Do not repeat these checks. For each added feature, cross-crate edge,
     dependency `features`, `default` or `optional` activation whose target no
     kfeat feature owns, read the feature's consumers and the dependency's
     role; a `crate/feature` string alone does not establish a violation.
  3. Classify it. Local implementation stays with one owner: `kalloc/buddy`
     enabling `alloc-engine/buddy` is wiring inside the `alloc_buddy` assembly.
     A system capability is independently configurable or needs participants
     in several subsystems or platforms: a runtime crate selecting task, RCU
     and platform SMP support is system assembly, and only kfeat forwards SMP
     features between kernel crates.
  4. For a system capability, name the edge, the missing `KFEAT_*` symbol or
     prerequisite, and where it belongs: a Kconfig symbol with its
     constraints, a kfeat feature assembling the participants, and a local
     feature in each participating crate. Report only relationships the PR
     introduces; do not demand a Kconfig symbol for every local feature.
- **Exceptions**: Crate-local implementation features, optional dependency
  activation and third-party library options may stay in their owning manifest
  when they do not choose an independently configurable kernel capability.
  Explicit test entry features such as `entry/unittest`, host tools, standalone
  workspaces and boot-helper implementation options need no matching Kconfig
  symbol; they must still preserve kernel configuration when building a kernel.
  Kconfig-only dependency gates such as `KFEAT_FS`, numeric settings and
  architecture/platform mapping need not have one-to-one Cargo feature names.
  The checks ignore dev-dependencies. List kfeat features without a Kconfig
  symbol, with their source, in `internal` under `[package.metadata.kfeat]`
  in `api/kfeat/Cargo.toml`; this is configuration, not an exception.
  Register each accepted violation in [exceptions.toml](exceptions.toml) as
  an `[[exception]]` with `rule`, `check`, `subject` and a `reason`: after
  the full rule id, the case above that permits it and why it holds, such
  as `crate-local implementation feature: <why the edge is local>`, or
  `TODO(cg): <planned repair>` for legacy wiring awaiting repair. While the
  owner is still open, write `TODO(cg): decide ownership: <candidates>`;
  see [README.md](README.md#registering-an-exception). A stale exception
  fails the check.
- **Counter-example**:

In a hypothetical runtime crate, a new feature selects tracing participants in
two peer subsystems that no kfeat feature owns, so the checks accept it:

```toml
[features]
packet-trace = ["knet?/packet-trace", "ktracing/packet-trace"]
```

The runtime owns a cross-subsystem capability that Kconfig can neither select
nor constrain.

- **Example**:

Declare the prerequisite in Kconfig:

```kconfig
config KFEAT_PACKET_TRACE
    bool "Packet tracing"
    depends on KFEAT_NET
```

Assemble the participants in `api/kfeat/Cargo.toml`:

```toml
[features]
packet_trace = ["knet?/packet-trace", "ktracing/packet-trace"]
```

Each participating crate declares its local implementation feature. `knet?/`
does not enable the optional `knet` dependency; the existing `KFEAT_NET`
selection supplies the networking assembly through `kfeat/net`.
