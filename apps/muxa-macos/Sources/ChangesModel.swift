import Foundation
import SwiftUI

// MARK: - Source and snapshot

/// Where the Changes module reads a repository from: a pane's or a Work's
/// working directory on one host.
struct ChangesSource: Equatable, Sendable {
    let hostAlias: String
    let isLocal: Bool
    let path: String?
}

enum ChangesCompare: Hashable, Sendable {
    /// HEAD against the index and the working tree.
    case uncommitted
    /// The merge-base with the upstream (or the default branch) against the
    /// working tree: what the agent changed on this branch, committed or not.
    case branch
}

struct BranchBase: Equatable, Sendable {
    /// What the base was resolved from, e.g. `origin/main`.
    let ref: String
    let sha: String

    var shortSHA: String { String(sha.prefix(8)) }
}

enum ChangesFailure: Error, Equatable, Sendable {
    case noPath
    case remote(String)
    case gitMissing
    case notRepository(String)
    case error(String)
}

struct GitChangesSnapshot: Equatable, Sendable {
    let root: String
    var branch: GitBranchInfo
    var files: [ChangedFile]
    var omittedCount: Int
    var compare: ChangesCompare
    var base: BranchBase?
    /// The +/- counts have arrived; until then the list shows "…".
    var countsLoaded: Bool

    var repositoryName: String { (root as NSString).lastPathComponent }

    var groups: [ChangeGroup] {
        ChangeGroup.allCases.filter { group in files.contains { $0.group == group } }
    }

    func files(in group: ChangeGroup) -> [ChangedFile] {
        files.filter { $0.group == group }
    }

    var totalAdded: Int { files.reduce(0) { $0 + ($1.added ?? 0) } }
    var totalDeleted: Int { files.reduce(0) { $0 + ($1.deleted ?? 0) } }
    var fileCount: Int { Set(files.map(\.path)).count + omittedCount }
}

// MARK: - Loading

/// The git commands behind the Changes module. Every call is nonisolated and
/// bounded (time and bytes), so none of it can stall the main actor.
struct ChangesLoader: Sendable {
    let git: GitRunner

    static let statusLimit = 8 * 1024 * 1024
    static let diffLimit = 4 * 1024 * 1024
    private static let diffFlags = [
        "--no-ext-diff", "--no-textconv", "--src-prefix=a/", "--dst-prefix=b/", "-U3",
    ]

    func repositoryRoot(for path: String) async -> Result<String, ChangesFailure> {
        let output = await git.run(["rev-parse", "--show-toplevel"], in: path, limit: 64 * 1024, timeout: 10)
        guard output.status == 0 else {
            if output.launchError == nil, !output.timedOut, output.stderr.contains("not a git repository") {
                return .failure(.notRepository(path))
            }
            return .failure(.error(output.failureReason))
        }
        let root = output.stdoutText.trimmingCharacters(in: .whitespacesAndNewlines)
        return root.isEmpty ? .failure(.notRepository(path)) : .success(root)
    }

    func status(root: String) async -> Result<GitStatusSnapshot, ChangesFailure> {
        let output = await git.run(
            ["status", "--porcelain=v2", "--branch", "-z", "--untracked-files=all", "--ignore-submodules=dirty"],
            in: root, limit: Self.statusLimit, timeout: 15
        )
        guard output.status == 0 else { return .failure(.error(output.failureReason)) }
        return .success(GitStatusParser.parse(output.stdout))
    }

    /// The upstream when there is one, else `origin/HEAD`, else a local
    /// `main` or `master`; nil when none resolves or HEAD is unborn.
    func branchBase(root: String, upstream: String?) async -> BranchBase? {
        var candidates: [String] = []
        if let upstream { candidates.append(upstream) }
        let originHead = await git.run(
            ["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"],
            in: root, limit: 4096, timeout: 5
        )
        if originHead.status == 0 {
            let ref = originHead.stdoutText.trimmingCharacters(in: .whitespacesAndNewlines)
            if !ref.isEmpty { candidates.append(ref) }
        }
        candidates += ["main", "master"]
        for ref in candidates {
            let mergeBase = await git.run(["merge-base", "HEAD", ref], in: root, limit: 4096, timeout: 10)
            let sha = mergeBase.stdoutText.trimmingCharacters(in: .whitespacesAndNewlines)
            if mergeBase.status == 0, !sha.isEmpty { return BranchBase(ref: ref, sha: sha) }
        }
        return nil
    }

