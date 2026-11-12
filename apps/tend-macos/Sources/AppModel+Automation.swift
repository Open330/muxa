import Foundation
import SwiftUI

// MARK: - Editing a rule

/// A duration field of the rule editor, so a validation issue can name the
/// field it belongs to without assembling a sentence from fragments.
enum TendAutomationDurationField: String, Hashable, Sendable {
    case idleFor = "for"
    case wait
    case fallback
    case jitter
    case cooldown

    /// The label the sheet puts in front of the field.
    var title: String {
        switch self {
        case .idleFor: String(localized: "Idle for")
        case .wait: String(localized: "Wait")
        case .fallback: String(localized: "Fallback")
        case .jitter: String(localized: "Jitter")
        case .cooldown: String(localized: "Cooldown")
        }
    }
}

/// Why the rule editor cannot save yet.
///
/// Every case mirrors a refusal in `AutomationRule::validate`, so the sheet
/// says what the daemon would say — before the round trip, and in Korean.
enum TendAutomationRuleIssue: Hashable, Sendable {
    case missingName
    case invalidName
    case nameTooLong
    case duplicateName
    case missingText
    case invalidAccount
    case textNotTerminalSafe
    case textTooLong
    case missingMessage
    case missingDuration(TendAutomationDurationField)
    case invalidDuration(TendAutomationDurationField)
    case durationTooLong(TendAutomationDurationField)
    case zeroIdleDuration
    case invalidWait
    case resetWaitNeedsRateLimit
    case invalidWorkRegex
    case invalidHost
    case invalidMaxPerHour
    case invalidAskCondition(String)

    var message: String {
        switch self {
        case .invalidAskCondition(let message):
            message
        case .missingName:
            String(localized: "Give this rule a name.")
        case .invalidName:
            String(localized: "Names may contain letters, digits, hyphens, underscores and dots only.")
        case .nameTooLong:
            String(localized: "Names may be at most 64 characters.")
        case .duplicateName:
            String(localized: "A rule with this name already exists.")
        case .missingText:
            String(localized: "Send prompt needs the text to type.")
        case .invalidAccount:
            String(localized: "Account must be one configured or aas account name, such as work@claude, or empty for the best account with quota left.")
        case .textNotTerminalSafe:
            String(localized: "Text may not contain terminal control characters — it is typed into a live agent. Tabs and newlines are fine.")
        case .textTooLong:
            String(localized: "Text may be at most 4096 bytes.")
        case .missingMessage:
            String(localized: "Notify needs the message to post.")
        case .missingDuration(let field):
            String(localized: "\(field.title) is required for this event.")
        case .invalidDuration(let field):
            String(localized: "\(field.title) must be a duration such as 30s, 5m, 2h or 1d.")
        case .durationTooLong(let field):
            String(localized: "\(field.title) may not exceed 24 hours.")
        case .zeroIdleDuration:
            String(localized: "Idle for must be greater than zero.")
        case .invalidWait:
            String(localized: "Wait must be a duration such as 5m, or an offset from the limit reset.")
        case .resetWaitNeedsRateLimit:
            String(localized: "Only a rate limit carries a reset time, so a reset wait needs that event.")
        case .invalidWorkRegex:
            String(localized: "Work id matches must be a valid regular expression.")
        case .invalidHost:
            String(localized: "Host must be local, tmux, cmux, rmux, zellij or herdr.")
        case .invalidMaxPerHour:
            String(localized: "Firings per hour must be between 1 and 60.")
        }
    }
}

/// The rule editor's fields. Durations stay text so the operator can type
/// the daemon's own grammar; `issues` is pure, so the validation table is
/// unit-tested rather than clicked through.
struct TendAutomationRuleDraft: Hashable, Sendable {
    /// The name this draft replaces, when editing; nil when adding.
    var originalName: String?
    var name = ""
    var event = TendAutomationEvent.rateLimited
    var enabled = true
    var agents: Set<String> = []
    var scopes: Set<String> = []
    var workspace = ""
    var work = ""
    var pane = ""
    var host = ""
    var notification: String?
    /// Kept through an edit; the editor has no field for it yet.
    var screen: String?
    var idleFor = ""
    var wait = ""
    var fallback = ""
    var jitter = ""
    var action = TendAutomationAction.sendPrompt
    var text = ""
    var message = ""
    var submit = true
    /// `swap_account`'s target; empty means `auto`.
    var account = ""
    var maxPerHour = TendAutomationRule.defaultMaxPerHour
    var cooldown = TendAutomationRule.defaultCooldown
    /// nil keeps the daemon's default: the condition that armed the rule.
    var onlyIfStill: TendAutomationCondition?
    var askCondition: TendAutomationAskCondition?

