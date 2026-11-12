import SwiftUI

/// Settings › Ask: the one place to set up Global Ask.
///
/// A provider is an *instance* of a closed engine, so the pane lists what the
/// user has configured, one collapsed row each, and offers tendd's built-ins
/// inside Add Provider. Daemons that predate provider instances fall back to
/// the flat, read-only list.
struct AskProvidersSettingsPane: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: AskProviderStore
    @State private var isAddingProvider = false
    @State private var editingProvider: TendAskProvider?
    @State private var removalTarget: TendAskProvider?

    private var selectedProviderIsListed: Bool {
        store.providers.contains { $0.id == model.askAgent }
    }

    private var isRemoving: Binding<Bool> {
        Binding(
            get: { removalTarget != nil },
            set: { presented in
                if !presented { removalTarget = nil }
            }
        )
    }

    var body: some View {
        Form {
            if model.askEnabled == false {
                disabledSection
            }

            defaultProviderSection

            if store.supportsInstances {
                configuredSection
            } else {
                legacySection
            }

            statusSection

            Section {
                HStack(spacing: 10) {
                    TendSettingsCaption("Re-check after installing a CLI. Reload tendd only when its PATH must change too.")
                    Spacer()
                    TendDaemonReloadButton(model: model, title: "Reload tendd PATH…")
                        .controlSize(.small)
                }
            }
        }
        .formStyle(.grouped)
        .task(id: model.isConnected) {
            await store.reload(model: model)
        }
        .sheet(isPresented: $isAddingProvider) {
            AskProviderAddSheet(model: model, store: store)
        }
        .sheet(item: $editingProvider) { provider in
            AskProviderEditSheet(providerID: provider.id, fallback: provider, store: store, model: model)
        }
        .alert("Remove this provider?", isPresented: isRemoving, presenting: removalTarget) { provider in
            Button("Cancel", role: .cancel) { removalTarget = nil }
            if store.hasKey(provider) {
                Button("Remove and Delete Key", role: .destructive) {
                    remove(provider, deletingKey: true)
                }
            }
            Button("Remove", role: .destructive) {
                remove(provider, deletingKey: false)
            }
        } message: { provider in
            if provider.id == model.askAgent {
                Text("\(provider.title) is the default provider, so tendd will fall back to another one. Past conversations stay in your history.")
            } else {
                Text("\(provider.title) is deleted from tend's config. Past conversations stay in your history.")
            }
        }
    }

    private func remove(_ provider: TendAskProvider, deletingKey: Bool) {
        removalTarget = nil
        Task { _ = await store.removeProvider(provider, deletingKey: deletingKey, using: model) }
    }

    private var disabledSection: some View {
        Section {
            HStack {
                Label(
                    model.askConfigurationPendingReload
                        ? "Global Ask is enabled in config. Reload tendd to apply it."
                        : "Global Ask is disabled in tend configuration.",
                    systemImage: "exclamationmark.circle"
                )
                .font(TendType.detail)
                .foregroundStyle(.orange)
                Spacer()
                if model.isEnablingAsk {
                    ProgressView().controlSize(.small)
                }
                Button(model.askConfigurationPendingReload ? "Reload tendd" : "Enable Global Ask") {
                    Task { await model.enableAsk() }
                }
                .buttonStyle(.borderedProminent)
                .disabled(model.isEnablingAsk)
            }
        }
    }

    private var defaultProviderSection: some View {
        Section {
            Picker("Default provider", selection: defaultProviderSelection) {
                ForEach(store.providers) { provider in
                    if let reason = store.usability(provider).reason {
                        Text(verbatim: "\(provider.title) (\(reason))")
                            .tag(provider.id)
                            .disabled(true)
                    } else {
                        Text(provider.title).tag(provider.id)
                    }
                }
                if !selectedProviderIsListed {
                    Text(store.title(for: model.askAgent)).tag(model.askAgent)
                }
            }
            .disabled(!model.isConnected)
            TendSettingsCaption(
                store.providersFromDaemon
                    ? "New Ask conversations start with this provider. Entries are disabled until their CLI is installed or a key is saved."
                    : "This tendd predates the provider list; only the built-in CLIs are shown. Update tend or choose Use Bundled tendd for API providers."
            )
        } header: {
            HStack {
                Text("Global Ask")
                Spacer()
                if store.isLoading {
                    ProgressView().controlSize(.small)
                }
                Button {
                    Task {
                        await store.detectInstalledTools(force: true)
                        await store.probeAppleModels(force: true)
                    }
                } label: {
                    Label("Re-check CLIs", systemImage: "arrow.clockwise")
                }
                .controlSize(.small)
                .disabled(store.isDetecting)
            }
        }
    }

    private var defaultProviderSelection: Binding<String> {
        Binding(
            get: { model.askAgent },
            set: { selected in
                guard selected != model.askAgent else { return }
                Task { await model.selectAskAgent(selected) }
            }
        )
    }

    // MARK: Sections

    /// Instances in tend's config: renameable, re-keyable, removable.
    private var configuredSection: some View {
        Section {
            if store.configuredProviders.isEmpty {
                TendSettingsCaption("Nothing configured yet. Add a provider to choose an engine, or one of the built-ins tendd ships with.")
            }
            ForEach(store.configuredProviders) { provider in
                AskProviderRow(
                    provider: provider,
                    store: store,
                    model: model,
                    onEdit: { editingProvider = provider },
                    onRemove: { removalTarget = provider }
                )
            }
        } header: {
            HStack {
                Text("Providers")
                Spacer()
                if store.isConfiguring {
                    ProgressView().controlSize(.small)
                }
                Button {
                    isAddingProvider = true
                } label: {
                    Label("Add Provider…", systemImage: "plus")
                }
                .controlSize(.small)
                .disabled(!model.isConnected || store.isConfiguring)
            }
        } footer: {
            TendSettingsCaption("Global Ask runs an installed agent CLI headlessly or calls a hosted API. Add one provider per account: keys stay in the macOS login Keychain, one per provider id.")
        }
    }

    /// A daemon that lists providers but does not understand instances: the
    /// old flat list, with a note explaining why Add and Remove are missing.
    private var legacySection: some View {
        Section("Providers") {
            if store.providersFromDaemon {
                Label(
                    "This tendd lists providers but cannot add or remove them. Update tend or choose Use Bundled tendd to compose your own provider list.",
                    systemImage: "info.circle"
                )
                .font(TendType.detail)
                .foregroundStyle(.secondary)
            }
            ForEach(store.providers) { provider in
                AskProviderRow(
                    provider: provider,
                    store: store,
                    model: model,
                    onEdit: { editingProvider = provider },
                    onRemove: nil
                )
            }
        }
    }

    @ViewBuilder
    private var statusSection: some View {
        let messages = model.askSettingsStatus != nil || store.configureStatus != nil
            || (model.askSettingsError ?? model.askError) != nil || store.configureError != nil
            || store.loadError != nil
        if messages {
            Section {
                AskProviderStatusLines(model: model, store: store)
                if let error = store.loadError {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .font(TendType.detail)
                        .foregroundStyle(.orange)
                        .textSelection(.enabled)
                }
            }
        }
    }
}

