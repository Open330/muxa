import Foundation
import Security

/// The code that drives a provider: the CLI argv or the HTTP shape muxad
/// uses to answer an Ask. Engines are closed — muxad owns them — while
/// provider *instances* are open, so several instances may share one engine
/// with their own title, model and API key.
enum AskProviderEngine: String, CaseIterable, Identifiable, Sendable {
    case claude
    case codex
    case gemini
    case anthropic
    case openai

    var id: String { rawValue }

    var kind: MuxaAskProvider.Kind {
        switch self {
        case .claude, .codex, .gemini: .cli
        case .anthropic, .openai: .api
        }
    }

    /// Name shown in the engine picker. Product names, not translated.
    var title: String {
        switch self {
        case .claude: "Claude Code"
        case .codex: "Codex CLI"
        case .gemini: "Gemini CLI"
        case .anthropic: "Anthropic API"
        case .openai: "OpenAI API"
        }
    }

    /// Executable an instance runs unless it overrides `executable`.
    var defaultExecutable: String? {
        kind == .cli ? rawValue : nil
    }

    /// Environment variable muxad reads the key from when the instance does
    /// not name its own `api_key_env`.
    var defaultCredentialEnv: String {
        switch self {
        case .claude, .anthropic: "ANTHROPIC_API_KEY"
        case .codex: "CODEX_API_KEY"
        case .gemini: "GEMINI_API_KEY"
        case .openai: "OPENAI_API_KEY"
        }
    }

    var defaultModel: String? {
        switch self {
        case .anthropic: "claude-sonnet-5"
        case .openai: "gpt-5"
        case .claude, .codex, .gemini: nil
        }
    }

    var credentialRequired: Bool { kind == .api }

    var symbolName: String {
        switch self {
        case .claude: "brain.head.profile"
        case .codex: "chevron.left.forwardslash.chevron.right"
        case .gemini: "diamond"
        case .anthropic: "brain"
        case .openai: "cpu"
        }
    }

    /// Shell command that installs the CLI; nil for the API engines.
    var installCommand: String? {
        switch self {
        case .claude: "npm install -g @anthropic-ai/claude-code"
        case .codex: "npm install -g @openai/codex"
        case .gemini: "npm install -g @google/gemini-cli"
        case .anthropic, .openai: nil
        }
    }
}

/// One Ask provider: a headless agent CLI (`claude`, `codex`, …) or a hosted
/// API (`anthropic`, `openai`, …). The daemon owns the list (`ask_providers`);
/// `builtIn` is the fallback for daemons that predate that request.
///
/// Identity is the stable `id` (the daemon's provider id and the value the
/// old enum used as `rawValue`); equality and hashing use only the id so a
/// provider decoded from the daemon compares equal to its static twin.
struct MuxaAskProvider: Decodable, Identifiable, Sendable {
    enum Kind: String, Decodable, Sendable {
        case cli
        case api
    }

    let id: String
    let title: String
    let kind: Kind
    /// Executable name for `cli` providers; nil for API providers.
    let cliExecutable: String?
    /// Environment variable the provider reads its API key from.
    let credentialEnv: String?
    /// True when the provider cannot run without an API key.
    let credentialRequired: Bool
    /// True when muxad can already see a key for this instance — its
    /// `api_key_env` is set in the daemon's environment. The app then needs
    /// no Keychain entry to make the provider usable.
    let credentialPresent: Bool
    /// The effective model the daemon will use (configured or default).
    let model: String?
    /// Mirrors the daemon's current Ask agent.
    let selected: Bool
    /// Engine id driving this instance (`claude`, `anthropic`, …). Several
    /// instances may share one engine; it defaults from the instance id so a
    /// daemon that predates instances still names an engine.
    let engine: String
    /// True when the id is one of the five engines muxa ships. It stays
    /// true after the user configures that id, so it answers "is this a
    /// shipped engine", not "is this in config" — see `isConfigured`.
    let builtin: Bool
    /// The daemon's own answer to "does this row have an
    /// `[ask.providers.<id>]` table". Absent on every daemon written so
    /// far, which is why `isConfigured` can work it out without this.
    let configured: Bool?
    /// True when the daemon's row actually carried `engine`, i.e. this muxad
    /// understands `ask_provider_add` / `ask_provider_remove`. Rows this app
    /// synthesises leave it false so the pane hides those actions.
    let declaresEngine: Bool

