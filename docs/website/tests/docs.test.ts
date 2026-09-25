import test from "node:test";
import assert from "node:assert/strict";
import { compileContent } from "../tooling/content";
test("content registry builds bilingual pages and an English-only index", async () => {
  const content = await compileContent(process.cwd());
  assert.equal(Object.keys(content.compiledDocs).length, 6);
  assert.equal(content.posts.length, 4);
  assert.deepEqual(
    content.sectionsByLocale.en[0].entries.map((e) => e.slug),
    content.sectionsByLocale["zh-CN"][0].entries.map((e) => e.slug),
  );
  const index = String(content.assets["llms.txt"]);
  assert.ok(index.includes("/docs/introduction/quick-start.en.md"));
  assert.ok(index.includes("/blog/kernel-component-boundaries.en.md"));
  assert.ok(!index.includes(".zh.md"));
  for (const match of index.matchAll(/\]\(\/([^)]*)\)/g))
    assert.ok(content.assets[match[1]], match[1]);
});
