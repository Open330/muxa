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

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            if store.isSupported {
                editor
            } else {
                unsupported
            }
        }
        .task(id: model.isConnected) {
            await store.load(model: model)
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
            TextEditor(text: $store.draft)
                .font(.system(size: 12, design: .monospaced))
                .disableAutocorrection(true)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .overlay(alignment: .center) {
                    if !store.hasLoaded, store.isLoading {
                        ProgressView()
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
                        Text("The editor now holds your version and the file's latest text is the baseline. Save again to apply yours on top, or Reload to take the file's.")
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
                        Task { await store.load(model: model, force: true) }
                    }
                    .disabled(store.isLoading || store.isSaving)
                    Button("Save") {
                        Task { await store.save(model: model) }
                    }
                    .buttonStyle(.borderedProminent)
                    .disabled(!store.isDirty || store.isSaving)
                }
                .controlSize(.small)
            }
            .padding(16)
        }
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
