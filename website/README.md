# Diri website

A compact, static product page using Diri's own icons, agent marks, system type, and 5/7/12px control radii. The page and interactive workspace use the official Rosé Pine dark palette throughout. There is no appearance toggle or stored theme preference.

## Preview

From this directory:

```sh
npm run dev
```

Open http://localhost:4177. Requires Node.js; no install or build step. Set `PORT` to use another local port. `npm run check` checks JavaScript syntax. `npm run verify` builds the Cloudflare output and validates the SEO and public file set.

## What's interactive

- Switch among four illustrative chats using the sidebar or agent buttons below the window. Each agent has its own terminal header, tool output, composer, and status line.
- Answer the sample Claude Code question.
- Open the command menu, search chats, or choose a project.
- Open compact links and notifications panels.
- Collapse the sidebar or toggle the Changes pane at desktop widths.

The workspace contains curated demonstration data, not a live Diri session. Download and source links go to the real public repository. The demo never launches an agent, runs commands, or sends data.

`index.html`, `style.css`, `app.js`, `agent-previews.js`, `mesh.js`, `downloads.js`, and `assets/` can be served by any static host. `server.mjs` is a loopback-only development server. There are no external fonts, analytics, runtime dependencies, or third-party scripts.

## Release downloads

On each page load, `downloads.js` checks GitHub's public [latest release API](https://docs.github.com/en/rest/releases/releases#get-the-latest-release). The primary button links directly to that release's universal macOS DMG, with its version and release notes beside it. Other downloads lists the macOS ZIP and Linux x86_64 AppImage/DEB only when those assets exist in the same release. Linux is not included in every release; the full release history and Linux installation guide are always available.

Publishing a new stable GitHub release updates downloads on subsequent page loads without a website rebuild, deploy hook, or token. The browser uses GitHub's normal HTTP caching. Only completed, nonempty desktop assets with matching repository/tag/filename URLs are accepted. Drafts and prereleases are excluded. Asset names follow the current release scripts; update the format list if packaging names change.

If the API is unavailable, rate-limited, returns invalid data, or takes more than five seconds, the original GitHub release link stays usable. It also works without JavaScript. There is no cached version in local storage that can outlive a removed release. The build CSP allows connections to `https://api.github.com`; downloads navigate directly to GitHub. `npm test` covers asset selection, new versions, missing packages, malformed data, errors, and timeout behavior; it runs as part of `npm run verify` and website CI without network access.

Browser checks against the production build and CSP covered 1440, 768, 390, and 320px widths, keyboard access to other downloads, a download click using the live v0.6.2 metadata and a stubbed binary, no JavaScript, API failure/rate limiting, and a future release with Linux packages. The live v0.6.2 DMG URL was also checked through its redirect to an HTTP 200 response.

## Verification

Checked in Chrome at 1440, 768, 390, and 320px: no document overflow; command search and back navigation; chat replies; link and notification panels; and dark appearance with a previously saved light preference. Reduced-motion preferences disable animations. App controls behind an open preview panel are inert.

## Visual reference and background

The window layout follows the repository screenshot at `docs/images/diri.png`, the Rust sidebar fixture screenshots, and the sidebar implementation: agent launcher, project hierarchy, terminal output and composer, and a separate Changes pane. It is still a browser reconstruction with demonstration content, not a capture of the current live session. Screen capture was unavailable in the editing environment.

The background uses a small WebGL mesh, capped at 960 pixels wide. A slow shader drift is capped at 24 fps, with no per-frame layout reads. Drawing pauses when the document is hidden or the preview is offscreen. A footer control pauses/resumes it. Reduced motion disables the animation and pointer response. A CSS mesh remains underneath for unavailable or lost WebGL contexts. The app window uses one backdrop blur layer, visible through only the sidebars and toolbar. The terminal body (including composer and footer) is opaque. Reduced transparency uses opaque surfaces throughout. Desktop windows use a 16:10 aspect ratio, a 248px sidebar from the native app, and a 320px Changes pane.

