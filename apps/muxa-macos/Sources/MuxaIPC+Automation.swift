import Foundation

// MARK: - Wire model

/// The agent condition an automation rule listens for.
///
/// `other` keeps a rule written by a newer daemon decodable instead of
/// failing the whole list; the rule editor only offers `pickable`.
enum MuxaAutomationEvent: Hashable, Sendable, Codable {
    case rateLimited
    case waitingInput
    case idleFor
    case error
    case other(String)

    /// What the editor offers, in the order the sheet lists them.
    static let pickable: [MuxaAutomationEvent] = [.rateLimited, .waitingInput, .idleFor, .error]

    init(rawValue: String) {
        switch rawValue {
        case "rate_limited": self = .rateLimited
        case "waiting_input": self = .waitingInput
        case "idle_for": self = .idleFor
        case "error": self = .error
        default: self = .other(rawValue)
        }
    }

    var rawValue: String {
        switch self {
        case .rateLimited: "rate_limited"
        case .waitingInput: "waiting_input"
        case .idleFor: "idle_for"
        case .error: "error"
        case .other(let raw): raw
        }
    }

    init(from decoder: Decoder) throws {
        self.init(rawValue: try decoder.singleValueContainer().decode(String.self))
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(rawValue)
    }

    /// Only a rate-limit cap carries a window, so only it can be filtered
    /// by one. It is also the only event that carries a reset time, so a
    /// `reset` anchor and a `fallback` mean nothing anywhere else — the
    /// daemon refuses both.
    var supportsScopeFilter: Bool { self == .rateLimited }
    var supportsResetTiming: Bool { self == .rateLimited }

    /// `idle_for` is the one event that needs its own duration, and the
    /// only one allowed to carry it.
    var requiresDuration: Bool { self == .idleFor }

    /// `wait` when a rule names none: the cap's own reset time for
    /// `rate_limited`, otherwise the moment of the event.
    var defaultWait: MuxaAutomationWait {
        supportsResetTiming ? .afterReset(0) : .delay(0)
    }

    /// What `only_if_still` re-checks when a rule names nothing.
    var defaultCondition: MuxaAutomationCondition {
        switch self {
        case .rateLimited: .rateLimited
        case .waitingInput: .waitingInput
        case .idleFor: .idle
        case .error: .error
        case .other(let raw): .other(raw)
        }
    }
}

/// What a rule does when it fires. `other` mirrors `MuxaAutomationEvent`.
///
/// The payload keys are exclusive: the daemon refuses `message` on a
/// `send_prompt` rule and `text`/`submit` on any other, so the editor and
/// `wireObject` send exactly one action's payload.
enum MuxaAutomationAction: Hashable, Sendable, Codable {
    case sendPrompt
    case notify
    case interrupt
    case other(String)

    static let pickable: [MuxaAutomationAction] = [.sendPrompt, .notify, .interrupt]

    init(rawValue: String) {
        switch rawValue {
        case "send_prompt": self = .sendPrompt
        case "notify": self = .notify
        case "interrupt": self = .interrupt
        default: self = .other(rawValue)
        }
    }

    var rawValue: String {
        switch self {
        case .sendPrompt: "send_prompt"
        case .notify: "notify"
        case .interrupt: "interrupt"
        case .other(let raw): raw
        }
    }

    init(from decoder: Decoder) throws {
        self.init(rawValue: try decoder.singleValueContainer().decode(String.self))
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(rawValue)
    }

    /// `send_prompt` is the only action that types into the agent, so it is
    /// the only one that carries `text` and `submit`.
    var needsText: Bool { self == .sendPrompt }
    var usesSubmit: Bool { self == .sendPrompt }
    /// `notify` carries `message` and nothing else.
    var needsMessage: Bool { self == .notify }
}

/// `only_if_still` — what is re-checked against the live registry when a
/// rule fires. `idle` (not `idle_for`) is the condition `idle_for` arms.
enum MuxaAutomationCondition: Hashable, Sendable, Codable {
    case rateLimited
    case waitingInput
    case idle
    case error
    case any
    case other(String)

    static let pickable: [MuxaAutomationCondition] = [
        .rateLimited, .waitingInput, .idle, .error, .any,
    ]

