import AppKit
import SwiftUI

// MARK: - What `aas usage --json` reports

/// One meter on an account: a rate-limit window, how much of it is spent,
/// and when it starts over.
struct AasMeter: Decodable, Sendable, Hashable, Identifiable {
    /// The window as aas names it ("5h", "7d"). A wire value shown verbatim.
    let label: String
    /// Percent of the window already used, 0…100.
    let usedPct: Double
    /// Unix milliseconds at which the window resets, when aas knows it.
    let resetMs: Int64?

    var id: String { label }

    /// 0…1 for a bar, clamped so a provider reporting 120% does not overflow.
    var fraction: Double { min(max(usedPct / 100, 0), 1) }

    /// Green while there is room, orange when it is tight, red once spent.
    var tint: Color {
        switch usedPct {
        case ..<70: .green
        case ..<90: .orange
        default: .red
        }
    }

    enum CodingKeys: String, CodingKey {
        case label, usedPct, resetMs
    }

    init(label: String, usedPct: Double, resetMs: Int64?) {
        self.label = label
        self.usedPct = usedPct
        self.resetMs = resetMs
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        label = try values.decodeIfPresent(String.self, forKey: .label) ?? ""
        usedPct = try values.decodeIfPresent(Double.self, forKey: .usedPct) ?? 0
        resetMs = try values.decodeIfPresent(Int64.self, forKey: .resetMs)
    }
}

/// One stored account as `aas usage --json` describes it.
///
/// Every field decodes leniently and `provider` is a free string: aas ships
/// on its own release train, so a Muxa that meets a newer aas must show what
/// it understands rather than refuse the whole answer. An account for a
/// provider this build has never heard of still lists, switches, and opens a
/// shell.
struct AasAccount: Decodable, Sendable, Hashable, Identifiable {
    /// Provider as aas spells it: "claude", "codex", "zai", …
    let provider: String
    /// Globally unique account name ("e-ed@claude"), and what `aas switch`
    /// and `aas export` take as their account argument.
    let name: String
    let email: String?
    /// Whether this is the account every new agent currently starts with.
    let active: Bool
    /// True when the numbers came from aas's cache rather than the provider.
    let cached: Bool
    let fetchedAtMs: Int64?
    let plan: String?
    /// The plan in the words aas shows ("max · 20x").
    let planLabel: String?
    /// aas's own `key=value` summary. Detail for a tooltip, not a row.
    let headline: String?
    /// What went wrong for this one account; the rest still decode.
    let error: String?
    let notes: [String]
    let meters: [AasMeter]
    let remainingPct: Double?

    var id: String { "\(provider)/\(name)" }

    /// The plan as a row should show it, or nil when aas reported none.
    var planText: String? {
        let text = planLabel?.isEmpty == false ? planLabel : plan
        return text?.isEmpty == false ? text : nil
    }

    enum CodingKeys: String, CodingKey {
        case provider, name, email, active, cached, fetchedAtMs
        case plan, planLabel, headline, error, notes, meters, remainingPct
    }

    init(
        provider: String,
        name: String,
        email: String? = nil,
        active: Bool = false,
        cached: Bool = true,
        fetchedAtMs: Int64? = nil,
        plan: String? = nil,
        planLabel: String? = nil,
        headline: String? = nil,
        error: String? = nil,
        notes: [String] = [],
        meters: [AasMeter] = [],
        remainingPct: Double? = nil
    ) {
        self.provider = provider
        self.name = name
        self.email = email
        self.active = active
        self.cached = cached
        self.fetchedAtMs = fetchedAtMs
        self.plan = plan
        self.planLabel = planLabel
        self.headline = headline
        self.error = error
        self.notes = notes
        self.meters = meters
        self.remainingPct = remainingPct
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        provider = try values.decodeIfPresent(String.self, forKey: .provider) ?? ""
        name = try values.decodeIfPresent(String.self, forKey: .name) ?? ""
        email = try values.decodeIfPresent(String.self, forKey: .email)
        active = try values.decodeIfPresent(Bool.self, forKey: .active) ?? false
        cached = try values.decodeIfPresent(Bool.self, forKey: .cached) ?? false
        fetchedAtMs = try values.decodeIfPresent(Int64.self, forKey: .fetchedAtMs)
        plan = try values.decodeIfPresent(String.self, forKey: .plan)
        planLabel = try values.decodeIfPresent(String.self, forKey: .planLabel)
        headline = try values.decodeIfPresent(String.self, forKey: .headline)
        error = try values.decodeIfPresent(String.self, forKey: .error)
        notes = try values.decodeIfPresent([String].self, forKey: .notes) ?? []
        meters = try values.decodeIfPresent([AasMeter].self, forKey: .meters) ?? []
        remainingPct = try values.decodeIfPresent(Double.self, forKey: .remainingPct)
    }
}

