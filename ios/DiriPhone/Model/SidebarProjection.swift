import Foundation

/// A port of `diri-app/src/store/projection.rs`.
///
/// The desktop settled on *one* ordering model after three defects stacked up
/// (unranked sessions tie-breaking on `created_at` descending, unranked
/// projects tie-breaking alphabetically, and a manual order seeded from a
/// HashMap). Reimplementing that from memory would reintroduce exactly those
/// bugs, so the comparison chains below are transcribed rather than invented.

/// One drawn row: a session plus where it sits in its lineage.
struct SidebarRow: Identifiable, Hashable, Sendable {
    var session: SessionRecord
    /// Zero is a session a human started; deeper rows were spawned by an
    /// ancestor through the MCP tools.
    var depth: Int
    var hasChildren: Bool
    var collapsed: Bool
    var pinned: Bool
    /// One bit per indent column, set when that column's rail continues past
    /// this row. The column directly parenting this row is bit `depth - 1`, so
    /// a last child leaves it clear and the rail stops on its elbow.
    var rails: UInt32

    var id: String { session.id }
}

/// One project group, in draw order.
struct SidebarGroup: Identifiable, Hashable, Sendable {
    var project: Project
    var rows: [SidebarRow]
    var archived: [SessionRecord]
    var pinned: Bool

    var id: String { project.id }
}

enum SidebarProjection {
    /// Rank sentinel for anything the manual order has not placed. The desktop
    /// uses `usize::MAX`; the effect that matters is that unranked rows sort
    /// after ranked ones and then fall through to the next tie-break.
    static let unranked = Int.max

    struct Preferences: Sendable {
        var sessionOrder: [String] = []
        var projectOrder: [String] = []
        var pinnedSessions: Set<String> = []
        var pinnedProjects: Set<String> = []
        var collapsedSessions: Set<String> = []
        var collapsedProjects: Set<String> = []

        init() {}
    }

    static func build(
        sessions: [SessionRecord],
        projects: [String: Project],
        prefs: Preferences
    ) -> [SidebarGroup] {
        let sessionRank = rankMap(prefs.sessionOrder)
        let projectRank = rankMap(prefs.projectOrder)

        var grouped: [String: [SessionRecord]] = [:]
        for session in sessions {
            grouped[session.projectID, default: []].append(session)
        }

        var ranked: [(arrival: Double, group: SidebarGroup)] = []
        for (projectID, members) in grouped {
            let project = projects[projectID] ?? syntheticProject(id: projectID, members: members)
            // A project is as old as its oldest session. That is the arrival
            // order a first-time user perceives, and it keeps a project from
            // jumping around as its sessions come and go.
            let arrival = members.map(\.createdAt).min() ?? .infinity

            var archived = members.filter(\.isArchived)
            let active = members.filter { !$0.isArchived }
            // Most recently archived first: the bucket is a recovery surface,
            // and the thing you just put away is the thing you are most likely
            // after.
            archived.sort { left, right in
                let leftAt = left.archivedAt ?? 0
                let rightAt = right.archivedAt ?? 0
                if leftAt != rightAt { return leftAt > rightAt }
                return left.id < right.id
            }

            let expanded = !prefs.collapsedProjects.contains(projectID)
            let rows = buildTree(
                active: active,
                sessionRank: sessionRank,
                pinned: prefs.pinnedSessions,
                collapsed: prefs.collapsedSessions,
                projectExpanded: expanded
            )

            ranked.append((
                arrival,
                SidebarGroup(
                    project: project,
                    rows: rows,
                    archived: archived,
                    pinned: prefs.pinnedProjects.contains(projectID)
                )
            ))
        }

        // Pinned projects lead, then the manual order, then arrival. The last
        // two agree by construction, so a project keeps its place whether or
        // not the manual order has been materialised yet.
        ranked.sort { left, right in
            if left.group.pinned != right.group.pinned { return left.group.pinned }
            let leftRank = rank(projectRank, left.group.project.id)
            let rightRank = rank(projectRank, right.group.project.id)
            if leftRank != rightRank { return leftRank < rightRank }
            if left.arrival != right.arrival { return left.arrival < right.arrival }
            return left.group.project.id < right.group.project.id
        }

        return ranked.map(\.group)
    }