/// What the last provider change did, said once.
private struct AskProviderStatusLines: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: AskProviderStore

    var body: some View {
        if let status = model.askSettingsStatus {
            Label(status, systemImage: "checkmark.circle.fill")
                .font(TendType.detail)
                .foregroundStyle(.green)
        }
        if let status = store.configureStatus {
            Label(status, systemImage: "checkmark.circle.fill")
                .font(TendType.detail)
                .foregroundStyle(.green)
        }
        if let error = model.askSettingsError ?? model.askError {
            Label(error, systemImage: "exclamationmark.triangle.fill")
                .font(TendType.detail)
                .foregroundStyle(.red)
                .textSelection(.enabled)
        }
        if let error = store.configureError {
            Label(error, systemImage: "exclamationmark.triangle.fill")
                .font(TendType.detail)
                .foregroundStyle(.red)
                .textSelection(.enabled)
        }
    }
}

/// CLI/API pill, shared by the provider row and the detected row.
struct AskProviderKindBadge: View {
    let kind: TendAskProvider.Kind

    var body: some View {
        Group {
            if kind == .cli {
                Text("CLI")
            } else {
                Text("API")
            }
        }
        .font(TendType.meta.weight(.semibold))
        .padding(.horizontal, 6)
        .padding(.vertical, 2)
        .background(Color.primary.opacity(0.08), in: Capsule())
    }
}