/// The accounts of one provider, in the order and under the heading aas
/// itself chose — so Muxa never has to invent a display name for a provider
/// it does not know.
struct AasProviderGroup: Decodable, Sendable, Hashable, Identifiable {
    let provider: String
    let title: String
    let accounts: [AasAccount]

    var id: String { provider }

    enum CodingKeys: String, CodingKey {
        case provider, title, accounts
    }

    init(provider: String, title: String, accounts: [AasAccount]) {
        self.provider = provider
        self.title = title
        self.accounts = accounts
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        provider = try values.decodeIfPresent(String.self, forKey: .provider) ?? ""
        title = try values.decodeIfPresent(String.self, forKey: .title) ?? provider
        accounts = try values.decodeIfPresent([AasAccount].self, forKey: .accounts) ?? []
    }
}

/// One `aas usage --json` answer. Extra top-level fields (aas grows them)
/// are ignored, and a missing `providerGroups` is derived from `accounts`.
struct AasUsageSnapshot: Decodable, Sendable {
    let accounts: [AasAccount]
    let groups: [AasProviderGroup]

    enum CodingKeys: String, CodingKey {
        case accounts, providerGroups
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        let accounts = try values.decodeIfPresent([AasAccount].self, forKey: .accounts) ?? []
        self.accounts = accounts
        let groups = try values.decodeIfPresent([AasProviderGroup].self, forKey: .providerGroups)
        self.groups = groups ?? Self.groups(from: accounts)
    }

    /// Groups an older aas's flat list by provider, first appearance first,
    /// with the provider's own spelling as the heading.
    static func groups(from accounts: [AasAccount]) -> [AasProviderGroup] {
        var order: [String] = []
        var byProvider: [String: [AasAccount]] = [:]
        for account in accounts {
            if byProvider[account.provider] == nil { order.append(account.provider) }
            byProvider[account.provider, default: []].append(account)
        }
        return order.map {
            AasProviderGroup(provider: $0, title: $0, accounts: byProvider[$0] ?? [])
        }
    }
}

// MARK: - Numbers in words

enum AasFormat {
    /// "6%" in the viewer's locale, from a 0…100 percentage.
    static func percent(_ usedPct: Double) -> String {
        (usedPct / 100).formatted(.percent.precision(.fractionLength(0)))
    }

    /// "Resets in 4h 12m" for a window that starts over at `resetMs`.
    /// Nil when aas reported no reset time.
    ///
    /// Whole units only: a countdown to the minute would need a ticking view
    /// for a number nobody reads that closely.
    static func resets(atMilliseconds resetMs: Int64?, now: Date = Date()) -> String? {
        guard let resetMs, resetMs > 0 else { return nil }
        let reset = Date(timeIntervalSince1970: Double(resetMs) / 1000)
        let seconds = Int(reset.timeIntervalSince(now).rounded(.down))
        if seconds <= 0 { return String(localized: "Resets now") }
        if seconds < 60 { return String(localized: "Resets in under a minute") }
        let minutes = seconds / 60
        if minutes < 60 { return String(localized: "Resets in \(minutes)m") }
        let hours = minutes / 60
        if hours < 24 { return String(localized: "Resets in \(hours)h \(minutes % 60)m") }
        return String(localized: "Resets in \(hours / 24)d \(hours % 24)h")
    }
}

// MARK: - `aas export` output

/// Reads the environment `aas export <account>` prints.
///
/// The output is credentials-adjacent, so this is deliberately the only
/// thing in the app that looks at it: it parses `export KEY=value` lines and
/// hands the pairs straight to the new shell. Values are never logged, never
/// written to disk, and never put in a message the UI shows.
enum AasEnvironment {
    /// Every `export KEY=value` line, unquoted. Anything else in the output —
    /// comments, blank lines, a `set -x`, a line aas adds in a later version —
    /// is ignored rather than guessed at.
    static func parse(_ output: String) -> [String: String] {
        var environment: [String: String] = [:]
        for rawLine in output.split(separator: "\n", omittingEmptySubsequences: false) {
            let line = rawLine.trimmingCharacters(in: .whitespaces)
            guard line.hasPrefix("export ") else { continue }
            let assignment = line.dropFirst("export ".count)
                .trimmingCharacters(in: .whitespaces)
            guard let separator = assignment.firstIndex(of: "=") else { continue }
            let key = String(assignment[assignment.startIndex..<separator])
            guard isEnvironmentName(key) else { continue }
            let value = String(assignment[assignment.index(after: separator)...])
            environment[key] = unquote(value)
        }
        return environment
    }