    enum CodingKeys: String, CodingKey {
        case id, title, kind, model, selected, engine, builtin, configured
        case cliExecutable = "executable"
        case credentialEnv = "credential_env"
        case credentialRequired = "credential_required"
        case credentialPresent = "credential_present"
    }

    init(
        id: String,
        title: String,
        kind: Kind,
        cliExecutable: String? = nil,
        credentialEnv: String? = nil,
        credentialRequired: Bool = false,
        credentialPresent: Bool = false,
        model: String? = nil,
        selected: Bool = false,
        engine: String? = nil,
        builtin: Bool = true,
        configured: Bool? = nil,
        declaresEngine: Bool = false
    ) {
        self.id = id
        self.title = title
        self.kind = kind
        self.cliExecutable = cliExecutable
        self.credentialEnv = credentialEnv
        self.credentialRequired = credentialRequired
        self.credentialPresent = credentialPresent
        self.model = model
        self.selected = selected
        self.engine = engine ?? Self.defaultEngine(for: id)
        self.builtin = builtin
        self.configured = configured
        self.declaresEngine = declaresEngine
    }

    /// A copy of this row with `selected` replaced; used by the built-in
    /// fallback list, which mirrors the daemon's agent without a daemon.
    func selecting(_ isSelected: Bool) -> MuxaAskProvider {
        MuxaAskProvider(
            id: id,
            title: title,
            kind: kind,
            cliExecutable: cliExecutable,
            credentialEnv: credentialEnv,
            credentialRequired: credentialRequired,
            credentialPresent: credentialPresent,
            model: model,
            selected: isSelected,
            engine: engine,
            builtin: builtin,
            configured: configured,
            declaresEngine: declaresEngine
        )
    }

