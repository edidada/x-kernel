# unittest_support — User Fixture Memory

The crate builds kernel test processes and provides TestUserBuffer,
TestUserValue and TestUserArray for tests that need real mapped user addresses.
It is test infrastructure; raw user pointers are consumed by the existing osvm
access helpers and syscall tests.

TestUserBuffer requires a current test thread and captures its LiveAddressSpace
capability. It rounds len.max(1) to whole base pages with checked arithmetic,
allocates a VirtMem FrameRun and zeroes only the visible prefix before publication.
A checked atomic reservation chooses a user VA. MmSpace::map_owned_linear consumes
the run, retains backing in the mapping runtime and gives every PTE an independent
hold. Buddy rounding slack stays owned but is not mapped into userspace.

The fixture stores its requested length, mapped extent and live-mm capability;
it no longer stores a raw kernel VA or manually frees pages. Drop unmaps the
fixture range through MmSpace, completing translation retirement. A child that
inherited the mapping retains its own VMA/PTE holds after the parent fixture ends.

Linear backing semantics are preserved: mappings share contents across fork and
retain the linear VMA/futex classification. Using shared-anonymous or private-COW
fixtures instead would change the contracts exercised by existing tests.

Construction and teardown run in a live test thread outside spinlocks. Zero-length
buffers still reserve one mapped base page; typed/byte access helpers keep their
existing logical-length and representation contracts. Arc, VMA and PTE index
bookkeeping still follow the current kernel heap allocation-failure policy.
