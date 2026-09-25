# Website content

This directory is the source of truth for documentation and blog content. React components and styles stay in `src/`; content compilation lives in `tooling/content.ts`.

## Structure

- `docs.json`: section ids, localized section names, and ordered article ids.
- `blog.json`: ordered blog article ids (publication date sorts first; this order resolves ties).
- `docs/<section>/<id>.en.md` and `<id>.zh.md`: English and Simplified Chinese documentation.
- `blog/<id>.en.md` and `<id>.zh.md`: blog translations.
- `assets/docs/` and `assets/blog/`: illustrations and other article media.

Filename suffix `zh` maps to website locale `zh-CN`; `en` maps to `en`. Both translations are required for registered articles. An article id is stable and consists of lowercase words separated by hyphens. Documentation ids must be unique across sections.

## Frontmatter

Every Markdown file requires `title` and `description`. Blog articles additionally require `author` and a quoted `date` in `YYYY-MM-DD` format. Do not duplicate title, description, language, or ordering in JSON/frontmatter. Language comes from the filename; ordering comes from the JSON arrays.

```yaml
---
title: "Quick Start"
description: "Build and boot X-kernel with QEMU."
---
```

Documentation may begin with an H1 matching the title; the renderer removes that first H1 because the page already renders the title. Blog bodies start below H1.

## Links and images

Use relative source paths, for example `quick-start.zh.md` and `../../assets/docs/boot-zh-CN.svg` in an Introduction article. Blog images use `../assets/blog/<name>.svg`. Prefer SVG for diagrams. Language-neutral illustrations can be shared; translated text inside an SVG needs separate assets.

The compiler resolves relative links to page URLs and images to `/markdown/assets/...`. Missing relative links, missing files, duplicate ids, or invalid metadata fail the build. Do not embed raw HTML image tags or use reference-style image links: use standard inline Markdown links/images so validation and rewriting apply.

## Output

- Documentation pages: `/docs/en/<id>` and `/docs/zh-CN/<id>`.
- Blog pages: `/blog/<id>`; the selected site language selects the translation.
- Raw Markdown: `/docs/<section>/<id>.en.md`, `.zh.md`, and `/blog/<id>.en.md`, `.zh.md`.
- Existing `/docs/<locale>/<id>.md` download links remain available.
- `/llms.txt`: English documentation and blog only. No Chinese index is generated.

Downloadable Markdown includes frontmatter and rewritten public links. Generated HTML and exported Markdown are written only to `dist/`, not back into this directory. Development serves the same generated endpoints; changing source content or JSON refreshes both registries.

## Adding an article

1. Add both language files with valid metadata.
2. Register the id in the appropriate JSON array.
3. Add SVGs under assets and reference them relatively.
4. Run `npm run build`, `npm run lint`, and `npm test`.
5. Against the running preview, run `npm run test:docs` and `npm run test:blog`.
