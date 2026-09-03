import Foundation
import SwiftUI

// MARK: - A very small TOML editor

/// The scalar kinds the Behaviour forms write. Anything richer belongs in
/// the Advanced tab's text editor, where the daemon validates the document.
enum MuxaTOMLScalar: Hashable, Sendable {
    case bool(Bool)
    case string(String)
    case integer(Int)

    var literal: String {
        switch self {
        case .bool(let value): value ? "true" : "false"
        case .string(let value): MuxaAutomationRule.tomlString(value)
        case .integer(let value): String(value)
        }
    }

    var boolValue: Bool? {
        if case .bool(let value) = self { return value }
        return nil
    }

    var stringValue: String? {
        if case .string(let value) = self { return value }
        return nil
    }
}

/// One `section.key = value` assignment to make in a document.
struct MuxaTOMLEdit: Hashable, Sendable {
    let section: String
    let key: String
    let value: MuxaTOMLScalar

    init(_ section: String, _ key: String, _ value: MuxaTOMLScalar) {
        self.section = section
        self.key = key
        self.value = value
    }
}

/// Reads and rewrites single scalar keys of a `[section]` table, leaving
/// every other byte of the document — comments, ordering, unrelated
/// sections — exactly as it was.
///
/// This is deliberately not a TOML parser. It understands enough of the
/// grammar to know what is *not* a table header or a key: comments, quoted
/// strings, and multi-line strings. Anything it cannot place, it leaves
/// alone, and the daemon validates the result before it lands on disk.
enum MuxaTOMLPatcher {
    /// Where a scan is when a line begins.
    enum LineState: Hashable, Sendable {
        case code
        case basicMultiline
        case literalMultiline
    }

    // MARK: Reading

    static func value(section: String, key: String, in text: String) -> MuxaTOMLScalar? {
        let lines = text.components(separatedBy: "\n")
        let states = lineStates(lines)
        guard let body = bodyRange(of: section, lines: lines, states: states) else { return nil }
        guard let index = keyLine(key, in: body, lines: lines, states: states) else { return nil }
        guard let assignment = assignment(in: lines[index], key: key) else { return nil }
        return scalar(from: assignment.value)
    }

    static func bool(section: String, key: String, in text: String, default fallback: Bool) -> Bool {
        value(section: section, key: key, in: text)?.boolValue ?? fallback
    }

    static func string(section: String, key: String, in text: String) -> String? {
        value(section: section, key: key, in: text)?.stringValue
    }

    // MARK: Writing

    static func apply(_ edits: [MuxaTOMLEdit], to text: String) -> String {
        edits.reduce(text) { apply($1, to: $0) }
    }

    static func apply(_ edit: MuxaTOMLEdit, to text: String) -> String {
        var lines = text.components(separatedBy: "\n")
        let states = lineStates(lines)
        let assignmentText = "\(edit.key) = \(edit.value.literal)"

        guard let body = bodyRange(of: edit.section, lines: lines, states: states) else {
            // No such table: append one. A document that does not end in a
            // newline gets one first, so the header starts its own line.
            var appended = lines
            while appended.last?.isEmpty == true { appended.removeLast() }
            if !appended.isEmpty { appended.append("") }
            appended.append("[\(edit.section)]")
            appended.append(assignmentText)
            appended.append("")
            return appended.joined(separator: "\n")
        }

        if let index = keyLine(edit.key, in: body, lines: lines, states: states),
           let assignment = assignment(in: lines[index], key: edit.key) {
            lines[index] = "\(assignment.prefix)\(edit.value.literal)\(assignment.comment)"
            return lines.joined(separator: "\n")
        }

        // The table exists without the key: put it after the table's last
        // non-blank line so it joins the block instead of splitting it.
        var insertion = body.lowerBound
        for index in body where !lines[index].trimmingCharacters(in: .whitespaces).isEmpty {
            insertion = index + 1
        }
        lines.insert(assignmentText, at: insertion)
        return lines.joined(separator: "\n")
    }

    // MARK: Scanning

