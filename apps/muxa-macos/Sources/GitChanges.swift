import Darwin
import Foundation

// The Changes module reads the repository an agent works in with the local
// `git`, the same way the operator would from a terminal. Everything here is
// plain data and parsing so it can run off the main actor and be tested with
// fixture text; the views only ever see the parsed models.

// MARK: - Running git

struct GitOutput: Sendable {
    let status: Int32
    let stdout: Data
    let stderr: String
    /// The output reached its cap and the rest was dropped.
    let truncated: Bool
    let timedOut: Bool
    /// Set when the process could not be started at all.
    let launchError: String?

    var stdoutText: String { String(decoding: stdout, as: UTF8.self) }

    /// The first stderr line, which is where git puts its one-line reason.
    var failureReason: String {
        if let launchError { return launchError }
        if timedOut { return String(localized: "git did not finish in time") }
        let line = stderr.split(whereSeparator: \.isNewline).first.map(String.init)
        return line ?? String(localized: "git exited with status \(status)")
    }
}

/// Runs git with the settings a read-only observer needs: no pager, no
/// colour, no external diff drivers, no prompts, and no optional locks so a
/// refresh never holds `index.lock` while the agent is committing.
struct GitRunner: Sendable {
    let executable: URL

    /// The git to use, resolved once. `/usr/bin/git` is an xcrun shim that
    /// opens the Command Line Tools installer when they are missing, so it
    /// only counts when `xcode-select -p` says a developer directory exists.
    static let shared: GitRunner? = {
        let fileManager = FileManager.default
        for path in ["/opt/homebrew/bin/git", "/usr/local/bin/git"] where fileManager.isExecutableFile(atPath: path) {
            return GitRunner(executable: URL(fileURLWithPath: path))
        }
        let shim = "/usr/bin/git"
        guard fileManager.isExecutableFile(atPath: shim), developerToolsInstalled() else { return nil }
        return GitRunner(executable: URL(fileURLWithPath: shim))
    }()

    private static func developerToolsInstalled() -> Bool {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/xcode-select")
        process.arguments = ["-p"]
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        do {
            try process.run()
            process.waitUntilExit()
            return process.terminationStatus == 0
        } catch {
            return false
        }
    }

    static func arguments(_ args: [String], in directory: String) -> [String] {
        [
            "--no-pager", "-C", directory,
            "-c", "core.quotepath=off",
            // `color.ui` alone loses to a more specific `color.diff=always`
            // in the operator's config, and escape codes break the parsers.
            "-c", "color.ui=false",
            "-c", "color.diff=false",
            "-c", "color.status=false",
            "-c", "core.fsmonitor=false",
            "-c", "diff.mnemonicPrefix=false",
            "-c", "diff.noprefix=false",
        ] + args
    }

    static var environment: [String: String] {
        var environment = ProcessInfo.processInfo.environment
        // A variable inherited from whoever launched the app must not point
        // git at some other repository.
        for key in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY", "GIT_COMMON_DIR"] {
            environment.removeValue(forKey: key)
        }
        environment["GIT_OPTIONAL_LOCKS"] = "0"
        environment["GIT_TERMINAL_PROMPT"] = "0"
        environment["GIT_PAGER"] = "cat"
        environment["LC_ALL"] = "C"
        return environment
    }

    func run(_ args: [String], in directory: String, limit: Int, timeout: TimeInterval) async -> GitOutput {
        await BoundedProcess.run(
            executable: executable,
            arguments: Self.arguments(args, in: directory),
            environment: Self.environment,
            limit: limit,
            timeout: timeout
        )
    }
}

/// One child process with a byte cap on its output, a timeout, and
/// cancellation. Output past the cap is read and dropped rather than left in
/// the pipe, so a chatty child can never block on a full pipe.
enum BoundedProcess {
    static func run(
        executable: URL,
        arguments: [String],
        environment: [String: String]? = nil,
        limit: Int,
        timeout: TimeInterval
    ) async -> GitOutput {
        let box = BoundedProcessBox(limit: limit)
        return await withTaskCancellationHandler {
            await withCheckedContinuation { continuation in
                box.start(
                    executable: executable,
                    arguments: arguments,
                    environment: environment,
                    timeout: timeout
                ) { continuation.resume(returning: $0) }
            }
        } onCancel: {
            box.terminate()
        }
    }
}

