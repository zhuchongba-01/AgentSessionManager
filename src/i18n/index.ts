import i18n from "i18next";
import { initReactI18next } from "react-i18next";

import zh from "./session-zh.json";

const resources = {
  zh: {
    translation: zh,
  },
};

i18n.use(initReactI18next).init({
  resources,
  lng: "zh",
  fallbackLng: "zh",

  interpolation: {
    escapeValue: false, // React 已经默认转义
  },

  // 开发模式下显示调试信息
  debug: false,
});

export default i18n;
