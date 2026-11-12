import AppKit
import SwiftUI

/// Settings › Advanced: the daemon this app talks to, and the configuration
/// file it reads.
struct AdvancedSettingsPane: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: TendConfigStore
    @Environment(\.openWindow) private var openWindow
    @State private var showsDetails = false

    private static let buildCommit: String? = {
        guard let url = Bundle.main.url(forResource: "tend-build", withExtension: "json"),
              let data = try? Data(contentsOf: url),
              let document = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let commit = document["commit"] as? String else { return nil }
        return String(commit.prefix(12))
    }()

    private var localHost: TendFleetHost? {
        model.fleetHosts.first(where: \.local)
    }

    private var connectionTitle: String {
        switch model.connectionState {
        case .connecting: String(localized: "Connecting")
        case .connected: String(localized: "Connected")
        case .upgradeRequired: String(localized: "Upgrade required")
        case .failed: String(localized: "Connection failed")
        }
    }

    private var connectionColor: Color {
        switch model.connectionState {
        case .connected: .green
        case .connecting: .orange
        case .upgradeRequired, .failed: .red
        }
    }

    var body: some View {
        Form {
            daemonSection
            configurationSection
        }
        .formStyle(.grouped)
        .task(id: model.isConnected) {
            await store.load(model: model)
        }
    }

    // MARK: Daemon

    private var daemonSection: some View {
        Section("Daemon") {
            LabeledContent("Status") {
                Label {
                    Text(connectionTitle)
                } icon: {
                    Image(systemName: "circle.fill")
                        .foregroundStyle(connectionColor)
                        .accessibilityHidden(true)
                }
            }
            LabeledContent("tend") {
                Group {
                    if let version = localHost?.tendVersion {
                        Text(version)
                    } else {
                        Text("Unavailable")
                    }
                }
                .monospacedDigit()
            }
            LabeledContent("Socket") {
                Text(model.client.socketPath)
                    .font(TendType.detail.monospaced())
                    .textSelection(.enabled)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            if case .failed(let detail) = model.connectionState {
                Text(detail).font(TendType.detail).foregroundStyle(.red).textSelection(.enabled)
            }
            if case .upgradeRequired(let detail) = model.connectionState {
                Text(detail).font(TendType.detail).foregroundStyle(.red).textSelection(.enabled)
            }
            HStack {
                Button("Retry Connection") { model.retryConnection() }
                    .disabled(model.connectionState == .connecting)
                Spacer()
                TendDaemonReloadButton(model: model) { store.clearRestartNotice() }
            }
            TendSettingsCaption("Reloading replaces the process on the owner-only socket. tmux sessions remain, but native PTY sessions end.")
            DisclosureGroup("Show details", isExpanded: $showsDetails) {
                if let commit = Self.buildCommit {
                    LabeledContent("App build") {
                        Text(verbatim: commit).font(TendType.detail.monospaced()).textSelection(.enabled)
                    }
                }
                LabeledContent("Daemon generation") {
                    Group {
                        if let generation = localHost?.daemonGeneration {
                            Text(verbatim: String(generation))
                        } else {
                            Text("Unavailable")
                        }
                    }
                    .monospacedDigit()
                }
                LabeledContent("Native shells") {
                    Text("\(model.sessions.lazy.filter { !$0.exited }.count) active")
                        .monospacedDigit()
                }
                LabeledContent("Hosts") {
                    Text("\(model.fleetHosts.lazy.filter { $0.state == "online" }.count) / \(model.fleetHosts.count) online")
                        .monospacedDigit()
                }
            }
        }
    }

    // MARK: Configuration file

    private var configurationSection: some View {
        Section("Configuration file") {
            LabeledContent("Path") {
                HStack(spacing: 6) {
                    Group {
                        if store.path.isEmpty {
                            Text("Path unavailable")
                        } else {
                            Text(verbatim: store.path)
                        }
                    }
                    .font(TendType.detail.monospaced())
                    .textSelection(.enabled)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    if store.hasLoaded, store.document?.exists == false {
                        Text("(not created yet)")
                            .font(TendType.detail)
                            .foregroundStyle(.secondary)
                    }
                }
            }
            HStack {
                Button {
                    revealInFinder()
                } label: {
                    Label("Reveal in Finder", systemImage: "folder")
                }
                .disabled(store.path.isEmpty)
                Spacer()
                if store.isLoading || store.isSaving {
                    ProgressView().controlSize(.small)
                }
                Button("Edit Raw TOML…") {
                    openWindow(id: TendRawConfigEditorView.windowID)
                    NSApp.activate(ignoringOtherApps: true)
                }
                .disabled(!store.isSupported)
            }
            if store.isSupported {
                TendSettingsCaption("The tend configuration file tendd reads. Everything the daemon and the CLI can be configured with lives here.")
            } else if !store.isLoading {
                Label("This tendd cannot edit its configuration", systemImage: "lock")
                    .font(TendType.detail.weight(.semibold))
                TendSettingsCaption("Editing the configuration file from the app needs a newer tendd. Update tend and reload the bundled daemon, or edit the file yourself and reload tendd afterwards.")
            }
            if let error = store.loadError {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(TendType.detail)
                    .foregroundStyle(.orange)
                    .textSelection(.enabled)
            }
        }
    }

    private func revealInFinder() {
        guard let url = store.document?.url else { return }
        if FileManager.default.fileExists(atPath: url.path) {
            NSWorkspace.shared.activateFileViewerSelecting([url])
        } else {
            NSWorkspace.shared.activateFileViewerSelecting([url.deletingLastPathComponent()])
        }
    }
}

