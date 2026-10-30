# Privacy and network connections

> What diri sends over the network, when, and how to turn each connection off. Diagnostics sharing is opt-in for new installs; update checks carry no identifiers.

diri has no account system, no advertising and no hosted relay. Your agents talk to their own services with their own sign-ins; diri does not proxy that traffic.

## Diagnostics are opt-in
On a new install diri asks once, in a small notice at the bottom of the window, whether to share anonymous diagnostics. Nothing is uploaded unless you choose **Share**. Ignoring the notice or choosing **Not now** keeps sharing off, and diri does not ask again. Change your answer any time in **Settings › Privacy › Share diagnostics**.

Installs from before this change keep the setting they already had.

Every diri process still keeps a local diagnostics log in `~/Library/Application Support/Dirijor/telemetry/spool`, capped at 64 MiB, so a bug you report can be investigated. It stays on your machine unless you share it. Crash reports follow the same rule. When you opt in, routine uploads include only what was recorded after you said yes. [Security model](/docs/security/#diagnostics-and-privacy) lists exactly what is and is not recorded.

## Every connection

| Connection | Goes to | What is sent | When | Optional | How to turn it off |
| --- | --- | --- | --- | --- | --- |
| **Diagnostics** | telemetry.diri.sh (Cloudflare Worker run by the project) | Recorded diagnostics (crashes, hangs, errors, timings, counts), a random install id, your Support ID, app/OS/CPU versions and the name you set. Never terminal contents, prompts, files or environment variables. | About once an hour, and within a minute after a crash, by the Engine. | Opt-in. New installs send nothing until you choose Share. Existing installs keep the setting they already had. | Settings › Privacy › Share diagnostics. `DIRI_TELEMETRY=off` also stops recording. |
| **Send now and Report a Problem** | telemetry.diri.sh | The same diagnostics as above, once. | Only when you click Send now or Help › Report a Problem. | Yes. The click is the consent, even with sharing off. | Don't click them. |
| **Update checks** | GitHub Releases (appcast.json and the release ZIP), with updates.diri.sh as a mirror when GitHub is slow | Plain downloads. No install id, cookies or account; only a fixed `diri-updater` User-Agent and your IP address. | 20 seconds after launch, then every 6 hours, and when you click Check for Updates. | Yes. | Turn off automatic updates in Settings. `DIRI_UPDATE_MIRROR=off` uses GitHub only. |
| **Release notes** | api.github.com (the latest diri release) | A plain request, no identifiers. The What's New clips ship inside the app and are never downloaded. | When you open Settings › What's New. | Yes. | Don't open that tab. |
| **Claude and Codex usage limits** | api.anthropic.com and chatgpt.com | The agent's own sign-in token, read from Claude Code's keychain item or Codex's auth file, to read your remaining quota and account profile. Tokens are never refreshed or sent anywhere else. | When you open the account menu or Accounts settings, on account switching, and every 3 minutes if the sidebar shows remaining quota. | Yes. | Keep the sidebar indicator on its default and don't open the account menu. |
| **Cursor usage** | cursor.com and api2.cursor.sh | Cursor's own session token and machine id, to read your Cursor usage events. Refreshes the Cursor token when it has expired. | At launch and every minute, only if Cursor is signed in on this machine. | Only runs with a Cursor sign-in. | Sign out of Cursor. There is no separate switch yet. |
| **Pull request status** | GitHub, through your own `gh` CLI | Pull request links that appeared in your sessions, with your `gh` sign-in. | Every minute while diri is in front, every 5 minutes or less often in the background, and when you open Worktrees. | Only runs when `gh` is installed. | Uninstall or sign out of `gh`. |
| **Your SSH hosts** | The hosts you add, through your own `ssh` and `scp` | Session input and output, the diri-remote helper binary (copied from the app, never downloaded), pasted images, and usage queries. | Only for remote sessions and hosts you configure. | Yes. | Remove the host. |
| **Git remotes** | Your repository's `origin`, through your own `git` | Your git credentials and the session branch. | A fetch before starting an agent in a new worktree from the default branch, and push/fetch when you move a session to another machine. | Yes. Offline it uses the last fetched branch. | Start agents without a new worktree, and don't move sessions. |
| **Agent installers** | The agent vendor's installer (claude.ai, chatgpt.com, npm, opencode.ai and others) | Whatever the vendor's installer sends. diri types the command into a terminal you can see. | Only when you click Install for an agent and confirm. | Yes. | Install agents yourself. |
| **Computer use driver** | github.com (a pinned cua-driver release, checked by SHA-256 and signing team) | A plain download. | Only when you click Set up in Settings › Agents › Computer use. | Yes. | Don't set it up. |
| **Kokoro voice** | huggingface.co, github.com and raw.githubusercontent.com | Plain downloads of the voice model, checked by SHA-256. Speech recognition and system voices stay on device. | When you pick a Kokoro voice and its files are missing. | Yes. | Use a system voice (the default). |
| **API tab** | Any URL you or an agent enter | The request as written. | When you click Send. An agent may send a GET it opened for you. | Yes. | Don't use the API tab. |
| **Browser** | Any site you or an agent open | Normal web traffic from the in-app browser or the agent browser tool (your own Node and Playwright). | Only while you or an agent browse. | Yes. | Don't use the browser. |
| **Phone access** | Local: listens on your Tailscale address only, and asks the local Tailscale service who is connecting | Nothing to the internet. No relay or cloud pairing service. | Only while Phone access is turned on in Settings. | Yes, off by default. | Turn off Phone access. |
| **Engine and MCP** | Local: a Unix socket and a loopback-only MCP port | Nothing leaves the machine. | Always, between diri's own processes and the agents it starts. | Required. | Not needed: it never leaves the machine. |

## Not fetched
diri does not download fonts, images, avatars or web assets. What's New clips, agent definitions and model prices ship inside the app. Links you click open in your browser; diri never requests them itself.

The full policy is in [PRIVACY.md](https://github.com/cristicretu/diri/blob/main/PRIVACY.md).