    /// Lenient wire decoding: only `id` is required so a newer daemon can
    /// add or drop fields without breaking the app.
    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let id = try container.decode(String.self, forKey: .id)
        let executable = try container.decodeIfPresent(String.self, forKey: .cliExecutable)
        let kind = try container.decodeIfPresent(String.self, forKey: .kind)
            .flatMap(Kind.init(rawValue:)) ?? (executable == nil ? .api : .cli)
        let title = try container.decodeIfPresent(String.self, forKey: .title)
        let credentialEnv = try container.decodeIfPresent(String.self, forKey: .credentialEnv)
        let credentialRequired = try container.decodeIfPresent(Bool.self, forKey: .credentialRequired)
        let credentialPresent = try container.decodeIfPresent(Bool.self, forKey: .credentialPresent)
        let model = try container.decodeIfPresent(String.self, forKey: .model)
        let selected = try container.decodeIfPresent(Bool.self, forKey: .selected)
        // A daemon that predates provider instances sends neither key: every
        // row is then a built-in whose engine is its own id.
        let engine = try container.decodeIfPresent(String.self, forKey: .engine)
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .flatMap { $0.isEmpty ? nil : $0 }
        let builtin = try container.decodeIfPresent(Bool.self, forKey: .builtin)
        let configured = try container.decodeIfPresent(Bool.self, forKey: .configured)
        self.init(
            id: id,
            title: title ?? Self.fallbackTitle(for: id),
            kind: kind,
            cliExecutable: executable,
            credentialEnv: credentialEnv,
            credentialRequired: credentialRequired ?? (kind == .api),
            credentialPresent: credentialPresent ?? false,
            model: model,
            selected: selected ?? false,
            engine: engine,
            builtin: builtin ?? true,
            configured: configured,
            declaresEngine: engine != nil
        )
    }

    /// The old enum's `init?(rawValue:)`: a known provider for the built-in
    /// ids, otherwise a generic CLI provider so a Keychain key can still be
    /// looked up for an id the daemon introduced after this build.
    init?(rawValue: String) {
        let id = rawValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !id.isEmpty else { return nil }
        if let known = Self.known.first(where: { $0.id == id }) {
            self = known
        } else {
            self.init(
                id: id,
                title: Self.fallbackTitle(for: id),
                kind: .cli,
                cliExecutable: id,
                credentialEnv: Self.defaultEnvironmentKey(for: id)
            )
        }
    }

    // MARK: Compatibility accessors (used by AppModel and the credential store)

    var rawValue: String { id }

    /// Executable name handed to `openProviderCLI`. API providers have none;
    /// returning the id makes resolution fail cleanly instead of crashing.
    var executable: String { cliExecutable ?? id }

    var environmentKey: String { credentialEnv ?? Self.defaultEnvironmentKey(for: id) }

    /// Keychain account under `MuxaProviderCredentialStore.service`; must stay
    /// `claude-api-key` / `codex-api-key` for keys saved by earlier builds.
    var keychainAccount: String { "\(id)-api-key" }

    var isCLI: Bool { kind == .cli }

    /// The engine this instance runs on, when this build knows it.
    var engineDescriptor: AskProviderEngine? { AskProviderEngine(rawValue: engine) }

    /// Whether an `[ask.providers.<id>]` table stands behind this row, which
    /// is what decides between "Add" and "Remove".
    ///
    /// `builtin` cannot answer it: muxad keeps a shipped id flagged built-in
    /// after the user configures it, because removing that entry leaves the
    /// engine standing rather than taking the row away. So a daemon's own
    /// `configured` wins when it sends one; failing that, an id muxa does
    /// not ship can only come from config, and a shipped id that overrides
    /// anything its engine supplies must have a table to say so. The one
    /// case this misreads is an empty `[ask.providers.<id>]` table on a
    /// built-in, which looks exactly like the shipped default on the wire.
    var isConfigured: Bool {
        if let configured { return configured }
        // A daemon that does not speak instances reports no config entries,
        // and the pane hides Add and Remove for it anyway.
        guard declaresEngine else { return false }
        guard builtin else { return true }
        guard let engineDescriptor else { return false }
        return title != engineDescriptor.title
            || model != engineDescriptor.defaultModel
            || cliExecutable != engineDescriptor.defaultExecutable
    }

    /// SF Symbol for the provider in conversation turns and cards. Instances
    /// of one engine share its symbol, so two Anthropic keys look alike.
    var symbolName: String {
        if let engineDescriptor { return engineDescriptor.symbolName }
        return kind == .api ? "network" : "terminal"
    }

    /// Shell command that installs a known CLI; nil when unknown.
    var installCommand: String? { engineDescriptor?.installCommand }

    // MARK: Built-in providers

    static let claude = MuxaAskProvider(
        id: "claude",
        title: "Claude Code",
        kind: .cli,
        cliExecutable: "claude",
        credentialEnv: "ANTHROPIC_API_KEY"
    )

    static let codex = MuxaAskProvider(
        id: "codex",
        title: "Codex",
        kind: .cli,
        cliExecutable: "codex",
        credentialEnv: "CODEX_API_KEY"
    )

    static let anthropic = MuxaAskProvider(
        id: "anthropic",
        title: "Anthropic API",
        kind: .api,
        credentialEnv: "ANTHROPIC_API_KEY",
        credentialRequired: true,
        model: "claude-sonnet-5"
    )

    static let openai = MuxaAskProvider(
        id: "openai",
        title: "OpenAI API",
        kind: .api,
        credentialEnv: "OPENAI_API_KEY",
        credentialRequired: true,
        model: "gpt-5"
    )

    /// Providers every muxad has supported: the list shown when the daemon
    /// does not advertise `ask_providers_v1`.
    static let builtIn: [MuxaAskProvider] = [claude, codex]

    /// Providers this build knows how to describe without the daemon.
    static let known: [MuxaAskProvider] = [claude, codex, anthropic, openai]

    /// Display title for an id, from the known table or capitalized.
    static func fallbackTitle(for id: String) -> String {
        known.first { $0.id == id }?.title ?? id.capitalized
    }

    /// Engine for an instance whose row named none: the built-in engine
    /// with that id, otherwise the id itself, which the app treats as an
    /// engine it cannot describe.
    static func defaultEngine(for id: String) -> String {
        AskProviderEngine(rawValue: id)?.rawValue ?? id
    }

    static func defaultEnvironmentKey(for id: String) -> String {
        let stem = id.uppercased().map { $0.isLetter || $0.isNumber ? String($0) : "_" }.joined()
        return "\(stem)_API_KEY"
    }
}

