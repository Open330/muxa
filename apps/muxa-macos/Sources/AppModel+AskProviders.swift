import AppKit
import SwiftUI

/// Whether a provider can answer an Ask right now, and why not.
enum AskProviderUsability: Equatable, Sendable {
    /// CLI found on PATH, or an API key is saved.
    case usable
    /// CLI detection has not finished. Treated as usable so the operator is
    /// not blocked while the probe runs; the daemon reports a missing CLI.
    case probing
    case notInstalled
    case missingKey

    var isUsable: Bool {
        self == .usable || self == .probing
    }

    /// Short reason for a disabled picker entry; nil when usable.
    var reason: String? {
        switch self {
        case .usable, .probing: nil
        case .notInstalled: String(localized: "not installed")
        case .missingKey: String(localized: "no API key")
        }
    }
}

/// Result of probing a CLI provider's executable on PATH.
enum AskProviderDetection: Equatable, Sendable {
    case probing
    case notInstalled
    case installed(InstalledTool)

    var tool: InstalledTool? {
        if case .installed(let tool) = self { return tool }
        return nil
    }
}

/// Provider list, CLI detection and Keychain presence for the Ask surfaces.
///
/// `@Published` state cannot live in an `AppModel` extension, so the views
/// share this object. Detection results are cached for the life of the
/// process; "Re-check" forces a new probe.
@MainActor
final class AskProviderStore: ObservableObject {
    static let shared = AskProviderStore()

    @Published private(set) var providers: [MuxaAskProvider] = MuxaAskProvider.builtIn
    /// True once `ask_providers` succeeded; false while on the built-in list.
    @Published private(set) var providersFromDaemon = false
    @Published private(set) var isLoading = false
    @Published private(set) var isConfiguring = false
    @Published private(set) var loadError: String?
    @Published private(set) var configureStatus: String?
    @Published private(set) var configureError: String?
    /// Keychain presence per provider id.
    @Published private(set) var keyPresence: [String: Bool] = [:]
    /// Executable name → detection; executables missing from the map have
    /// not been probed yet.
    @Published private(set) var detections: [String: AskProviderDetection] = [:]
    private var detectionTask: Task<Void, Never>?

    init() {}

    var isDetecting: Bool {
        detections.values.contains(.probing)
    }

    // MARK: Loading

    /// Loads the daemon's list when it advertises `ask_providers_v1`, else
    /// the built-in CLI providers; then refreshes Keychain state and probes
    /// any CLI not yet detected.
    func reload(model: AppModel) async {
        isLoading = true
        defer { isLoading = false }
        loadError = nil
        if await model.client.supports(MuxaIPCClient.askProvidersCapability) {
            do {
                let listed = try await model.client.listAskProviders()
                providers = listed.isEmpty ? Self.fallbackProviders(selected: model.askAgent) : listed
                providersFromDaemon = !listed.isEmpty
            } catch {
                loadError = error.localizedDescription
                if !providersFromDaemon {
                    providers = Self.fallbackProviders(selected: model.askAgent)
                }
            }
        } else {
            providersFromDaemon = false
            providers = Self.fallbackProviders(selected: model.askAgent)
        }
        refreshKeyPresence()
        await detectInstalledTools()
    }

    /// Probes every CLI provider's executable once per launch; `force`
    /// re-probes all of them (after the user installs a CLI).
    func detectInstalledTools(force: Bool = false) async {
        if let running = detectionTask {
            await running.value
        }
        let pending = cliExecutables.filter { force || detections[$0] == nil }
        guard !pending.isEmpty else { return }
        for name in pending {
            detections[name] = .probing
        }
        let task = Task { @MainActor [weak self] in
            let found = await InstalledTools.detect(pending)
            guard let self else { return }
            for name in pending {
                detections[name] = found.first { $0.name == name }.map(AskProviderDetection.installed) ?? .notInstalled
            }
        }
        detectionTask = task
        await task.value
        if detectionTask == task {
            detectionTask = nil
        }
    }

    func refreshKeyPresence() {
        keyPresence = Dictionary(
            providers.map { ($0.id, MuxaProviderCredentialStore.hasKey(for: $0)) },
            uniquingKeysWith: { first, _ in first }
        )
    }

    // MARK: Queries

    func provider(id: String) -> MuxaAskProvider? {
        providers.first { $0.id == id }
    }

    /// Display title for a provider id, even one this build never listed.
    func title(for id: String) -> String {
        provider(id: id)?.title ?? MuxaAskProvider.fallbackTitle(for: id)
    }

    func symbolName(for id: String) -> String {
        provider(id: id)?.symbolName ?? MuxaAskProvider(rawValue: id)?.symbolName ?? "sparkles"
    }