private final class BoundedProcessBox: @unchecked Sendable {
    private let lock = NSLock()
    private let limit: Int
    private let process = Process()
    private var stdout = Data()
    private var stderr = Data()
    private var truncated = false
    private var timedOut = false
    private var cancelled = false
    private var stdoutClosed = false
    private var stderrClosed = false
    private var exited = false
    private var completion: (@Sendable (GitOutput) -> Void)?

    init(limit: Int) {
        self.limit = limit
    }

    func start(
        executable: URL,
        arguments: [String],
        environment: [String: String]?,
        timeout: TimeInterval,
        completion: @escaping @Sendable (GitOutput) -> Void
    ) {
        process.executableURL = executable
        process.arguments = arguments
        if let environment { process.environment = environment }
        let out = Pipe()
        let err = Pipe()
        process.standardOutput = out
        process.standardError = err
        process.standardInput = FileHandle.nullDevice

        out.fileHandleForReading.readabilityHandler = { [self] handle in
            let data = handle.availableData
            if data.isEmpty {
                handle.readabilityHandler = nil
                closeStream(stdout: true)
            } else {
                append(data, stdout: true)
            }
        }
        err.fileHandleForReading.readabilityHandler = { [self] handle in
            let data = handle.availableData
            if data.isEmpty {
                handle.readabilityHandler = nil
                closeStream(stdout: false)
            } else {
                append(data, stdout: false)
            }
        }
        process.terminationHandler = { [self] _ in
            lock.withLock { exited = true }
            finishIfDone()
            // A grandchild that inherited the pipes can keep them open after
            // git itself has exited; don't wait on it forever.
            DispatchQueue.global().asyncAfter(deadline: .now() + 2) { [self] in
                finish(force: true)
            }
        }

        let startNow = lock.withLock { () -> Bool in
            self.completion = completion
            return !cancelled
        }
        guard startNow else {
            out.fileHandleForReading.readabilityHandler = nil
            err.fileHandleForReading.readabilityHandler = nil
            deliver(GitOutput(status: -1, stdout: Data(), stderr: "", truncated: false, timedOut: false, launchError: "Cancelled"))
            return
        }
        do {
            try process.run()
        } catch {
            out.fileHandleForReading.readabilityHandler = nil
            err.fileHandleForReading.readabilityHandler = nil
            deliver(GitOutput(
                status: -1, stdout: Data(), stderr: "", truncated: false, timedOut: false,
                launchError: error.localizedDescription
            ))
            return
        }
        DispatchQueue.global().asyncAfter(deadline: .now() + timeout) { [self] in
            let running = lock.withLock { () -> Bool in
                guard !exited else { return false }
                timedOut = true
                return true
            }
            if running { terminate() }
        }
    }

    func terminate() {
        let pid: pid_t? = lock.withLock {
            cancelled = true
            return process.isRunning ? process.processIdentifier : nil
        }
        guard let pid else { return }
        kill(pid, SIGTERM)
        DispatchQueue.global().asyncAfter(deadline: .now() + 1) { [self] in
            let stillRunning = lock.withLock { !exited }
            if stillRunning { kill(pid, SIGKILL) }
        }
    }

    private func append(_ data: Data, stdout isStdout: Bool) {
        lock.withLock {
            if isStdout {
                let room = limit - stdout.count
                if room >= data.count {
                    stdout.append(data)
                } else {
                    if room > 0 { stdout.append(data.prefix(room)) }
                    truncated = true
                }
            } else if stderr.count < 64 * 1024 {
                stderr.append(data.prefix(64 * 1024 - stderr.count))
            }
        }
    }

    private func closeStream(stdout isStdout: Bool) {
        lock.withLock {
            if isStdout { stdoutClosed = true } else { stderrClosed = true }
        }
        finishIfDone()
    }

    private func finishIfDone() {
        finish(force: false)
    }

    private func finish(force: Bool) {
        let output: GitOutput? = lock.withLock {
            guard completion != nil, exited, force || (stdoutClosed && stderrClosed) else { return nil }
            return GitOutput(
                status: process.terminationStatus,
                stdout: stdout,
                stderr: String(decoding: stderr, as: UTF8.self),
                truncated: truncated,
                timedOut: timedOut,
                launchError: nil
            )
        }
        if let output { deliver(output) }
    }

    private func deliver(_ output: GitOutput) {
        let completion = lock.withLock { () -> (@Sendable (GitOutput) -> Void)? in
            defer { self.completion = nil }
            return self.completion
        }
        completion?(output)
    }
}

