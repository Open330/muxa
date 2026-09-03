import SwiftUI

/// Settings › Providers: the one place to set up Global Ask. Driven by the
/// daemon's provider list when available, the built-in CLIs otherwise.
struct AskProvidersSettingsPane: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: AskProviderStore
    @State private var confirmsReload = false

    private var selectedProviderIsListed: Bool {
        store.providers.contains { $0.id == model.askAgent }
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                settingsHeading(
                    "Ask Providers",
                    detail: "Global Ask runs an installed agent CLI headlessly or calls a hosted API. Keys stay in the macOS login Keychain."
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

                ForEach(store.providers) { provider in
                    AskProviderCard(provider: provider, store: store, model: model)
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
        .alert("Reload the bundled muxad?", isPresented: $confirmsReload) {
            Button("Cancel", role: .cancel) {}
            Button("Reload", role: .destructive) { model.replaceRunningDaemon() }
        } message: {
            Text("Native PTY sessions owned by muxad will end. tmux sessions are not terminated.")
        }
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
                            Text("\(provider.title) (\(reason))")
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

/// One provider: kind badge, detection, credential state and actions.
struct AskProviderCard: View {
    let provider: MuxaAskProvider
    @ObservedObject var store: AskProviderStore
    @ObservedObject var model: AppModel
    @State private var key = ""
    @State private var modelName = ""

    private var hasKey: Bool { store.hasKey(provider) }
    private var detection: AskProviderDetection { store.detection(for: provider) }
    private var usability: AskProviderUsability { store.usability(provider) }
    private var isSelected: Bool { model.askAgent == provider.id }

    private var trimmedKey: String {
        key.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private var modelChanged: Bool {
        modelName.trimmingCharacters(in: .whitespacesAndNewlines) != (provider.model ?? "")
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            header

            if provider.kind == .cli {
                detectionLine
            }

            credentialRow

            if provider.kind == .api, store.providersFromDaemon {
                modelRow
            }
        }
        .padding(14)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
        .overlay {
            RoundedRectangle(cornerRadius: 12)
                .stroke(isSelected ? Color.accentColor.opacity(0.5) : Color.primary.opacity(0.06), lineWidth: 1)
        }
        .onAppear { modelName = provider.model ?? "" }
        .onChange(of: provider.model) { current in modelName = current ?? "" }
    }

    private var header: some View {
        HStack(spacing: 8) {
            Image(systemName: provider.symbolName)
                .foregroundStyle(.tint)
                .frame(width: 20)
            Text(provider.title)
                .font(.headline)
            Text(provider.kind == .cli ? "CLI" : "API")
                .font(.caption2.weight(.semibold))
                .padding(.horizontal, 6)
                .padding(.vertical, 2)
                .background(Color.primary.opacity(0.08), in: Capsule())
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

    private var modelRow: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                TextField("Model", text: $modelName, prompt: Text(provider.model ?? "Default model"))
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

    private func saveKey() {
        guard !trimmedKey.isEmpty else { return }
        if store.saveKey(key, for: provider, model: model) {
            key = ""
        }
    }

    private func saveModel() {
        guard modelChanged else { return }
        Task { await store.configure(provider: provider, model: modelName, using: model) }
    }
}
