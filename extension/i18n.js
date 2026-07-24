(function (globalScope) {
  function getI18nApi() {
    return globalScope.chrome?.i18n;
  }

  function getMessageOrFallback(key, fallback, i18nApi = getI18nApi()) {
    try {
      const message = i18nApi?.getMessage(key);
      return typeof message === "string" && message.trim() ? message : fallback;
    } catch (_err) {
      return fallback;
    }
  }

  function applyDocumentTranslations(documentRef) {
    if (!documentRef) return;

    for (const element of documentRef.querySelectorAll("[data-i18n]")) {
      element.textContent = getMessageOrFallback(element.dataset.i18n, element.textContent);
    }

    for (const element of documentRef.querySelectorAll("[data-i18n-aria-label]")) {
      element.setAttribute(
        "aria-label",
        getMessageOrFallback(element.dataset.i18nAriaLabel, element.getAttribute("aria-label") || ""),
      );
    }

    try {
      const uiLanguage = getI18nApi()?.getUILanguage();
      if (typeof uiLanguage === "string" && uiLanguage.trim()) {
        documentRef.documentElement.lang = uiLanguage;
      }
    } catch (_err) {
      // Keep the English HTML language fallback when Chrome i18n is unavailable.
    }
  }

  const api = { getMessageOrFallback, applyDocumentTranslations };
  globalScope.PedelecI18n = api;

  if (typeof module !== "undefined") {
    module.exports = api;
  }
})(typeof globalThis !== "undefined" ? globalThis : this);
