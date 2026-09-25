import { mockHomeContentRepository } from "../../mocks/home/homeContent";
import type { HomeContentRepository } from "../api/homeContent";

/** Composition root: pages depend on this contract, never on fixture files. */
export const homeContentRepository: HomeContentRepository =
  mockHomeContentRepository;
