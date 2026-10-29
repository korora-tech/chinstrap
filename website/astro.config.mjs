// @ts-check
import { defineConfig } from 'astro/config';

export default defineConfig({
  site: 'https://chinstrap.korora.tech',
  // Static output: Cloudflare serves dist/ as Workers static assets (see wrangler.jsonc).
  output: 'static',
  trailingSlash: 'never',
  build: { format: 'file' },
  vite: {
    // The docs page renders ../README.md, which sits outside this project.
    server: { fs: { allow: ['..'] } },
  },
});
