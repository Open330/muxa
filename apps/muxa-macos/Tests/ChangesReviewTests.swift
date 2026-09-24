import Foundation
import Testing
@testable import Muxa

// MARK: - Fixtures

/// NUL-joined records, the way `-z` output arrives.
private func z(_ records: String...) -> Data {
    Data((records.joined(separator: "\0") + "\0").utf8)
}

private let sampleDiff = """
diff --git a/src/app.rs b/src/app.rs
index 1111111..2222222 100644
--- a/src/app.rs
+++ b/src/app.rs
@@ -10,3 +10,4 @@ fn main() {
 let a = 1;
-let b = 2;
+let b = 3;
+let c = 4;
 let d = 5;
@@ -40,2 +41,2 @@ impl App
-    old();
+    new();
 }
\\ No newline at end of file

"""

private func line(_ id: Int, _ kind: DiffLineKind, old: Int?, new: Int?, _ text: String) -> DiffLine {
    DiffLine(id: id, kind: kind, oldNumber: old, newNumber: new, text: text)
}

// MARK: - Porcelain v2

@Test func statusParsesBranchHeadersAndEveryEntryKind() {
    let data = z(
        "# branch.oid 0123456789abcdef",
        "# branch.head feat/review",
        "# branch.upstream origin/feat/review",
        "# branch.ab +2 -1",
        "1 .M N... 100644 100644 100644 aaaa bbbb src/app.rs",
        "1 M. N... 100644 100644 100644 aaaa bbbb README.md",
        "1 MM N... 100644 100644 100644 aaaa bbbb both.txt",
        "2 R. N... 100644 100644 100644 aaaa bbbb R100 new name.rs",
        "old name.rs",
        "u UU N... 100644 100644 100644 100644 aaaa bbbb cccc conflict.rs",
        "? 새 파일.md"
    )
    let snapshot = GitStatusParser.parse(data)

    #expect(snapshot.branch.head == "feat/review")
    #expect(snapshot.branch.upstream == "origin/feat/review")
    #expect(snapshot.branch.ahead == 2)
    #expect(snapshot.branch.behind == 1)
    #expect(!snapshot.branch.isDetached)

    let summary = snapshot.files.map { "\($0.group.rawValue) \($0.kind.rawValue) \($0.path)" }
    #expect(summary == [
        "unstaged M src/app.rs",
        "staged M README.md",
        "staged M both.txt",
        "unstaged M both.txt",
        "staged R new name.rs",
        "conflicts U conflict.rs",
        "untracked ? 새 파일.md",
    ])
    #expect(snapshot.files[4].origPath == "old name.rs")
    #expect(snapshot.omittedCount == 0)
}

@Test func statusHandlesDetachedAndInitialHeads() {
    let detached = GitStatusParser.parse(z("# branch.oid 0123456789abcdef", "# branch.head (detached)"))
    #expect(detached.branch.isDetached)
    #expect(detached.branch.displayName == "01234567")

    let initial = GitStatusParser.parse(z("# branch.oid (initial)", "# branch.head main", "? a.txt"))
    #expect(initial.branch.isInitial)
    #expect(initial.branch.displayName == "main")
    #expect(initial.branch.upstream == nil)
    #expect(initial.files.count == 1)
}

@Test func statusCapsTheListAndCountsTheRest() {
    let records = (0..<5).map { "? file\($0).txt" }
    let data = Data((records.joined(separator: "\0") + "\0").utf8)
    let snapshot = GitStatusParser.parse(data, limit: 3)
    #expect(snapshot.files.map(\.path) == ["file0.txt", "file1.txt", "file2.txt"])
    #expect(snapshot.omittedCount == 2)
}

// MARK: - numstat / name-status

@Test func numstatParsesPlainBinaryAndRenamedEntries() {
    let data = Data("12\t3\tsrc/app.rs\0-\t-\tlogo.png\04\t0\t\0old.rs\0new.rs\0".utf8)
    let counts = GitNumstatParser.parse(data)
    #expect(counts["src/app.rs"] == LineCounts(added: 12, deleted: 3))
    #expect(counts["logo.png"]?.isBinary == true)
    #expect(counts["new.rs"] == LineCounts(added: 4, deleted: 0))
    #expect(counts["old.rs"] == nil)
}