    /// The agent kinds the picker offers, as tendd stores them. Shorthand
    /// (`claude`, `agy`) is accepted on the wire but normalised
    /// to these, so the app writes the canonical spelling from the start.
    static let agentKinds = ["claude_code", "codex", "opencode", "antigravity"]
    /// `RateLimitScope`, for the rate-limit event's window filter.
    static let rateLimitScopes = ["five_hour", "seven_day", "unknown"]
    /// `local` is every pane this daemon governs; the rest narrow to one
    /// pane-id namespace.
    static let hosts = ["local", "tmux", "cmux", "rmux", "zellij", "herdr"]

    var trimmedName: String { name.trimmingCharacters(in: .whitespacesAndNewlines) }

    /// A TOML bare key as the daemon spells it: ASCII letters, digits, `-`,
    /// `_` and `.`, at least one.
    static func isValidName(_ name: String) -> Bool {
        !name.isEmpty && name.allSatisfy { character in
            character.isASCII
                && (character.isLetter || character.isNumber
                    || character == "_" || character == "-" || character == ".")
        }
    }

    func issues(existingNames: Set<String>) -> [TendAutomationRuleIssue] {
        var issues: [TendAutomationRuleIssue] = []
        let name = trimmedName
        if name.isEmpty {
            issues.append(.missingName)
        } else if !Self.isValidName(name) {
            issues.append(.invalidName)
        } else if name.count > TendAutomationRule.maximumNameLength {
            issues.append(.nameTooLong)
        } else if name != originalName, existingNames.contains(name) {
            issues.append(.duplicateName)
        }

        issues.append(contentsOf: actionIssues)
        if let error = askCondition?.validationError {
            issues.append(.invalidAskCondition(error))
        }

        if event.requiresDuration {
            let value = idleFor.trimmingCharacters(in: .whitespaces)
            if value.isEmpty {
                issues.append(.missingDuration(.idleFor))
            } else if let seconds = TendAutomationDuration.parse(value) {
                if seconds > TendAutomationDuration.maximumSeconds {
                    issues.append(.durationTooLong(.idleFor))
                } else if seconds == 0 {
                    issues.append(.zeroIdleDuration)
                }
            } else {
                issues.append(.invalidDuration(.idleFor))
            }
        }

        if !wait.trimmingCharacters(in: .whitespaces).isEmpty {
            switch TendAutomationWaitText.parse(wait) {
            case nil:
                issues.append(.invalidWait)
            case .afterReset(let offset):
                if !event.supportsResetTiming {
                    issues.append(.resetWaitNeedsRateLimit)
                } else if abs(offset) > TendAutomationDuration.maximumSeconds {
                    issues.append(.durationTooLong(.wait))
                }
            case .delay(let seconds):
                if seconds > TendAutomationDuration.maximumSeconds {
                    issues.append(.durationTooLong(.wait))
                }
            }
        }

        if event.supportsResetTiming {
            issues.append(contentsOf: durationIssues(fallback, field: .fallback))
        }
        issues.append(contentsOf: durationIssues(jitter, field: .jitter))
        issues.append(contentsOf: durationIssues(cooldown, field: .cooldown))

        let work = self.work.trimmingCharacters(in: .whitespacesAndNewlines)
        if !work.isEmpty, (try? NSRegularExpression(pattern: work)) == nil {
            issues.append(.invalidWorkRegex)
        }
        let host = self.host.trimmingCharacters(in: .whitespaces)
        if !host.isEmpty, !Self.hosts.contains(host) {
            issues.append(.invalidHost)
        }

        if !(1...TendAutomationRule.maximumMaxPerHour).contains(maxPerHour) {
            issues.append(.invalidMaxPerHour)
        }
        return issues
    }

