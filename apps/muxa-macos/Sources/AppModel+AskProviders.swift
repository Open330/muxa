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
    /// Installed, but the system says it cannot answer right now — Apple
    /// Intelligence turned off, the model still downloading. Carries the
    /// explanation to show beside the provider.
    case unavailable(String)

    var isUsable: Bool {
        self == .usable || self == .probing
    }

    /// Short reason for a disabled picker entry; nil when usable.
    var reason: String? {
        switch self {
        case .usable, .probing: nil
        case .notInstalled: String(localized: "not installed")
        case .missingKey: String(localized: "no API key")
        case .unavailable: String(localized: "unavailable")
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

/// A provider instance being composed in the Add Provider sheet.
///
/// The engine is closed (muxad drives it); everything else is the user's,
/// including the id, which is the config key, the Ask agent name and the
/// Keychain account. The rules below are pure so they can be unit-tested.
struct AskProviderDraft: Equatable, Sendable {
    var engine: AskProviderEngine = .anthropic
    var id: String = ""
    var title: String = ""
    var model: String = ""
    var executable: String = ""
    var apiKey: String = ""

    var trimmedID: String { id.trimmingCharacters(in: .whitespacesAndNewlines) }

    /// A TOML bare key: ASCII letters, digits, `-` and `_`, at least one.
    static func isValidIdentifier(_ id: String) -> Bool {
        !id.isEmpty && id.allSatisfy { character in
            character.isASCII
                && (character.isLetter || character.isNumber || character == "_" || character == "-")
        }
    }

    /// `base`, or `base-2`, `base-3`, … until it is free.
    static func uniqueIdentifier(base: String, taken: Set<String>) -> String {
        guard taken.contains(base) else { return base }
        var suffix = 2
        while taken.contains("\(base)-\(suffix)") {
            suffix += 1
        }
        return "\(base)-\(suffix)"
    }

    /// Prefill for a freshly picked engine: its id, made unique against the
    /// instances already in config.
    static func suggestedIdentifier(for engine: AskProviderEngine, taken: Set<String>) -> String {
        uniqueIdentifier(base: engine.rawValue, taken: taken)
    }

    /// Why the sheet cannot save yet, or nil when the draft is ready. The
    /// daemon enforces the same rules; this keeps the button honest.
    func validationMessage(taken: Set<String>) -> String? {
        let id = trimmedID
        if id.isEmpty {
            return String(localized: "Give this provider an id.")
        }
        if !Self.isValidIdentifier(id) {
            return String(localized: "Ids may contain letters, digits, hyphens and underscores only.")
        }
        if taken.contains(id) {
            return String(localized: "A provider with this id is already configured.")
        }
        return nil
    }

    func isReady(taken: Set<String>) -> Bool {
        validationMessage(taken: taken) == nil
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
    /// True when the daemon's rows carry `engine`, i.e. it understands
    /// `ask_provider_add` / `ask_provider_remove`. Older daemons keep the
    /// read-only pane.
    @Published private(set) var supportsInstances = false
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
    /// What `muxa-afm --probe` last said about Apple's system models; nil
    /// until it has answered, and again if it could not be run.
    @Published private(set) var appleProbe: AskAppleProbe?
    private var detectionTask: Task<Void, Never>?
    private var appleProbeHelper: String?
    /// Directory of the muxad serving the socket, once looked up; nil when
    /// that could not be told, and the app's own helper is then trusted.
    @Published private(set) var daemonDirectory: String?

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
                adopt(listed, selected: model.askAgent)
            } catch {
                loadError = error.localizedDescription
                if !providersFromDaemon {
                    adopt([], selected: model.askAgent)
                }
            }
        } else {
            adopt([], selected: model.askAgent)
        }
        refreshKeyPresence()
        if let owner = try? await DaemonSocketOwner.find(socketPath: model.client.socketPath) {
            daemonDirectory = (owner.executablePath as NSString).deletingLastPathComponent
        }
        await detectInstalledTools()
        await probeAppleModels()
    }

    /// Asks the `muxa-afm` helper whether Apple's models can answer on this
    /// Mac. Being installed is not the question for this engine — the helper
    /// ships with the app — so the pane shows this instead. Runs once per
    /// helper; `force` asks again, after Apple Intelligence was turned on.
    func probeAppleModels(force: Bool = false) async {
        guard let helper = providers.lazy
            .filter(\.isApple)
            .compactMap({ self.detection(for: $0).tool?.path })
            .first
        else {
            appleProbe = nil
            appleProbeHelper = nil
            return
        }
        guard force || appleProbeHelper != helper else { return }
        appleProbeHelper = helper
        let output = await InstalledTools.runCapturing(helper, ["--probe"], timeout: 5)
        guard appleProbeHelper == helper else { return }
        appleProbe = output.flatMap(AskAppleProbe.decode)
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

    /// Takes a fresh `ask_providers` list, falling back to the built-ins
    /// when the daemon sent none, and re-derives everything keyed off it.
    private func adopt(_ listed: [MuxaAskProvider], selected: String) {
        providers = listed.isEmpty ? Self.fallbackProviders(selected: selected) : listed
        providersFromDaemon = !listed.isEmpty
        supportsInstances = listed.contains(where: \.declaresEngine)
        refreshKeyPresence()
    }

    /// Drops the status and error lines, so a sheet opened after a failed
    /// attempt does not lead with the previous complaint.
    func clearStatus() {
        configureStatus = nil
        configureError = nil
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

    /// Instances the user has written into config: the ones that can be
    /// renamed, re-keyed and removed.
    var configuredProviders: [MuxaAskProvider] {
        providers.filter(\.isConfigured)
    }

    /// Built-in engines no config entry covers yet; "Add" writes one of them
    /// into config so it can carry its own title, model and key.
    var detectedProviders: [MuxaAskProvider] {
        providers.filter { !$0.isConfigured }
    }

    /// Ids already in config, which the Add sheet must not reuse.
    var configuredIdentifiers: Set<String> {
        Set(configuredProviders.map(\.id))
    }

    /// Display title for a provider id, even one this build never listed.
    func title(for id: String) -> String {
        provider(id: id)?.title ?? MuxaAskProvider.fallbackTitle(for: id)
    }

    /// Display titles for the providers these entries name, resolved once so
    /// an export formatter can run off the main actor's state.
    func titles(for entries: [MuxaAskEntry]) -> [String: String] {
        Dictionary(
            entries.map { ($0.agent, title(for: $0.agent)) },
            uniquingKeysWith: { first, _ in first }
        )
    }

    func symbolName(for id: String) -> String {
        provider(id: id)?.symbolName ?? MuxaAskProvider(rawValue: id)?.symbolName ?? "sparkles"
    }

    func hasKey(_ provider: MuxaAskProvider) -> Bool {
        keyPresence[provider.id] ?? false
    }

    func detection(for provider: MuxaAskProvider) -> AskProviderDetection {
        guard provider.kind == .cli, let executable = provider.cliExecutable else { return .notInstalled }
        let isExecutable: (String) -> Bool = { FileManager.default.isExecutableFile(atPath: $0) }
        if let direct = Self.pathDetection(for: executable, isExecutable: isExecutable) {
            return direct
        }
        if provider.isApple,
           let bundled = Self.bundledHelperDetection(
               for: executable,
               helpersDirectory: Self.bundledHelpersDirectory,
               daemonDirectory: daemonDirectory,
               isExecutable: isExecutable
           ) {
            return bundled
        }
        return detections[executable] ?? .probing
    }

    func usability(_ provider: MuxaAskProvider) -> AskProviderUsability {
        let detection = detection(for: provider)
        if provider.isApple, case .installed = detection {
            return Self.appleUsability(model: provider.model, probe: appleProbe)
        }
        return Self.usability(
            kind: provider.kind,
            detection: detection,
            hasKey: hasKey(provider),
            credentialPresent: provider.credentialPresent
        )
    }

    /// `Contents/Helpers` of the running app, where `muxa-afm` ships.
    private static var bundledHelpersDirectory: String {
        Bundle.main.bundleURL
            .appendingPathComponent("Contents", isDirectory: true)
            .appendingPathComponent("Helpers", isDirectory: true)
            .path
    }

    func isUsable(_ provider: MuxaAskProvider) -> Bool {
        usability(provider).isUsable
    }

    /// Bare command names to probe on PATH. An instance that pins an
    /// absolute `executable` is checked directly instead.
    private var cliExecutables: [String] {
        var seen = Set<String>()
        return providers.compactMap { provider in
            guard provider.kind == .cli, let executable = provider.cliExecutable,
                  !executable.hasPrefix("/"), seen.insert(executable).inserted else { return nil }
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

    /// Persists `[ask.providers.<id>]` keys through `ask_provider_configure`.
    /// `.keep` leaves a key untouched; a blank value clears the override so
    /// the daemon's default applies.
    func configure(
        provider: MuxaAskProvider,
        title: MuxaAskProviderFieldUpdate = .keep,
        model: MuxaAskProviderFieldUpdate = .keep,
        executable: MuxaAskProviderFieldUpdate = .keep,
        using appModel: AppModel
    ) async -> Bool {
        guard !isConfiguring else { return false }
        isConfiguring = true
        defer { isConfiguring = false }
        configureError = nil
        configureStatus = nil
        do {
            let updated = try await appModel.client.configureAskProvider(
                provider.id,
                title: title,
                model: model,
                executable: executable
            )
            adopt(updated, selected: appModel.askAgent)
            switch model {
            case .set(let value):
                configureStatus = String(localized: "\(provider.title) will answer with \(value).")
            case .clear:
                configureStatus = String(localized: "\(provider.title) will use the daemon's default model.")
            case .keep:
                configureStatus = String(localized: "Updated \(provider.title).")
            }
            return true
        } catch {
            configureError = error.localizedDescription
            return false
        }
    }

    /// Writes a new `[ask.providers.<id>]` entry and, when the sheet
    /// collected one, saves its API key under the new id so instances that
    /// share an engine keep separate keys.
    func addProvider(
        id providerID: String,
        engine: String,
        title: String? = nil,
        model: String? = nil,
        executable: String? = nil,
        apiKey: String? = nil,
        using appModel: AppModel
    ) async -> Bool {
        guard !isConfiguring else { return false }
        isConfiguring = true
        defer { isConfiguring = false }
        configureError = nil
        configureStatus = nil
        do {
            let updated = try await appModel.client.addAskProvider(
                id: providerID,
                engine: engine,
                title: title,
                model: model,
                executable: executable
            )
            adopt(updated, selected: appModel.askAgent)
            if let apiKey, !apiKey.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
               let saved = provider(id: providerID) ?? MuxaAskProvider(rawValue: providerID) {
                _ = saveKey(apiKey, for: saved, model: appModel)
            }
            configureStatus = String(localized: "Added \(provider(id: providerID)?.title ?? providerID).")
            await detectInstalledTools()
            await adoptDaemonSelection(using: appModel)
            return true
        } catch {
            configureError = error.localizedDescription
            return false
        }
    }

    /// Deletes the instance from config. `deletingKey` also drops its
    /// Keychain entry, which nothing else would ever read again.
    func removeProvider(
        _ provider: MuxaAskProvider,
        deletingKey: Bool,
        using appModel: AppModel
    ) async -> Bool {
        guard !isConfiguring else { return false }
        isConfiguring = true
        defer { isConfiguring = false }
        configureError = nil
        configureStatus = nil
        do {
            let updated = try await appModel.client.removeAskProvider(provider.id)
            if deletingKey {
                appModel.removeProviderKey(provider)
            }
            adopt(updated, selected: appModel.askAgent)
            configureStatus = String(localized: "Removed \(provider.title).")
            await adoptDaemonSelection(using: appModel)
            return true
        } catch {
            configureError = error.localizedDescription
            return false
        }
    }

    /// Removing the default provider makes the daemon pick another one;
    /// follow it so the pickers and the Ask bar agree with config.
    private func adoptDaemonSelection(using appModel: AppModel) async {
        guard let selected = providers.first(where: \.selected), selected.id != appModel.askAgent else { return }
        await appModel.selectAskAgent(selected.id)
    }

    // MARK: Pure rules (unit-tested)

    /// The "usable" rule table: API providers need a key the app can hand
    /// over, or one muxad already has in its environment
    /// (`credential_present`); CLI providers need the executable on PATH.
    nonisolated static func usability(
        kind: MuxaAskProvider.Kind,
        detection: AskProviderDetection,
        hasKey: Bool,
        credentialPresent: Bool = false
    ) -> AskProviderUsability {
        switch kind {
        case .api:
            return hasKey || credentialPresent ? .usable : .missingKey
        case .cli:
            switch detection {
            case .installed: return .usable
            case .probing: return .probing
            case .notInstalled: return .notInstalled
            }
        }
    }

    /// Detection for an instance that pins an absolute `executable`: the
    /// file decides, PATH is irrelevant. Returns nil for a bare command
    /// name, which is probed on PATH instead.
    nonisolated static func pathDetection(
        for executable: String,
        isExecutable: (String) -> Bool
    ) -> AskProviderDetection? {
        guard executable.hasPrefix("/") else { return nil }
        guard isExecutable(executable) else { return .notInstalled }
        return .installed(
            InstalledTool(
                name: (executable as NSString).lastPathComponent,
                path: executable,
                version: nil
            )
        )
    }

    /// Detection for the `apple` engine's default helper, which is not on
    /// anyone's PATH: it ships in this app's `Contents/Helpers`, the same
    /// place the bundled muxad looks. Nil for any other name, and when the
    /// helper is missing — a development build — so PATH still gets a say.
    ///
    /// The app's copy counts only where muxad will look for it: beside the
    /// daemon itself (the bundled muxad), or in a `Muxa.app` installed in
    /// `/Applications` or `~/Applications` (a Homebrew muxad). An app run
    /// from anywhere else next to a Homebrew daemon would otherwise say the
    /// provider is ready while every turn fails with "helper not found".
    nonisolated static func bundledHelperDetection(
        for executable: String,
        helpersDirectory: String,
        daemonDirectory: String? = nil,
        homeDirectory: String = NSHomeDirectory(),
        isExecutable: (String) -> Bool
    ) -> AskProviderDetection? {
        guard executable == AskProviderEngine.appleHelperName else { return nil }
        let path = (helpersDirectory as NSString).appendingPathComponent(executable)
        guard isExecutable(path) else { return nil }
        if let daemonDirectory {
            let searched = [
                daemonDirectory,
                "/Applications/Muxa.app/Contents/Helpers",
                (homeDirectory as NSString).appendingPathComponent("Applications/Muxa.app/Contents/Helpers"),
            ].map { ($0 as NSString).standardizingPath }
            guard searched.contains((helpersDirectory as NSString).standardizingPath) else { return nil }
        }
        return .installed(InstalledTool(name: executable, path: path, version: nil))
    }

    /// Whether an installed `apple` instance can answer: the probe's verdict
    /// on the model the instance names. No probe yet, or none that could be
    /// read, stays usable — muxad reports the real reason on the first Ask.
    nonisolated static func appleUsability(
        model: String?,
        probe: AskAppleProbe?
    ) -> AskProviderUsability {
        guard let probe else { return .usable }
        guard let choice = AskAppleModel(configured: model) else {
            let accepted = AskAppleModel.allCases.map(\.rawValue).joined(separator: ", ")
            return .unavailable(String(localized: "Unknown model. Use one of: \(accepted)."))
        }
        guard let status = probe.status(of: choice) else {
            return .unavailable(String(localized: "The \(choice.rawValue) model needs a newer macOS."))
        }
        if status.available { return .usable }
        return .unavailable(status.message ?? String(localized: "The \(choice.rawValue) model is unavailable."))
    }

    /// The built-in list with `selected` mirroring the daemon's agent.
    nonisolated static func fallbackProviders(selected: String) -> [MuxaAskProvider] {
        MuxaAskProvider.builtIn.map { $0.selecting($0.id == selected) }
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

