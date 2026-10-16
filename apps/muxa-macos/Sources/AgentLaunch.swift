import Foundation

// MARK: - Start one agent (New Agent sheet, ⌥⌘T)
//
// Everything here is computed without the daemon so the choices the sheet
// and the quick start make — which agent, where, relative to what — are
// unit-testable. `AppModel+Agents.swift` feeds it the live snapshot and
// sends the resulting request; muxad runs the canonical `muxa agent start`.

/// The providers `muxa agent start --agent` accepts, by their CLI names.
enum MuxaAgentProgram: String, CaseIterable, Codable, Identifiable, Sendable {
    case claude, codex, gemini, agy, opencode

    var id: Self { self }

    var displayName: String {
        switch self {
        case .claude: "Claude"
        case .codex: "Codex"
        case .gemini: "Gemini"
        case .agy: "Antigravity"
        case .opencode: "OpenCode"
        }
    }

    /// The CLI's own aliases, so a config value such as `claude-code` or
    /// `cx` still names a provider.
    init?(configValue: String?) {
        switch configValue?.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() {
        case "claude", "claude_code", "claude-code": self = .claude
        case "codex", "cx": self = .codex
        case "gemini", "gemini_cli", "gemini-cli": self = .gemini
        case "agy", "antigravity": self = .agy
        case "opencode": self = .opencode
        default: return nil
        }
    }
}

/// Where the new agent runs. Raw values are what the defaults store keeps.
enum MuxaAgentPlacementKind: String, CaseIterable, Codable, Identifiable, Sendable {
    /// Split the focused tmux pane.
    case splitPane
    /// A new window in a tmux session.
    case newWindow
    /// A new detached tmux session named after the directory.
    case newSession
    /// A Muxa-owned PTY session on this Mac.
    case native

    var id: Self { self }

    /// `agent_start`'s `placement` value.
    var wireValue: String {
        switch self {
        case .splitPane: "pane"
        case .newWindow: "window"
        case .newSession: "session"
        case .native: "native"
        }
    }

    var title: String {
        switch self {
        case .splitPane: String(localized: "Split pane")
        case .newWindow: String(localized: "New window")
        case .newSession: String(localized: "New session")
        case .native: String(localized: "Muxa terminal")
        }
    }

    /// The `[mcp.guide].placement` value this kind corresponds to.
    init?(guidePlacement: String?) {
        switch guidePlacement {
        case "pane": self = .splitPane
        case "window": self = .newWindow
        case "session": self = .newSession
        default: return nil
        }
    }
}

/// The tmux pane the operator is looking at, as a launch target.
struct MuxaAgentLaunchPaneTarget: Equatable, Sendable {
    let paneID: String
    let sessionID: String
    let sessionName: String
    /// The pane row's short socket name (`default`).
    let socket: String
}

/// What the focused editor says about where a new agent should start.
struct MuxaAgentLaunchFocus: Equatable, Sendable {
    /// Fleet host alias of the focused pane; nil for this Mac.
    var hostAlias: String?
    var cwd: String?
    /// Set only when the focused editor is a tmux pane.
    var pane: MuxaAgentLaunchPaneTarget?

    static let empty = MuxaAgentLaunchFocus()

    /// A focused pane: its directory (the agent's own cwd when tmux has not
    /// reported one) and, for tmux panes, the pane itself as a split target.
    static func pane(
        _ pane: MuxaPaneInfo,
        hostAlias: String,
        isLocalHost: Bool,
        agentCwd: String?
    ) -> MuxaAgentLaunchFocus {
        let cwd = [pane.currentPath, agentCwd ?? ""]
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .first { !$0.isEmpty }
        let target = pane.hostKind == "tmux"
            ? MuxaAgentLaunchPaneTarget(
                paneID: pane.paneID,
                sessionID: pane.stableSessionID,
                sessionName: pane.session,
                socket: pane.endpointSocket
            )
            : nil
        return MuxaAgentLaunchFocus(
            hostAlias: isLocalHost ? nil : hostAlias,
            cwd: cwd,
            pane: target
        )
    }