    func hasKey(_ provider: MuxaAskProvider) -> Bool {
        keyPresence[provider.id] ?? false
    }

    func detection(for provider: MuxaAskProvider) -> AskProviderDetection {
        guard provider.kind == .cli, let executable = provider.cliExecutable else { return .notInstalled }
        return detections[executable] ?? .probing
    }

    func usability(_ provider: MuxaAskProvider) -> AskProviderUsability {
        Self.usability(kind: provider.kind, detection: detection(for: provider), hasKey: hasKey(provider))
    }

    func isUsable(_ provider: MuxaAskProvider) -> Bool {
        usability(provider).isUsable
    }

    private var cliExecutables: [String] {
        var seen = Set<String>()
        return providers.compactMap { provider in
            guard provider.kind == .cli, let executable = provider.cliExecutable,
                  seen.insert(executable).inserted else { return nil }
            return executable
        }
    }

    // MARK: Mutations

    /// Saves through `AppModel` (which owns the status line) and refreshes
    /// the cached presence for that provider.
    func saveKey(_ key: String, for provider: MuxaAskProvider, model: AppModel) -> Bool {
        let saved = model.saveProviderKey(key, provider: provider)
        keyPresence[provider.id] = MuxaProviderCredentialStore.hasKey(for: provider)
        return saved
    }

    func removeKey(for provider: MuxaAskProvider, model: AppModel) {
        model.removeProviderKey(provider)
        keyPresence[provider.id] = MuxaProviderCredentialStore.hasKey(for: provider)
    }

    /// Persists the model for an API provider through `ask_provider_configure`.
    /// A blank model clears the override so the daemon's default applies.
    func configure(provider: MuxaAskProvider, model modelName: String?, using appModel: AppModel) async -> Bool {
        guard !isConfiguring else { return false }
        isConfiguring = true
        defer { isConfiguring = false }
        configureError = nil
        configureStatus = nil
        let update = MuxaAskProviderFieldUpdate(modelName)
        do {
            let updated = try await appModel.client.configureAskProvider(provider.id, model: update)
            if !updated.isEmpty {
                providers = updated
                providersFromDaemon = true
                refreshKeyPresence()
            }
            switch update {
            case .set(let value):
                configureStatus = String(localized: "\(provider.title) will answer with \(value).")
            case .clear, .keep:
                configureStatus = String(localized: "\(provider.title) will use the daemon's default model.")
            }
            return true
        } catch {
            configureError = error.localizedDescription
            return false
        }
    }

    // MARK: Pure rules (unit-tested)

    /// The "usable" rule table: API providers need a saved key; CLI
    /// providers need the executable on PATH.
    nonisolated static func usability(
        kind: MuxaAskProvider.Kind,
        detection: AskProviderDetection,
        hasKey: Bool
    ) -> AskProviderUsability {
        switch kind {
        case .api:
            return hasKey ? .usable : .missingKey
        case .cli:
            switch detection {
            case .installed: return .usable
            case .probing: return .probing
            case .notInstalled: return .notInstalled
            }
        }
    }

    /// The built-in list with `selected` mirroring the daemon's agent.
    nonisolated static func fallbackProviders(selected: String) -> [MuxaAskProvider] {
        MuxaAskProvider.builtIn.map { provider in
            MuxaAskProvider(
                id: provider.id,
                title: provider.title,
                kind: provider.kind,
                cliExecutable: provider.cliExecutable,
                credentialEnv: provider.credentialEnv,
                credentialRequired: provider.credentialRequired,
                model: provider.model,
                selected: provider.id == selected
            )
        }
    }
}

/// Opens the Settings window on a chosen tab, the way `MuxaApp`'s menu-bar
/// entry does: the SwiftUI action on macOS 14, the AppKit selector before.
enum MuxaSettingsOpener {
    /// Records the tab the Settings window should show next. `MuxaSettingsView`
    /// binds its `TabView` to this key, so an open window switches too.
    static func select(_ tab: MuxaSettingsTab, in defaults: UserDefaults = .standard) {
        defaults.set(tab.rawValue, forKey: MuxaPreferences.settingsTabKey)
    }

    /// macOS 13 path: the responder-chain selector the Settings scene answers.
    @MainActor
    static func openLegacySettingsWindow() {
        let opened = NSApp.sendAction(Selector(("showSettingsWindow:")), to: nil, from: nil)
        if !opened {
            NSApp.sendAction(Selector(("showPreferencesWindow:")), to: nil, from: nil)
        }
        NSApp.activate(ignoringOtherApps: true)
    }
}