    /// The status entries with their +/- counts filled in.
    func uncommittedCounts(root: String, files: [ChangedFile]) async -> [ChangedFile] {
        async let unstaged = git.run(["diff", "--numstat", "-z", "--no-ext-diff", "--no-textconv"], in: root, limit: Self.statusLimit, timeout: 20)
        async let staged = git.run(["diff", "--cached", "--numstat", "-z", "-M", "--no-ext-diff", "--no-textconv"], in: root, limit: Self.statusLimit, timeout: 20)
        let unstagedCounts = GitNumstatParser.parse(await unstaged.stdout)
        let stagedCounts = GitNumstatParser.parse(await staged.stdout)
        return files.map { file in
            var file = file
            let counts: LineCounts? = switch file.group {
            case .staged: stagedCounts[file.path]
            case .unstaged, .conflicts: unstagedCounts[file.path]
            case .untracked: Self.untrackedCounts(root: root, path: file.path)
            case .branch: nil
            }
            if let counts { file.apply(counts) }
            return file
        }
    }

    func branchFiles(root: String, base: BranchBase) async -> Result<[ChangedFile], ChangesFailure> {
        async let names = git.run(["diff", "--name-status", "-z", "-M", "--no-ext-diff", base.sha], in: root, limit: Self.statusLimit, timeout: 20)
        async let numbers = git.run(["diff", "--numstat", "-z", "-M", "--no-ext-diff", "--no-textconv", base.sha], in: root, limit: Self.statusLimit, timeout: 20)
        let nameOutput = await names
        guard nameOutput.status == 0 else { return .failure(.error(nameOutput.failureReason)) }
        let counts = GitNumstatParser.parse(await numbers.stdout)
        return .success(GitNameStatusParser.parse(nameOutput.stdout).map { file in
            var file = file
            if let value = counts[file.path] { file.apply(value) }
            return file
        })
    }

    struct LoadedDiff: Sendable {
        let file: DiffFile
        let bytes: Int
    }

    func diff(root: String, file: ChangedFile, base: BranchBase?) async -> Result<LoadedDiff, ChangesFailure> {
        var paths = [file.path]
        if let origPath = file.origPath { paths.insert(origPath, at: 0) }
        let args: [String]
        var noIndex = false
        switch file.group {
        case .staged:
            args = ["diff", "--cached", "-M"] + Self.diffFlags + ["--"] + paths
        case .unstaged, .conflicts:
            args = ["diff"] + Self.diffFlags + ["--", file.path]
        case .untracked:
            noIndex = true
            args = ["diff", "--no-index"] + Self.diffFlags + ["--", "/dev/null", file.path]
        case .branch:
            guard let base else { return .failure(.error(String(localized: "No branch base to compare with"))) }
            args = ["diff", "-M"] + Self.diffFlags + [base.sha, "--"] + paths
        }
        let output = await git.run(args, in: root, limit: Self.diffLimit, timeout: 10)
        // `--no-index` exits 1 whenever the files differ, which they always do.
        let succeeded = output.status == 0 || (noIndex && output.status == 1 && output.launchError == nil && !output.timedOut)
        guard succeeded else { return .failure(.error(output.failureReason)) }
        let parsed = UnifiedDiffParser.parse(output.stdoutText, truncated: output.truncated)
        var diff = parsed.first(where: { $0.path == file.path }) ?? parsed.first ?? DiffFile()
        if diff.newPath == nil, !diff.isDeleted { diff.newPath = file.path }
        return .success(LoadedDiff(file: diff, bytes: output.stdout.count))
    }