    /// The state each line *starts* in. Lines that start inside a
    /// multi-line string are never mistaken for headers or keys.
    static func lineStates(_ lines: [String]) -> [LineState] {
        var states: [LineState] = []
        states.reserveCapacity(lines.count)
        var state = LineState.code
        for line in lines {
            states.append(state)
            state = advance(state, through: line)
        }
        return states
    }

    private static func advance(_ start: LineState, through line: String) -> LineState {
        var state = start
        let characters = Array(line)
        var index = 0
        while index < characters.count {
            switch state {
            case .code:
                let character = characters[index]
                if character == "#" {
                    return .code
                }
                if matches(characters, at: index, "\"\"\"") {
                    state = .basicMultiline
                    index += 3
                    continue
                }
                if matches(characters, at: index, "'''") {
                    state = .literalMultiline
                    index += 3
                    continue
                }
                if character == "\"" {
                    index = endOfBasicString(characters, from: index + 1)
                    continue
                }
                if character == "'" {
                    index = endOfLiteralString(characters, from: index + 1)
                    continue
                }
                index += 1
            case .basicMultiline:
                if matches(characters, at: index, "\"\"\"") {
                    state = .code
                    index += 3
                    continue
                }
                index += characters[index] == "\\" ? 2 : 1
            case .literalMultiline:
                if matches(characters, at: index, "'''") {
                    state = .code
                    index += 3
                    continue
                }
                index += 1
            }
        }
        return state
    }

    private static func matches(_ characters: [Character], at index: Int, _ token: String) -> Bool {
        let token = Array(token)
        guard index + token.count <= characters.count else { return false }
        return Array(characters[index..<(index + token.count)]) == token
    }

    /// Index just past the closing quote, or the end of the line when the
    /// string never closes (a malformed document; the daemon will say so).
    private static func endOfBasicString(_ characters: [Character], from start: Int) -> Int {
        var index = start
        while index < characters.count {
            if characters[index] == "\\" {
                index += 2
                continue
            }
            if characters[index] == "\"" { return index + 1 }
            index += 1
        }
        return characters.count
    }

    private static func endOfLiteralString(_ characters: [Character], from start: Int) -> Int {
        var index = start
        while index < characters.count {
            if characters[index] == "'" { return index + 1 }
            index += 1
        }
        return characters.count
    }

    /// The table header a line declares, when it declares one. `[[a.b]]`
    /// array-of-table headers end a table's body but never start one this
    /// patcher writes into.
    static func header(of line: String, state: LineState) -> (name: String, isArray: Bool)? {
        guard state == .code else { return nil }
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        guard trimmed.hasPrefix("[") else { return nil }
        let isArray = trimmed.hasPrefix("[[")
        let open = isArray ? 2 : 1
        let closing = isArray ? "]]" : "]"
        guard let range = trimmed.range(of: closing, options: .backwards) else { return nil }
        let name = String(trimmed[trimmed.index(trimmed.startIndex, offsetBy: open)..<range.lowerBound])
        return (name.trimmingCharacters(in: .whitespaces), isArray)
    }

    /// The lines belonging to `[section]`, header excluded.
    private static func bodyRange(
        of section: String,
        lines: [String],
        states: [LineState]
    ) -> Range<Int>? {
        guard let start = lines.indices.first(where: { index in
            let found = header(of: lines[index], state: states[index])
            return found?.isArray == false && found?.name == section
        }) else { return nil }
        var end = lines.count
        var index = start + 1
        while index < lines.count {
            if header(of: lines[index], state: states[index]) != nil {
                end = index
                break
            }
            index += 1
        }
        return (start + 1)..<end
    }

    private static func keyLine(
        _ key: String,
        in body: Range<Int>,
        lines: [String],
        states: [LineState]
    ) -> Int? {
        body.first { index in
            states[index] == .code && assignment(in: lines[index], key: key) != nil
        }
    }