    init(rawValue: String) {
        switch rawValue {
        case "rate_limited": self = .rateLimited
        case "waiting_input": self = .waitingInput
        case "idle": self = .idle
        case "error": self = .error
        case "any": self = .any
        default: self = .other(rawValue)
        }
    }

    var rawValue: String {
        switch self {
        case .rateLimited: "rate_limited"
        case .waitingInput: "waiting_input"
        case .idle: "idle"
        case .error: "error"
        case .any: "any"
        case .other(let raw): raw
        }
    }

    init(from decoder: Decoder) throws {
        self.init(rawValue: try decoder.singleValueContainer().decode(String.self))
    }

    func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(rawValue)
    }
}

/// One `[[automation.rule]]`, in both directions.
///
/// Decoding takes an `automation_rules` row, which is a *complete*
/// description: the filters and the action payload verbatim, plus the
/// timing and guards with the daemon's defaults already resolved. That is
/// what lets the editor load a rule, change one field, and hand the whole
/// thing back without dropping what it did not render.
///
/// Encoding drops the keys the chosen event or action cannot carry, because
/// the daemon refuses them (`scope` off `rate_limited`, `for` off
/// `idle_for`, `message` on `send_prompt`, `text`/`submit` on anything else).
struct MuxaAutomationRule: Decodable, Hashable, Sendable, Identifiable {
    var name: String
    var on: MuxaAutomationEvent
    var enabled: Bool
    var action: MuxaAutomationAction

    // Filters, verbatim.
    var agent: [String]
    var workspace: String?
    var work: String?
    var pane: String?
    var host: String?
    var scope: [String]
    /// `for` — how long the agent must have been idle (`idle_for` only).
    var idleFor: String?

    // Action payload, verbatim.
    var text: String?
    var message: String?
    var submit: Bool

    // Timing and guards, defaults already resolved on the way in.
    var wait: String?
    var fallback: String?
    var jitter: String?
    var maxPerHour: Int
    var cooldown: String
    var onlyIfStill: MuxaAutomationCondition?

    // Derived, read-only: present on a row from the daemon, never sent back.
    /// One-line filter summary the daemon renders, or `any`.
    var filters: String?
    var firedLastHour: Int?
    var lastFiredAt: String?

    var id: String { name }

    /// The daemon's documented defaults, so a rule built here behaves the
    /// same as the same rule written by hand with the key left out.
    static let defaultMaxPerHour = 3
    static let maximumMaxPerHour = 60
    static let defaultCooldown = "2m"
    static let defaultFallback = "15m"
    static let defaultJitter = "15s"
    /// `text` and `message` are bounded, and `name` is a short TOML key.
    static let maximumTextBytes = 4096
    static let maximumNameLength = 64

    init(
        name: String,
        on: MuxaAutomationEvent,
        enabled: Bool = true,
        agent: [String] = [],
        workspace: String? = nil,
        work: String? = nil,
        pane: String? = nil,
        host: String? = nil,
        scope: [String] = [],
        idleFor: String? = nil,
        wait: String? = nil,
        fallback: String? = nil,
        jitter: String? = nil,
        action: MuxaAutomationAction,
        text: String? = nil,
        message: String? = nil,
        submit: Bool = true,
        maxPerHour: Int = MuxaAutomationRule.defaultMaxPerHour,
        cooldown: String = MuxaAutomationRule.defaultCooldown,
        onlyIfStill: MuxaAutomationCondition? = nil
    ) {
        self.name = name
        self.on = on
        self.enabled = enabled
        self.action = action
        self.agent = agent
        self.workspace = workspace
        self.work = work
        self.pane = pane
        self.host = host
        self.scope = scope
        self.idleFor = idleFor
        self.text = text
        self.message = message
        self.submit = submit
        self.wait = wait
        self.fallback = fallback
        self.jitter = jitter
        self.maxPerHour = maxPerHour
        self.cooldown = cooldown
        self.onlyIfStill = onlyIfStill
        filters = nil
        firedLastHour = nil
        lastFiredAt = nil
    }