    /// An untracked file's line count, read straight from disk: small text
    /// files only, since git has no numstat for them.
    static func untrackedCounts(root: String, path: String) -> LineCounts? {
        let url = URL(fileURLWithPath: root).appendingPathComponent(path)
        guard let attributes = try? FileManager.default.attributesOfItem(atPath: url.path),
              attributes[.type] as? FileAttributeType == .typeRegular,
              let size = attributes[.size] as? Int, size <= 1024 * 1024,
              let data = try? Data(contentsOf: url) else { return nil }
        if data.prefix(8000).contains(0) { return LineCounts(added: nil, deleted: nil) }
        var lines = data.reduce(0) { $0 + ($1 == 0x0A ? 1 : 0) }
        if let last = data.last, last != 0x0A { lines += 1 }
        return LineCounts(added: lines, deleted: 0)
    }
}

extension ChangedFile {
    mutating func apply(_ counts: LineCounts) {
        added = counts.added
        deleted = counts.deleted
        isBinary = counts.isBinary
    }
}

// MARK: - Rules

enum ChangesRefreshRules {
    /// Refresh when the agent settles: it was working and now is not.
    static func settled(from old: String?, to new: String?) -> Bool {
        guard let old, let new else { return false }
        return MuxaAttention.activeStates.contains(old) && !MuxaAttention.activeStates.contains(new)
    }

    static let largeDiffLines = 3_000
    static let largeDiffBytes = 1024 * 1024

    static func isLarge(lines: Int, bytes: Int) -> Bool {
        lines > largeDiffLines || bytes > largeDiffBytes
    }
}

/// A line range picked in the diff, always inside one hunk so the quoted
/// block stays contiguous.
struct DiffSelection: Equatable, Sendable {
    let hunkID: Int
    var anchor: Int
    var head: Int

    var range: ClosedRange<Int> { min(anchor, head)...max(anchor, head) }

    /// Click selects a line; shift-click extends within the same hunk and
    /// starts over anywhere else. Combined (conflict) hunks can't be picked.
    static func select(line: Int, in file: DiffFile, current: DiffSelection?, extend: Bool) -> DiffSelection? {
        guard let hunk = file.hunk(containing: line), !hunk.isCombined else { return nil }
        if extend, var current, current.hunkID == hunk.id {
            current.head = line
            return current
        }
        return DiffSelection(hunkID: hunk.id, anchor: line, head: line)
    }

    func lines(in file: DiffFile) -> [DiffLine] {
        guard let hunk = file.hunks.first(where: { $0.id == hunkID }) else { return [] }
        return hunk.lines.filter { range.contains($0.id) }
    }
}

// MARK: - Review draft

enum ReviewSide: Sendable {
    /// Line numbers in the changed (new) file.
    case new
    /// Only removed lines: numbers in the file before the change.
    case old
}

struct ReviewQuoteLine: Equatable, Sendable {
    let kind: DiffLineKind
    let text: String
}

struct ReviewComment: Identifiable, Equatable, Sendable {
    let id: UUID
    let path: String
    let side: ReviewSide
    let startLine: Int
    let endLine: Int
    /// A snapshot of the selected lines, so the comment keeps its context
    /// when the diff moves on.
    let quote: [ReviewQuoteLine]
    var body: String
    /// Its quote is no longer in the current diff.
    var outdated = false

    static let quoteLimit = 40

    /// Anchors a comment on the selected lines: new-file numbers when the
    /// range has any, old-file numbers for a removal-only range.
    init?(id: UUID = UUID(), path: String, lines: [DiffLine], body: String) {
        let lines = lines.filter { $0.kind != .noNewline }
        guard !lines.isEmpty else { return nil }
        let newNumbers = lines.compactMap(\.newNumber)
        let numbers = newNumbers.isEmpty ? lines.compactMap(\.oldNumber) : newNumbers
        guard let first = numbers.first, let last = numbers.last else { return nil }
        self.id = id
        self.path = path
        self.side = newNumbers.isEmpty ? .old : .new
        self.startLine = first
        self.endLine = last
        self.quote = lines.prefix(Self.quoteLimit).map { ReviewQuoteLine(kind: $0.kind, text: $0.text) }
        self.body = body
    }

