import AppKit
import SwiftUI

/// The `[notifier]` and `[collaboration]` values on screen in Settings ›
/// Notifications and Settings › Agents, applied as they change.
///
/// Both tabs edit the one draft, and every change writes through the same
/// `config_write` the raw editor uses, rewriting only the keys that differ
/// from the file, so a malformed edit cannot come from here. Writes run one
/// after another: a second change made while the first is saving is diffed
/// against the file the first one left.
@MainActor
final class TendBehaviourDraft: ObservableObject {
    static let shared = TendBehaviourDraft()

    @Published var settings = TendBehaviourSettings() {
        didSet {
            guard !isSyncing, settings != oldValue else { return }
            // A daemon that cannot write its configuration gets nothing
            // written: the draft stays as edited, for the block to copy.
            guard store.isSupported else { return }
            scheduleApply()
        }
    }
    @Published private(set) var pendingWrites = 0

    private let store: TendConfigStore
    private weak var model: AppModel?
    private var isSyncing = false
    private var lastWrite: Task<Void, Never>?

    init(store: TendConfigStore = .shared) {
        self.store = store
    }

    var isApplying: Bool { pendingWrites > 0 }

    /// Whether the draft holds a value the file does not, once every write
    /// has finished — a refused write leaves this true.
    var hasUnappliedChanges: Bool {
        !isApplying && store.hasLoaded && settings != store.behaviour
    }

    func attach(_ model: AppModel) {
        self.model = model
    }

    /// Takes the file's values, unless a write of the draft is still on its
    /// way — then the write's own completion takes them. With no file read
    /// (a daemon that cannot edit its configuration) there is nothing to
    /// take, and the edited draft is what the copy block shows.
    func syncFromStore() {
        guard pendingWrites == 0, store.hasLoaded else { return }
        replace(with: store.behaviour)
    }

    func revert() {
        replace(with: store.behaviour)
    }

    /// Puts the edits a conflict interrupted on top of the file as it is now.
    func retry() {
        guard let model else { return }
        pendingWrites += 1
        let previous = lastWrite
        lastWrite = Task { @MainActor in
            await previous?.value
            await store.retryPendingEdits(model: model)
            finishWrite()
        }
    }

    private func replace(with value: TendBehaviourSettings) {
        isSyncing = true
        settings = value
        isSyncing = false
    }

    private func scheduleApply() {
        guard let model else { return }
        pendingWrites += 1
        let previous = lastWrite
        lastWrite = Task { @MainActor in
            await previous?.value
            let edits = settings.edits(against: store.behaviour)
            if !edits.isEmpty {
                await store.apply(edits, model: model)
            }
            finishWrite()
        }
    }

    private func finishWrite() {
        pendingWrites -= 1
        // A write that landed leaves the file equal to the draft; one that
        // was refused leaves the draft as typed, with the reason beside it.
        if pendingWrites == 0, store.isSupported, store.saveError == nil, store.conflictMessage == nil {
            replace(with: store.behaviour)
        }
    }
}

// MARK: - Notifications

/// Settings › Notifications: the Mac app's own notifications first, then
/// tendd's, which only matter while Tend is not running.
struct TendNotificationsSettingsPane: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: TendConfigStore
    @ObservedObject private var draft = TendBehaviourDraft.shared
    @State private var showsDaemonNotifier = false

    var body: some View {
        Form {
            TendAppNotificationsSection(
                attention: model.attention,
                // The draft's own values, so this warning and the delivery
                // warning below never describe two different configurations.
                daemonNotifierEnabled: draft.settings.daemonPostsNotifications
            )

            Section {
                DisclosureGroup(isExpanded: $showsDaemonNotifier) {
                    Toggle("Post a desktop notification when an agent needs you", isOn: $draft.settings.notifierEnabled)
                    Picker("Delivery", selection: $draft.settings.notifierBackend) {
                        ForEach(TendNotifierBackend.allCases) { backend in
                            Text(notifierBackendTitle(backend)).tag(backend)
                        }
                    }
                    .disabled(!draft.settings.notifierEnabled)
                    if draft.settings.notifierEnabled, draft.settings.notifierBackend == .none {
                        Label("Choose a delivery method, or nothing is posted.", systemImage: "exclamationmark.circle")
                            .font(TendType.detail)
                            .foregroundStyle(.orange)
                    }
                } label: {
                    HStack(spacing: 6) {
                        Text("Also notify from tendd when Tend isn't running")
                        TendSettingsInfoButton(title: "Notifications from tendd") {
                            Text("tendd notifies when an agent starts waiting for a person, hits an error, or stops mid-turn. Repeats for the same agent are suppressed for 30 seconds.")
                        }
                    }
                }
                .disabled(!store.isSupported && store.hasLoaded)
            }

            TendBehaviourStatusSection(model: model, store: store, draft: draft)
        }
        .formStyle(.grouped)
        .tendBehaviourLoading(model: model, store: store, draft: draft)
        .onAppear {
            if draft.settings.daemonPostsNotifications { showsDaemonNotifier = true }
        }
    }
}

