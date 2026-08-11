import DirijorCore
import Foundation
import Testing

@testable import DirijorDetection

// MARK: - Manifest decode

@Suite struct ManifestDecodeTests {
    @Test func bundledManifestsLoad() throws {
        let engine = try ManifestEngine()
        // Sanity: claude-code idle prompt evaluates via the real bundled manifest.
        let snap = ScreenSnapshot(
            lines: ["╭────────────╮", "│ ❯          │", "╰────────────╯"],
            contentSeq: 1, cols: 80, rows: 24)
        let obs = engine.evaluate(snap, manifestID: "claude-code")
        #expect(obs?.state == .idle)
    }

    @Test func genericManifestIsProcessOnlyEmpty() throws {
        let data = """
        {"schemaVersion":1,"id":"generic","version":"1","statusModel":"processOnly","rules":[]}
        """.data(using: .utf8)!
        let m = try JSONDecoder().decode(Manifest.self, from: data)
        #expect(m.statusModel == .processOnly)
        #expect(m.rules.isEmpty)
        #expect(m.id == "generic")
    }

    @Test func ruleDecodesAllFields() throws {
        let data = """
        {"schemaVersion":1,"id":"t","version":"1","statusModel":"full","rules":[
          {"id":"r1","state":"blockedPermission","priority":1000,"region":"whole_recent",
           "when":{"all":[{"contains":"x"},{"not":{"regex":"^y"}}]},
           "flags":["visible_blocker"],
           "capture":{"region":"prompt_box_body","regionLines":8,"maxChars":200}}
        ]}
        """.data(using: .utf8)!
        let m = try JSONDecoder().decode(Manifest.self, from: data)
        let r = try #require(m.rules.first)
        #expect(r.id == "r1")
        #expect(r.state == .blockedPermission)
        #expect(r.priority == 1000)
        #expect(r.region == .whole_recent)
        #expect(r.flags.contains("visible_blocker"))
        #expect(r.capture?.region == .prompt_box_body)
        #expect(r.capture?.regionLines == 8)
        #expect(r.capture?.maxChars == 200)
        #expect(r.isBlocker)
    }

    @Test func regionLinesDefaultsToFive() throws {
        let data = """
        {"id":"r","state":"idle","priority":1,"region":"bottom_non_empty_lines",
         "when":{"contains":"x"}}
        """.data(using: .utf8)!
        let r = try JSONDecoder().decode(Rule.self, from: data)
        #expect(r.regionLines == 5)
        #expect(r.flags.isEmpty)
        #expect(r.capture == nil)
    }
}

// MARK: - Predicate combinators

@Suite struct PredicateTests {
    private func decode(_ json: String) throws -> DirijorDetection.Predicate {
        try JSONDecoder().decode(DirijorDetection.Predicate.self, from: json.data(using: .utf8)!)
    }

    private func ctx(_ text: String, progress: Int? = nil) -> PredicateContext {
        PredicateContext(text: text, lines: text.split(separator: "\n").map(String.init),
                         progressState: progress)
    }

