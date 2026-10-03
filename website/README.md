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

`index.html`, `style.css`, `app.js`, `agent-previews.js`, `downloads.js`, and `assets/` can be served by any static host. `server.mjs` is a loopback-only development server. There are no external fonts, analytics, runtime dependencies, or third-party scripts.

## Generated pages

`npm run docs` (`scripts/site.mjs`) regenerates every data-driven page. Output is committed, so `npm run dev` still needs no build step, and `npm run verify` fails when anything is stale.

| Section | Source | Builder |
| --- | --- | --- |
| `/docs/` | `docs-src/*.md`, sidebar order in `NAV` | `scripts/docs.mjs` |
| `/agents/` | The Engine's agent manifests in `diri/crates/diri-engine/manifests` | `scripts/pages.mjs` |
| `/compare/` | `compare-src/*.md` (competitor facts must cite a source and a checked date) | `scripts/pages.mjs` |
| `/whats-new/` and `/whats-new/<version>/` | `whats-new-src/releases.json` | `scripts/pages.mjs` |

Each run also rewrites `llms.txt`, `llms-full.txt`, the generated sitemap entries, the Markdown twin of each docs, agent and comparison page, and `docs/search.json`. Guides stay hand-written HTML; only their share image is set by the generator. Adding a manifest adds an agent page. Adding a release to `releases.json` adds a release page and updates What's new.

Pages use a small Markdown subset: `##`–`####` headings (ids are generated), paragraphs, single-level lists, GFM tables, fenced code with an optional title after the language (```` ```toml ~/.codex/config.toml ````), `> [!NOTE]` / `[!TIP]` / `[!WARNING]` callouts, images on their own line, and inline `<kbd>`. Frontmatter needs `title` and an 80–180 character `description`; `nav` and `lead` are optional.

### Share cards

Every generated page and guide has its own 1200×630 card in `og/`. `npm run og` draws the ones whose text changed with headless Chrome and saves JPEGs with `sips`, so it runs on a Mac (set `CHROME` to use another Chrome). `og/rendered.json` records what each card was drawn from; `npm run verify` names any card that needs redrawing. Change the design in `scripts/og.mjs` and bump `CARD_DESIGN` in `scripts/site.mjs` to redraw them all.

### MCP reference and agent access

The [MCP tool reference](https://diri.sh/docs/mcp-tools/) is generated from `docs-src/mcp-tools.json`, which a Rust test (`diri/crates/dirijor-mcp/tests/docs_catalog.rs`) keeps identical to the server's real tool catalog. After changing a tool, run `DIRI_UPDATE_DOCS=1 cargo test -p dirijor-mcp --test docs_catalog` from `diri/`, then `npm run docs`.

Agents can read the site three ways: the Markdown twin of each page, `llms.txt` / `llms-full.txt`, and the read-only docs MCP server at `/mcp` (`functions/mcp.js`, see [CLOUDFLARE.md](CLOUDFLARE.md)). ⌘K search and the MCP server share one ranking function, `docs-search.js`.

## Release downloads

On each page load, `downloads.js` checks GitHub's public [latest release API](https://docs.github.com/en/rest/releases/releases#get-the-latest-release). The primary button links directly to that release's universal macOS DMG, with its version and release notes beside it. Other downloads lists the macOS ZIP and the Linux x86_64 and arm64 AppImage/DEB only when those assets exist in the same release. Linux is not included in every release; the full release history and Linux installation guide are always available.

Publishing a new stable GitHub release updates downloads on subsequent page loads without a website rebuild, deploy hook, or token. The browser uses GitHub's normal HTTP caching. Only completed, nonempty desktop assets with matching repository/tag/filename URLs are accepted. Drafts and prereleases are excluded. Asset names follow the current release scripts; update the format list if packaging names change.

If the API is unavailable, rate-limited, returns invalid data, or takes more than five seconds, the original GitHub release link stays usable. It also works without JavaScript. There is no cached version in local storage that can outlive a removed release. The build CSP allows connections to `https://api.github.com`; downloads navigate directly to GitHub. `npm test` covers asset selection, new versions, missing packages, malformed data, errors, and timeout behavior; it runs as part of `npm run verify` and website CI without network access.

Browser checks against the production build and CSP covered 1440, 768, 390, and 320px widths, keyboard access to other downloads, a download click using the live v0.6.2 metadata and a stubbed binary, no JavaScript, API failure/rate limiting, and a future release with Linux packages. The live v0.6.2 DMG URL was also checked through its redirect to an HTTP 200 response.

## Verification

Checked in Chrome at 1440, 768, 390, and 320px: no document overflow; command search and back navigation; chat replies; link and notification panels; and dark appearance with a previously saved light preference. Reduced-motion preferences disable animations. App controls behind an open preview panel are inert.

## Visual reference and background

The window layout follows a headless render of the current app (`render_workspace_workbench_screenshot` in `diri-app`): status glyph on the left of each row, agent mark on the right, agents started by another agent nested under it, a Filter agents row and account footer, and pane-style title actions. It is a browser reconstruction with demonstration content, not a capture of a live session. The default chat shows a Claude session starting three agents, which appear nested in the sidebar.

The page background is plain. The only backdrop is the painted desktop behind the window (CSS gradients plus a static SVG grain), so the window's glass has something to blur. Tints follow the app's measured glass density: window fill about .56, sidebar clear over it, terminal about .65 on top. Reduced transparency uses opaque surfaces. There is no WebGL or animation behind the page.

Agent presentation references: [Claude Code](https://github.com/anthropics/claude-code), [Cursor CLI](https://cursor.com/cli), the local Codex CLI and Diri screenshot, and [Gemini CLI](https://github.com/google-gemini/gemini-cli). The Gemini ASCII mark is adapted from `packages/cli/src/ui/components/AsciiArt.ts`, copyright 2025 Google LLC, licensed under Apache-2.0 (see the repository LICENSE). The compact account footer uses the supplied name and illustrative daily cost; it does not fetch account data.

## Feature coverage

The page has one primary download action with other packages in an expandable list, a GitHub mark in the masthead, and 24 feature descriptions grouped by agents/sessions, parallel work, review, usage/accounts, workspace preferences, and devices. The complete inventory is static HTML, available without JavaScript. It uses shared columns and subtle separators instead of individual cards or hidden accordions.

Claims were checked against the current repository: root README and `docs/GETTING_STARTED.md`; the 22 Engine manifests; app history, delegation, commands, code viewer, account settings, usage/limits, skills, and resource controls; and `ios/README.md`. Phone and Linux support remain labeled beta. Cost figures are estimates, history names Claude/Codex, and forks are qualified by provider support. No roadmap-only functionality is advertised.

Checked this revision at 1440, 768, 390, and 320px for feature/text overflow, header alignment, GitHub visibility, and a single download action.

The terminal, Changes pane, and command list in the mockup clip overflow without becoming scroll containers. Wheel and touch scrolling over the preview scroll the page; agent switching and command selection remain interactive.
