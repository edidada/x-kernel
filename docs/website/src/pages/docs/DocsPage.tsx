import { useEffect, useMemo, useRef, useState } from "react";
import { Link, useParams, useNavigate } from "react-router-dom";
import { CopyMarkdownIcon } from "./DocsIcon";
import Navbar from "../../features/navigation/ui/Navbar";
import { docPath, type DocsEntry } from "./docs.config";
import { contentFor } from "./content";
import "./docs.css";
import { sectionsByLocale } from "virtual:docs";
import { useTranslation } from "react-i18next";

function DocumentArticle({ entry }: { entry: DocsEntry }) {
  const { markdown, html } = contentFor(entry);
  const zh = entry.locale === "zh-CN";
  const [copyStatus, setCopyStatus] = useState("idle");
  const copyLabel =
    copyStatus === "Copied"
      ? zh
        ? "已复制"
        : "Copied"
      : copyStatus === "failed"
        ? zh
          ? "复制失败 — 打开 Markdown"
          : "Copy failed — open Markdown"
        : zh
          ? "复制 Markdown"
          : "Copy as Markdown";
  return (
    <>
      <header className="docs-header">
        <h1 id={entry.slug}>{entry.title}</h1>
        <button
          type="button"
          aria-label={copyLabel}
          title={copyLabel}
          className={copyStatus === "Copied" ? "copied" : undefined}
          data-tooltip={copyLabel}
          onClick={async () => {
            try {
              await navigator.clipboard.writeText(markdown);
              setCopyStatus("Copied");
            } catch {
              setCopyStatus("failed");
            }
          }}
        >
          {copyStatus === "Copied" ? (
            <i className="docs-icon docs-icon-check" aria-hidden="true" />
          ) : (
            <CopyMarkdownIcon />
          )}
        </button>
      </header>
      {copyStatus === "failed" && (
        <a href={`${docPath(entry.locale, entry.slug)}.md`}>
          {zh ? "打开 Markdown" : "Open Markdown"}
        </a>
      )}
      {/* HTML is compiled and sanitized from repository Markdown before Vite starts. */}
      <div
        className="docs-content"
        lang={entry.locale}
        dangerouslySetInnerHTML={{ __html: html }}
      />
    </>
  );
}

