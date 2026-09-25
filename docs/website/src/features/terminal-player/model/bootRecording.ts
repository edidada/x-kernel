import session from "../../../../assets/recordings/x-kernel/boot/session.json";
import type { TerminalEvent, TerminalRecording } from "../lib/playback";

/** Selected real output, ending at the ready guest shell. */
export const bootRecording: TerminalRecording = {
  title: "boot",
  events: session.flatMap(({ command, output, prompt }): TerminalEvent[] => [
    { t: 350, type: "prompt", text: "~/x-kernel $ " },
    { t: 150, type: "type", text: command },
    { t: 200, type: "enter" },
    ...output
      .split(/(?<=\n)/)
      .map((text): TerminalEvent => ({ t: 40, type: "output", text })),
    ...(prompt
      ? [{ t: 200, type: "prompt" as const, text: prompt, cls: "tp-dim" }]
      : []),
    { type: "wait", ms: 800 },
  ]),
};
