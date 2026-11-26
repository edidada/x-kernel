# ca-apploader — Security

## Scope and trust model

This document covers `uapps/apploader/ca-apploader/src/main.rs`. The CA is run
by a test operator on the host. It trusts the local image paths it is given and
the guest CID argument. It does not authenticate the guest TA or verify the
uploaded image's signature.

## External inputs

- Command-line arguments: image paths, `--second`, and the guest CID. Missing or
  extra arguments are rejected with `InvalidInput`.
- Host files: TA ELF contents. Empty files and files over 16 MiB are rejected
  before connecting.
- TIPC responses from the apploader and probe TAs: exact lengths, command echo,
  status values, `pong`, and echoed bulk bytes are all checked. Short reads map
  to `UnexpectedEof`.

## Unsafe and FFI inventory

The crate contains no `unsafe` blocks and no direct FFI. SysV shared memory and
TIPC are handled by the guest TA and `libtrusty-rs`; the CA only sends bytes.

## Threat analysis

| ID | Threat | Severity | Trigger | Response and residual risk |
|---|---|---|---|---|
| T-01 | Oversized or empty image | Low | Operator passes an invalid file | Rejected before any connection. |
| T-02 | Wrong guest CID or unavailable bridge | Medium | QEMU was started with another CID or no vsock | Connection fails with `ConnectionRefused`. |
| T-03 | Truncated or forged response | Medium | TA or bridge returns a short/incorrect reply | Command echo, status, and payload checks fail the run. |
| T-04 | Untrusted TA image is uploaded | High | Operator points the CA at an arbitrary ELF | The CA forwards it without signature verification; guest-side policy is out of scope for this fixture. |
| T-05 | Bulk echo corruption | Medium | Transport or TA corrupts a chunk | The deterministic pattern comparison detects the first mismatching offset. |

## Known limitations

- No signing, authentication, or replay protection.
- The CID defaults to `103`, matching the QEMU launcher default; a different
  guest CID must be passed explicitly.
- Probes validate only the acceptance services, not general TA behavior.

## Audit checklist

- Keep image size validation before connecting.
- Keep exact command/status validation for every protocol step.
- Keep bulk verification chunk-by-chunk rather than sampling.
- Do not add credentials or production trust claims to this test driver.
