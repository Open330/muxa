import AppKit
import SwiftUI

struct WorkStartView: View {
    @ObservedObject var model: AppModel
    @Binding var isPresented: Bool

    @State private var work = ""
    @State private var workspace = ""
    @State private var pipeline = ""
    @State private var external = ""
    @State private var skill = ""
    @State private var taskBody = ""
    @State private var context = ""
    @State private var dryRun = false
    @State private var host = ""
    @AppStorage("nativeWorkDirectory") private var cwd = ""
    @State private var remoteFolder = ""

    private var isLocalHost: Bool { model.isLocalHost(host) }

    private var options: MuxaWorkOptions? { model.workOptions(for: isLocalHost ? nil : host) }

    /// The folder field edits the persisted local default for the local host
    /// and a per-sheet path for a remote host, because remote paths mean
    /// nothing here and must not be remembered as the local default.
    private var folderBinding: Binding<String> {
        isLocalHost ? $cwd : $remoteFolder
    }

    private var matchedRoute: MuxaWorkOptions.Route? {
        options?.route(matching: work)
    }

    /// The pipeline the launch would use: the explicit choice, else the
    /// matching route's pipeline.
    private var effectivePipeline: MuxaWorkOptions.Pipeline? {
        guard let options else { return nil }
        if pipeline.isEmpty { return options.defaultPipeline(for: work) }
        return options.pipeline(named: pipeline)
    }

    private var localSessionNames: [String] {
        model.executionSnapshot.watchHosts
            .first(where: { isLocalHost ? $0.host.local : $0.host.alias == host })?
            .sessions
            .map(\.name)
            .filter { !$0.isEmpty } ?? []
    }

    private var workspaceSuggestions: [String] {
        var seen = Set<String>()
        var suggestions: [String] = []
        for candidate in [matchedRoute?.workspace].compactMap({ $0 }) + localSessionNames
        where seen.insert(candidate).inserted {
            suggestions.append(candidate)
        }
        return suggestions
    }

    /// Whether the launch directory is pinned by the route (cwd, worktree,
    /// or a prepare command) rather than by this form.
    private var routePinsDirectory: Bool {
        guard let route = matchedRoute else { return false }
        return route.worktree || route.prepare || (route.cwd?.isEmpty == false)
    }

    /// muxad runs `muxa work up` from its own working directory, which for a
    /// GUI-launched daemon is `/`. Never let that become an agent's project
    /// folder: when neither the form nor the route names one, use home.
    private var effectiveDirectory: String? {
        let trimmed = folderBinding.wrappedValue.trimmingCharacters(in: .whitespacesAndNewlines)
        if !trimmed.isEmpty { return trimmed }
        if routePinsDirectory { return nil }
        // A remote host's home is not known here; let the remote CLI use its
        // own cwd rules (the route, else the login directory).
        return isLocalHost ? FileManager.default.homeDirectoryForCurrentUser.path : nil
    }

