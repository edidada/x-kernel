# 物理页分配收口：后续迁移范围

本文记录 FrameMeta/FrameRef review 后确认的使用者迁移。描述符、启动准备、内容
映射和旧 buddy 适配已拆分；首批迁移覆盖页缓存 Folio 与共享文件 PTE，其他使用者
仍需按下列协议继续推进。

## 目标

普通受管理 RAM 的运行时使用者，通过 FrameRef 或明确的多页持有对象取得物理页，
结束持有时统一进入公共生命周期。PA/PFN 是物理页身份；裸 buddy/PCP 操作作为
分配器内部库存操作，不能作为子系统绕过生命周期的分配、归还入口。

## 已确认的边界

- FrameRegistry 只管理物理覆盖范围、描述符存储与 PFN 查询。
- 启动层拥有描述符存储选择与永久预留，显式提供存储给注册表。
- 页内容映射与描述符查询独立；当前内容访问使用 LinearFrameMapping。
- buddy_frame_adapter 是过渡模块。buddy 的注册、分配、释放改成 PA/PFN，且裸 VA
  使用者完成迁移时，同步删除该模块、调用点及只为适配使用的反向地址转换。
- FrameRef 保留分配器实例借用。独立库存的页必须回到所属实例，实例和底层存储
  的有效期必须覆盖全部持有。

## 迁移顺序与检查点

1. 盘点裸页分配、归还和所有权转移的使用者，列出取得、共享、归还与硬件使用边界。
2. 先收口普通单页路径；页表页已有引用转移路径，需要一起审计其归还边界。
3. 迁移 GlobalPage、页缓存/Folio、匿名及共享内容对象等持有者，移除它们直接操作
   裸库存的路径。需要连续多页、DMA 或其他专用约束的路径先明确整段持有协议，
   保证实际 backing 页均被覆盖，再迁移。
4. 同步建立映射持有：建立映射时承担持有责任，撤销 PTE 后保留到必要的 TLB
   失效/同步完成边界，再结束映射持有。原始 PA 转移必须保持一份可追踪的持有责任。
5. 文件截断等操作另行满足映射失效与内容语义；公共引用计数负责存储存活，不能
   替代这些操作的通知、同步和撤映射协议。
6. 将仍需存在的裸 API 限制到分配器内部、启动交接或明确的硬件引用转移边界。
   字节堆及 slab 持有 backing 页也要纳入协议，但内部空闲库存与对象引用分别管理。
7. 改造 buddy 的物理地址接口并删除过渡模块。页内容和管理信息的访问映射继续
   通过独立接口提供。

## 验证要求

每条迁移路径检查最后持有结束后只归还一次、共享期间不提前归还、失败路径不丢失
持有，以及旧映射无法访问已经复用的存储。终版运行相关内核单测和 harness；不要求
每个中间提交重复完整 harness。按使用者和协议边界拆分提交，便于后续拆小 PR。


## 首批迁移：页缓存与共享文件映射

- Folio 通过 alloc_zeroed_frame 取得一份 FrameRef，Drop 自动结束缓存持有。
- 共享文件缺页在 folio 借用内取得另一份持有，交给 map_owned。
- 页表按 VA 保存 provider-defined MappingReference；unmap、稀疏失效与 PROT_NONE
  清除 PTE 后把持有交给有界批次，finish 完成 TLB 失效后结束持有。
- move_mapping 先取得目标持有，再撤销源 PTE，确保 mremap 搬移时不丢失生命周期。
- 旧 raw map 仍保持外部 backing 契约。DMA 和字节堆 backing 的收口尚未完成；这些路径按整段持有与
  映射协议继续迁移。GlobalPage 与连续多页支持见下。
- 映射索引增加 heap bookkeeping 成本，尚未给出性能改善结论；全量可失败堆分配
  与后续 reclaim 仍是独立工作。


## 第二批迁移：GlobalPage 和 VMM backing

- FrameRun 为完整的 power-of-two backing 建立每个基础页的持有，实际页数包含
  buddy 向上取整的 slack。可以从 live run 取得单页 FrameRef，最终逐页归还。
- GlobalPage 内部持有 FrameRun，safe 字节视图只暴露请求范围。PA 直接从持有返回，
  into_raw/from_raw 改为明确的物理所有权转移，不能收养裸 alloc_pages 结果。
- VMM 页表的原始持有显式转移，销毁按 PA 恢复；guest RAM 保留也持有 FrameRun。
  第二阶段 mapper 先析构，reservation 后释放，vCPU 的 VM 引用必须已结束使用。
- 本批逐页释放为 O(backing 页数)，未实现 run 批量归还或复合页协议。DMA、字节堆 backing、启动期临时页仍需分别迁移。


## 第三批迁移：匿名页、私有文件页与共享匿名页

- 匿名私有页状态保存 FrameRun，所有权随 first-touch/COW commit 转移；失败自动
  结束持有。快照与 detached result 持有状态，避免对象槽位移除后复制源提前归还。
- 对象槽位的 COW refs 与物理引用分开，PTE/临时句柄不影响 COW 独占判断。
  对象 Drop 结束剩余槽位计数，避免父对象退出后子对象仍被误判为共享。
- 首次缺页、fork、重新建立映射均通过 map_owned_sized 建立独立映射持有。
  COW 使用 replace_owned_if_same：compare 失败恢复旧持有，成功把旧持有送入
  TLB 退役批次。物理归还不再依靠 memspace 的裸 dealloc_frame。
- 私有文件结果页使用相同匿名对象和映射协议，文件读取失败自动归还未发布 backing。
  文件截断、失效通知与内容同步协议仍独立存在。
- 共享匿名 SharedPages 保存 FrameRun，每个 PTE 克隆完整 backing 的持有。
  HAL 使用 FrameRun 作为映射 capability，精确覆盖现有大页 leaf；目录按包含地址
  找到 leaf 持有，从大页中间地址撤映射也会退役全部基础页。
- 此处保持已有显式大页 leaf 能力，不实现 THP、拆分/合并或复合页元数据。
  FrameRun 克隆、归还均为 O(基础页数)；BTreeMap/Vec/Arc 的可失败堆分配仍待推进。
