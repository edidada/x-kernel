import { readFile, readdir } from "node:fs/promises";
import path from "node:path";
import matter from "gray-matter";
import { Marked } from "marked";
import { gfmHeadingId } from "marked-gfm-heading-id";
import sanitizeHtml from "sanitize-html";
import type { Plugin } from "vite";
import type { DocsSection } from "../src/pages/docs/docs.config.ts";
import { compileBlogPost } from "./blog-content.ts";

const languages = { en: "en", zh: "zh-CN" } as const;
type Language = keyof typeof languages;
type Source = {
  kind: "docs" | "blog";
  slug: string;
  language: Language;
  file: string;
  source: string;
  title: string;
  description: string;
  order: number;
};
const virtualDocs = "\0virtual:docs";
const virtualBlog = "\0virtual:blog";

/** One registry drives rendered pages, downloadable sources, assets and llms.txt. */
export async function compileContent(root: string) {
  const directory = path.join(root, "markdown");
  const docsConfig = JSON.parse(
    await readFile(path.join(directory, "docs.json"), "utf8"),
  ) as {
    sections: {
      id: string;
      title: Record<Language, string>;
      articles: string[];
    }[];
  };
  const blogConfig = JSON.parse(
    await readFile(path.join(directory, "blog.json"), "utf8"),
  ) as { articles: string[] };
  const sources: Source[] = [];
  const assets: Record<string, string | Uint8Array> = {};
  const sectionsByLocale: Record<string, DocsSection[]> = {
    en: [],
    "zh-CN": [],
  };
  const ids = new Set<string>();
  async function article(
    kind: Source["kind"],
    slug: string,
    language: Language,
    file: string,
    order: number,
  ) {
    if (!/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(slug))
      throw new Error(`Invalid article id: ${slug}`);
    const key = `${kind}/${language}/${slug}`;
    if (ids.has(key)) throw new Error(`Duplicate article: ${key}`);
    ids.add(key);
    const source = await readFile(path.join(directory, file), "utf8");
    const { data } = matter(source);
    for (const field of ["title", "description"])
      if (typeof data[field] !== "string" || !data[field].trim())
        throw new Error(`${file}: missing ${field}`);
    const entry = {
      kind,
      slug,
      language,
      file,
      source,
      title: data.title as string,
      description: data.description as string,
      order,
    };
    sources.push(entry);
    return {
      slug,
      locale: languages[language],
      file,
      title: entry.title,
      description: entry.description,
    };
  }
  const sectionIds = new Set<string>();
  for (const section of docsConfig.sections) {
    if (!/^[a-z0-9-]+$/.test(section.id) || sectionIds.has(section.id))
      throw new Error(`Invalid or duplicate section: ${section.id}`);
    sectionIds.add(section.id);
    for (const language of Object.keys(languages) as Language[]) {
      if (!section.title[language])
        throw new Error(`Missing section title: ${section.id}/${language}`);
      const entries = [];
      for (const [order, slug] of section.articles.entries())
        entries.push(
          await article(
            "docs",
            slug,
            language,
            `docs/${section.id}/${slug}.${language}.md`,
            order,
          ),
        );
      sectionsByLocale[languages[language]].push({
        title: `${sectionIds.size}. ${section.title[language]}`,
        entries,
      });
    }
  }
  for (const language of Object.keys(languages) as Language[])
    for (const [order, slug] of blogConfig.articles.entries())
      await article(
        "blog",
        slug,
        language,
        `blog/${slug}.${language}.md`,
        order,
      );
  for (const name of await readdir(path.join(directory, "assets"), {
    recursive: true,
    withFileTypes: true,
  })) {
    if (!name.isFile()) continue;
    const file = path.join(name.parentPath, name.name);
    assets[
      `markdown/${path.relative(directory, file).split(path.sep).join("/")}`
    ] = await readFile(file);
  }
  const route = (entry: Source) =>
    entry.kind === "docs"
      ? `/docs/${languages[entry.language]}/${entry.slug}`
      : `/blog/${entry.slug}`;
  const byFile = new Map(sources.map((entry) => [entry.file, entry]));
  async function rewrite(entry: Source) {
    const { content } = matter(entry.source);
    // Rewrite destinations while retaining readable, downloadable Markdown.
    return content.replace(
      /(!?\[[^\]]*\]\()([^\s)]+)(\))/g,
      (_match, start: string, href: string, end: string) => {
        if (/^(?:[a-z]+:|\/\/|#)/i.test(href)) return start + href + end;
        if (href.startsWith("/")) {
          if (
            /^\/(docs|blog)\//.test(href) &&
            !sources.some((item) => route(item) === href.split("#")[0])
          )
            throw new Error(`${entry.file}: unknown article link ${href}`);
          return start + href + end;
        }
        const [relative, hash] = href.split("#");
        const target = path.posix.normalize(
          path.posix.join(path.posix.dirname(entry.file), relative),
        );
        const linked = byFile.get(target);
        const url = linked
          ? route(linked)
          : assets[`markdown/${target}`]
            ? `/markdown/${target}`
            : undefined;
        if (!url)
          throw new Error(`${entry.file}: missing link or image ${href}`);
        return start + url + (hash ? `#${hash}` : "") + end;
      },
    );
  }
  const compiledDocs: Record<string, { markdown: string; html: string }> = {};
  const posts = [];
  const articles: Record<string, string> = {};
  for (const entry of sources) {
    const content = await rewrite(entry);
    const key = `${languages[entry.language]}/${entry.slug}`;
    assets[entry.file] = matter.stringify(content, matter(entry.source).data);
    // Canonical raw downloads remain usable alongside existing shared links.
    if (entry.kind === "docs")
      assets[`docs/${languages[entry.language]}/${entry.slug}.md`] =
        matter.stringify(content, matter(entry.source).data);
    if (entry.kind === "blog") {
      const compiled = compileBlogPost(
        matter.stringify(content, {
          ...matter(entry.source).data,
          locale: languages[entry.language],
          order: entry.order,
        }),
        entry.slug,
      );
      posts.push(compiled.post);
      articles[key] = compiled.html;
    } else {
      const parser = new Marked({ gfm: true, async: false });
      parser.use(gfmHeadingId());
      const tokens = parser.lexer(content.trimStart());
      if (tokens[0]?.type === "heading" && tokens[0].depth === 1)
        tokens.shift();
      const html = sanitizeHtml(parser.parser(tokens) as string, {
        allowedTags: sanitizeHtml.defaults.allowedTags.concat(["img"]),
        allowedAttributes: {
          ...sanitizeHtml.defaults.allowedAttributes,
          h2: ["id"],
          h3: ["id"],
          h4: ["id"],
          code: ["class"],
        },
      });
      compiledDocs[key] = { markdown: content, html };
    }
  }
  posts.sort((a, b) => b.date.localeCompare(a.date) || a.order - b.order);
  let index =
    "# X-kernel\n\n> A componentized, multi-architecture monolithic kernel written in Rust.\n";
  for (const section of sectionsByLocale.en) {
    index += `\n## ${section.title}\n\n`;
    for (const entry of section.entries)
      index += `- [${entry.title}](/${entry.file}): ${entry.description}\n`;
  }
  index += "\n## Blog\n\n";
  for (const post of posts.filter((post) => post.locale === "en")) {
    const entry = sources.find(
      (entry) =>
        entry.kind === "blog" &&
        entry.language === "en" &&
        entry.slug === post.slug,
    )!;
    index += `- [${entry.title}](/${entry.file}): ${entry.description}\n`;
  }
  assets["llms.txt"] = index;
  return { sectionsByLocale, compiledDocs, posts, articles, assets };
}

