import { defineConfig } from "cf/config";

export default defineConfig({
	accountId: "0aeb879de8e3cdde5fb3d413025222ce",
	worker: {
		name: "chinstrap-website",
		compatibilityDate: "2026-10-01",
		// A static Astro site: cf builds it and serves only the assets.
		assets: { htmlHandling: "drop-trailing-slash", notFoundHandling: "404-page" },
		// Cloudflare creates the DNS record and certificate on first deploy;
		// the korora.tech zone is in the same account.
		domains: ["chinstrap.korora.tech"],
		workersDev: false,
		previewUrls: false,
	},
});
