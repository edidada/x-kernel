# Watchdog fatal report protocol

These host tests import the production `src/rendezvous/state.rs`. They exercise
concurrent reporter election, sparse frozen target membership, nested NMI
admission, snapshot publication, late collection and stopped-clock termination.
The storage tests also import the production `TerminalSnapshot` and real
`klazy::Once`, supplying synthetic CPU identities, trap frames and output. They
exercise nested collection, immutable first publication and readers racing a
late collector. The imported Once module additionally runs its existing host
tests. No test emulates architecture NMI delivery or hardware stack capture.

Run from the repository root with the host target selected explicitly:

```sh
RUSTFLAGS='' CARGO_ENCODED_RUSTFLAGS='' cargo test \
  --manifest-path io/watchdog/tests/report-protocol/Cargo.toml \
  --target "$(rustc -vV | sed -n 's/^host: //p')" \
  --target-dir target/watchdog-report-protocol
```
