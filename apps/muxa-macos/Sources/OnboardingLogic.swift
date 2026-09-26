import AppKit
import Foundation

// MARK: - Preferences and the launch decision

/// What opens by itself on launch: the full Welcome guide on a first run,
/// a short What's New after an upgrade that has highlights.
enum OnboardingLaunchPresentation: Equatable, Sendable {
    case welcomeGuide
    case whatsNew(version: String)
}

/// Keys and the pure "what opens on launch?" decision. Kept apart from
/// `MuxaPreferences` so the onboarding peer owns its own defaults.
enum OnboardingPreferences {
    /// `CFBundleShortVersionString` of the build whose guide the user
    /// dismissed with "Don't show again" (or whose What's New was shown);
    /// unset until then.
    static let completedVersionKey = "muxa.onboarding.completedVersion"
    /// Scene id of the Welcome window (`openWindow(id:)`).
    static let windowID = "onboarding"
    /// Scene id of the What's New window.
    static let whatsNewWindowID = "whats-new"
    /// Identifier stamped on the guide window so it can be found again.
    static let windowIdentifier = "muxa.onboarding"
    static let whatsNewWindowIdentifier = "muxa.whats-new"

    /// The running app's marketing version; "0" when the bundle has none
    /// (unit-test hosts), which keeps the comparison well defined.
    static var currentVersion: String {
        let version = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
        return version.flatMap { $0.isEmpty ? nil : $0 } ?? "0"
    }

    /// The full guide only when no version was recorded yet. A recorded
    /// version older than the running one gets What's New instead — and only
    /// when some release in between has highlights. Equal or newer recorded
    /// versions stay quiet, so a downgrade does not nag either.
    static func launchPresentation(
        currentVersion: String,
        completedVersion: String?,
        releases: [MuxaWhatsNew.Release] = MuxaWhatsNew.catalog
    ) -> OnboardingLaunchPresentation? {
        guard let completedVersion,
              !completedVersion.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        else { return .welcomeGuide }
        guard compareVersions(completedVersion, currentVersion) == .orderedAscending else { return nil }
        let pending = MuxaWhatsNew.releases(after: completedVersion, through: currentVersion, in: releases)
        return pending.isEmpty ? nil : .whatsNew(version: currentVersion)
    }

    /// Component-wise numeric comparison ("0.1.9" < "0.1.10", "0.2" == "0.2.0").
    /// Non-numeric suffixes such as "-beta" are ignored.
    static func compareVersions(_ lhs: String, _ rhs: String) -> ComparisonResult {
        let left = numericComponents(of: lhs)
        let right = numericComponents(of: rhs)
        for index in 0..<max(left.count, right.count) {
            let leftValue = index < left.count ? left[index] : 0
            let rightValue = index < right.count ? right[index] : 0
            if leftValue < rightValue { return .orderedAscending }
            if leftValue > rightValue { return .orderedDescending }
        }
        return .orderedSame
    }

    private static func numericComponents(of version: String) -> [Int] {
        version
            .trimmingCharacters(in: .whitespacesAndNewlines)
            .split(separator: ".")
            .map { Int($0.prefix { $0.isNumber }) ?? 0 }
    }

    /// The Welcome window must never open inside a test host. Covers the
    /// XCTest runner variables and the Swift Testing ones.
    static func isRunningTests(
        environment: [String: String] = ProcessInfo.processInfo.environment
    ) -> Bool {
        let xctestKeys = ["XCTestConfigurationFilePath", "XCTestBundlePath", "XCTestSessionIdentifier"]
        if xctestKeys.contains(where: { environment[$0] != nil }) { return true }
        return environment.keys.contains { $0.hasPrefix("SWIFT_TESTING") || $0.hasPrefix("XCTesting") }
    }

    /// The full launch decision: nothing inside a test host, otherwise
    /// `launchPresentation` for the stored version.
    static func launchPresentationOnLaunch(
        defaults: UserDefaults = .standard,
        currentVersion: String = currentVersion,
        environment: [String: String] = ProcessInfo.processInfo.environment,
        releases: [MuxaWhatsNew.Release] = MuxaWhatsNew.catalog
    ) -> OnboardingLaunchPresentation? {
        guard !isRunningTests(environment: environment) else { return nil }
        return launchPresentation(
            currentVersion: currentVersion,
            completedVersion: defaults.string(forKey: completedVersionKey),
            releases: releases
        )
    }