    enum CodingKeys: String, CodingKey {
        case name, on, enabled, action
        case agent, workspace, work, pane, host, scope
        case text, message, submit
        case wait, fallback, jitter, cooldown, filters
        case idleFor = "for"
        case maxPerHour = "max_per_hour"
        case onlyIfStill = "only_if_still"
        case firedLastHour = "fired_last_hour"
        case lastFiredAt = "last_fired_at"
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        name = try values.decodeIfPresent(String.self, forKey: .name) ?? ""
        on = try values.decodeIfPresent(MuxaAutomationEvent.self, forKey: .on) ?? .error
        enabled = try values.decodeIfPresent(Bool.self, forKey: .enabled) ?? true
        action = try values.decodeIfPresent(MuxaAutomationAction.self, forKey: .action) ?? .notify
        agent = try values.decodeIfPresent([String].self, forKey: .agent) ?? []
        workspace = try values.decodeIfPresent(String.self, forKey: .workspace)
        work = try values.decodeIfPresent(String.self, forKey: .work)
        pane = try values.decodeIfPresent(String.self, forKey: .pane)
        host = try values.decodeIfPresent(String.self, forKey: .host)
        scope = try values.decodeIfPresent([String].self, forKey: .scope) ?? []
        idleFor = try values.decodeIfPresent(String.self, forKey: .idleFor)
        text = try values.decodeIfPresent(String.self, forKey: .text)
        message = try values.decodeIfPresent(String.self, forKey: .message)
        submit = try values.decodeIfPresent(Bool.self, forKey: .submit) ?? true
        wait = try values.decodeIfPresent(String.self, forKey: .wait)
        fallback = try values.decodeIfPresent(String.self, forKey: .fallback)
        jitter = try values.decodeIfPresent(String.self, forKey: .jitter)
        maxPerHour = try values.decodeIfPresent(Int.self, forKey: .maxPerHour) ?? Self.defaultMaxPerHour
        cooldown = try values.decodeIfPresent(String.self, forKey: .cooldown) ?? Self.defaultCooldown
        onlyIfStill = try values.decodeIfPresent(MuxaAutomationCondition.self, forKey: .onlyIfStill)
        filters = try values.decodeIfPresent(String.self, forKey: .filters)
        firedLastHour = try values.decodeIfPresent(Int.self, forKey: .firedLastHour)
        lastFiredAt = try values.decodeIfPresent(String.self, forKey: .lastFiredAt)
    }

    /// The `rule` payload of `automation_set_rule`. The derived fields are
    /// never sent, and a key the event or action cannot carry is dropped
    /// rather than sent empty — the daemon validates as strictly as it
    /// loads, so a stray `message` on a `send_prompt` rule is a refusal.
    var wireObject: [String: Any] {
        var object: [String: Any] = [
            "name": name,
            "on": on.rawValue,
            "enabled": enabled,
            "action": action.rawValue,
            "max_per_hour": maxPerHour,
            "cooldown": cooldown,
        ]
        if !agent.isEmpty { object["agent"] = agent }
        if !scope.isEmpty, on.supportsScopeFilter { object["scope"] = scope }
        Self.put(workspace, key: "workspace", in: &object)
        Self.put(work, key: "work", in: &object)
        Self.put(pane, key: "pane", in: &object)
        Self.put(host, key: "host", in: &object)
        if on.requiresDuration { Self.put(idleFor, key: "for", in: &object) }
        Self.put(wait, key: "wait", in: &object)
        if on.supportsResetTiming { Self.put(fallback, key: "fallback", in: &object) }
        Self.put(jitter, key: "jitter", in: &object)
        if action.needsText {
            Self.put(text, key: "text", in: &object)
            object["submit"] = submit
        }
        if action.needsMessage { Self.put(message, key: "message", in: &object) }
        if let onlyIfStill { object["only_if_still"] = onlyIfStill.rawValue }
        return object
    }

