# `mm/anon` Design

Kernel unit tests live in `ut-tests/` and are mounted from their original
source modules with `unittest::path_mod_test!` under `cfg(unittest)`.

`mm/anon` owns the Linux-aligned anonymous object boundary.

Current scope:

- `AnonSharedObject`: stable owner for shared anonymous mappings.
- `AnonPrivateObject`: stable owner for private anonymous mappings.
- `AnonLineageId`: fork/COW lineage identity shared by related private objects.
- shared-anon mapped views registered through the common `mm/vmobj`
  `MappingView` language.
- private-anon mapped views registered through the same `mm/vmobj`
  `MappingView` language.

Module boundary:

- `mm/anon` owns anonymous object identity and lineage.
- `mm/anon` also owns the anonymous branch of object-side mapped-view/rmap
  registration.
- `mm/anon` now also owns private-anon page slots and their fork/COW sharing
  state.
- anonymous object identity is typed separately from file-backed object
  identity through `VmObjectId::Anon(AnonObjectId)`.
- `mm/memspace` consumes these objects from runtime code to describe VMA
  backing and fork/COW relationships.
- `mm/vmobj` remains the shared object/rmap language layer for both file-backed
  and anonymous reverse-mapping infrastructure.

Runtime role:

- anonymous shared/private runtimes use stable object identities;
- fork/COW flows through explicit anonymous lineage rather than runtime-local
  state alone.
- anonymous shared mappings register formal object-side views through the same
  `vmobj` language that file-backed mappings use.
- anonymous private mappings register views so fork/COW descendants and
  file-private anonymous result pages live in one private-anon object/rmap
  world.
- file-private mappings expose their private anonymous result object and
  lineage through `VmArea.backing()`.
- private-anon objects produce object-side invalidation work for operations
  such as `madvise(MADV_DONTNEED)`.
- `AnonPrivateObject` owns actual private page contents: page slots,
  fork-shared COW references, and `MADV_DONTNEED` discard state live in the
  object layer.
- private-anon page-state updates are transactional:
  - prepare first-touch page publication in `mm/anon`, install the PTE in the
    runtime, then commit the page slot into `AnonPrivateObject`;
  - detach object slots before unmapping and finalize COW slot counts after the
    runtime finishes tearing down visible PTEs; physical lifetime also has
    independent mapping holds;
  - prepare fork-shared page state first, then commit it into the child
    object only after parent write-protect, child page-table installation, and
    the page-table finalization boundary succeed.

First-touch contract:

- `AnonPrivateObject::prepare_first_touch_page()` validates that the object
  offset is currently empty but does not publish a page slot.
- `PreparedAnonPrivatePage::commit()` publishes the frame into the object only
  after the runtime has installed the PTE.
- the runtime transfers its freshly allocated `FrameRun` into commit. Failure
  drops this object hold automatically. A visible PTE has its own cloned hold
  and must still be withdrawn and invalidated by the runtime.
- direct page-slot installation is not part of the public object API; external
  runtime code must use the prepare/commit path.

Fork-share contract:

- `AnonPrivateObject::prepare_fork_child_pages()` retains existing private
  page slots but does not publish them into the child object.
- the runtime write-protects the parent PTE, maps the child PTE, and finalizes
  both page-table mutation batches before committing the child object slots.
- if any step fails before commit, the prepared fork state drops and releases
  retained page references; runtime rollback restores parent PTE flags and
  removes child PTEs installed during the failed transaction.

COW replacement contract:

- runtime code must use `replace_page_if_same_after()` when publishing a COW
  replacement page into an `AnonPrivateObject`.
- the object slot is rechecked against the page handle observed before copy;
  if the slot changed, the runtime must drop the prepared frame and retry the
  fault instead of overwriting the competing slot.
- the runtime-provided commit closure performs the page-table compare/replace
  step; object publication happens only after that closure succeeds.


Physical lifetime and COW sharing:

- `AnonPrivatePageState` owns a `FrameRun`. Its `refs` counts object slots and
  prepared fork slots, solely for COW exclusivity; it does not count PTEs or
  transient `AnonPrivatePageHandle` snapshots.
- Cloned handles keep the state and backing alive for copying even after its
  last object slot is detached. `try_clone_frame` creates an independent mapping
  hold covering the entire leaf. PTE removal keeps that hold until TLB retirement.
- `AnonPrivateReleasedPage` is an owning deferred handle, not an address to free.
  Ending it or a snapshot releases backing only when all state and mapping holds
  are gone. Runtime cleanup drops these handles rather than calling raw buddy.
- `AnonPrivateObject::drop` decrements every remaining slot count outside its
  page-map lock. This restores a surviving child's COW exclusivity and ends the
  destroyed object's state holds; independent PTE holds remain valid.
- COW publication consumes new backing and uses `replace_owned_if_same` for the
  PTE transaction. Old mapping holds remain live through the flush, while old
  object/snapshot holds protect the source during copying and publication.