    private var canSubmit: Bool {
        guard !work.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty, !model.isStartingWork else {
            return false
        }
        // With a loaded config the launch is predictable: refuse the combos
        // the CLI would refuse instead of surfacing its error afterwards.
        guard let options else { return true }
        if options.pipelines.isEmpty { return false }
        return effectivePipeline != nil
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: "play.square.stack.fill")
                    .font(.system(size: 28))
                    .foregroundStyle(.tint)
                VStack(alignment: .leading, spacing: 3) {
                    Text("Start Work")
                        .font(.system(size: 14, weight: .semibold))
                    Text("Create or converge a collaborator pipeline without leaving Muxa.")
                        .foregroundStyle(.secondary)
                }
                Spacer()
                if model.isLoadingWorkOptions {
                    ProgressView().controlSize(.small)
                }
            }
            .padding(20)

            Divider()

            Form {
                Section("Identity") {
                    hostPicker
                    TextField("Work ID, for example auth-cleanup", text: $work)
                    routeSummary
                    HStack {
                        TextField("Workspace (optional)", text: $workspace)
                        if !workspaceSuggestions.isEmpty {
                            Menu {
                                ForEach(workspaceSuggestions, id: \.self) { suggestion in
                                    Button(suggestion) { workspace = suggestion }
                                }
                            } label: {
                                Image(systemName: "chevron.up.chevron.down")
                            }
                            .menuStyle(.borderlessButton)
                            .fixedSize()
                            .help("Use the route's workspace or an existing session")
                        }
                    }
                    HStack {
                        TextField(
                            isLocalHost
                                ? "Project folder (use configured route when empty)"
                                : "Project folder on \(host) (use its route when empty)",
                            text: folderBinding
                        )
                        if isLocalHost {
                            Button("Choose…", action: chooseDirectory)
                        }
                    }
                    if folderBinding.wrappedValue.trimmingCharacters(in: .whitespaces).isEmpty, options != nil {
                        Group {
                            if routePinsDirectory {
                                Text("The route decides the folder.")
                            } else if isLocalHost {
                                Text("Defaults to your home folder because the route names none; choose the project you want the agents to work in.")
                            } else {
                                Text("The route on \(host) names no folder; type the project path on that host.")
                            }
                        }
                        .font(MuxaType.detail)
                        .foregroundStyle(routePinsDirectory ? Color.secondary : Color.orange)
                    }
                }

                if let plan = model.workStartPlan {
                    Section("Plan") {
                        WorkPlanView(result: plan) {
                            dryRun = false
                            submit()
                        }
                    }
                }

                Section("Pipeline") {
                    pipelineSection
                }

                Section("Task") {
                    TextField("External issue, for example ENG-1234 (optional)", text: $external)
                    Text("An empty external issue creates a local Muxa Work; the issue never becomes the Work identity.")
                        .font(MuxaType.detail)
                        .foregroundStyle(.secondary)
                    TextEditor(text: $taskBody)
                        .font(.body)
                        .frame(minHeight: 86)
                        .overlay(alignment: .topLeading) {
                            if taskBody.isEmpty {
                                Text("What should the collaborators accomplish?")
                                    .foregroundStyle(.secondary)
                                    .padding(.top, 7)
                                    .padding(.leading, 5)
                                    .allowsHitTesting(false)
                            }
                        }
                    DisclosureGroup("Advanced context") {
                        skillField
                        TextField("Additional context (optional)", text: $context)
                        Toggle("Plan only — do not create agents", isOn: $dryRun)
                    }
                }
            }
            .formStyle(.grouped)

            if let error = model.workStartError {
                VStack(alignment: .leading, spacing: 8) {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .font(MuxaType.detail)
                        .foregroundStyle(.red)
                        .textSelection(.enabled)
                    if model.needsWorkConfiguration {
                        HStack {
                            Text("No Work routing is configured yet. Install a preset above, or let an agent write the config in an interactive Shell tab.")
                                .font(MuxaType.detail)
                                .foregroundStyle(.secondary)
                            Spacer()
                            Button("Configure Work…", action: configureWork)
                                .buttonStyle(.muxaPrimary)
                        }
                    }
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 8)
                .frame(maxWidth: .infinity, alignment: .leading)
            } else if let status = model.workStartStatus {
                HStack(spacing: 8) {
                    if model.isStartingWork { ProgressView().controlSize(.small) }
                    Text(status)
                        .font(MuxaType.detail)
                        .foregroundStyle(.secondary)
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 8)
                .frame(maxWidth: .infinity, alignment: .leading)
            }

            Divider()

            HStack {
                Text("Runs the bundled canonical `muxa work up` implementation through owner-only muxad IPC.")
                    .font(MuxaType.meta)
                    .foregroundStyle(.secondary)
                Spacer()
                Button("Cancel") { isPresented = false }
                    .disabled(model.isStartingWork)
                Button(dryRun ? "Build Plan" : "Start Work") { submit() }
                    .buttonStyle(.muxaPrimary)
                    .disabled(!canSubmit)
                    .keyboardShortcut(.defaultAction)
            }
            .padding(16)
        }
        .frame(width: 700, height: 720)
        .onAppear {
            if let preselected = model.workStartPreselectedPipeline {
                pipeline = preselected
            }
            if let preselectedHost = model.workStartPreselectedHost, !model.isLocalHost(preselectedHost) {
                host = preselectedHost
            }
        }
        .onChange(of: host) { selected in
            pipeline = ""
            workspace = ""
            Task { await model.loadWorkOptions(host: model.isLocalHost(selected) ? nil : selected) }
        }
        .onChange(of: model.workOptions) { updated in
            // A preset installed from this sheet becomes the selection; a
            // previously chosen pipeline that vanished from the config
            // returns to the route default.
            guard let updated else { return }
            if !pipeline.isEmpty, updated.pipeline(named: pipeline) == nil {
                pipeline = ""
            }
        }
    }

    @ViewBuilder
    private var hostPicker: some View {
        let hosts = model.workCapableHosts
        if hosts.count > 1 {
            Picker("Host", selection: $host) {
                ForEach(hosts) { candidate in
                    Group {
                        if candidate.local {
                            Text("\(candidate.alias) (this Mac)")
                        } else {
                            Text(candidate.alias)
                        }
                    }
                    .tag(candidate.local ? "" : candidate.alias)
                }
            }
            if !isLocalHost, !model.supportsHostWorkCommands {
                Label(
                    "Starting Work on \(host) needs the updated muxad on this Mac (Use Bundled muxad), and muxa on \(host) must know `work options`.",
                    systemImage: "exclamationmark.triangle"
                )
                .font(MuxaType.detail)
                .foregroundStyle(.orange)
            } else if !isLocalHost {
                Text("The pipeline and route come from \(host)'s config; agents start in tmux on that host.")
                    .font(MuxaType.detail)
                    .foregroundStyle(.secondary)
            }
        }
    }

    @ViewBuilder
    private var routeSummary: some View {
        if let options, !work.trimmingCharacters(in: .whitespaces).isEmpty {
            if let route = matchedRoute {
                Label {
                    Text(routeDescription(route))
                        .font(MuxaType.detail)
                        .foregroundStyle(.secondary)
                } icon: {
                    Image(systemName: "arrow.triangle.branch")
                        .foregroundStyle(.tint)
                }
            } else if options.pipelines.isEmpty {
                EmptyView()
            } else {
                Label("No route matches this Work id; choose a pipeline below.", systemImage: "arrow.triangle.branch")
                    .font(MuxaType.detail)
                    .foregroundStyle(.orange)
            }
        }
    }

    private func routeDescription(_ route: MuxaWorkOptions.Route) -> String {
        var parts = [String(localized: "Route \(route.match)")]
        if let name = route.pipeline, !name.isEmpty {
            parts.append(String(localized: "pipeline \(name)"))
        }
        if let workspace = route.workspace, !workspace.isEmpty {
            parts.append(String(localized: "workspace \(workspace)"))
        }
        if route.worktree {
            parts.append(String(localized: "own git worktree"))
        } else if let cwd = route.cwd, !cwd.isEmpty {
            parts.append(String(localized: "cwd \(cwd)"))
        }
        return parts.joined(separator: " → ")
    }

    @ViewBuilder
    private var pipelineSection: some View {
        if let options {
            if options.pipelines.isEmpty {
                WorkPresetGallery(
                    options: options,
                    host: isLocalHost ? nil : host,
                    model: model,
                    onInstalled: { installed in pipeline = installed },
                    onDescribe: describeWithAgent
                )
            } else {
                Picker("Pipeline", selection: $pipeline) {
                    Text(defaultPipelineLabel).tag("")
                    ForEach(options.pipelines) { candidate in
                        Text(candidate.name).tag(candidate.name)
                    }
                }
                if let selected = effectivePipeline {
                    VStack(alignment: .leading, spacing: 8) {
                        if let description = selected.description, !description.isEmpty {
                            Text(description)
                                .font(MuxaType.detail)
                                .foregroundStyle(.secondary)
                        }
                        PipelineStagesView(agents: selected.agents)
                        if let layout = selected.layout, !layout.isEmpty {
                            Text("tmux layout \(layout)")
                                .font(MuxaType.meta.monospaced())
                                .foregroundStyle(.secondary)
                        }
                    }
                } else if pipeline.isEmpty {
                    Text("The route for this Work id names no pipeline. Pick one to launch.")
                        .font(MuxaType.detail)
                        .foregroundStyle(.orange)
                }
            }
        } else if let error = model.workOptionsError(for: isLocalHost ? nil : host) {
            TextField("Pipeline (use configured route when empty)", text: $pipeline)
            Label(error, systemImage: "exclamationmark.triangle.fill")
                .font(MuxaType.detail)
                .foregroundStyle(.orange)
                .textSelection(.enabled)
        } else {
            TextField("Pipeline (use configured route when empty)", text: $pipeline)
            Group {
                if isLocalHost {
                    Text("Reading pipelines from the muxa config…")
                } else {
                    Text("Reading pipelines from \(host)…")
                }
            }
            .font(MuxaType.detail)
            .foregroundStyle(.secondary)
        }
    }

    /// `muxa work init` runs in a local Shell tab, so it is offered for the
    /// local host only.
    private var describeWithAgent: (() -> Void)? {
        guard isLocalHost else { return nil }
        return { configureWork() }
    }

    private var defaultPipelineLabel: String {
        if let name = matchedRoute?.pipeline, !name.isEmpty {
            return String(localized: "Route default (\(name))")
        }
        return work.trimmingCharacters(in: .whitespaces).isEmpty
            ? String(localized: "Route default")
            : String(localized: "Route default (none)")
    }

    @ViewBuilder
    private var skillField: some View {
        if let skills = options?.skills, !skills.isEmpty {
            Picker("Message skill", selection: $skill) {
                Text("None").tag("")
                ForEach(skills) { candidate in
                    Text(verbatim: candidate.summary.map { "\(candidate.name) — \($0)" } ?? candidate.name)
                        .tag(candidate.name)
                }
            }
        } else {
            TextField("Message skill (optional)", text: $skill)
        }
    }

    private func submit() {
        let request = MuxaWorkStartRequest(
            work: work,
            workspace: workspace,
            pipeline: pipeline,
            cwd: effectiveDirectory,
            external: external,
            skill: skill,
            body: taskBody,
            context: context,
            dryRun: dryRun,
            host: isLocalHost ? nil : host
        )
        Task {
            if await model.startWork(request) {
                isPresented = false
            }
        }
    }

    private func chooseDirectory() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.directoryURL = cwd.isEmpty
            ? FileManager.default.homeDirectoryForCurrentUser
            : URL(fileURLWithPath: cwd, isDirectory: true)
        if panel.runModal() == .OK, let url = panel.url {
            cwd = url.path
            if workspace.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                workspace = url.lastPathComponent
            }
        }
    }

    private func configureWork() {
        Task {
            if await model.configureWork(cwd: cwd) {
                isPresented = false
            }
        }
    }
}

