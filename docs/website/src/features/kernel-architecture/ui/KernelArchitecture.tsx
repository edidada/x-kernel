import { useState } from "react";
import { useTranslation } from "react-i18next";
import "./kernel-architecture.css";

const crateRows = [
  ["kexec", "kprocess", "ktask", "ksched"],
  ["memspace", "kalloc", "page_table", "vmobj"],
  ["kvfs", "filemap", "ksync", "kspin"],
] as const;

export default function KernelArchitecture() {
  const { t } = useTranslation("home");
  const [paused, setPaused] = useState(false);
  return (
    <section
      className="kernel-architecture home-section"
      aria-labelledby="architecture-title"
    >
      <h2 id="architecture-title">{t("architecture.title")}</h2>
      <div className="kernel-architecture-panel">
        <div className="kernel-architecture-label">
          <span>X-kernel architecture</span>
          <span>{t("architecture.badge")}</span>
        </div>
        <div className="kernel-architecture-layer">
          {t("architecture.userSpace")}
        </div>
        <div className="kernel-architecture-interface">
          {t("architecture.syscall")}
        </div>
        <div
          className="kernel-crates"
          data-paused={paused}
          onPointerLeave={(event) => {
            if (event.pointerType === "mouse") setPaused(false);
          }}
        >
          <div className="kernel-architecture-label">
            <span>{t("architecture.kernelSpace")}</span>
            <button
              type="button"
              className="kernel-crates-toggle"
              onClick={() => setPaused(!paused)}
              aria-label={t(
                paused ? "architecture.resume" : "architecture.pause",
              )}
              aria-pressed={paused}
            >
              {paused ? "▶" : "Ⅱ"}
            </button>
          </div>
          <div className="kernel-crates-rows">
            {crateRows.map((row, index) => (
              <div className="kernel-crates-row" key={index}>
                <div className="kernel-crates-track">
                  {[false, true].map((duplicate) => (
                    <div
                      className="kernel-crates-group"
                      key={String(duplicate)}
                      aria-hidden={duplicate || undefined}
                    >
                      {row.map((name) => (
                        <span className="kernel-crate" key={name}>
                          {name}
                        </span>
                      ))}
                    </div>
                  ))}
                </div>
              </div>
            ))}
          </div>
        </div>
        <div className="kernel-architecture-layer">
          {t("architecture.hardware")}
        </div>
        <p className="kernel-architecture-note">{t("architecture.note")}</p>
      </div>
    </section>
  );
}
