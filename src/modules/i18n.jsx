import React, { createContext, useContext, useState, useEffect } from 'react';
import { translations } from './translations';

const I18nContext = createContext();

const getInitialLanguage = () => {
  try {
    // 1. 优先遵循用户在软件内手动切换并保存的语言
    const savedLang = localStorage.getItem('app_lang');
    if (savedLang === 'zh' || savedLang === 'en') {
      return savedLang;
    }

    // 2. 如果没有缓存，读取系统/浏览器环境语言
    // 现代 Mac 系统的 Webview 容器会提供系统首选语言列表
    const systemLanguages = navigator.languages || [navigator.language];
    
    for (const lang of systemLanguages) {
      if (!lang) continue;
      const lowerLang = lang.toLowerCase();
      if (lowerLang.startsWith('zh')) {
        return 'zh';
      }
      if (lowerLang.startsWith('en')) {
        return 'en';
      }
    }
  } catch (e) {
    console.error("Failed to detect system language, fallback to english.", e);
  }

  // 3. 兜底策略：既然你是要上架微软商店，为了通过审核，兜底必须是英文（en）
  // 这样任何海外审核员打开，如果系统不是中文，一律默认显示英文，100% 安全
  return 'en';
};

export function I18nProvider({ children }) {
  // 💡 使用智能获取的语言作为状态初值
  const [language, setLanguage] = useState(getInitialLanguage);

  useEffect(() => {
    localStorage.setItem('app_lang', language);
  }, [language]);

  const t = (path, params = {}) => {
    const keys = path.split('.');
    let value = translations[language];
    
    for (const key of keys) {
      if (!value[key]) return path;
      value = value[key];
    }

    if (typeof value === 'string') {
      let result = value;
      Object.entries(params).forEach(([k, v]) => {
        result = result.replace(`{{${k}}}`, v);
      });
      return result;
    }
    
    return path;
  };

  return (
    <I18nContext.Provider value={{ t, language, setLanguage }}>
      {children}
    </I18nContext.Provider>
  );
}

export function useTranslation() {
  const context = useContext(I18nContext);
  if (!context) throw new Error('useTranslation must be used within I18nProvider');
  return context;
}
