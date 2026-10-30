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

Pushes to `main` that touch `website/` or `README.md` are built and deployed by `.github/workflows/website.yml`. It runs `cf deploy`. The deploy job runs in the `production` environment and logs in to Infisical over OIDC as the Korora Tech CI identity, which reads the Korora Tech Cloudflare token (`CLOUDFLARE_API_TOKEN`). The identity, this repository's `production` environment and its `INFISICAL_MACHINE_IDENTITY_ID` secret are managed in [korora-tech/korora-tech](https://github.com/korora-tech/korora-tech).

`cloudflare.config.ts` names the account and binds the Worker to `chinstrap.korora.tech` as a custom domain; the first deploy creates the DNS record and certificate, which requires `korora.tech` to be a zone on the same Cloudflare account.

Manual deploy: `npx cf auth login && npm run deploy`.