    /// Splits `  enabled = false  # note` into the text before the value,
    /// the value itself, and the trailing comment, when the line assigns
    /// `key`. Bare and quoted key spellings both match.
    static func assignment(
        in line: String,
        key: String
    ) -> (prefix: String, value: String, comment: String)? {
        guard let equals = line.firstIndex(of: "=") else { return nil }
        let name = line[line.startIndex..<equals].trimmingCharacters(in: .whitespaces)
        let unquoted = name.hasPrefix("\"") && name.hasSuffix("\"") && name.count >= 2
            ? String(name.dropFirst().dropLast())
            : name
        guard unquoted == key else { return nil }
        let rest = line[line.index(after: equals)...]
        let leading = rest.prefix { $0 == " " || $0 == "\t" }
        let remainder = rest[leading.endIndex...]
        let split = splitTrailingComment(String(remainder))
        return (
            prefix: String(line[line.startIndex...equals]) + String(leading),
            value: split.value,
            comment: split.comment
        )
    }

    /// Everything from the first `#` outside a string is a comment; the
    /// whitespace before it belongs to the comment so it survives a rewrite.
    private static func splitTrailingComment(_ text: String) -> (value: String, comment: String) {
        let characters = Array(text)
        var index = 0
        while index < characters.count {
            let character = characters[index]
            if character == "#" {
                var start = index
                while start > 0, characters[start - 1] == " " || characters[start - 1] == "\t" {
                    start -= 1
                }
                return (String(characters[0..<start]), String(characters[start...]))
            }
            if character == "\"" {
                index = endOfBasicString(characters, from: index + 1)
                continue
            }
            if character == "'" {
                index = endOfLiteralString(characters, from: index + 1)
                continue
            }
            index += 1
        }
        return (text, "")
    }

    /// The value of an assignment, for the scalar kinds this file writes.
    static func scalar(from text: String) -> MuxaTOMLScalar? {
        let trimmed = text.trimmingCharacters(in: .whitespaces)
        if trimmed == "true" { return .bool(true) }
        if trimmed == "false" { return .bool(false) }
        if trimmed.hasPrefix("\""), trimmed.hasSuffix("\""), trimmed.count >= 2 {
            return .string(unescape(String(trimmed.dropFirst().dropLast())))
        }
        if trimmed.hasPrefix("'"), trimmed.hasSuffix("'"), trimmed.count >= 2 {
            return .string(String(trimmed.dropFirst().dropLast()))
        }
        if let integer = Int(trimmed.replacingOccurrences(of: "_", with: "")) {
            return .integer(integer)
        }
        return nil
    }

    private static func unescape(_ text: String) -> String {
        var result = ""
        var iterator = text.makeIterator()
        while let character = iterator.next() {
            guard character == "\\" else {
                result.append(character)
                continue
            }
            switch iterator.next() {
            case "n": result.append("\n")
            case "r": result.append("\r")
            case "t": result.append("\t")
            case "\"": result.append("\"")
            case "\\": result.append("\\")
            case let other?: result.append(other)
            case nil: break
            }
        }
        return result
    }
}

// MARK: - The two sections the Behaviour tab owns

enum MuxaNotifierBackend: String, CaseIterable, Hashable, Sendable, Identifiable {
    case none
    case libnotify

    var id: String { rawValue }
}

enum MuxaCollaborationWake: String, CaseIterable, Hashable, Sendable, Identifiable {
    case never
    case idleOnly = "idle_only"

    var id: String { rawValue }
}

enum MuxaCollaborationWakePayload: String, CaseIterable, Hashable, Sendable, Identifiable {
    case notice
    case operatorFull = "operator_full"
    case full

    var id: String { rawValue }
}

enum MuxaCollaborationScope: String, CaseIterable, Hashable, Sendable, Identifiable {
    case window
    case host

    var id: String { rawValue }
}

/// `[notifier]` and `[collaboration]`, the two sections a person actually
/// tunes, as a form's worth of state. Defaults match the daemon's, so a
/// document that names none of these keys reads back as the daemon runs.
struct MuxaBehaviourSettings: Hashable, Sendable {
    var notifierEnabled = false
    var notifierBackend = MuxaNotifierBackend.none
    var collaborationEnabled = false
    var collaborationWake = MuxaCollaborationWake.idleOnly
    var collaborationWakePayload = MuxaCollaborationWakePayload.operatorFull
    var collaborationScope = MuxaCollaborationScope.window