// MARK: - Raw TOML window

/// The daemon's `config.toml`, read and written whole, in a window of its
/// own. Every section tend understands is reachable here — routes,
/// pipelines, skills, and the ones no form covers — with the daemon
/// validating the document before it replaces the file.
struct TendRawConfigEditorView: View {
    static let windowID = "tend.config-editor"

    @ObservedObject var model: AppModel
    @ObservedObject var store: TendConfigStore
    @State private var confirmsReload = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 8) {
                Image(systemName: "doc.text")
                    .foregroundStyle(.secondary)
                    .accessibilityHidden(true)
                Group {
                    if store.path.isEmpty {
                        Text("Path unavailable")
                    } else {
                        Text(verbatim: store.path)
                    }
                }
                .font(TendType.detail.monospaced())
                .textSelection(.enabled)
                .lineLimit(1)
                .truncationMode(.middle)
                Spacer()
                Button("Reload") {
                    if store.isDirty || store.isLaunchDirty {
                        confirmsReload = true
                    } else {
                        Task { await store.load(model: model, force: true) }
                    }
                }
                .controlSize(.small)
                .disabled(store.isLoading || store.isSaving)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 10)

            if store.isLaunchDirty {
                Label("Save or discard launch option changes before editing Raw TOML.", systemImage: "exclamationmark.triangle")
                    .font(TendType.detail)
                    .foregroundStyle(.orange)
                    .padding(.horizontal, 16)
                    .padding(.bottom, 8)
            }

            Divider()

            TextEditor(text: $store.draft)
                .disabled(store.isLoading || store.isSaving || store.isLaunchDirty || !store.isSupported)
                .font(.system(size: 12, design: .monospaced))
                .disableAutocorrection(true)
                .accessibilityLabel(Text("Configuration file"))
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .overlay(alignment: .center) {
                    if !store.hasLoaded, store.isLoading {
                        ProgressView()
                    }
                }
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            TendSettingsFooter(
                isBusy: store.isSaving || store.isLoading,
                canRevert: store.isDirty,
                canSave: store.isDirty && !store.isLaunchDirty,
                revert: { store.revertDraft() },
                save: { Task { await store.save(model: model) } }
            ) {
                footerStatus
            }
        }
        .frame(minWidth: 560, idealWidth: 760, minHeight: 420, idealHeight: 620)
        .task(id: model.isConnected) {
            await store.load(model: model)
        }
        .confirmationDialog("Discard unsaved configuration changes and reload?", isPresented: $confirmsReload) {
            Button("Discard and Reload", role: .destructive) {
                Task { await store.load(model: model, force: true) }
            }
        }
    }

    @ViewBuilder
    private var footerStatus: some View {
        if let conflict = store.conflictMessage {
            // A concurrent edit, not a bad document: the daemon sent back
            // what is on disk, which is now the baseline.
            VStack(alignment: .leading, spacing: 3) {
                Label(conflict, systemImage: "arrow.triangle.branch")
                    .foregroundStyle(.orange)
                    .textSelection(.enabled)
                Text("Your raw text is preserved. Reload to take the file's version, or review it before saving again.")
                    .foregroundStyle(.secondary)
            }
        } else if let error = store.saveError {
            // tendd's own parse/validation message, verbatim.
            Label(error, systemImage: "xmark.octagon.fill")
                .font(TendType.detail.monospaced())
                .foregroundStyle(.red)
                .textSelection(.enabled)
        } else if let error = store.loadError {
            Label(error, systemImage: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
                .textSelection(.enabled)
        } else if let status = store.status {
            Label(status, systemImage: "checkmark.circle.fill")
                .foregroundStyle(.green)
        } else {
            Text("Saving checks the file has not changed underneath and refuses a document tend cannot parse.")
                .foregroundStyle(.secondary)
        }
    }
}

// MARK: - Launch arguments (Settings › Agents)

