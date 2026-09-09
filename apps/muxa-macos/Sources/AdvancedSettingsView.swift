import AppKit
import SwiftUI

/// Settings › Advanced: the daemon's `config.toml`, read and written whole.
///
/// Every section muxa understands is reachable here — routes, pipelines,
/// skills, and the ones no form covers — with the daemon validating the
/// document before it replaces the file.
struct AdvancedSettingsPane: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: MuxaConfigStore
    @State private var showsLaunchOptions = true
    @State private var confirmsReload = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            if store.isSupported {
                VStack(spacing: 0) {
                    Picker("Configuration editor", selection: $showsLaunchOptions) {
                        Text("Launch options").tag(true)
                        Text("Raw TOML").tag(false)
                    }
                    .pickerStyle(.segmented)
                    .padding(12)
                    editor
                }
            } else {
                unsupported
            }
        }
        .task(id: model.isConnected) {
            await store.load(model: model)
        }
        .confirmationDialog("Discard unsaved configuration changes and reload?", isPresented: $confirmsReload) {
            Button("Discard and Reload", role: .destructive) {
                Task { await store.load(model: model, force: true) }
            }
        }
    }

    // MARK: Header

    private var header: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .top) {
                settingsHeading(
                    "Advanced",
                    detail: "The muxa configuration file muxad reads. Everything the daemon and the CLI can be configured with lives here."
                )
                Spacer()
                if store.isLoading || store.isSaving {
                    ProgressView().controlSize(.small)
                }
            }
            HStack(spacing: 8) {
                Image(systemName: "doc.text")
                    .foregroundStyle(.secondary)
                Group {
                    if store.path.isEmpty {
                        Text("Path unavailable")
                    } else {
                        Text(verbatim: store.path)
                    }
                }
                .font(.caption.monospaced())
                .textSelection(.enabled)
                .lineLimit(1)
                .truncationMode(.middle)
                if store.hasLoaded, store.document?.exists == false {
                    Text("(not created yet)")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Button {
                    revealInFinder()
                } label: {
                    Label("Reveal in Finder", systemImage: "folder")
                }
                .controlSize(.small)
                .disabled(store.path.isEmpty)
            }
        }
        .padding(20)
    }

    // MARK: Editor

    private var editor: some View {
        VStack(alignment: .leading, spacing: 0) {
            if showsLaunchOptions {
                launchOptions
            } else {
                if store.isLaunchDirty {
                    Label("Save or discard launch option changes before editing Raw TOML.", systemImage: "exclamationmark.triangle")
                        .font(.caption)
                        .foregroundStyle(.orange)
                        .padding(12)
                }
                TextEditor(text: $store.draft)
                .disabled(store.isLoading || store.isSaving || store.isLaunchDirty)
                .font(.system(size: 12, design: .monospaced))
                .disableAutocorrection(true)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .overlay(alignment: .center) {
                    if !store.hasLoaded, store.isLoading {
                        ProgressView()
                    }
                }
            }

            Divider()

            VStack(alignment: .leading, spacing: 8) {
                if let conflict = store.conflictMessage {
                    // A concurrent edit, not a bad document: the daemon sent
                    // back what is on disk, which is now the baseline.
                    VStack(alignment: .leading, spacing: 3) {
                        Label(conflict, systemImage: "arrow.triangle.branch")
                            .foregroundStyle(.orange)
                            .textSelection(.enabled)
                        Text(showsLaunchOptions
                             ? "Reload to review the current file before applying launch options again."
                             : "Your raw text is preserved. Reload to take the file's version, or review it before saving again.")
                            .foregroundStyle(.secondary)
                    }
                    .font(.caption)
                    .fixedSize(horizontal: false, vertical: true)
                } else if let error = store.saveError {
                    // muxad's own parse/validation message, verbatim.
                    Label(error, systemImage: "xmark.octagon.fill")
                        .font(.caption.monospaced())
                        .foregroundStyle(.red)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
                if let error = store.loadError {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .font(.caption)
                        .foregroundStyle(.orange)
                        .textSelection(.enabled)
                }
                if let status = store.status, store.saveError == nil {
                    Label(status, systemImage: "checkmark.circle.fill")
                        .font(.caption)
                        .foregroundStyle(.green)
                }

                HStack(spacing: 10) {
                    Text("Most changes apply when muxad restarts. Saving checks the file has not changed underneath and refuses a document muxa cannot parse.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    Spacer(minLength: 8)
                    MuxaDaemonReloadButton(model: model)
                    Button("Reload") {
                        if store.isDirty || store.isLaunchDirty {
                            confirmsReload = true
                        } else {
                            Task { await store.load(model: model, force: true) }
                        }
                    }
                    .disabled(store.isLoading || store.isSaving)
                    Button("Save") {
                        Task {
                            if showsLaunchOptions { await store.saveLaunch(model: model) }
                            else { await store.save(model: model) }
                        }
                    }
                    .buttonStyle(.borderedProminent)
                    .disabled(store.isSaving || store.isLoading || (showsLaunchOptions
                        ? !store.isLaunchDirty || store.isDirty || store.launchNeedsReload
                        : !store.isDirty || store.isLaunchDirty))
                }
                .controlSize(.small)
            }
            .padding(16)
        }
    }

    private var launchOptions: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                Text("Each entry is one literal CLI argument, not a shell command. Options replace the entire inherited list; they are never appended. Changes affect future launches, not running agents.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                if store.isDirty {
                    Label("Raw TOML has unsaved changes. Save or reload it first.", systemImage: "exclamationmark.triangle")
                        .foregroundStyle(.orange)
                }
                if store.launchNeedsReload {
                    Label("Reload to refresh launch options from the configuration file.", systemImage: "arrow.clockwise")
                        .foregroundStyle(.orange)
                }
                if let settings = store.launchDraft {
                    if let program = settings.legacyGuide.program {
                        GroupBox("Legacy guide options") {
                            VStack(alignment: .leading, spacing: 6) {
                                Text("[mcp.guide].options still supplies \(program)'s fallback. Provider defaults override it, even when empty. These legacy settings are preserved; edit them in Raw TOML if needed.")
                                Text(verbatim: String(describing: settings.legacyGuide.options))
                                    .font(.caption.monospaced())
                                    .textSelection(.enabled)
                            }
                            .frame(maxWidth: .infinity, alignment: .leading)
                        }
                    }
                    Text("Provider defaults").font(.headline)
                    ForEach(settings.providers.indices, id: \.self) { index in
                        let provider = settings.providers[index]
                        MuxaLaunchOptionsRow(
                            title: provider.program,
                            detail: "[agent.\(provider.program)]",
                            inheritLabel: "Inherit legacy guide options or no options",
                            options: Binding(
                                get: { store.launchDraft?.providers[index].options },
                                set: { store.launchDraft?.providers[index].options = $0 }
                            ),
                            effective: settings.effectiveOptions(program: provider.program, override: nil)
                        )
                    }
                    Text("Pipeline overrides").font(.headline)
                    if settings.pipelines.isEmpty {
                        Text("No pipeline agents configured. Add pipelines in Raw TOML.")
                            .foregroundStyle(.secondary)
                    }
                    ForEach(settings.pipelines.indices, id: \.self) { index in
                        let agent = settings.pipelines[index]
                        MuxaLaunchOptionsRow(
                            title: "\(agent.pipeline) / \(agent.name) · \(agent.program)",
                            detail: "pipeline agent #\(agent.index + 1)",
                            inheritLabel: "Inherit provider defaults",
                            options: Binding(
                                get: { store.launchDraft?.pipelines[index].options },
                                set: { store.launchDraft?.pipelines[index].options = $0 }
                            ),
                            effective: settings.effectiveOptions(program: agent.program, override: agent.options)
                        )
                    }
                    Button("Discard Launch Changes") { store.discardLaunch() }
                        .disabled(!store.isLaunchDirty)
                } else if !store.isLoading {
                    Text(store.supportsLaunch
                         ? "Launch options could not be read. Correct the configuration in Raw TOML and reload."
                         : "This muxad does not support launch option forms. Update the daemon or use Raw TOML.")
                        .foregroundStyle(.secondary)
                }
            }
            .disabled(store.isSaving || store.isLoading || store.isDirty || store.launchNeedsReload)
            .padding(16)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    // MARK: Capability fallback

    private var unsupported: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label("This muxad cannot edit its configuration", systemImage: "lock")
                .font(.headline)
            Text("Editing the configuration file from the app needs a newer muxad. Update muxa and reload the bundled daemon, or edit the file yourself and reload muxad afterwards.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            Text("The file muxad reads is named at the top of this pane; muxa config path prints it from the command line.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            HStack {
                MuxaDaemonReloadButton(model: model)
                Spacer()
            }
            .controlSize(.small)
            Spacer()
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(20)
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

private struct MuxaLaunchOptionsRow: View {
    let title: String
    let detail: String
    let inheritLabel: LocalizedStringKey
    @Binding var options: [String]?
    let effective: [String]

    var body: some View {
        GroupBox {
            VStack(alignment: .leading, spacing: 8) {
                Text(verbatim: detail).font(.caption.monospaced()).foregroundStyle(.secondary)
                Toggle("Override inherited options", isOn: Binding(
                    get: { options != nil },
                    set: { options = $0 ? effective : nil }
                ))
                if let arguments = options {
                    ForEach(arguments.indices, id: \.self) { index in
                        HStack {
                            Text("\(index + 1)").foregroundStyle(.secondary)
                            TextField("Argument (empty string allowed)", text: Binding(
                                get: { options?[index] ?? "" },
                                set: { options?[index] = $0 }
                            ))
                            .font(.system(.body, design: .monospaced))
                            .textFieldStyle(.roundedBorder)
                            Button { options?.swapAt(index, index - 1) } label: {
                                Image(systemName: "arrow.up")
                            }
                            .disabled(index == 0)
                            .accessibilityLabel("Move argument up")
                            Button { options?.remove(at: index) } label: {
                                Image(systemName: "minus.circle")
                            }
                            .accessibilityLabel("Remove argument")
                        }
                    }
                    HStack {
                        Button("Add Argument") { options?.append("") }
                        Button("Use Empty List []") { options = [] }
                    }
                    if arguments.isEmpty {
                        Text("Explicit []: no extra options, even if defaults exist.")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                } else {
                    Text(inheritLabel).font(.caption).foregroundStyle(.secondary)
                }
                Text("Effective options: \(String(describing: effective))")
                    .font(.caption.monospaced())
                    .textSelection(.enabled)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        } label: {
            Text(verbatim: title).font(.headline)
        }
    }
}