    /// Arranges one project's active sessions into the lineage forest the
    /// sidebar draws.
    private static func buildTree(
        active: [SessionRecord],
        sessionRank: [String: Int],
        pinned: Set<String>,
        collapsed: Set<String>,
        projectExpanded: Bool
    ) -> [SidebarRow] {
        let parents = resolveParents(active)
        var children = [[Int]](repeating: [], count: active.count)
        var roots: [Int] = []
        for (index, parent) in parents.enumerated() {
            if let parent { children[parent].append(index) } else { roots.append(index) }
        }

        let order: (Int, Int) -> Bool = { left, right in
            siblingBefore(active[left], active[right], ranks: sessionRank, pinned: pinned)
        }
        roots.sort(by: order)
        for index in children.indices { children[index].sort(by: order) }

        var rows: [SidebarRow] = []
        rows.reserveCapacity(active.count)
        // Explicit stack rather than recursion: a spawn chain is
        // attacker-shaped input in the sense that nothing in the daemon bounds
        // its depth.
        var stack: [(index: Int, depth: Int, visible: Bool, rails: UInt32)] = []
        for index in roots.reversed() {
            stack.append((index, 0, projectExpanded, 0))
        }
        while let (index, depth, visible, rails) = stack.popLast() {
            let session = active[index]
            let hasChildren = !children[index].isEmpty
            let isCollapsed = hasChildren && collapsed.contains(session.id)
            if visible {
                rows.append(SidebarRow(
                    session: session,
                    depth: depth,
                    hasChildren: hasChildren,
                    collapsed: isCollapsed,
                    pinned: pinned.contains(session.id),
                    rails: rails
                ))
            }
            let childrenVisible = visible && !isCollapsed
            let count = children[index].count
            // Children inherit this row's rails and light the column beside it,
            // except for the last one — whose rail stops on its own elbow.
            let inherited = rails | (1 << UInt32(min(depth, 31)))
            for (position, child) in children[index].enumerated().reversed() {
                let childRails = position + 1 == count ? rails : inherited
                stack.append((child, depth + 1, childrenVisible, childRails))
            }
        }
        return rows
    }

    /// Resolves each session's parent to an index, dropping self-parents and
    /// breaking cycles so a malformed chain cannot hang the projection.
    private static func resolveParents(_ active: [SessionRecord]) -> [Int?] {
        var index: [String: Int] = [:]
        for (position, session) in active.enumerated() { index[session.id] = position }

        var parents: [Int?] = active.enumerated().map { position, session in
            guard let parent = session.parent, let resolved = index[parent], resolved != position
            else { return nil }
            return resolved
        }

        for start in parents.indices {
            var seen: Set<Int> = [start]
            var cursor = parents[start]
            while let node = cursor {
                if seen.contains(node) {
                    // The cycle is broken at the row that closed it, which
                    // promotes that row to a root rather than losing it.
                    parents[start] = nil
                    break
                }
                seen.insert(node)
                cursor = parents[node]
            }
        }
        return parents
    }

    /// `sibling_cmp` — pinned, then manual rank, then creation time ASCENDING,
    /// then id. The ascending creation order is the fix from the desktop's
    /// ordering commit: descending put every new session at the top of its
    /// project instead of the bottom.
    static func siblingBefore(
        _ left: SessionRecord,
        _ right: SessionRecord,
        ranks: [String: Int],
        pinned: Set<String>
    ) -> Bool {
        let leftPinned = pinned.contains(left.id)
        let rightPinned = pinned.contains(right.id)
        if leftPinned != rightPinned { return leftPinned }
        let leftRank = rank(ranks, left.id)
        let rightRank = rank(ranks, right.id)
        if leftRank != rightRank { return leftRank < rightRank }
        if left.createdAt != right.createdAt { return left.createdAt < right.createdAt }
        return left.id < right.id
    }

    private static func rankMap(_ order: [String]) -> [String: Int] {
        var map: [String: Int] = [:]
        for (rank, id) in order.enumerated() { map[id] = rank }
        return map
    }

    private static func rank(_ ranks: [String: Int], _ id: String) -> Int {
        ranks[id] ?? unranked
    }

    /// A group whose project the daemon did not send. Its id is opaque
    /// (`p_6e5a8d7b38f5`), so the name has to come from where its sessions are
    /// actually running — never from the id, which would put a hash on screen.
    private static func syntheticProject(id: String, members: [SessionRecord]) -> Project {
        let root = members.first?.cwd ?? id
        return Project(id: id, name: (root as NSString).lastPathComponent, root: root)
    }
}