    /// The guide's own window, found by the identifier the tracker stamps on
    /// it. Used when the tracked reference is gone.
    @MainActor
    static func existingWindow(identifier: String = windowIdentifier) -> NSWindow? {
        NSApp.windows.first { $0.identifier?.rawValue == identifier }
    }

    static func markCompleted(version: String = currentVersion, defaults: UserDefaults = .standard) {
        defaults.set(version, forKey: completedVersionKey)
    }
}

/// Process-wide guard so a workbench window that is closed and reopened
/// during one run does not offer the guide a second time.
@MainActor
enum OnboardingLaunch {
    static var presentedThisSession = false

    /// Returns what to open exactly once per process. What's New counts as
    /// seen as soon as it is offered, so it never comes back for the same
    /// version; the full guide waits for "Don't show this guide again".
    static func consumeLaunchPresentation() -> OnboardingLaunchPresentation? {
        guard !presentedThisSession,
              let presentation = OnboardingPreferences.launchPresentationOnLaunch()
        else { return nil }
        presentedThisSession = true
        if case .whatsNew(let version) = presentation {
            OnboardingPreferences.markCompleted(version: version)
        }
        return presentation
    }
}

// MARK: - What's New

/// Highlights per release, newest first. Add an entry when a release has
/// something an upgrading user should know about; a release without one
/// upgrades silently.
enum MuxaWhatsNew {
    struct Highlight: Identifiable, Equatable, Sendable {
        let systemImage: String
        let title: String
        let detail: String
        var id: String { title }
    }

    struct Release: Identifiable, Equatable, Sendable {
        let version: String
        let highlights: [Highlight]
        var id: String { version }
    }

    static var catalog: [Release] {
        [
            Release(version: "0.8.54", highlights: [
                Highlight(
                    systemImage: "arrowshape.turn.up.left",
                    title: String(localized: "Answer agents from a notification"),
                    detail: String(localized: "Reply to an agent waiting for input, or mark it read, right from the banner.")
                ),
                Highlight(
                    systemImage: "rectangle.split.2x1",
                    title: String(localized: "Side-by-side diffs"),
                    detail: String(localized: "Switch Changes to a split view and tick files as Viewed; a mark clears when the file changes again.")
                ),
                Highlight(
                    systemImage: "clock.arrow.circlepath",
                    title: String(localized: "Compare before you restore"),
                    detail: String(localized: "Restore Snapshot shows what is missing, what is already running, and what is new since the snapshot.")
                ),
                Highlight(
                    systemImage: "server.rack",
                    title: String(localized: "Every host in one place"),
                    detail: String(localized: "⌘J shows agents from all your hosts, and usage counts an account shared by several Macs once.")
                ),
            ]),
            Release(version: "0.8.53", highlights: [
                Highlight(
                    systemImage: "rectangle.split.3x1",
                    title: String(localized: "An editor-style workbench"),
                    detail: String(localized: "Tabs in the title row, an activity bar with a side bar you toggle with ⌘B, and a status bar. Press ⌘/ for every shortcut.")
                ),
                Highlight(
                    systemImage: "bell.badge",
                    title: String(localized: "Agent notifications"),
                    detail: String(localized: "A banner, a dot, and a Dock badge when an agent waits for you, fails, or finishes a turn. ⌘J jumps to the agent, ⇧⌘J to the next one waiting.")
                ),
                Highlight(
                    systemImage: "sparkles.rectangle.stack",
                    title: String(localized: "Start agents from the app"),
                    detail: String(localized: "⌥⌘T starts your default agent in the focused folder; ⌥⇧⌘T picks the agent, folder, and placement.")
                ),
                Highlight(
                    systemImage: "gauge.with.dots.needle.33percent",
                    title: String(localized: "Usage in the status bar"),
                    detail: String(localized: "See how much of each provider's limit your agents have used.")
                ),
                Highlight(
                    systemImage: "plusminus",
                    title: String(localized: "Changes review"),
                    detail: String(localized: "⌃⇧G shows what agents changed in the working tree; comment on lines and send the review back.")
                ),
                Highlight(
                    systemImage: "clock.arrow.circlepath",
                    title: String(localized: "Session snapshots"),
                    detail: String(localized: "Muxa snapshots your tmux sessions automatically so they can be restored after a restart.")
                ),
            ]),
        ]
    }