struct NativeWatchView: View {
    @ObservedObject var model: AppModel
    @Environment(\.colorScheme) private var colorScheme

    private var selectedPane: MuxaWatchPane? {
        model.watchSelection.flatMap { model.executionSnapshot.watchPane(id: $0) }
    }

    var body: some View {
        Group {
            if let selectedPane {
                FleetPaneWorkspace(pane: selectedPane, model: model)
                    .id(selectedPane.id)
            } else {
                ConsoleUnavailableView(
                    title: "No pane selected",
                    systemImage: "sidebar.left",
                    description: "Choose a session, window, or pane in Explorer. Muxa will resolve it to a live pane."
                )
            }
        }
        .background(MuxaSurfacePalette.workspace(for: colorScheme))
    }
}

private struct FleetPaneWorkspace: View {
    private enum PaneModule: CaseIterable, Identifiable {
        case overview
        case collaborate
        case changes // WS-D

        var id: Self { self }

        var title: LocalizedStringKey {
            switch self {
            case .overview: "Overview"
            case .collaborate: "Collaborate"
            case .changes: "Changes" // WS-D
            }
        }
    }

    let pane: MuxaWatchPane
    @ObservedObject var model: AppModel
    @State private var attachedSessionID: String?
    @State private var module: PaneModule = .overview
    @ObservedObject private var reviewDrafts = ReviewDraftStore.shared // WS-D: Changes tab badge

