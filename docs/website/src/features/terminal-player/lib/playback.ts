export const CHARACTER_DELAY_MS = 45;

type DelayedEvent = { t?: number };
type DisplayEvent =
  | { type: "prompt"; text?: string; cls?: string }
  | { type: "enter" }
  | { type: "output"; text: string; cls?: string }
  | { type: "replace-line"; text: string; cls?: string }
  | { type: "clear" };

/** Relative delays in the source recording, measured in milliseconds. */
export type TerminalEvent =
  | (DelayedEvent & DisplayEvent)
  | (DelayedEvent & { type: "type"; text: string })
  | { type: "wait"; ms: number };

export interface TerminalRecording {
  title: string;
  shortTitle?: string;
  events: readonly TerminalEvent[];
}

type TimelineAction = { time: number } & (
  DisplayEvent | { type: "character"; text: string }
);

export interface TerminalTimeline {
  actions: readonly TimelineAction[];
  duration: number;
}

export interface TerminalToken {
  text: string;
  className?: string;
}

export interface TerminalFrame {
  tokens: TerminalToken[];
  cursor: boolean;
}

/** Expand relative event delays into absolute timestamps for deterministic seeking. */
export function createTimeline(
  events: readonly TerminalEvent[],
): TerminalTimeline {
  let elapsedMs = 0;
  const actions: TimelineAction[] = [];

  for (const event of events) {
    if (event.type === "wait") {
      elapsedMs += event.ms;
      continue;
    }

    const eventDelayMs = event.t ?? 0;
    if (event.type === "type") {
      if (event.text.length === 0) {
        elapsedMs += eventDelayMs;
        continue;
      }

      for (let index = 0; index < event.text.length; index += 1) {
        elapsedMs += index === 0 ? eventDelayMs : CHARACTER_DELAY_MS;
        actions.push({
          time: elapsedMs,
          type: "character",
          text: event.text.charAt(index),
        });
      }
      // The original player waits `t` both before the first character and
      // after the last one. Preserve that pause when seeking or resuming.
      elapsedMs += eventDelayMs;
      continue;
    }

    elapsedMs += eventDelayMs;
    actions.push({ ...event, time: elapsedMs });
  }

  return { actions, duration: elapsedMs };
}

/** Remove the current line while retaining earlier text and its color tokens. */
function removeCurrentLine(tokens: TerminalToken[]): TerminalToken[] {
  for (let index = tokens.length - 1; index >= 0; index -= 1) {
    const token = tokens[index];
    if (!token) continue;
    const lastNewline = token.text.lastIndexOf("\n");
    if (lastNewline === -1) continue;

    return [
      ...tokens.slice(0, index),
      { ...token, text: token.text.slice(0, lastNewline + 1) },
    ];
  }
  return [];
}

/** Replay up to a timestamp; seeking never depends on a previously rendered frame. */
export function frameAt(
  timeline: TerminalTimeline,
  elapsedMs: number,
): TerminalFrame {
  let tokens: TerminalToken[] = [];
  let cursor = false;
  const append = (text: string, className?: string): void => {
    tokens.push({ text, className });
  };

  for (const action of timeline.actions) {
    if (action.time > elapsedMs) break;
    switch (action.type) {
      case "prompt":
        append(action.text || "$ ", action.cls || "tp-prompt-text");
        cursor = true;
        break;
      case "character":
        append(action.text, "tp-cmd-text");
        break;
      case "enter":
        append("\n");
        cursor = false;
        break;
      case "output":
        append(action.text, action.cls || "tp-output-text");
        cursor = false;
        break;
      case "replace-line":
        tokens = removeCurrentLine(tokens);
        if (action.text) append(action.text, action.cls || "tp-dim");
        cursor = false;
        break;
      case "clear":
        tokens = [];
        cursor = false;
        break;
      default: {
        // A new action type must also define how it changes the rendered frame.
        const unexpectedAction: never = action;
        throw new Error(
          `Unsupported terminal action: ${JSON.stringify(unexpectedAction)}`,
        );
      }
    }
  }
  return { tokens, cursor };
}
