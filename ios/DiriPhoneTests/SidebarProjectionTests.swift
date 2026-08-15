import XCTest
@testable import DiriPhone

/// These mirror the invariants the desktop's ordering commit established. If
/// one of them fails, the phone and the desktop are drawing different lists
/// from the same data, which is the whole thing this port exists to avoid.
final class SidebarProjectionTests: XCTestCase {
    private func session(
        _ id: String,
        project: String = "/repo",
        parent: String? = nil,
        created: Double = 0,
        title: String = "s"
    ) -> SessionRecord {
        SessionRecord(
            id: id,
            kind: .claudeCode,
            foregroundAgent: nil,
            cwd: project,
            projectID: project,
            worktreePath: nil,
            gitBranch: nil,
            title: title,
            status: .idle,
            needsInput: nil,
            parent: parent,
            createdAt: created,
            updatedAt: created,
            lastSeenAt: nil,
            pinned: false,
            archivedAt: nil,
            host: nil,
            hibernation: nil
        )
    }

    private func rows(_ sessions: [SessionRecord], prefs: SidebarProjection.Preferences = .init()) -> [SidebarRow] {
        SidebarProjection.build(sessions: sessions, projects: [:], prefs: prefs)
            .flatMap(\.rows)
    }

    /// The defect the desktop fixed: unranked sessions tie-broke on
    /// `created_at` DESCENDING, so a new session landed at the top of its
    /// project instead of the bottom.
    func testUnrankedSessionsSortOldestFirst() {
        let list = rows([
            session("c", created: 300),
            session("a", created: 100),
            session("b", created: 200),
        ])
        XCTAssertEqual(list.map(\.session.id), ["a", "b", "c"])
    }

    func testPinnedSessionsLeadTheirSiblings() {
        var prefs = SidebarProjection.Preferences()
        prefs.pinnedSessions = ["c"]
        let list = rows(
            [session("a", created: 100), session("b", created: 200), session("c", created: 300)],
            prefs: prefs
        )
        XCTAssertEqual(list.map(\.session.id), ["c", "a", "b"])
    }

    func testManualOrderBeatsCreationTime() {
        var prefs = SidebarProjection.Preferences()
        prefs.sessionOrder = ["c", "a"]
        let list = rows(
            [session("a", created: 100), session("b", created: 200), session("c", created: 300)],
            prefs: prefs
        )
        // Ranked rows lead in their ranked order; the unranked one falls to the
        // end rather than springing back above its dragged siblings.
        XCTAssertEqual(list.map(\.session.id), ["c", "a", "b"])
    }

    func testChildrenNestUnderTheirParent() {
        let list = rows([
            session("root", created: 100),
            session("child", parent: "root", created: 200),
            session("grandchild", parent: "child", created: 300),
        ])
        XCTAssertEqual(list.map(\.session.id), ["root", "child", "grandchild"])
        XCTAssertEqual(list.map(\.depth), [0, 1, 2])
    }

    /// The rail bit for a column is set only while that ancestor still has
    /// siblings below, so the last child's rail stops on its own elbow.
    func testLastChildClearsItsRailColumn() {
        let list = rows([
            session("root", created: 100),
            session("first", parent: "root", created: 200),
            session("last", parent: "root", created: 300),
        ])
        let first = try! XCTUnwrap(list.first { $0.session.id == "first" })
        let last = try! XCTUnwrap(list.first { $0.session.id == "last" })
        XCTAssertEqual(first.rails & 1, 1, "a non-final child's parent column keeps running")
        XCTAssertEqual(last.rails & 1, 0, "the final child's column stops at its elbow")
    }

    func testCollapsedParentHidesItsSubtree() {
        var prefs = SidebarProjection.Preferences()
        prefs.collapsedSessions = ["root"]
        let list = rows([
            session("root", created: 100),
            session("child", parent: "root", created: 200),
        ], prefs: prefs)
        XCTAssertEqual(list.map(\.session.id), ["root"])
        XCTAssertTrue(list[0].hasChildren)
        XCTAssertTrue(list[0].collapsed)
    }

    /// Nothing in the daemon bounds the depth of a spawn chain, and a cycle
    /// must not hang the projection or drop the rows inside it.
    func testAParentCycleIsBrokenRatherThanLooping() {
        var one = session("one", created: 100)
        var two = session("two", created: 200)
        one.parent = "two"
        two.parent = "one"
        let list = rows([one, two])
        XCTAssertEqual(list.count, 2, "both rows survive the cycle")
    }

    func testSelfParentIsIgnored() {
        var alone = session("alone", created: 100)
        alone.parent = "alone"
        let list = rows([alone])
        XCTAssertEqual(list.map(\.depth), [0])
    }

    func testArchivedSessionsAreBucketedMostRecentFirst() {
        var old = session("old", created: 100)
        old.archivedAt = 500
        var recent = session("recent", created: 200)
        recent.archivedAt = 900
        let groups = SidebarProjection.build(
            sessions: [old, recent], projects: [:], prefs: .init()
        )
        XCTAssertEqual(groups.first?.archived.map(\.id), ["recent", "old"])
        XCTAssertTrue(groups.first?.rows.isEmpty ?? false, "archived rows are not drawn inline")
    }

    /// A project is as old as its oldest session, which is what stops a project
    /// jumping around as its sessions come and go.
    func testProjectsSortByArrivalOfTheirOldestSession() {
        let groups = SidebarProjection.build(
            sessions: [
                session("new", project: "/newer", created: 900),
                session("old", project: "/older", created: 100),
            ],
            projects: [:],
            prefs: .init()
        )
        XCTAssertEqual(groups.map(\.project.id), ["/older", "/newer"])
    }
}
