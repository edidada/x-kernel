# apploader — Security

## Scope and trust model

This document covers `uapps/apploader/src/main.rs`. The TA trusts the host CA's
image by design: it checks transport framing, size bounds, and offsets, but no
signature or signer policy. The kernel `.secure` service authenticates only the
fixed apploader TA UUID and verifies the handoff digest; it does not establish
who produced the image.

The TA is an acceptance fixture. Installing it makes every NS TIPC client able
to upload an image, so it must not be deployed as a production loader.

## External inputs

- TIPC messages from `com.android.trusty.apploader`: BEGIN, DATA, and COMMIT.
  Lengths are bounded by `MAX_IMAGE_BYTES` (16 MiB) and `MESSAGE_BYTES`
  (4 KiB + 12); offsets must match the received byte count.
- Host filesystem input is consumed by the CA, not by this TA.
- The kernel `.secure` response: command echo, status, and exact length are
  validated; a mismatch maps to `LoadFailed`.

## Unsafe and FFI inventory

- `std::ptr::copy_nonoverlapping` copies `length_bytes` from the image `Vec`
  into the attached segment. `on_message` only reaches the copy after
  `image.len() == length_bytes`; `SharedSegment::new` created the segment with
  the same size and keeps it attached for the copy. The copy method is unsafe
  and its contract requires the caller to preserve that bound. The two ranges
  are distinct allocations, and both pointers have alignment 1 because the
  copied elements are `u8`.
- `libc::shmget`/`shmat`/`shmctl(IPC_RMID)`/`shmdt` are standard SysV calls.
  The size is the validated image length, the address is the `shmat` result,
  failure is checked through return values, and `IPC_RMID` makes cleanup valid
  after detach or process death.

## Threat analysis

| ID | Threat | Severity | Trigger | Response and residual risk |
|---|---|---|---|---|
| T-01 | Oversized or malformed upload | Medium | BEGIN advertises > 16 MiB or DATA exceeds the declared length | Rejected with `TooLarge`/`Invalid`; the connection closes. |
| T-02 | Image substitution in transit | High | A peer between CA and TA changes bytes | SHA-256 is computed by the CA and rechecked by the kernel; this protects integrity, not authenticity. |
| T-03 | Local guest process uploads arbitrary ELF | High | Any NS client connects to the public port | Accepted by design for this fixture; the kernel launches it as a TA. Declared acceptance limitation. |
| T-04 | Shared-memory exhaustion | Medium | Repeated large uploads | Segment creation fails and returns `NoMemory`; the connection closes. |
| T-05 | Stale segment after crash | Low | TA exits mid-upload | `IPC_RMID` was issued at creation, so the segment is released with the last attachment. |
| T-06 | TIPC or `.secure` failure | Medium | Bridge or kernel service unavailable | The upload fails with `LoadFailed`; no TA is published. |

## Known limitations

- No signature verification, signer policy, duplicate-UUID policy, or
  least-privilege filesystem/credential derivation.
- No concurrent uploads or unload/query protocol.
- Only self-contained static AArch64 ELF images are supported by the kernel
  loader; `PT_INTERP` and `PT_DYNAMIC` are rejected.

## Audit checklist

- Keep BEGIN/DATA/COMMIT size and offset checks before any allocation or copy.
- Keep `image.len() == length_bytes` as the precondition of the kernel handoff.
- Keep the segment size equal to the declared image length and retain
  `IPC_RMID` at creation.
- Keep the `copy_nonoverlapping` SAFETY rationale in sync with those checks.
- Do not describe this fixture as a production security boundary.
