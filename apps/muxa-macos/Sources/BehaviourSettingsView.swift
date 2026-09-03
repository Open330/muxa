import AppKit
import SwiftUI

/// Settings › Behaviour: the two sections of muxa configuration a person
/// actually tunes — desktop notifications and agent-to-agent collaboration.
///
/// The form writes through the same `config_write` the Advanced tab uses,
/// rewriting only the keys below, so a malformed edit cannot come from here.
struct BehaviourSettingsPane: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: MuxaConfigStore
    @State private var settings = MuxaBehaviourSettings()
    @State private var copiedTOML = false

    private var isChanged: Bool {
        store.hasLoaded && settings != store.behaviour
    }

    var body: some View {
        Form {
            Section("Notifications") {
                Toggle("Post a desktop notification when an agent needs you", isOn: $settings.notifierEnabled)
                Picker("Delivery", selection: $settings.notifierBackend) {
                    ForEach(MuxaNotifierBackend.allCases) { backend in
                        Text(notifierBackendTitle(backend)).tag(backend)
                    }
                }
                .disabled(!settings.notifierEnabled)
                Text("muxad notifies when an agent starts waiting for a person, hits an error, or stops mid-turn. Repeats for the same agent are suppressed for 30 seconds.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                if settings.notifierEnabled, settings.notifierBackend == .none {
                    Label("Choose a delivery method, or nothing is posted.", systemImage: "exclamationmark.circle")
                        .font(.caption)
                        .foregroundStyle(.orange)
                }
            }

            Section("Collaboration") {
                Toggle("Let agents send each other requests", isOn: $settings.collaborationEnabled)
                Text("Enabling this is a grant: a request may be typed into a peer agent's pane.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Group {
                    Picker("Wake", selection: $settings.collaborationWake) {
                        ForEach(MuxaCollaborationWake.allCases) { wake in
                            Text(collaborationWakeTitle(wake)).tag(wake)
                        }
                    }
                    Text(collaborationWakeDetail(settings.collaborationWake))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Picker("Delivered payload", selection: $settings.collaborationWakePayload) {
                        ForEach(MuxaCollaborationWakePayload.allCases) { payload in
                            Text(collaborationPayloadTitle(payload)).tag(payload)
                        }
                    }
                    Text(collaborationPayloadDetail(settings.collaborationWakePayload))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Picker("Pane scope", selection: $settings.collaborationScope) {
                        ForEach(MuxaCollaborationScope.allCases) { scope in
                            Text(collaborationScopeTitle(scope)).tag(scope)
                        }
                    }
                    Text(collaborationScopeDetail(settings.collaborationScope))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .disabled(!settings.collaborationEnabled)
            }

            if store.isSupported {
                Section {
                    HStack {
                        Group {
                            if let conflict = store.conflictMessage {
                                Label(conflict, systemImage: "arrow.triangle.branch")
                                    .foregroundStyle(.orange)
                                    .textSelection(.enabled)
                            } else if let error = store.saveError {
                                Label(error, systemImage: "xmark.octagon.fill")
                                    .foregroundStyle(.red)
                                    .textSelection(.enabled)
                            } else if let error = store.loadError {
                                Label(error, systemImage: "exclamationmark.triangle.fill")
                                    .foregroundStyle(.orange)
                                    .textSelection(.enabled)
                            } else if isChanged {
                                Text("Saved changes apply when muxad restarts.")
                                    .foregroundStyle(.secondary)
                            } else if let status = store.status {
                                Label(status, systemImage: "checkmark.circle.fill")
                                    .foregroundStyle(.green)
                            } else {
                                Text("These values are read from muxa configuration.")
                                    .foregroundStyle(.secondary)
                            }
                        }
                        .font(.caption)
                        .fixedSize(horizontal: false, vertical: true)
                        Spacer(minLength: 8)
                        if store.isSaving || store.isLoading {
                            ProgressView().controlSize(.small)
                        }
                        MuxaDaemonReloadButton(model: model)
                        if !store.retryEdits.isEmpty {
                            Button("Apply Again") {
                                Task {
                                    _ = await store.retryPendingEdits(model: model)
                                    settings = store.behaviour
                                }
                            }
                            .help("Puts the same change on top of the file as it now stands.")
                            .disabled(store.isSaving)
                        }
                        Button("Revert") { settings = store.behaviour }
                            .disabled(!isChanged || store.isSaving)
                        Button("Save") {
                            Task {
                                _ = await store.apply(settings.edits(against: store.behaviour), model: model)
                                settings = store.behaviour
                            }
                        }
                        .buttonStyle(.borderedProminent)
                        .disabled(!isChanged || store.isSaving)
                    }
                    .controlSize(.small)
                }
            } else {
                Section("Not editable from here") {
                    Text("This muxad cannot write its configuration. Copy the block below into the muxa configuration file, then reload muxad.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    Text(verbatim: snippet)
                        .font(.caption.monospaced())
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(8)
                        .background(Color.primary.opacity(0.05), in: RoundedRectangle(cornerRadius: 8))
                    HStack {
                        Button {
                            NSPasteboard.general.clearContents()
                            NSPasteboard.general.setString(snippet, forType: .string)
                            copiedTOML = true
                        } label: {
                            Label(copiedTOML ? "Copied" : "Copy", systemImage: "doc.on.doc")
                        }
                        Spacer()
                        MuxaDaemonReloadButton(model: model)
                    }
                    .controlSize(.small)
                }
            }
        }
        .formStyle(.grouped)
        .padding(.top, 8)
        .task(id: model.isConnected) {
            await store.load(model: model)
            settings = store.behaviour
        }
        .onChange(of: store.loadedText) { _ in
            settings = store.behaviour
        }
        .onChange(of: settings) { _ in
            copiedTOML = false
        }
    }

    /// What the two sections would look like in the file, for the daemon
    /// that cannot be written to. Only values that differ from muxa's
    /// defaults are listed — those are the ones worth writing down.
    private var snippet: String {
        let edits = settings.edits(against: .daemonDefaults)
        guard !edits.isEmpty else {
            return "# " + String(localized: "Everything here already matches muxa's defaults.")
        }
        return MuxaTOMLPatcher.apply(edits, to: "")
            .trimmingCharacters(in: .whitespacesAndNewlines)
    }
}

// MARK: - Titles

private func notifierBackendTitle(_ backend: MuxaNotifierBackend) -> String {
    switch backend {
    case .none: String(localized: "Off")
    case .libnotify: String(localized: "System notification centre")
    }
}

private func collaborationWakeTitle(_ wake: MuxaCollaborationWake) -> String {
    switch wake {
    case .never: String(localized: "Never wake an agent")
    case .idleOnly: String(localized: "Only at an idle prompt")
    }
}

private func collaborationWakeDetail(_ wake: MuxaCollaborationWake) -> String {
    switch wake {
    case .never:
        String(localized: "Requests wait in the mailbox until the agent reads them.")
    case .idleOnly:
        String(localized: "A waiting agent is nudged at its top-level prompt, never mid-turn.")
    }
}

private func collaborationPayloadTitle(_ payload: MuxaCollaborationWakePayload) -> String {
    switch payload {
    case .notice: String(localized: "A notice only")
    case .operatorFull: String(localized: "Your requests in full")
    case .full: String(localized: "Every request in full")
    }
}

private func collaborationPayloadDetail(_ payload: MuxaCollaborationWakePayload) -> String {
    switch payload {
    case .notice:
        String(localized: "The agent is told it has mail and fetches the request itself.")
    case .operatorFull:
        String(localized: "Requests you send are delivered as text; agent-to-agent requests stay in the mailbox.")
    case .full:
        String(localized: "Every request is delivered as text, whoever sent it.")
    }
}

private func collaborationScopeTitle(_ scope: MuxaCollaborationScope) -> String {
    switch scope {
    case .window: String(localized: "This tmux window")
    case .host: String(localized: "Anywhere on this host")
    }
}

private func collaborationScopeDetail(_ scope: MuxaCollaborationScope) -> String {
    switch scope {
    case .window:
        String(localized: "A pane may only be addressed from inside its own window — sharing a window is the consent.")
    case .host:
        String(localized: "Any tracked agent pane on this host may be addressed by id. Aliases and roles stay window-scoped.")
    }
}