    var body: some View {
        VStack(spacing: 0) {
            paneHeader
            Divider()
            VSplitView {
                Group {
                    switch module {
                    case .overview:
                        FleetPaneInspector(
                            pane: pane,
                            model: model,
                            compact: false,
                            openInShell: { openInShell() }
                        )
                    case .collaborate:
                        MuxaCollaborationView(
                            pane: pane,
                            client: model.client,
                            mailboxRevision: model.mailboxRevisions[pane.host.alias]
                        )
                    case .changes: // WS-D
                        ChangesWorkspaceView(pane: pane, model: model)
                    }
                }
                .frame(minHeight: 180)

                WatchLivePanePanel(
                    pane: pane,
                    model: model,
                    attachedSessionID: attachedSessionID,
                    startAttach: attachInPanel,
                    stopAttach: stopPanelAttach,
                    sessionExited: panelSessionExited
                )
                .frame(minHeight: 200, idealHeight: 360)
            }
        }
        .onDisappear(perform: stopPanelAttach)
        // WS-D: ⌃⇧G Show Changes
        .onReceive(NotificationCenter.default.publisher(for: .muxaShowChanges)) { note in
            if ChangesShowRequest.matches(note.object, pane: pane, watchSelection: model.watchSelection) {
                module = .changes
            }
        }
    }

