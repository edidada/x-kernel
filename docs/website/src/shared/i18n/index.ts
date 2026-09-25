import i18n from "i18next";
import LanguageDetector from "i18next-browser-languagedetector";
import { initReactI18next } from "react-i18next";
import enCommon from "./locales/en/common";
import enHome from "./locales/en/home";
import zhCommon from "./locales/zh-CN/common";
import zhHome from "./locales/zh-CN/home";

export const supportedLanguages = ["en", "zh-CN"] as const;
export type SupportedLanguage = (typeof supportedLanguages)[number];

const resources = {
  en: { common: enCommon, home: enHome },
  "zh-CN": { common: zhCommon, home: zhHome },
};

function syncDocumentLanguage() {
  document.documentElement.lang = i18n.resolvedLanguage ?? "en";
}
i18n.on("languageChanged", syncDocumentLanguage);

await i18n
  .use(LanguageDetector)
  .use(initReactI18next)
  .init({
    resources,
    supportedLngs: supportedLanguages,
    fallbackLng: "en",
    defaultNS: "common",
    detection: {
      order: ["localStorage", "navigator"],
      lookupLocalStorage: "x-kernel.language",
      caches: ["localStorage"],
      convertDetectedLanguage: (language) =>
        language.toLowerCase().startsWith("zh") ? "zh-CN" : language,
    },
    interpolation: { escapeValue: false },
  });
syncDocumentLanguage();

export default i18n;
