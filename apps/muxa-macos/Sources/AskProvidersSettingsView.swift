import SwiftUI

/// Settings › Providers: the one place to set up Global Ask.
///
/// A provider is an *instance* of a closed engine, so the pane shows what the
/// user has configured first and offers muxad's built-ins below it. Daemons
/// that predate provider instances fall back to the flat, read-only list.
struct AskProvidersSettingsPane: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: AskProviderStore
    @State private var confirmsReload = false
    @State private var isAddingProvider = false
    @State private var removalTarget: MuxaAskProvider?

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
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                settingsHeading(
                    "Ask Providers",
                    detail: "Global Ask runs an installed agent CLI headlessly or calls a hosted API. Add one provider per account: keys stay in the macOS login Keychain, one per provider id."
                )

                if model.askEnabled == false {
                    disabledBanner
                }

                defaultProviderCard

                if let error = store.loadError {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .font(.caption)
                        .foregroundStyle(.orange)
                        .textSelection(.enabled)
                }

                if store.supportsInstances {
                    configuredSection
                    detectedSection
                } else {
                    legacyList
                }

                statusLines

                HStack(spacing: 10) {
                    Text("Re-check after installing a CLI. Reload muxad only when its PATH must change too.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Spacer()
                    Button {
                        Task { await store.detectInstalledTools(force: true) }
                    } label: {
                        Label("Re-check CLIs", systemImage: "arrow.clockwise")
                    }
                    .disabled(store.isDetecting)
                    Button("Reload muxad PATH…") { confirmsReload = true }
                }
            }
            .padding(20)
        }
        .task(id: model.isConnected) {
            await store.reload(model: model)
        }
        .sheet(isPresented: $isAddingProvider) {
            AskProviderAddSheet(model: model, store: store)
        }
        .alert("Reload the bundled muxad?", isPresented: $confirmsReload) {
            Button("Cancel", role: .cancel) {}
            Button("Reload", role: .destructive) { model.replaceRunningDaemon() }
        } message: {
            Text("Native PTY sessions owned by muxad will end. tmux sessions are not terminated.")
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
                Text("\(provider.title) is the default provider, so muxad will fall back to another one. Past conversations stay in your history.")
            } else {
                Text("\(provider.title) is deleted from muxa's config. Past conversations stay in your history.")
            }
        }
    }

    private func remove(_ provider: MuxaAskProvider, deletingKey: Bool) {
        removalTarget = nil
        Task { _ = await store.removeProvider(provider, deletingKey: deletingKey, using: model) }
    }

    private var disabledBanner: some View {
        HStack {
            Label(
                model.askConfigurationPendingReload
                    ? "Global Ask is enabled in config. Reload muxad to apply it."
                    : "Global Ask is disabled in muxa configuration.",
                systemImage: "exclamationmark.circle"
            )
            .foregroundStyle(.orange)
            Spacer()
            if model.isEnablingAsk {
                ProgressView().controlSize(.small)
            }
            Button(model.askConfigurationPendingReload ? "Reload muxad" : "Enable Global Ask") {
                Task { await model.enableAsk() }
            }
            .buttonStyle(.borderedProminent)
            .disabled(model.isEnablingAsk)
        }
        .padding(12)
        .background(Color.orange.opacity(0.08), in: RoundedRectangle(cornerRadius: 12))
    }

    private var defaultProviderCard: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 10) {
                Label("Default provider", systemImage: "sparkles")
                    .font(.headline)
                Spacer()
                if store.isLoading {
                    ProgressView().controlSize(.small)
                }
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
                .labelsHidden()
                .frame(width: 220)
                .disabled(!model.isConnected)
            }
            Text(
                store.providersFromDaemon
                    ? "New Ask conversations start with this provider. Entries are disabled until their CLI is installed or a key is saved."
                    : "This muxad predates the provider list; only the built-in CLIs are shown. Update muxa or choose Use Bundled muxad for API providers."
            )
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        .padding(14)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
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

    /// Instances in muxa's config: renameable, re-keyable, removable.
    private var configuredSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 10) {
                Label("Configured", systemImage: "checklist")
                    .font(.headline)
                Spacer()
                if store.isConfiguring {
                    ProgressView().controlSize(.small)
                }
                Button {
                    isAddingProvider = true
                } label: {
                    Label("Add Provider…", systemImage: "plus")
                }
                .disabled(!model.isConnected || store.isConfiguring)
            }
            if store.configuredProviders.isEmpty {
                Text("Nothing configured yet. Add a provider, or add one of the detected built-ins below.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            ForEach(store.configuredProviders) { provider in
                AskProviderCard(provider: provider, store: store, model: model) {
                    removalTarget = provider
                }
            }
        }
    }

    /// Built-in engines muxad answers with even without config.
    private var detectedSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label("Detected", systemImage: "sparkle.magnifyingglass")
                .font(.headline)
            Text("Engines muxad ships with. Add one to give it its own title, model and API key; add it twice for two accounts.")
                .font(.caption)
                .foregroundStyle(.secondary)
            if store.detectedProviders.isEmpty {
                Text("Every built-in engine is already configured.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            ForEach(store.detectedProviders) { provider in
                AskProviderDetectedRow(provider: provider, store: store, model: model)
            }
        }
    }

    /// A daemon that lists providers but does not understand instances: the
    /// old flat list, with a note explaining why Add and Remove are missing.
    private var legacyList: some View {
        VStack(alignment: .leading, spacing: 10) {
            if store.providersFromDaemon {
                Label(
                    "This muxad lists providers but cannot add or remove them. Update muxa or choose Use Bundled muxad to compose your own provider list.",
                    systemImage: "info.circle"
                )
                .font(.caption)
                .foregroundStyle(.secondary)
            }
            ForEach(store.providers) { provider in
                AskProviderCard(provider: provider, store: store, model: model) {}
            }
        }
    }

    @ViewBuilder
    private var statusLines: some View {
        if let status = model.askSettingsStatus {
            Label(status, systemImage: "checkmark.circle.fill")
                .font(.caption)
                .foregroundStyle(.green)
        }
        if let status = store.configureStatus {
            Label(status, systemImage: "checkmark.circle.fill")
                .font(.caption)
                .foregroundStyle(.green)
        }
        if let error = model.askSettingsError ?? model.askError {
            Label(error, systemImage: "exclamationmark.triangle.fill")
                .font(.caption)
                .foregroundStyle(.red)
                .textSelection(.enabled)
        }
        if let error = store.configureError {
            Label(error, systemImage: "exclamationmark.triangle.fill")
                .font(.caption)
                .foregroundStyle(.red)
                .textSelection(.enabled)
        }
    }
}

