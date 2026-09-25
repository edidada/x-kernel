export type DocsLocale = "en" | "zh-CN";
export type DocsEntry = {
  slug: string;
  title: string;
  description: string;
  file: string;
  locale: DocsLocale;
};
export type DocsSection = { title: string; entries: DocsEntry[] };
export const docPath = (locale: DocsLocale, slug: string) =>
  `/docs/${locale}/${slug}`;
