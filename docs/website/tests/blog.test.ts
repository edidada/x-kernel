import test from "node:test";
import assert from "node:assert/strict";
import { compileBlogPost } from "../tooling/blog-content";
const source = `---
title: "Test post"
date: "2026-09-23"
author: "Author"
description: "Summary"
---
## Heading

**Markdown** and [a link](https://x-kernel.com).

<script>alert(1)</script>
<iframe src="https://www.youtube.com/embed/example" title="Video"></iframe>
<iframe src="https://unknown.example/embed"></iframe>
`;
test("blog compiles frontmatter and Markdown while retaining approved embeds", () => {
  const result = compileBlogPost(source, "example");
  assert.equal(result.post.title, "Test post");
  assert.match(result.html, /<strong>Markdown<\/strong>/);
  assert.match(result.html, /id="heading"/);
  assert.match(result.html, /youtube.com/);
  assert.doesNotMatch(result.html, /script|unknown.example/);
});
test("blog reports invalid metadata before rendering", () => {
  assert.throws(
    () =>
      compileBlogPost(
        source.replace('title: "Test post"', "title: 12"),
        "example",
      ),
    /title/,
  );
  assert.throws(
    () =>
      compileBlogPost(source.replace("2026-09-23", "2026-02-30"), "example"),
    /date/,
  );
});
