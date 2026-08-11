import Foundation
import Testing

@testable import DirijorDetection

/// End-to-end manifest evaluation against realistic Claude/Codex screens.
@Suite struct GoldenScreenTests {
    let engine: ManifestEngine

    init() throws {
        engine = try ManifestEngine()
    }

    private func snap(_ lines: [String], title: String? = nil, progress: Int? = nil) -> ScreenSnapshot {
        ScreenSnapshot(lines: lines, oscTitle: title, oscProgressState: progress,
                       contentSeq: 1, cols: 100, rows: 30)
    }

    // MARK: Claude

    @Test func claudeIdlePromptBox() {
        let s = snap([
            "Done. Anything else?",
            "╭────────────────────────────────────────────╮",
            "│ ❯                                          │",
            "╰────────────────────────────────────────────╯",
        ])
        let obs = engine.evaluate(s, manifestID: "claude-code")
        #expect(obs?.state == .idle)
        #expect(obs?.matchedRuleID == "idle-prompt-box")
    }

    @Test func claudePermissionDialog() {
        let s = snap([
            "╭────────────────────────────────────────────╮",
            "│ Bash command                               │",
            "│                                            │",
            "│ rm -rf build                               │",
            "│                                            │",
            "│ Do you want to proceed?                    │",
            "│ ❯ 1. Yes                                   │",
            "│   2. No, and tell Claude what to do (esc)  │",
            "╰────────────────────────────────────────────╯",
            "esc to cancel",
        ])
        let obs = try! #require(engine.evaluate(s, manifestID: "claude-code"))
        #expect(obs.state == .blockedPermission)
        #expect(obs.options == ["Yes", "No, and tell Claude what to do (esc)"])
        #expect(obs.promptExcerpt?.contains("rm -rf build") == true)
    }

    @Test func claudeWorkingBrailleTitle() {
        // U+2839 (⠹) is within the braille range.
        let s = snap(["thinking..."], title: "⠹ Waddling…")
        let obs = engine.evaluate(s, manifestID: "claude-code")
        #expect(obs?.state == .working)
        #expect(obs?.matchedRuleID == "working-spinner")
    }

    @Test func claudeTranscriptViewerSkips() {
        let s = snap([
            "Showing detailed transcript · ctrl+r to toggle",
            "╭────────────╮",
            "│ ❯          │",
            "╰────────────╯",
        ])
        let obs = engine.evaluate(s, manifestID: "claude-code")
        #expect(obs?.state == .skip)
        #expect(obs?.priority == 1200)
    }

    @Test func claudeIdleProgressZero() {
        let s = snap(["no box here"], progress: 0)
        let obs = engine.evaluate(s, manifestID: "claude-code")
        #expect(obs?.state == .idle)
        #expect(obs?.matchedRuleID == "idle-progress-zero")
    }

    // MARK: Codex

    @Test func codexActionRequiredTitle() {
        let s = snap([
            "running command…",
            "npm install",
        ], title: "● Action Required")
        let obs = try! #require(engine.evaluate(s, manifestID: "codex"))
        #expect(obs.state == .blockedPermission)
        #expect(obs.matchedRuleID == "action-required-title")
        #expect(obs.promptExcerpt?.contains("npm install") == true)
    }

    @Test func codexConfirmPrompt() {
        let s = snap([
            "╭─ Allow command? ─────────────╮",
            "│ npm install                  │",
            "│ ❯ 1. Yes                     │",
            "│   2. No                      │",
            "╰──────────────────────────────╯",
            "Press enter to confirm or esc to cancel",
        ])
        let obs = try! #require(engine.evaluate(s, manifestID: "codex"))
        #expect(obs.state == .blockedPermission)
        #expect(obs.options == ["Yes", "No"])
    }

    @Test func codexIdlePrompt() {
        let s = snap([
            "╭──────────────────────────────╮",
            "│ › Ask Codex to do something  │",
            "╰──────────────────────────────╯",
        ])
        let obs = engine.evaluate(s, manifestID: "codex")
        #expect(obs?.state == .idle)
        #expect(obs?.matchedRuleID == "idle-prompt-box")
    }

    // MARK: Cursor

    @Test func cursorConfirmDialog() {
        let s = snap([
            "╭──────────────────────────────╮",
            "│ Run this command?            │",
            "│ npm install                  │",
            "│ Run (y)   Reject (esc/n)     │",
            "╰──────────────────────────────╯",
        ])
        let obs = try! #require(engine.evaluate(s, manifestID: "cursor"))
        #expect(obs.state == .blockedPermission)
        #expect(obs.matchedRuleID == "confirm-dialog")
        #expect(obs.promptExcerpt?.contains("npm install") == true)
    }

    @Test func cursorWorkingStatusLine() {
        let s = snap([
            "some earlier output",
            "Generating",
        ])
        let obs = engine.evaluate(s, manifestID: "cursor")
        #expect(obs?.state == .working)
        #expect(obs?.matchedRuleID == "working-status-line")
    }