    /// A POSIX environment variable name: letters, digits and `_`, never
    /// starting with a digit. Keeps a malformed line out of the shell.
    static func isEnvironmentName(_ key: String) -> Bool {
        guard let first = key.first else { return false }
        guard first == "_" || (first.isASCII && first.isLetter) else { return false }
        return key.allSatisfy { $0 == "_" || ($0.isASCII && ($0.isLetter || $0.isNumber)) }
    }

    /// Strips one layer of shell quoting. Inside double quotes only the four
    /// characters the shell itself escapes are unescaped, so a Windows-style
    /// path or a literal `\n` survives intact.
    static func unquote(_ value: String) -> String {
        guard value.count >= 2, let first = value.first, let last = value.last,
              first == last, first == "\"" || first == "'" else { return value }
        let inner = String(value.dropFirst().dropLast())
        guard first == "\"" else { return inner }
        let characters = Array(inner)
        var unescaped = ""
        var index = 0
        while index < characters.count {
            let character = characters[index]
            if character == "\\", index + 1 < characters.count,
               "\"\\$`".contains(characters[index + 1]) {
                unescaped.append(characters[index + 1])
                index += 2
                continue
            }
            unescaped.append(character)
            index += 1
        }
        return unescaped
    }
}

/// The native shell Muxa spawns for one account: its own PTY, the app's
/// terminal environment, and the account's variables on top.
enum AasShellLaunch {
    /// Variable naming which account a shell carries. Namespaced under muxa
    /// so it cannot collide with anything aas defines, and it holds the
    /// account's name — never any of its values.
    static let accountVariable = "MUXA_AAS_ACCOUNT"

    static func launch(
        account: AasAccount,
        accountEnvironment: [String: String],
        base: [String: String],
        shell: String,
        appVersion: String,
        home: String
    ) -> MuxaNativeShellLaunch {
        var environment = MuxaNativeShellLaunch.terminalEnvironment(
            base: base,
            shell: shell,
            appVersion: appVersion
        )
        // A PTY of the app's own. A development terminal's tmux markers would
        // make an agent started in this shell believe it runs inside tmux.
        environment.removeValue(forKey: "TMUX")
        environment.removeValue(forKey: "TMUX_PANE")
        for (key, value) in accountEnvironment { environment[key] = value }
        environment[accountVariable] = account.name
        return MuxaNativeShellLaunch(
            command: shell,
            arguments: [],
            cwd: home,
            name: String(localized: "\(account.name) shell"),
            environment: environment
        )
    }
}

// MARK: - The accounts the module holds

/// What `aas usage --json` last said, when it said it, and what went wrong.
///
/// The cached read is cheap, so the pane refreshes it freely; `--fresh`
/// costs a live request per account and only ever runs when the operator
/// asks for it.
@MainActor
final class AasAccountStore: ObservableObject {
    @Published private(set) var groups: [AasProviderGroup] = []
    @Published private(set) var accounts: [AasAccount] = []
    @Published private(set) var lastRefreshedAt: Date?
    @Published private(set) var isRefreshing = false
    @Published private(set) var error: String?

    /// Seconds `aas usage` is given. Live reads talk to every provider, so
    /// they get a much longer rope than a cache read.
    static let cachedTimeout: TimeInterval = 30
    static let liveTimeout: TimeInterval = 120

    func refresh(live: Bool = false) async {
        guard !isRefreshing else { return }
        isRefreshing = true
        defer { isRefreshing = false }
        do {
            let output = try await MuxaModuleProcess.run(
                "aas",
                live ? ["usage", "--json", "--fresh"] : ["usage", "--json"],
                timeout: live ? Self.liveTimeout : Self.cachedTimeout
            )
            guard output.succeeded else {
                error = Self.failureMessage(output)
                return
            }
            let snapshot = try JSONDecoder().decode(
                AasUsageSnapshot.self,
                from: Data(output.stdout.utf8)
            )
            groups = snapshot.groups
            accounts = snapshot.accounts
            lastRefreshedAt = Date()
            error = nil
        } catch {
            // Keep the accounts already on screen: a failed refresh should
            // not empty a pane that still says something true.
            self.error = error.localizedDescription
        }
    }