export function contentPlugin(): Plugin {
  let root: string;
  let pending: ReturnType<typeof compileContent> | undefined;
  const compiled = () => (pending ??= compileContent(root));
  return {
    name: "markdown-content",
    configResolved(config) {
      root = config.root;
    },
    buildStart() {
      pending = undefined;
    },
    resolveId(id) {
      if (id === "virtual:docs") return virtualDocs;
      if (id === "virtual:blog") return virtualBlog;
    },
    async load(id) {
      if (id !== virtualDocs && id !== virtualBlog) return;
      const data = await compiled();
      return id === virtualDocs
        ? `export const sectionsByLocale=${JSON.stringify(data.sectionsByLocale)}; export const compiledDocs=${JSON.stringify(data.compiledDocs)};`
        : `export const posts=${JSON.stringify(data.posts)}; export const articles=${JSON.stringify(data.articles)};`;
    },
    async generateBundle() {
      for (const [fileName, source] of Object.entries(
        (await compiled()).assets,
      ))
        this.emitFile({ type: "asset", fileName, source });
    },
    configureServer(server) {
      const directory = path.join(root, "markdown");
      server.watcher.add(directory);
      const refresh = (file: string) => {
        if (!file.startsWith(directory + path.sep)) return;
        pending = undefined;
        for (const id of [virtualDocs, virtualBlog]) {
          const module = server.moduleGraph.getModuleById(id);
          if (module) server.moduleGraph.invalidateModule(module);
        }
        server.ws.send({ type: "full-reload" });
      };
      server.watcher.on("all", (_event, file) => refresh(file));
      server.middlewares.use(async (req, res, next) => {
        const pathname = decodeURIComponent(
          new URL(req.url ?? "/", "http://localhost").pathname,
        ).slice(1);
        if (!(
          pathname === "llms.txt" ||
          pathname.startsWith("markdown/assets/") ||
          /^(docs|blog)\/.*\.md$/.test(pathname)
        ))
          return next();
        try {
          const asset = (await compiled()).assets[pathname];
          if (asset === undefined) {
            res.statusCode = 404;
            res.end("Not found");
            return;
          }
          res.setHeader(
            "Content-Type",
            pathname.endsWith(".svg")
              ? "image/svg+xml"
              : pathname.endsWith(".png")
                ? "image/png"
                : "text/plain; charset=utf-8",
          );
          res.end(asset);
        } catch (error) {
          next(error);
        }
      });
    },
  };
}