    /// A focused shell, Work, or agent editor: only a directory.
    static func directory(_ cwd: String?, hostAlias: String? = nil) -> MuxaAgentLaunchFocus {
        let trimmed = cwd?.trimmingCharacters(in: .whitespacesAndNewlines)
        return MuxaAgentLaunchFocus(
            hostAlias: hostAlias,
            cwd: trimmed?.isEmpty == false ? trimmed : nil,
            pane: nil
        )
    }
}

/// The operator's ⌥⌘T default, kept per Mac in UserDefaults. It starts out
/// from `[mcp.guide]` and never writes back to it: that section also steers
/// how agents launch agents over MCP.
struct MuxaAgentLaunchDefaults: Codable, Equatable, Sendable {
    static let storageKey = "muxa.newAgent.default.v1"

    var program: MuxaAgentProgram?
    var placement: MuxaAgentPlacementKind?
    /// Provider arguments that replace the configured ones; nil inherits.
    var options: [String]?

    static func load(from defaults: UserDefaults = .standard) -> MuxaAgentLaunchDefaults {
        guard let data = defaults.data(forKey: storageKey),
              let decoded = try? JSONDecoder().decode(Self.self, from: data) else {
            return MuxaAgentLaunchDefaults()
        }
        return decoded
    }

    func save(to defaults: UserDefaults = .standard) {
        guard let data = try? JSONEncoder().encode(self) else { return }
        defaults.set(data, forKey: Self.storageKey)
    }

    /// The agent a quick start uses: the remembered one, else the configured
    /// launch default. Nil means the operator has to choose.
    func program(guide: MuxaLaunchSettings.LegacyGuide?) -> MuxaAgentProgram? {
        program ?? MuxaAgentProgram(configValue: guide?.program)
    }
}

/// Which placements make sense for a host and focus.
struct MuxaAgentPlacementAvailability: Equatable, Sendable {
    var splitPane: Bool
    var newWindow: Bool
    var newSession = true
    var native: Bool

    /// Split needs a focused tmux pane on the launch host; a new window
    /// needs a tmux session there; the Muxa terminal is this Mac's own.
    init(focus: MuxaAgentLaunchFocus, launchHost: String?, sessionCount: Int) {
        splitPane = focus.pane != nil && focus.hostAlias == launchHost
        newWindow = sessionCount > 0
        native = launchHost == nil
    }

    init(splitPane: Bool, newWindow: Bool, newSession: Bool = true, native: Bool) {
        self.splitPane = splitPane
        self.newWindow = newWindow
        self.newSession = newSession
        self.native = native
    }

    func contains(_ kind: MuxaAgentPlacementKind) -> Bool {
        switch kind {
        case .splitPane: splitPane
        case .newWindow: newWindow
        case .newSession: newSession
        case .native: native
        }
    }

    var kinds: [MuxaAgentPlacementKind] {
        MuxaAgentPlacementKind.allCases.filter(contains)
    }

    /// The remembered placement, else the configured one, degraded along
    /// split → window → session when it is unavailable here. A remembered
    /// Muxa terminal on a remote host falls back to a new session.
    func resolve(
        remembered: MuxaAgentPlacementKind?,
        guidePlacement: String?
    ) -> MuxaAgentPlacementKind {
        let preferred = remembered
            ?? MuxaAgentPlacementKind(guidePlacement: guidePlacement)
            ?? .splitPane
        if contains(preferred) { return preferred }
        let chain: [MuxaAgentPlacementKind] = switch preferred {
        case .splitPane: [.newWindow, .newSession]
        case .newWindow: [.newSession]
        case .newSession, .native: [.newSession]
        }
        return chain.first(where: contains) ?? .newSession
    }
}

/// One `agent_start` request, built from the form (or the quick start).
struct MuxaAgentStartRequest: Equatable, Sendable {
    var program: MuxaAgentProgram
    /// Fleet host alias; nil for this Mac.
    var hostAlias: String?
    var placement: MuxaAgentPlacementKind
    var target: String?
    var tmuxSocket: String?
    var direction: String?
    var cwd: String
    var prompt: String?
    var name: String?
    var role: String?
    /// Nil inherits `[agent.<program>]`; a list replaces it.
    var options: [String]?
    /// Native only: the login PATH and terminal identity for the PTY.
    var environment: [String: String] = [:]