    /// The action's payload is exclusive: `send_prompt` carries `text` and
    /// `submit`, `notify` carries `message`, `interrupt` carries neither.
    private var actionIssues: [TendAutomationRuleIssue] {
        if action.carriesText {
            let text = self.text.trimmingCharacters(in: .whitespacesAndNewlines)
            var issues: [TendAutomationRuleIssue] = []
            if action.carriesAccount, !Self.isValidAccount(account) {
                issues.append(.invalidAccount)
            }
            if text.isEmpty { return action.needsText ? issues + [.missingText] : issues }
            if self.text.utf8.count > TendAutomationRule.maximumTextBytes {
                issues.append(.textTooLong)
            }
            if !TendAutomationRule.isTerminalSafe(self.text) {
                issues.append(.textNotTerminalSafe)
            }
            return issues
        }
        if action.needsMessage {
            return message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                ? [.missingMessage]
                : []
        }
        return []
    }

    /// Every optional duration may be empty; anything else must parse and
    /// stay inside the daemon's 24-hour ceiling.
    private func durationIssues(
        _ value: String,
        field: TendAutomationDurationField
    ) -> [TendAutomationRuleIssue] {
        let trimmed = value.trimmingCharacters(in: .whitespaces)
        if trimmed.isEmpty { return [] }
        guard let seconds = TendAutomationDuration.parse(trimmed) else {
            return [.invalidDuration(field)]
        }
        return seconds > TendAutomationDuration.maximumSeconds ? [.durationTooLong(field)] : []
    }

    /// Empty (`auto`) or one aas account name: the daemon refuses blank
    /// and whitespace-bearing names.
    static func isValidAccount(_ account: String) -> Bool {
        let trimmed = account.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty || !trimmed.contains(where: \.isWhitespace)
    }

    func isReady(existingNames: Set<String>) -> Bool {
        issues(existingNames: existingNames).isEmpty
    }

    /// What `wait` means, for the sheet's plain-language preview. An empty
    /// field is the event's own default, not "immediately": a `rate_limited`
    /// rule that names no `wait` fires when the cap resets.
    var timing: TendAutomationWait? {
        let trimmed = wait.trimmingCharacters(in: .whitespaces)
        if trimmed.isEmpty {
            // Another account is the way around the cap, not a reason to
            // wait it out: a swap fires on the event unless told otherwise.
            return action == .swapAccount ? .delay(0) : event.defaultWait
        }
        return TendAutomationWaitText.parse(trimmed)
    }

    /// Seconds `fallback` names, or the daemon's default when it names none.
    var fallbackSeconds: TimeInterval? {
        guard event.supportsResetTiming else { return nil }
        let trimmed = fallback.trimmingCharacters(in: .whitespaces)
        if trimmed.isEmpty {
            return TendAutomationDuration.parse(TendAutomationRule.defaultFallback)
        }
        return TendAutomationDuration.parse(trimmed)
    }

    /// True while `fallback` is showing the daemon's default rather than a
    /// value the rule names.
    var fallbackIsDefault: Bool {
        fallback.trimmingCharacters(in: .whitespaces).isEmpty
    }

    var jitterSeconds: TimeInterval? {
        let trimmed = jitter.trimmingCharacters(in: .whitespaces)
        if trimmed.isEmpty {
            return TendAutomationDuration.parse(TendAutomationRule.defaultJitter)
        }
        return TendAutomationDuration.parse(trimmed)
    }

    var jitterIsDefault: Bool {
        jitter.trimmingCharacters(in: .whitespaces).isEmpty
    }

    /// The rule as the daemon will store it. Fields the chosen event or
    /// action does not use are dropped, so switching the event or action
    /// picker never leaves a stale value behind for the daemon to refuse.
    var rule: TendAutomationRule {
        TendAutomationRule(
            name: trimmedName,
            on: event,
            enabled: enabled,
            agent: Self.sorted(agents),
            workspace: workspace,
            work: work,
            pane: pane,
            host: host,
            notification: notification,
            screen: screen,
            scope: event.supportsScopeFilter ? Self.sorted(scopes) : [],
            idleFor: event.requiresDuration ? idleFor : nil,
            wait: wait,
            fallback: event.supportsResetTiming ? fallback : nil,
            jitter: jitter,
            action: action,
            text: action.carriesText ? text : nil,
            message: action.needsMessage ? message : nil,
            submit: submit,
            account: action.carriesAccount
                ? account.trimmingCharacters(in: .whitespacesAndNewlines)
                : nil,
            maxPerHour: maxPerHour,
            cooldown: cooldown.trimmingCharacters(in: .whitespaces).isEmpty
                ? TendAutomationRule.defaultCooldown
                : cooldown.trimmingCharacters(in: .whitespaces),
            onlyIfStill: onlyIfStill,
            askCondition: askCondition
        )
    }

