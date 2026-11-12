# kseccomp security and reliability

## Scope, assets, and boundaries

seccomp is a voluntary sandbox that user code inside the guest imposes on the kernel: **the trust boundary lies between the kernel and
user code**, and the goal of a filter is to reduce the syscall surface available to a malicious or compromised user process.
The interpreter in this crate sits on the kernel side of that boundary, so it must satisfy the following:

1. Never amplify privileges: any interpretation exception (unknown instruction, out-of-bounds access, division by zero) must fail closed,
   rather than falling back to an "unfiltered" allow.
2. Never leak kernel data: `run` may only read the `SeccompData` snapshot it was given,
   with no other memory reads or writes (this crate contains no `unsafe`).
3. The deny path must hand the syscall layer a deterministic, executable result, and the decision precedence must match Linux
   (otherwise a malicious program could bypass the intended constraints by having "two filters return different actions").

## Invariants and commitments

- **fail-closed fallback**: `run` returns `SECCOMP_RET_KILL_THREAD` by default; an out-of-bounds
  load returns 0, and division by zero / illegal instructions / illegal jumps terminate immediately with deny, never going beyond the program boundary
  or accessing memory outside the snapshot.
- **Unrecognized actions are not allowed**: `TRAP` and any unrecognized return value are classified as
  `Action::Unknown`, which has the lowest precedence value, so `highest_priority` yields
  `Decision::Unsupported`, and dispatch terminates the thread with `SIGSYS`.
- **Determinism**: a program is a stateless straight-line machine (A/X/M + pc); the same program on the same snapshot always yields the same result;
  there is no entropy, no shared concurrency, and no global state.
- **Data minimization**: the `SeccompData` snapshot contains only the 64 bytes of fields the seccomp ABI needs;
  the interpreter does not touch any other kernel memory.
- **Length limit**: `SeccompFilter::new` enforces `BPF_MAXINSNS=4096`, preventing a malformed
  user program from dragging down the kernel or triggering excessive allocation (the ksyscall side validates before allocating).
- **Install-time validation**: instruction encodings and all forward jump targets are validated before installation, and an illegal program returns
  `EINVAL`; there is no ambiguous failure of the form "installs successfully but is certain to kill when run".
- **Cooperation with PR_SET_NO_NEW_PRIVS / CAP_SYS_ADMIN**: the seccomp installation gate is on the
  `sys_seccomp` side (privileged or no_new_privileges); this crate only guarantees that an installed
  filter is interpreted faithfully, and does not perform authorization on its own.

## Defenses against sandbox escape

| Attack vector | Defense |
| --- | --- |
| 过滤器返回 ALLOW 绕过 | 链式合并取最高优先级；ALLOW 在 `Action::precedence` 中优先级最低 |
| 用 TRAP / 未知返回码绕过 | `Action::Unknown` 优先级最高（最严）→ `Decision::Unsupported` → dispatch `do_exit(SIGSYS)` |
| 用 TRACE/USER_NOTIF 绕过 | 枚举为不可执行 → `GET_ACTION_AVAIL` 返回 `ENOSYS`，dispatch 对 syscall 返回 `ENOSYS`，不会放行 |
| 越界 `BPF_ABS/IND` 读取内核内存 | load 全部以 64 字节快照边界校验，越界补零 |
| 除零 / 移位崩溃 | ALU 除零/取模零在安装期即 `EINVAL`，运行期仍以 deny 兜底；移位按 `wrapping_*` 语义 |
| 超大 `sock_fprog.len` 或超长链拖垮内核 | `SeccompFilter::new` 与 ksyscall 双重 `BPF_MAXINSNS`；链总长受 Linux `MAX_INSNS_PER_PATH` 约束 |
| 用非法程序把内核变成"必然杀进程"的放大器 | 安装期校验先行拒绝，调用方可在安装点看到 `EINVAL` |
| syscall 入口不作为（漏评估） | `handle_syscall` 在分发前统一 `evaluate_seccomp`，单点入口 |
| strict 白名单生成错（放行错误 syscall） | `SeccompFilter::strict_allowlist` 由单元测试固定"只放行四个号、其余 kill"的语义；号来源是 `linux_sysno::Sysno` |

## Audit points

- `bpf::run`: all branches for illegal instructions / out-of-bounds / division by zero resolve to `DEFAULT_RETURN`
  (deny); there should be no implicit path that can allow.
- `filter::validate`: class / size / mode / operator and forward jump targets are all validated; any newly added
  executable encoding must extend this function in sync.
- `Action::from_ret` / `highest_priority`: the precedence table Unknown > KILL_PROCESS >
  KILL_THREAD > ERRNO > USER_NOTIF > TRACE > LOG > ALLOW is consistent with
  `seccomp_run_filters` (including the "unknown values are strictest" policy).
- `SeccompFilter::strict_allowlist`: the layout must guarantee that the `jt` of each `JEQ` lands exactly on the
  trailing `ALLOW`, and does not clobber the accumulator (a load instruction must not be used as a trampoline).
- The ksyscall installation gate: the `flags` allowlist (TSYNC/TSYNC_ESRCH/LOG/SPEC_ALLOW);
  any unknown flag yields `EINVAL`, avoiding silent ignoring.

## Known boundaries

- `USER_NOTIF`/`TRACE` are not implemented: `GET_ACTION_AVAIL` returns `ENOSYS` for both,
  and dispatch fails closed for both → `ENOSYS`. They are not silently allowed.
- `TRAP` does not deliver a `SIGSYS` callback frame (Linux builds the
  `siginfo` from the data of `SECCOMP_RET_TRAP`); it is currently handled in the strictest way by terminating the thread directly; a process that depends on a `SIGSYS` handler will
  observe a difference.
- `SECCOMP_FILTER_FLAG_LOG` is accepted but produces no audit log; `SPEC_ALLOW` has no side effects.
- TSYNC only broadcasts to sibling threads that are "already published"; a thread that is under construction and not yet published will, after publication,
  inherit from a thread that is still running (see `Thread::prepare_*_fork/thread_clone` for the inheritance points).