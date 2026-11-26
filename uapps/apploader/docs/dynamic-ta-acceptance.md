# Dynamic TA loader acceptance

## Scope

This test validates dynamic TA upload and execution on AArch64 QEMU. The
preinstalled apploader TA accepts an ELF image from a host CA, transfers it to
the kernel `.secure` service through SysV shared memory, and the kernel starts a
new TA process.

Trusty assigns image signature and signer-policy verification to the apploader
TA; the kernel validates caller identity, handoff integrity, and ELF structure.
The current acceptance apploader does not yet implement its verification
policy. SHA-256 protects the handoff against mismatch only, so this is not a
production security acceptance.

## Components

| Component | Responsibility |
|---|---|
| [CA](../ca-apploader/src/main.rs) | Reads TA ELFs, uploads them through `VSOCK:<cid>:0`, and probes business ports. |
| [Apploader TA](../src/main.rs) | Publishes `com.android.trusty.apploader`, receives an image, and creates the shared-memory handoff. |
| [Kernel service](../../../tee/apploader_service/src/lib.rs) | Accepts only the fixed apploader UUID, copies and hashes the image, and invokes the launch callback. |
| [Entry wiring](../../../entry/src/main.rs) | Starts `.secure` before PID 1 and connects the callback to `posix-process`. |
| [Dynamic TA creation](../../../posix/process/src/dynamic_ta.rs) | Creates a parentless process and user task from the accepted memory image. |
| [Memory ELF loader](../../../process/kexec/src/elf_mem_loader.rs) | Validates and maps the AArch64 ELF without a VFS file. |

Only the apploader TA is installed in the guest image. Both test TAs remain on
the host and must not appear under `/tee/ta` on disk.

## Data flow

```text
Host CA
  -> vsock port 0 + service name
  -> vsock-TIPC bridge
  -> apploader TA
  -> SysV shared memory + 41-byte LOAD
  -> kernel .secure
  -> posix-process / kexec
  -> dynamic TA process
```

The dynamic TA logical executable identity is `/tee/ta/{uuid}.ta`. The path is
process metadata, not a file created in the guest image.

## Kernel handoff protocol

The apploader creates an `IPC_PRIVATE` segment, writes the image, marks it for
removal with `IPC_RMID`, and sends one TIPC control message:

| Offset | Type | Meaning |
|---:|---|---|
| 0 | `u8` | `LOAD = 0` |
| 1 | `u32` | SysV shmid |
| 5 | `u32` | Image length |
| 9 | `[u8; 32]` | SHA-256 |

The kernel requires exactly 41 bytes, no attached TIPC handles, a nonnegative
shmid, and `0 < length <= 16 MiB`. It copies the complete segment, verifies the
digest, and passes only the private copy to the launcher. The response is eight
bytes: `u32 command | 1` and `u32 status`.

Status values are `0 Ok`, `1 Invalid`, `2 TooLarge`, `3 NoMemory`,
`4 DigestMismatch`, `5 Unsupported`, and `6 LoadFailed`.

The service uses one sequential receiver and a 30-second message timeout. Each
accepted TIPC channel has four 41-byte receive slots; this is not a bound on the
port's pending connection list.

## Build and run

Run from the repository root:

```sh
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig

cp disk.img target/apploader-uapps-test.img
make uapps UAPPS=apploader DISK_IMG=target/apploader-uapps-test.img

cargo build --release \
  --manifest-path uapps/apploader/ca-apploader/Cargo.toml
RUSTFLAGS='-C link-arg=-Wl,--no-gc-sections' \
  cargo build --release --target aarch64-unknown-linux-musl \
  --manifest-path uapps/apploader/dynamic-ta-pingpong/Cargo.toml
RUSTFLAGS='-C link-arg=-Wl,--no-gc-sections' \
  cargo build --release --target aarch64-unknown-linux-musl \
  --manifest-path uapps/apploader/dynamic-ta-bulk-echo/Cargo.toml

make build
make run ACCEL=n DISK_IMG=target/apploader-uapps-test.img
```

After the guest prints `apploader: ready`, run the CA from another terminal:

```sh
uapps/apploader/ca-apploader/target/release/ca-apploader \
  uapps/apploader/dynamic-ta-pingpong/target/aarch64-unknown-linux-musl/release/dynamic-ta-pingpong \
  --second \
  uapps/apploader/dynamic-ta-bulk-echo/target/aarch64-unknown-linux-musl/release/dynamic-ta-bulk-echo
```

## Expected results

```text
ca-apploader: dynamic TA launched: ...dynamic-ta-pingpong
ca-apploader: dynamic TA launched: ...dynamic-ta-bulk-echo
ca-apploader: first dynamic TA responded to ping/pong
ca-apploader: second dynamic TA echoed 1048576 verified bytes
ca-apploader: first dynamic TA responded to ping/pong
```

`ps` shows distinct UUID-based names. Names are truncated by the 16-byte
`comm` limit; `/proc/<pid>/cmdline` contains the full `/tee/ta/{uuid}.ta` value.
`ipcs -m` must report no shared-memory segments after both uploads.

## Known limits

- No signature verification, duplicate-UUID policy, or least-privilege TA
  filesystem/credential derivation.
- No concurrent upload, unload, query, or retry protocol.
- The selected compatibility format is a self-contained static AArch64 ELF.
  External interpreters and dependency-library resolution are rejected; the
  Trusty `.app`/manifest format is intentionally not supported.
- The V11/crosvm deployment path uses the same artifacts but requires a separate
  signed-kernel and protected-VM validation.
