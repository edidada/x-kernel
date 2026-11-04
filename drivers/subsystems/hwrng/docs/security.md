# hwrng — Security and reliability

## Trust model and external boundaries

Hardware bytes are untrusted input. This crate does not touch MMIO, user
pointers or DMA, and provides no entropy-quality guarantee. Consumers decide
whether to credit returned bytes as entropy. Implementations must validate
device lengths before copying into caller-owned buffers.

The hwrng registry owns admission and in-flight accounting independently of
device-online state. driver_model supplies only PublicationSource metadata,
not generic withdrawal. Model publishers supply provenance retaining the
original driver's immutable identity, not the device's later binding name.

## Memory and thread safety

The contract contains no unsafe code. Drivers serialize consumption so concurrent
readers cannot consume the same bytes twice. In-flight DMA uses driver-owned
storage retained across wait errors. Host completion is Send + Sync and may
be notified in IRQ context without allocation or sleeping.

## Threats and failure handling

| Failure | Effect | Required control |
| --- | --- | --- |
| Notification races with sleep | Reader waits indefinitely | Register before predicate; preserve wake hint across park |
| Malformed device length | Buffer overrun | Driver validates length; successful count cannot exceed buffer |
| Device never completes and reports no fault | Reader remains asleep | No timeout or task-interruption exit exists in this contract; hardware progress needs separate monitoring |

`KError::InvalidInput` and `NoMemory` can arise from the wait provider
without a hardware fault. Since the shared error no longer records its origin,
a consumer must check `HwrngDevice::is_failed` before permanently disabling
the source on a read error. Terminal state is monotonic; host wait failures
never establish it. False does not guarantee eventual completion. A wait error does not cancel an in-flight DMA request.
No random bytes are logged by the contract. Source health tests, DMA teardown,
and malicious-device isolation remain outside this crate.

## Audit checklist

Check caller-buffer lifetime, exclusive consumption, wait-error cleanup,
notification outside device locks, and task-context validation in each provider.

## Consumer capability isolation

HwrngHandle exposes only metadata and guarded RNG operations, not raw T or
Arc<dyn HwrngDevice>. Both reads and failure queries hold an operation permit;
after withdrawal they return BadState without calling the driver. Consumers
must distinguish unavailable publication from terminal device failure. Entropy
stops its selected collector for either condition, without inventing a hardware
failure flag. Registration drop does not cancel outstanding DMA or replace the
driver's quiesce/join protocol.

## Synchronous unregistration

Admission closure and count increments use the same short SpinNoPreempt lock.
Only task-context handle use is supported; read and unregister require sleepable
context. All driver calls, host waits and notifications happen outside source
and registry locks. Runtime destructors must not perform sleeping teardown on a
last consumer reference; drivers must release active resources explicitly.

RngDrain is a trusted host contract, not interchangeable with fallible read wait
providers. Preparation failure invokes the predicate zero times and changes no
registration state. After its first invocation, a conforming provider must drain
to completion. Violating this contract is not recoverable via a partial-unregister
retry protocol. The X-Kernel adapter validates before checking/registering and
uses a stack CompletionWait node; no new allocation is required during drain.

Removed callbacks run after in-flight operations reach zero and before unregister
returns. They cannot wake reads needed for that drain; the driver must do so
first. They must restore execution context and avoid reverse dependencies on
the publisher's management lock. Notification does not mean DMA/IRQ teardown
has completed. Credential Drop deliberately leaves the registry/runtime alive;
this observable leak is a publisher bug, not an automatic recovery path.