// MARK: - Status

enum ChangeGroup: String, CaseIterable, Sendable {
    case conflicts
    case staged
    case unstaged
    case untracked
    /// Branch mode: everything since the merge-base, committed or not.
    case branch
}

enum ChangeKind: String, Sendable {
    case modified = "M"
    case added = "A"
    case deleted = "D"
    case renamed = "R"
    case copied = "C"
    case typeChanged = "T"
    case untracked = "?"
    case conflicted = "U"

    init(statusLetter: Character) {
        switch statusLetter {
        case "A": self = .added
        case "D": self = .deleted
        case "R": self = .renamed
        case "C": self = .copied
        case "T": self = .typeChanged
        case "U": self = .conflicted
        case "?": self = .untracked
        default: self = .modified
        }
    }
}

struct ChangedFile: Identifiable, Hashable, Sendable {
    let group: ChangeGroup
    let path: String
    var origPath: String?
    let kind: ChangeKind
    var added: Int?
    var deleted: Int?
    var isBinary = false

    var id: String { "\(group.rawValue):\(path)" }

    var fileName: String { (path as NSString).lastPathComponent }

    var directory: String {
        let directory = (path as NSString).deletingLastPathComponent
        return directory.isEmpty ? "" : directory + "/"
    }
}

struct GitBranchInfo: Equatable, Sendable {
    var oid: String?
    var head: String?
    var upstream: String?
    var ahead = 0
    var behind = 0

    var isDetached: Bool { head == nil }
    var isInitial: Bool { oid == nil }

    /// The branch name, or the short commit on a detached HEAD.
    var displayName: String {
        if let head { return head }
        if let oid { return String(oid.prefix(8)) }
        return String(localized: "detached")
    }
}

struct GitStatusSnapshot: Equatable, Sendable {
    var branch = GitBranchInfo()
    var files: [ChangedFile] = []
    /// Entries past the cap, counted but not listed.
    var omittedCount = 0
}

/// Parses `git status --porcelain=v2 --branch -z`.
enum GitStatusParser {
    static let defaultLimit = 2_000

    static func parse(_ data: Data, limit: Int = defaultLimit) -> GitStatusSnapshot {
        var snapshot = GitStatusSnapshot()
        var records = data.split(separator: 0, omittingEmptySubsequences: false)
            .map { String(decoding: $0, as: UTF8.self) }[...]
        var listed = 0

        func add(_ file: ChangedFile) {
            if listed < limit {
                snapshot.files.append(file)
                listed += 1
            } else {
                snapshot.omittedCount += 1
            }
        }

        while let record = records.popFirst() {
            guard let tag = record.first else { continue }
            switch tag {
            case "#":
                parseHeader(record, into: &snapshot.branch)
            case "1", "2":
                let fieldCount = tag == "1" ? 8 : 9
                let fields = record.split(separator: " ", maxSplits: fieldCount, omittingEmptySubsequences: false)
                guard fields.count == fieldCount + 1 else { continue }
                let xy = Array(fields[1])
                guard xy.count == 2 else { continue }
                let path = String(fields[fieldCount])
                let origPath = tag == "2" ? records.popFirst() : nil
                if xy[0] != "." {
                    add(ChangedFile(group: .staged, path: path, origPath: origPath, kind: ChangeKind(statusLetter: xy[0])))
                }
                if xy[1] != "." {
                    // A rename is recorded against the index, so the worktree
                    // side is an ordinary change to the new path.
                    let kind = ChangeKind(statusLetter: xy[1])
                    add(ChangedFile(group: .unstaged, path: path, origPath: nil, kind: kind == .renamed ? .modified : kind))
                }
            case "u":
                let fields = record.split(separator: " ", maxSplits: 10, omittingEmptySubsequences: false)
                guard fields.count == 11 else { continue }
                add(ChangedFile(group: .conflicts, path: String(fields[10]), kind: .conflicted))
            case "?":
                add(ChangedFile(group: .untracked, path: String(record.dropFirst(2)), kind: .untracked))
            default:
                // "!" (ignored) is not requested; anything newer is skipped.
                continue
            }
        }
        return snapshot
    }

