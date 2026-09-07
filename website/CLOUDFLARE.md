# Deploy Diri to Cloudflare Pages

The production domain is **https://diri.sh/**. Connect this repository to Cloudflare Pages once to enable automatic deployments. The GitHub website workflow validates changes; Cloudflare's Git integration publishes them.

## Connect GitHub once

In Cloudflare, open **Workers & Pages → Create application → Pages → Connect to Git**, authorize `cristicretu/diri`, and use the settings below. Merge the website PR into `main` before the first production build.

After connecting, pushes to `main` that change the website trigger production deployments. Enable preview deployments for other branches to get review URLs on website PRs. In **Settings → Builds → Build watch paths**, set the include path to `website/**` so app-only changes do not rebuild the landing page.

See Cloudflare's [GitHub integration](https://developers.cloudflare.com/pages/configuration/git-integration/github-integration/) and [build watch paths](https://developers.cloudflare.com/pages/configuration/build-watch-paths/).

## Build settings

| Setting | Value |
| --- | --- |
| Framework preset | None |
| Root directory | `website` |
| Build command | `npm run verify` |
| Build output directory | `dist` |
| Environment variable | `NODE_VERSION` = `22` |
| Production branch | `main`, after the website branch is merged |
| Build watch path | `website/**` |

There are no install-time or runtime dependencies. The build copies only public files, fingerprints CSS/JavaScript, and generates Cloudflare `_headers`. Keep Cloudflare's default HTML caching; immutable caching applies only to fingerprinted files.

For Direct Upload, run `npm --prefix website run build` from the repository root and upload the **contents** of `website/dist`. The directory is self-contained. Do not upload the whole repository or `website/` source directory.

To reproduce Cloudflare's routing locally, run from `website/`:

```sh
npx wrangler pages dev dist --compatibility-date=2026-09-03
```

## Domain setup

1. Add the existing `diri.sh` zone to the same Cloudflare account as Pages. Apex domains require Cloudflare nameservers. Preserve existing DNS records when moving a zone.
2. In the Pages project, open **Custom domains → Set up a domain** and add `diri.sh`. Complete the provided DNS steps and wait for the certificate to become active. Adding a CNAME alone does not associate the domain with Pages.
3. Add `www.diri.sh` and configure a permanent redirect to `https://diri.sh`, preserving paths and query strings. Enable **Always Use HTTPS** for the zone.
4. After `diri.sh` works, use Cloudflare **Bulk Redirects** to redirect the project's production `pages.dev` hostname to `https://diri.sh`. Preserve paths and queries. Leave branch/hash preview hosts available for review.

Domain redirects must be configured in Cloudflare; Pages `_redirects` does not support domain-level rules. The shipped `_headers` already sets `X-Robots-Tag: noindex` on production and preview `pages.dev` hosts. The custom domain stays indexable. `/index.html` redirects to `/`; unknown paths return a real 404 rather than a duplicate home page.

See Cloudflare's [custom domain setup](https://developers.cloudflare.com/pages/configuration/custom-domains/), [Pages-domain redirects](https://developers.cloudflare.com/pages/how-to/redirect-to-custom-domain/), and [www redirects](https://developers.cloudflare.com/pages/how-to/www-redirect/).

## Search launch checks

- Confirm `https://diri.sh/` returns 200 and has no `noindex` response header. Check HTTP, www, and the production Pages hostname redirect to the canonical host.
- Verify `https://diri.sh/robots.txt`, `/sitemap.xml`, `/favicon-96.png`, and `/assets/social-card.jpg` return the correct content types and are public.
- Add `diri.sh` as a Domain property in [Google Search Console](https://search.google.com/search-console). Add the TXT verification value that Google supplies, then submit `https://diri.sh/sitemap.xml` and inspect the home page. No verification token is guessed or embedded in this repository.
- Validate the deployed structured data with [Schema Markup Validator](https://validator.schema.org/). The app has truthful free-software metadata; no review scores are fabricated. Google requires a qualifying review/rating for software-app rich results, so those results are not claimed.
- Check the deployed URL in [PageSpeed Insights](https://pagespeed.web.dev/) after the domain is live. Local Lighthouse scores do not establish real-user Core Web Vitals or guarantee rankings.

SEO references: Google's [canonical URL guidance](https://developers.google.com/search/docs/crawling-indexing/consolidate-duplicate-urls), [site-name metadata](https://developers.google.com/search/docs/appearance/site-names), and [software-app structured data](https://developers.google.com/search/docs/appearance/structured-data/software-app).
