# Diri website — first draft

A compact, static product page using Diri's own icons, agent marks, system type, and 5/7/12px control radii. The page and interactive workspace use the official Rosé Pine dark palette throughout. There is no appearance toggle or stored theme preference.

## Preview

From this directory:

```sh
npm run dev
```

Open http://localhost:4177. Requires Node.js; no install or build step. Set `PORT` to use another local port. `npm run check` checks JavaScript syntax.

## What's interactive

- Switch among four illustrative chats using the sidebar or agent buttons below the window. Each agent has its own terminal header, tool output, composer, and status line.
- Answer the sample Claude Code question.
- Open the command menu, search chats, or choose a project.
- Open compact links and notifications panels.
- Collapse the sidebar or toggle the Changes pane at desktop widths.

The workspace contains curated demonstration data, not a live Diri session. Download and source links go to the real public repository. The demo never launches an agent, runs commands, or sends data.

`index.html`, `style.css`, `app.js`, `agent-previews.js`, `mesh.js`, and `assets/` can be served by any static host. `server.mjs` is a loopback-only development server. There are no external fonts, analytics, runtime dependencies, or third-party scripts.

## Verification

Checked in Chrome at 1440, 768, 390, and 320px: no document overflow; command search and back navigation; chat replies; link and notification panels; and dark appearance with a previously saved light preference. Reduced-motion preferences disable animations. App controls behind an open preview panel are inert.

## Visual reference and background

The window layout follows the repository screenshot at `docs/images/diri.png`, the Rust sidebar fixture screenshots, and the sidebar implementation: agent launcher, project hierarchy, terminal output and composer, and a separate Changes pane. It is still a browser reconstruction with demonstration content, not a capture of the current live session. Screen capture was unavailable in the editing environment.

The background uses a small WebGL mesh, capped at 960 pixels wide. A slow shader drift is capped at 24 fps, with no per-frame layout reads. Drawing pauses when the document is hidden or the preview is offscreen. A footer control pauses/resumes it. Reduced motion disables the animation and pointer response. A CSS mesh remains underneath for unavailable or lost WebGL contexts. The app window uses one backdrop blur layer, visible through only the sidebars and toolbar. The terminal body (including composer and footer) is opaque. Reduced transparency uses opaque surfaces throughout. Desktop windows use a 16:10 aspect ratio, a 248px sidebar from the native app, and a 320px Changes pane.

Agent presentation references: [Claude Code](https://github.com/anthropics/claude-code), [Cursor CLI](https://cursor.com/cli), the local Codex CLI and Diri screenshot, and [Gemini CLI](https://github.com/google-gemini/gemini-cli). The Gemini ASCII mark is adapted from `packages/cli/src/ui/components/AsciiArt.ts`, copyright 2025 Google LLC, licensed under Apache-2.0 (see the repository LICENSE). The compact account footer uses the supplied name and illustrative daily cost; it does not fetch account data.

## Feature coverage

The page has one download action, a GitHub mark in the masthead, and 24 feature descriptions grouped by agents/sessions, parallel work, review, usage/accounts, workspace preferences, and devices. The complete inventory is static HTML, available without JavaScript. It uses shared columns and subtle separators instead of individual cards or hidden accordions.

Claims were checked against the current repository: root README and `docs/GETTING_STARTED.md`; the 22 Engine manifests; app history, delegation, commands, code viewer, account settings, usage/limits, skills, and resource controls; and `ios/README.md`. Phone and Linux support remain labeled beta. Cost figures are estimates, history names Claude/Codex, and forks are qualified by provider support. No roadmap-only functionality is advertised.

Checked this revision at 1440, 768, 390, and 320px for feature/text overflow, header alignment, GitHub visibility, and a single download action. The mesh height is bounded to the product area so adding feature content does not enlarge the shader drawing buffer or move the gradient away from the app.

The terminal, Changes pane, and command list in the mockup clip overflow without becoming scroll containers. Wheel and touch scrolling over the preview scroll the page; agent switching and command selection remain interactive.

## Mesh experiment

The default mesh adds slowly warped color folds, stationary fine grain, and a diffuse glow aligned with the mockup’s lower edge. Compare the previous shader at `http://localhost:4177/?mesh=original`; the page layout is identical. Window geometry is read only during resize, and all effects share the existing shader pass and drawing buffer.

Verified both shaders compile without WebGL errors; the experimental version drew 20 frames in a one-second local Chrome sample under its 24 fps cap. Pause/resume, reduced motion, offscreen suspension, CSS fallback, and mobile widths were checked. Mockup scrolling passes to the page, mouse focus restoration has no outline, and keyboard restoration uses a 1px inset indicator.

Preview controls share translucent hover, selected, selected-hover, and pressed fills with short color transitions. Hover styling is limited to fine pointers. Tabs, provider buttons, and rows retain distinct selection; Links and Notifications expose their open state and clear it on dismissal. Keyboard focus uses an inset indicator, while pointer restoration does not draw a ring. Verified mouse press/hover, selected hover, touch dismissal, keyboard palette navigation, and toolbar state cleanup.
