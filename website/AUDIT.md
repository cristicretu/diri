# Production readiness check

Audited with Lighthouse 13.4.1 against the local Cloudflare Pages emulator, using its default mobile profile. Recorded at 2026-09-07T08:05:43.967Z.

| Category | Score |
| --- | --- |
| Performance | 100 |
| Accessibility | 100 |
| Best Practices | 100 |
| SEO | 100 |

- Largest Contentful Paint: 1.2 s.
- Total Blocking Time: 0 ms.
- Cumulative Layout Shift: 0.

These are local lab measurements, not live-domain field data or a search-ranking guarantee.

`npm run verify` passes. Browser checks cover Cloudflare CSP, local assets and MIME types, fingerprinted asset caching, canonical and social metadata, JSON-LD, static content with JavaScript disabled, the index redirect, real 404 responses, exclusion of source files, and the interactive demo. Desktop and 390/320px layouts were checked with the text-only download button and platform marks.

Shader tests cover compilation, animation cap, pause/resume, reduced motion, offscreen suspension, and the no-WebGL fallback. The synchronous shader startup delay observed in the first audit was removed using asynchronous link-completion checks where supported.

Live-domain checks remain in [CLOUDFLARE.md](CLOUDFLARE.md): domain association and TLS, www/Pages-host redirects, crawler response headers, Search Console verification, sitemap submission, and a post-launch PageSpeed run.