    static let daemonDefaults = MuxaBehaviourSettings()

    static func read(from text: String) -> MuxaBehaviourSettings {
        var settings = MuxaBehaviourSettings()
        settings.notifierEnabled = MuxaTOMLPatcher.bool(
            section: "notifier", key: "enabled", in: text, default: settings.notifierEnabled
        )
        if let backend = MuxaTOMLPatcher.string(section: "notifier", key: "backend", in: text),
           let parsed = MuxaNotifierBackend(rawValue: backend) {
            settings.notifierBackend = parsed
        }
        settings.collaborationEnabled = MuxaTOMLPatcher.bool(
            section: "collaboration", key: "enabled", in: text, default: settings.collaborationEnabled
        )
        if let wake = MuxaTOMLPatcher.string(section: "collaboration", key: "wake", in: text),
           let parsed = MuxaCollaborationWake(rawValue: wake) {
            settings.collaborationWake = parsed
        }
        if let payload = MuxaTOMLPatcher.string(section: "collaboration", key: "wake_payload", in: text),
           let parsed = MuxaCollaborationWakePayload(rawValue: payload) {
            settings.collaborationWakePayload = parsed
        }
        if let scope = MuxaTOMLPatcher.string(section: "collaboration", key: "scope", in: text),
           let parsed = MuxaCollaborationScope(rawValue: scope) {
            settings.collaborationScope = parsed
        }
        return settings
    }

    /// Only the keys that actually changed, so saving one toggle does not
    /// spell out five defaults the operator never wrote.
    func edits(against current: MuxaBehaviourSettings) -> [MuxaTOMLEdit] {
        var edits: [MuxaTOMLEdit] = []
        if notifierEnabled != current.notifierEnabled {
            edits.append(MuxaTOMLEdit("notifier", "enabled", .bool(notifierEnabled)))
        }
        if notifierBackend != current.notifierBackend {
            edits.append(MuxaTOMLEdit("notifier", "backend", .string(notifierBackend.rawValue)))
        }
        if collaborationEnabled != current.collaborationEnabled {
            edits.append(MuxaTOMLEdit("collaboration", "enabled", .bool(collaborationEnabled)))
        }
        if collaborationWake != current.collaborationWake {
            edits.append(MuxaTOMLEdit("collaboration", "wake", .string(collaborationWake.rawValue)))
        }
        if collaborationWakePayload != current.collaborationWakePayload {
            edits.append(
                MuxaTOMLEdit("collaboration", "wake_payload", .string(collaborationWakePayload.rawValue))
            )
        }
        if collaborationScope != current.collaborationScope {
            edits.append(MuxaTOMLEdit("collaboration", "scope", .string(collaborationScope.rawValue)))
        }
        return edits
    }
}

// MARK: - Store

/// The daemon's `config.toml` for the Advanced and Behaviour tabs.
///
/// `@Published` state cannot live in an `AppModel` extension, so both panes
/// share this object: one load, one draft, one `expected_text` baseline.
@MainActor
final class MuxaConfigStore: ObservableObject {
    static let shared = MuxaConfigStore()

    @Published private(set) var document: MuxaDaemonConfigDocument?
    /// The Advanced editor's buffer. Diverges from `document.text` while
    /// the operator types; `expected_text` always carries `document.text`.
    @Published var draft = ""
    @Published private(set) var isSupported = false
    @Published private(set) var isLoading = false
    @Published private(set) var isSaving = false
    @Published private(set) var loadError: String?
    /// The daemon's parse/validation message, shown verbatim.
    @Published private(set) var saveError: String?
    /// Set instead when the refusal was a concurrent edit: the file moved
    /// under the editor and `document` now holds what is on disk.
    @Published private(set) var conflictMessage: String?
    /// The edits a conflicted `apply(_:)` was making, so the operator can
    /// put them on top of the file as it now stands with one button.
    @Published private(set) var retryEdits: [MuxaTOMLEdit] = []
    @Published private(set) var status: String?

