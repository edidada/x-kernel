# apploader-service security and reliability

## Trust and boundaries

The service trusts only the apploader TA UUID encoded in `start`; other TA
connections are accepted by the port ACL and then closed before a message is
read. Image bytes and the declared digest come from that TA and are not a
signature. Trusty assigns signature and signer-policy verification to the
apploader TA; the kernel intentionally validates identity, handoff integrity,
and executable structure only. The current acceptance apploader has not
implemented its verification policy. The launch callback trusts the service to
deliver a bounded, kernel-owned copy.

There is no local `unsafe`, FFI, or user-pointer access. `read_shm_segment`
performs fault-aware backing reads without a current user process.

## Threats and controls

| Threat | Impact | Control and residual risk |
|---|---|---|
| Untrusted TA submits an image | High | Exact apploader UUID check before reading; a compromised apploader remains trusted. |
| Oversized or malformed request | Medium | Exactly 41 bytes, no TIPC handles, `0 < length <= 16 MiB`, nonnegative shmid, exact segment size. |
| Image changes during handoff | Medium | The kernel copies before hashing and launching; digest detects a mismatched copy but not an untrusted signer. |
| User space claims `.secure` | High | The kernel publishes the port before PID 1 and panics if publication fails. |
| Shared-memory ID is not owned by the caller | High | The helper assumes caller authorization and does not check shmid ownership; the fixed trusted apploader UUID is the current boundary. |

## Failure handling and limits

Per-session errors close the connection and do not stop the receiver. Launch
errors map to fixed status values. Signature verification remains the apploader
TA's responsibility; the acceptance TA has not implemented it. There is no
concurrent-session policy, duplicate-UUID policy, or retry after service startup
failure.

## Audit checklist

- Keep the fixed UUID check before message reads.
- Keep all length and shmid checks before allocation or segment access.
- Copy and hash into kernel-owned memory before invoking the callback.
- Keep `.secure` publication ahead of PID 1.
- Update this document when signature policy or protocol versioning is added.