/// CLI/API pill, shared by the provider card and the detected row.
struct AskProviderKindBadge: View {
    let kind: MuxaAskProvider.Kind

    var body: some View {
        Group {
            if kind == .cli {
                Text("CLI")
            } else {
                Text("API")
            }
        }
        .font(.caption2.weight(.semibold))
        .padding(.horizontal, 6)
        .padding(.vertical, 2)
        .background(Color.primary.opacity(0.08), in: Capsule())
    }
}

/// A built-in engine the user has not configured: what it needs, and Add.
struct AskProviderDetectedRow: View {
    let provider: MuxaAskProvider
    @ObservedObject var store: AskProviderStore
    @ObservedObject var model: AppModel

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: provider.symbolName)
                .foregroundStyle(.tint)
                .frame(width: 20)
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    Text(provider.title)
                        .font(.subheadline.weight(.medium))
                    AskProviderKindBadge(kind: provider.kind)
                    if provider.id == model.askAgent {
                        Label("Default", systemImage: "checkmark")
                            .font(.caption2.weight(.semibold))
                            .foregroundStyle(.tint)
                    }
                }
                requirement
            }
            Spacer(minLength: 8)
            Button {
                Task {
                    _ = await store.addProvider(
                        id: provider.id,
                        engine: provider.engine,
                        using: model
                    )
                }
            } label: {
                Label("Add", systemImage: "plus")
            }
            .controlSize(.small)
            .disabled(!model.isConnected || store.isConfiguring)
            .help("Writes this engine into muxa's config so it can carry its own title, model and key")
        }
        .padding(12)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
    }

    /// What the engine still needs before it can answer an Ask.
    @ViewBuilder
    private var requirement: some View {
        if provider.kind == .api {
            if provider.credentialPresent {
                Text("Key already in muxad's environment")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                Text("API key required")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        } else {
            switch store.detection(for: provider) {
            case .probing:
                Text("Looking for \(provider.executable) on your PATH…")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            case .installed(let tool):
                Text("CLI found at \(tool.path)")
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            case .notInstalled:
                Text("\(provider.executable) is not on your PATH yet")
                    .font(.caption)
                    .foregroundStyle(.orange)
            }
        }
    }
}