    static func draft(editing rule: TendAutomationRule) -> TendAutomationRuleDraft {
        TendAutomationRuleDraft(
            originalName: rule.name,
            name: rule.name,
            event: rule.on,
            enabled: rule.enabled,
            agents: Set(rule.agent),
            scopes: Set(rule.scope),
            workspace: rule.workspace ?? "",
            work: rule.work ?? "",
            pane: rule.pane ?? "",
            host: rule.host ?? "",
            notification: rule.notification,
            screen: rule.screen,
            idleFor: rule.idleFor ?? "",
            wait: rule.wait ?? "",
            fallback: rule.fallback ?? "",
            jitter: rule.jitter ?? "",
            action: rule.action,
            text: rule.text ?? "",
            message: rule.message ?? "",
            submit: rule.submit,
            account: rule.account ?? "",
            maxPerHour: rule.maxPerHour,
            cooldown: rule.cooldown,
            onlyIfStill: rule.onlyIfStill,
            askCondition: rule.askCondition
        )
    }

    /// The session-limit shortcut: the recommended rule, already filled in.
    static var sessionLimitDraft: TendAutomationRuleDraft {
        var draft = Self.draft(editing: TendAutomationRule.sessionLimitRecommendation)
        // It is a new rule, not an edit of an existing one.
        draft.originalName = nil
        return draft
    }

    private static func sorted(_ values: Set<String>) -> [String] {
        values.sorted()
    }
}

// MARK: - The reset anchor

/// The reset anchor, and the two spellings the app reads it in.
///
/// tendd writes `{{reset}}+2m` — the `{{…}}` shape it uses for every value a
/// template fills in — and still reads the older bare `reset`. The wire
/// file's duration grammar predates the braces, so the app strips them here
/// rather than teaching the parser a second spelling.
///
/// Reading is all this type does; the one place that *writes* an anchor is
/// `TendAutomationWaitDraft.text`, which composes it from the editor's
/// controls.
enum TendAutomationWaitText {
    /// How tendd spells the anchor, and how the app writes it back.
    static let anchor = "{{reset}}"
    /// The spelling rules written before the braces still carry.
    static let legacyAnchor = "reset"

    /// True while the value anchors on the cap's reset time, in either
    /// spelling — including an anchor whose offset does not parse.
    static func isAnchored(_ text: String) -> Bool {
        let trimmed = text.trimmingCharacters(in: .whitespaces)
        return trimmed.hasPrefix(anchor) || trimmed.hasPrefix(legacyAnchor)
    }

    /// `{{reset}}`, `{{reset}}+2m`, `reset-30s` or `5m` → what it means.
    static func parse(_ text: String) -> TendAutomationWait? {
        TendAutomationDuration.parseWait(withoutBraces(text))
    }

    /// The offset a reset-anchored wait names. Nil when the wait is not
    /// anchored on the reset — or when its offset does not parse.
    static func resetOffset(_ text: String) -> TimeInterval? {
        guard case .afterReset(let offset)? = parse(text) else { return nil }
        return offset
    }

    /// What a row shows beside the anchor chip: the offset in the daemon's
    /// own grammar, or — when it does not parse — the text as it was
    /// written after the anchor. Either way the token itself stays off the
    /// screen, and no part of the value is dropped.
    static func anchorOffsetText(_ text: String) -> String {
        if let offset = resetOffset(text) {
            guard offset != 0 else { return "" }
            return (offset < 0 ? "-" : "+") + TendAutomationDuration.render(abs(offset))
        }
        let trimmed = text.trimmingCharacters(in: .whitespaces)
        for spelling in [anchor, legacyAnchor] where trimmed.hasPrefix(spelling) {
            return String(trimmed.dropFirst(spelling.count))
        }
        return trimmed
    }

    /// The anchor spelled the way the wire file's grammar expects it.
    private static func withoutBraces(_ text: String) -> String {
        let trimmed = text.trimmingCharacters(in: .whitespaces)
        guard trimmed.hasPrefix(anchor) else { return trimmed }
        return legacyAnchor + trimmed.dropFirst(anchor.count)
    }
}

