# 命名规则

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

## CG-NAME-001：裸数值名称应携带无法由类型表达的单位

- **规则**：当整数或浮点类型本身不能表达单位时，变量、字段、参数或返回值名称
  应携带单位后缀，例如 `_bytes`、`_pages`、`_ns`、`_us`、`_ms`、`_secs`、
  `_hz`、`_addr`、`_pfn`。
- **等级**：MUST
- **适用对象**：新增或重命名的数值型参数、字段、局部变量、常量和返回值 helper。
- **判定方法**：
  1. 找到类型为 `usize`、`u64`、`u32`、`isize`、`i64`、`i32` 或等价裸数值类型的名称。
  2. 如果该值表示大小、长度、偏移、地址、页数、时间、频率、计数周期或硬件编号，
     且名称没有表达单位，则违反本规则。
  3. 如果类型已经表达单位或语义，例如 `Bytes`、`PageCount`、`Duration`、
     `VirtAddr`、`PhysAddr`，名称可以不重复单位。
- **例外**：
  - 循环下标、数组索引、计数器等局部短生命周期值，例如 `i`、`index`、`count`。
  - Linux/POSIX ABI 要求使用的字段名或参数名。
- **反例**：

```rust
fn map_region(start: usize, size: usize, timeout: u64) -> KResult<()> {
    mm::map(start, size, timeout)
}
```

`start`、`size` 和 `timeout` 都是裸数值，调用点无法判断单位。

- **正例**：

```rust
fn map_region(start_addr: usize, size_bytes: usize, timeout_ns: u64) -> KResult<()> {
    mm::map(start_addr, size_bytes, timeout_ns)
}
```

## CG-NAME-002：布尔值名称应表达可断言命题

- **规则**：布尔变量、字段、参数和返回 `bool` 的函数 / 方法名称应表达一个可直接
  判断真假的命题，优先使用 `is_*`、`has_*`、`can_*`、`should_*`、`needs_*`、
  `allows_*` 等形式。
- **等级**：MUST
- **适用对象**：新增或重命名的 `bool` 变量、字段、参数、函数和方法。
- **判定方法**：
  1. 找到类型为 `bool` 或返回值为 `bool` 的新增 / 修改名称。
  2. 如果名称是名词、动词短语或缩写，读者不能从名称直接读出 true 代表的状态，
     则违反本规则。
  3. 如果名称使用否定形式，例如 `not_*`、`no_*`、`disable_*`，且调用点出现
     双重否定或分支语义反转，应要求改成正向命题。
- **例外**：
  - 外部 ABI、硬件寄存器 bit 名称或协议字段要求保留原名。
  - 极小作用域内与标准库或语言惯例一致的名称，例如 `ok`。
- **反例**：

```rust
let lock = task.state() == TaskState::Blocked;
if !lock {
    task.wake();
}
```

`lock` 不是可断言命题，无法表达 true 的语义。

- **正例**：

```rust
let is_blocked = task.state() == TaskState::Blocked;
if !is_blocked {
    task.wake();
}
```

## CG-NAME-003：可调用值的局部名称应显式表达可调用性

- **规则**：局部变量保存闭包或函数指针，且该变量稍后被调用时，名称应以 `_fn`
  结尾或使用其他明确表达可调用性的名称。
- **等级**：SHOULD
- **适用对象**：新增或重命名的局部闭包变量、函数指针变量和回调变量。
- **判定方法**：
  1. 找到被赋值为闭包、函数项、函数指针或回调对象的局部变量。
  2. 如果该变量在后续代码中以调用形式出现，例如 `handler(arg)`，但名称看起来像
     普通数据对象，则应报告本规则。
  3. 如果变量名称已经明确表达可调用性，例如 `handler_fn`、`callback_fn`、
     `run_callback`，则符合本规则。
- **例外**：
  - API 或 trait 约定要求的参数名。
  - 立即传入高阶函数、没有后续局部调用的闭包。
- **反例**：

```rust
let task = || current_task().pid();
let pid = task();
```

`task` 看起来像任务对象，不像可调用值。

- **正例**：

```rust
let task_fn = || current_task().pid();
let pid = task_fn();
```

## CG-NAME-004：已使用的参数不应保留 `_` 前缀

- **规则**：函数、闭包、trait impl 或测试中的参数只要在函数体内被读取、写入、
  移动或传递，就不应使用 `_name` 形式；`_` 前缀只用于明确未使用的绑定。
- **等级**：MUST
- **适用对象**：新增或修改的参数、闭包参数和 pattern binding。
- **判定方法**：
  1. 找到名称以 `_` 开头但不是单独 `_` 的绑定。
  2. 如果该绑定在作用域内被使用，则违反本规则。
  3. 如果绑定确实未使用，允许保留 `_name` 以说明保留参数的语义。
- **例外**：
  - 外部宏生成代码或必须匹配宏 pattern 的绑定名。
  - 语言或 trait 签名无法改变，但实现体确实未使用该参数。
- **反例**：

```rust
fn set_timeout(_timeout_ns: u64) {
    timer::set(_timeout_ns);
}
```

`_timeout_ns` 已被使用，`_` 前缀会误导 reviewer 认为该参数未参与逻辑。

- **正例**：

```rust
fn set_timeout(timeout_ns: u64) {
    timer::set(timeout_ns);
}
```

## CG-NAME-005：名称不应隐藏可观察成本或副作用

- **规则**：函数或方法名称应反映调用是否会执行 O(n) 扫描、分配、阻塞、状态修改
  或硬件 / 用户内存访问；带有 `get_`、`as_`、名词式 accessor 等轻量读取含义的
  名称，不应用于这些可观察成本或副作用。
- **等级**：SHOULD
- **适用对象**：新增或重命名的函数、方法和局部 helper。
- **判定方法**：
  1. 查看函数体或被调用 helper，判断该函数是否会扫描集合、分配内存、阻塞等待、
     修改状态、执行 MMIO / IO、访问用户内存或获取可能阻塞的锁。
  2. 如果名称看起来像廉价字段读取或纯转换，例如 `get_*`、`as_*`、`*_ref`、
     名词式 accessor，但实际存在上述成本或副作用，则报告本规则。
  3. 符合规则的名称应使用能表达行为的动词，例如 `find_*`、`collect_*`、
     `load_*`、`read_*`、`write_*`、`allocate_*`、`wait_*`、`update_*`。
- **例外**：
  - 已有 trait、标准库或外部 ABI 要求的函数名。
  - 函数体只读取缓存字段，且成本与普通 accessor 一致。
- **反例**：

```rust
fn task(pid: Pid) -> Option<Arc<Task>> {
    task_table().iter().find(|task| task.pid() == pid).cloned()
}
```

名称 `task` 隐藏了全表扫描成本。

- **正例**：

```rust
fn find_task(pid: Pid) -> Option<Arc<Task>> {
    task_table().iter().find(|task| task.pid() == pid).cloned()
}
```
