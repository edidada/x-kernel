# `mm/anon` Security

Trust boundary:

- `mm/anon` does not validate user pointers or VMA permissions.
- It only provides stable anonymous object identities and private lineage
  relationships consumed by `mm/memspace`.
- It also owns anonymous shared mapped-view registrations, but it still does
  not mutate page tables directly.
- It now also owns anonymous private mapped-view registrations, including the
  private-anon result side of file-private mappings.

Invariants:

- each `AnonSharedObject` has a stable typed identity
  `VmObjectId::Anon(AnonObjectId)`;
- each `AnonPrivateObject` has a stable typed identity
  `VmObjectId::Anon(AnonObjectId)`;
- related private objects created by `fork_child()`/`cow_child()` retain the
  same `AnonLineageId`;
- shared-anonymous mapped views stay attached to the owning `AnonSharedObject`
  until their registration guards drop;
- private-anonymous mapped views stay attached to the owning
  `AnonPrivateObject` until their registration guards drop;
- shared/private view guards are `KArc` owners and their registries are,
  respectively, `KHashMap` / `KBTreeMap`; a guard or registry allocation
  failure must return `KError::NoMemory` before the `MappingView` is
  published;
- if a registry insertion fails after the local guard was created, the guard
  must drop only after its mutex guard has been released, so unregister cannot
  self-deadlock and no stale id remains visible;
- private registry insertion must reserve every possible B-tree split node
  before the first tree mutation, preserving existing views on OOM;
- private-anon object-side discard events must preserve object-relative byte
  offsets exactly; they must not reuse file-backed truncate/EOF rounding rules;
- first-touch private pages must not become object-visible until the runtime
  has installed the corresponding PTE; if PTE installation or object commit
  fails, the runtime remains responsible for releasing the uncommitted frame;
- private-anon unmap and `MADV_DONTNEED` must not free a frame before the
  relevant present PTEs have been torn down; object-side detach and final page
  release therefore happen in separate phases;
- fork/COW sharing into a child object must not commit new child page slots
  before parent write-protect, child page-table installation, and the relevant
  page-table finalization boundary succeed; failed fork preparation must roll
  back retained page refs automatically;
- COW replacement must not overwrite an anonymous private object slot unless it
  still matches the page handle observed before copy; stale object state must
  become a retry, not a blind replacement;
- private page contents owned by `AnonPrivateObject` must be released exactly
  once across unmap, fork/COW replacement, and `MADV_DONTNEED` discard;
- runtime code must not infer anonymous object identity or private page
  ownership from ad hoc runtime-local frame tables when an explicit
  `Anon*Object` is available.

Current OOM boundary:

- `MappingViewSpec.notifier` is still a legacy `Arc<dyn MappingViewNotifier>`.
  It is created before `register_view()` can consume the spec, so a real OOM
  in that trait-object allocation is not covered by the registry's `KResult`.
- `AnonSharedObject` / `AnonPrivateObject` owners and private page-slot map
  are also still legacy allocations. The fallible view registry must therefore
  be reviewed as a bounded slice, not as a statement that every anonymous
  object operation can already return ENOMEM.
