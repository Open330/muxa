import Foundation
import Security

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
    /// The effective model the daemon will use (configured or default).
    let model: String?
    /// Mirrors the daemon's current Ask agent.
    let selected: Bool

    enum CodingKeys: String, CodingKey {
        case id, title, kind, model, selected
        case cliExecutable = "executable"
        case credentialEnv = "credential_env"
        case credentialRequired = "credential_required"
    }

    init(
        id: String,
        title: String,
        kind: Kind,
        cliExecutable: String? = nil,
        credentialEnv: String? = nil,
        credentialRequired: Bool = false,
        model: String? = nil,
        selected: Bool = false
    ) {
        self.id = id
        self.title = title
        self.kind = kind
        self.cliExecutable = cliExecutable
        self.credentialEnv = credentialEnv
        self.credentialRequired = credentialRequired
        self.model = model
        self.selected = selected
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
        let model = try container.decodeIfPresent(String.self, forKey: .model)
        let selected = try container.decodeIfPresent(Bool.self, forKey: .selected)
        self.init(
            id: id,
            title: title ?? Self.fallbackTitle(for: id),
            kind: kind,
            cliExecutable: executable,
            credentialEnv: credentialEnv,
            credentialRequired: credentialRequired ?? (kind == .api),
            model: model,
            selected: selected ?? false
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

    /// SF Symbol for the provider in conversation turns and cards.
    var symbolName: String {
        switch id {
        case "claude": "brain.head.profile"
        case "codex": "chevron.left.forwardslash.chevron.right"
        case "anthropic": "brain"
        case "openai": "cpu"
        case "gemini": "diamond"
        default: kind == .api ? "network" : "terminal"
        }
    }

    /// Shell command that installs a known CLI; nil when unknown.
    var installCommand: String? {
        switch id {
        case "claude": "npm install -g @anthropic-ai/claude-code"
        case "codex": "npm install -g @openai/codex"
        case "gemini": "npm install -g @google/gemini-cli"
        default: nil
        }
    }

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

    static func executablePath(_ name: String) -> String? {
        augmentedPath(ProcessInfo.processInfo.environment["PATH"])
            .split(separator: ":")
            .map(String.init)
            .map { URL(fileURLWithPath: $0).appendingPathComponent(name).path }
            .first { FileManager.default.isExecutableFile(atPath: $0) }
    }
}
