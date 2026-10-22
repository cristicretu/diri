# Agent marks

Monochrome marks for every Agent Diri knows beyond the four hand-drawn brand
marks (Claude, OpenAI, Cursor, Gemini): the launch manifests' Agents and the
coding CLIs the Engine recognises typed into a shell
(`diri-engine/src/recognition.rs`). `src/agent_marks.rs` is the catalog.

Marks identify their tools only; no affiliation or endorsement is implied.
Names and marks remain the property of their owners.

## Rules

- An official logo is used only when its artwork is published under a licence
  that allows redistributing it (MIT or Apache-2.0 below). The original file
  is kept unmodified in `source/`, and the licence text in `licenses/`.
- Every other Agent gets a **monogram**: its initial(s) in Inter Bold
  (SIL OFL 1.1), knocked out of a rounded tile. Website favicons, press-kit
  files and PNGs with no stated licence are not redistributed.
- `<id>.svg` is generated, never hand-edited: one even-odd path in a 24×24
  view box, centred, its larger side `24 × size`. `size` is the optical
  correction (solid blocks smaller, open or spiky shapes larger) judged against
  the brand marks in the grid below.

```sh
python3 generate.py /path/to/Inter-4.1/extras/ttf/Inter-Bold.ttf   # needs fonttools
DIRI_VISUAL_OUTPUT=/tmp/marks.png cargo test -p diri-app render_agent_marks_screenshot -- --ignored
```

Inter 4.1: `https://github.com/rsms/inter/releases/download/v4.1/Inter-4.1.zip`
(sha256 `9883fdd4a49d4fb66bd8177ba6625ef9a64aa45899767dde3d36aa425756b11e`).

## Sources (fetched 2026-10-11)

LobeHub = `https://raw.githubusercontent.com/lobehub/lobe-icons/c385b2b8d1f9e19aa86e628d4e23c91ee1111a47/packages/static-svg/icons/`

| Mark | Agent | Artwork | Source | Licence | sha256 (16) |
| --- | --- | --- | --- | --- | --- |
| `aider` | Aider | `source/aider-safari-pinned-tab.svg` | `https://raw.githubusercontent.com/Aider-AI/aider/5dc9490bb35f9729ef2c95d00a19ccd30c26339c/aider/website/assets/icons/safari-pinned-tab.svg` | Apache-2.0 | `145269573c298162` |
| `antigravity` | Antigravity | `source/antigravity.svg` | LobeHub `antigravity.svg` | MIT (LobeHub) | `43f551fd125c17fb` |
| `cline` | Cline CLI | `source/cline-logo-filled.svg` | `https://raw.githubusercontent.com/cline/cline/fa840c741c3fc2eb49e7e0a4484895a99dae5cc5/apps/cline-hub/src/webview/public/cline-logo-filled.svg` | Apache-2.0 | `6aab009bf089a8dc` |
| `codebuddy` | CodeBuddy | `source/codebuddy.svg` | LobeHub `codebuddy.svg` | MIT (LobeHub) | `abe926f4acd9d564` |
| `command-code` | Command Code | `source/commandcode.svg` | LobeHub `commandcode.svg` | MIT (LobeHub) | `0b3090f6e9194f32` |
| `copilot` | Copilot CLI | `source/copilot-24.svg` | `https://raw.githubusercontent.com/primer/octicons/97825f832c98f817867f770d084c08e3edc6f78c/icons/copilot-24.svg` | MIT (GitHub Primer) | `eeafb3c2f333e04c` |
| `goose` | Goose | `source/goose.svg` | LobeHub `goose.svg` | MIT (LobeHub) | `8779d9a61c5b0be7` |
| `grok` | Grok | `source/grok.svg` | LobeHub `grok.svg` | MIT (LobeHub) | `9175fc90c2265516` |
| `junie` | Junie | `source/junie.svg` | LobeHub `junie.svg` | MIT (LobeHub) | `dded070018637752` |
| `kilo` | Kilo Code | `source/kilo.svg` | `https://raw.githubusercontent.com/Kilo-Org/kilocode/974723450caa7c91f140b128e2f2e3b4a932ade8/packages/ui/src/assets/icons/provider/kilo.svg` | MIT | `425602fa01faa0eb` |
| `kimi` | Kimi | `source/kimi.svg` | LobeHub `kimi.svg` | MIT (LobeHub) | `ed08131b46acf70e` |
| `kiro` | Kiro | `source/kiro.svg` | LobeHub `kiro.svg` | MIT (LobeHub) | `1e56073e35799b0d` |
| `muse` | Muse Code | `source/meta.svg` (Meta's mark, which Muse Code's docs use) | LobeHub `meta.svg` | MIT (LobeHub) | `805ef9a35305393e` |
| `openclaw` | OpenClaw | `source/openclaw.svg` | LobeHub `openclaw.svg` | MIT (LobeHub) | `8ef4de8032b06492` |
| `opencode` | OpenCode | `source/opencode.svg` | LobeHub `opencode.svg` | MIT (LobeHub) | `7cfa6e9d6726f7c9` |
| `openhands` | OpenHands | `source/openhands.svg` | LobeHub `openhands.svg` | MIT (LobeHub) | `246a996b64ceb4e6` |
| `qoder` | Qoder CLI | `source/qoder.svg` | LobeHub `qoder.svg` | MIT (LobeHub) | `f8503aacc41a58e7` |
| `qwen` | Qwen Code | `source/qwen.svg` | LobeHub `qwen.svg` | MIT (LobeHub) | `dcb3ba2f2b55ccba` |
| `traecli` | TRAE CLI | `source/trae.svg` | LobeHub `trae.svg` | MIT (LobeHub) | `59ef6255dfce52d4` |
| `vibe` | Mistral Vibe | `source/mistral-vibe.svg` | `https://raw.githubusercontent.com/mistralai/mistral-vibe/7cb91894c40bb25173abcfa36e5ea2b4b81eb28c/distribution/zed/icons/mistral_vibe.svg` | Apache-2.0 | `7c0e3122d2be9fc6` |

Monograms (Inter Bold, OFL 1.1), and why there is no logo:

| Mark | Agent | Letters | Why a monogram |
| --- | --- | --- | --- |
| `abacusai` | Abacus.AI | Ab | Only an unlicensed favicon PNG |
| `amp` | Amp | A | Website asset, no licence stated |
| `auggie` | Auggie | Au | Website favicon, no licence stated |
| `codebuff` | Codebuff | Cb | Only an unlicensed PNG |
| `coderabbit` | CodeRabbit | CR | Website asset, no licence stated |
| `crush` | Crush | C | PNG under FSL-1.1; mark is Charm's trademark |
| `devin` | Devin | D | Brand kit grants no redistribution licence |
| `droid` | Droid | Dr | Website favicon, no licence stated |
| `fx` | fx | fx | No published mark |
| `hermes` | Hermes | H | The logo is a detailed portrait that does not survive 14 px |
| `maki` | Maki | M | No published mark |
| `mcode` | MCode | Mc | No published mark |
| `pi` | Pi | π | Press-kit asset, no licence stated |
| `warp` | Warp | W | Website asset, no licence stated |
| `whipcode` | WhipCode | Wh | No published mark |
