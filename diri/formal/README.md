# Claude Code prompt cache

A Claude Code login can put its per-conversation scratchpad path in a cached message. Three fresh interactive Opus 5.5 sessions with Diri injection reproduced the cache writes. Replacing the 36-byte conversation UUID in the scratchpad path made the recorded tools, system blocks and messages byte-identical across all three. The Diri session ID, recovery directory, socket path and bearer token did not appear in the compared prompt content.

## Measurements

All rows used `hi`, `claude-opus-5-5` and high effort. Each entry is cache creation / cache read, in input tokens.

| Run | Session 1 | Session 2 | Session 3 |
| --- | ---: | ---: | ---: |
| ADEMark B1, Diri 0.11.2 | 13,075 / 26,124 | 10,465 / 28,963 | 10,461 / 28,963 |
| ADEMark B1, bare Claude Code | 11,180 / 24,701 | 11,489 / 26,657 | 8,629 / 29,514 |
| ADEMark B1, Paseo | 9,951 / 12,184 | 0 / 22,135 | 0 / 22,135 |
| Live reproduction, Diri stdio injection | 33,563 / 26,124 | 15,251 / 44,435 | 15,245 / 44,435 |

The reproduction used the ADEMark manifest's Claude Code 2.1.303, one worktree, the installed Diri hook settings, stdio MCP config and skills plugin, distinct conversation and Diri IDs, and a native Claude.ai login. The user profile and repository differed from the benchmark. The live row measures the first main response in each session. ADEMark's B1 rows summarize main-thread usage over a turn. Its published rows and manifest contain no original request bodies, so the byte offsets below apply to the reproduction.

### First request differences

* **Loopback fake Messages API, fixture API key.** Two interactive bare launches yielded 81,781-byte requests with 23 tools. Two injected launches yielded 135,855-byte requests with 78 tools, including 55 from Diri. The sole decoded difference within each pair was the session UUID in the JSON string at `metadata.user_id`. The first differing raw byte was at offset 79,071 bare and 133,140 injected. Claude minted the bare ID and used the Diri-supplied `--session-id` in the injected case. Tools and their ordering, system text, cache markers and every message matched within each pair. The markers were on `system[1]`, `system[2]` and `messages[1].content[0]`. The fake returned a Messages response and did no token accounting.
* **Native authenticated runs.** Claude Code's own first-request records preserved the system and tool shapes, ordered message blobs and cache markers. Across three runs the sole prompt difference was bytes **[1052, 1088)** in the UTF-8 text of `messages[1].content[0]`, a 41,663-byte block. The containing path was `/private/tmp/claude-<uid>/<project-slug>/<conversation-uuid>/scratchpad`. The environment reminder, date, Git state, skills, hook text, tool definitions and MCP instructions otherwise matched. All three markers had a one-hour TTL, with global scope on the first system marker. The scratchpad UUID occurs before the last marker and changes the cache key for that prefix. The timing metadata on hook records was outside the prompt. [Anthropic prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)
* **Why authentication matters.** Anthropic documents the scratchpad for Claude.ai logins on its first-party API. An API-key fixture omits it. The supported `enableArtifact: false` setting removes eligibility for the scratchpad and also turns off Claude's Artifact tool. `CLAUDE_CODE_TMPDIR` moves the parent directory and retains the session component. Diri preserves the user's artifact preference and Claude's conversation identity. [Scratchpad directory](https://code.claude.com/docs/en/claude-directory#session-scratchpad-directory) · [Artifact setting](https://code.claude.com/docs/en/settings-reference#enableartifact)

## Checking Diri's contribution

From this directory run `lake build` with the pinned Lean toolchain. From `diri/` run `cargo test -p diri-engine --test mcp_http fresh_claude_spawns_share_prompt_visible_injection`.

`PromptCache.lean` models the Claude injection boundary. With a fixed manifest, effective preferences and project catalog, changing the session changes only the runtime projection. The ordering theorem states that MCP schemas and plugin skills retain their declared catalog sequence. The model treats resource contents as inputs. It does not model Anthropic's renderer or compute token counts.

The Rust regression starts two Claude stand-ins through the Engine in the same project. It checks for distinct CLI conversation UUIDs, compares the remaining launch arguments, hook settings, emitted skill files and the authenticated MCP initialize and tool-list response bytes. Effective preferences include whether Diri reads the tab aloud. Selecting one tab for voice intentionally changes its prompt hook output. The guarantee is for sessions with the same effective preference.