    /// A breadcrumb bar, like an editor's path above the file: the inspector
    /// below already titles the pane, so this row only locates it and
    /// switches the module.
    private var paneHeader: some View {
        HStack(spacing: 8) {
            HostIdentityBadge(identity: pane.host, size: 14)
            breadcrumbs
                .layoutPriority(-1)
            Spacer(minLength: 8)
            MuxaPaneUsageAccessory(agent: pane.agent, target: MuxaAgentActionTarget(pane)) // WS-C usage
            if let target = MuxaAgentActionTarget(pane) {
                MuxaAgentActionBar(model: model, target: target)
            }
            // WS-D: the Changes tab carries the pending review comment count.
            MuxaTextTabs(items: PaneModule.allCases, selection: $module) {
                $0 == .changes ? ChangesTab.title(for: pane, drafts: reviewDrafts) : $0.title
            }
            Button {
                Task { await model.refresh() }
            } label: {
                Label("Refresh", systemImage: "arrow.clockwise")
            }
            .buttonStyle(.muxaIcon)
            .help("Refresh")
        }
        .padding(.leading, 12)
        .padding(.trailing, 8)
        .frame(height: MuxaTheme.breadcrumbHeight)
    }

    private var breadcrumbs: some View {
        let parts = [pane.host.alias, pane.pane.session, pane.pane.windowName, pane.pane.paneID]
            .filter { !$0.isEmpty }
        return HStack(spacing: 4) {
            ForEach(Array(parts.enumerated()), id: \.offset) { index, part in
                if index > 0 {
                    Image(systemName: "chevron.right")
                        .font(.system(size: 8, weight: .semibold))
                        .foregroundStyle(.tertiary)
                }
                Text(verbatim: part)
                    .foregroundStyle(index == parts.count - 1 ? Color.primary : Color.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
        }
        .font(.system(size: 12))
    }

    private func attachInPanel() {
        guard attachedSessionID == nil, !model.isAttachingPane else { return }
        Task {
            guard let session = await model.attach(pane: pane, selectShell: false) else { return }
            attachedSessionID = session.id
        }
    }

    private func openInShell() {
        if let sessionID = attachedSessionID {
            attachedSessionID = nil
            model.select(.shell(sessionID))
            return
        }
        Task { await model.attach(pane: pane) }
    }

    private func stopPanelAttach() {
        guard let sessionID = attachedSessionID else { return }
        attachedSessionID = nil
        Task {
            try? await model.client.terminateSession(id: sessionID)
            await model.refresh()
        }
    }

    private func panelSessionExited() {
        attachedSessionID = nil
        Task { await model.refresh() }
    }
}

struct HostRegistrationView: View {
    @ObservedObject var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @State private var alias = ""
    @State private var ssh = ""
    @State private var mode = "observe"
    @State private var connect = "auto"
    @State private var muxaPath = "muxa"
    @State private var remoteSocket = ""
    @State private var overwrite = false

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                VStack(alignment: .leading, spacing: 3) {
                    Text("Register Host")
                        .font(.system(size: 14, weight: .semibold))
                    Text("Add an OpenSSH target to Muxa's central host inventory.")
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Button("Cancel") {
                    model.isPresentingHostRegistration = false
                    dismiss()
                }
                    .keyboardShortcut(.cancelAction)
                Button("Register") { register() }
                    .keyboardShortcut(.defaultAction)
                    .buttonStyle(.muxaPrimary)
                    .disabled(model.isRegisteringHost || alias.trimmingCharacters(in: .whitespaces).isEmpty || ssh.trimmingCharacters(in: .whitespaces).isEmpty)
            }
            .padding(20)

            Divider()

            Form {
                Section("Identity") {
                    TextField("Alias", text: $alias, prompt: Text(verbatim: "build-mac"))
                    TextField("SSH target", text: $ssh, prompt: Text("user@host or ~/.ssh/config alias"))
                    Picker("Access", selection: $mode) {
                        Text("Observe only").tag("observe")
                        Text("Control").tag("control")
                    }
                    Picker("Connect", selection: $connect) {
                        Text("Automatically").tag("auto")
                        Text("On demand").tag("on-demand")
                    }
                }

                Section("Remote runtime") {
                    TextField("muxa executable", text: $muxaPath)
                    TextField("Remote socket (optional)", text: $remoteSocket)
                }

                Section {
                    Toggle("Replace an existing host with this alias", isOn: $overwrite)
                    Text("Observe is the safe default. Control permits prompts, attach, and collaboration operations on the remote host.")
                        .font(MuxaType.detail)
                        .foregroundStyle(.secondary)
                }

                if let error = model.hostRegistrationError {
                    Section("Registration failed") {
                        Text(error)
                            .foregroundStyle(.red)
                            .textSelection(.enabled)
                    }
                }
            }
            .formStyle(.grouped)

            if model.isRegisteringHost {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Saving inventory and reloading muxad…")
                        .font(MuxaType.detail)
                        .foregroundStyle(.secondary)
                }
                .padding(.bottom, 14)
            }
        }
        .frame(width: 560, height: 540)
    }

    private func register() {
        Task {
            let registered = await model.registerHost(
                MuxaHostRegistrationRequest(
                    alias: alias,
                    ssh: ssh,
                    mode: mode,
                    connect: connect,
                    muxaPath: muxaPath,
                    remoteSocket: remoteSocket,
                    overwrite: overwrite
                )
            )
            if registered { dismiss() }
        }
    }
}

