import { bootRecording } from "./bootRecording";
import type { TerminalEvent, TerminalRecording } from "../lib/playback";

/** Curated demo, not a captured session. See README.md for source references. */
function command(text: string, prompt = "~/x-kernel $ "): TerminalEvent[] {
  return [
    { t: 300, type: "prompt", text: prompt },
    { t: 200, type: "type", text },
    { t: 250, type: "enter" },
  ];
}

function output(text: string, cls = "tp-output-text"): TerminalEvent {
  return { t: 400, type: "output", text: `${text}\n`, cls };
}

function idle(prompt = "~/x-kernel $ "): TerminalEvent[] {
  return [
    { t: 300, type: "prompt", text: prompt },
    { type: "wait", ms: 1200 },
  ];
}

export const terminalRecordings: readonly [
  TerminalRecording,
  ...TerminalRecording[],
] = [
  bootRecording,
  {
    title: "configure",
    events: [
      ...command("cp platforms/kplat-aarch64/qemu_defconfig .config"),
      ...command("make defconfig"),
      ...command("make rootfs"),
      output("# Root filesystem prepared before booting.", "tp-dim"),
      ...idle(),
    ],
  },
  {
    title: "build",
    events: [
      ...command("make build"),
      output("# Build output excerpt", "tp-dim"),
      output("   Compiling entry", "tp-prompt-text"),
      output("Symbol table updated; relinking kernel"),
      ...idle(),
    ],
  },
  {
    title: "explore",
    events: [
      ...command("pwd", "~ # "),
      output("/root"),
      ...command('echo "Hello, X-kernel!"', "~ # "),
      output("Hello, X-kernel!", "tp-prompt-text"),
      ...command("printf 'Rust. Open source. Multi-architecture.\\n'", "~ # "),
      output("Rust. Open source. Multi-architecture."),
      ...idle("~ # "),
    ],
  },
];
