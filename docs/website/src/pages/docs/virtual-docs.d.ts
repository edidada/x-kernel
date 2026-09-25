declare module "virtual:docs" {
  export const sectionsByLocale: Record<
    import("./docs.config").DocsLocale,
    import("./docs.config").DocsSection[]
  >;
  export const compiledDocs: Record<string, { markdown: string; html: string }>;
}