    var location: String {
        let lines = startLine == endLine ? "\(startLine)" : "\(startLine)-\(endLine)"
        return "\(path):\(lines)"
    }

    /// Where the quote sits in `file`: the id of its last line, preferring
    /// the match at the original line number when the quote repeats.
    func anchorLineID(in file: DiffFile) -> Int? {
        guard file.path == path, !quote.isEmpty else { return nil }
        var fallback: Int?
        for hunk in file.hunks where !hunk.isCombined {
            let lines = hunk.lines.filter { $0.kind != .noNewline }
            guard lines.count >= quote.count else { continue }
            for start in 0...(lines.count - quote.count) {
                let window = lines[start..<(start + quote.count)]
                guard zip(window, quote).allSatisfy({ $0.kind == $1.kind && $0.text == $1.text }) else { continue }
                let number = side == .new ? window.compactMap(\.newNumber).first : window.compactMap(\.oldNumber).first
                if number == startLine { return window.last?.id }
                if fallback == nil { fallback = window.last?.id }
            }
        }
        return fallback
    }
}

struct ReviewDraft: Equatable, Sendable {
    var note = ""
    var comments: [ReviewComment] = []

    var isEmpty: Bool {
        comments.isEmpty && note.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }
}

struct ReviewDraftKey: Hashable, Sendable {
    let hostAlias: String
    let root: String
}

/// Review drafts in memory, one per host and repository, so a draft
/// survives switching modules and editors and is shared by every pane in
/// the same checkout. Drafts are not kept across launches.
@MainActor
final class ReviewDraftStore: ObservableObject {
    static let shared = ReviewDraftStore()

    @Published private(set) var drafts: [ReviewDraftKey: ReviewDraft] = [:]
    /// The repository root each (host, directory) resolved to, so a pane can
    /// badge its Changes tab before it has run git itself.
    @Published private(set) var roots: [String: String] = [:]

    private static func pathKey(_ hostAlias: String, _ path: String) -> String {
        "\(hostAlias)\u{0}\(path)"
    }

    func register(hostAlias: String, path: String, root: String) {
        let key = Self.pathKey(hostAlias, path)
        if roots[key] != root { roots[key] = root }
    }

    func key(hostAlias: String, path: String?) -> ReviewDraftKey? {
        guard let path, let root = roots[Self.pathKey(hostAlias, path)] else { return nil }
        return ReviewDraftKey(hostAlias: hostAlias, root: root)
    }

    func draft(for key: ReviewDraftKey) -> ReviewDraft {
        drafts[key] ?? ReviewDraft()
    }

    func commentCount(hostAlias: String, path: String?) -> Int {
        key(hostAlias: hostAlias, path: path).map { draft(for: $0).comments.count } ?? 0
    }

    func add(_ comment: ReviewComment, to key: ReviewDraftKey) {
        drafts[key, default: ReviewDraft()].comments.append(comment)
    }

    func update(_ id: UUID, body: String, in key: ReviewDraftKey) {
        guard let index = drafts[key]?.comments.firstIndex(where: { $0.id == id }) else { return }
        drafts[key]?.comments[index].body = body
    }

    func remove(_ id: UUID, from key: ReviewDraftKey) {
        drafts[key]?.comments.removeAll { $0.id == id }
    }

    func setNote(_ note: String, for key: ReviewDraftKey) {
        drafts[key, default: ReviewDraft()].note = note
    }

    func clear(_ key: ReviewDraftKey) {
        drafts[key] = nil
    }

    /// Re-checks the comments on `file`'s path against its fresh diff.
    func refreshOutdated(in key: ReviewDraftKey, file: DiffFile) {
        guard var draft = drafts[key] else { return }
        for index in draft.comments.indices where draft.comments[index].path == file.path {
            draft.comments[index].outdated = draft.comments[index].anchorLineID(in: file) == nil
        }
        if draft != drafts[key] { drafts[key] = draft }
    }

