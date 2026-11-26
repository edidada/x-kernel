# dynamic-ta-pingpong — Design

## Purpose and scope

`dynamic-ta-pingpong` is the minimal dynamic TA fixture. The host CA uploads
this ELF through the apploader; the kernel launches it as a fresh TA process,
and the CA then connects to its service and verifies a basic request/response
round trip.

The fixture is never installed in the guest image and is not autostarted.

## Architecture and protocol

The TA publishes `com.xkernel.test.dynamic-ta` with two receive slots and a
4-byte message size. For each `ping` it replies `pong` on the same channel; any
other payload closes the connection.

Its `.ta_head` carries UUID `8ea3a3d8-c164-4f58-98a2-b4c06d1ba4e7`, which
becomes `/tee/ta/{uuid}.ta` and the task name in the guest.

## Execution context and lifecycle

The program is a single-threaded `rust_libtipc` event loop with an empty
per-connection state. Setup failures from `PortCfg::new`, dispatcher
construction, `add_service`, or `Manager::new_with_dispatcher` are returned from
`run()` and exit non-zero. The event loop owns the port and channel handles.

## Dependencies

`rust_libtipc` provides the TIPC port and event loop. The crate has no `unsafe`
code and no direct FFI.
