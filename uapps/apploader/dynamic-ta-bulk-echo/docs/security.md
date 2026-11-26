# dynamic-ta-bulk-echo — Security

## Scope and trust model

This document covers `uapps/apploader/dynamic-ta-bulk-echo/src/main.rs`. The TA
is a test-only echo service; it does not authenticate peers and holds no
persistent state. Its authenticity as a launched TA is decided by the apploader
and kernel path.

## External inputs

- TIPC messages on `com.xkernel.test.dynamic-ta-bulk`, at most `CHUNK_BYTES`
  (4 KiB). The receive buffer is sized by the port configuration, so oversized
  messages are rejected by the TIPC layer before reaching the callback.
- A zero-length message closes the connection.

## Unsafe and FFI inventory

None. The crate uses safe `rust_libtipc` APIs and contains no raw pointers, FFI
calls, or assembly.

## Threat analysis

| ID | Threat | Severity | Trigger | Response and residual risk |
|---|---|---|---|---|
| T-01 | Oversized message | Medium | Peer sends more than 4 KiB | The TIPC port rejects it or the callback sees only the bounded buffer; the connection is closed on error. |
| T-02 | Echo reflection abuse | Low | Peer uses the TA to reflect traffic | The service only echoes to the same channel in a test environment. |
| T-03 | Probe flooding | Low | Repeated connects/sends | Four receive slots bound the queue; no kernel resources are retained after close. |
| T-04 | Corrupted transfer undetected | Low | Transport corrupts bytes | The host CA compares every chunk against a deterministic pattern. |

## Known limitations

- No authentication, rate limiting, or integrity check inside the TA.
- Only useful as a bounded bulk-throughput and ordering probe.

## Audit checklist

- Keep `CHUNK_BYTES` within the TIPC receive-slot bound.
- Keep zero-length messages closing the connection.
- Keep setup fallible; do not reintroduce panicking setup calls.
- Keep the service path and `.ta_head` UUID in sync with the CA and fixtures.