    /// The same rule as a `[[automation.rule]]` block. Shown by the editor
    /// so a rule can be pasted into `config.toml` when the daemon in front
    /// of the app cannot write rules yet.
    var tomlSnippet: String {
        var lines = ["[[automation.rule]]"]
        lines.append("name = \(Self.tomlString(name))")
        lines.append("on = \(Self.tomlString(on.rawValue))")
        lines.append("enabled = \(enabled)")
        if !agent.isEmpty {
            lines.append("agent = [\(agent.map(Self.tomlString).joined(separator: ", "))]")
        }
        if !scope.isEmpty, on.supportsScopeFilter {
            lines.append("scope = [\(scope.map(Self.tomlString).joined(separator: ", "))]")
        }
        for (key, value) in [
            ("workspace", workspace), ("work", work), ("pane", pane), ("host", host),
        ] {
            guard let value = Self.trimmed(value) else { continue }
            lines.append("\(key) = \(Self.tomlString(value))")
        }
        if on.requiresDuration, let value = Self.trimmed(idleFor) {
            lines.append("for = \(Self.tomlString(value))")
        }
        if let value = Self.trimmed(wait) {
            lines.append("wait = \(Self.tomlString(value))")
        }
        if on.supportsResetTiming, let value = Self.trimmed(fallback) {
            lines.append("fallback = \(Self.tomlString(value))")
        }
        if let value = Self.trimmed(jitter) {
            lines.append("jitter = \(Self.tomlString(value))")
        }
        lines.append("action = \(Self.tomlString(action.rawValue))")
        if action.needsText, let value = text, !value.isEmpty {
            lines.append("text = \(Self.tomlString(value))")
            lines.append("submit = \(submit)")
        }
        if action.needsMessage, let value = Self.trimmed(message) {
            lines.append("message = \(Self.tomlString(value))")
        }
        lines.append("max_per_hour = \(maxPerHour)")
        lines.append("cooldown = \(Self.tomlString(cooldown))")
        if let onlyIfStill {
            lines.append("only_if_still = \(Self.tomlString(onlyIfStill.rawValue))")
        }
        return lines.joined(separator: "\n") + "\n"
    }

    /// The rule the Automations tab offers as its one-click shortcut: the
    /// session-limit case the whole feature was asked for.
    static var sessionLimitRecommendation: MuxaAutomationRule {
        MuxaAutomationRule(
            name: "resume-after-limit",
            on: .rateLimited,
            wait: "\(MuxaAutomationDuration.resetAnchor)+2m",
            fallback: "20m",
            action: .sendPrompt,
            // Prompt text, not UI chrome: it is typed into the agent.
            text: "continue",
            maxPerHour: 2,
            cooldown: "5m"
        )
    }

    private static func trimmed(_ value: String?) -> String? {
        guard let value = value?.trimmingCharacters(in: .whitespacesAndNewlines),
              !value.isEmpty else { return nil }
        return value
    }

    private static func put(_ value: String?, key: String, in object: inout [String: Any]) {
        guard let value = trimmed(value) else { return }
        object[key] = value
    }

    /// A TOML basic string. Only the escapes TOML requires; rule fields are
    /// short identifiers and prompts, never binary.
    static func tomlString(_ value: String) -> String {
        var escaped = ""
        for character in value {
            switch character {
            case "\\": escaped += "\\\\"
            case "\"": escaped += "\\\""
            case "\n": escaped += "\\n"
            case "\r": escaped += "\\r"
            case "\t": escaped += "\\t"
            default: escaped.append(character)
            }
        }
        return "\"\(escaped)\""
    }

    /// The daemon refuses text carrying terminal control characters: an
    /// automation types it into a live TUI, where an escape sequence turns
    /// "resume" into arbitrary key bindings. Tabs and newlines are fine.
    static func isTerminalSafe(_ text: String) -> Bool {
        text.unicodeScalars.allSatisfy { scalar in
            scalar == "\n" || scalar == "\t" || !CharacterSet.controlCharacters.contains(scalar)
        }
    }
}

/// `automation_rules`: the engine's two switches plus the rules.
struct MuxaAutomationSnapshot: Decodable, Sendable, Equatable {
    var enabled: Bool
    /// RFC3339, exactly as the daemon sent it; nil when not paused.
    var pausedUntilText: String?
    var rules: [MuxaAutomationRule]
    /// The ceiling every rule shares. The daemon states it so the safety
    /// notes quote the guard that is actually in force.
    var globalMaxPerHour: Int

    static let empty = MuxaAutomationSnapshot(enabled: false, pausedUntilText: nil, rules: [])

    init(
        enabled: Bool,
        pausedUntilText: String?,
        rules: [MuxaAutomationRule],
        globalMaxPerHour: Int = 30
    ) {
        self.enabled = enabled
        self.pausedUntilText = pausedUntilText
        self.rules = rules
        self.globalMaxPerHour = globalMaxPerHour
    }