/// Ready, checking, missing a key…: one badge for where a provider stands.
struct AskProviderUsabilityBadge: View {
    let usability: AskProviderUsability

    var body: some View {
        switch usability {
        case .usable:
            Label("Ready", systemImage: "checkmark.circle.fill")
                .font(TendType.meta.weight(.semibold))
                .foregroundStyle(.green)
        case .probing:
            Label("Checking", systemImage: "ellipsis.circle")
                .font(TendType.meta.weight(.semibold))
                .foregroundStyle(.secondary)
        case .notInstalled:
            Label("Not installed", systemImage: "xmark.circle")
                .font(TendType.meta.weight(.semibold))
                .foregroundStyle(.red)
        case .missingKey:
            Label("API key required", systemImage: "key")
                .font(TendType.meta.weight(.semibold))
                .foregroundStyle(.orange)
        case .unavailable:
            Label("Unavailable", systemImage: "exclamationmark.triangle")
                .font(TendType.meta.weight(.semibold))
                .foregroundStyle(.orange)
        }
    }
}

/// One provider, collapsed: what it is, whether it can answer, and Edit… /
/// Remove… for the rest.
struct AskProviderRow: View {
    let provider: TendAskProvider
    @ObservedObject var store: AskProviderStore
    @ObservedObject var model: AppModel
    let onEdit: () -> Void
    /// Nil for a built-in row, which cannot be removed.
    let onRemove: (() -> Void)?

    private var isConfigured: Bool { store.supportsInstances && provider.isConfigured }

    var body: some View {
        HStack(spacing: 8) {
            TendAgentLogo(asset: provider.logo, fallbackSymbol: provider.symbolName, size: 16)
                .foregroundStyle(.tint)
                .frame(width: 20)
            Text(provider.title)
                .lineLimit(1)
            AskProviderKindBadge(kind: provider.kind)
            AskProviderUsabilityBadge(usability: store.usability(provider))
            if model.askAgent == provider.id {
                Label("Default", systemImage: "checkmark")
                    .font(TendType.meta.weight(.semibold))
                    .foregroundStyle(.tint)
            }
            Spacer(minLength: 8)
            if provider.kind == .cli, provider.takesCredential {
                Button("Log In…") {
                    Task { await model.openProviderCLI(provider) }
                }
                .disabled(store.detection(for: provider).tool == nil || !model.isConnected)
                .help("Opens the CLI in a Tend shell so you can sign in")
            }
            Button("Edit…", action: onEdit)
                .accessibilityLabel(Text("Edit \(provider.title)"))
            if isConfigured, let onRemove {
                Button("Remove…", role: .destructive, action: onRemove)
                    .disabled(!model.isConnected || store.isConfiguring)
                    .help("Deletes this provider from tend's config")
                    .accessibilityLabel(Text("Remove \(provider.title)"))
            }
        }
        .controlSize(.small)
    }
}

/// A built-in engine the user has not configured: what it needs, and Add.
struct AskProviderDetectedRow: View {
    let provider: TendAskProvider
    @ObservedObject var store: AskProviderStore
    @ObservedObject var model: AppModel
    var onAdded: () -> Void = {}