    /// Releases newer than `completed` up to and including `current`, newest
    /// first, capped so a long-absent user gets a short list.
    static func releases(
        after completed: String?,
        through current: String,
        in releases: [Release] = catalog,
        limit: Int = 3
    ) -> [Release] {
        let pending = releases.filter { release in
            guard !release.highlights.isEmpty,
                  OnboardingPreferences.compareVersions(release.version, current) != .orderedDescending
            else { return false }
            guard let completed, !completed.trimmingCharacters(in: .whitespaces).isEmpty else { return true }
            return OnboardingPreferences.compareVersions(completed, release.version) == .orderedAscending
        }
        return Array(
            pending
                .sorted { OnboardingPreferences.compareVersions($0.version, $1.version) == .orderedDescending }
                .prefix(limit)
        )
    }

    /// What the What's New window lists for a version: that release when it
    /// has highlights, else the newest release at or below it.
    static func releasesToShow(for version: String, in releases: [Release] = catalog) -> [Release] {
        Self.releases(after: nil, through: version, in: releases, limit: 1)
    }
}

// MARK: - Checklist evaluation (pure, unit-tested)

enum OnboardingCheckStatus: Equatable, Sendable {
    case ready
    case attention
    case unknown

    var systemImage: String {
        switch self {
        case .ready: "checkmark.circle.fill"
        case .attention: "exclamationmark.triangle.fill"
        case .unknown: "questionmark.circle"
        }
    }
}

/// A command the checklist offers to copy or type into a new shell.
struct OnboardingInstallCommand: Identifiable, Equatable, Sendable {
    /// The program it installs or signs in.
    let program: String
    let displayName: String
    let command: String
    var id: String { program + command }
}

enum OnboardingChecklist {
    /// Programs the checklist probes: tmux plus every known agent CLI.
    static var probedPrograms: [String] {
        ["tmux"] + InstalledTools.agentPrograms
    }

    static func connectionStatus(_ state: AppModel.ConnectionState) -> OnboardingCheckStatus {
        switch state {
        case .connected: .ready
        case .connecting: .unknown
        case .failed, .upgradeRequired: .attention
        }
    }

    /// `detected == nil` means the probe has not finished yet.
    static func toolStatus(named name: String, in detected: [InstalledTool]?) -> OnboardingCheckStatus {
        guard let detected else { return .unknown }
        return detected.contains { $0.name == name } ? .ready : .attention
    }

    static func agentTools(in detected: [InstalledTool]?) -> [InstalledTool] {
        (detected ?? []).filter { InstalledTools.agentPrograms.contains($0.name) }
    }

    static func agentsStatus(in detected: [InstalledTool]?) -> OnboardingCheckStatus {
        guard detected != nil else { return .unknown }
        return agentTools(in: detected).isEmpty ? .attention : .ready
    }

    static func askStatus(_ enabled: Bool?) -> OnboardingCheckStatus {
        switch enabled {
        case .some(true): .ready
        case .some(false): .attention
        case .none: .unknown
        }
    }

    static func workFolderStatus(path: String, exists: (String) -> Bool) -> OnboardingCheckStatus {
        let trimmed = path.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return .attention }
        return exists(trimmed) ? .ready : .attention
    }

    static func fleetHostsStatus(remoteHostCount: Int) -> OnboardingCheckStatus {
        remoteHostCount > 0 ? .ready : .attention
    }

    /// tmux comes from Homebrew; there is no other install path worth
    /// suggesting on a Mac.
    static let tmuxInstall = OnboardingInstallCommand(
        program: "tmux",
        displayName: "tmux",
        command: "brew install tmux"
    )

    /// Each provider's own documented install command. Antigravity ships as
    /// an app with its CLI inside, so it has no line here.
    static let agentInstalls: [OnboardingInstallCommand] = [
        OnboardingInstallCommand(
            program: "claude",
            displayName: "Claude Code",
            command: "curl -fsSL https://claude.ai/install.sh | bash"
        ),
        OnboardingInstallCommand(program: "codex", displayName: "Codex", command: "npm install -g @openai/codex"),
        OnboardingInstallCommand(
            program: "gemini",
            displayName: "Gemini CLI",
            command: "npm install -g @google/gemini-cli"
        ),
        OnboardingInstallCommand(
            program: "opencode",
            displayName: "OpenCode",
            command: "curl -fsSL https://opencode.ai/install | bash"
        ),
    ]

    /// Install lines to offer: tmux when it is missing, every agent CLI when
    /// none was found. Nothing while the probe is still running.
    static func installCommands(for detected: [InstalledTool]?) -> (tmux: OnboardingInstallCommand?, agents: [OnboardingInstallCommand]) {
        guard let detected else { return (nil, []) }
        let tmux = detected.contains { $0.name == "tmux" } ? nil : tmuxInstall
        let agents = agentTools(in: detected).isEmpty ? agentInstalls : []
        return (tmux, agents)
    }

    /// Whether an installed agent CLI has signed in, where that can be read
    /// from a file without running the CLI. Only Codex qualifies: `codex
    /// login` always writes `auth.json` under `$CODEX_HOME` (default
    /// `~/.codex`), for ChatGPT and API-key sign-ins alike. Claude Code keeps
    /// its credentials in the Keychain and Gemini and OpenCode also accept
    /// API keys from the environment, so a missing file proves nothing for
    /// them: nil means "cannot tell" and the row stays quiet.
    static func signInStatus(
        program: String,
        home: String,
        environment: [String: String],
        fileExists: (String) -> Bool
    ) -> OnboardingCheckStatus? {
        guard program == "codex" else { return nil }
        let codexHome = environment["CODEX_HOME"].flatMap { $0.isEmpty ? nil : $0 }
            ?? (home as NSString).appendingPathComponent(".codex")
        return fileExists((codexHome as NSString).appendingPathComponent("auth.json")) ? .ready : .attention
    }

    /// The command that signs a CLI in, for the attention hint.
    static func signInCommand(program: String) -> OnboardingInstallCommand? {
        switch program {
        case "codex": OnboardingInstallCommand(program: "codex", displayName: "Codex", command: "codex login")
        default: nil
        }
    }
}