/// The Timing section's anchor control as a value: which anchor the operator
/// picked, which way the offset runs, and the duration they typed.
///
/// It exists so `{{reset}}+2m` is composed by a pure function the tests
/// exercise rather than typed by hand into a text field. A value the
/// controls cannot express — an odd spelling someone wrote in the file —
/// becomes `.freeform` and is handed back exactly as it arrived.
struct TendAutomationWaitDraft: Hashable, Sendable {
    enum Anchor: Hashable, Sendable {
        /// The cap's own reset time, plus or minus the offset.
        case reset
        /// A fixed delay measured from the event.
        case event
        /// A spelling the controls cannot express; kept verbatim.
        case freeform
    }

    var anchor: Anchor = .event
    /// `reset` only: the offset runs backwards from the reset time.
    var isBefore = false
    /// The duration the operator typed. Empty means no offset at all.
    var offset = ""
    /// The value as it was written, while `anchor` is `.freeform`.
    var freeform = ""

    /// Reads a rule's `wait` into the controls. An empty wait is the event's
    /// own default, so the anchor starts where the daemon would put it.
    static func read(_ wait: String, event: TendAutomationEvent) -> Self {
        let trimmed = wait.trimmingCharacters(in: .whitespaces)
        if trimmed.isEmpty {
            return Self(anchor: event.supportsResetTiming ? .reset : .event)
        }
        switch TendAutomationWaitText.parse(trimmed) {
        case .afterReset(let offset):
            return Self(
                anchor: .reset,
                isBefore: offset < 0,
                offset: offset == 0 ? "" : TendAutomationDuration.render(abs(offset))
            )
        case .delay(let seconds):
            return Self(anchor: .event, offset: TendAutomationDuration.render(seconds))
        case nil:
            return Self(anchor: .freeform, freeform: trimmed)
        }
    }

    /// What the controls compose, for `TendAutomationRuleDraft.wait`.
    var text: String {
        switch anchor {
        case .freeform:
            return freeform
        case .event:
            return offset.trimmingCharacters(in: .whitespaces)
        case .reset:
            let magnitude = offset.trimmingCharacters(in: .whitespaces)
            guard !magnitude.isEmpty else { return TendAutomationWaitText.anchor }
            return TendAutomationWaitText.anchor + (isBefore ? "-" : "+") + magnitude
        }
    }

    /// True while a hand-written value could be read by the controls after
    /// all, so the sheet can offer the way back without rewriting anything
    /// on its own.
    var freeformIsReadable: Bool {
        guard anchor == .freeform else { return false }
        let trimmed = freeform.trimmingCharacters(in: .whitespaces)
        return trimmed.isEmpty || TendAutomationWaitText.parse(trimmed) != nil
    }
}

// MARK: - Wire values with a face

/// How an agent kind is drawn: the product's own name and its official
/// logo (`logo`, drawn by `TendAgentLogo`). The logos are the vendors' own
/// marks, used only to identify the product next to its name; their sources
/// are in `Resources/AgentLogos-NOTICE.md`. The SF Symbol and tint remain
/// for a kind with no logo and for places that need a colour, not a mark.
///
/// The symbol comes from `AskProviderEngine` and the tint from
/// `agentProgramTint` wherever those already know the product, so there is
/// one table per question rather than three. Lives here rather than in the
/// Automations pane because any later pane showing an agent kind wants it.
struct TendAgentMark: Hashable, Sendable {
    /// The value on the wire (`claude_code`), which is what a rule stores.
    let wire: String
    /// The product's name. A proper noun, so it is not localized.
    let name: String
    /// The Ask engine that already owns this product's symbol, when there
    /// is one; `antigravity` and `opencode` have no Ask engine yet.
    let engine: AskProviderEngine?
    /// The same product as `agentProgramTint` spells it.
    let program: String
    /// Used only where no Ask engine carries the symbol.
    let ownSymbol: String?

    var symbol: String { engine?.symbolName ?? ownSymbol ?? "terminal" }
    var tint: Color { agentProgramTint(program) }
    /// The product's logo; `logo?.colorName` is its asset name.
    var logo: TendAgentLogoAsset? { TendAgentLogoAsset(kind: program) }

