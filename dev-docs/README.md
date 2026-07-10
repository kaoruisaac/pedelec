# Pedelec documentation site

This is a standalone Astro Starlight project for the Pedelec documentation site.

```sh
cd dev-docs
npm ci
npm run dev
```

Use `npm run build` to generate the static site in `dist/`, then `npm run preview` to inspect that production build locally. No global Astro, Starlight, or SolidJS installation is required.

English content lives in `src/content/docs/`; Traditional Chinese content lives in `src/content/docs/zh-tw/`. Add matching pages in both locations to preserve locale-to-locale navigation.