// MARK: - First agent

enum OnboardingFirstAgent {
    /// Why the Start button is disabled; nil when it can start.
    enum Blocker: Equatable, Sendable {
        case notConnected
        case checking
        case noAgentCLI
        case daemonTooOld
        case missingFolder
    }

    /// Order matters: the first problem is the one worth fixing first.
    /// `supportsAgentStart` and `installed` are nil while still being read.
    static func blocker(
        isConnected: Bool,
        supportsAgentStart: Bool?,
        installed: [MuxaAgentProgram]?,
        folderExists: Bool
    ) -> Blocker? {
        guard isConnected else { return .notConnected }
        guard let supportsAgentStart, let installed else { return .checking }
        guard supportsAgentStart else { return .daemonTooOld }
        guard !installed.isEmpty else { return .noAgentCLI }
        guard folderExists else { return .missingFolder }
        return nil
    }

    /// Detected agent CLIs as providers, in the catalog's order.
    static func installedPrograms(in detected: [InstalledTool]?) -> [MuxaAgentProgram]? {
        guard let detected else { return nil }
        let names = Set(detected.map(\.name))
        return MuxaAgentProgram.allCases.filter { names.contains($0.rawValue) }
    }

    /// The default agent (remembered or configured) when it is installed,
    /// else the first installed one.
    static func defaultProgram(
        preferred: MuxaAgentProgram?,
        installed: [MuxaAgentProgram]
    ) -> MuxaAgentProgram? {
        if let preferred, installed.contains(preferred) { return preferred }
        return installed.first
    }

    /// The folder to offer: the Work folder, else the most recent launch
    /// folder, else home.
    static func defaultFolder(workDirectory: String, recent: [String], home: String) -> String {
        let work = workDirectory.trimmingCharacters(in: .whitespacesAndNewlines)
        if !work.isEmpty { return work }
        return recent.first { !$0.isEmpty } ?? home
    }

    /// A first agent gets its own tmux session rather than a window in
    /// whatever session happens to exist; without tmux it runs in a Muxa
    /// terminal. A remembered placement still wins when it fits.
    static func placement(remembered: MuxaAgentPlacementKind?, tmuxInstalled: Bool) -> MuxaAgentPlacementKind {
        guard tmuxInstalled else { return .native }
        return remembered == .native ? .native : .newSession
    }
}

// MARK: - Shortcuts card

enum OnboardingShortcuts {
    /// The keys the guide highlights, looked up in `MuxaShortcutCatalog` so
    /// the titles always match the ⌘/ sheet.
    static let highlightedKeys = ["⌘J", "⌥⌘T", "⌘B", "⌘P", "⌘/"]

    static func highlights(
        in sections: [MuxaShortcutCatalog.Section] = MuxaShortcutCatalog.sections
    ) -> [MuxaShortcutCatalog.Entry] {
        let entries = sections.flatMap(\.entries)
        return highlightedKeys.compactMap { keys in entries.first { $0.keys == keys } }
    }
}
