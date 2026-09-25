declare module "virtual:blog" {
  export const posts: import("./model").BlogPost[];
  export const articles: Record<string, string>;
}