@Test func nameStatusParsesRenamesForBranchMode() {
    let files = GitNameStatusParser.parse(z("M", "a.rs", "R087", "old.rs", "new.rs", "A", "b.rs", "D", "c.rs"))
    #expect(files.map { "\($0.kind.rawValue) \($0.path)" } == ["M a.rs", "R new.rs", "A b.rs", "D c.rs"])
    #expect(files[1].origPath == "old.rs")
    #expect(files.allSatisfy { $0.group == .branch })
}

// MARK: - Unified diff

@Test func unifiedDiffNumbersLinesAcrossHunks() throws {
    let file = try #require(UnifiedDiffParser.parse(sampleDiff).first)
    #expect(file.path == "src/app.rs")
    #expect(file.hunks.count == 2)
    #expect(!file.truncated)

    let first = file.hunks[0]
    #expect(first.section == "fn main() {")
    #expect(first.lines.map(\.kind) == [.context, .removed, .added, .added, .context])
    #expect(first.lines.map(\.oldNumber) == [10, 11, nil, nil, 12])
    #expect(first.lines.map(\.newNumber) == [10, nil, 11, 12, 13])

    let second = file.hunks[1]
    #expect(second.lines.map(\.kind) == [.removed, .added, .context, .noNewline])
    #expect(second.lines.map(\.oldNumber) == [40, nil, 41, nil])
    #expect(second.lines.map(\.newNumber) == [nil, 41, 42, nil])
    // Ids run through the whole file, so a selection is unambiguous.
    #expect(file.hunks.flatMap(\.lines).map(\.id) == Array(0..<9))
}

@Test func unifiedDiffReadsNewDeletedBinaryAndModeOnlyFiles() {
    let text = """
    diff --git a/new.rs b/new.rs
    new file mode 100644
    --- /dev/null
    +++ b/new.rs
    @@ -0,0 +1,2 @@
    +one
    +two
    diff --git a/gone.rs b/gone.rs
    deleted file mode 100644
    --- a/gone.rs
    +++ /dev/null
    @@ -1 +0,0 @@
    -bye
    diff --git a/logo.png b/logo.png
    Binary files a/logo.png and b/logo.png differ
    diff --git a/run.sh b/run.sh
    old mode 100644
    new mode 100755

    """
    let files = UnifiedDiffParser.parse(text)
    #expect(files.count == 4)
    #expect(files[0].isNew && files[0].path == "new.rs")
    #expect(files[0].hunks[0].lines.map(\.newNumber) == [1, 2])
    #expect(files[1].isDeleted && files[1].path == "gone.rs")
    #expect(files[1].hunks[0].lines.map(\.oldNumber) == [1])
    #expect(files[2].isBinary && files[2].hunks.isEmpty)
    #expect(files[3].modeChange == "100644 → 100755")
    #expect(files[3].hunks.isEmpty)
}

@Test func unifiedDiffKeepsContentThatLooksLikeHeaders() throws {
    // A removed "-- x" line reads as "--- x"; the hunk counters keep it
    // content rather than a new file header.
    let text = """
    diff --git a/a.sql b/a.sql
    --- a/a.sql
    +++ b/a.sql
    @@ -1,2 +1,2 @@
    --- comment
    +++ other
     keep

    """
    let file = try #require(UnifiedDiffParser.parse(text).first)
    #expect(file.path == "a.sql")
    #expect(file.hunks[0].lines.map(\.kind) == [.removed, .added, .context])
    #expect(file.hunks[0].lines[0].text == "-- comment")
}

@Test func unifiedDiffMarksATruncatedTail() throws {
    let text = """
    diff --git a/big.txt b/big.txt
    --- a/big.txt
    +++ b/big.txt
    @@ -1,10 +1,10 @@
    -a
    +b

    """
    let file = try #require(UnifiedDiffParser.parse(text, truncated: true).first)
    #expect(file.truncated)
    #expect(file.hunks[0].lines.count == 2)
}