    var body: some View {
        HStack(spacing: 10) {
            TendAgentLogo(asset: provider.logo, fallbackSymbol: provider.symbolName, size: 16)
                .foregroundStyle(.tint)
                .frame(width: 20)
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    Text(provider.title)
                    AskProviderKindBadge(kind: provider.kind)
                }
                requirement
            }
            Spacer(minLength: 8)
            Button {
                Task {
                    let added = await store.addProvider(
                        id: provider.id,
                        engine: provider.engine,
                        using: model
                    )
                    if added { onAdded() }
                }
            } label: {
                Label("Add", systemImage: "plus")
            }
            .controlSize(.small)
            .disabled(!model.isConnected || store.isConfiguring)
            .help("Writes this engine into tend's config so it can carry its own title, model and key")
            .accessibilityLabel(Text("Add \(provider.title)"))
        }
    }

    /// What the engine still needs before it can answer an Ask.
    @ViewBuilder
    private var requirement: some View {
        if provider.kind == .api {
            if provider.credentialPresent {
                TendSettingsCaption("Key already in tendd's environment")
            } else {
                TendSettingsCaption("API key required")
            }
        } else if provider.isApple {
            AskAppleAvailabilityLine(provider: provider, store: store)
        } else {
            switch store.detection(for: provider) {
            case .probing:
                TendSettingsCaption("Looking for \(provider.executable) on your PATH…")
            case .installed(let tool):
                Text("CLI found at \(tool.path)")
                    .font(TendType.detail.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            case .notInstalled:
                Text("\(provider.executable) is not on your PATH yet")
                    .font(TendType.detail)
                    .foregroundStyle(.orange)
            }
        }
    }
}

/// Where an `apple` provider stands: the helper, then the system's verdict on
/// the model it names. Installation is never the interesting answer for this
/// engine — the helper ships with the app — so this replaces the PATH line.
struct AskAppleAvailabilityLine: View {
    let provider: TendAskProvider
    @ObservedObject var store: AskProviderStore

    var body: some View {
        switch store.detection(for: provider) {
        case .probing:
            Text("Looking for the \(provider.executable) helper…")
                .font(TendType.detail)
                .foregroundStyle(.secondary)
        case .notInstalled:
            Text("The \(provider.executable) helper is missing. It ships inside Tend.app; a development build may need it built first.")
                .font(TendType.detail)
                .foregroundStyle(.orange)
                .fixedSize(horizontal: false, vertical: true)
        case .installed:
            switch store.usability(provider) {
            case .unavailable(let message):
                Text(verbatim: message)
                    .font(TendType.detail)
                    .foregroundStyle(.orange)
                    .fixedSize(horizontal: false, vertical: true)
                    .textSelection(.enabled)
            case .usable, .probing, .notInstalled, .missingKey:
                Text(readyLine)
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
            }
        }
    }

    /// "Runs on this Mac · on-device · 8,192-token context", with whatever
    /// the probe knows; just the first part before it has answered.
    private var readyLine: String {
        let model = AskAppleModel(configured: provider.model) ?? .onDevice
        var parts = [
            model == .onDevice
                ? String(localized: "Runs on this Mac, no key needed")
                : String(localized: "Runs on Apple's Private Cloud Compute, no key needed"),
        ]
        if let contextSize = store.appleProbe?.status(of: model)?.contextSize {
            parts.append(String(localized: "\(contextSize)-token context"))
        }
        return parts.joined(separator: " · ")
    }
}

/// One provider, opened: detection, credential state, the
/// `[ask.providers.<id>]` fields with their labels, and Remove.
struct AskProviderEditSheet: View {
    let providerID: String
    let fallback: TendAskProvider
    @ObservedObject var store: AskProviderStore
    @ObservedObject var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @State private var key = ""
    @State private var titleText = ""
    @State private var modelName = ""
    @State private var executablePath = ""

    /// The provider as the store now describes it, so a save shows through.
    private var provider: TendAskProvider {
        store.providers.first { $0.id == providerID } ?? fallback
    }

    private var hasKey: Bool { store.hasKey(provider) }
    private var detection: AskProviderDetection { store.detection(for: provider) }
    /// Only an instance that exists in config can be edited or removed.
    private var isConfigured: Bool { store.supportsInstances && provider.isConfigured }