    enum CodingKeys: String, CodingKey {
        case enabled, rules
        case pausedUntilText = "paused_until"
        case globalMaxPerHour = "global_max_per_hour"
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        enabled = try values.decodeIfPresent(Bool.self, forKey: .enabled) ?? true
        pausedUntilText = try values.decodeIfPresent(String.self, forKey: .pausedUntilText)
        rules = try values.decodeIfPresent([MuxaAutomationRule].self, forKey: .rules) ?? []
        globalMaxPerHour = try values.decodeIfPresent(Int.self, forKey: .globalMaxPerHour) ?? 30
    }

    var pausedUntil: Date? {
        MuxaAutomationTime.parse(pausedUntilText)
    }

    /// A pause in the past has expired, so the engine is live again.
    func isPaused(now: Date = Date()) -> Bool {
        guard let until = pausedUntil else { return false }
        return until > now
    }
}

/// What became of one evaluated firing.
enum MuxaAutomationOutcome: Hashable, Sendable {
    case fired
    case skipped
    case failed
    case other(String)

    init(rawValue: String) {
        switch rawValue {
        case "fired": self = .fired
        case "skipped": self = .skipped
        case "failed": self = .failed
        default: self = .other(rawValue)
        }
    }

    var rawValue: String {
        switch self {
        case .fired: "fired"
        case .skipped: "skipped"
        case .failed: "failed"
        case .other(let raw): raw
        }
    }
}

/// One firing, as `automation_log` returns it (newest first).
struct MuxaAutomationLogEntry: Decodable, Hashable, Sendable {
    let rule: String
    let pane: String?
    let agent: String?
    let firedAt: String?
    let action: MuxaAutomationAction?
    let outcome: MuxaAutomationOutcome
    /// The skip reason, the failure, or the text that was sent.
    let detail: String?
    /// The arming episode, so one cap fires a rule once across restarts.
    let episode: String?

    enum CodingKeys: String, CodingKey {
        case rule, pane, agent, action, outcome, detail, episode
        case firedAt = "fired_at"
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        rule = try values.decodeIfPresent(String.self, forKey: .rule) ?? ""
        pane = try values.decodeIfPresent(String.self, forKey: .pane)
        agent = try values.decodeIfPresent(String.self, forKey: .agent)
        firedAt = try values.decodeIfPresent(String.self, forKey: .firedAt)
        action = try values.decodeIfPresent(MuxaAutomationAction.self, forKey: .action)
        outcome = MuxaAutomationOutcome(
            rawValue: try values.decodeIfPresent(String.self, forKey: .outcome) ?? ""
        )
        detail = try values.decodeIfPresent(String.self, forKey: .detail)
        episode = try values.decodeIfPresent(String.self, forKey: .episode)
    }

    var firedDate: Date? { MuxaAutomationTime.parse(firedAt) }

    /// On a skip the detail is a reason token the app can name; on a firing
    /// it is the text that was sent, which is the operator's own.
    var skipReason: String? {
        outcome == .skipped ? detail : nil
    }
}

/// `automation_test`: what a rule would do right now, firing nothing.
struct MuxaAutomationTestReport: Decodable, Hashable, Sendable {
    let rule: String
    let enabled: Bool
    let engineEnabled: Bool
    let pausedUntil: String?
    let candidates: [MuxaAutomationTestCandidate]

    enum CodingKeys: String, CodingKey {
        case rule, enabled, candidates
        case engineEnabled = "engine_enabled"
        case pausedUntil = "paused_until"
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        rule = try values.decodeIfPresent(String.self, forKey: .rule) ?? ""
        enabled = try values.decodeIfPresent(Bool.self, forKey: .enabled) ?? false
        engineEnabled = try values.decodeIfPresent(Bool.self, forKey: .engineEnabled) ?? false
        pausedUntil = try values.decodeIfPresent(String.self, forKey: .pausedUntil)
        candidates = try values.decodeIfPresent(
            [MuxaAutomationTestCandidate].self,
            forKey: .candidates
        ) ?? []
    }

    /// Only the rows the rule would actually act on.
    var firing: [MuxaAutomationTestCandidate] {
        candidates.filter(\.wouldFire)
    }
}

/// One agent the rule was evaluated against.
struct MuxaAutomationTestCandidate: Decodable, Hashable, Sendable {
    let pane: String?
    let agentSessionID: String
    let agent: String
    let state: String
    /// `fire`, or the skip reason.
    let decision: String
    let fireAt: String?
    let detail: String?

