import Foundation
import Security

enum MuxaAskProvider: String, CaseIterable, Identifiable, Sendable {
    case claude
    case codex

    var id: Self { self }
    var title: String { self == .claude ? "Claude Code" : "Codex" }
    var executable: String { rawValue }
    var environmentKey: String { self == .claude ? "ANTHROPIC_API_KEY" : "CODEX_API_KEY" }
    var keychainAccount: String { "\(rawValue)-api-key" }
}

enum MuxaProviderCredentialStore {
    private static let service = "dev.muxa.mac.ask-provider"

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
            throw MuxaIPCError.server("API key cannot be empty")
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
        return MuxaIPCError.server("Keychain: \(detail)")
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