    @Test func cursorIdlePrompt() {
        let s = snap([
            "╭──────────────────────────────╮",
            "│ → Add a follow-up            │",
            "╰──────────────────────────────╯",
        ])
        let obs = engine.evaluate(s, manifestID: "cursor")
        #expect(obs?.state == .idle)
    }

    // MARK: Gemini

    @Test func geminiConfirmDialog() {
        let s = snap([
            "╭──────────────────────────────────────╮",
            "│ Apply this change?                   │",
            "│ ● 1. Yes, allow once                 │",
            "│   2. Yes, allow always               │",
            "│   3. No, suggest changes (esc)       │",
            "╰──────────────────────────────────────╯",
        ])
        let obs = try! #require(engine.evaluate(s, manifestID: "gemini"))
        #expect(obs.state == .blockedPermission)
        #expect(obs.matchedRuleID == "confirm-dialog")
    }

    @Test func geminiWorkingCancelTimer() {
        let s = snap([
            "⠹ Polishing the code (esc to cancel, 12s)",
        ])
        let obs = engine.evaluate(s, manifestID: "gemini")
        #expect(obs?.state == .working)
        #expect(obs?.matchedRuleID == "working-cancel-timer")
    }

    @Test func geminiIdlePrompt() {
        let s = snap([
            "╭──────────────────────────────────────╮",
            "│ >   Type your message or @path/to/file │",
            "╰──────────────────────────────────────╯",
        ])
        let obs = engine.evaluate(s, manifestID: "gemini")
        #expect(obs?.state == .idle)
    }

    // MARK: Cline
    // Text and layout markers come from Cline's official OpenTUI components.

    @Test func clineToolPermission() {
        let s = snap([
            "Approve tool call?",
            "run_commands",
            "  $ swift test",
            "[y] approve  [n] deny",
        ])
        let obs = try! #require(engine.evaluate(s, manifestID: "cline"))
        #expect(obs.state == .blockedPermission)
        #expect(obs.matchedRuleID == "tool-permission")
        #expect(obs.promptExcerpt?.contains("swift test") == true)
    }

    @Test func clineFollowUpQuestion() {
        let s = snap([
            "Which approach should I use?",
            "❯ Keep the API",
            "  Replace it",
            "  Type a response...",
            "↑/↓ navigate, Enter to select, 1-2 to pick",
        ])
        let obs = engine.evaluate(s, manifestID: "cline")
        #expect(obs?.state == .blockedQuestion)
        #expect(obs?.matchedRuleID == "follow-up-question")
    }

    @Test func clineStreamingOutranksPersistentComposer() {
        let s = snap([
            "⠋ Streaming a response",
            "❯ Ask anything...",
            "○ Plan  ● Act  (Tab)",
        ])
        let obs = engine.evaluate(s, manifestID: "cline")
        #expect(obs?.state == .working)
        #expect(obs?.matchedRuleID == "streaming-spinner")
    }

