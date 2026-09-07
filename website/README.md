# Diri website — first draft

A compact, static product page using Diri's own icons, agent marks, system type, and 5/7/12px control radii. The warm page surrounds a dark interactive workspace; both the page and the workspace support light/dark appearances.

## Preview

From this directory:

```sh
npm run dev
```

Open http://localhost:4177. Requires Node.js; no install or build step. Set `PORT` to use another local port. `npm run check` checks JavaScript syntax.

## What's interactive

- Switch among four illustrative chats, or answer the sample Codex question.
- Open the command menu, search chats, choose a project, and navigate Settings → Color theme.
- Preview themes with arrow keys or the pointer. Enter commits; Escape restores the previous appearance.
- Open compact links and notifications panels.
- Switch the website appearance in the footer.

The workspace contains curated demonstration data, not a live Diri session. Download and source links go to the real public repository. The demo never launches an agent, runs commands, or sends data. Only the website appearance choice is stored locally.

`index.html`, `style.css`, `app.js`, and `assets/` can be served by any static host. `server.mjs` is a loopback-only development server. There are no external fonts, analytics, runtime dependencies, or third-party scripts.

## Verification

Checked in Chrome at 1440, 768, 390, and 320px: no document overflow; command search and back navigation; theme preview/cancel; chat replies; link panel; and website appearance. Reduced-motion preferences disable animations. App controls behind an open preview panel are inert.
