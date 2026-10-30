// @ts-check
import cloudflare from '@astrojs/cloudflare';
import { defineConfig } from 'astro/config';

export default defineConfig({
  site: 'https://chinstrap.korora.tech',
  // Static output: the adapter writes the Build Output cf deploys, and the
  // Worker serves only static assets (see cloudflare.config.ts).
  output: 'static',
  // A static site needs no KV sessions or Images binding, and prerenders in
  // Node like a plain Astro build.
  adapter: cloudflare({ imageService: 'compile', prerenderEnvironment: 'node' }),
  session: false,
  trailingSlash: 'never',
  build: { format: 'file' },
  vite: {
    // The docs page renders ../README.md, which sits outside this project.
    server: { fs: { allow: ['..'] } },
  },
});
