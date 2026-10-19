# Diri website

A compact, static product page using Diri's own icons, agent marks, system type, and 5/7/12px control radii. The page and interactive workspace use the official Rosé Pine dark palette throughout. There is no appearance toggle or stored theme preference.

## Preview

From this directory:

```sh
npm run dev
```

Open http://localhost:4177. Requires Node.js; no install or build step. Set `PORT` to use another local port. `npm run check` checks JavaScript syntax. `npm run verify` builds the Cloudflare output and validates the SEO and public file set.

## The hero

The window under the headline is a recording of the real app, not a mock: `assets/features/agents.webp`, made by `diri/scripts/site-clips.sh` from the ignored `render_site_clips` test in `diri-app`. It is captured with the glass material, so the sidebar keeps its alpha and the page's gradient shows through it, blurred by `backdrop-filter` behind the image. The feature clips below it come from the same recorder.

## Feature coverage

The page has one primary download action with other packages in an expandable list, a GitHub mark in the masthead, and 24 feature descriptions grouped by agents/sessions, parallel work, review, usage/accounts, workspace preferences, and devices. The complete inventory is static HTML, available without JavaScript. It uses shared columns and subtle separators instead of individual cards or hidden accordions.

Claims were checked against the current repository: root README and `docs/GETTING_STARTED.md`; the 22 Engine manifests; app history, delegation, commands, code viewer, account settings, usage/limits, skills, and resource controls; and `ios/README.md`. Phone and Linux support remain labeled beta. Cost figures are estimates, history names Claude/Codex, and forks are qualified by provider support. No roadmap-only functionality is advertised.

Checked this revision at 1440, 768, 390, and 320px for feature/text overflow, header alignment, GitHub visibility, and a single download action.

The terminal, Changes pane, and command list in the mockup clip overflow without becoming scroll containers. Wheel and touch scrolling over the preview scroll the page; agent switching and command selection remain interactive.