    func report(_ message: String?) { error = message }

    /// What a failed `aas` run said, in one line. Used for `usage` and
    /// `switch`, whose output is a status message — never for `export`.
    static func failureMessage(_ output: MuxaModuleProcess.Output) -> String {
        let lines = (output.stderr + "\n" + output.stdout)
            .split(separator: "\n")
            .map { $0.trimmingCharacters(in: .whitespaces) }
        if let first = lines.first(where: { !$0.isEmpty }) { return first }
        return String(localized: "aas could not read your accounts.")
    }
}

// MARK: - The module

/// aas — the operator's account switcher — as a Muxa module.
///
/// The module reads what aas already knows and offers the two things the app
/// is in a position to do: switch the active account (with a warning that it
/// is a machine-wide change) and open one of Muxa's own shells carrying an
/// account's environment. It never stores an account, a token or an
/// environment itself; aas owns all of that.
@MainActor
final class AasModule: MuxaModule {
    nonisolated static let identity = MuxaModuleIdentity(
        id: "aas",
        title: "aas — Agent Account Switcher",
        blurb: String(localized: "Your accounts for each agent provider: what every one of them has left, the switch between them, and a shell that starts as one of them."),
        symbolName: "person.2.badge.key",
        executable: "aas",
        homepage: URL(string: "https://github.com/Open330/aas")
    )

    /// One line saying how to get aas, shown where it is missing.
    static var installHint: String {
        String(localized: "Install aas to switch accounts: `curl -fsSL https://raw.githubusercontent.com/open330/aas/main/install.sh | sh`")
    }

    let store = AasAccountStore()
    private(set) var availability: MuxaModuleAvailability = .probing
    /// The first successful probe loads the cached accounts; later probes —
    /// the pane runs one on every appearance — leave them alone.
    private var hasLoadedAccounts = false

    func probe() async {
        let result: Result<MuxaModuleProcess.Output, any Error>
        do {
            result = .success(try await MuxaModuleProcess.run("aas", ["--version"], timeout: 10))
        } catch {
            result = .failure(error)
        }
        availability = Self.availability(from: result)
        guard availability.isAvailable, !hasLoadedAccounts else { return }
        hasLoadedAccounts = true
        await store.refresh()
    }

    /// What `aas --version` says about whether the module can work. Split out
    /// so the "not installed is not an error" rule is testable without aas.
    static func availability(
        from result: Result<MuxaModuleProcess.Output, any Error>
    ) -> MuxaModuleAvailability {
        switch result {
        case .success(let output):
            guard output.succeeded else {
                return .unusable(reason: AasAccountStore.failureMessage(output))
            }
            let version = output.stdout
                .split(separator: "\n")
                .map { $0.trimmingCharacters(in: .whitespaces) }
                .first { !$0.isEmpty }
            return .available(version: version, detail: nil)
        case .failure(let error):
            if let failure = error as? MuxaModuleProcess.Failure, case .notFound = failure {
                return .missing(hint: installHint)
            }
            return .unusable(reason: error.localizedDescription)
        }
    }

    func settingsPane(model: AppModel) -> AnyView {
        AnyView(AasSettingsPane(module: self, store: store, model: model))
    }

    // MARK: Contributed actions

    func actions(for context: MuxaModuleContext, model: AppModel) -> [MuxaModuleAction] {
        switch context {
        case .app:
            appActions(model: model)
        case .agent(let agent):
            switchActions(forAgentKind: agent.agent.kind)
        case .pipeline, .work:
            []
        }
    }

    private func appActions(model: AppModel) -> [MuxaModuleAction] {
        var actions: [MuxaModuleAction] = [
            MuxaModuleAction(
                id: "aas.refresh",
                title: "Refresh accounts",
                symbolName: "arrow.clockwise"
            ) { [store] in
                await store.refresh()
            },
            MuxaModuleAction(
                id: "aas.settings",
                title: "Open Accounts settings",
                symbolName: "gearshape"
            ) {
                Self.openSettings()
            },
        ]
        let disconnected = model.isConnected
            ? nil
            : String(localized: "Muxa is not connected to its daemon.")
        for account in store.accounts {
            actions.append(MuxaModuleAction(
                id: "aas.shell.\(account.id)",
                title: "New shell as \(account.name)…",
                symbolName: "terminal",
                disabledReason: disconnected
            ) { [weak self] in
                await self?.openShell(for: account, model: model)
            })
        }
        return actions
    }