    /// Keys muxad accepts in `env` (its `NATIVE_ENV_KEYS`).
    static let nativeEnvironmentKeys: Set<String> = [
        "PATH", "TERM", "COLORTERM", "TERM_PROGRAM", "TERM_PROGRAM_VERSION",
        "LANG", "LC_ALL", "LC_CTYPE",
    ]

    /// Fills in the target for the placement: the focused pane for a split,
    /// the chosen session for a window, nothing for a new session. Only this
    /// Mac's tmux servers are pinned by socket — a fleet host resolves its
    /// own. The split direction comes from `[mcp.guide].direction`.
    static func make(
        program: MuxaAgentProgram,
        hostAlias: String?,
        placement: MuxaAgentPlacementKind,
        focus: MuxaAgentLaunchFocus,
        windowSession: (id: String, socket: String)?,
        cwd: String,
        prompt: String? = nil,
        name: String? = nil,
        role: String? = nil,
        options: [String]? = nil,
        guideDirection: String? = nil,
        environment: [String: String] = [:]
    ) -> MuxaAgentStartRequest {
        var request = MuxaAgentStartRequest(
            program: program,
            hostAlias: hostAlias,
            placement: placement,
            cwd: cwd,
            prompt: prompt.nonBlank,
            name: name.nonBlank,
            role: placement == .native ? nil : role.nonBlank,
            options: options
        )
        let local = hostAlias == nil
        switch placement {
        case .splitPane:
            request.target = focus.pane?.paneID
            request.tmuxSocket = local ? focus.pane?.socket : nil
            request.direction = guideDirection == "down" ? "down" : "right"
        case .newWindow:
            let session = windowSession
                ?? focus.pane.map { (id: $0.sessionID, socket: $0.socket) }
            request.target = session?.id
            request.tmuxSocket = local ? session?.socket : nil
        case .newSession:
            break
        case .native:
            request.environment = environment.filter { nativeEnvironmentKeys.contains($0.key) }
        }
        return request
    }

    /// The `request` object of an `agent_start` IPC call.
    func ipcPayload() -> [String: Any] {
        var object: [String: Any] = [
            "agent": program.rawValue,
            "placement": placement.wireValue,
            "cwd": cwd,
        ]
        if let hostAlias { object["host"] = hostAlias }
        if let target { object["target"] = target }
        if let tmuxSocket { object["tmux_socket"] = tmuxSocket }
        if let direction { object["direction"] = direction }
        if let prompt { object["prompt"] = prompt }
        if let name { object["name"] = name }
        if let role { object["role"] = role }
        if let options { object["options"] = options }
        if !environment.isEmpty { object["env"] = environment }
        return object
    }

    /// `muxa agent start` argv for an older muxad without `agent_start_v1`,
    /// run by the app itself on this Mac. The same one-word `--flag=value`
    /// shape muxad builds (`agent_control::arguments`).
    func cliArguments() -> [String] {
        let native = placement == .native
        var arguments = [
            "agent", "start", "--json",
            "--agent=\(program.rawValue)",
            "--host=\(native ? "native" : "tmux")",
        ]
        if native {
            // Older CLIs defaulted `--direction` to `right` and then refused
            // it on the native host.
            arguments.append("--direction=auto")
        } else {
            arguments.append("--placement=\(placement.wireValue)")
            if let target { arguments.append("--target=\(target)") }
            if let direction { arguments.append("--direction=\(direction)") }
        }
        arguments.append("--cwd=\(cwd)")
        if let prompt { arguments.append("--prompt=\(prompt)") }
        if let name { arguments.append("--name=\(name)") }
        if !native, let role { arguments.append("--role=\(role)") }
        for option in options ?? [] { arguments.append("--option=\(option)") }
        return arguments
    }
}

/// `agent_start`'s answer: the CLI's envelope plus the host that ran it.
struct MuxaAgentStartResult: Decodable, Equatable, Sendable {
    let host: String
    let agent: String
    let placement: String
    let pane: String?
    let session: String?
    let window: String?
    let name: String?
    let cwd: String
    let promptSupplied: Bool?
    let fleetHost: String?