export default function DocsPage() {
  const { slug, locale: routeLocale } = useParams<{
    slug?: string;
    locale?: string;
  }>();
  const { i18n } = useTranslation();
  const navigate = useNavigate();
  const locale = i18n.resolvedLanguage === "zh-CN" ? "zh-CN" : "en";
  const zh = locale === "zh-CN";
  const docsSections = useMemo(() => sectionsByLocale[locale], [locale]);
  const allDocs = docsSections.flatMap((section) => section.entries);
  const defaultDoc = allDocs[0];
  const initialRoute = useRef(true);
  useEffect(() => {
    if (initialRoute.current) {
      initialRoute.current = false;
      if (
        (routeLocale === "en" || routeLocale === "zh-CN") &&
        routeLocale !== locale
      ) {
        void i18n.changeLanguage(routeLocale);
        return;
      }
    }
    navigate(docPath(locale, slug ?? "what-is-x-kernel"), { replace: true });
  }, [locale, routeLocale, slug, navigate, i18n]);
  const current = slug
    ? allDocs.find((entry) => entry.slug === slug)
    : defaultDoc;
  const onePage = slug === "all";
  const [query, setQuery] = useState("");
  const [mobileMenu, setMobileMenu] = useState(false);
  const searchRef = useRef<HTMLInputElement>(null);
  const filteredSections = useMemo(
    () =>
      docsSections
        .map((section) => ({
          ...section,
          entries: section.entries.filter((entry) =>
            `${entry.title} ${contentFor(entry).markdown}`
              .toLowerCase()
              .includes(query.toLowerCase()),
          ),
        }))
        .filter((section) => section.entries.length > 0),
    [query, docsSections],
  );
  useEffect(() => {
    document.title = `${onePage ? "Documentation" : (current?.title ?? "Page not found")} - X-kernel docs`;
    window.scrollTo(0, 0);
  }, [current, onePage]);
  useEffect(() => {
    const focusSearch = (event: KeyboardEvent) => {
      if (
        event.key !== "/" ||
        event.target instanceof HTMLInputElement ||
        event.target instanceof HTMLTextAreaElement
      )
        return;
      event.preventDefault();
      setMobileMenu(true);
      requestAnimationFrame(() => searchRef.current?.focus());
    };
    document.addEventListener("keydown", focusSearch);
    return () => document.removeEventListener("keydown", focusSearch);
  }, []);
  const index = current ? allDocs.indexOf(current) : -1;
  const previous = allDocs[index - 1];
  const next = current ? allDocs[index + 1] : undefined;
  return (
    <div className="docs-route">
      <header className="docs-topbar">
        <Navbar />
      </header>
      <main className="docs-page">
        <button
          type="button"
          className="docs-mobile-toggle"
          aria-expanded={mobileMenu}
          aria-controls="docs-sidebar"
          onClick={() => setMobileMenu(!mobileMenu)}
        >
          ☰ {zh ? "文档" : "Documentation"}
        </button>
        <div className="docs-container">
          <aside
            id="docs-sidebar"
            className={`docs-sidebar${mobileMenu ? " is-open" : ""}`}
          >
            <label className="docs-search">
              <i className="docs-icon docs-icon-search" aria-hidden="true" />
              <input
                ref={searchRef}
                value={query}
                onChange={(event) => setQuery(event.target.value)}
                placeholder={zh ? "搜索文档" : "Search docs"}
                aria-label={zh ? "搜索文档" : "Search documentation"}
              />
              {query ? (
                <button
                  type="button"
                  className="docs-search-clear"
                  aria-label={zh ? "清空搜索" : "Clear search"}
                  onClick={() => {
                    setQuery("");
                    searchRef.current?.focus();
                  }}
                >
                  <i className="docs-icon docs-icon-times" aria-hidden="true" />
                </button>
              ) : (
                <kbd>/</kbd>
              )}
            </label>
            <div className="docs-sidebar-top">
              <Link to={docPath(locale, "all")}>
                {zh ? "完整文档" : "One Page"}
              </Link>
              <span>·</span>
              <a href="/llms.txt">llms.txt</a>
            </div>
            {filteredSections.map((section) => (
              <details key={section.title} open className="docs-section">
                <summary>{section.title}</summary>
                {section.entries.map((entry) => (
                  <Link
                    aria-current={
                      entry.slug === current?.slug ? "page" : undefined
                    }
                    className={entry.slug === current?.slug ? "active" : ""}
                    key={entry.slug}
                    to={docPath(locale, entry.slug)}
                    onClick={() => setMobileMenu(false)}
                  >
                    {entry.title}
                  </Link>
                ))}
              </details>
            ))}
            {!filteredSections.length && (
              <p role="status">{zh ? "未找到文档。" : "No documents found."}</p>
            )}
          </aside>
          <article className="docs-main">
            {onePage ? (
              allDocs.map((entry) => (
                <section className="docs-full-article" key={entry.slug}>
                  <DocumentArticle entry={entry} />
                </section>
              ))
            ) : current ? (
              <DocumentArticle
                key={`${locale}/${current.slug}`}
                entry={current}
              />
            ) : (
              <>
                <h1>{zh ? "未找到页面" : "Page not found"}</h1>
                <Link to="/docs">
                  {zh ? "返回文档" : "Back to documentation"}
                </Link>
              </>
            )}
            {!onePage && current && (
              <div
                className="docs-page-nav"
                role="navigation"
                aria-label="Documentation navigation"
              >
                {previous && (
                  <Link to={docPath(locale, previous.slug)}>
                    ← {previous.title}
                  </Link>
                )}
                {next && (
                  <Link className="docs-next" to={docPath(locale, next.slug)}>
                    {next.title} →
                  </Link>
                )}
              </div>
            )}
          </article>
        </div>
      </main>
    </div>
  );
}