    @Test func clineIdleComposerSupportsBothModes() {
        #expect(
            engine.evaluate(snap(["❯ Ask anything..."]), manifestID: "cline")?.state == .idle)
        #expect(
            engine.evaluate(snap(["❯ Plan something..."]), manifestID: "cline")?.state == .idle)
    }

    // MARK: Maki
    // These are the stable form and mode-bar shapes in Maki's TUI.

    @Test func makiPermissionForm() {
        let s = snap([
            "Permission required",
            "bash: rm build/output",
            "y allow    n deny",
        ])
        let obs = engine.evaluate(s, manifestID: "maki")
        #expect(obs?.state == .blockedPermission)
        #expect(obs?.matchedRuleID == "permission-prompt")
    }

    @Test func makiPlanCompleteForm() {
        let s = snap([
            "Plan complete",
            "space toggle parallel    edit plan",
            "enter confirm",
        ])
        let obs = engine.evaluate(s, manifestID: "maki")
        #expect(obs?.state == .blockedQuestion)
        #expect(obs?.matchedRuleID == "plan-complete-form")
    }

    @Test func makiModeBarDistinguishesWorkingFromIdle() {
        let working = engine.evaluate(snap([" ⠋ ⠙ [BUILD] sonnet"]), manifestID: "maki")
        #expect(working?.state == .working)
        #expect(working?.matchedRuleID == "status-bar-spinner-working")

        let idle = engine.evaluate(snap([" [PLAN] sonnet"]), manifestID: "maki")
        #expect(idle?.state == .idle)
        #expect(idle?.matchedRuleID == "status-bar-idle")
    }

    @Test func makiNarrowPaneFallbackRejectsQueuedPrompt() {
        let idle = engine.evaluate(snap(["────────", "❯ ", "model"]), manifestID: "maki")
        #expect(idle?.state == .idle)
        #expect(idle?.matchedRuleID == "prompt-box-idle")

        let queued = engine.evaluate(
            snap(["queue another prompt", "❯ ", "model"]), manifestID: "maki")
        #expect(queued == nil)
    }

    // MARK: Aider
    // Screens below are verbatim from live aider 0.86.2 PTY captures.

    @Test func aiderCreateFileConfirm() {
        let s = snap([
            "hello",
            "tokens: 601 sent, 8 received.",
            "hello.txt",
            "Create new file? (Y)es/(N)o [Yes]:",
        ])
        let obs = try! #require(engine.evaluate(s, manifestID: "aider"))
        #expect(obs.state == .blockedPermission)
        #expect(obs.matchedRuleID == "confirm-prompt")
        #expect(obs.promptExcerpt?.contains("Create new file") == true)
    }

    @Test func aiderStartupWarningConfirm() {
        let s = snap([
            "You can skip this check with --no-show-model-warnings",
            "https://aider.chat/docs/llms/warnings.html",
            "Open documentation url for more info? (Y)es/(N)o/(D)on't ask again [Yes]:",
        ])
        let obs = engine.evaluate(s, manifestID: "aider")
        #expect(obs?.state == .blockedPermission)
    }

    @Test func aiderRequestSpinner() {
        let s = snap([
            "> Create a file hello.txt containing exactly the word hi",
            "        ░█ Waiting for openai/deepseek-v4-flash",
        ])
        let obs = engine.evaluate(s, manifestID: "aider")
        #expect(obs?.state == .working)
        #expect(obs?.matchedRuleID == "working-spinner")
    }

    @Test func aiderCommitSpinner() {
        let s = snap([
            "Applied edit to hello.txt",
            "        █░ Generating commit message with openai/deepseek-v4-flash",
        ])
        let obs = engine.evaluate(s, manifestID: "aider")
        #expect(obs?.state == .working)
    }

    @Test func aiderIdleBarePrompt() {
        let s = snap([
            "Commit 5e7632a feat: add hello.txt with 'hi' content",
            "hello.txt",
            ">  ",
        ])
        let obs = engine.evaluate(s, manifestID: "aider")
        #expect(obs?.state == .idle)
        #expect(obs?.matchedRuleID == "idle-prompt")
    }

    @Test func aiderChatModePrompt() {
        let s = snap([
            "Aider v0.86.2",
            "ask>  ",
        ])
        let obs = engine.evaluate(s, manifestID: "aider")
        #expect(obs?.state == .idle)
    }

    @Test func aiderStreamingMatchesNothingSoStateHolds() {
        // Mid-stream there is no spinner and no prompt; the engine returns nil
        // and the reducer holds the previous (working) state — by design.
        let s = snap([
            "hello.txt",
            "hi",
        ])
        #expect(engine.evaluate(s, manifestID: "aider") == nil)
    }

    // MARK: Aider — constructed regression screens
    // Unlike the captures above these are assembled by hand, each pinning a
    // false reading the rules used to produce.

    @Test func aiderAnsweredConfirmReleasesTheBlocker() {
        // The answered line stays on screen and the spinner only repaints its
        // own row: a substring match over the bottom lines kept reporting
        // "needs input" for the whole model wait, right after the user answered.
        let s = snap([
            "hello.txt",
            "Create new file? (Y)es/(N)o [Yes]: y",
            "        ░█ Waiting for openai/deepseek-v4-flash",
        ])
        let obs = engine.evaluate(s, manifestID: "aider")
        #expect(obs?.state == .working)
        #expect(obs?.matchedRuleID == "working-spinner")
    }

    @Test func aiderWrappedConfirmStillBlocks() {
        // A long path wraps the (A)ll/(S)kip-all variant: "(Y)es/(N)o" lands on
        // one row and the colon on the next, so the rule cannot be per-line.
        let s = snap([
            "Add src/components/dashboard/widgets/RevenueChart.tsx to the chat? (Y)es/(N)o/(A)ll/(S)k",
            "ip all/(D)on't ask again [Yes]: ",
        ])
        let obs = try! #require(engine.evaluate(s, manifestID: "aider"))
        #expect(obs.state == .blockedPermission)
        #expect(obs.matchedRuleID == "confirm-prompt")
    }

    @Test func aiderStreamedArrowIsNotThePrompt() {
        // '-->' and '->' matched the prompt pattern while [a-z-] allowed the
        // hyphen, flipping a streaming turn to idle.
        #expect(engine.evaluate(snap(["<!-- primary nav", "-->"]), manifestID: "aider") == nil)
        #expect(engine.evaluate(snap(["fn parse(s: &str)", "->"]), manifestID: "aider") == nil)
    }

    @Test func unknownManifestReturnsNil() {
        #expect(engine.evaluate(snap(["x"]), manifestID: "nope") == nil)
    }
}