    private static func parseHeader(_ record: String, into branch: inout GitBranchInfo) {
        let parts = record.split(separator: " ", maxSplits: 2)
        guard parts.count == 3 else { return }
        let value = String(parts[2])
        switch parts[1] {
        case "branch.oid":
            branch.oid = value == "(initial)" ? nil : value
        case "branch.head":
            branch.head = value == "(detached)" ? nil : value
        case "branch.upstream":
            branch.upstream = value
        case "branch.ab":
            let counts = value.split(separator: " ")
            if counts.count == 2 {
                branch.ahead = Int(counts[0].dropFirst()) ?? 0
                branch.behind = Int(counts[1].dropFirst()) ?? 0
            }
        default:
            break
        }
    }
}

struct LineCounts: Equatable, Sendable {
    /// nil for a binary file, which git counts as `-`.
    let added: Int?
    let deleted: Int?

    var isBinary: Bool { added == nil && deleted == nil }
}

/// Parses `git diff --numstat -z`, keyed by the (new) path.
enum GitNumstatParser {
    static func parse(_ data: Data) -> [String: LineCounts] {
        var tokens = data.split(separator: 0, omittingEmptySubsequences: false)
            .map { String(decoding: $0, as: UTF8.self) }[...]
        var counts: [String: LineCounts] = [:]
        while let token = tokens.popFirst() {
            let fields = token.split(separator: "\t", maxSplits: 2, omittingEmptySubsequences: false)
            guard fields.count == 3 else { continue }
            let value = LineCounts(added: Int(fields[0]), deleted: Int(fields[1]))
            if fields[2].isEmpty {
                // Rename or copy: `added\tdeleted\t\0old\0new\0`.
                _ = tokens.popFirst()
                guard let newPath = tokens.popFirst() else { break }
                counts[newPath] = value
            } else {
                counts[String(fields[2])] = value
            }
        }
        return counts
    }
}

/// Parses `git diff --name-status -z`, for branch mode's file list.
enum GitNameStatusParser {
    static func parse(_ data: Data) -> [ChangedFile] {
        var tokens = data.split(separator: 0, omittingEmptySubsequences: false)
            .map { String(decoding: $0, as: UTF8.self) }[...]
        var files: [ChangedFile] = []
        while let status = tokens.popFirst() {
            guard let letter = status.first else { continue }
            let kind = ChangeKind(statusLetter: letter)
            if kind == .renamed || kind == .copied {
                guard let old = tokens.popFirst(), let new = tokens.popFirst() else { break }
                files.append(ChangedFile(group: .branch, path: new, origPath: old, kind: kind))
            } else {
                guard let path = tokens.popFirst() else { break }
                files.append(ChangedFile(group: .branch, path: path, kind: kind))
            }
        }
        return files
    }
}

// MARK: - Unified diff

enum DiffLineKind: Sendable {
    case context
    case added
    case removed
    /// `\ No newline at end of file`
    case noNewline
}

struct DiffLine: Identifiable, Equatable, Sendable {
    /// Unique within the file, in display order.
    let id: Int
    let kind: DiffLineKind
    let oldNumber: Int?
    let newNumber: Int?
    let text: String
}

struct DiffHunk: Identifiable, Equatable, Sendable {
    let id: Int
    let header: String
    let oldStart: Int
    let oldCount: Int
    let newStart: Int
    let newCount: Int
    let section: String
    var lines: [DiffLine]
    /// A combined (`@@@`) conflict hunk: shown as-is, not commentable.
    var isCombined = false
}

struct DiffFile: Equatable, Sendable {
    var oldPath: String?
    var newPath: String?
    var isNew = false
    var isDeleted = false
    var isBinary = false
    var modeChange: String?
    var hunks: [DiffHunk] = []
    /// The output was cut off (by the byte cap) before the last hunk ended.
    var truncated = false

    var path: String { newPath ?? oldPath ?? "" }
    var lineCount: Int { hunks.reduce(0) { $0 + $1.lines.count } }

    func hunk(containing lineID: Int) -> DiffHunk? {
        hunks.first { hunk in
            guard let first = hunk.lines.first, let last = hunk.lines.last else { return false }
            return (first.id...last.id).contains(lineID)
        }
    }
}