@Test func unifiedDiffParsesRenamesAndQuotedPaths() throws {
    let text = """
    diff --git a/old.rs b/new.rs
    similarity index 90%
    rename from old.rs
    rename to new.rs
    diff --git "a/tab\\there.txt" "b/tab\\there.txt"
    --- "a/tab\\there.txt"
    +++ "b/tab\\there.txt"
    @@ -1 +1 @@
    -x
    +y

    """
    let files = UnifiedDiffParser.parse(text)
    #expect(files[0].oldPath == "old.rs")
    #expect(files[0].newPath == "new.rs")
    #expect(files[1].path == "tab\there.txt")
    #expect(UnifiedDiffParser.unquote("\"\\355\\225\\234.txt\"") == "한.txt")
}

@Test func hunkHeaderDefaultsAMissingCountToOne() throws {
    let hunk = try #require(UnifiedDiffParser.parseHunkHeader("@@ -3 +4,0 @@", id: 0))
    #expect(hunk.oldStart == 3 && hunk.oldCount == 1)
    #expect(hunk.newStart == 4 && hunk.newCount == 0)
    #expect(hunk.section.isEmpty)
    #expect(UnifiedDiffParser.parseHunkHeader("@@ nonsense", id: 0) == nil)
}

// MARK: - Refresh rules

@Test func changesRefreshWhenTheAgentSettles() {
    #expect(ChangesRefreshRules.settled(from: "working", to: "idle"))
    #expect(ChangesRefreshRules.settled(from: "working", to: "waiting_input"))
    #expect(ChangesRefreshRules.settled(from: "starting", to: "error"))
    #expect(!ChangesRefreshRules.settled(from: "idle", to: "idle"))
    #expect(!ChangesRefreshRules.settled(from: "idle", to: "working"))
    #expect(!ChangesRefreshRules.settled(from: nil, to: "idle"))
    #expect(!ChangesRefreshRules.settled(from: "working", to: nil))
    #expect(ChangesRefreshRules.isLarge(lines: 3_001, bytes: 10))
    #expect(ChangesRefreshRules.isLarge(lines: 10, bytes: 2 * 1024 * 1024))
    #expect(!ChangesRefreshRules.isLarge(lines: 3_000, bytes: 1024))
}

// MARK: - Selection and anchoring

@Test func selectionExtendsOnlyWithinOneHunk() throws {
    let file = try #require(UnifiedDiffParser.parse(sampleDiff).first)
    let first = try #require(DiffSelection.select(line: 1, in: file, current: nil, extend: false))
    #expect(first.range == 1...1)

    let extended = try #require(DiffSelection.select(line: 3, in: file, current: first, extend: true))
    #expect(extended.range == 1...3)
    #expect(extended.lines(in: file).map(\.text) == ["let b = 2;", "let b = 3;", "let c = 4;"])

    // Shift-click in another hunk starts over there.
    let elsewhere = try #require(DiffSelection.select(line: 6, in: file, current: extended, extend: true))
    #expect(elsewhere.hunkID == 1)
    #expect(elsewhere.range == 6...6)

    // Extending backwards keeps the anchor.
    let backwards = try #require(DiffSelection.select(line: 0, in: file, current: first, extend: true))
    #expect(backwards.range == 0...1)
}

@Test func commentsAnchorOnNewNumbersUnlessOnlyLinesWereRemoved() throws {
    let mixed = try #require(ReviewComment(path: "a.rs", lines: [
        line(1, .removed, old: 11, new: nil, "let b = 2;"),
        line(2, .added, old: nil, new: 11, "let b = 3;"),
        line(3, .added, old: nil, new: 12, "let c = 4;"),
    ], body: "why"))
    #expect(mixed.side == .new)
    #expect(mixed.location == "a.rs:11-12")

    let removed = try #require(ReviewComment(path: "a.rs", lines: [
        line(1, .removed, old: 40, new: nil, "old();"),
    ], body: "keep this"))
    #expect(removed.side == .old)
    #expect(removed.location == "a.rs:40")

    #expect(ReviewComment(path: "a.rs", lines: [line(1, .noNewline, old: nil, new: nil, "\\ No newline")], body: "x") == nil)
}

