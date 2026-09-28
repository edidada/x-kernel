import { useState } from "react";
import { useTranslation } from "react-i18next";
import { KindIcon } from "./KindIcon";
import "../../../shared/styles/builder.css";
import "../../../shared/styles/kernel-explorer.css";

const modules = [
  ["memory", "checklist"],
  ["scheduler", "window"],
  ["filesystem", "terminal"],
  ["network", "gear"],
  ["drivers", "stack"],
  ["tee", "more"],
] as const;
const paths: Record<string, string> = {
  memory: "mm/ · mm/memspace/ · core/kuaccess/",
  scheduler: "task/ · process/",
  filesystem: "fs/ · fs/filesystems/",
  network: "net/ · posix/net/",
  drivers: "drivers/ · io/",
  tee: "tee/ · tee_apps/",
};
export default function KernelExplorer() {
  const { t } = useTranslation("home");
  const [active, setActive] = useState("memory");

  const repo = "https://gitee.com/openkylin/x-kernel";
  return (
    <section
      className="kernel-explorer kernel-explorer-page home-section"
      aria-labelledby="kernel-explorer-title"
    >
      <div className="kernel-explorer-heading">
        <h2 id="kernel-explorer-title">{t("explorer.title")}</h2>
      </div>
      <div className="explorer-composer kernel-composer">
        <div className="form-group">
          <div
            className="kind-row kernel-kind-row"
            role="tablist"
            aria-label={t("explorer.modulesLabel")}
          >
            <div className="kind-radios">
              {modules.map(([id, icon]) => (
                <button
                  key={id}
                  type="button"
                  role="tab"
                  aria-selected={id === active}
                  className={`kind-btn ${id === active ? "active" : "dimmed"}`}
                  onClick={() => setActive(id)}
                >
                  <span className="kind-btn-icon">
                    {icon === "more" ? (
                      <span className="kind-more-icon">…</span>
                    ) : (
                      <KindIcon
                        kind={
                          icon as
                            | "checklist"
                            | "window"
                            | "terminal"
                            | "gear"
                            | "stack"
                        }
                      />
                    )}
                  </span>
                  <span className="kind-btn-label">
                    {t(`explorer.modules.${id}.label`)}
                  </span>
                </button>
              ))}
            </div>
          </div>
        </div>
        <div className="steps">
          <div className="prompt-box kernel-prompt-box" role="tabpanel">
            <div className="name-line">
              <div
                className="kernel-module-path name-display"
                aria-label={t("explorer.openSource")}
              >
                <strong>X-kernel</strong>
                <span>/</span>
                <span>{active === "memory" ? "mm" : active}</span>
              </div>
            </div>
            <div className="kernel-prompt-copy">
              <h3>{t(`explorer.modules.${active}.title`)}</h3>
              <p>{t(`explorer.modules.${active}.description`)}</p>
            </div>
            <div className="kernel-source-path">{paths[active]}</div>
            <div className="prompt-box-bar">
              <div className="pill-row">
                <a
                  className="pill-link"
                  href={`${repo}/tree/main/${paths[active].split("/")[0]}`}
                  target="_blank"
                  rel="noreferrer"
                >
                  #
                </a>
              </div>
              <a
                className="send-btn"
                href={`${repo}/tree/main/${paths[active].split("/")[0]}`}
                target="_blank"
                rel="noreferrer"
                aria-label={t("explorer.openSource")}
              >
                <svg
                  width="18"
                  height="18"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="2"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                >
                  <path d="M5 12h14M13 6l6 6-6 6" />
                </svg>
              </a>
            </div>
          </div>
        </div>
      </div>
      <p className="kernel-module-caption">
        <strong>{t(`explorer.modules.${active}.name`)}</strong>
        <span>{t(`explorer.modules.${active}.keywords`)}</span>
      </p>
    </section>
  );
}
