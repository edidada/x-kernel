import "./home-sections.css";
import { useEffect } from "react";
import { homeContentRepository } from "../../shared/config/content";
import TerminalPlayer from "../../features/terminal-player/ui/TerminalPlayer";

import Navbar from "../../features/navigation/ui/Navbar";
import KernelArchitecture from "../../features/kernel-architecture/ui/KernelArchitecture";
import KernelExplorer from "../../features/kernel-explorer/ui/KernelExplorer";
import { useTranslation } from "react-i18next";

export default function HomePage() {
  const { t } = useTranslation("home");
  const content = homeContentRepository;
  useEffect(() => {
    document.title = t("hero.title");
    document
      .querySelector('meta[name="description"]')
      ?.setAttribute("content", t("hero.subtitle"));
  }, [t]);
  return (
    <>
      <div className="above-fold">
        <Navbar />
        <section className="hero home-page-1">
          <div className="hero-headline">
            <div className="hero-spacer" />
            <div className="home-hed">
              <h1 className="hero-hed">{t("hero.title")}</h1>
              <p className="hero-dek">{t("hero.subtitle")}</p>
            </div>
            <div className="hero-spacer" />
          </div>
        </section>
      </div>
      <section className="home-page-2 home-section">
        <div className="home-page-head">
          <h2 className="home-section-title">{t("api.title")}</h2>
        </div>
        <TerminalPlayer recordings={content.getTerminalRecordings()} />
      </section>
      <KernelExplorer />
      <KernelArchitecture />
      <section
        className="xkernel-architectures home-section"
        aria-labelledby="architectures-title"
      >
        <div>
          <p className="xkernel-eyebrow">{t("architectures.eyebrow")}</p>
          <h2 id="architectures-title">{t("architectures.title")}</h2>
          <p>{t("architectures.subtitle")}</p>
        </div>
        <div>
          <div className="architecture-support-grid">
            {["aarch64", "x86", "riscv", "loongarch"].map((key) => (
              <div key={key}>
                <strong>{t(`architectures.items.${key}.name`)}</strong>
                <small>{t(`architectures.items.${key}.description`)}</small>
              </div>
            ))}
          </div>
          <p className="platform-line">
            <span />
            {t("architectures.platforms")}
          </p>
        </div>
      </section>
      <div className="home-closing home-section">
        <section className="xkernel-values" aria-labelledby="values-title">
          <p className="xkernel-eyebrow">{t("values.eyebrow")}</p>
          <h2 id="values-title">{t("values.title")}</h2>
          <div className="value-grid">
            {["rust", "tee", "open"].map((key, index) => (
              <article key={key}>
                <small>
                  0{index + 1} / {t(`values.items.${key}.label`)}
                </small>
                <h3>{t(`values.items.${key}.title`)}</h3>
                <p>{t(`values.items.${key}.description`)}</p>
              </article>
            ))}
          </div>
        </section>
        <section className="xkernel-cta">
          <div>
            <h2>{t("cta.title")}</h2>
            <p>{t("cta.subtitle")}</p>
          </div>
          <div className="xkernel-cta-actions">
            <a href="/docs">{t("cta.docs")} ↗</a>
            <a
              className="primary"
              href="https://gitee.com/openkylin/x-kernel"
              target="_blank"
              rel="noreferrer"
            >
              {t("cta.source")} ↗
            </a>
          </div>
        </section>
      </div>
      <footer className="xkernel-footer">
        <span>X-kernel · Apache-2.0</span>
        <span>
          <a href="/docs">Docs</a>
          <i>/</i>
          <a href="https://gitee.com/openkylin/x-kernel">Gitee</a>
          <i>/</i>
          <a href="https://x-kernel.zulipchat.com">Community</a>
        </span>
      </footer>
    </>
  );
}
