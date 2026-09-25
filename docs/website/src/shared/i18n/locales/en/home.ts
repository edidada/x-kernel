export default {
  hero: {
    subtitle:
      "An open-source, multi-architecture monolithic operating system built with Rust. Secure, independent, and trustworthy.",
    title: "X-kernel, explore infinite possibilities",
  },
  api: {
    title: "From source to system.",
  },
  explorer: {
    title: "From one machine to every subsystem.",
    modulesLabel: "Kernel subsystems",
    openSource: "Open source module",
    openDocs: "Open documentation",
    modules: {
      memory: {
        label: "Memory",
        title: "Give every page of memory its place.",
        description:
          "From physical page allocation to virtual address spaces, memory management keeps the kernel and hardware in order.",
        name: "Memory Management",
        keywords: "Physical memory · address spaces · page allocation",
      },
      scheduler: {
        label: "Scheduling",
        title: "Give every task a fair turn.",
        description:
          "Explore task state, scheduling and execution contexts across the kernel.",
        name: "Process Scheduling",
        keywords: "Tasks · threads · timers",
      },
      filesystem: {
        label: "Filesystem",
        title: "Turn blocks into a usable system.",
        description:
          "Follow filesystems, VFS boundaries and the path from storage to a process.",
        name: "Filesystem",
        keywords: "VFS · files · storage",
      },
      network: {
        label: "Networking",
        title: "Connect the kernel to the world.",
        description:
          "Trace packets through the network stack and the POSIX networking layer.",
        name: "Networking",
        keywords: "Protocols · sockets · packets",
      },
      drivers: {
        label: "Device Drivers",
        title: "Make hardware understandable.",
        description:
          "Discover the shared driver and I/O layers that connect devices to the kernel.",
        name: "Device Drivers",
        keywords: "I/O · devices · platform support",
      },
      tee: {
        label: "Trusted Execution",
        title: "Keep trust close to the core.",
        description:
          "Explore the trusted execution environment support built into X-kernel.",
        name: "TEE",
        keywords: "Trusted execution · isolation · security",
      },
    },
  },
  architecture: {
    title: "Componentized. Modular.",
    badge: "overview",
    userSpace: "User space · Applications",
    syscall: "System call interface",
    kernelSpace: "Rust componentized monolithic kernel",
    hardware: "Architecture & Platforms · Hardware / Virtualization",
    note: "Clear boundaries · Composable interfaces · Acyclic dependencies",
    pause: "Pause component scrolling",
    resume: "Resume component scrolling",
  },
  architectures: {
    eyebrow: "MULTI-ARCHITECTURE",
    subtitle:
      "From virtual machines to real hardware. Start with the architecture you know and go deeper.",
    title: "Across architectures, explore more possibilities.",
    platforms: "Supported platforms  QEMU · Hygon CSV · Kylin-x Pkvm",
    items: {
      aarch64: { name: "AArch64", description: "64-bit ARM" },
      x86: { name: "x86_64", description: "64-bit x86" },
      riscv: { name: "RISC-V 64", description: "Open instruction set" },
      loongarch: { name: "LoongArch64", description: "64-bit LoongArch" },
    },
  },
  values: {
    eyebrow: "BUILT IN THE OPEN",
    title: "Secure, independent, trustworthy.",
    items: {
      rust: {
        label: "RUST",
        title: "Start with the language.",
        description:
          "Rust's type system and ownership model help reduce memory safety risks.",
      },
      tee: {
        label: "TEE",
        title: "Explore trusted execution.",
        description:
          "TEE support makes trusted execution part of the kernel's design space.",
      },
      open: {
        label: "OPEN SOURCE",
        title: "Build together in the open.",
        description:
          "Apache-2.0 licensed source, open for inspection and contribution.",
      },
    },
  },
  cta: {
    subtitle: "Read the source, understand the kernel, build together.",
    title: "Start exploring from here.",
    docs: "Read the docs",
    source: "View source",
  },
  terminal: {
    label: "Terminal demos",
    pause: "Pause",
    play: "Play",
    restart: "Restart",
    position: "Playback position",
    time: "{{elapsed}} of {{duration}}",
    tabs: {
      configure: "configure",
      build: "build",
      boot: "boot",
      explore: "explore",
    },
    shortTabs: {
      configure: "configure",
      build: "build",
      boot: "boot",
      explore: "explore",
    },
  },
} as const;
