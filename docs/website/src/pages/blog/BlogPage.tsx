import { useEffect } from "react";
import { Link, useParams } from "react-router-dom";
import Navbar from "../../features/navigation/ui/Navbar";
import { posts, articles } from "virtual:blog";
import "./blog.css";
import { useTranslation } from "react-i18next";

export default function BlogPage() {
  const { slug } = useParams<{ slug?: string }>();
  const { i18n } = useTranslation();
  const locale = i18n.resolvedLanguage === "zh-CN" ? "zh-CN" : "en";
  const localizedPosts = posts.filter((post) => post.locale === locale);
  const post = localizedPosts.find((post) => post.slug === slug);
  useEffect(() => {
    document.title = post ? post.title + " — X-kernel blog" : "X-kernel blog";
    window.scrollTo(0, 0);
  }, [post]);
  return (
    <div
      className={`blog-route ${slug ? "blog-article" : "blog-list"}`}
      lang={locale}
    >
      <div className="blog-topbar">
        <header>
          <Navbar />
        </header>
      </div>
      <main>
        {!slug ? (
          <section className="post-list">
            {localizedPosts.map((post) => (
              <article className="post-card" key={post.slug}>
                <Link to={`/blog/${post.slug}`}>
                  <h2>{post.title}</h2>
                  <div className="post-meta">
                    <span>{post.date}</span>
                    <span>{post.author}</span>
                  </div>
                  <p className="post-description">{post.description}</p>
                </Link>
              </article>
            ))}
          </section>
        ) : post ? (
          <>
            <header className="post-header">
              <h1 className="post-title">{post.title}</h1>
              <div className="post-meta">
                <span>{post.date}</span>
                <span>{post.author}</span>
              </div>
            </header>
            {/* Markdown is compiled and sanitized by the build plugin. */}
            <article
              className="post-content"
              dangerouslySetInnerHTML={{
                __html: articles[`${locale}/${post.slug}`],
              }}
            />
            <footer className="post-footer">
              <Link to="/blog">
                ← {locale === "zh-CN" ? "全部文章" : "All posts"}
              </Link>
            </footer>
          </>
        ) : (
          <>
            <h1>Post not found</h1>
            <Link to="/blog">
              ← {locale === "zh-CN" ? "全部文章" : "All posts"}
            </Link>
          </>
        )}
      </main>
    </div>
  );
}