struct FleetPaneModuleView: View {
    let pane: MuxaWatchPane
    @ObservedObject var model: AppModel

    var body: some View {
        FleetPaneWorkspace(pane: pane, model: model)
    }
}

struct PromptFeedback: Equatable {
    let message: String
    let succeeded: Bool
}

enum PromptComposerRules {
    static func canSend(prompt: String, sending: Bool, hosts: [MuxaFleetHostIdentity]) -> Bool {
        !prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && !sending && !hosts.isEmpty
            && hosts.allSatisfy { $0.local || $0.mode == "control" }
    }

    static func shouldClearDraft(current: String, submitted: String, revision: UUID, submittedRevision: UUID) -> Bool {
        current == submitted && revision == submittedRevision
    }

    static func handlesSendKey(keyCode: UInt16, modifiers: NSEvent.ModifierFlags, focused: Bool, markedText: Bool) -> Bool {
        focused && !markedText && keyCode == 36
            && modifiers.intersection(.deviceIndependentFlagsMask).subtracting([.capsLock, .numericPad]) == .command
    }
}

struct PromptComposerStatus: View {
    let feedback: PromptFeedback?
    @State private var showsDetails = false

    var body: some View {
        Color.clear
            .frame(width: 132, height: 24)
            .overlay(alignment: .trailing) {
                if let feedback {
                    Button {
                        showsDetails.toggle()
                    } label: {
                        Label(feedback.message, systemImage: feedback.succeeded ? "checkmark.circle" : "exclamationmark.circle")
                            .font(MuxaType.meta)
                            .lineLimit(1)
                            .truncationMode(.tail)
                            .foregroundStyle(feedback.succeeded ? Color.green : Color.red)
                            .frame(maxWidth: .infinity, alignment: .trailing)
                    }
                    .buttonStyle(.plain)
                    .help(Text(verbatim: feedback.message))
                    .accessibilityLabel(Text(verbatim: feedback.message))
                    .accessibilityHint("Show prompt status details")
                    .popover(isPresented: $showsDetails) {
                        ScrollView {
                            Text(verbatim: feedback.message)
                                .font(.callout)
                                .textSelection(.enabled)
                                .frame(maxWidth: .infinity, alignment: .leading)
                                .padding(12)
                        }
                        .frame(width: 360, height: 180)
                    }
                }
            }
            .onChange(of: feedback) { _ in showsDetails = false }
    }
}

private struct PromptComposerSendKey: NSViewRepresentable {
    let focused: Bool
    let send: () -> Void

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        context.coordinator.update(view: view, focused: focused, send: send)
        context.coordinator.monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak coordinator = context.coordinator] event in
            guard let coordinator,
                  let window = coordinator.view?.window,
                  window.isKeyWindow, event.window === window,
                  let editor = window.firstResponder as? NSTextView,
                  PromptComposerRules.handlesSendKey(
                    keyCode: event.keyCode, modifiers: event.modifierFlags,
                    focused: coordinator.focused, markedText: editor.hasMarkedText()
                  ) else { return event }
            coordinator.send()
            return nil
        }
        return view
    }

    func updateNSView(_ view: NSView, context: Context) {
        context.coordinator.update(view: view, focused: focused, send: send)
    }

    static func dismantleNSView(_ view: NSView, coordinator: Coordinator) {
        if let monitor = coordinator.monitor { NSEvent.removeMonitor(monitor) }
        coordinator.monitor = nil
    }

    final class Coordinator {
        weak var view: NSView?
        var focused = false
        var send: () -> Void = {}
        var monitor: Any?

        func update(view: NSView, focused: Bool, send: @escaping () -> Void) {
            self.view = view
            self.focused = focused
            self.send = send
        }
    }
}

