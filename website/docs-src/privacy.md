---
title: Privacy and network connections
nav: Privacy
description: What diri sends over the network, when, and how to turn each connection off. Diagnostics sharing is opt-in for new installs; update checks carry no identifiers.
lead: Every network connection diri makes, what it sends, when, and how to turn it off. The same list is in **Settings › Privacy**.
---
diri has no account system, no advertising and no hosted relay. Your agents talk to their own services with their own sign-ins; diri does not proxy that traffic.

## Diagnostics are opt-in
On a new install diri asks once, in a small notice at the bottom of the window, whether to share anonymous diagnostics. Nothing is uploaded unless you choose **Share**. Ignoring the notice or choosing **Not now** keeps sharing off, and diri does not ask again. Change your answer any time in **Settings › Privacy › Share diagnostics**.

Installs from before this change keep the setting they already had.

Every diri process still keeps a local diagnostics log in `~/Library/Application Support/Dirijor/telemetry/spool`, capped at 64 MiB, so a bug you report can be investigated. It stays on your machine unless you share it. Crash reports follow the same rule. When you opt in, routine uploads include only what was recorded after you said yes. [Security model](/docs/security/#diagnostics-and-privacy) lists exactly what is and is not recorded.

## Every connection

{{connections}}

## Not fetched
diri does not download fonts, images, avatars or web assets. What's New clips, agent definitions and model prices ship inside the app. Links you click open in your browser; diri never requests them itself.

The full policy is in [PRIVACY.md](https://github.com/cristicretu/diri/blob/main/PRIVACY.md).
