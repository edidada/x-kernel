# 边界与执行上下文规则

## 规则格式

每条规则使用以下字段：

- **规则 ID**：稳定编号，review 评论和后续统计可引用。
- **规则**：应遵守的编码要求。
- **等级**：`MUST`、`SHOULD` 或 `MAY`。
- **适用对象**：需要检查该规则的代码对象。
- **判定方法**：如何判断代码是否符合规则。
- **例外**：不按本规则报告问题的情况。
- **反例**：违反规则的典型代码。
- **正例**：符合规则的典型代码，或推荐修改方式。

## CG-CTX-001：用户态地址应先包装为用户指针抽象再访问

- **规则**：来自用户态的地址、裸指针或原始 ABI carrier 可能在访问时触发
  page fault，进入内核后应先转换为项目认可的用户指针抽象，再通过该抽象
  完成读写、copy、切片访问或指针偏移；不得把它当作普通内核指针直接解引用
  或构造普通引用 / slice。
- **等级**：MUST
- **适用对象**：syscall frontend、ABI 适配层、ioctl 处理、用户内存 copy helper，
  以及其他接收用户态地址的入口代码。
- **判定方法**：
  1. 找到新增或修改代码中来自 syscall 参数、ioctl 参数、ABI 结构字段或
     userspace 约定的地址值。
  2. 如果这些值以 `usize`、`u64`、`*const T`、`*mut T`、`NonNull<T>` 或
     等价裸地址形式参与解引用、`ptr::read` / `ptr::write`、`copy_nonoverlapping`、
     `slice::from_raw_parts`、`slice::from_raw_parts_mut`、普通引用构造或
     手写地址偏移，则违反本规则。
  3. 符合规则的代码应先转换为 `UserPtr<T>`、`UserConstPtr<T>`、`VmBytes`、
     `VmBytesMut`、`IoVectorBuf` 或项目等价用户内存抽象，再调用该抽象提供的
     `read_vm`、`write_vm`、`read_vm_slice`、`write_vm_slice` 或等价访问方法。
  4. 如果需要做 null、长度、对齐、范围或溢出检查，检查对象仍应保持为用户
     指针抽象或裸地址值；检查通过也不表示可以把该地址当作普通内核指针访问。
- **例外**：
  - `core/kuaccess`、`process/osvm`、架构 `copy_user` 实现或等价底层用户内存访问实现；
    这些代码必须用自己的安全契约说明 fault、权限和地址有效性处理方式。
  - 仅把用户态地址作为 opaque token 保存、比较、日志记录或传递给用户内存
    抽象构造函数，且没有直接内存访问。
- **反例**：

```rust
pub fn sys_set_limit(raw_limit: usize) -> KResult<isize> {
    let raw_limit = raw_limit as *const RawLimit;
    // 错误：用户态地址可能 fault，不能构造普通引用。
    let limit = unsafe { &*raw_limit };
    resource::set_current_limit((*limit).try_into()?)?;
    Ok(0)
}
```

这段代码绕过了用户内存抽象，把用户态地址当成普通内核指针访问。

- **正例**：

```rust
pub fn sys_set_limit(raw_limit: usize) -> KResult<isize> {
    let raw_limit = UserConstPtr::<RawLimit>::from(raw_limit);
    let limit = raw_limit.read_vm()?.try_into()?;
    resource::set_current_limit(limit)?;
    Ok(0)
}
```

## CG-CTX-002：用户态 ABI 类型不应传入普通子系统逻辑

- **规则**：用户态指针抽象和用户态 ABI carrier 应停留在 syscall frontend、
  ABI 适配层、用户内存 copy helper 或等价边界层；普通子系统逻辑应接收
  内核拥有的语义对象，不应接收 `UserPtr<T>`、`UserConstPtr<T>`、裸用户地址
  或只为匹配用户态 ABI 定义的 raw 结构。
- **等级**：MUST
- **适用对象**：syscall frontend、ABI 适配层、ioctl 处理、用户内存 copy helper，
  以及这些层调用的普通子系统 API。
- **判定方法**：
  1. 找到新增或修改的函数参数、结构体字段和跨模块调用。
  2. 如果普通子系统 API 接收 `UserPtr<T>`、`UserConstPtr<T>`、`usize` /
     `u64` 形式的用户地址，或 `#[repr(C)]` / Linux ABI raw 结构等用户态
     ABI carrier，则继续检查调用链。
  3. 如果调用方可以在边界层完成 copy、校验和转换，却把这些用户态对象传入
     普通子系统逻辑，则违反本规则。
  4. 符合规则的代码应在边界层转换为内核语义类型，例如 limit、路径对象、
     fd 对象、handle、slice、buffer、枚举或带不变量的 domain struct。
- **例外**：
  - `core/kuaccess`、`process/osvm`、syscall frontend、ABI 适配层、ioctl
    ABI 分发层或等价用户内存边界模块。
  - 明确负责延迟 copy-in / copy-out 的用户内存抽象，例如 `VmBytes`、
    `VmBytesMut`、`IoVectorBuf`，且其 API 契约记录了用户内存访问边界。
  - 普通子系统定义的类型虽然可由 ABI 数据转换得到，但类型本身表达的是内核
    语义对象，而不是用户态布局或用户态地址。
- **反例**：

```rust
pub fn sys_set_limit(raw_limit: UserConstPtr<RawLimit>) -> KResult<isize> {
    resource::set_current_limit(raw_limit)?;
    Ok(0)
}
```

`resource::set_current_limit` 属于资源管理子系统逻辑，不应接收用户指针。

- **正例**：

```rust
pub fn sys_set_limit(raw_limit: UserConstPtr<RawLimit>) -> KResult<isize> {
    let limit = raw_limit.read_vm()?.try_into()?;
    resource::set_current_limit(limit)?;
    Ok(0)
}
```