struct PanePromptComposer: View {
    let host: MuxaFleetHostIdentity
    let pane: MuxaPaneInfo
    let client: MuxaIPCClient
    @Binding var prompt: String
    @Binding var sending: Bool
    @Binding var feedback: PromptFeedback?
    @State private var draftRevision = UUID()
    @FocusState private var promptFocused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            ViewThatFits(in: .horizontal) {
                HStack(spacing: 8) {
                    promptField
                    sendControls.fixedSize()
                }
                VStack(alignment: .trailing, spacing: 8) {
                    promptField
                    sendControls
                }
            }
            if !host.local && host.mode != "control" {
                Text("This host is registered in observe mode. Change it to control to send prompts.")
                    .font(MuxaType.meta)
                    .foregroundStyle(.orange)
            }
        }
        .background(PromptComposerSendKey(focused: promptFocused, send: send))
    }

    private var promptField: some View {
        TextField("Send a prompt to this agent/pane", text: Binding(
            get: { prompt },
            set: { prompt = $0; draftRevision = UUID() }
        ), axis: .vertical)
            .lineLimit(1...4)
            .focused($promptFocused)
            .muxaFieldChrome(focused: promptFocused)
            .onSubmit(send)
    }

    private var sendControls: some View {
        HStack(spacing: 8) {
            PromptComposerStatus(feedback: feedback)
            sendButton
        }
    }

    private var sendButton: some View {
        Button("Send", action: send)
            .buttonStyle(.muxaPrimary)
            .disabled(!PromptComposerRules.canSend(prompt: prompt, sending: sending, hosts: [host]))
    }

    private func send() {
        guard PromptComposerRules.canSend(prompt: prompt, sending: sending, hosts: [host]) else { return }
        let submitted = prompt
        let submittedRevision = draftRevision
        let text = submitted.trimmingCharacters(in: .whitespacesAndNewlines)
        sending = true
        feedback = nil
        Task {
            defer { sending = false }
            do {
                try await client.sendFleetPrompt(host: host, pane: pane, text: text)
                if PromptComposerRules.shouldClearDraft(current: prompt, submitted: submitted, revision: draftRevision, submittedRevision: submittedRevision) {
                    prompt = ""
                }
                feedback = PromptFeedback(message: String(localized: "Sent and submitted"), succeeded: true)
            } catch {
                feedback = PromptFeedback(message: error.localizedDescription, succeeded: false)
            }
        }
    }
}

struct WorkPromptComposer: View {
    let work: MuxaWorkGroup
    @ObservedObject var model: AppModel
    @State private var prompt = ""
    @State private var sending = false
    @State private var feedback: PromptFeedback?
    @State private var draftRevision = UUID()
    @FocusState private var promptFocused: Bool