    /// Comments on files that no longer show up as changed are outdated.
    func markMissing(in key: ReviewDraftKey, changedPaths: Set<String>) {
        guard var draft = drafts[key] else { return }
        for index in draft.comments.indices where !changedPaths.contains(draft.comments[index].path) {
            draft.comments[index].outdated = true
        }
        if draft != drafts[key] { drafts[key] = draft }
    }
}

// MARK: - Prompt

enum ReviewSendLimits {
    /// Past this the sheet warns: a very long paste is slow through tmux and
    /// can trip an agent's input box.
    static let warnBytes = 16 * 1024
    static let maxBytes = 64 * 1024
}

/// Turns a draft into the one prompt the agent receives. The prompt is
/// written in English whatever the app's language: it is an instruction to
/// the agent, not UI text.
enum ReviewPromptFormatter {
    static let quoteHead = 6
    static let quoteTail = 3
    static let quoteTrimThreshold = 12

    static func format(draft: ReviewDraft, repositoryName: String, branch: String?, base: BranchBase?) -> String {
        let comments = draft.comments.sorted {
            ($0.path, $0.startLine, $0.endLine) < ($1.path, $1.startLine, $1.endLine)
        }
        var context = [String]()
        if let branch { context.append(branch) }
        if let base { context.append("changes since \(base.ref) @ \(base.shortSHA)") }
        context.append(comments.count == 1 ? "1 comment" : "\(comments.count) comments")
        var sections = [
            "Review of your changes in \(repositoryName) (\(context.joined(separator: ", "))). "
                + "Please address each one, then reply with a short summary of what you changed.",
        ]
        let note = draft.note.trimmingCharacters(in: .whitespacesAndNewlines)
        if !note.isEmpty { sections.append(note) }
        for (index, comment) in comments.enumerated() {
            var heading = "\(index + 1). \(comment.location)"
            if comment.side == .old { heading += " (line numbers before the change)" }
            if comment.outdated { heading += " (may be outdated)" }
            let quote = quoteLines(comment.quote)
            let fence = quote.contains { $0.contains("```") } ? "````" : "```"
            var block = [heading, fence + "diff"] + quote + [fence]
            let body = comment.body.trimmingCharacters(in: .whitespacesAndNewlines)
            if !body.isEmpty { block.append(body) }
            sections.append(block.joined(separator: "\n"))
        }
        return sections.joined(separator: "\n\n")
    }

    static func quoteLines(_ quote: [ReviewQuoteLine]) -> [String] {
        let lines = quote.map { line -> String in
            switch line.kind {
            case .added: "+" + line.text
            case .removed: "-" + line.text
            case .context: " " + line.text
            case .noNewline: line.text
            }
        }
        guard lines.count > quoteTrimThreshold else { return lines }
        return Array(lines.prefix(quoteHead)) + ["…"] + Array(lines.suffix(quoteTail))
    }
}

// MARK: - Model

@MainActor
final class ChangesModel: ObservableObject {
    enum DiffState: Equatable {
        case none
        case loading
        case loaded(DiffFile)
        /// Parsed but held back until the operator asks for it.
        case large(DiffFile, lines: Int)
        case failed(String)
    }

    @Published private(set) var source: ChangesSource
    @Published private(set) var failure: ChangesFailure?
    @Published private(set) var snapshot: GitChangesSnapshot?
    @Published private(set) var isRefreshing = false
    /// A refresh failed after an earlier one succeeded: the last good
    /// result stays on screen under this banner.
    @Published private(set) var refreshError: String?
    @Published private(set) var compare: ChangesCompare = .uncommitted
    @Published private(set) var branchBase: BranchBase?
    @Published private(set) var selectedFileID: String?
    @Published private(set) var diff: DiffState = .none
    @Published var collapsedHunks: Set<Int> = []
    @Published var selection: DiffSelection?

    let drafts: ReviewDraftStore
    private var loader: ChangesLoader?
    private var refreshTask: Task<Void, Never>?
    private var refreshGeneration = 0
    private var refreshPending = false
    private var diffTask: Task<Void, Never>?
    private var settleTask: Task<Void, Never>?
    private var forcedLargePaths: Set<String> = []

