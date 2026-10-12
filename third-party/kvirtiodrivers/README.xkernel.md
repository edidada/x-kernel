# X-Kernel DMA integration

Vendored from the MIT-licensed `kvirtiodrivers` 0.13.4 registry release.
The original license and source attribution are retained in this directory.

Local extensions add an optional device-bound DMA provider to Transport.
Queues and GPU DMA allocations retain that provider through destruction.
Descriptor mappings are prepared transactionally before publication, including
rollback when any buffer or indirect descriptor table fails to map.
The wire protocol and static HAL MMIO interface are unchanged.

This is a standalone upstream workspace, excluded from kernel lint policy.
Validate with `cargo test --manifest-path third-party/kvirtiodrivers/Cargo.toml`.
