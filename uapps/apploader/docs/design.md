# apploader — Design

## Purpose and scope

`uapps/apploader` is the preinstalled acceptance apploader TA. It receives a TA
ELF image from the host CA through the vsock-TIPC bridge and forwards it to the
kernel `.secure` receiver over SysV shared memory. The kernel then creates an
independent dynamic TA process without writing `/tee/ta`.

This is an acceptance fixture, not a production apploader: it does not
implement signature verification, duplicate-UUID policy, or least-privilege
credential derivation. Only this TA is installed by `make uapps`; the fixture
TAs stay on the host.

## Architecture

```text
host CA
  -> VSOCK:<cid>:0 + "com.android.trusty.apploader"
  -> knet vsock-TIPC bridge
  -> this TA (public port)
  -> IPC_PRIVATE SysV segment + 41-byte LOAD
  -> kernel "com.android.trusty.apploader.secure"
  -> posix-process / kexec -> dynamic TA
```

The TA publishes `com.android.trusty.apploader` with `allow_ns_connect`, four
receive slots and `MESSAGE_BYTES = DATA_BYTES + 12`, where `DATA_BYTES` is
4 KiB so one message stays inside TIPC's 8 KiB receive-slot bound. It connects
to `com.android.trusty.apploader.secure` as a client.

## Upload protocol

A connection carries exactly one upload:

| Step | Message | Fields |
|---|---|---|
| BEGIN | 48 bytes | `u32 CMD_BEGIN`, `u32 VERSION=1`, `u64 image_len`, `[u8; 32] SHA-256` |
| DATA | 12 + N bytes | `u32 CMD_DATA`, `u64 offset`, N payload bytes (`N <= 4 KiB`) |
| COMMIT | 4 bytes | `u32 CMD_COMMIT` |

Each response is eight bytes: `u32 command | 1` and `u32 status`. Status values
are `0 Ok`, `1 Invalid`, `2 TooLarge`, `3 NoMemory`, and `6 LoadFailed`. Any
non-OK status closes the connection. BEGIN rejects images outside
`1..=16 MiB`; DATA rejects a mismatched offset, a payload beyond the declared
length, and messages over `MESSAGE_BYTES`.

## Kernel handoff

At COMMIT the TA checks `image.len() == length_bytes`, then:

1. creates `IPC_PRIVATE` shared memory of exactly `length_bytes`;
2. copies the image into the attached segment;
3. marks the segment `IPC_RMID` so it is released when the last attachment goes
   away, including after a crash;
4. sends the 41-byte `LOAD` request (`command`, `shmid`, `length_bytes`,
   digest) and requires the matching eight-byte response.

The kernel copies the segment into a private buffer, re-hashes it, and owns the
launch. Detaching or modifying the source segment after that point does not
change the launched image.

## Execution context and lifecycle

The program is a single-threaded `rust_libtipc` event loop. Service callbacks
run on that thread and may block in TIPC operations. Each connection owns a
`RefCell<Option<Upload>>`; the image buffer is released when the connection
ends. `SharedSegment` detaches on drop. Setup failures are returned by `run()`
and exit the process with a non-zero status instead of panicking.