extension MuxaAskProvider: Hashable {
    static func == (lhs: MuxaAskProvider, rhs: MuxaAskProvider) -> Bool {
        lhs.id == rhs.id
    }

    func hash(into hasher: inout Hasher) {
        hasher.combine(id)
    }
}

enum MuxaProviderCredentialStore {
    /// Keychain service shared by every provider key; unchanged since the
    /// first release so saved keys keep resolving.
    static let service = "dev.muxa.mac.ask-provider"

    static func hasKey(for provider: MuxaAskProvider) -> Bool {
        key(for: provider) != nil
    }

    static func key(for provider: MuxaAskProvider) -> String? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: provider.keychainAccount,
            kSecMatchLimit as String: kSecMatchLimitOne,
            kSecReturnData as String: true,
        ]
        var result: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &result) == errSecSuccess,
              let data = result as? Data,
              let value = String(data: data, encoding: .utf8),
              !value.isEmpty else { return nil }
        return value
    }

    static func save(_ value: String, for provider: MuxaAskProvider) throws {
        let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            throw MuxaIPCError.server(String(localized: "API key cannot be empty"))
        }
        let selector: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: provider.keychainAccount,
        ]
        let attributes: [String: Any] = [kSecValueData as String: Data(trimmed.utf8)]
        let update = SecItemUpdate(selector as CFDictionary, attributes as CFDictionary)
        if update == errSecItemNotFound {
            var item = selector
            item[kSecValueData as String] = Data(trimmed.utf8)
            let status = SecItemAdd(item as CFDictionary, nil)
            guard status == errSecSuccess else { throw keychainError(status) }
        } else if update != errSecSuccess {
            throw keychainError(update)
        }
    }

    static func remove(for provider: MuxaAskProvider) throws {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: provider.keychainAccount,
        ]
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw keychainError(status)
        }
    }

    static func augmentPath(_ base: [String: String]) -> [String: String] {
        var environment = base
        environment["PATH"] = MuxaExecutableResolver.augmentedPath(base["PATH"])
        return environment
    }

    static func environment(
        _ base: [String: String],
        for provider: MuxaAskProvider
    ) -> [String: String] {
        var environment = augmentPath(base)
        if environment[provider.environmentKey]?.isEmpty != false,
           let key = key(for: provider) {
            environment[provider.environmentKey] = key
        }
        return environment
    }

    private static func keychainError(_ status: OSStatus) -> Error {
        let detail = SecCopyErrorMessageString(status, nil) as String? ?? "status \(status)"
        return MuxaIPCError.server(String(localized: "Keychain: \(detail)"))
    }
}

enum MuxaExecutableResolver {
    static func augmentedPath(_ existing: String?) -> String {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        let preferred = [
            "\(home)/.local/bin",
            "\(home)/.cargo/bin",
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
        ]
        var seen = Set<String>()
        return (preferred + (existing ?? "").split(separator: ":").map(String.init))
            .filter { !$0.isEmpty && seen.insert($0).inserted }
            .joined(separator: ":")
    }

    /// An absolute path is taken as given — an instance may pin a second
    /// install that is on nobody's PATH — otherwise the name is looked up
    /// the way a login shell would.
    static func executablePath(_ name: String) -> String? {
        if name.hasPrefix("/") {
            return FileManager.default.isExecutableFile(atPath: name) ? name : nil
        }
        return augmentedPath(ProcessInfo.processInfo.environment["PATH"])
            .split(separator: ":")
            .map(String.init)
            .map { URL(fileURLWithPath: $0).appendingPathComponent(name).path }
            .first { FileManager.default.isExecutableFile(atPath: $0) }
    }
}