    init(source: ChangesSource, drafts: ReviewDraftStore = .shared) {
        self.source = source
        self.drafts = drafts
    }

    var draftKey: ReviewDraftKey? {
        snapshot.map { ReviewDraftKey(hostAlias: source.hostAlias, root: $0.root) }
    }

    var selectedFile: ChangedFile? {
        snapshot?.files.first { $0.id == selectedFileID }
    }

    func updateSource(_ source: ChangesSource) {
        guard source != self.source else { return }
        self.source = source
        snapshot = nil
        diff = .none
        restart()
    }

    func setCompare(_ compare: ChangesCompare) {
        guard compare != self.compare else { return }
        guard compare == .uncommitted || branchBase != nil else { return }
        self.compare = compare
        restart()
    }

    func select(fileID: String?) {
        guard fileID != selectedFileID else { return }
        selectedFileID = fileID
        selection = nil
        collapsedHunks = []
        loadDiff(keepCurrent: false)
    }

    func moveSelection(by offset: Int) {
        guard let files = snapshot?.files, !files.isEmpty else { return }
        let index = files.firstIndex { $0.id == selectedFileID } ?? (offset > 0 ? -1 : files.count)
        select(fileID: files[max(0, min(files.count - 1, index + offset))].id)
    }

    func showLargeDiff() {
        guard case .large(let file, _) = diff else { return }
        forcedLargePaths.insert(file.path)
        diff = .loaded(file)
    }