// MARK: - Agents

/// Settings › Agents: how agents reach each other, and the arguments every
/// agent is launched with.
struct TendAgentsSettingsPane: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: TendConfigStore
    @ObservedObject private var draft = TendBehaviourDraft.shared

    var body: some View {
        Form {
            Section("Collaboration") {
                Toggle("Let agents send each other requests", isOn: $draft.settings.collaborationEnabled)
                Group {
                    Picker("Wake", selection: $draft.settings.collaborationWake) {
                        ForEach(TendCollaborationWake.allCases) { wake in
                            Text(collaborationWakeTitle(wake)).tag(wake)
                        }
                    }
                    Picker("Delivered payload", selection: $draft.settings.collaborationWakePayload) {
                        ForEach(TendCollaborationWakePayload.allCases) { payload in
                            Text(collaborationPayloadTitle(payload)).tag(payload)
                        }
                    }
                    Picker("Pane scope", selection: $draft.settings.collaborationScope) {
                        ForEach(TendCollaborationScope.allCases) { scope in
                            Text(collaborationScopeTitle(scope)).tag(scope)
                        }
                    }
                    TendSettingsCaption(verbatim: [
                        collaborationWakeDetail(draft.settings.collaborationWake),
                        collaborationPayloadDetail(draft.settings.collaborationWakePayload),
                        collaborationScopeDetail(draft.settings.collaborationScope),
                    ].joined(separator: " "))
                }
                .disabled(!draft.settings.collaborationEnabled)
            }
            .disabled(!store.isSupported && store.hasLoaded)

            TendBehaviourStatusSection(model: model, store: store, draft: draft)

            TendLaunchArgumentsSections(model: model, store: store)
        }
        .formStyle(.grouped)
        .tendBehaviourLoading(model: model, store: store, draft: draft)
        .safeAreaInset(edge: .bottom, spacing: 0) {
            if store.isLaunchDirty {
                TendSettingsFooter(
                    isBusy: store.isSaving || store.isLoading,
                    canRevert: store.isLaunchDirty,
                    canSave: store.isLaunchDirty && !store.isDirty && !store.launchNeedsReload,
                    revert: { store.discardLaunch() },
                    save: { Task { await store.saveLaunch(model: model) } }
                ) {
                    Text("Launch arguments have unsaved changes.")
                        .foregroundStyle(.secondary)
                }
            }
        }
    }
}

// MARK: - Shared by both tabs

private extension View {
    func tendBehaviourLoading(model: AppModel, store: TendConfigStore, draft: TendBehaviourDraft) -> some View {
        task(id: model.isConnected) {
            draft.attach(model)
            await store.load(model: model)
            draft.syncFromStore()
        }
        .onChange(of: store.loadedText) { _ in draft.syncFromStore() }
    }
}