    enum CodingKeys: String, CodingKey {
        case pane, agent, state, decision, detail
        case agentSessionID = "agent_session_id"
        case fireAt = "fire_at"
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        pane = try values.decodeIfPresent(String.self, forKey: .pane)
        agentSessionID = try values.decodeIfPresent(String.self, forKey: .agentSessionID) ?? ""
        agent = try values.decodeIfPresent(String.self, forKey: .agent) ?? ""
        state = try values.decodeIfPresent(String.self, forKey: .state) ?? ""
        decision = try values.decodeIfPresent(String.self, forKey: .decision) ?? ""
        fireAt = try values.decodeIfPresent(String.self, forKey: .fireAt)
        detail = try values.decodeIfPresent(String.self, forKey: .detail)
    }

    var wouldFire: Bool { decision == "fire" }
    var fireDate: Date? { MuxaAutomationTime.parse(fireAt) }
}

// MARK: - Durations

/// The daemon's duration grammar, mirrored so the editor can refuse a value
/// before the daemon does: `45s`, `5m`, `2h`, `1d`, and a bare `0`.
///
/// A bare number is refused **on purpose** — `20` reads as seconds to one
/// operator and minutes to the next, and the difference between those two
/// is a runaway.
enum MuxaAutomationDuration {
    /// The daemon refuses a configured duration longer than this.
    static let maximumSeconds: TimeInterval = 24 * 60 * 60

    /// Seconds for `45s` / `5m` / `2h` / `1d` / `0`, or nil when the text is
    /// not one. An empty string is *not* a duration; callers decide whether
    /// an absent value is allowed.
    static func parse(_ text: String) -> TimeInterval? {
        let trimmed = text.trimmingCharacters(in: .whitespaces)
        if trimmed.isEmpty { return nil }
        if trimmed == "0" { return 0 }
        guard let unit = trimmed.last, let seconds = secondsPerUnit(unit) else { return nil }
        let digits = trimmed.dropLast()
        guard !digits.isEmpty, digits.allSatisfy({ $0.isASCII && $0.isNumber }),
              let amount = Double(digits) else { return nil }
        return amount * seconds
    }

    /// `wait`'s grammar, which adds the reset-relative forms. The offset may
    /// be negative — `reset-30s` acts just before the window reopens.
    /// The daemon spells the anchor `{{reset}}`, the way it spells every
    /// value it fills in from context. Builds before that wrote a bare
    /// `reset`, and muxad still loads those, so both are read here.
    static let resetAnchor = "{{reset}}"

    static func parseWait(_ text: String) -> MuxaAutomationWait? {
        let trimmed = text.trimmingCharacters(in: .whitespaces)
        let anchor: String
        if trimmed.hasPrefix(resetAnchor) {
            anchor = resetAnchor
        } else if trimmed.hasPrefix("reset") {
            anchor = "reset"
        } else {
            guard let seconds = parse(trimmed) else { return nil }
            return .delay(seconds)
        }
        let rest = String(trimmed.dropFirst(anchor.count)).trimmingCharacters(in: .whitespaces)
        if rest.isEmpty { return .afterReset(0) }
        let sign: TimeInterval
        switch rest.first {
        case "+": sign = 1
        case "-": sign = -1
        default: return nil
        }
        guard let magnitude = parse(String(rest.dropFirst())) else { return nil }
        return .afterReset(sign * magnitude)
    }

    /// The compact spelling the daemon renders back, largest exact unit
    /// first — so a value the app writes round-trips through config.toml.
    static func render(_ seconds: TimeInterval) -> String {
        let total = Int(seconds.rounded())
        if total == 0 { return "0s" }
        if total % 86_400 == 0 { return "\(total / 86_400)d" }
        if total % 3_600 == 0 { return "\(total / 3_600)h" }
        if total % 60 == 0 { return "\(total / 60)m" }
        return "\(total)s"
    }

    private static func secondsPerUnit(_ unit: Character) -> TimeInterval? {
        switch unit {
        case "s": 1
        case "m": 60
        case "h": 3600
        case "d": 86_400
        default: nil
        }
    }
}

