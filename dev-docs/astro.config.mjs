import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import solidJs from '@astrojs/solid-js';

export default defineConfig({
  site: 'https://kaoruisaac.github.io',
  base: '/pedelec',
  integrations: [
    starlight({
      title: 'Pedelec',
      social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/kaoruisaac/pedelec' }],
      locales: {
        root: {
          label: 'English',
          lang: 'en',
        },
        'zh-tw': {
          label: '繁體中文',
          lang: 'zh-TW',
        },
      },
    }),
    solidJs(),
  ],
});