/// What happened to the last change: a refusal, a conflict, or — for a
/// daemon that cannot write its configuration — the block to paste.
private struct TendBehaviourStatusSection: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: TendConfigStore
    @ObservedObject var draft: TendBehaviourDraft
    @State private var copiedTOML = false

    var body: some View {
        if store.hasLoaded || store.loadError != nil || !store.isSupported {
            if store.isSupported {
                if let message = problem {
                    Section {
                        HStack(spacing: 8) {
                            message
                                .font(TendType.detail)
                                .textSelection(.enabled)
                                .fixedSize(horizontal: false, vertical: true)
                            Spacer(minLength: 8)
                            if !store.retryEdits.isEmpty {
                                Button("Apply Again") { draft.retry() }
                                    .help("Puts the same change on top of the file as it now stands.")
                                    .disabled(store.isSaving)
                            }
                            if draft.hasUnappliedChanges {
                                Button("Revert") { draft.revert() }
                                    .disabled(store.isSaving)
                            }
                        }
                        .controlSize(.small)
                    }
                }
            } else if !store.isLoading {
                Section("Not editable from here") {
                    TendSettingsCaption("This tendd cannot write its configuration. Copy the block below into the tend configuration file, then restart tendd from Advanced.")
                    Text(verbatim: snippet)
                        .font(TendType.detail.monospaced())
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(8)
                        .background(Color.primary.opacity(0.05), in: RoundedRectangle(cornerRadius: TendTheme.panelRadius))
                    Button {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(snippet, forType: .string)
                        copiedTOML = true
                    } label: {
                        Label(copiedTOML ? "Copied" : "Copy", systemImage: "doc.on.doc")
                    }
                    .controlSize(.small)
                }
                .onChange(of: draft.settings) { _ in copiedTOML = false }
            }
        }
    }

    private var problem: Label<Text, Image>? {
        if let conflict = store.conflictMessage {
            return Label { Text(verbatim: conflict).foregroundColor(.orange) } icon: {
                Image(systemName: "arrow.triangle.branch")
            }
        }
        if let error = store.saveError {
            return Label { Text(verbatim: error).foregroundColor(.red) } icon: {
                Image(systemName: "xmark.octagon.fill")
            }
        }
        if let error = store.loadError {
            return Label { Text(verbatim: error).foregroundColor(.orange) } icon: {
                Image(systemName: "exclamationmark.triangle.fill")
            }
        }
        return nil
    }

    /// What the two sections would look like in the file, for the daemon
    /// that cannot be written to. Only values that differ from tend's
    /// defaults are listed — those are the ones worth writing down.
    private var snippet: String {
        let edits = draft.settings.edits(against: .daemonDefaults)
        guard !edits.isEmpty else {
            return "# " + String(localized: "Everything here already matches tend's defaults.")
        }
        return TendTOMLPatcher.apply(edits, to: "")
            .trimmingCharacters(in: .whitespacesAndNewlines)
    }
}

// MARK: - Titles

private func notifierBackendTitle(_ backend: TendNotifierBackend) -> String {
    switch backend {
    case .none: String(localized: "Off")
    case .libnotify: String(localized: "System notification centre")
    }
}

private func collaborationWakeTitle(_ wake: TendCollaborationWake) -> String {
    switch wake {
    case .never: String(localized: "Never wake an agent")
    case .idleOnly: String(localized: "Only at an idle prompt")
    }
}

private func collaborationWakeDetail(_ wake: TendCollaborationWake) -> String {
    switch wake {
    case .never:
        String(localized: "Requests wait in the mailbox until the agent reads them.")
    case .idleOnly:
        String(localized: "A waiting agent is nudged at its top-level prompt, never mid-turn.")
    }
}

private func collaborationPayloadTitle(_ payload: TendCollaborationWakePayload) -> String {
    switch payload {
    case .notice: String(localized: "A notice only")
    case .operatorFull: String(localized: "Your requests in full")
    case .adaptive: String(localized: "Short worker requests inline")
    case .full: String(localized: "Every request in full")
    }
}

private func collaborationPayloadDetail(_ payload: TendCollaborationWakePayload) -> String {
    switch payload {
    case .notice:
        String(localized: "The agent is told it has mail and fetches the request itself.")
    case .operatorFull:
        String(localized: "Requests you send are delivered as text; agent-to-agent requests stay in the mailbox.")
    case .adaptive:
        String(localized: "Small read-only worker requests arrive in one turn; other agent requests stay in the mailbox.")
    case .full:
        String(localized: "Every request is delivered as text, whoever sent it.")
    }
}

private func collaborationScopeTitle(_ scope: TendCollaborationScope) -> String {
    switch scope {
    case .window: String(localized: "This tmux window")
    case .host: String(localized: "Anywhere on this host")
    }
}

private func collaborationScopeDetail(_ scope: TendCollaborationScope) -> String {
    switch scope {
    case .window:
        String(localized: "A pane may only be addressed from inside its own window — sharing a window is the consent.")
    case .host:
        String(localized: "Any tracked agent pane on this host may be addressed by id. Aliases and roles stay window-scoped.")
    }
}
