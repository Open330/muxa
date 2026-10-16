import CryptoKit
import Foundation

// Review helpers for the Changes module that sit between the parsed diff and
// the views: the side-by-side pairing and the per-file "Viewed" marks. Plain
// data, so they can be tested with fixture diffs.

// MARK: - Side-by-side

enum DiffLayout: String, CaseIterable, Sendable {
    case unified
    case split

    static let storageKey = "muxa.changes.diffLayout"
}

/// One row of the split view: the old file's line on the left, the new
/// file's on the right. A context line sits on both sides; a missing side is
/// a filler.
struct SplitDiffRow: Identifiable, Equatable, Sendable {
    let left: DiffLine?
    let right: DiffLine?

    /// Unique within the file: every line lands on exactly one row.
    var id: Int { left?.id ?? right?.id ?? -1 }

    /// The line ids on the row, for anchoring comments and the selection.
    var lineIDs: [Int] {
        switch (left?.id, right?.id) {
        case (let l?, let r?): l == r ? [l] : [l, r]
        case (let l?, nil): [l]
        case (nil, let r?): [r]
        case (nil, nil): []
        }
    }

    func contains(_ lineID: Int) -> Bool {
        left?.id == lineID || right?.id == lineID
    }
}

enum SplitDiffPairer {
    /// Pairs a hunk's lines into rows: each run of removals is set against
    /// the additions that follow it, line by line, and the longer run's
    /// extra lines get a blank opposite. `\ No newline` markers stay on the
    /// side of the line they follow. Combined (conflict) hunks have no two
    /// sides; callers show them unified.
    static func rows(for hunk: DiffHunk) -> [SplitDiffRow] {
        var rows: [SplitDiffRow] = []
        rows.reserveCapacity(hunk.lines.count)
        var removed: [DiffLine] = []
        var added: [DiffLine] = []
        /// The side the last line went to, for a trailing marker.
        var lastKind = DiffLineKind.context

        func flush() {
            for index in 0..<max(removed.count, added.count) {
                rows.append(SplitDiffRow(
                    left: index < removed.count ? removed[index] : nil,
                    right: index < added.count ? added[index] : nil
                ))
            }
            removed = []
            added = []
        }

        for line in hunk.lines {
            switch line.kind {
            case .removed:
                // A removal after additions starts a new change block.
                if !added.isEmpty { flush() }
                removed.append(line)
                lastKind = .removed
            case .added:
                added.append(line)
                lastKind = .added
            case .noNewline:
                switch lastKind {
                case .removed: removed.append(line)
                case .added: added.append(line)
                case .context, .noNewline:
                    flush()
                    rows.append(SplitDiffRow(left: line, right: line))
                }
            case .context:
                flush()
                rows.append(SplitDiffRow(left: line, right: line))
                lastKind = .context
            }
        }
        flush()
        return rows
    }
}

// MARK: - Viewed files

enum DiffFingerprint {
    /// A stable digest of what a file's diff shows. Paths are left out (the
    /// viewed key already names the file) so the same change reads the same
    /// whether it came from a one-file diff or a batch.
    static func of(_ file: DiffFile) -> String {
        var hash = SHA256()
        func add(_ text: String) {
            hash.update(data: Data(text.utf8))
            hash.update(data: Data([0]))
        }
        add(file.oldPath ?? "")
        add("\(file.isNew) \(file.isDeleted) \(file.isBinary)")
        add(file.modeChange ?? "")
        for hunk in file.hunks {
            add(hunk.header)
            for line in hunk.lines {
                let marker = switch line.kind {
                case .context: " "
                case .added: "+"
                case .removed: "-"
                case .noNewline: "\\"
                }
                add(marker + line.text)
            }
        }
        return hash.finalize().map { String(format: "%02x", $0) }.joined()
    }

    /// Fingerprints for every complete file in a multi-file diff, keyed by
    /// path. A file cut off by the byte cap is left out: its digest would
    /// not match the full diff.
    static func byPath(parsing text: String, truncated: Bool) -> [String: String] {
        var result: [String: String] = [:]
        for file in UnifiedDiffParser.parse(text, truncated: truncated) where !file.truncated && !file.path.isEmpty {
            result[file.path] = of(file)
        }
        return result
    }
}

struct ViewedRecord: Codable, Equatable, Sendable {
    let fingerprint: String
    let viewedAt: Date
}

/// Which files the operator has marked viewed, keyed by host, repository and
/// file (its list and path), and holding the digest of the diff they saw. A
/// mark stands only while the file's diff still has that digest.
struct ViewedFiles: Equatable, Sendable {
    static let limit = 2_000

    private(set) var records: [String: ViewedRecord] = [:]

    init(records: [String: ViewedRecord] = [:]) {
        self.records = records
    }

    static func key(hostAlias: String, root: String, fileID: String) -> String {
        "\(hostAlias)\u{0}\(root)\u{0}\(fileID)"
    }

    func isViewed(_ key: String) -> Bool {
        records[key] != nil
    }

    func fingerprint(_ key: String) -> String? {
        records[key]?.fingerprint
    }

    mutating func mark(_ key: String, fingerprint: String, at date: Date = Date()) {
        records[key] = ViewedRecord(fingerprint: fingerprint, viewedAt: date)
        prune()
    }

    mutating func unmark(_ key: String) {
        records[key] = nil
    }

    /// Drops the marks on files whose diff now reads differently. Keys
    /// missing from `fingerprints` are left alone: not knowing is not a
    /// change. Returns whether anything was dropped.
    @discardableResult
    mutating func reconcile(_ fingerprints: [String: String]) -> Bool {
        var changed = false
        for (key, fingerprint) in fingerprints {
            if let record = records[key], record.fingerprint != fingerprint {
                records[key] = nil
                changed = true
            }
        }
        return changed
    }

    /// Keeps the newest marks once there are more than `limit`.
    mutating func prune(limit: Int = limit) {
        guard records.count > limit else { return }
        let newest = records.sorted { $0.value.viewedAt > $1.value.viewedAt }.prefix(limit)
        records = Dictionary(uniqueKeysWithValues: newest.map { ($0.key, $0.value) })
    }
}

/// The viewed marks, shared by every Changes editor and kept across
/// launches.
@MainActor
final class ViewedFilesStore: ObservableObject {
    static let shared = ViewedFilesStore()
    static let persistenceKey = "muxa.changes.viewed"

    @Published private(set) var state: ViewedFiles
    private let defaults: UserDefaults?

    init(defaults: UserDefaults? = .standard) {
        self.defaults = defaults
        if let data = defaults?.data(forKey: Self.persistenceKey),
           let stored = try? JSONDecoder().decode([String: ViewedRecord].self, from: data) {
            state = ViewedFiles(records: stored)
        } else {
            state = ViewedFiles()
        }
    }

    func isViewed(_ key: String) -> Bool { state.isViewed(key) }

    func mark(_ key: String, fingerprint: String) {
        state.mark(key, fingerprint: fingerprint)
        save()
    }

    func unmark(_ key: String) {
        guard state.isViewed(key) else { return }
        state.unmark(key)
        save()
    }

    func reconcile(_ fingerprints: [String: String]) {
        var next = state
        guard next.reconcile(fingerprints) else { return }
        state = next
        save()
    }

    private func save() {
        guard let defaults, let data = try? JSONEncoder().encode(state.records) else { return }
        defaults.set(data, forKey: Self.persistenceKey)
    }
}