    private func switchActions(forAgentKind kind: String) -> [MuxaModuleAction] {
        let known = Set(store.accounts.map(\.provider))
        guard let provider = Self.provider(forAgentKind: kind, known: known) else { return [] }
        return store.accounts.filter { $0.provider == provider }.map { account in
            MuxaModuleAction(
                id: "aas.switch.\(account.id)",
                title: "Switch \(account.provider) to \(account.name)",
                symbolName: "person.crop.circle.badge.checkmark",
                disabledReason: account.active
                    ? String(localized: "\(account.name) is already the active \(account.provider) account.")
                    : nil
            ) { [weak self] in
                await self?.switchAccount(account)
            }
        }
    }

    /// The aas provider an agent buys its usage from, or nil when nothing
    /// stored matches. Goes through `MuxaAgentMark` so the wire spellings the
    /// daemon uses (`claude_code`, `gemini_cli`) land on the product name aas
    /// knows, and falls back to the kind itself so a provider added to aas
    /// after this build still matches.
    static func provider(forAgentKind kind: String, known: Set<String>) -> String? {
        let candidates = [
            MuxaAgentMark.known(for: kind)?.program,
            kind.lowercased(),
        ]
        return candidates.compactMap { $0 }.first { known.contains($0) }
    }

    /// Opens Settings on the Modules tab, where this module's card lives.
    static func openSettings() {
        MuxaSettingsOpener.select(.modules)
        MuxaSettingsOpener.openLegacySettingsWindow()
    }

    // MARK: Doing the two things

    /// Switches the active account after the operator confirms. `aas switch`
    /// is machine-wide, so the confirmation says so and there is no path that
    /// skips it.
    func switchAccount(_ account: AasAccount) async {
        guard AasSwitchConfirmation.confirm(account) else { return }
        do {
            let output = try await MuxaModuleProcess.run(
                "aas",
                ["switch", account.provider, account.name],
                timeout: 60
            )
            store.report(output.succeeded ? nil : AasAccountStore.failureMessage(output))
        } catch {
            store.report(error.localizedDescription)
        }
        await store.refresh()
    }

    /// Opens one of Muxa's own shells with `aas export <account>`'s variables
    /// in its environment.
    ///
    /// The export output goes from the pipe into the shell's environment and
    /// nowhere else: a failure reports the account's name and not one word of
    /// what aas printed.
    func openShell(for account: AasAccount, model: AppModel) async {
        guard model.isConnected else { return }
        let accountEnvironment: [String: String]
        do {
            let output = try await MuxaModuleProcess.run(
                "aas",
                ["export", account.provider, account.name],
                timeout: 20
            )
            guard output.succeeded else {
                store.report(Self.exportFailureMessage(for: account))
                return
            }
            accountEnvironment = AasEnvironment.parse(output.stdout)
        } catch {
            store.report(Self.exportFailureMessage(for: account))
            return
        }
        guard !accountEnvironment.isEmpty else {
            store.report(Self.exportFailureMessage(for: account))
            return
        }
        let base = ProcessInfo.processInfo.environment
        let launch = AasShellLaunch.launch(
            account: account,
            accountEnvironment: accountEnvironment,
            base: base,
            shell: base["SHELL"].flatMap { $0.isEmpty ? nil : $0 } ?? "/bin/zsh",
            appVersion: Bundle.main
                .object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
                ?? "development",
            home: FileManager.default.homeDirectoryForCurrentUser.path
        )
        do {
            let session = try await model.client.spawnShell(
                command: launch.command,
                arguments: launch.arguments,
                cwd: launch.cwd,
                name: launch.name,
                environment: launch.environment
            )
            model.registerSpawnedSession(session)
            await model.refresh()
            store.report(nil)
        } catch {
            store.report(error.localizedDescription)
        }
    }

    /// The one thing said about a failed export. Deliberately takes no
    /// process output: nothing aas printed for an account may reach the UI.
    static func exportFailureMessage(for account: AasAccount) -> String {
        String(localized: "Muxa could not read the environment for \(account.name). Check it with `aas export \(account.name)` in a terminal.")
    }
}