/// The arguments agents are launched with: one row per provider, and the
/// per-pipeline overrides behind a disclosure.
struct TendLaunchArgumentsSections: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: TendConfigStore
    @State private var showsPipelines = false

    var body: some View {
        Section {
            if store.isDirty {
                Label("Raw TOML has unsaved changes. Save or reload it first.", systemImage: "exclamationmark.triangle")
                    .font(TendType.detail)
                    .foregroundStyle(.orange)
            }
            if store.launchNeedsReload {
                HStack {
                    Label("Reload to refresh launch options from the configuration file.", systemImage: "arrow.clockwise")
                        .font(TendType.detail)
                        .foregroundStyle(.orange)
                    Spacer()
                    Button("Reload") { Task { await store.load(model: model, force: true) } }
                        .controlSize(.small)
                        .disabled(store.isLoading || store.isSaving || store.isDirty || store.isLaunchDirty)
                }
            }
            if let settings = store.launchDraft {
                if let program = settings.legacyGuide.program {
                    VStack(alignment: .leading, spacing: 4) {
                        Text("Legacy guide options").font(TendType.detail.weight(.semibold))
                        TendSettingsCaption("[mcp.guide].options still supplies \(program)'s fallback. Provider defaults override it, even when empty. These legacy settings are preserved; edit them in Raw TOML if needed.")
                        Text(verbatim: TendLaunchSettings.displayText(settings.legacyGuide.options))
                            .font(TendType.detail.monospaced())
                            .textSelection(.enabled)
                    }
                }
                ForEach(settings.providers.indices, id: \.self) { index in
                    let provider = settings.providers[index]
                    TendLaunchOptionsRow(
                        title: provider.program,
                        detail: "[agent.\(provider.program)]",
                        inheritLabel: "Inherit legacy guide options or no options",
                        options: Binding(
                            get: { store.launchDraft?.providers[index].options },
                            set: { store.launchDraft?.providers[index].options = $0 }
                        ),
                        effective: settings.effectiveOptions(program: provider.program, override: nil)
                    )
                    .disabled(formDisabled)
                }
                DisclosureGroup("Pipeline overrides", isExpanded: $showsPipelines) {
                    if settings.pipelines.isEmpty {
                        TendSettingsCaption("No pipeline agents configured. Add pipelines in Raw TOML.")
                    }
                    ForEach(settings.pipelines.indices, id: \.self) { index in
                        let agent = settings.pipelines[index]
                        TendLaunchOptionsRow(
                            title: "\(agent.pipeline) / \(agent.name) · \(agent.program)",
                            detail: String(localized: "pipeline agent #\(agent.index + 1)"),
                            inheritLabel: "Inherit provider defaults",
                            options: Binding(
                                get: { store.launchDraft?.pipelines[index].options },
                                set: { store.launchDraft?.pipelines[index].options = $0 }
                            ),
                            effective: settings.effectiveOptions(program: agent.program, override: agent.options)
                        )
                        .disabled(formDisabled)
                    }
                }
            } else if !store.isLoading {
                TendSettingsCaption(store.supportsLaunch
                     ? "Launch options could not be read. Correct the configuration in Raw TOML and reload."
                     : "This tendd does not support launch option forms. Update the daemon or use Raw TOML.")
            }
        } header: {
            HStack(spacing: 6) {
                Text("Launch arguments")
                TendSettingsInfoButton(title: "Launch arguments") {
                    Text("Each entry is one literal CLI argument, not a shell command. Options replace the entire inherited list; they are never appended. Changes affect future launches, not running agents.")
                }
            }
        }
    }

    private var formDisabled: Bool {
        store.isSaving || store.isLoading || store.isDirty || store.launchNeedsReload
    }
}

private struct TendLaunchOptionsRow: View {
    let title: String
    let detail: String
    let inheritLabel: LocalizedStringKey
    @Binding var options: [String]?
    let effective: [String]

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline) {
                Text(verbatim: title).font(.headline)
                Text(verbatim: detail).font(TendType.detail.monospaced()).foregroundStyle(.secondary)
                Spacer()
                Toggle("Override inherited options", isOn: Binding(
                    get: { options != nil },
                    set: { options = $0 ? effective : nil }
                ))
                .toggleStyle(.checkbox)
            }
            if let arguments = options {
                ForEach(arguments.indices, id: \.self) { index in
                    HStack {
                        Text(verbatim: "\(index + 1)").foregroundStyle(.secondary).accessibilityHidden(true)
                        TextField("Argument (empty string allowed)", text: Binding(
                            get: { options?[index] ?? "" },
                            set: { options?[index] = $0 }
                        ))
                        .labelsHidden()
                        .font(.system(.body, design: .monospaced))
                        .textFieldStyle(.roundedBorder)
                        Button { options?.swapAt(index, index - 1) } label: {
                            Image(systemName: "arrow.up")
                        }
                        .buttonStyle(.borderless)
                        .disabled(index == 0)
                        .accessibilityLabel("Move argument up")
                        Button { options?.remove(at: index) } label: {
                            Image(systemName: "minus.circle")
                        }
                        .buttonStyle(.borderless)
                        .accessibilityLabel("Remove argument")
                    }
                }
                HStack {
                    Button("Add Argument") { options?.append("") }
                    Button("Use Empty List []") { options = [] }
                }
                .controlSize(.small)
                if arguments.isEmpty {
                    TendSettingsCaption("Explicit []: no extra options, even if defaults exist.")
                }
            } else {
                TendSettingsCaption(inheritLabel)
            }
            Text("Effective options: \(TendLaunchSettings.displayText(effective))")
                .font(TendType.detail.monospaced())
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
        }
        .padding(.vertical, 2)
    }
}
