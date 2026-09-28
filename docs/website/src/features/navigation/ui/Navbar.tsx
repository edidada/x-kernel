import { links } from "../model/navigation";
import pandaLogo from "../../../../assets/images/x-kernel/panda.svg";
import ThemeToggle from "../../../shared/ThemeToggle";
import { useTranslation } from "react-i18next";

export default function Navbar() {
  const { t, i18n } = useTranslation();
  return (
    <nav data-nav-collapse>
      <div className="nav-left">
        <a href={links.home} className="nav-logo">
          <img src={pandaLogo} alt="Panda" />
        </a>
        <a href={links.home} className="nav-logo-text">
          x-kernel
        </a>
        <a href={links.docs} className="nav-link nav-link-docs">
          {t("nav.docs")}
        </a>
        <a href={links.blog} className="nav-link nav-link-blog">
          {t("nav.blog")}
        </a>
        <a href={links.community} className="nav-link nav-link-community">
          {t("nav.community")}
        </a>
      </div>
      <div className="nav-right">
        <ThemeToggle />
        <div
          className="language-switcher"
          role="group"
          aria-label={t("nav.language")}
        >
          <button
            className={`language-option${i18n.resolvedLanguage === "zh-CN" ? " active" : ""}`}
            type="button"
            aria-pressed={i18n.resolvedLanguage === "zh-CN"}
            aria-label={t("nav.switchTo", { language: "中文" })}
            onClick={() => void i18n.changeLanguage("zh-CN")}
          >
            中
          </button>
          <span className="language-divider">/</span>
          <button
            className={`language-option${i18n.resolvedLanguage === "en" ? " active" : ""}`}
            type="button"
            aria-pressed={i18n.resolvedLanguage === "en"}
            aria-label={t("nav.switchTo", { language: "English" })}
            onClick={() => void i18n.changeLanguage("en")}
          >
            En
          </button>
        </div>
      </div>
    </nav>
  );
}