    /// Every agent kind this build can put a face on. A rule may name one
    /// that is not here — the daemon knows agents this build does not — and
    /// that value keeps working under its own wire spelling.
    static let all: [TendAgentMark] = [
        TendAgentMark(
            wire: "claude_code", name: "Claude Code",
            engine: .claude, program: "claude", ownSymbol: nil
        ),
        TendAgentMark(
            wire: "codex", name: "Codex",
            engine: .codex, program: "codex", ownSymbol: nil
        ),
        TendAgentMark(
            wire: "antigravity", name: "Antigravity",
            engine: nil, program: "agy", ownSymbol: "arrow.up.circle"
        ),
        TendAgentMark(
            wire: "opencode", name: "OpenCode",
            engine: nil, program: "opencode", ownSymbol: "terminal"
        ),
    ]

    /// The mark for a wire value, or nil when this build does not know it.
    static func known(for wire: String) -> TendAgentMark? {
        all.first { $0.wire == wire }
    }

    /// What to print for a wire value: the product's name, or the value as
    /// it arrived.
    static func title(for wire: String) -> String {
        known(for: wire)?.name ?? wire
    }

    /// The product's name for an agent kind or program in any spelling
    /// (`claude_code`, `claude`, `agy`), or the kind made readable.
    static func productName(forKind kind: String) -> String {
        known(for: kind)?.name
            ?? TendAgentProgram(configValue: kind)?.displayName
            ?? kind.replacingOccurrences(of: "_", with: " ")
    }
}

/// tendd's `RateLimitScope` in words. A window this build does not know is
/// shown verbatim rather than hidden.
func automationLimitScopeTitle(_ scope: String) -> String {
    switch scope {
    case "five_hour": String(localized: "5-hour limit")
    case "seven_day": String(localized: "Weekly limit")
    case "unknown": String(localized: "Unspecified")
    default: scope
    }
}

// MARK: - Store

/// The automation rules, their switches, and the run log.
///
/// `@Published` state cannot live in an `AppModel` extension, so the
/// Automations pane shares this object. Every mutation replies with the
/// refreshed list, so there is one place the snapshot is replaced.
@MainActor
final class AutomationStore: ObservableObject {
    static let shared = AutomationStore()

    /// What the run log section shows, and what `automation_log` is asked for.
    static let logLimit = 50

    @Published private(set) var snapshot = TendAutomationSnapshot.empty {
        didSet { publishLimitCoverage() }
    }
    @Published private(set) var log: [TendAutomationLogEntry] = []
    @Published private(set) var isSupported = false
    @Published private(set) var isAskSupported = false
    @Published private(set) var hasLoaded = false {
        didSet { publishLimitCoverage() }
    }
    @Published private(set) var isLoading = false
    @Published private(set) var isMutating = false
    @Published private(set) var loadError: String?
    @Published private(set) var actionError: String?
    @Published private(set) var status: String?
    /// A master switch written to `config.toml` but not yet live, because
    /// tendd reads its configuration once at start.
    /// The last `automation_test` report, for the pane's dry-run sheet.
    @Published private(set) var testReport: TendAutomationTestReport?
    @Published private(set) var isTesting = false

    init() {}

    /// Tell every agent status which capped agents are taken care of.
    /// Until a daemon answers, none are.
    private func publishLimitCoverage() {
        TendLimitCoverage.setRules(
            TendLimitCoverage.grants(enabled: masterEnabled, rules: snapshot.rules),
            pausedUntil: snapshot.pausedUntil
        )
    }

    /// Re-read just the rules (no run log): cheap enough for the app's
    /// reconciliation loop, so the limit policy follows edits made in
    /// `config.toml` or by the CLI.
    func refreshRules(model: AppModel) async {
        guard !isMutating, !isLoading else { return }
        guard await model.client.supports(TendIPCClient.automationCapability) else {
            isSupported = false
            // Nothing acts on a cap here, whatever an earlier daemon had.
            if snapshot != .empty { snapshot = .empty }
            return
        }
        isSupported = true
        if let snapshot = try? await model.client.automationList() {
            hasLoaded = true
            if self.snapshot != snapshot { self.snapshot = snapshot }
        }
        await followAutomationSwaps(model: model)
    }

    /// Swaps this daemon's automations started, already followed.
    private var followedSwapOperations: Set<String> = []
    /// How far back a newly seen admitted swap is still worth showing.
    nonisolated static let followSwapsFiredWithin: TimeInterval = 15 * 60

