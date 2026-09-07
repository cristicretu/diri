# Diri website — first draft

A compact, static product page using Diri's own icons, agent marks, system type, and 5/7/12px control radii. The page and interactive workspace use the official Rosé Pine dark palette throughout. There is no appearance toggle or stored theme preference.

## Preview

From this directory:

```sh
npm run dev
```

Open http://localhost:4177. Requires Node.js; no install or build step. Set `PORT` to use another local port. `npm run check` checks JavaScript syntax.

## What's interactive

- Switch among four illustrative chats, or answer the sample Codex question.
- Open the command menu, search chats, or choose a project.
- Open compact links and notifications panels.
- Collapse the sidebar or toggle the Changes pane at desktop widths.

The workspace contains curated demonstration data, not a live Diri session. Download and source links go to the real public repository. The demo never launches an agent, runs commands, or sends data.

`index.html`, `style.css`, `app.js`, `mesh.js`, and `assets/` can be served by any static host. `server.mjs` is a loopback-only development server. There are no external fonts, analytics, runtime dependencies, or third-party scripts.

## Verification

Checked in Chrome at 1440, 768, 390, and 320px: no document overflow; command search and back navigation; chat replies; link and notification panels; and dark appearance with a previously saved light preference. Reduced-motion preferences disable animations. App controls behind an open preview panel are inert.

## Visual reference and background

The window layout follows the repository screenshot at `docs/images/diri.png`, the Rust sidebar fixture screenshots, and the sidebar implementation: agent launcher, project hierarchy, terminal output and composer, and a separate Changes pane. It is still a browser reconstruction with demonstration content, not a capture of the current live session. Screen capture was unavailable in the editing environment.

The background uses a small WebGL mesh, capped at 960 pixels wide. A slow shader drift is capped at 24 fps, with no per-frame layout reads. Drawing pauses when the document is hidden or the preview is offscreen. A footer control pauses/resumes it. Reduced motion disables the animation and pointer response. A CSS mesh remains underneath for unavailable or lost WebGL contexts. The app window uses one backdrop blur layer, visible through only the sidebars and toolbar. The terminal body (including composer and footer) is opaque. Reduced transparency uses opaque surfaces throughout. Desktop windows use a 16:10 aspect ratio, a 248px sidebar from the native app, and a 320px Changes pane.
