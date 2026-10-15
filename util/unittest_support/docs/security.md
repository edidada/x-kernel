# unittest_support — Fixture Lifetime Boundaries

Inputs are trusted kernel test code. The current test thread must own a live user
address space. LiveAddressSpace keeps that mm active during fixture use; this is
separate from the FrameRun/PTE holds keeping physical storage alive.

TestUserBuffer::new validates byte rounding before allocation and uses checked VA
reservation. It writes through a raw pointer only before any mapping or physical
hold clone is published, while the newly allocated run exclusively covers the
entire initialized prefix. The zero-fill unsafe block states those bounds.

MmSpace::map_owned_linear consumes backing even on validation or mapping failure.
Its runtime withdraws a successfully mapped prefix before finishing failure cleanup;
an already occupied conflicting leaf remains untouched. The fixture has no raw
allocation address to double-free. PTE holds survive clearing until TLB finish;
child runtimes and PTEs independently retain inherited backing.

Drop requires that test code has not replaced the fixture's VMA with an unrelated
mapping; failure is a fixture contract violation and fails the test. Physical holds
do not establish payload access synchronization. TestUserValue/TestUserArray still
rely on their callers' existing typed-representation obligations; this migration
only changes page ownership and does not introduce a generic safe serialization API.

Regression coverage checks initialization, logical zero length, page rounding,
read/write bounds, scoped unmap and overflowing lengths in a real test user task.
The mapping-layer tests separately cover fork survival, TLB retirement, conflict
rollback, refault after PROT_NONE and rejection of unmapped rounding slack.
