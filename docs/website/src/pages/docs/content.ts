import { compiledDocs } from "virtual:docs";
import type { DocsEntry } from "./docs.config";

export function contentFor(entry: DocsEntry) {
  const content = compiledDocs[`${entry.locale}/${entry.slug}`];
  if (!content) throw new Error(`Missing document: ${entry.slug}`);
  return content;
}
