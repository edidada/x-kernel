import type { HomeContentRepository } from "../../shared/api/homeContent";
import { terminalRecordings } from "../../features/terminal-player/model/recordings";

/** Local fixture adapter. Replace this object with an HTTP adapter later. */
export const mockHomeContentRepository: HomeContentRepository = {
  getTerminalRecordings: () => terminalRecordings,
};