    private var trimmedKey: String {
        key.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private var titleUpdate: TendAskProviderFieldUpdate {
        titleText.trimmingCharacters(in: .whitespacesAndNewlines) == provider.title
            ? .keep
            : TendAskProviderFieldUpdate(titleText)
    }

    private var modelUpdate: TendAskProviderFieldUpdate {
        modelName.trimmingCharacters(in: .whitespacesAndNewlines) == (provider.model ?? "")
            ? .keep
            : TendAskProviderFieldUpdate(modelName)
    }

    private var executableUpdate: TendAskProviderFieldUpdate {
        executablePath.trimmingCharacters(in: .whitespacesAndNewlines) == (provider.cliExecutable ?? "")
            ? .keep
            : TendAskProviderFieldUpdate(executablePath)
    }

    private var detailsChanged: Bool {
        titleUpdate != .keep || modelUpdate != .keep || executableUpdate != .keep
    }

    private var modelChanged: Bool { modelUpdate != .keep }

    var body: some View {
        VStack(spacing: 0) {
            Form {
                Section {
                    HStack(spacing: 8) {
                        AskProviderKindBadge(kind: provider.kind)
                        AskProviderUsabilityBadge(usability: store.usability(provider))
                        if model.askAgent == provider.id {
                            Label("Default", systemImage: "checkmark")
                                .font(TendType.meta.weight(.semibold))
                                .foregroundStyle(.tint)
                        }
                        Spacer()
                        Text(verbatim: provider.id)
                            .font(TendType.detail.monospaced())
                            .foregroundStyle(.secondary)
                            .textSelection(.enabled)
                    }
                    if provider.isApple {
                        AskAppleAvailabilityLine(provider: provider, store: store)
                    } else if provider.kind == .cli {
                        detectionLine
                    }
                } header: {
                    Label {
                        Text(provider.title)
                    } icon: {
                        TendAgentLogo(asset: provider.logo, fallbackSymbol: provider.symbolName, size: 14)
                    }
                }

                if provider.takesCredential {
                    credentialSection
                }

                if isConfigured {
                    detailsSection
                } else if provider.kind == .api, store.providersFromDaemon {
                    modelSection
                }

                if store.configureError != nil || store.configureStatus != nil {
                    Section {
                        AskProviderStatusLines(model: model, store: store)
                    }
                }
            }
            .formStyle(.grouped)

            Divider()
            HStack {
                if store.isConfiguring {
                    ProgressView().controlSize(.small)
                }
                Spacer()
                Button("Done") { dismiss() }
                    .keyboardShortcut(.cancelAction)
            }
            .padding(14)
        }
        .frame(minWidth: 520, idealWidth: 560, minHeight: 420, idealHeight: 520)
        .onAppear {
            store.clearStatus()
            resetFields()
        }
        .onChange(of: provider.title) { _ in resetFields() }
        .onChange(of: provider.model) { _ in resetFields() }
        .onChange(of: provider.cliExecutable) { _ in resetFields() }
    }

    @ViewBuilder
    private var detectionLine: some View {
        switch detection {
        case .probing:
            HStack(spacing: 6) {
                ProgressView().controlSize(.mini)
                TendSettingsCaption("Looking for \(provider.executable) on your PATH…")
            }
        case .installed(let tool):
            // The version line often carries its own parentheses
            // ("2.1.3 (Claude Code)"), so the path gets a line of its own
            // instead of being wrapped in more.
            VStack(alignment: .leading, spacing: 2) {
                Text("Installed · \(InstalledTools.versionSummary(name: tool.name, version: tool.version))")
                    .font(TendType.detail.monospaced())
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                    .lineLimit(1)
                Text(verbatim: tool.path)
                    .font(TendType.detail.monospaced())
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
        case .notInstalled:
            VStack(alignment: .leading, spacing: 4) {
                TendSettingsCaption("Install the \(provider.executable) command and make sure it is on your PATH, then re-check.")
                if let command = provider.installCommand {
                    Text(verbatim: command)
                        .font(TendType.detail.monospaced())
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                }
            }
        }
    }

    private var credentialSection: some View {
        Section {
            HStack(spacing: 8) {
                SecureField("API key", text: $key, prompt: Text(keyFieldTitle))
                    .onSubmit(saveKey)
                Button("Save", action: saveKey)
                    .controlSize(.small)
                    .disabled(trimmedKey.isEmpty)
                if hasKey {
                    Button("Remove", role: .destructive) {
                        store.removeKey(for: provider, model: model)
                    }
                    .controlSize(.small)
                }
            }
            HStack(spacing: 8) {
                if hasKey {
                    Label("Key saved in Keychain", systemImage: "key.fill")
                        .font(TendType.detail)
                        .foregroundStyle(.blue)
                } else if provider.credentialPresent {
                    Label("tendd already has this key in its environment", systemImage: "checkmark.seal.fill")
                        .font(TendType.detail)
                        .foregroundStyle(.green)
                } else if provider.kind == .cli {
                    TendSettingsCaption("Optional. Without a key the CLI's own sign-in is used.")
                } else {
                    TendSettingsCaption("Required. The key is sent to tendd only for each Ask, never written to config or logs.")
                }
                Spacer()
                Text(verbatim: provider.environmentKey)
                    .font(TendType.detail.monospaced())
                    .foregroundStyle(.secondary)
            }
        } header: {
            Text("API key")
        }
    }

    private var keyFieldTitle: String {
        if hasKey {
            return String(localized: "Replace the saved API key")
        }
        return provider.kind == .cli
            ? String(localized: "API key (optional)")
            : String(localized: "API key")
    }

    /// The instance's `[ask.providers.<id>]` fields, saved in one request.
    private var detailsSection: some View {
        Section {
            TextField("Title", text: $titleText, prompt: Text(verbatim: provider.title))
            TextField("Model", text: $modelName, prompt: modelPrompt)
            if provider.kind == .cli {
                TextField("Executable", text: $executablePath, prompt: executablePrompt)
                    .font(.body.monospaced())
            }
            if provider.isApple {
                TendSettingsCaption("Model: on-device, or private-cloud for Private Cloud Compute (macOS 27 or later; not working yet).")
            }
            HStack {
                TendSettingsCaption("Stored under this provider's id in tend's config. Empty fields fall back to the engine's defaults.")
                Spacer()
                Button("Save Changes", action: saveDetails)
                    .controlSize(.small)
                    .disabled(!detailsChanged || store.isConfiguring || !model.isConnected)
            }
        } header: {
            Text("Details")
        }
    }

    private var modelSection: some View {
        Section {
            HStack(spacing: 8) {
                TextField("Model", text: $modelName, prompt: modelPrompt)
                    .onSubmit(saveModel)
                Button("Save Model", action: saveModel)
                    .controlSize(.small)
                    .disabled(!modelChanged || store.isConfiguring || !model.isConnected)
            }
            TendSettingsCaption("Saved to tendd's config for this provider. Leave the field empty to use the daemon's default.")
        } header: {
            Text("Details")
        }
    }

    private var modelPrompt: Text {
        guard let configured = provider.model else { return Text("Default model") }
        return Text(verbatim: configured)
    }

    private var executablePrompt: Text {
        guard let executable = provider.engineDescriptor?.defaultExecutable else {
            return Text("Command or absolute path")
        }
        return Text(verbatim: executable)
    }

    private func resetFields() {
        titleText = provider.title
        modelName = provider.model ?? ""
        executablePath = provider.cliExecutable ?? ""
    }

    private func saveKey() {
        guard !trimmedKey.isEmpty else { return }
        if store.saveKey(key, for: provider, model: model) {
            key = ""
        }
    }

    private func saveModel() {
        guard modelChanged else { return }
        Task { _ = await store.configure(provider: provider, model: modelUpdate, using: model) }
    }

    private func saveDetails() {
        guard detailsChanged else { return }
        Task {
            _ = await store.configure(
                provider: provider,
                title: titleUpdate,
                model: modelUpdate,
                executable: executableUpdate,
                using: model
            )
        }
    }
}

/// Composes a new provider instance: an engine, an id that becomes the config
/// key and the Keychain account, and the fields that make it this account's.
struct AskProviderAddSheet: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: AskProviderStore
    @Environment(\.dismiss) private var dismiss
    @State private var draft = AskProviderDraft()
    /// True once the operator typed an id, so the engine stops prefilling it.
    @State private var identifierEdited = false

    private var taken: Set<String> { store.configuredIdentifiers }
    private var suggestedIdentifier: String {
        AskProviderDraft.suggestedIdentifier(for: draft.engine, taken: taken)
    }

    private var identifier: Binding<String> {
        Binding(
            get: { draft.id },
            set: { entered in
                identifierEdited = !entered.isEmpty
                draft.id = entered
            }
        )
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            VStack(alignment: .leading, spacing: 3) {
                Text("Add Provider")
                    .font(.title3.weight(.semibold))
                Text("A provider is one instance of an engine. Add the same engine twice to keep a work key and a personal key apart.")
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }

            Form {
                if store.supportsInstances, !store.detectedProviders.isEmpty {
                    Section {
                        ForEach(store.detectedProviders) { provider in
                            AskProviderDetectedRow(provider: provider, store: store, model: model) {
                                dismiss()
                            }
                        }
                    } header: {
                        Text("Built-in engines")
                    } footer: {
                        TendSettingsCaption("Engines tendd ships with. Add one to give it its own title, model and API key; add it twice for two accounts.")
                    }
                }
                Section("New provider") {
                    Picker("Engine", selection: $draft.engine) {
                        ForEach(AskProviderEngine.allCases) { engine in
                            Text(verbatim: engine.title).tag(engine)
                        }
                    }
                    TextField("Id", text: identifier, prompt: Text(verbatim: suggestedIdentifier))
                        .font(.body.monospaced())
                    TextField("Title", text: $draft.title, prompt: Text("Optional"))
                    TextField("Model", text: $draft.model, prompt: modelPrompt)
                    if draft.engine.kind == .cli {
                        TextField("Executable", text: $draft.executable, prompt: executablePrompt)
                            .font(.body.monospaced())
                    } else {
                        SecureField("API key", text: $draft.apiKey)
                    }
                }
            }
            .formStyle(.grouped)

            footnote

            if let message = draft.validationMessage(taken: taken), !draft.trimmedID.isEmpty {
                Label(message, systemImage: "exclamationmark.triangle.fill")
                    .font(TendType.detail)
                    .foregroundStyle(.orange)
            }
            if let error = store.configureError {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(TendType.detail)
                    .foregroundStyle(.red)
                    .textSelection(.enabled)
            }

            HStack(spacing: 10) {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                if store.isConfiguring {
                    ProgressView().controlSize(.small)
                }
                Button("Add Provider", action: add)
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut(.defaultAction)
                    .disabled(!draft.isReady(taken: taken) || store.isConfiguring || !model.isConnected)
            }
        }
        .padding(20)
        .frame(width: 520)
        .frame(minHeight: 460, idealHeight: 600)
        .onAppear {
            store.clearStatus()
            prefillIdentifier()
        }
        .onChange(of: draft.engine) { _ in prefillIdentifier() }
    }

