# chinstrap.korora.tech

The Chinstrap website: an [Astro](https://astro.build) static site served by Cloudflare Workers static assets on `chinstrap.korora.tech`.

- `src/pages/index.astro` is the landing page.
- `src/pages/docs.astro` renders the repository's `README.md` at build time, so the docs never drift from it. Edit the README, not this page.

## Develop

Needs Node.js 22.23.4 or newer (`.node-version` pins 24).

```sh
npm ci
npm run dev       # http://localhost:4321
npm run preview   # serve the site locally with cf dev (same headers and 404 handling as production)
```

## Deploy

Pushes to `main` that touch `website/` or `README.md` are built and deployed by `.github/workflows/website.yml`. It runs `cf deploy` with the Korora Tech organization's Cloudflare token (`CLOUDLFLARE_API_TOKEN`).

`cloudflare.config.ts` names the account and binds the Worker to `chinstrap.korora.tech` as a custom domain; the first deploy creates the DNS record and certificate, which requires `korora.tech` to be a zone on the same Cloudflare account.

Manual deploy: `npx cf auth login && npm run deploy`.