enum UnifiedDiffParser {
    static func parse(_ text: String, truncated: Bool = false) -> [DiffFile] {
        var files: [DiffFile] = []
        var current: DiffFile?
        var lineID = 0
        var oldLine = 0
        var newLine = 0
        var oldRemaining = 0
        var newRemaining = 0
        var inCombined = false

        func flush() {
            if var file = current {
                if oldRemaining > 0 || newRemaining > 0 { file.truncated = true }
                files.append(file)
            }
            current = nil
        }

        func appendHunkLine(_ line: String) {
            guard current != nil, !(current?.hunks.isEmpty ?? true) else { return }
            let index = current!.hunks.count - 1
            if inCombined {
                current!.hunks[index].lines.append(DiffLine(id: lineID, kind: .context, oldNumber: nil, newNumber: nil, text: line))
                lineID += 1
                return
            }
            let marker = line.first
            let body = line.isEmpty ? "" : String(line.dropFirst())
            let diffLine: DiffLine
            switch marker {
            case "+":
                diffLine = DiffLine(id: lineID, kind: .added, oldNumber: nil, newNumber: newLine, text: body)
                newLine += 1
                newRemaining -= 1
            case "-":
                diffLine = DiffLine(id: lineID, kind: .removed, oldNumber: oldLine, newNumber: nil, text: body)
                oldLine += 1
                oldRemaining -= 1
            case "\\":
                diffLine = DiffLine(id: lineID, kind: .noNewline, oldNumber: nil, newNumber: nil, text: line)
            default:
                // " " — or an empty line from a tool that strips trailing
                // whitespace, which is still context.
                diffLine = DiffLine(id: lineID, kind: .context, oldNumber: oldLine, newNumber: newLine, text: body)
                oldLine += 1
                newLine += 1
                oldRemaining -= 1
                newRemaining -= 1
            }
            current!.hunks[index].lines.append(diffLine)
            lineID += 1
        }

        var lines = text.components(separatedBy: "\n")
        if lines.last == "" { lines.removeLast() }

        for raw in lines {
            let line = raw.hasSuffix("\r") ? String(raw.dropLast()) : raw
            let inHunk = current.map { !$0.hunks.isEmpty } ?? false
                && (oldRemaining > 0 || newRemaining > 0 || inCombined)

            // `\ No newline at end of file` follows the hunk's last line, when
            // the counters have already run out.
            let noNewline = line.hasPrefix("\\ ") && !(current?.hunks.isEmpty ?? true)
            if noNewline || (inHunk && (!inCombined || !(line.hasPrefix("diff ") || line.hasPrefix("@@@")))) {
                appendHunkLine(line)
                continue
            }
            if line.hasPrefix("diff --git ") || line.hasPrefix("diff --cc ") || line.hasPrefix("diff --combined ") {
                flush()
                inCombined = false
                lineID = 0
                current = DiffFile()
                if line.hasPrefix("diff --git "), let (old, new) = gitHeaderPaths(String(line.dropFirst(11))) {
                    current?.oldPath = old
                    current?.newPath = new
                } else if line.hasPrefix("diff --cc ") {
                    let path = unquote(String(line.dropFirst(10)))
                    current?.oldPath = path
                    current?.newPath = path
                }
                continue
            }
            if current == nil { current = DiffFile() }
            let nextHunkID = current?.hunks.count ?? 0
            if line.hasPrefix("@@@") {
                inCombined = true
                current?.hunks.append(DiffHunk(
                    id: nextHunkID, header: line, oldStart: 0, oldCount: 0,
                    newStart: 0, newCount: 0, section: "", lines: [], isCombined: true
                ))
                continue
            }
            if line.hasPrefix("@@"), let hunk = parseHunkHeader(line, id: nextHunkID) {
                current?.hunks.append(hunk)
                oldLine = hunk.oldStart
                newLine = hunk.newStart
                oldRemaining = hunk.oldCount
                newRemaining = hunk.newCount
                continue
            }
            if line.hasPrefix("--- ") {
                if let path = diffPath(String(line.dropFirst(4))) {
                    current?.oldPath = path
                } else {
                    current?.isNew = true
                    current?.oldPath = nil
                }
            } else if line.hasPrefix("+++ ") {
                if let path = diffPath(String(line.dropFirst(4))) {
                    current?.newPath = path
                } else {
                    current?.isDeleted = true
                    current?.newPath = nil
                }
            } else if line.hasPrefix("new file mode") {
                current?.isNew = true
            } else if line.hasPrefix("deleted file mode") {
                current?.isDeleted = true
            } else if line.hasPrefix("old mode ") {
                current?.modeChange = String(line.dropFirst(9))
            } else if line.hasPrefix("new mode ") {
                let oldMode = current?.modeChange ?? ""
                current?.modeChange = "\(oldMode) → \(line.dropFirst(9))"
            } else if line.hasPrefix("rename from ") {
                current?.oldPath = unquote(String(line.dropFirst(12)))
            } else if line.hasPrefix("rename to ") {
                current?.newPath = unquote(String(line.dropFirst(10)))
            } else if line.hasPrefix("Binary files ") || line == "GIT binary patch" {
                current?.isBinary = true
            }
        }
        flush()
        if truncated, !files.isEmpty { files[files.count - 1].truncated = true }
        return files
    }