    @Test func containsIsCaseInsensitive() throws {
        let p = try decode(#"{"contains":"Allow Command?"}"#)
        #expect(p.evaluate(ctx("please: allow command? now")))
        #expect(!p.evaluate(ctx("nothing here")))
    }

    @Test func regexMatchesWholeText() throws {
        let p = try decode(#"{"regex":"^abc"}"#)
        #expect(p.evaluate(ctx("abcdef")))
        #expect(!p.evaluate(ctx("xabc")))
    }

    @Test func lineRegexMatchesAnyLine() throws {
        let p = try decode(#"{"lineRegex":"^\\s*\\d+\\."}"#)
        #expect(p.evaluate(ctx("header\n  2. Second\nfooter")))
        #expect(!p.evaluate(ctx("header\nfooter")))
    }

    @Test func progressPredicate() throws {
        let p = try decode(#"{"progress":{"state":0}}"#)
        #expect(p.evaluate(ctx("", progress: 0)))
        #expect(!p.evaluate(ctx("", progress: 1)))
        #expect(!p.evaluate(ctx("", progress: nil)))
    }

    @Test func anyCombinator() throws {
        let p = try decode(#"{"any":[{"contains":"foo"},{"contains":"bar"}]}"#)
        #expect(p.evaluate(ctx("has bar")))
        #expect(!p.evaluate(ctx("has baz")))
    }

    @Test func allCombinator() throws {
        let p = try decode(#"{"all":[{"contains":"foo"},{"contains":"bar"}]}"#)
        #expect(p.evaluate(ctx("foo and bar")))
        #expect(!p.evaluate(ctx("only foo")))
    }

    @Test func notCombinator() throws {
        let p = try decode(#"{"not":{"contains":"foo"}}"#)
        #expect(p.evaluate(ctx("bar")))
        #expect(!p.evaluate(ctx("foo")))
    }
}

// MARK: - Region extraction

@Suite struct RegionExtractionTests {
    @Test func wholeRecentFiltersBlanksAndCaps() {
        var lines = ["   ", "a", "", "b"]
        #expect(Regions.wholeRecent(lines) == ["a", "b"])
        lines = (0..<100).map { "line\($0)" }
        #expect(Regions.wholeRecent(lines).count == 60)
        #expect(Regions.wholeRecent(lines).first == "line40")
    }

    @Test func bottomNonEmptyReturnsLastN() {
        let lines = ["a", "", "b", "  ", "c", "d"]
        #expect(Regions.bottomNonEmpty(lines, 2) == ["c", "d"])
        #expect(Regions.bottomNonEmpty(lines, 10) == ["a", "b", "c", "d"])
        #expect(Regions.bottomNonEmpty(lines, 0).isEmpty)
    }

    @Test func promptBoxBodyStripsBordersOfLastBox() {
        let lines = [
            "some output above",
            "╭──────────────────────────╮",
            "│ Bash command             │",
            "│ rm -rf build             │",
            "│ ❯ 1. Yes                 │",
            "│   2. No                  │",
            "╰──────────────────────────╯",
            "esc to cancel",
        ]
        let body = Regions.promptBoxBody(lines)
        #expect(body.contains { $0.contains("Bash command") })
        #expect(body.contains { $0.contains("❯ 1. Yes") })
        // Pure border rows are dropped.
        #expect(!body.contains { $0.contains("╭") || $0.contains("╰") })
    }

    @Test func promptBoxBodyFallbackToPromptMarker() {
        let lines = ["output", "❯ type here"]
        let body = Regions.promptBoxBody(lines)
        #expect(body == ["❯ type here"])
    }

    @Test func numberedOptionsExtractsLabels() {
        let lines = [" ❯ 1. Yes ", "   2. No, tell Claude what to do (esc)", "not an option"]
        let opts = Regions.numberedOptions(lines)
        #expect(opts == ["Yes", "No, tell Claude what to do (esc)"])
    }

    @Test func oscTitleAndProgressRegions() {
        let snap = ScreenSnapshot(
            lines: ["x"], oscTitle: "⠹ Working", oscProgressState: 0, contentSeq: 1,
            cols: 80, rows: 24)
        #expect(Regions.extract(.osc_title, regionLines: 5, from: snap) == ["⠹ Working"])
        #expect(Regions.extract(.osc_progress, regionLines: 5, from: snap).isEmpty)
        let noTitle = ScreenSnapshot(lines: ["x"], contentSeq: 1, cols: 80, rows: 24)
        #expect(Regions.extract(.osc_title, regionLines: 5, from: noTitle).isEmpty)
    }
}

// MARK: - Every bundled manifest

/// The engine loads manifests best-effort (a broken user override must not take
/// out every other agent's detection), which means a typo in a BUNDLED manifest
/// — an unbalanced regex, an unknown region — would silently ship as "this
/// agent has no rules". These tests decode strictly instead.
@Suite struct BundledManifestIntegrityTests {
    @Test func everyBundledManifestDecodesStrictly() throws {
        let urls = AgentCatalog.manifestURLs(overridesDirectory: nil)
        #expect(urls.count >= 19, "expected the full bundled agent roster, got \(urls.count)")
        for url in urls {
            let data = try Data(contentsOf: url)
            // Throws on a bad predicate, an unknown region, or a regex
            // NSRegularExpression rejects — the failure modes a hand-written
            // manifest actually has.
            let manifest = try JSONDecoder().decode(Manifest.self, from: data)
            #expect(
                manifest.id == url.deletingPathExtension().lastPathComponent,
                "\(url.lastPathComponent) declares id \(manifest.id)")
            #expect(
                manifest.statusModel == .processOnly || !manifest.rules.isEmpty,
                "\(manifest.id) claims full status detection but has no rules")
        }
    }

    @Test func everyFullManifestIsReachableThroughTheEngine() throws {
        let engine = try ManifestEngine()
        for descriptor in AgentCatalog.shared.ordered where descriptor.firstClass {
            // A first-class agent whose manifest failed to load would evaluate
            // to nil forever and sit in "working" — the exact silent failure
            // the strict decode above is guarding against, seen from the
            // engine's side.
            let snapshot = ScreenSnapshot(
                lines: ["nothing that matches anything"], contentSeq: 1, cols: 80, rows: 24)
            _ = engine.evaluate(snapshot, manifestID: descriptor.id)
            #expect(engine.storage.manifests[descriptor.id] != nil, "no rules for \(descriptor.id)")
        }
    }

    @Test func clineAndMakiCarryOfficialSetupGuidance() throws {
        let urls = Dictionary(uniqueKeysWithValues: AgentCatalog.manifestURLs(
            overridesDirectory: nil
        ).map { ($0.deletingPathExtension().lastPathComponent, $0) })
        for (id, expectedURL) in [
            ("cline", "https://docs.cline.bot/getting-started/installing-cline"),
            ("maki", "https://github.com/tontinton/maki#installation"),
        ] {
            let url = try #require(urls[id], "missing \(id) manifest")
            let root = try #require(
                JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
            let agent = try #require(root["agent"] as? [String: Any])
            let setup = try #require(agent["setup"] as? [String: Any])
            #expect(setup["url"] as? String == expectedURL)
            #expect(!(setup["installHint"] as? String ?? "").isEmpty)
            #expect(!(setup["signInHint"] as? String ?? "").isEmpty)
        }
    }
}
