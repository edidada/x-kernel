# Apploader dynamic TA acceptance

This uapp bundles the preinstalled apploader TA with its host CA and two
dynamic TA fixtures:

- [`src/`](src) is the apploader TA. It is the only program in this directory
  that is built by `make uapps`, installed into the guest image, and
  autostarted.
- [`ca-apploader/`](ca-apploader) is the host CA. It uploads TA ELF images
  through the guest's vsock-TIPC bridge and probes their business ports.
- [`dynamic-ta-pingpong/`](dynamic-ta-pingpong) is uploaded by the CA and
  verifies the basic `ping`/`pong` service path.
- [`dynamic-ta-bulk-echo/`](dynamic-ta-bulk-echo) is uploaded by the CA and
  verifies a 1 MiB transfer in 4 KiB echo messages.
- [`docs/dynamic-ta-acceptance.md`](docs/dynamic-ta-acceptance.md) describes
  the implementation, protocol, and complete acceptance procedure.

The CA and both dynamic TAs are test build inputs only. They are intentionally
not listed in [`uapp.toml`](uapp.toml), installed into the guest image, or
started by the guest autostart hook.

## Build and run

Run these commands from the repository root:

```sh
cp platforms/kplat-aarch64/qemu_defconfig .config
make defconfig
cp disk.img target/apploader-uapps-test.img
make uapps UAPPS=apploader DISK_IMG=target/apploader-uapps-test.img

cargo build --release \
  --manifest-path uapps/apploader/ca-apploader/Cargo.toml
CC=aarch64-linux-musl-gcc \
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc \
  RUSTFLAGS='-C link-arg=-Wl,--no-gc-sections' \
  cargo build --release --target aarch64-unknown-linux-musl \
  --manifest-path uapps/apploader/dynamic-ta-pingpong/Cargo.toml
CC=aarch64-linux-musl-gcc \
  CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc \
  RUSTFLAGS='-C link-arg=-Wl,--no-gc-sections' \
  cargo build --release --target aarch64-unknown-linux-musl \
  --manifest-path uapps/apploader/dynamic-ta-bulk-echo/Cargo.toml

make build
make run ACCEL=n DISK_IMG=target/apploader-uapps-test.img
```

After the guest prints `apploader: ready`, run the host CA from another
terminal:

```sh
uapps/apploader/ca-apploader/target/release/ca-apploader \
  uapps/apploader/dynamic-ta-pingpong/target/aarch64-unknown-linux-musl/release/dynamic-ta-pingpong \
  --second \
  uapps/apploader/dynamic-ta-bulk-echo/target/aarch64-unknown-linux-musl/release/dynamic-ta-bulk-echo
```

This acceptance path does not provide signature verification or production
security coverage.