    /// `@@ -a,b +c,d @@ section`; a missing count means 1.
    static func parseHunkHeader(_ line: String, id: Int) -> DiffHunk? {
        let scanner = line.dropFirst(3)
        guard let end = scanner.range(of: " @@") else { return nil }
        let ranges = scanner[..<end.lowerBound].split(separator: " ")
        guard ranges.count == 2, ranges[0].hasPrefix("-"), ranges[1].hasPrefix("+") else { return nil }
        func range(_ text: Substring) -> (Int, Int)? {
            let parts = text.dropFirst().split(separator: ",", omittingEmptySubsequences: false)
            guard let start = Int(parts[0]) else { return nil }
            let count = parts.count > 1 ? Int(parts[1]) : 1
            guard let count else { return nil }
            return (start, count)
        }
        guard let (oldStart, oldCount) = range(ranges[0]), let (newStart, newCount) = range(ranges[1]) else { return nil }
        let section = scanner[end.upperBound...].trimmingCharacters(in: .whitespaces)
        return DiffHunk(
            id: id, header: line, oldStart: oldStart, oldCount: oldCount,
            newStart: newStart, newCount: newCount, section: section, lines: []
        )
    }

    /// `a/old b/new` from `diff --git`. The split is only unambiguous when
    /// both sides are equal or quoted; the `---`/`+++` lines refine it later.
    private static func gitHeaderPaths(_ rest: String) -> (String, String)? {
        if rest.hasPrefix("\""), let close = rest.dropFirst().firstIndex(of: "\"") {
            let old = unquote(String(rest[...close]))
            let new = unquote(String(rest[rest.index(after: close)...]).trimmingCharacters(in: .whitespaces))
            return (stripPrefix(old), stripPrefix(new))
        }
        // Equal halves: "a/x b/x".
        let half = (rest.count - 1) / 2
        if rest.count % 2 == 1 {
            let old = String(rest.prefix(half))
            let new = String(rest.suffix(half))
            if stripPrefix(old) == stripPrefix(new) { return (stripPrefix(old), stripPrefix(new)) }
        }
        guard let split = rest.range(of: " b/") else { return nil }
        return (stripPrefix(String(rest[..<split.lowerBound])), String(rest[split.upperBound...]))
    }

    private static func diffPath(_ text: String) -> String? {
        let path = unquote(text.components(separatedBy: "\t").first ?? text)
        return path == "/dev/null" ? nil : stripPrefix(path)
    }

    private static func stripPrefix(_ path: String) -> String {
        path.hasPrefix("a/") || path.hasPrefix("b/") ? String(path.dropFirst(2)) : path
    }

    /// git's C-style quoting, used for paths with control characters or
    /// quotes even with `core.quotepath=off`.
    static func unquote(_ text: String) -> String {
        guard text.count >= 2, text.hasPrefix("\""), text.hasSuffix("\"") else { return text }
        var bytes: [UInt8] = []
        var iterator = Array(text.utf8.dropFirst().dropLast()).makeIterator()
        while let byte = iterator.next() {
            guard byte == UInt8(ascii: "\\"), let next = iterator.next() else {
                bytes.append(byte)
                continue
            }
            switch next {
            case UInt8(ascii: "n"): bytes.append(0x0A)
            case UInt8(ascii: "t"): bytes.append(0x09)
            case UInt8(ascii: "r"): bytes.append(0x0D)
            case UInt8(ascii: "0")...UInt8(ascii: "7"):
                var value = Int(next - UInt8(ascii: "0"))
                for _ in 0..<2 {
                    guard let digit = iterator.next() else { break }
                    value = value * 8 + Int(digit - UInt8(ascii: "0"))
                }
                bytes.append(UInt8(truncatingIfNeeded: value))
            default: bytes.append(next)
            }
        }
        return String(decoding: bytes, as: UTF8.self)
    }
}