/// What a rule's `wait` means, once parsed.
enum MuxaAutomationWait: Hashable, Sendable {
    /// A plain duration after the event; zero is "as soon as it matches".
    case delay(TimeInterval)
    /// `reset` (offset 0), `reset+2m`, or `reset-30s`, relative to the
    /// cap's own reset time.
    case afterReset(TimeInterval)

    /// A reset anchor only means something on `rate_limited`; the daemon
    /// refuses one anywhere else.
    var needsResetTime: Bool {
        if case .afterReset = self { return true }
        return false
    }
}

/// RFC3339 in and out, with and without fractional seconds.
enum MuxaAutomationTime {
    static func parse(_ value: String?) -> Date? {
        guard let value, !value.isEmpty else { return nil }
        if let date = try? Date.ISO8601FormatStyle(includingFractionalSeconds: true).parse(value) {
            return date
        }
        return try? Date.ISO8601FormatStyle().parse(value)
    }

    static func text(_ date: Date) -> String {
        date.formatted(Date.ISO8601FormatStyle())
    }

    /// "Until tomorrow" is the next day at 09:00 local time — the start of
    /// the next working morning, not an opaque 24-hour offset.
    static func tomorrowMorning(
        from now: Date = Date(),
        calendar: Calendar = .current,
        hour: Int = 9
    ) -> Date {
        let nextDay = calendar.date(byAdding: .day, value: 1, to: now) ?? now.addingTimeInterval(86_400)
        return calendar.date(
            bySettingHour: hour,
            minute: 0,
            second: 0,
            of: nextDay
        ) ?? nextDay
    }
}

// MARK: - Transport

private struct MuxaAutomationEnvelope: Decodable {
    let ok: Bool?
    let error: String?
    let automationRules: MuxaAutomationSnapshot?
    let automationLog: [MuxaAutomationLogEntry]?
    let automationTest: MuxaAutomationTestReport?

    enum CodingKeys: String, CodingKey {
        case ok, error
        case automationRules = "automation_rules"
        case automationLog = "automation_log"
        case automationTest = "automation_test"
    }
}

/// The `automation_*` requests, on their own connection.
///
/// `MuxaIPCClient`'s exchange is private to that file, so — like
/// `MuxaWorkComposeClient` — this opens its own serialized transport. The
/// calls are short; the separate queue only keeps a settings pane from
/// queueing behind a terminal read.
final class MuxaAutomationClient: Sendable {
    static let requestTimeout: TimeInterval = 5

    let socketPath: String
    private let transport: SerializedIPCTransport

    init(socketPath: String) {
        self.socketPath = socketPath
        transport = SerializedIPCTransport(label: "dev.muxa.mac.ipc-automation") { path, payload in
            try UnixSocket.request(path: path, payload: payload, timeout: Self.requestTimeout)
        }
    }

    /// Test seam: exchanges go through `request` instead of the socket.
    init(socketPath: String, request: @escaping MuxaIPCRequestHandler) {
        self.socketPath = socketPath
        transport = SerializedIPCTransport(label: "dev.muxa.mac.ipc-automation-test", handler: request)
    }

    // MARK: Request bodies (pure, so the wire shape is unit-tested)

    static func listRequest() -> [String: Any] {
        ["protocol": MuxaIPCClient.protocolVersion, "kind": "automation_list"]
    }

    static func logRequest(limit: Int) -> [String: Any] {
        ["protocol": MuxaIPCClient.protocolVersion, "kind": "automation_log", "limit": limit]
    }

    /// `name` nil is the engine's own switch: the daemon flips
    /// `[automation] enabled` live and writes it back to config.toml.
    static func setEnabledRequest(name: String?, enabled: Bool) -> [String: Any] {
        var body: [String: Any] = [
            "protocol": MuxaIPCClient.protocolVersion,
            "kind": "automation_set_enabled",
            "enabled": enabled,
        ]
        if let name { body["name"] = name }
        return body
    }

    /// `until` nil is sent as JSON `null` — that is how the daemon reads
    /// "resume now", distinct from the key being absent.
    static func pauseRequest(until: String?) -> [String: Any] {
        [
            "protocol": MuxaIPCClient.protocolVersion,
            "kind": "automation_pause",
            "until": until ?? NSNull(),
        ]
    }

