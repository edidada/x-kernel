export interface BlogPost {
  slug: string;
  locale: "en" | "zh-CN";
  title: string;
  date: string;
  author: string;
  description: string;
  /** Ordering for posts published on the same date. */
  order: number;
}
