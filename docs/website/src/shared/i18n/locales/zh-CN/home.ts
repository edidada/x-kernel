export default {
  hero: {
    subtitle: "一个 Rust 开发的多架构开源宏内核操作系统，安全、自主、可信。",
    title: "X-kernel，探索无限可能",
  },
  api: {
    title: "从源码到运行。",
  },
  explorer: {
    title: "从一台内核，探索每一个子系统。",
    modulesLabel: "内核子系统",
    openSource: "打开源码模块",
    openDocs: "打开文档",
    modules: {
      memory: {
        label: "内存",
        title: "让每一页内存，都有自己的位置。",
        description:
          "从物理页分配，到虚拟地址空间。内存管理模块负责建立内核与硬件之间的秩序。",
        name: "Memory Management",
        keywords: "物理内存 · 地址空间 · 页面分配",
      },
      scheduler: {
        label: "调度",
        title: "让每一个任务，都获得执行的机会。",
        description: "探索内核中的任务状态、调度策略和执行上下文。",
        name: "Process Scheduling",
        keywords: "任务 · 线程 · 定时器",
      },
      filesystem: {
        label: "文件系统",
        title: "把数据块，组织成可用的系统。",
        description: "沿着文件系统、VFS 边界，理解存储如何抵达一个进程。",
        name: "Filesystem",
        keywords: "VFS · 文件 · 存储",
      },
      network: {
        label: "网络",
        title: "让内核，连接真实的世界。",
        description: "跟踪数据包穿过网络协议栈和 POSIX 网络层的路径。",
        name: "Networking",
        keywords: "协议 · 套接字 · 数据包",
      },
      drivers: {
        label: "设备驱动",
        title: "让硬件，变得可以理解。",
        description: "探索连接设备与内核的驱动和 I/O 层。",
        name: "Device Drivers",
        keywords: "I/O · 设备 · 平台支持",
      },
      tee: {
        label: "可信执行",
        title: "把信任，放在内核的核心。",
        description: "探索 X-kernel 内置的可信执行环境支持。",
        name: "TEE",
        keywords: "可信执行 · 隔离 · 安全",
      },
    },
  },
  architecture: {
    title: "组件化，模块化。",
    badge: "overview",
    userSpace: "用户空间 · 应用程序",
    syscall: "系统调用接口",
    kernelSpace: "Rust 组件化宏内核",
    hardware: "架构与平台 · 硬件 / 虚拟化",
    note: "明确边界 · 接口组合 · 无环依赖",
    pause: "暂停组件滚动",
    resume: "继续组件滚动",
  },
  architectures: {
    eyebrow: "MULTI-ARCHITECTURE",
    subtitle: "从虚拟机开始，走向不同硬件平台。选择熟悉的架构，深入系统底层。",
    title: "跨越架构，探索更多可能。",
    platforms: "支持平台  QEMU · 海光 CSV · Kylin-x Pkvm",
    items: {
      aarch64: { name: "AArch64", description: "64-bit ARM" },
      x86: { name: "x86_64", description: "64-bit x86" },
      riscv: { name: "RISC-V 64", description: "开放指令集架构" },
      loongarch: { name: "LoongArch64", description: "64-bit LoongArch" },
    },
  },
  values: {
    eyebrow: "BUILT IN THE OPEN",
    title: "安全、自主、可信。",
    items: {
      rust: {
        label: "RUST",
        title: "从语言开始",
        description:
          "使用 Rust 构建内核，借助类型系统与所有权机制，降低内存安全风险。",
      },
      tee: {
        label: "TEE",
        title: "探索可信执行",
        description: "提供 TEE 相关支持，让可信执行环境成为内核探索的一部分。",
      },
      open: {
        label: "OPEN SOURCE",
        title: "开放，才能共建",
        description:
          "以 Apache-2.0 协议开放源码，从阅读一个模块到贡献第一份代码。",
      },
    },
  },
  cta: {
    subtitle: "阅读源码，理解内核，一起构建。",
    title: "从这里，开始你的探索。",
    docs: "阅读文档",
    source: "查看源码",
  },
  terminal: {
    label: "终端演示",
    pause: "暂停",
    play: "播放",
    restart: "重新播放",
    position: "播放进度",
    time: "{{elapsed}} / {{duration}}",
    tabs: { configure: "配置", build: "构建", boot: "启动", explore: "探索" },
    shortTabs: {
      configure: "配置",
      build: "构建",
      boot: "启动",
      explore: "探索",
    },
  },
} as const;
