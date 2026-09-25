import matter from "gray-matter";
import { Marked } from "marked";
import { gfmHeadingId } from "marked-gfm-heading-id";
import sanitizeHtml from "sanitize-html";
import type { BlogPost } from "../src/pages/blog/model.ts";

export function compileBlogPost(source: string, slug: string) {
  const { data, content } = matter(source);
  const required = (key: string): string => {
    const value: unknown = data[key];
    if (typeof value !== "string" || !value.trim())
      throw new Error(
        `Blog "${slug}": frontmatter "${key}" must be a nonempty string.`,
      );
    return value;
  };
  const date = required("date");
  if (
    !/^\d{4}-\d{2}-\d{2}$/.test(date) ||
    Number.isNaN(Date.parse(date)) ||
    new Date(date).toISOString().slice(0, 10) !== date
  )
    throw new Error(`Blog "${slug}": date must be YYYY-MM-DD.`);
  const order: unknown = data.order ?? 0;
  if (typeof order !== "number" || !Number.isSafeInteger(order))
    throw new Error(`Blog "${slug}": order must be an integer.`);
  const locale = data.locale ?? "en";
  if (locale !== "en" && locale !== "zh-CN")
    throw new Error(`Blog "${slug}": invalid locale.`);
  const post: BlogPost = {
    locale,
    slug,
    title: required("title"),
    date,
    author: required("author"),
    description: required("description"),
    order,
  };
  const parser = new Marked({ gfm: true, async: false });
  parser.use(gfmHeadingId());
  const html = sanitizeHtml(parser.parse(content) as string, {
    allowedTags: sanitizeHtml.defaults.allowedTags.concat(["img", "iframe"]),
    allowedAttributes: {
      ...sanitizeHtml.defaults.allowedAttributes,
      "*": ["class", "id"],
      iframe: [
        "src",
        "title",
        "width",
        "height",
        "allow",
        "allowfullscreen",
        "loading",
        "frameborder",
      ],
      img: ["src", "alt", "width", "height", "loading"],
    },
    allowedIframeHostnames: ["www.youtube.com"],
  });
  return { post, html };
}