    @ViewBuilder
    private var footnote: some View {
        if draft.engine.kind == .api {
            Text("The key is stored in your login Keychain under this provider's id and sent to tendd only for each Ask.")
                .font(TendType.detail)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        } else if draft.engine == .apple {
            Text("Runs on this Mac's own models, with no key. Leave the model empty for on-device, or enter private-cloud for Private Cloud Compute (macOS 27 or later; it does not work yet). The helper ships inside Tend, so the executable can stay empty.")
                .font(TendType.detail)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        } else {
            Text("Leave the executable empty to use the command on tendd's PATH. Point it at an absolute path to pin a second install.")
                .font(TendType.detail)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var modelPrompt: Text {
        guard let engineDefault = draft.engine.defaultModel else { return Text("Engine default") }
        return Text(verbatim: engineDefault)
    }

    private var executablePrompt: Text {
        guard let executable = draft.engine.defaultExecutable else {
            return Text("Command or absolute path")
        }
        return Text(verbatim: executable)
    }

    /// Keeps the id in step with the engine until the operator types one.
    private func prefillIdentifier() {
        guard !identifierEdited else { return }
        draft.id = suggestedIdentifier
    }

    private func add() {
        let draft = draft
        Task {
            let added = await store.addProvider(
                id: draft.trimmedID,
                engine: draft.engine.rawValue,
                title: draft.title,
                model: draft.model,
                executable: draft.engine.kind == .cli ? draft.executable : nil,
                apiKey: draft.apiKey,
                using: model
            )
            if added { dismiss() }
        }
    }
}