    init() {}

    var loadedText: String { document?.text ?? "" }
    var hasLoaded: Bool { document != nil }
    var isDirty: Bool { hasLoaded && draft != loadedText }
    var path: String { document?.path ?? "" }

    var behaviour: MuxaBehaviourSettings {
        MuxaBehaviourSettings.read(from: loadedText)
    }

    /// Reads the document once per connection; `force` re-reads and
    /// discards an unsaved draft.
    func load(model: AppModel, force: Bool = false) async {
        isSupported = await model.client.supports(MuxaIPCClient.configEditCapability)
        guard isSupported else {
            document = nil
            return
        }
        guard force || document == nil else { return }
        isLoading = true
        defer { isLoading = false }
        loadError = nil
        do {
            let loaded = try await model.client.readDaemonConfig()
            document = loaded
            draft = loaded.text
            saveError = nil
            conflictMessage = nil
            retryEdits = []
            if force { status = String(localized: "Reloaded from disk.") }
        } catch {
            loadError = error.localizedDescription
        }
    }

    /// Writes the Advanced editor's buffer.
    @discardableResult
    func save(model: AppModel) async -> Bool {
        await write(text: draft, model: model, success: String(localized: "Saved."))
    }

    /// Writes `edits` against the last loaded text. Refuses while the
    /// Advanced editor holds unsaved changes rather than silently
    /// discarding them.
    @discardableResult
    func apply(_ edits: [MuxaTOMLEdit], model: AppModel) async -> Bool {
        guard !edits.isEmpty else { return true }
        guard !isDirty else {
            saveError = String(
                localized: "Advanced has unsaved changes. Save or reload it before changing this."
            )
            return false
        }
        let patched = MuxaTOMLPatcher.apply(edits, to: loadedText)
        let wrote = await write(text: patched, model: model, success: String(localized: "Saved."))
        // A conflict leaves `document` holding the file as it now stands, so
        // the same edits re-applied land on top of whatever changed.
        retryEdits = wrote || conflictMessage == nil ? [] : edits
        return wrote
    }

    /// Re-applies the edits a conflict interrupted, against the document the
    /// daemon handed back.
    @discardableResult
    func retryPendingEdits(model: AppModel) async -> Bool {
        let edits = retryEdits
        guard !edits.isEmpty else { return false }
        conflictMessage = nil
        return await apply(edits, model: model)
    }

    private func write(text: String, model: AppModel, success: String) async -> Bool {
        guard !isSaving else { return false }
        // Never write a document that was never read: `expected_text` would
        // be nil and an empty draft would replace the operator's file.
        guard let loaded = document else {
            saveError = String(localized: "The configuration file has not been read yet.")
            return false
        }
        isSaving = true
        defer { isSaving = false }
        let wasDirty = isDirty
        saveError = nil
        conflictMessage = nil
        status = nil
        do {
            let saved = try await model.client.writeDaemonConfig(
                text: text,
                // A file that does not exist yet has no text to match on.
                expectedText: loaded.exists ? loaded.text : nil
            )
            document = saved
            draft = saved.text
            retryEdits = []
            status = success
            return true
        } catch let conflict as MuxaConfigConflict {
            // Someone else wrote the file between the read and this write.
            // Take what is on disk as the new baseline so a second attempt
            // is against the current file — and keep the operator's work:
            // an edited draft stays, an untouched one follows the file.
            document = conflict.current
            if !wasDirty { draft = conflict.current.text }
            conflictMessage = conflict.message
            saveError = conflict.message
            return false
        } catch {
            saveError = error.localizedDescription
            return false
        }
    }

    func revertDraft() {
        draft = loadedText
        saveError = nil
        conflictMessage = nil
        retryEdits = []
        status = nil
    }

    func clearStatus() {
        status = nil
        saveError = nil
        conflictMessage = nil
    }
}
