const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const { getMessageOrFallback } = require("./i18n.js");

const extensionDir = __dirname;
const readJson = (relativePath) => JSON.parse(fs.readFileSync(path.join(extensionDir, relativePath), "utf8"));
const enMessages = readJson("_locales/en/messages.json");
const zhTwMessages = readJson("_locales/zh_TW/messages.json");

function messageKeysFromHtml(html) {
  return [...html.matchAll(/\bdata-i18n(?:-aria-label)?="([^"]+)"/g)].map((match) => match[1]);
}

function messageKeysFromJavaScript(source) {
  return [...source.matchAll(/(?:getMessageOrFallback|getMessage)\("([^"]+)"/g)].map((match) => match[1]);
}

test("English and Traditional Chinese locale files have matching, non-empty messages", () => {
  assert.deepEqual(Object.keys(enMessages).sort(), Object.keys(zhTwMessages).sort());
  for (const [locale, messages] of Object.entries({ enMessages, zhTwMessages })) {
    for (const [key, entry] of Object.entries(messages)) {
      assert.equal(typeof entry.message, "string", `${locale}.${key} must be a string`);
      assert.notEqual(entry.message.trim(), "", `${locale}.${key} must not be empty`);
    }
  }
});

test("manifest uses localized metadata with English as the default locale", () => {
  const manifest = readJson("manifest.json");
  assert.equal(manifest.default_locale, "en");
  assert.equal(manifest.name, "__MSG_extensionName__");
  assert.equal(manifest.description, "__MSG_extensionDescription__");
  assert.equal(manifest.action.default_title, "__MSG_extensionName__");

  const manifestKeys = [...JSON.stringify(manifest).matchAll(/__MSG_(.+?)__/g)].map((match) => match[1]);
  for (const key of manifestKeys) {
    assert.ok(enMessages[key], `English locale is missing ${key}`);
    assert.ok(zhTwMessages[key], `Traditional Chinese locale is missing ${key}`);
  }
});

test("popup and background localization references exist in both locale files", () => {
  const keys = new Set([
    ...messageKeysFromHtml(fs.readFileSync(path.join(extensionDir, "popup.html"), "utf8")),
    ...messageKeysFromJavaScript(fs.readFileSync(path.join(extensionDir, "popup.js"), "utf8")),
    ...messageKeysFromJavaScript(fs.readFileSync(path.join(extensionDir, "background.js"), "utf8")),
  ]);

  for (const key of keys) {
    assert.ok(enMessages[key], `English locale is missing ${key}`);
    assert.ok(zhTwMessages[key], `Traditional Chinese locale is missing ${key}`);
  }
});

test("getMessageOrFallback returns localized text or the supplied English fallback", () => {
  assert.equal(getMessageOrFallback("key", "English", { getMessage: () => "繁體中文" }), "繁體中文");
  assert.equal(getMessageOrFallback("key", "English", { getMessage: () => "" }), "English");
  assert.equal(getMessageOrFallback("key", "English", undefined), "English");
  assert.equal(getMessageOrFallback("key", "English", { getMessage: () => { throw new Error("unavailable"); } }), "English");
});
