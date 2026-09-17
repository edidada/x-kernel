# kns design

Kernel unit tests live in `ut-tests/` and are mounted from their original
source modules with `unittest::path_mod_test!` under `cfg(unittest)`.

## Purpose and background

`kns` bundles process namespace references in `NsProxy`, exposes namespaces as
first-class file descriptors through `NsFd`, and owns the two-phase `unshare`
preparation that the syscall layer commits. Process runtime code
owns/replaces the bundle; syscall code parses flags and authorizes operations.
Mount trees belong to `kvfs::MntNamespace`, process paths to
`fs_context::FsStruct`, task-active PID identity to `kidentity`, credential/user
namespace semantics to `kcred`, and cgroup hierarchy/controller state to
`kcgroup`. `NsProxy` does not duplicate those owners.

## Scope and architecture

`src/lib.rs` exports modules `error`, `ipc`, `net`, `nsfd`, `nsproxy`, `pid`,
`time`, `types`, and `uts`. It re-exports `CgroupNamespace`, `NamespaceId`,
`UserNamespace`, `MntNamespace`, and `PidNamespace` from their owner crates.

```text
process runtime -> Arc<NsProxy>
     +-> mnt_ns: Arc<MntNamespace>
     +-> uts_ns: Arc<UtsNamespace> -> RwLock<UtsInner>
     +-> ipc_ns: Arc<IpcNamespace> -> NamespaceId
     +-> pid_ns_for_children: Arc<PidNamespace>
     +-> net_ns: Arc<NetNamespace> -> NamespaceId
     +-> cgroup_ns: Arc<CgroupNamespace>
     +-> time_ns / time_ns_for_children: Arc<TimeNamespace>

NsFd (namespace file descriptor)
     +-> exactly one namespace reference, pinned for the fd lifetime
     +-> anon_inode:[<type>_ns] display path, reads EOF, writes denied
```

`NamespaceFlags` represents Linux creation bits. `NamespaceType` supplies names
for namespace displays. IPC/net/time types hold IDs, not their managers. The
current `posix-ipc` implementation still uses global message/shared-memory
managers without keying them by this IPC identity. Creating a new IPC namespace
ID therefore does not currently isolate those resources. This crate does not
own their queues, segments or manager migration.

### NsFd

`nsfd.rs` defines `NsFd` with one variant per namespace type (`Uts`, `Ipc`,
`Mnt`, `Pid`, `Net`, `User`, `Cgroup`), each holding `Arc` to the namespace
object. Key behavior:

- `ns_type()` maps to `NamespaceType`; `namespace_flag()` maps to the
  `CLONE_NEW*` flag that creates the type.
- Opening `/proc/<pid>/ns/<type>` (procfs `ThreadNsDir`) materializes an
  `NsFd` snapshot of the process's current namespace and attaches it as the
  open description's private data, so `setns(2)` downcasts it.
- Display names follow Linux's anonymous-inode convention:
  `anon_inode:[uts_ns]`, `[ipc_ns]`, `[mnt_ns]`, `[pid_ns_for_children]`,
  `[net_ns]`, `[user_ns]`, `[cgroup_ns]`.
- Reads return EOF, writes are denied (the fd is a capability handle only).

## Execution context

Bundle creation/cloning, unshare preparation, and UTS operations require a
working allocator and sleepable locks and are intended for normal task context.
They are not interrupt or atomic-context primitives. `new_initial` requires boot
to have installed the initial VFS mount namespace; the explicit-mount
constructor avoids that global lookup. There is no current-task lookup,
CPU-local state, or MMIO here. For `clone_for_child` NEWNS, callers provide
exclusive access to a private `FsStruct` whose root and pwd are already
initialized.

## Construction and clone flow

`new_initial` caches a bundle with `Once`. The explicit-mount constructor
creates fresh UTS/IPC/net/time objects, shares the global root PID namespace,
and reuses `CgroupNamespace::initial`. Independent explicit-mount bundles still
share the initial cgroup hierarchy so boot mounts and PID 1 do not acquire
separate system roots.

`clone_for_child` rejects NEWNET, NEWUSER, NEWCGROUP, and NEWTIME with
`Unimplemented`; the X-Kernel container plan forbids creating namespace objects
that only change an ID without isolating the underlying tables. With NEWNS it
rejects `NamespaceFsContext::Shared`; otherwise it snapshots root/pwd, calls
KVFS `clone_with_root_and_pwd`, retargets the private `FsStruct` as a pair, and
retains the new mount namespace. NEWUTS copies both names into a new identity;
NEWIPC creates a new namespace ID; NEWPID creates a child `PidNamespace`
nested below the parent's `pid_ns_for_children`. Unselected references are
shared. The function does not install the result into a running process.