@Test func outdatedDetectionFollowsTheQuoteNotTheLineNumber() throws {
    let file = try #require(UnifiedDiffParser.parse(sampleDiff).first)
    let selection = try #require(DiffSelection.select(line: 2, in: file, current: nil, extend: false))
    let comment = try #require(ReviewComment(path: "src/app.rs", lines: selection.lines(in: file), body: "b"))
    #expect(comment.anchorLineID(in: file) == 2)

    // The same change moved down by an inserted hunk: still found.
    let shifted = sampleDiff
        .replacingOccurrences(of: "@@ -10,3 +10,4 @@", with: "@@ -20,3 +20,4 @@")
    let moved = try #require(UnifiedDiffParser.parse(shifted).first)
    #expect(comment.anchorLineID(in: moved) == 2)

    // The line is gone from the diff: outdated.
    let rewritten = sampleDiff.replacingOccurrences(of: "+let b = 3;", with: "+let b = 30;")
    let changed = try #require(UnifiedDiffParser.parse(rewritten).first)
    #expect(comment.anchorLineID(in: changed) == nil)
}

// MARK: - Draft store

@MainActor
@Test func draftStoreKeysByHostAndRootAndTracksOutdatedComments() throws {
    let store = ReviewDraftStore()
    store.register(hostAlias: "local", path: "/repo/sub", root: "/repo")
    let key = try #require(store.key(hostAlias: "local", path: "/repo/sub"))
    #expect(key == ReviewDraftKey(hostAlias: "local", root: "/repo"))
    #expect(store.key(hostAlias: "other", path: "/repo/sub") == nil)

    let file = try #require(UnifiedDiffParser.parse(sampleDiff).first)
    let comment = try #require(ReviewComment(path: "src/app.rs", lines: [file.hunks[0].lines[2]], body: "first"))
    store.add(comment, to: key)
    #expect(store.commentCount(hostAlias: "local", path: "/repo/sub") == 1)

    store.update(comment.id, body: "edited", in: key)
    #expect(store.draft(for: key).comments.first?.body == "edited")

    let rewritten = sampleDiff.replacingOccurrences(of: "+let b = 3;", with: "+let b = 30;")
    store.refreshOutdated(in: key, file: try #require(UnifiedDiffParser.parse(rewritten).first))
    #expect(store.draft(for: key).comments.first?.outdated == true)
    store.refreshOutdated(in: key, file: file)
    #expect(store.draft(for: key).comments.first?.outdated == false)
    store.markMissing(in: key, changedPaths: ["README.md"])
    #expect(store.draft(for: key).comments.first?.outdated == true)

    store.setNote("overall", for: key)
    store.remove(comment.id, from: key)
    #expect(!store.draft(for: key).isEmpty)
    store.clear(key)
    #expect(store.draft(for: key).isEmpty)
}

// MARK: - Prompt

@Test func reviewPromptMatchesTheGoldenFormat() throws {
    let file = try #require(UnifiedDiffParser.parse(sampleDiff).first)
    let first = try #require(ReviewComment(path: "src/app.rs", lines: Array(file.hunks[0].lines[1...2]), body: "Why 3?"))
    var second = try #require(ReviewComment(path: "README.md", lines: [line(0, .added, old: nil, new: 7, "Usage")], body: " Document the flag. "))
    second.outdated = true
    let removed = try #require(ReviewComment(path: "src/app.rs", lines: [file.hunks[1].lines[0]], body: "Keep old()."))
    let draft = ReviewDraft(note: "Looks close.", comments: [first, removed, second])

    let prompt = ReviewPromptFormatter.format(draft: draft, repositoryName: "muxa", branch: "feat/x", base: nil)
    #expect(prompt == """
    Review of your changes in muxa (feat/x, 3 comments). Please address each one, then reply with a short summary of what you changed.

    Looks close.

    1. README.md:7 (may be outdated)
    ```diff
    +Usage
    ```
    Document the flag.

    2. src/app.rs:11
    ```diff
    -let b = 2;
    +let b = 3;
    ```
    Why 3?

    3. src/app.rs:40 (line numbers before the change)
    ```diff
    -    old();
    ```
    Keep old().
    """)
}

@Test func reviewPromptTrimsLongQuotesAndEscalatesFences() throws {
    let lines = (1...20).map { line($0, .added, old: nil, new: $0, $0 == 2 ? "```" : "line \($0)") }
    let comment = try #require(ReviewComment(path: "doc.md", lines: lines, body: "Too long"))
    let draft = ReviewDraft(comments: [comment])
    let base = BranchBase(ref: "origin/main", sha: "abcdef0123456789")
    let prompt = ReviewPromptFormatter.format(draft: draft, repositoryName: "r", branch: "b", base: base)

    #expect(prompt.hasPrefix("Review of your changes in r (b, changes since origin/main @ abcdef01, 1 comment)."))
    #expect(prompt.contains("````diff\n+line 1\n+```\n+line 3\n+line 4\n+line 5\n+line 6\n…\n+line 18\n+line 19\n+line 20\n````"))
    #expect(ReviewPromptFormatter.quoteLines(comment.quote).count == 10)
}

@Test func reviewSendNeedsContentAControllableHostAndAReasonableSize() {
    let local = MuxaFleetHostIdentity(alias: "local", local: true, state: "online", mode: "observe")
    let observe = MuxaFleetHostIdentity(alias: "remote", local: false, state: "online", mode: "observe")
    let comment = ReviewComment(path: "a", lines: [line(0, .added, old: nil, new: 1, "x")], body: "y")!
    let draft = ReviewDraft(comments: [comment])

    #expect(ReviewSendRules.canSend(draft: draft, previewEdited: false, preview: "p", sending: false, hosts: [local]))
    #expect(!ReviewSendRules.canSend(draft: ReviewDraft(), previewEdited: false, preview: "p", sending: false, hosts: [local]))
    #expect(ReviewSendRules.canSend(draft: ReviewDraft(), previewEdited: true, preview: "p", sending: false, hosts: [local]))
    #expect(!ReviewSendRules.canSend(draft: draft, previewEdited: false, preview: "p", sending: false, hosts: [observe]))
    #expect(!ReviewSendRules.canSend(draft: draft, previewEdited: false, preview: "p", sending: true, hosts: [local]))
    let huge = String(repeating: "x", count: ReviewSendLimits.maxBytes + 1)
    #expect(!ReviewSendRules.canSend(draft: draft, previewEdited: false, preview: huge, sending: false, hosts: [local]))
}

@Test func showChangesRequestsMatchOnlyTheirEditor() throws {
    let host = MuxaFleetHostIdentity(alias: "local", local: true, state: "online", mode: "control")
    let paneInfo = try JSONDecoder().decode(MuxaPaneInfo.self, from: Data("""
    {"pane_id":"%1","session_id":"$1","session":"s","window_id":"@1","window_name":"w","window_index":"0",
     "pane_index":"0","current_command":"zsh","title":"","current_path":"/tmp/work"}
    """.utf8))
    let pane = MuxaWatchPane(host: host, pane: paneInfo, agent: nil)
    let other = MuxaWatchPaneIdentity(hostAlias: "local", socket: pane.id.socket, paneID: "%2")

    #expect(ChangesShowRequest.matches(MuxaSidebarSelection.pane(pane.id), pane: pane, watchSelection: nil))
    #expect(!ChangesShowRequest.matches(MuxaSidebarSelection.pane(other), pane: pane, watchSelection: nil))
    #expect(ChangesShowRequest.matches(MuxaSidebarSelection.watch, pane: pane, watchSelection: pane.id))
    #expect(!ChangesShowRequest.matches(MuxaSidebarSelection.watch, pane: pane, watchSelection: other))
    #expect(!ChangesShowRequest.matches(nil, pane: pane, watchSelection: pane.id))
    #expect(ChangesSource(pane: pane) == ChangesSource(hostAlias: "local", isLocal: true, path: "/tmp/work"))
}

// MARK: - Running git for real

private func gitAvailable() -> GitRunner? { GitRunner.shared }

@Test func boundedProcessTimesOutAndCapsOutput() async {
    let slow = await BoundedProcess.run(
        executable: URL(fileURLWithPath: "/bin/sleep"), arguments: ["5"], limit: 1024, timeout: 0.3
    )
    #expect(slow.timedOut)
    #expect(slow.status != 0)

    let chatty = await BoundedProcess.run(
        executable: URL(fileURLWithPath: "/usr/bin/yes"), arguments: [], limit: 4096, timeout: 0.5
    )
    #expect(chatty.truncated)
    #expect(chatty.stdout.count == 4096)
}

@Test(.enabled(if: GitRunner.shared != nil))
func loaderReadsARealRepository() async throws {
    let git = try #require(gitAvailable())
    let root = FileManager.default.temporaryDirectory
        .appendingPathComponent("muxa-changes-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: root) }
    let dir = root.path

    // The operator's global config may sign commits or run hooks; the
    // fixture repository needs neither.
    func run(_ args: String...) async {
        let output = await git.run(
            ["-c", "user.name=t", "-c", "user.email=t@example.com", "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"] + args,
            in: dir, limit: 1 << 20, timeout: 10
        )
        #expect(output.status == 0, "git \(args.joined(separator: " ")): \(output.stderr)")
    }
    await run("init", "-q", "-b", "main")
    try "one\ntwo\nthree\n".write(toFile: "\(dir)/a.txt", atomically: true, encoding: .utf8)
    try "keep\n".write(toFile: "\(dir)/b.txt", atomically: true, encoding: .utf8)
    await run("add", ".")
    await run("commit", "-q", "-m", "init")
    await run("checkout", "-q", "-b", "feature")
    try "one\n2\nthree\nfour\n".write(toFile: "\(dir)/a.txt", atomically: true, encoding: .utf8)
    try "kept\n".write(toFile: "\(dir)/b.txt", atomically: true, encoding: .utf8)
    await run("add", "b.txt")
    try "new\nfile\n".write(toFile: "\(dir)/c.txt", atomically: true, encoding: .utf8)

    let loader = ChangesLoader(git: git)
    let resolved = try await loader.repositoryRoot(for: dir).get()
    #expect(URL(fileURLWithPath: resolved).resolvingSymlinksInPath() == root.resolvingSymlinksInPath())

    let status = try await loader.status(root: resolved).get()
    #expect(status.branch.head == "feature")
    let counted = await loader.uncommittedCounts(root: resolved, files: status.files)
    let summary = counted.map { "\($0.group.rawValue) \($0.path) +\($0.added ?? -1) -\($0.deleted ?? -1)" }.sorted()
    #expect(summary == ["staged b.txt +1 -1", "unstaged a.txt +2 -1", "untracked c.txt +2 -0"])

    let unstaged = try #require(counted.first { $0.group == .unstaged })
    let diff = try await loader.diff(root: resolved, file: unstaged, base: nil).get().file
    #expect(diff.path == "a.txt")
    #expect(diff.hunks.flatMap(\.lines).filter { $0.kind == .added }.map(\.text) == ["2", "four"])

    let untracked = try #require(counted.first { $0.group == .untracked })
    let newFile = try await loader.diff(root: resolved, file: untracked, base: nil).get().file
    #expect(newFile.isNew)
    #expect(newFile.hunks.first?.lines.map(\.text) == ["new", "file"])

    let base = try #require(await loader.branchBase(root: resolved, upstream: nil))
    #expect(base.ref == "main")
    let branch = try await loader.branchFiles(root: resolved, base: base).get()
    #expect(Set(branch.map(\.path)) == ["a.txt", "b.txt"])

    let notRepo = await loader.repositoryRoot(for: "/")
    #expect(notRepo == .failure(.notRepository("/")))
}

// MARK: - Review fixes

/// A file both staged and changed has two diffs; a comment written on the
/// staged one must not go outdated when the unstaged one is shown.
@MainActor
@Test func outdatedCheckOnlyUsesTheDiffTheCommentWasWrittenOn() throws {
    let store = ReviewDraftStore()
    store.register(hostAlias: "local", path: "/repo", root: "/repo")
    let key = try #require(store.key(hostAlias: "local", path: "/repo"))
    let file = try #require(UnifiedDiffParser.parse(sampleDiff).first)
    var comment = try #require(ReviewComment(path: "src/app.rs", lines: [file.hunks[0].lines[2]], body: "staged note"))
    comment.group = .staged
    store.add(comment, to: key)

    let other = sampleDiff.replacingOccurrences(of: "+let b = 3;", with: "+let b = 30;")
    let otherFile = try #require(UnifiedDiffParser.parse(other).first)
    store.refreshOutdated(in: key, file: otherFile, group: .unstaged)
    #expect(store.draft(for: key).comments.first?.outdated == false)
    store.refreshOutdated(in: key, file: otherFile, group: .staged)
    #expect(store.draft(for: key).comments.first?.outdated == true)
}