Agent presentation references: [Claude Code](https://github.com/anthropics/claude-code), [Cursor CLI](https://cursor.com/cli), the local Codex CLI and Diri screenshot, and [Gemini CLI](https://github.com/google-gemini/gemini-cli). The Gemini ASCII mark is adapted from `packages/cli/src/ui/components/AsciiArt.ts`, copyright 2025 Google LLC, licensed under Apache-2.0 (see the repository LICENSE). The compact account footer uses the supplied name and illustrative daily cost; it does not fetch account data.

## Feature coverage

The page has one primary download action with other packages in an expandable list, a GitHub mark in the masthead, and 24 feature descriptions grouped by agents/sessions, parallel work, review, usage/accounts, workspace preferences, and devices. The complete inventory is static HTML, available without JavaScript. It uses shared columns and subtle separators instead of individual cards or hidden accordions.

Claims were checked against the current repository: root README and `docs/GETTING_STARTED.md`; the 22 Engine manifests; app history, delegation, commands, code viewer, account settings, usage/limits, skills, and resource controls; and `ios/README.md`. Phone and Linux support remain labeled beta. Cost figures are estimates, history names Claude/Codex, and forks are qualified by provider support. No roadmap-only functionality is advertised.

Checked this revision at 1440, 768, 390, and 320px for feature/text overflow, header alignment, GitHub visibility, and a single download action. The mesh height is bounded to the product area so adding feature content does not enlarge the shader drawing buffer or move the gradient away from the app.

The terminal, Changes pane, and command list in the mockup clip overflow without becoming scroll containers. Wheel and touch scrolling over the preview scroll the page; agent switching and command selection remain interactive.

## Mesh experiment

The default mesh adds slowly warped color folds, stationary fine grain, and a diffuse glow aligned with the mockup’s lower edge. Compare the previous shader at `http://localhost:4177/?mesh=original`; the page layout is identical. Window geometry is read only during resize, and all effects share the existing shader pass and drawing buffer.

Verified both shaders compile without WebGL errors; the experimental version drew 20 frames in a one-second local Chrome sample under its 24 fps cap. Pause/resume, reduced motion, offscreen suspension, CSS fallback, and mobile widths were checked. Mockup scrolling passes to the page, mouse focus restoration has no outline, and keyboard restoration uses a 1px inset indicator.

Preview controls share translucent hover, selected, selected-hover, and pressed fills with short color transitions. Hover styling is limited to fine pointers. Tabs, provider buttons, and rows retain distinct selection; Links and Notifications expose their open state and clear it on dismissal. Keyboard focus uses an inset indicator, while pointer restoration does not draw a ring. Verified mouse press/hover, selected hover, touch dismissal, keyboard palette navigation, and toolbar state cleanup.

## Production and SEO

The canonical site is `https://diri.sh/`. Cloudflare Pages configuration, domain redirects, and the remaining DNS/Search Console steps are in [CLOUDFLARE.md](CLOUDFLARE.md). `npm run build` produces only deployable files in `dist/`, fingerprints CSS/JavaScript for immutable browser caching, and generates CSP and preview-host noindex headers. Runtime code has no added dependencies.

The page includes a descriptive title, description, canonical URL, Open Graph and Twitter large-image metadata, WebSite/WebPage/SoftwareApplication JSON-LD, a single-URL sitemap, robots.txt, branded icons, and an 80 KB social card. The interactive demonstration is excluded from Google snippets with `data-nosnippet`; real feature copy remains static and crawlable. The custom 404 prevents Pages from treating arbitrary URLs as copies of the home page.

The Apple and Linux marks are from [Simple Icons](https://github.com/simple-icons/simple-icons), distributed under CC0; brand rights remain with their owners. The button is text-only, with platform icons alongside it. Shader linking uses `KHR_parallel_shader_compile` when available to avoid blocking the page while the program compiles; the CSS background remains available throughout startup.

The footer uses a two-row layout: Diri branding and Documentation/GitHub links, followed by the license and a labeled animation control. It introduces no extra download action. Desktop and 390/320px layouts, motion toggling, and reduced-motion visibility were verified.