    /// Every account switch shows in the status bar, the ones automation
    /// starts too: a recent admitted `swap_account` firing is followed like
    /// a switch started from the sheet. Read only while a switch rule is on.
    private func followAutomationSwaps(model: AppModel, now: Date = Date()) async {
        guard snapshot.enabled, snapshot.rules.contains(where: { $0.enabled && $0.action == .swapAccount }),
              let entries = try? await model.client.automationLog(limit: 20) else { return }
        for entry in Self.swapsToFollow(entries, followed: followedSwapOperations, now: now) {
            guard let key = entry.operation else { continue }
            followedSwapOperations.insert(key)
            model.swaps.follow(
                requestKey: key,
                host: nil,
                title: String(localized: "\(entry.rule) · \(entry.pane ?? "")")
            )
        }
    }

    nonisolated static func swapsToFollow(
        _ entries: [TendAutomationLogEntry], followed: Set<String>, now: Date
    ) -> [TendAutomationLogEntry] {
        entries.filter { entry in
            guard entry.outcome == .admitted, entry.action == .swapAccount,
                  let key = entry.operation, !followed.contains(key),
                  let fired = TendAutomationTime.parse(entry.firedAt) else { return false }
            return now.timeIntervalSince(fired) <= followSwapsFiredWithin
        }
    }

    var rules: [TendAutomationRule] {
        snapshot.rules.sorted { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
    }

    var existingNames: Set<String> {
        Set(snapshot.rules.map(\.name))
    }

    /// The engine's switch, as the daemon reports it.
    var masterEnabled: Bool {
        snapshot.enabled
    }

    func reload(model: AppModel) async {
        isSupported = await model.client.supports(TendIPCClient.automationCapability)
        isAskSupported = await model.client.supports(TendIPCClient.automationAskCapability)
        guard isSupported else {
            hasLoaded = true
            return
        }
        isLoading = true
        defer { isLoading = false }
        loadError = nil
        do {
            snapshot = try await model.client.automationList()
            log = try await model.client.automationLog(limit: Self.logLimit)
            hasLoaded = true
        } catch {
            loadError = error.localizedDescription
        }
    }

    func setEnabled(_ rule: TendAutomationRule, enabled: Bool, model: AppModel) async {
        await mutate(model: model) { client in
            try await client.automationSetEnabled(name: rule.name, enabled: enabled)
        }
    }

    /// `until` nil resumes now.
    func pause(until: Date?, model: AppModel) async {
        await mutate(model: model) { client in
            try await client.automationPause(until: until.map(TendAutomationTime.text))
        }
    }

    @discardableResult
    func save(_ rule: TendAutomationRule, model: AppModel) async -> Bool {
        let issues = TendAutomationRuleDraft.draft(editing: rule).issues(existingNames: existingNames)
        guard issues.isEmpty else {
            actionError = issues.map(\.message).joined(separator: "\n")
            return false
        }
        return await mutate(model: model) { client in
            try await client.automationSetRule(rule)
        }
    }

    @discardableResult
    func remove(_ rule: TendAutomationRule, model: AppModel) async -> Bool {
        await mutate(model: model) { client in
            try await client.automationRemoveRule(name: rule.name)
        }
    }

    /// Asks the daemon what the rule would do against the live registry.
    /// Fires nothing, so it is safe to run on an enabled rule.
    func test(_ rule: TendAutomationRule, model: AppModel) async {
        guard !isTesting else { return }
        isTesting = true
        defer { isTesting = false }
        actionError = nil
        do {
            testReport = try await model.client.automationTest(name: rule.name)
        } catch {
            testReport = nil
            actionError = error.localizedDescription
        }
    }

    func clearTestReport() {
        testReport = nil
    }

    /// Flips the engine itself. The daemon applies it live and writes
    /// `[automation] enabled` back to config.toml, so there is nothing to
    /// hold pending.
    func setMasterEnabled(_ enabled: Bool, model: AppModel) async {
        await mutate(model: model) { client in
            try await client.automationSetEnabled(name: nil, enabled: enabled)
        }
    }

    func clearStatus() {
        status = nil
        actionError = nil
    }

    @discardableResult
    private func mutate(
        model: AppModel,
        _ body: @MainActor (TendIPCClient) async throws -> TendAutomationSnapshot
    ) async -> Bool {
        guard !isMutating else { return false }
        isMutating = true
        defer { isMutating = false }
        actionError = nil
        status = nil
        do {
            snapshot = try await body(model.client)
            hasLoaded = true
            log = (try? await model.client.automationLog(limit: Self.logLimit)) ?? log
            return true
        } catch {
            actionError = error.localizedDescription
            return false
        }
    }
}
