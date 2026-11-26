# dynamic-ta-pingpong — Security

## Scope and trust model

This document covers `uapps/apploader/dynamic-ta-pingpong/src/main.rs`. The TA
answers only the fixed `ping`/`pong` contract and holds no persistent state.
It is an acceptance fixture launched from an uploaded image, so its own
authenticity is decided by the apploader and kernel path, not by this code.

## External inputs

- TIPC messages on `com.xkernel.test.dynamic-ta`. A payload other than exactly
  `ping` closes the connection.
- The peer UUID is not inspected; any TA or NS client that can reach the port
  may probe it.

## Unsafe and FFI inventory

None. The crate uses safe `rust_libtipc` APIs and contains no raw pointers,
FFI calls, or assembly.

## Threat analysis

| ID | Threat | Severity | Trigger | Response and residual risk |
|---|---|---|---|---|
| T-01 | Unexpected payload | Low | Peer sends anything other than `ping` | The connection closes without a reply. |
| T-02 | Probe flooding | Low | Repeated connects/sends | Bounded by the fixture's two receive slots; this is a test-only service. |
| T-03 | Impersonation of the service | Low | Another TA publishes the same path | TIPC port publication rejects a duplicate live path. |

## Known limitations

- No authentication, rate limiting, or state beyond the connection itself.
- Only useful as a liveness/round-trip probe.

## Audit checklist

- Keep the exact `ping` match and the `pong` reply.
- Keep setup fallible; do not reintroduce panicking setup calls.
- Keep the service path and `.ta_head` UUID in sync with the CA and fixtures.