## unshare two-phase protocol

`NsProxy::prepare_unshare` performs all Linux checks and returns a
`PreparedUnshare`, so the commit phase only installs precomputed state:

1. NEWNET/NEWUSER/NEWCGROUP/NEWTIME are rejected with `Unsupported`
   (`EOPNOTSUPP`) — never pseudo-isolation.
2. Any `NEW*` flag requires `UnshareContext::privileged` (CAP_SYS_ADMIN in the
   absence of user namespaces); plain `CLONE_FS`/`CLONE_FILES` do not.
3. `NEWNS` while the `fs_struct` is shared and `CLONE_FS` was not requested is
   `InvalidArgument` (`EINVAL`), matching Linux `check_fs_flag()`.
4. `NEWNS` clones the mount tree at the current root/pwd and builds a private
   `FsStruct` retargeted into the clone; `CLONE_FS` without NEWNS detaches a
   private copy; `CLONE_FILES` is recorded for the commit phase.

The syscall layer supplies process facts through `UnshareContext`
(privilege and fs-sharing) and commits via `Process::apply_prepared_unshare`.

`NsProxy::with_namespace` builds a proxy with one namespace slot swapped,
used by `setns(2)`. Net/User/Cgroup slots are rejected as `Unsupported`
("pseudo-isolation" guard).

## setns semantics

`setns(2)` in `core/ksyscall` resolves an fd through two channels:

- `NsFd` channel: `nstype` must be zero or match the fd's namespace type
  (`EINVAL` otherwise); `CLONE_NEWPID` requires the caller to be
  single-threaded.
- `PidFd` channel (Linux 5.8+): exactly one `CLONE_NEW*` bit must be given,
  and the caller joins that namespace of the pidfd's live process.

Both channels build the replacement proxy with `with_namespace` and commit
through `Process::apply_setns`; `CLONE_NEWNS` additionally retargets the fs
root/pwd into the target mount namespace's visible root.

## UTS data flow and concurrency

`UtsInner` owns two 65-byte `c_char` arrays. Setters reject names longer than 64
bytes, zero the array, then copy bytes; arbitrary byte values and embedded NUL
are accepted. Slice getters stop at the first NUL. `UtsNamespace` uses an
`RwLock<UtsInner>`; vector getters allocate a copy, while `read_names_into`
copies both full arrays into caller buffers under one read lock.

`NsProxy` is immutable after construction; its namespace objects provide their
own synchronization. Runtime pointer replacement belongs to `kprocess`.
Namespace IDs use the `kcred` allocator; cgroups have a separate ID type. ID
allocation is not a memory-ordering/publication protocol.

## Decisions, lifecycle, and limitations

Unsupported clone flags fail explicitly instead of silently claiming isolation.
`NamespaceFsContext` makes mount-copy versus shared-filesystem conflicts visible
in the API. Credential and active-PID roles are not folded into `NsProxy`.

There is no custom bundle drop: final `Arc` release drops references and the
respective namespace owners clean their resources. IPC/net/time ID-only types
have no local resource manager teardown. `NsFd` pins the namespace for the fd
lifetime, so a namespace survives while any fd or bundle references it.

Non-goals and increments (explicitly out of scope, recorded because they echo
the tgoskits gaps):

- **NEWNET/NEWUSER/NEWCGROUP/NEWTIME isolation**: the object types exist and
  NsFd/procfs expose them, but switching net/user/cgroup references without
  isolating the underlying interface/credential/cgroup tables is rejected.
  User namespaces additionally need `uid_map`/`gid_map` chown and mount
  permission downgrade semantics.
- **fs-sharing counting**: the shared-`fs_struct` (`NEWNS` without `CLONE_FS`)
  check covers published threads of the caller; cross-process `CLONE_FS`
  sharing is not counted (no `fs->users` counter yet).
- **setns PID reachability**: only the single-threaded requirement and
  reference swap are enforced; Linux's requirement that the target PID
  namespace be an ancestor of the caller's active namespace is not modeled
  (there is no per-process active-ns field).
- **bind-mount lifecycle**: namespace files keep identity in their inode and
  can be bound, but re-opening a bound file after the owning process exits
  fails with `ESRCH`; a namespace registry (nsfs-style) is needed for the
  libnetwork bind/setns/GC lifecycle. `/proc/<pid>/ns/<type>` files are
  regular files whose readlink fails with `EINVAL` (documented deviation from
  Linux's nsfs magic symlinks, mirroring the Docker-netns precedent).

No complete mount-propagation or IPC implementation is implied by the presence
of a namespace reference.