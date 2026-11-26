# apploader-service design

## Purpose and scope

`apploader_service` publishes the internal TIPC service
`com.android.trusty.apploader.secure` and receives dynamic TA images from the
preinstalled apploader TA. It validates the transfer, copies the image into
private kernel memory, and passes the owned bytes to a launch callback.

`entry` starts the service before PID 1 and supplies the callback that calls
`posix-process::spawn_dynamic_ta`. The service does not own process creation,
ELF layout, or TA signature policy.

## Architecture and flow

```text
apploader TA
  -> create/write SysV shm
  -> LOAD(command, shmid, length, SHA-256)
  -> apploader_service
       -> fixed-UUID peer check
       -> read_shm_segment
       -> SHA-256 check
       -> LaunchTa(Vec<u8>)
       -> posix-process
  <- 8-byte response(command | 1, status)
```

Each accepted channel has four 41-byte receive slots, while one receiver task
handles sessions serially; additional connections wait in the port pending
queue. The port allows only `ALLOW_TA_CONNECT`. Each accepted connection handles
one `LOAD` and then closes.

`receive_load_request` owns transport framing (no handles, 1..=41 bytes) and
decodes the frame directly into a validated `LoadRequest`. A frame whose command
is readable but whose contents fail validation is answered with
`Invalid`/`TooLarge` and reported as `None`, so the caller only closes the
connection. The wire layout is `u8 command`, `u32 shmid`, `u32 image length`,
and 32-byte SHA-256; it is decoded with a `zerocopy` layout type so field offsets
are not hand-computed.

## Execution and lifecycle

`start` runs in sleepable late initialization, creates and publishes the port,
then spawns the `apploader-secure` kernel task. Publication must succeed before
PID 1 starts so user space cannot claim the kernel service name.

`entry` calls `start` unconditionally on AArch64. The `dynamic_apploader`
Cargo feature, forwarded from `kfeat` when `KFEAT_DYNAMIC_APPLOADER` is
selected, makes the function a no-op otherwise, so builds without it never
publish `.secure` and never reference its Kconfig constant. The implementation
stays compiled and checked in both configurations.

Incoming data may block while `read_shm_segment` reads the backing file. The
receiver waits up to 30 seconds for messages and send capacity. The `IpcPort`
and receiver task live for the kernel lifetime.

## Design decisions and limits

SysV shared memory carries image bytes while TIPC carries only control data.
The kernel copies before launching, so the source segment may be detached after
handoff. The 41-byte protocol has no version field. Signature and signer-policy
verification intentionally belong to the apploader TA; the acceptance TA has not
implemented that policy yet. Current limits include a 16 MiB image bound and no
unload/query protocol.