    /// Called with the agent's state on every change; refreshes, debounced,
    /// once the agent settles.
    func agentStateChanged(from old: String?, to new: String?) {
        guard ChangesRefreshRules.settled(from: old, to: new) else { return }
        settleTask?.cancel()
        settleTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: 500_000_000)
            guard !Task.isCancelled else { return }
            self?.refresh()
        }
    }

    /// At most one refresh runs; a request during it queues exactly one more.
    func refresh() {
        guard refreshTask == nil else {
            refreshPending = true
            return
        }
        refreshGeneration += 1
        let generation = refreshGeneration
        refreshTask = Task { [weak self] in
            await self?.performRefresh()
            guard let self, generation == self.refreshGeneration else { return }
            self.refreshTask = nil
            if self.refreshPending {
                self.refreshPending = false
                self.refresh()
            }
        }
    }

    private func restart() {
        refreshTask?.cancel()
        refreshTask = nil
        refreshPending = false
        refresh()
    }

    private func performRefresh() async {
        guard source.isLocal else {
            fail(.remote(source.hostAlias))
            return
        }
        guard let path = source.path, FileManager.default.fileExists(atPath: path) else {
            fail(.noPath)
            return
        }
        isRefreshing = true
        // A refresh cancelled by a restart leaves the spinner to its successor.
        defer { if !Task.isCancelled { isRefreshing = false } }

        if loader == nil {
            // Resolving git may run `xcode-select` once: keep it off the
            // main actor.
            guard let git = await Task.detached(operation: { GitRunner.shared }).value else {
                fail(.gitMissing)
                return
            }
            loader = ChangesLoader(git: git)
        }
        guard let loader else { return }

        let root: String
        switch await loader.repositoryRoot(for: path) {
        case .success(let value): root = value
        case .failure(let failure):
            if case .error(let message) = failure, snapshot != nil {
                refreshError = message
            } else {
                fail(failure)
            }
            return
        }
        guard !Task.isCancelled else { return }
        drafts.register(hostAlias: source.hostAlias, path: path, root: root)

        let status: GitStatusSnapshot
        switch await loader.status(root: root) {
        case .success(let value): status = value
        case .failure(let failure):
            if case .error(let message) = failure, snapshot != nil {
                refreshError = message
            } else {
                fail(failure)
            }
            return
        }
        guard !Task.isCancelled else { return }

        branchBase = await loader.branchBase(root: root, upstream: status.branch.upstream)
        guard !Task.isCancelled else { return }
        if compare == .branch, branchBase == nil { compare = .uncommitted }

        let previous = snapshot?.root == root && snapshot?.compare == compare ? snapshot : nil
        let untracked = status.files.filter { $0.group == .untracked }
        var next: GitChangesSnapshot
        switch compare {
        case .uncommitted:
            // Show the list straight away, carrying over the last counts so
            // rows don't flicker to "…" on every refresh.
            let carried = status.files.map { file -> ChangedFile in
                guard let old = previous?.files.first(where: { $0.id == file.id }) else { return file }
                var file = file
                file.added = old.added
                file.deleted = old.deleted
                file.isBinary = old.isBinary
                return file
            }
            next = GitChangesSnapshot(
                root: root, branch: status.branch, files: carried, omittedCount: status.omittedCount,
                compare: .uncommitted, base: nil, countsLoaded: previous?.countsLoaded ?? false
            )
            publish(next)
            let counted = await loader.uncommittedCounts(root: root, files: status.files)
            guard !Task.isCancelled else { return }
            next.files = counted
            next.countsLoaded = true
        case .branch:
            guard let base = branchBase else { return }
            switch await loader.branchFiles(root: root, base: base) {
            case .success(let files):
                let countedUntracked = await loader.uncommittedCounts(root: root, files: untracked)
                next = GitChangesSnapshot(
                    root: root, branch: status.branch, files: files + countedUntracked,
                    omittedCount: status.omittedCount, compare: .branch, base: base, countsLoaded: true
                )
            case .failure(let failure):
                if case .error(let message) = failure { refreshError = message } else { fail(failure) }
                return
            }
            guard !Task.isCancelled else { return }
        }
        refreshError = nil
        failure = nil
        publish(next)
        if let key = draftKey {
            drafts.markMissing(in: key, changedPaths: Set(next.files.map(\.path)))
        }
        loadDiff(keepCurrent: true)
    }

    private func fail(_ failure: ChangesFailure) {
        self.failure = failure
        isRefreshing = false
        snapshot = nil
        refreshError = nil
        diff = .none
        selectedFileID = nil
        selection = nil
    }

    /// Publishes a snapshot and keeps the file selection across refreshes,
    /// by id, then by path, then the first file.
    private func publish(_ next: GitChangesSnapshot) {
        let previousPath = selectedFile?.path
        snapshot = next
        failure = nil
        if next.files.contains(where: { $0.id == selectedFileID }) { return }
        let byPath = previousPath.flatMap { path in next.files.first { $0.path == path } }
        let fileID = (byPath ?? next.files.first)?.id
        if fileID != selectedFileID {
            selectedFileID = fileID
            selection = nil
            collapsedHunks = []
            diff = .none
        }
    }

    private func loadDiff(keepCurrent: Bool) {
        diffTask?.cancel()
        guard let loader, let snapshot, let file = selectedFile else {
            diff = .none
            return
        }
        let showsSameFile: Bool = switch diff {
        case .loaded(let current), .large(let current, _): current.path == file.path
        default: false
        }
        if !(keepCurrent && showsSameFile) { diff = .loading }
        let root = snapshot.root
        let base = snapshot.base
        let key = draftKey
        diffTask = Task { [weak self] in
            let result = await loader.diff(root: root, file: file, base: base)
            guard !Task.isCancelled, let self, self.selectedFileID == file.id else { return }
            switch result {
            case .success(let loaded):
                if let key { self.drafts.refreshOutdated(in: key, file: loaded.file) }
                let lines = loaded.file.lineCount
                if ChangesRefreshRules.isLarge(lines: lines, bytes: loaded.bytes),
                   !self.forcedLargePaths.contains(loaded.file.path) {
                    self.diff = .large(loaded.file, lines: lines)
                } else {
                    self.diff = .loaded(loaded.file)
                }
                if let selection = self.selection,
                   loaded.file.hunks.first(where: { $0.id == selection.hunkID }) == nil {
                    self.selection = nil
                }
            case .failure(let failure):
                if case .error(let message) = failure {
                    self.diff = .failed(message)
                } else {
                    self.diff = .failed(String(localized: "The diff could not be read"))
                }
            }
        }
    }
}
