# dynamic-ta-bulk-echo — Design

## Purpose and scope

`dynamic-ta-bulk-echo` is the bulk-data acceptance fixture. The host CA uploads
it through the apploader, then sends 1 MiB in 4 KiB chunks and verifies every
echoed chunk. The TA never stores the whole transfer; it echoes one message at
a time.

The fixture is not installed in the guest image and is not autostarted.

## Architecture and protocol

The TA publishes `com.xkernel.test.dynamic-ta-bulk` with four receive slots and
`CHUNK_BYTES = 4 KiB`, staying inside TIPC's 8 KiB receive-slot bound. Each
received message is echoed back unchanged; a zero-length message closes the
connection.

Its `.ta_head` carries UUID `40f8e451-b549-4eeb-a907-27a52c8cd95a`, which
becomes `/tee/ta/{uuid}.ta` and the task name in the guest.

## Execution context and lifecycle

The program is a single-threaded `rust_libtipc` event loop. Callbacks run on the
loop thread and may block in TIPC send/receive. Setup failures are returned from
`run()` and exit non-zero; the event loop owns all handles.

## Dependencies

`rust_libtipc` provides the TIPC port and event loop. The crate has no `unsafe`
code and no direct FFI.
