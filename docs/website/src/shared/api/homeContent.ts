import type { TerminalRecording } from "../../features/terminal-player/lib/playback";

/** The contract a real CMS/API adapter must satisfy. */
export interface HomeContentRepository {
  getTerminalRecordings(): readonly [TerminalRecording, ...TerminalRecording[]];
}