    enum CodingKeys: String, CodingKey {
        case host, agent, placement, pane, session, window, name, cwd
        case promptSupplied = "prompt_supplied"
        case fleetHost = "fleet_host"
    }

    var isNative: Bool { host == "native" }
}

/// Finds the editor for a freshly started agent once the snapshot has it.
enum MuxaPendingAgentTab {
    /// `.pane` for a tmux agent on `hostAlias`, `.shell` for a native one,
    /// or nil while the refresh has not caught up yet. Pane ids repeat
    /// across hosts, so the host has to match as well.
    static func selection(
        for result: MuxaAgentStartResult,
        hostAlias: String,
        panes: [MuxaWatchPaneIdentity],
        sessionIDs: Set<String>
    ) -> MuxaSidebarSelection? {
        if result.isNative {
            guard let session = result.session, sessionIDs.contains(session) else { return nil }
            return .shell(session)
        }
        guard let pane = result.pane else { return nil }
        return panes
            .first { $0.hostAlias == hostAlias && $0.paneID == pane }
            .map(MuxaSidebarSelection.pane)
    }

    /// Seconds to wait before each refresh: quick at first, then every 2 s
    /// for about 15 s in total.
    static let refreshDelays: [Double] = [0.5, 1, 2, 2, 2, 2, 2, 2, 2]
}

/// Directories agents were started in from this Mac, newest first.
enum MuxaRecentAgentDirectories {
    static let storageKey = "muxa.newAgent.recentDirectories.v1"
    static let limit = 10

    static func adding(_ path: String, to list: [String]) -> [String] {
        let standardized = standardize(path)
        guard !standardized.isEmpty else { return list }
        let rest = list.filter { standardize($0) != standardized }
        return Array(([standardized] + rest).prefix(limit))
    }

    static func load(from defaults: UserDefaults = .standard) -> [String] {
        defaults.stringArray(forKey: storageKey) ?? []
    }

    static func record(_ path: String, in defaults: UserDefaults = .standard) {
        defaults.set(adding(path, to: load(from: defaults)), forKey: storageKey)
    }

    private static func standardize(_ path: String) -> String {
        let trimmed = path.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return "" }
        let standardized = (trimmed as NSString).standardizingPath
        return standardized.count > 1 && standardized.hasSuffix("/")
            ? String(standardized.dropLast())
            : standardized
    }
}

/// The sheet's one-line provider options field.
enum MuxaAgentLaunchOptionsText {
    /// Splits on whitespace, keeping single- or double-quoted runs (and
    /// backslash-escaped characters) together, as a shell would.
    static func split(_ text: String) -> [String] {
        var words: [String] = []
        var current = ""
        var inWord = false
        var quote: Character?
        var escaped = false
        for character in text {
            if escaped {
                current.append(character)
                escaped = false
                continue
            }
            if character == "\\", quote != "'" {
                escaped = true
                inWord = true
                continue
            }
            if let open = quote {
                if character == open { quote = nil } else { current.append(character) }
                continue
            }
            if character == "'" || character == "\"" {
                quote = character
                inWord = true
            } else if character.isWhitespace {
                if inWord { words.append(current) }
                current = ""
                inWord = false
            } else {
                current.append(character)
                inWord = true
            }
        }
        if inWord { words.append(current) }
        return words
    }

    /// The inverse, for showing effective options: words with spaces or
    /// quotes are single-quoted.
    static func join(_ words: [String]) -> String {
        words.map { word in
            let plain = !word.isEmpty && word.allSatisfy { character in
                character.isLetter || character.isNumber || "_-+./:=,@%".contains(character)
            }
            return plain ? word : "'\(word.replacingOccurrences(of: "'", with: "'\\''"))'"
        }
        .joined(separator: " ")
    }
}

private extension Optional where Wrapped == String {
    /// Nil for nil, empty, or whitespace-only text; otherwise the text as is
    /// (a prompt keeps its own leading spaces and newlines).
    var nonBlank: String? {
        guard let self, !self.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return nil }
        return self
    }
}