    static func setRuleRequest(_ rule: MuxaAutomationRule) -> [String: Any] {
        [
            "protocol": MuxaIPCClient.protocolVersion,
            "kind": "automation_set_rule",
            "rule": rule.wireObject,
        ]
    }

    static func removeRuleRequest(name: String) -> [String: Any] {
        [
            "protocol": MuxaIPCClient.protocolVersion,
            "kind": "automation_remove_rule",
            "name": name,
        ]
    }

    static func testRequest(name: String) -> [String: Any] {
        [
            "protocol": MuxaIPCClient.protocolVersion,
            "kind": "automation_test",
            "name": name,
        ]
    }

    // MARK: Calls

    func list() async throws -> MuxaAutomationSnapshot {
        try await snapshot(Self.listRequest())
    }

    func log(limit: Int) async throws -> [MuxaAutomationLogEntry] {
        let response = try await call(Self.logRequest(limit: limit))
        return response.automationLog ?? []
    }

    func setEnabled(name: String?, enabled: Bool) async throws -> MuxaAutomationSnapshot {
        try await snapshot(Self.setEnabledRequest(name: name, enabled: enabled))
    }

    func pause(until: String?) async throws -> MuxaAutomationSnapshot {
        try await snapshot(Self.pauseRequest(until: until))
    }

    func setRule(_ rule: MuxaAutomationRule) async throws -> MuxaAutomationSnapshot {
        try await snapshot(Self.setRuleRequest(rule))
    }

    func removeRule(name: String) async throws -> MuxaAutomationSnapshot {
        try await snapshot(Self.removeRuleRequest(name: name))
    }

    /// Evaluates a rule against the live registry and reports what it would
    /// do. Fires nothing and records nothing.
    func test(name: String) async throws -> MuxaAutomationTestReport {
        let response = try await call(Self.testRequest(name: name))
        guard let report = response.automationTest else {
            throw MuxaIPCError.missingField("automation_test")
        }
        return report
    }

    private func snapshot(_ object: [String: Any]) async throws -> MuxaAutomationSnapshot {
        let response = try await call(object)
        guard let rules = response.automationRules else {
            throw MuxaIPCError.missingField("automation_rules")
        }
        return rules
    }

    private func call(_ object: [String: Any]) async throws -> MuxaAutomationEnvelope {
        let payload = try JSONSerialization.data(withJSONObject: object)
        let data = try await transport.request(
            path: socketPath,
            payload: payload,
            timeout: Self.requestTimeout
        )
        let response = try JSONDecoder().decode(MuxaAutomationEnvelope.self, from: data)
        if response.ok == false {
            throw MuxaIPCError.server(response.error ?? "muxad rejected the request")
        }
        return response
    }
}

extension MuxaIPCClient {
    static let automationCapability = "automation_v1"

    nonisolated func makeAutomationClient() -> MuxaAutomationClient {
        MuxaAutomationClient(socketPath: socketPath)
    }

    /// Throws rather than returning an empty list when the daemon predates
    /// the engine: the pane shows "update muxa" instead of "no rules".
    private func requireAutomation() throws -> MuxaAutomationClient {
        guard supports(Self.automationCapability) else {
            throw MuxaIPCError.server(
                "muxad does not support automation rules; update muxa and restart muxad"
            )
        }
        return makeAutomationClient()
    }

    func automationList() async throws -> MuxaAutomationSnapshot {
        try await requireAutomation().list()
    }

    func automationLog(limit: Int) async throws -> [MuxaAutomationLogEntry] {
        try await requireAutomation().log(limit: limit)
    }

    func automationSetEnabled(name: String?, enabled: Bool) async throws -> MuxaAutomationSnapshot {
        try await requireAutomation().setEnabled(name: name, enabled: enabled)
    }

    func automationPause(until: String?) async throws -> MuxaAutomationSnapshot {
        try await requireAutomation().pause(until: until)
    }

    func automationSetRule(_ rule: MuxaAutomationRule) async throws -> MuxaAutomationSnapshot {
        try await requireAutomation().setRule(rule)
    }

    func automationRemoveRule(name: String) async throws -> MuxaAutomationSnapshot {
        try await requireAutomation().removeRule(name: name)
    }

    func automationTest(name: String) async throws -> MuxaAutomationTestReport {
        try await requireAutomation().test(name: name)
    }
}
