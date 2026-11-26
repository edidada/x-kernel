# ca-apploader — Design

## Purpose and scope

`ca-apploader` is the host-side acceptance CA for the dynamic TA path. It reads
TA ELF files from the host filesystem, uploads them through the vsock-TIPC
bridge to the preinstalled apploader TA, and then probes the uploaded TAs'
business services.

It is a test driver only. It does not sign images and does not implement the
production apploader's trust policy.

## Architecture

```text
ca-apploader (host)
  -> libtrusty-rs TipcChannel "VSOCK:<cid>:0"
  -> knet vsock-TIPC bridge
  -> apploader TA "com.android.trusty.apploader"
```

The first positional argument is the first TA image. An optional
`--second <image>` argument uploads a second TA; the remaining optional argument
is the guest CID (default `103`). After uploading, the CA connects to
`com.xkernel.test.dynamic-ta` for the ping/pong probe and, when a second image
was given, to `com.xkernel.test.dynamic-ta-bulk` for the bulk echo probe.

## Upload flow

`upload_image` mirrors the apploader protocol:

1. read the file and require `1..=16 MiB`;
2. connect and send the 48-byte BEGIN (`VERSION=1`, length, SHA-256);
3. send DATA messages of at most 4 KiB with a 12-byte header (`CMD_DATA`,
   byte offset), requiring an OK response for each;
4. send COMMIT and require the matching status.

Every response is validated for command echo and `status == 0`; a mismatch is
reported as an `io::Error` and the process exits non-zero.

## Probes

- Ping/pong sends `ping` and requires exactly `pong`.
- Bulk echo sends 1 MiB in 4 KiB chunks with a deterministic byte pattern and
  compares every echoed chunk in order.

Both probes fail on a short read, a mismatched payload, or a wrong service.

## Execution context and lifecycle

The CA is a single-threaded blocking host process. Each upload and probe owns
its own `TipcChannel`; channels close on drop. Setup and protocol failures are
returned as `io::Result` and terminate with status 1.