    private var promptHosts: [MuxaFleetHostIdentity] {
        work.participants.filter { $0.pane != nil }.map(\.host)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            Text("Steer this Work")
                .font(.headline)
            ViewThatFits(in: .horizontal) {
                HStack(spacing: 8) {
                    workPromptField
                    sendControls.fixedSize()
                }
                VStack(alignment: .trailing, spacing: 8) {
                    workPromptField
                    sendControls
                }
            }
            if promptHosts.contains(where: { !$0.local && $0.mode != "control" }) {
                Text("This host is registered in observe mode. Change it to control to send prompts.")
                    .font(MuxaType.meta)
                    .foregroundStyle(.orange)
            }
        }
        .background(PromptComposerSendKey(focused: promptFocused, send: send))
        .padding(14)
        .background(MuxaTheme.panelFill, in: RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous))
    }

    private var workPromptField: some View {
        TextField("Send the next instruction to every live collaborator", text: Binding(
            get: { prompt },
            set: { prompt = $0; draftRevision = UUID() }
        ), axis: .vertical)
            .lineLimit(1...4)
            .focused($promptFocused)
            .muxaFieldChrome(focused: promptFocused)
            .onSubmit(send)
    }

    private var sendControls: some View {
        HStack(spacing: 8) {
            PromptComposerStatus(feedback: feedback)
            workSendButton
        }
    }

    private var workSendButton: some View {
        Button("Send to \(work.participants.count)", action: send)
            .buttonStyle(.muxaPrimary)
            .disabled(!PromptComposerRules.canSend(prompt: prompt, sending: sending, hosts: promptHosts))
    }

    private func send() {
        guard PromptComposerRules.canSend(prompt: prompt, sending: sending, hosts: promptHosts) else { return }
        let submitted = prompt
        let submittedRevision = draftRevision
        let text = submitted.trimmingCharacters(in: .whitespacesAndNewlines)
        sending = true
        feedback = nil
        Task {
            defer { sending = false }
            do {
                let count = try await model.prompt(work: work, text: text)
                if PromptComposerRules.shouldClearDraft(current: prompt, submitted: submitted, revision: draftRevision, submittedRevision: submittedRevision) {
                    prompt = ""
                }
                feedback = PromptFeedback(
                    message: String(localized: "Sent to \(count) collaborators"),
                    succeeded: true
                )
            } catch {
                feedback = PromptFeedback(message: error.localizedDescription, succeeded: false)
            }
        }
    }
}

struct HostIdentityBadge: View {
    private let alias: String
    private let local: Bool
    private let state: String
    private let size: CGFloat

    init(host: MuxaFleetHost, size: CGFloat = 26) {
        alias = host.alias
        local = host.local
        state = host.state
        self.size = size
    }

    init(identity: MuxaFleetHostIdentity, size: CGFloat = 26) {
        alias = identity.alias
        local = identity.local
        state = identity.state
        self.size = size
    }

    var body: some View {
        ZStack {
            RoundedRectangle(cornerRadius: size * 0.25, style: .continuous)
                .fill(identityColor.opacity(0.14))
            Image(systemName: local ? "laptopcomputer" : "server.rack")
                .font(.system(size: size * 0.46, weight: .medium))
                .foregroundStyle(identityColor)
        }
        .frame(width: size, height: size)
        .overlay(alignment: .bottomTrailing) {
            Circle()
                .fill(fleetHostColor(state))
                .frame(width: max(6, size * 0.24), height: max(6, size * 0.24))
                .overlay(Circle().stroke(Color(nsColor: .windowBackgroundColor), lineWidth: 1.5))
                .offset(x: 2, y: 2)
        }
        .help(Text(verbatim: "\(alias) · \(fleetHostStateLabel(state))"))
    }

    /// A stable per-host accent makes a host recognizable even when every
    /// machine is currently in the same fleet state. The status dot remains
    /// reserved for live health/attention state.
    private var identityColor: Color {
        let palette: [Color] = [.blue, .indigo, .purple, .pink, .orange, .teal, .mint]
        let fingerprint = alias.utf8.reduce(UInt(0)) { ($0 &* 31) &+ UInt($1) }
        return palette[Int(fingerprint % UInt(palette.count))]
    }
}

struct DetachedModuleView: View {
    let route: MuxaModuleRoute
    @ObservedObject var model: AppModel

    var body: some View {
        switch route {
        case .shell(let id):
            if let session = model.sessions.first(where: { $0.id == id }) {
                TerminalPane(
                    client: model.client,
                    sessionID: session.id,
                    replayInitialHistory: true
                )
            } else {
                moduleMissing("Native shell is no longer available")
            }
        case .fleetPane(let id):
            if let pane = model.executionSnapshot.watchPane(id: id) {
                FleetPaneModuleView(pane: pane, model: model)
            } else {
                moduleMissing("This pane is no longer available")
            }
        }
    }

    private func moduleMissing(_ text: LocalizedStringKey) -> some View {
        ConsoleUnavailableView(
            title: "Module unavailable",
            systemImage: "terminal.fill",
            description: text
        )
        .frame(minWidth: 680, minHeight: 480)
    }
}

struct ConsoleUnavailableView: View {
    let title: LocalizedStringKey
    let systemImage: String
    let description: LocalizedStringKey

    var body: some View {
        VStack(spacing: 9) {
            Image(systemName: systemImage)
                .font(.system(size: 32))
                .foregroundStyle(.secondary)
            Text(title).font(.headline)
            Text(description)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        .padding(24)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

private extension String {
    var nonEmpty: String? { isEmpty ? nil : self }
}