/// The confirmation in front of every switch. `aas switch` changes the
/// account for the whole machine, which is not what "switch this pane"
/// sounds like, so the alert says what actually happens.
@MainActor
enum AasSwitchConfirmation {
    static func confirm(_ account: AasAccount) -> Bool {
        let alert = NSAlert()
        alert.alertStyle = .warning
        alert.messageText = String(localized: "Switch \(account.provider) to \(account.name)?")
        alert.informativeText = String(localized: "This changes the active account for every new agent on this Mac, not only for this pane. Agents that are already running keep the account they started with.")
        alert.addButton(withTitle: String(localized: "Switch account"))
        alert.addButton(withTitle: String(localized: "Cancel"))
        return alert.runModal() == .alertFirstButtonReturn
    }
}

// MARK: - Settings › Modules › aas

private struct AasSettingsPane: View {
    let module: AasModule
    @ObservedObject var store: AasAccountStore
    @ObservedObject var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            controls
            if let error = store.error {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if store.groups.isEmpty {
                emptyState
            } else {
                ForEach(store.groups) { group in
                    AasProviderSection(group: group, module: module, model: model)
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private var controls: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                Button("Refresh") { Task { await store.refresh() } }
                Button("Refresh live") { Task { await store.refresh(live: true) } }
                if store.isRefreshing {
                    ProgressView().controlSize(.mini)
                }
                Spacer(minLength: 8)
                if let refreshed = store.lastRefreshedAt {
                    Text("Updated \(refreshed.formatted(date: .omitted, time: .shortened))")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
            .controlSize(.small)
            .disabled(store.isRefreshing)
            Text("Refresh reads the usage aas already has. Refresh live asks every provider again — one request per account — so it takes longer.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var emptyState: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("No accounts stored yet.")
                .font(.callout)
            Text("Add one with `aas login <provider> <name>` in a terminal — for example `aas login claude work` — then refresh.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

private struct AasProviderSection: View {
    let group: AasProviderGroup
    let module: AasModule
    @ObservedObject var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(verbatim: group.title)
                .font(.subheadline.weight(.semibold))
            ForEach(group.accounts) { account in
                AasAccountRow(account: account, module: module, model: model)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

private struct AasAccountRow: View {
    let account: AasAccount
    let module: AasModule
    @ObservedObject var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Circle()
                    .fill(account.active ? Color.green : Color.secondary.opacity(0.25))
                    .frame(width: 8, height: 8)
                    .accessibilityLabel(account.active ? Text("Active account") : Text("Not active"))
                VStack(alignment: .leading, spacing: 2) {
                    Text(verbatim: account.name)
                        .font(.body.weight(.medium))
                    if let email = account.email, !email.isEmpty {
                        Text(verbatim: email)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                Spacer(minLength: 8)
                if let plan = account.planText {
                    Text(verbatim: plan)
                        .font(.caption)
                        .padding(.horizontal, 6)
                        .padding(.vertical, 2)
                        .background(Color.accentColor.opacity(0.12), in: Capsule())
                }
                Button("New shell") {
                    Task { await module.openShell(for: account, model: model) }
                }
                .controlSize(.small)
                .disabled(!model.isConnected)
                .help("Opens a Muxa shell that starts as this account.")
            }
            if let error = account.error, !error.isEmpty {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
            }
            ForEach(account.meters) { meter in
                AasMeterRow(meter: meter)
            }
            ForEach(account.notes, id: \.self) { note in
                Text(verbatim: note)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
        .help(headline)
    }

    /// aas's own `key=value` summary of the plan, on hover. Raw provider
    /// detail, so it stays out of the row itself.
    private var headline: Text { Text(verbatim: account.headline ?? "") }
}

private struct AasMeterRow: View {
    let meter: AasMeter

    var body: some View {
        HStack(spacing: 8) {
            Text(verbatim: meter.label)
                .font(.caption.monospaced())
                .foregroundStyle(.secondary)
                .frame(width: 26, alignment: .leading)
            ProgressView(value: meter.fraction)
                .progressViewStyle(.linear)
                .tint(meter.tint)
            Text(verbatim: AasFormat.percent(meter.usedPct))
                .font(.caption.monospacedDigit())
                .frame(width: 40, alignment: .trailing)
            if let resets = AasFormat.resets(atMilliseconds: meter.resetMs) {
                Text(verbatim: resets)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .frame(width: 150, alignment: .leading)
            }
        }
    }
}