/// One configured provider: kind badge, detection, credential state, the
/// `[ask.providers.<id>]` fields, and Remove.
struct AskProviderCard: View {
    let provider: MuxaAskProvider
    @ObservedObject var store: AskProviderStore
    @ObservedObject var model: AppModel
    /// Asks the pane to confirm removal; ignored for a built-in row.
    let onRemove: () -> Void
    @State private var key = ""
    @State private var titleText = ""
    @State private var modelName = ""
    @State private var executablePath = ""

    private var hasKey: Bool { store.hasKey(provider) }
    private var detection: AskProviderDetection { store.detection(for: provider) }
    private var usability: AskProviderUsability { store.usability(provider) }
    private var isSelected: Bool { model.askAgent == provider.id }
    /// Only an instance that exists in config can be edited or removed.
    private var isConfigured: Bool { store.supportsInstances && provider.isConfigured }

    private var trimmedKey: String {
        key.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private var titleUpdate: MuxaAskProviderFieldUpdate {
        titleText.trimmingCharacters(in: .whitespacesAndNewlines) == provider.title
            ? .keep
            : MuxaAskProviderFieldUpdate(titleText)
    }

    private var modelUpdate: MuxaAskProviderFieldUpdate {
        modelName.trimmingCharacters(in: .whitespacesAndNewlines) == (provider.model ?? "")
            ? .keep
            : MuxaAskProviderFieldUpdate(modelName)
    }

    private var executableUpdate: MuxaAskProviderFieldUpdate {
        executablePath.trimmingCharacters(in: .whitespacesAndNewlines) == (provider.cliExecutable ?? "")
            ? .keep
            : MuxaAskProviderFieldUpdate(executablePath)
    }

    private var detailsChanged: Bool {
        titleUpdate != .keep || modelUpdate != .keep || executableUpdate != .keep
    }

    private var modelChanged: Bool { modelUpdate != .keep }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            header

            if provider.kind == .cli {
                detectionLine
            }

            credentialRow

            if isConfigured {
                detailsEditor
            } else if provider.kind == .api, store.providersFromDaemon {
                modelRow
            }
        }
        .padding(14)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
        .overlay {
            RoundedRectangle(cornerRadius: 12)
                .stroke(isSelected ? Color.accentColor.opacity(0.5) : Color.primary.opacity(0.06), lineWidth: 1)
        }
        .onAppear(perform: resetFields)
        .onChange(of: provider.title) { _ in resetFields() }
        .onChange(of: provider.model) { _ in resetFields() }
        .onChange(of: provider.cliExecutable) { _ in resetFields() }
    }

    private var header: some View {
        HStack(spacing: 8) {
            Image(systemName: provider.symbolName)
                .foregroundStyle(.tint)
                .frame(width: 20)
            Text(provider.title)
                .font(.headline)
            AskProviderKindBadge(kind: provider.kind)
            usabilityBadge
            if isSelected {
                Label("Default", systemImage: "checkmark")
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(.tint)
            }
            Spacer()
            if provider.kind == .cli {
                Button("Log In…") {
                    Task { await model.openProviderCLI(provider) }
                }
                .controlSize(.small)
                .disabled(detection.tool == nil || !model.isConnected)
                .help("Opens the CLI in a Muxa shell so you can sign in")
            }
            if isConfigured {
                Button("Remove…", role: .destructive, action: onRemove)
                    .controlSize(.small)
                    .disabled(!model.isConnected || store.isConfiguring)
                    .help("Deletes this provider from muxa's config")
            }
        }
    }

    @ViewBuilder
    private var usabilityBadge: some View {
        switch usability {
        case .usable:
            Label("Ready", systemImage: "checkmark.circle.fill")
                .font(.caption2.weight(.semibold))
                .foregroundStyle(.green)
        case .probing:
            Label("Checking", systemImage: "ellipsis.circle")
                .font(.caption2.weight(.semibold))
                .foregroundStyle(.secondary)
        case .notInstalled:
            Label("Not installed", systemImage: "xmark.circle")
                .font(.caption2.weight(.semibold))
                .foregroundStyle(.red)
        case .missingKey:
            Label("API key required", systemImage: "key")
                .font(.caption2.weight(.semibold))
                .foregroundStyle(.orange)
        }
    }

    @ViewBuilder
    private var detectionLine: some View {
        switch detection {
        case .probing:
            HStack(spacing: 6) {
                ProgressView().controlSize(.mini)
                Text("Looking for \(provider.executable) on your PATH…")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        case .installed(let tool):
            Text("Installed · \(tool.name) \(tool.version ?? "") at \(tool.path)")
                .font(.caption.monospaced())
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
                .lineLimit(2)
        case .notInstalled:
            VStack(alignment: .leading, spacing: 4) {
                Text("Install the \(provider.executable) command and make sure it is on your PATH, then re-check.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                if let command = provider.installCommand {
                    Text(verbatim: command)
                        .font(.caption.monospaced())
                        .foregroundStyle(.tertiary)
                        .textSelection(.enabled)
                }
            }
        }
    }

    private var credentialRow: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                SecureField(keyFieldTitle, text: $key)
                    .textFieldStyle(.roundedBorder)
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
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(.blue)
                } else if provider.credentialPresent {
                    Label("muxad already has this key in its environment", systemImage: "checkmark.seal.fill")
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(.green)
                } else if provider.kind == .cli {
                    Text("Optional. Without a key the CLI's own sign-in is used.")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                } else {
                    Text("Required. The key is sent to muxad only for each Ask, never written to config or logs.")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Text(verbatim: provider.environmentKey)
                    .font(.caption2.monospaced())
                    .foregroundStyle(.tertiary)
            }
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
    private var detailsEditor: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                TextField("Title", text: $titleText, prompt: Text(verbatim: provider.title))
                    .textFieldStyle(.roundedBorder)
                TextField("Model", text: $modelName, prompt: modelPrompt)
                    .textFieldStyle(.roundedBorder)
            }
            if provider.kind == .cli {
                TextField("Executable", text: $executablePath, prompt: executablePrompt)
                    .textFieldStyle(.roundedBorder)
                    .font(.body.monospaced())
            }
            HStack(spacing: 8) {
                Text("Stored under this provider's id in muxa's config. Empty fields fall back to the engine's defaults.")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                Spacer()
                if store.isConfiguring {
                    ProgressView().controlSize(.small)
                }
                Button("Save Changes", action: saveDetails)
                    .controlSize(.small)
                    .disabled(!detailsChanged || store.isConfiguring || !model.isConnected)
            }
        }
    }

    private var modelRow: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                TextField("Model", text: $modelName, prompt: modelPrompt)
                    .textFieldStyle(.roundedBorder)
                    .onSubmit(saveModel)
                Button("Save Model", action: saveModel)
                    .controlSize(.small)
                    .disabled(!modelChanged || store.isConfiguring || !model.isConnected)
                if store.isConfiguring {
                    ProgressView().controlSize(.small)
                }
            }
            Text("Saved to muxad's config for this provider. Leave the field empty to use the daemon's default.")
                .font(.caption2)
                .foregroundStyle(.secondary)
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
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }

            Form {
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
            .formStyle(.grouped)

            footnote

            if let message = draft.validationMessage(taken: taken), !draft.trimmedID.isEmpty {
                Label(message, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.orange)
            }
            if let error = store.configureError {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
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
        .frame(width: 460)
        .onAppear {
            store.clearStatus()
            prefillIdentifier()
        }
        .onChange(of: draft.engine) { _ in prefillIdentifier() }
    }

    @ViewBuilder
    private var footnote: some View {
        if draft.engine.kind == .api {
            Text("The key is stored in your login Keychain under this provider's id and sent to muxad only for each Ask.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        } else {
            Text("Leave the executable empty to use the command on muxad's PATH. Point it at an absolute path to pin a second install.")
                .font(.caption)
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
