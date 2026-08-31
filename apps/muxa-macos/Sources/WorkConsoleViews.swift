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
    @AppStorage("nativeWorkDirectory") private var cwd = ""

    var body: some View {
        VStack(spacing: 0) {
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: "play.square.stack.fill")
                    .font(.system(size: 28))
                    .foregroundStyle(.tint)
                VStack(alignment: .leading, spacing: 3) {
                    Text("Start Work")
                        .font(.title2.weight(.semibold))
                    Text("Create or converge the configured collaborator pipeline without leaving Muxa.")
                        .foregroundStyle(.secondary)
                }
                Spacer()
            }
            .padding(20)

            Divider()

            Form {
                Section("Identity") {
                    TextField("Work ID, for example auth-cleanup", text: $work)
                    TextField("Workspace (optional)", text: $workspace)
                    HStack {
                        TextField("Project folder (use configured route when empty)", text: $cwd)
                        Button("Choose…", action: chooseDirectory)
                    }
                }

                Section("Team") {
                    TextField("Pipeline (use configured route when empty)", text: $pipeline)
                    TextField("External issue, for example CAL-1234 (optional)", text: $external)
                    Text("An empty external issue creates a local Muxa Work; the issue never becomes the Work identity.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }

                Section("Initial task") {
                    TextEditor(text: $taskBody)
                        .font(.body)
                        .frame(minHeight: 86)
                        .overlay(alignment: .topLeading) {
                            if taskBody.isEmpty {
                                Text("What should the collaborators accomplish?")
                                    .foregroundStyle(.tertiary)
                                    .padding(.top, 7)
                                    .padding(.leading, 5)
                                    .allowsHitTesting(false)
                            }
                        }
                    DisclosureGroup("Advanced context") {
                        TextField("Message skill (optional)", text: $skill)
                        TextField("Additional context (optional)", text: $context)
                        Toggle("Plan only — do not create agents", isOn: $dryRun)
                    }
                }
            }
            .formStyle(.grouped)

            if let error = model.workStartError {
                VStack(alignment: .leading, spacing: 8) {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .font(.caption)
                        .foregroundStyle(.red)
                        .textSelection(.enabled)
                    if model.needsWorkConfiguration {
                        HStack {
                            Text("No Work routing is configured yet. Muxa can guide you through it in an interactive Shell tab.")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            Spacer()
                            Button("Configure Work…", action: configureWork)
                                .buttonStyle(.borderedProminent)
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
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .padding(.horizontal, 20)
                .padding(.bottom, 8)
                .frame(maxWidth: .infinity, alignment: .leading)
            }

            Divider()

            HStack {
                Text("Runs the bundled canonical `muxa work up` implementation through owner-only muxad IPC.")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
                Spacer()
                Button("Cancel") { isPresented = false }
                    .disabled(model.isStartingWork)
                Button(dryRun ? "Build Plan" : "Start Work") { submit() }
                    .buttonStyle(.borderedProminent)
                    .disabled(work.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || model.isStartingWork)
                    .keyboardShortcut(.defaultAction)
            }
            .padding(16)
        }
        .frame(width: 650, height: 650)
    }

    private func submit() {
        let request = MuxaWorkStartRequest(
            work: work,
            workspace: workspace,
            pipeline: pipeline,
            cwd: cwd,
            external: external,
            skill: skill,
            body: taskBody,
            context: context,
            dryRun: dryRun
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

struct WorkCommandCenterView: View {
    @ObservedObject var model: AppModel
    @Environment(\.colorScheme) private var colorScheme

    private let columns = [GridItem(.adaptive(minimum: 260, maximum: 420), spacing: 12)]
    private let metricColumns = [GridItem(.adaptive(minimum: 120), spacing: 12)]

    private var attentionCount: Int {
        model.workGroups.lazy.filter { $0.attentionCount > 0 }.count
    }

    private var workingCount: Int {
        model.hostedAgents.lazy.filter { ["working", "starting"].contains($0.agent.state) }.count
    }

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 24) {
                ViewThatFits(in: .horizontal) {
                    HStack(alignment: .top, spacing: 18) {
                        commandCenterTitle
                            .frame(minWidth: 360, alignment: .leading)
                        Spacer(minLength: 0)
                        commandCenterActions
                    }
                    VStack(alignment: .leading, spacing: 14) {
                        commandCenterTitle
                        commandCenterActions
                    }
                }

                LazyVGrid(columns: metricColumns, alignment: .leading, spacing: 12) {
                    CommandCenterMetric(title: "Managed Work", value: model.workGroups.count, color: .accentColor)
                    CommandCenterMetric(title: "Working Agents", value: workingCount, color: .blue)
                    CommandCenterMetric(title: "Needs Attention", value: attentionCount, color: .orange)
                    CommandCenterMetric(title: "Fleet Hosts", value: model.fleetHosts.count, color: .mint)
                }

                VStack(alignment: .leading, spacing: 10) {
                    Text("Fleet scope")
                        .font(.title2.weight(.semibold))
                    ScrollView(.horizontal, showsIndicators: false) {
                        HStack(spacing: 10) {
                            ForEach(model.fleetHosts) { host in
                                Button {
                                    model.select(.host(host.id))
                                } label: {
                                    HStack(spacing: 9) {
                                        HostIdentityBadge(host: host, size: 30)
                                        VStack(alignment: .leading, spacing: 1) {
                                            Text(host.alias).fontWeight(.medium)
                                            Text("\(host.remote?.agents.filter { $0.state != "stopped" }.count ?? 0) agents · \(host.state)")
                                                .font(.caption2)
                                                .foregroundStyle(.secondary)
                                        }
                                    }
                                    .padding(.horizontal, 11)
                                    .padding(.vertical, 8)
                                    .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 10))
                                }
                                .buttonStyle(.plain)
                            }
                        }
                    }
                }

                VStack(alignment: .leading, spacing: 10) {
                    Text("Active Work")
                        .font(.title2.weight(.semibold))
                    if model.workGroups.isEmpty {
                        VStack(spacing: 12) {
                            Image(systemName: "square.stack.3d.up.slash")
                                .font(.system(size: 34))
                                .foregroundStyle(.secondary)
                            Text("No managed Work yet")
                                .font(.headline)
                            Text("Start a configured pipeline here. Muxa will keep Work identity separate from its tmux window and agents.")
                                .foregroundStyle(.secondary)
                                .multilineTextAlignment(.center)
                            Button("Start your first Work") { model.presentWorkStart() }
                                .buttonStyle(.borderedProminent)
                        }
                        .padding(32)
                        .frame(maxWidth: .infinity)
                        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))
                    } else {
                        LazyVGrid(columns: columns, alignment: .leading, spacing: 12) {
                            ForEach(model.workGroups) { work in
                                WorkCommandCard(work: work) {
                                    model.select(.work(work.identity))
                                }
                            }
                        }
                    }
                }
            }
            .padding(28)
            .frame(maxWidth: 1250, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .top)
        }
        .background(MuxaSurfacePalette.workspace(for: colorScheme).ignoresSafeArea())
    }

    private var commandCenterTitle: some View {
        VStack(alignment: .leading, spacing: 5) {
            Text("Work Command Center")
                .font(.largeTitle.weight(.semibold))
                .fixedSize(horizontal: false, vertical: true)
            Text("Start outcomes, coordinate collaborators, and inspect their execution without returning to tmux.")
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var commandCenterActions: some View {
        HStack(spacing: 10) {
            Button {
                model.select(.watch)
            } label: {
                Label("Live Watch", systemImage: "waveform.path.ecg.rectangle")
            }
            .buttonStyle(.bordered)
            Button {
                model.presentWorkStart()
            } label: {
                Label("Start Work", systemImage: "play.fill")
            }
            .buttonStyle(.borderedProminent)
            .disabled(!model.isConnected || model.isStartingWork)
        }
    }
}

private struct CommandCenterMetric: View {
    let title: String
    let value: Int
    let color: Color

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("\(value)")
                .font(.title.weight(.semibold).monospacedDigit())
                .foregroundStyle(color)
            Text(title)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
    }
}

private struct WorkCommandCard: View {
    let work: MuxaWorkGroup
    let open: () -> Void

    var body: some View {
        Button(action: open) {
            VStack(alignment: .leading, spacing: 12) {
                HStack {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(work.workspaceID.uppercased())
                            .font(.caption2.weight(.semibold))
                            .foregroundStyle(.secondary)
                        Text(work.title)
                            .font(.headline)
                    }
                    Spacer()
                    Label(
                        work.attentionCount > 0 ? "Attention" : work.workingCount > 0 ? "Running" : "Ready",
                        systemImage: "circle.fill"
                    )
                    .font(.caption.weight(.medium))
                    .foregroundStyle(work.attentionCount > 0 ? .orange : work.workingCount > 0 ? .blue : .green)
                }
                Text(work.pipelineRun.pipeline)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                HStack(spacing: 14) {
                    Label("\(work.participants.count)", systemImage: "person.2")
                    Label("\(work.completedCount)/\(work.totalCount)", systemImage: "checkmark.circle")
                    if !work.hostAliases.isEmpty {
                        Label(work.hostAliases.joined(separator: ", "), systemImage: "network")
                    }
                }
                .font(.caption)
                .foregroundStyle(.secondary)
            }
            .padding(15)
            .frame(maxWidth: .infinity, minHeight: 140, alignment: .topLeading)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
            .overlay {
                RoundedRectangle(cornerRadius: 12)
                    .stroke(
                        work.attentionCount > 0
                            ? Color.orange.opacity(0.5)
                            : Color(nsColor: .separatorColor).opacity(0.5)
                    )
            }
        }
        .buttonStyle(.plain)
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
    private enum PaneModule: String, CaseIterable, Identifiable {
        case overview = "Overview"
        case collaborate = "Collaborate"

        var id: Self { self }
    }

    let pane: MuxaWatchPane
    @ObservedObject var model: AppModel
    @State private var attachedSessionID: String?
    @State private var module: PaneModule = .overview

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
                        MuxaCollaborationView(pane: pane, client: model.client)
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
                .frame(minHeight: 220, idealHeight: 360)
            }
        }
        .onDisappear(perform: stopPanelAttach)
    }

    private var paneHeader: some View {
        HStack(spacing: 10) {
            HostIdentityBadge(identity: pane.host, size: 28)
            VStack(alignment: .leading, spacing: 1) {
                Text(pane.agent?.aiTitle ?? pane.pane.agentAlias.map { "@\($0)" } ?? pane.pane.windowName)
                    .font(.headline)
                Text("\(pane.host.alias) · \(pane.pane.session) › \(pane.pane.windowName) › \(pane.pane.paneID)")
                    .font(.caption2.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 8)
            Picker("Pane module", selection: $module) {
                ForEach(PaneModule.allCases) { module in
                    Text(module.rawValue).tag(module)
                }
            }
            .labelsHidden()
            .pickerStyle(.segmented)
            .frame(width: 210)
            Button {
                Task { await model.refresh() }
            } label: {
                Label("Refresh", systemImage: "arrow.clockwise")
            }
        }
        .padding(.horizontal, 14)
        .frame(height: 48)
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

struct MuxaAskView: View {
    @ObservedObject var model: AppModel
    @State private var prompt = ""
    @State private var agent = "claude"

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Label("Ask", systemImage: "sparkles")
                    .font(.headline)
                Picker("Agent", selection: $agent) {
                    Text("Claude").tag("claude")
                    Text("Codex").tag("codex")
                }
                .labelsHidden()
                .frame(width: 120)
                Spacer()
                Button {
                    Task { await model.resetAskConversation() }
                } label: {
                    Label("New Thread", systemImage: "plus.bubble")
                }
                .help("Start a new conversation without deleting history")
            }
            .padding(.horizontal, 14)
            .frame(height: 40)

            Divider()

            ScrollView {
                LazyVStack(alignment: .leading, spacing: 10) {
                    if model.askEntries.isEmpty {
                        ConsoleUnavailableView(
                            title: "No Ask history",
                            systemImage: "bubble.left.and.bubble.right",
                            description: "Ask a headless agent a question. Answers remain available after this view closes."
                        )
                        .frame(minHeight: 150)
                    } else {
                        ForEach(model.askEntries) { entry in
                            AskHistoryCard(entry: entry)
                        }
                    }
                }
                .padding(12)
            }

            Divider()

            VStack(alignment: .leading, spacing: 7) {
                TextEditor(text: $prompt)
                    .font(.body)
                    .scrollContentBackground(.hidden)
                    .padding(7)
                    .frame(minHeight: 64, maxHeight: 100)
                    .background(Color.primary.opacity(0.055), in: RoundedRectangle(cornerRadius: 7))
                HStack {
                    if let error = model.askError {
                        Text(error)
                            .font(.caption)
                            .foregroundStyle(.red)
                            .lineLimit(2)
                    }
                    Spacer()
                    if model.isSendingAsk { ProgressView().controlSize(.small) }
                    Button("Ask \(agent.capitalized)") { send() }
                        .buttonStyle(.borderedProminent)
                        .disabled(model.isSendingAsk || prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                }
            }
            .padding(12)
        }
        .onAppear { agent = model.askAgent }
    }

    private func send() {
        let submitted = prompt
        Task {
            if await model.sendAsk(prompt: submitted, agent: agent) {
                prompt = ""
            }
        }
    }
}

private struct AskHistoryCard: View {
    let entry: MuxaAskEntry

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 7) {
                Text(entry.agent.capitalized)
                    .font(.caption.weight(.semibold))
                Text(entry.status.replacingOccurrences(of: "_", with: " ").capitalized)
                    .font(.caption2.weight(.medium))
                    .foregroundStyle(entry.status == "failed" ? Color.red : entry.status == "running" ? Color.blue : Color.green)
                if entry.status == "running" { ProgressView().controlSize(.mini) }
                Spacer()
                Text(entry.askedAt)
                    .font(.caption2.monospacedDigit())
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
            }
            MarkdownContent(source: entry.prompt)
                .font(.subheadline.weight(.medium))
            if !entry.answer.isEmpty {
                Divider()
                MarkdownContent(source: entry.answer)
            }
            if let error = entry.error, !error.isEmpty {
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.red)
                    .textSelection(.enabled)
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 8))
    }
}

private struct MuxaCollaborationView: View {
    private enum ModuleTab: String, CaseIterable, Identifiable {
        case activity = "Activity"
        case compose = "Compose"
        var id: Self { self }
    }

    private enum MailboxTab: String, CaseIterable, Identifiable {
        case incoming = "Incoming"
        case sent = "Sent"
        var id: Self { self }
    }

    private enum DisplayMode: String, CaseIterable, Identifiable {
        case compact = "Compact"
        case detailed = "Detailed"
        var id: Self { self }
    }

    let pane: MuxaWatchPane
    let client: MuxaIPCClient
    @State private var mailbox = MuxaCollaborationMailbox(incoming: [], sent: [])
    @State private var module: ModuleTab = .activity
    @State private var tab: MailboxTab = .sent
    @State private var displayMode: DisplayMode = .compact
    @State private var kind = "question"
    @State private var workMode = "read_only"
    @State private var message = ""
    @State private var loading = false
    @State private var sending = false
    @State private var error: String?
    @State private var replyingTo: MuxaCollaborationRequest?

    private var requests: [MuxaCollaborationRequest] {
        tab == .incoming ? mailbox.incoming : mailbox.sent
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Label("Collaborate", systemImage: "person.2.wave.2")
                    .font(.headline)
                Picker("Collaborate module", selection: $module) {
                    ForEach(ModuleTab.allCases) { item in Text(item.rawValue).tag(item) }
                }
                .labelsHidden()
                .pickerStyle(.segmented)
                .frame(width: 180)
                Spacer()
                if loading { ProgressView().controlSize(.mini) }
                Button { Task { await load() } } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .buttonStyle(.borderless)
            }
            .padding(.horizontal, 14)
            .frame(height: 40)

            Divider()

            switch module {
            case .activity:
                HStack(spacing: 10) {
                    Picker("Mailbox", selection: $tab) {
                        ForEach(MailboxTab.allCases) { item in
                            Text("\(item.rawValue) \(item == .incoming ? mailbox.incoming.count : mailbox.sent.count)")
                                .tag(item)
                        }
                    }
                    .labelsHidden()
                    .pickerStyle(.segmented)
                    .frame(width: 220)
                    Spacer()
                    Picker("Density", selection: $displayMode) {
                        ForEach(DisplayMode.allCases) { item in Text(item.rawValue).tag(item) }
                    }
                    .labelsHidden()
                    .pickerStyle(.segmented)
                    .frame(width: 175)
                }
                .padding(.horizontal, 12)
                .frame(height: 40)
                .background(Color.primary.opacity(0.025))

                Divider()

                ScrollView {
                    LazyVStack(alignment: .leading, spacing: displayMode == .compact ? 4 : 9) {
                        if requests.isEmpty {
                            Text(tab == .incoming ? "No incoming requests for this agent." : "No requests sent from the operator in this room.")
                                .foregroundStyle(.secondary)
                                .frame(maxWidth: .infinity, minHeight: 90, alignment: .center)
                        } else {
                            ForEach(requests) { request in
                                CollaborationRequestCard(
                                    request: request,
                                    incoming: tab == .incoming,
                                    compact: displayMode == .compact,
                                    claim: { Task { await claim() } },
                                    reply: { replyingTo = request }
                                )
                            }
                        }
                    }
                    .padding(displayMode == .compact ? 8 : 12)
                }
            case .compose:
                collaborationComposer
            }
        }
        .task(id: pane.id) { await pollMailbox() }
        .sheet(item: $replyingTo) { request in
            CollaborationReplyView(request: request, pane: pane, client: client) {
                replyingTo = nil
                Task { await load() }
            }
        }
    }

    private var collaborationComposer: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Picker("Kind", selection: $kind) {
                    Text("Question").tag("question")
                    Text("Review").tag("review")
                    Text("Task").tag("task")
                    Text("Notice").tag("notice")
                }
                .frame(width: 130)
                Picker("Mode", selection: $workMode) {
                    Text("Read only").tag("read_only")
                    Text("Execute").tag("execute")
                }
                .frame(width: 130)
                Text("to \(pane.pane.agentAlias.map { "@\($0)" } ?? pane.pane.paneID)")
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                Spacer()
            }
            TextEditor(text: $message)
                .scrollContentBackground(.hidden)
                .padding(8)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Color.primary.opacity(0.055), in: RoundedRectangle(cornerRadius: 8))
            HStack {
                if let error {
                    Text(error).font(.caption).foregroundStyle(.red).lineLimit(2)
                }
                Spacer()
                if sending { ProgressView().controlSize(.small) }
                Button("Send Collaboration") { send() }
                    .buttonStyle(.borderedProminent)
                    .disabled(sending || message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(14)
    }

    private func pollMailbox() async {
        while !Task.isCancelled {
            await load()
            try? await Task.sleep(for: .seconds(2))
        }
    }

    private func load() async {
        guard !loading else { return }
        loading = true
        defer { loading = false }
        do {
            mailbox = try await client.collaborationMailbox(host: pane.host, pane: pane.pane)
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func send() {
        let submitted = message
        sending = true
        error = nil
        Task {
            defer { sending = false }
            do {
                _ = try await client.sendCollaboration(
                    host: pane.host,
                    pane: pane.pane,
                    kind: kind,
                    body: submitted,
                    workMode: workMode
                )
                message = ""
                tab = .sent
                module = .activity
                await load()
            } catch {
                self.error = error.localizedDescription
            }
        }
    }

    private func claim() async {
        do {
            try await client.claimCollaboration(host: pane.host, pane: pane.pane)
            await load()
        } catch {
            self.error = error.localizedDescription
        }
    }
}

private struct CollaborationRequestCard: View {
    let request: MuxaCollaborationRequest
    let incoming: Bool
    let compact: Bool
    let claim: () -> Void
    let reply: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            HStack(spacing: 7) {
                Text(request.kind.capitalized)
                    .font(.caption.weight(.semibold))
                Text(request.workMode == "execute" ? "Execute" : "Read only")
                    .font(.caption2)
                    .foregroundStyle(request.workMode == "execute" ? Color.orange : Color.secondary)
                Text(request.status.capitalized)
                    .font(.caption2.weight(.medium))
                    .foregroundStyle(collaborationStatusColor(request.status))
                Spacer()
                Text("\(request.from.label) → \(request.to.label)")
                    .font(.caption2.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            MarkdownContent(source: request.body, lineLimit: compact ? 2 : nil)
            if let response = request.reply {
                if !compact {
                    Divider()
                    MarkdownContent(source: response.body)
                }
                Label(response.status.capitalized, systemImage: "arrowshape.turn.up.left.fill")
                    .font(.caption2)
                    .foregroundStyle(collaborationStatusColor(response.status))
            }
            if incoming, request.reply == nil {
                HStack {
                    Spacer()
                    if request.status == "queued" {
                        Button("Claim", action: claim)
                    }
                    if request.status == "claimed" {
                        Button("Reply…", action: reply)
                            .buttonStyle(.borderedProminent)
                    }
                }
                .controlSize(.small)
            }
        }
        .padding(compact ? 8 : 11)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            Color.primary.opacity(compact ? 0.035 : 0.065),
            in: RoundedRectangle(cornerRadius: 8)
        )
    }
}

private struct CollaborationReplyView: View {
    let request: MuxaCollaborationRequest
    let pane: MuxaWatchPane
    let client: MuxaIPCClient
    let completed: () -> Void
    @State private var status = "completed"
    @State private var replyText = ""
    @State private var sending = false
    @State private var error: String?

    var replyBody: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Reply to \(request.from.label)")
                .font(.title2.weight(.semibold))
            MarkdownContent(source: request.body)
                .padding(10)
                .background(Color.primary.opacity(0.05), in: RoundedRectangle(cornerRadius: 7))
            Picker("Outcome", selection: $status) {
                Text("Completed").tag("completed")
                Text("Blocked").tag("blocked")
                Text("Declined").tag("declined")
                Text("Failed").tag("failed")
            }
            TextEditor(text: $replyText)
                .frame(minHeight: 120)
                .padding(7)
                .background(Color.primary.opacity(0.05), in: RoundedRectangle(cornerRadius: 7))
            if let error { Text(error).font(.caption).foregroundStyle(.red) }
            HStack {
                Spacer()
                Button("Cancel", action: completed)
                Button("Reply") { send() }
                    .buttonStyle(.borderedProminent)
                    .disabled(sending || replyText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(20)
        .frame(width: 520, height: 430)
    }

    var body: some View { replyBody }

    private func send() {
        sending = true
        Task {
            defer { sending = false }
            do {
                try await client.replyCollaboration(
                    host: pane.host,
                    pane: pane.pane,
                    requestID: request.id,
                    status: status,
                    body: replyText
                )
                completed()
            } catch {
                self.error = error.localizedDescription
            }
        }
    }
}

private func collaborationStatusColor(_ status: String) -> Color {
    switch status {
    case "completed": .green
    case "blocked", "declined", "failed", "expired", "cancelled": .red
    case "claimed": .blue
    default: .secondary
    }
}

struct HostRegistrationView: View {
    @ObservedObject var model: AppModel
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
                    Text("Register Fleet Host")
                        .font(.title2.weight(.semibold))
                    Text("Add an OpenSSH target to Muxa's central host inventory.")
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Button("Cancel") { model.isPresentingHostRegistration = false }
                    .keyboardShortcut(.cancelAction)
                Button("Register") { register() }
                    .keyboardShortcut(.defaultAction)
                    .buttonStyle(.borderedProminent)
                    .disabled(model.isRegisteringHost || alias.trimmingCharacters(in: .whitespaces).isEmpty || ssh.trimmingCharacters(in: .whitespaces).isEmpty)
            }
            .padding(20)

            Divider()

            Form {
                Section("Identity") {
                    TextField("Alias", text: $alias, prompt: Text("build-mac"))
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
                        .font(.caption)
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
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                .padding(.bottom, 14)
            }
        }
        .frame(width: 560, height: 540)
    }

    private func register() {
        Task {
            _ = await model.registerHost(
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
        }
    }
}

private struct WatchLivePanePanel: View {
    let pane: MuxaWatchPane
    @ObservedObject var model: AppModel
    let attachedSessionID: String?
    let startAttach: () -> Void
    let stopAttach: () -> Void
    let sessionExited: () -> Void
    @Environment(\.colorScheme) private var colorScheme
    @State private var prompt = ""
    @State private var sending = false
    @State private var feedback: String?

    private var attachedSession: MuxaSession? {
        attachedSessionID.flatMap { id in model.sessions.first(where: { $0.id == id }) }
    }

    private var canAttach: Bool {
        pane.host.local || pane.host.mode == "control"
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 9) {
                Label("Live Pane", systemImage: "terminal")
                    .font(.caption.weight(.semibold))
                    .fixedSize()
                Text("\(pane.host.alias) · \(pane.pane.session) › \(pane.pane.windowName) › \(pane.pane.paneID)")
                    .font(.caption2.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                Text(attachedSessionID == nil ? "Read-only" : "Interactive")
                    .font(.caption2.weight(.medium))
                    .foregroundStyle(attachedSessionID == nil ? Color.secondary : Color.green)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(Color.primary.opacity(0.07), in: Capsule())
                Spacer(minLength: 4)
                if model.isAttachingPane {
                    ProgressView().controlSize(.mini)
                }
                if attachedSessionID == nil {
                    Button {
                        startAttach()
                    } label: {
                        Label("Click to Type", systemImage: "keyboard")
                    }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.small)
                    .disabled(model.isAttachingPane || !canAttach)
                    .help(canAttach ? "Attach this pane here and enable keyboard input" : "This host is available in observe mode only")
                } else {
                    Button {
                        stopAttach()
                    } label: {
                        Label("Stop", systemImage: "stop.fill")
                    }
                    .buttonStyle(.borderless)
                }
            }
            .padding(.horizontal, 10)
            .frame(height: 36)
            .background(MuxaSurfacePalette.sidebar(for: colorScheme))

            Divider()

            if let session = attachedSession, !session.exited {
                TerminalPane(
                    client: model.client,
                    sessionID: session.id,
                    replayInitialHistory: session.hasBeenAttached == true,
                    allowsRaw: false,
                    showsToolbar: false,
                    onExit: sessionExited
                )
                .id(session.id)
            } else {
                PaneCaptureView(
                    client: model.client,
                    target: MuxaPaneTarget(host: pane.host, pane: pane.pane),
                    showsHeader: false
                )
                .id(pane.id)
                .contentShape(Rectangle())
                .onTapGesture {
                    guard canAttach, !model.isAttachingPane else { return }
                    startAttach()
                }
                .help(canAttach ? "Click anywhere in the preview to attach and type" : "Read-only pane preview")
                .task(id: attachedSessionID) {
                    if attachedSessionID != nil, attachedSession?.exited == true {
                        sessionExited()
                    }
                }

                Divider()

                PanePromptComposer(
                    host: pane.host,
                    pane: pane.pane,
                    client: model.client,
                    prompt: $prompt,
                    sending: $sending,
                    feedback: $feedback
                )
                .padding(10)
                .background(MuxaSurfacePalette.sidebar(for: colorScheme))
            }
        }
        .frame(minHeight: 190)
        .overlay {
            Rectangle()
                .stroke(Color(nsColor: .separatorColor).opacity(0.7), lineWidth: 0.5)
        }
    }
}

struct WatchHostTree: View {
    let group: MuxaWatchHost
    let selectedPaneID: MuxaWatchPaneIdentity?
    let selectPane: (MuxaWatchPaneIdentity) -> Void
    let openPinnedPane: (MuxaWatchPaneIdentity) -> Void
    let forceExpanded: Bool
    let workLabel: (MuxaWatchWindow) -> String?
    @State private var manualExpansion: Bool?

    private var containsSelection: Bool {
        selectedPaneID.map { selected in
            group.sessions.contains { session in
                session.windows.contains { window in
                    window.panes.contains { $0.id == selected }
                }
            }
        } ?? false
    }

    private var expanded: Bool {
        forceExpanded || (manualExpansion ?? containsSelection)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Button { manualExpansion = !expanded } label: {
                HStack(spacing: 6) {
                    hierarchyChevron(expanded)
                    HostIdentityBadge(host: group.host, size: 20)
                    Text(group.host.alias)
                        .font(.callout.weight(.semibold))
                        .lineLimit(1)
                    Spacer(minLength: 4)
                    Text("\(group.paneCount)")
                        .font(.caption2.monospacedDigit())
                        .foregroundStyle(.secondary)
                    Circle()
                        .fill(fleetHostColor(group.host.state))
                        .frame(width: 6, height: 6)
                }
                .padding(.horizontal, 5)
                .frame(height: 30)
                .background(expanded ? Color.primary.opacity(0.035) : Color.clear)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)

            if expanded {
                ForEach(group.sessions) { session in
                    WatchSessionTree(
                        session: session,
                        selectedPaneID: selectedPaneID,
                        selectPane: selectPane,
                        openPinnedPane: openPinnedPane,
                        forceExpanded: forceExpanded,
                        workLabel: workLabel
                    )
                }
            }
        }
    }
}

private struct WatchSessionTree: View {
    let session: MuxaWatchSession
    let selectedPaneID: MuxaWatchPaneIdentity?
    let selectPane: (MuxaWatchPaneIdentity) -> Void
    let openPinnedPane: (MuxaWatchPaneIdentity) -> Void
    let forceExpanded: Bool
    let workLabel: (MuxaWatchWindow) -> String?
    @State private var manualExpansion: Bool?

    private var selectedPath: Bool {
        selectedPaneID.map { selected in
            session.windows.contains { window in
                window.panes.contains { $0.id == selected }
            }
        } ?? false
    }

    private var expanded: Bool {
        forceExpanded || (manualExpansion ?? (singleWindow != nil || selectedPath))
    }

    private var singleWindow: MuxaWatchWindow? {
        session.windows.count == 1 ? session.windows.first : nil
    }

    private var firstPane: MuxaWatchPane? {
        session.windows.first?.panes.first
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 0) {
                explorerIndent(depth: 1)
                Button { manualExpansion = !expanded } label: {
                    hierarchyChevron(expanded)
                        .frame(width: 18, height: 26)
                }
                .buttonStyle(.plain)
                Button {
                    if let firstPane { selectPane(firstPane.id) }
                    if !expanded { manualExpansion = true }
                } label: {
                    HStack(spacing: 6) {
                        Image(systemName: singleWindow == nil ? "square.3.layers.3d" : "rectangle.stack")
                            .foregroundStyle(singleWindow == nil ? Color.secondary : Color.accentColor)
                            .frame(width: 17)
                        Text(session.name.isEmpty ? session.sessionID : session.name)
                            .font(.callout)
                            .lineLimit(1)
                        if let singleWindow {
                            Image(systemName: "chevron.right")
                                .font(.system(size: 7, weight: .bold))
                                .foregroundStyle(.tertiary)
                            Text(workLabel(singleWindow) ?? (singleWindow.name.isEmpty ? singleWindow.windowID : singleWindow.name))
                                .font(.callout)
                                .foregroundStyle(workLabel(singleWindow) == nil ? Color.secondary : Color.accentColor)
                                .lineLimit(1)
                        }
                        Spacer(minLength: 3)
                        Text(singleWindow.map { "\($0.panes.count)" } ?? "\(session.windows.count)")
                            .font(.caption2.monospacedDigit())
                            .foregroundStyle(.tertiary)
                    }
                    .frame(height: 26)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .simultaneousGesture(
                    TapGesture(count: 2).onEnded {
                        if let firstPane { openPinnedPane(firstPane.id) }
                    }
                )
            }
            .background(selectedPath ? Color.accentColor.opacity(0.08) : Color.clear)

            if expanded {
                if let singleWindow {
                    ForEach(singleWindow.panes) { pane in
                        WatchPaneRow(
                            pane: pane,
                            selected: selectedPaneID == pane.id,
                            depth: 2,
                            selectPane: selectPane,
                            openPinnedPane: openPinnedPane
                        )
                    }
                } else {
                    ForEach(session.windows) { window in
                        WatchWindowTree(
                            window: window,
                            selectedPaneID: selectedPaneID,
                            selectPane: selectPane,
                            openPinnedPane: openPinnedPane,
                            forceExpanded: forceExpanded,
                            logicalWork: workLabel(window)
                        )
                    }
                }
            }
        }
    }
}

private struct WatchWindowTree: View {
    let window: MuxaWatchWindow
    let selectedPaneID: MuxaWatchPaneIdentity?
    let selectPane: (MuxaWatchPaneIdentity) -> Void
    let openPinnedPane: (MuxaWatchPaneIdentity) -> Void
    let forceExpanded: Bool
    let logicalWork: String?
    @State private var manualExpansion: Bool?

    private var containsSelection: Bool {
        selectedPaneID.map { selected in window.panes.contains { $0.id == selected } } ?? false
    }

    private var expanded: Bool {
        forceExpanded || (manualExpansion ?? containsSelection)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 0) {
                explorerIndent(depth: 2)
                Button { manualExpansion = !expanded } label: {
                    hierarchyChevron(expanded)
                        .frame(width: 18, height: 26)
                }
                .buttonStyle(.plain)
                Button {
                    if let pane = window.panes.first { selectPane(pane.id) }
                    if !expanded { manualExpansion = true }
                } label: {
                    HStack(spacing: 6) {
                        Image(systemName: logicalWork == nil ? "macwindow" : "square.stack.3d.up")
                            .foregroundStyle(logicalWork == nil ? Color.secondary : Color.accentColor)
                            .frame(width: 17)
                        Text(logicalWork ?? (window.name.isEmpty ? window.windowID : window.name))
                            .font(.callout)
                            .fontWeight(logicalWork == nil ? .regular : .medium)
                            .lineLimit(1)
                        Spacer(minLength: 3)
                        Text("#\(window.index) · \(window.panes.count)")
                            .font(.caption2.monospacedDigit())
                            .foregroundStyle(.tertiary)
                    }
                    .frame(height: 26)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .simultaneousGesture(
                    TapGesture(count: 2).onEnded {
                        if let pane = window.panes.first { openPinnedPane(pane.id) }
                    }
                )
            }
            .background(containsSelection ? Color.accentColor.opacity(0.08) : Color.clear)

            if expanded {
                ForEach(window.panes) { pane in
                    WatchPaneRow(
                        pane: pane,
                        selected: selectedPaneID == pane.id,
                        depth: 3,
                        selectPane: selectPane,
                        openPinnedPane: openPinnedPane
                    )
                }
            }
        }
    }
}

private struct WatchPaneRow: View {
    let pane: MuxaWatchPane
    let selected: Bool
    let depth: Int
    let selectPane: (MuxaWatchPaneIdentity) -> Void
    let openPinnedPane: (MuxaWatchPaneIdentity) -> Void

    private var title: String {
        pane.pane.agentAlias.map { "@\($0)" }
            ?? pane.agent?.aiTitle
            ?? pane.pane.title.nonEmpty
            ?? pane.pane.currentCommand.nonEmpty
            ?? pane.pane.paneID
    }

    private var subtitle: String {
        let state = pane.agent.map { agentStateLabel($0.state) }
        return [pane.pane.paneID, pane.pane.currentCommand, state]
            .compactMap { $0?.nonEmpty }
            .joined(separator: " · ")
    }

    var body: some View {
        Button { selectPane(pane.id) } label: {
            HStack(spacing: 0) {
                explorerIndent(depth: depth)
                Color.clear.frame(width: 18)
                HStack(spacing: 6) {
                    Image(systemName: pane.agent == nil ? "terminal" : "person.crop.circle")
                        .font(.caption)
                        .foregroundStyle(pane.agent.map { agentStateColor($0.state) } ?? Color.secondary)
                        .frame(width: 17)
                Circle()
                    .fill(pane.agent.map { agentStateColor($0.state) } ?? Color.secondary)
                        .frame(width: 5, height: 5)
                    Text(title)
                        .font(.callout)
                        .lineLimit(1)
                    Spacer(minLength: 3)
                    Text(subtitle)
                        .font(.caption2.monospaced())
                        .foregroundStyle(.tertiary)
                        .lineLimit(1)
                    if paneNeedsAttention(pane) {
                        Image(systemName: "exclamationmark.circle.fill")
                            .foregroundStyle(.orange)
                    }
                }
                .padding(.trailing, 6)
            }
            .frame(height: 27)
            .background(
                selected ? Color.accentColor.opacity(0.18) : Color.clear
            )
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .simultaneousGesture(
            TapGesture(count: 2).onEnded { openPinnedPane(pane.id) }
        )
        .contextMenu {
            Button("Open") { selectPane(pane.id) }
            Button("Open Pinned") { openPinnedPane(pane.id) }
        }
    }
}

private func explorerIndent(depth: Int) -> some View {
    HStack(spacing: 11) {
        ForEach(0..<depth, id: \.self) { _ in
            Rectangle()
                .fill(Color(nsColor: .separatorColor).opacity(0.42))
                .frame(width: 1)
        }
    }
    .frame(width: CGFloat(depth) * 12, height: 27, alignment: .trailing)
}

private func hierarchyChevron(_ expanded: Bool) -> some View {
    Image(systemName: "chevron.right")
        .font(.caption2.weight(.semibold))
        .foregroundStyle(.secondary)
        .rotationEffect(.degrees(expanded ? 90 : 0))
        .frame(width: 10)
}

private struct FleetPaneInspector: View {
    let pane: MuxaWatchPane
    @ObservedObject var model: AppModel
    let compact: Bool
    let openInShell: () -> Void

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: compact ? 12 : 16) {
                ViewThatFits(in: .horizontal) {
                    HStack(alignment: .top, spacing: 12) {
                        inspectorIdentity
                            .frame(minWidth: 250, alignment: .leading)
                        Spacer(minLength: 8)
                        inspectorActions
                            .fixedSize()
                    }
                    VStack(alignment: .leading, spacing: 10) {
                        inspectorIdentity
                        compactInspectorActions
                    }
                }

                if let agent = pane.agent {
                    ViewThatFits(in: .horizontal) {
                        HStack(spacing: 8) {
                            agentStatus(agent)
                        }
                        VStack(alignment: .leading, spacing: 4) {
                            agentStatus(agent)
                        }
                    }
                    .font(.caption)
                    if let summary = agent.recap ?? agent.lastNotification ?? agent.lastResponse,
                       !summary.isEmpty {
                        MarkdownContent(source: summary, lineLimit: compact ? 5 : 7)
                            .padding(12)
                            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 10))
                    }
                }

                if let error = model.attachError {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .font(.caption)
                        .foregroundStyle(.red)
                        .textSelection(.enabled)
                }

            }
            .padding(compact ? 12 : 18)
        }
    }

    private var inspectorIdentity: some View {
        HStack(alignment: .top, spacing: 10) {
            HostIdentityBadge(identity: pane.host, size: 36)
            VStack(alignment: .leading, spacing: 2) {
                Text(pane.agent?.aiTitle ?? pane.pane.agentAlias.map { "@\($0)" } ?? pane.pane.title.nonEmpty ?? pane.pane.currentCommand)
                    .font(.title2.weight(.semibold))
                    .lineLimit(2)
                Text("\(pane.host.alias) · \(pane.pane.session) › \(pane.pane.windowName) › \(pane.pane.paneID)")
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
            }
        }
    }

    private var inspectorActions: some View {
        HStack(spacing: 8) {
            Button {
                openInShell()
            } label: {
                Label("Open in Shell", systemImage: "rectangle.on.rectangle")
            }
        }
    }

    private var compactInspectorActions: some View {
        HStack(spacing: 8) {
            Button {
                openInShell()
            } label: {
                Label("Shell", systemImage: "rectangle.on.rectangle")
            }
        }
    }

    @ViewBuilder
    private func agentStatus(_ agent: MuxaAgent) -> some View {
        Label(agentStateLabel(agent.state), systemImage: "circle.fill")
            .foregroundStyle(agentStateColor(agent.state))
        if let modelName = agent.model { Text(modelName) }
        if let context = agent.contextUsedPercent {
            Text("context \(context, format: .number.precision(.fractionLength(0)))%")
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

private struct PanePromptComposer: View {
    let host: MuxaFleetHostIdentity
    let pane: MuxaPaneInfo
    let client: MuxaIPCClient
    @Binding var prompt: String
    @Binding var sending: Bool
    @Binding var feedback: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            ViewThatFits(in: .horizontal) {
                HStack(spacing: 8) {
                    promptField
                    sendButton.fixedSize()
                }
                VStack(alignment: .trailing, spacing: 8) {
                    promptField
                    sendButton
                }
            }
            if !host.local && host.mode != "control" {
                Text("This host is registered in observe mode. Change it to control to send prompts.")
                    .font(.caption2)
                    .foregroundStyle(.orange)
            } else if let feedback {
                Text(feedback)
                    .font(.caption2)
                    .foregroundStyle(feedback.hasPrefix("Sent") ? .green : .red)
            }
        }
    }

    private var promptField: some View {
        TextField("Send a prompt to this agent/pane", text: $prompt, axis: .vertical)
            .textFieldStyle(.roundedBorder)
            .lineLimit(1...4)
            .onSubmit(send)
    }

    private var sendButton: some View {
        Button("Send", action: send)
            .buttonStyle(.borderedProminent)
            .disabled(prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || sending || (!host.local && host.mode != "control"))
    }

    private func send() {
        let text = prompt.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, !sending else { return }
        sending = true
        feedback = nil
        Task {
            defer { sending = false }
            do {
                try await client.sendFleetPrompt(host: host, pane: pane, text: text)
                prompt = ""
                feedback = "Sent and submitted"
            } catch {
                feedback = error.localizedDescription
            }
        }
    }
}

struct WorkPromptComposer: View {
    let work: MuxaWorkGroup
    @ObservedObject var model: AppModel
    @State private var prompt = ""
    @State private var sending = false
    @State private var feedback: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            Text("Steer this Work")
                .font(.headline)
            ViewThatFits(in: .horizontal) {
                HStack(spacing: 8) {
                    workPromptField
                    workSendButton.fixedSize()
                }
                VStack(alignment: .trailing, spacing: 8) {
                    workPromptField
                    workSendButton
                }
            }
            if let feedback {
                Text(feedback)
                    .font(.caption)
                    .foregroundStyle(feedback.hasPrefix("Sent") ? .green : .red)
            }
        }
        .padding(14)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
    }

    private var workPromptField: some View {
        TextField("Send the next instruction to every live collaborator", text: $prompt, axis: .vertical)
            .textFieldStyle(.roundedBorder)
            .lineLimit(1...4)
            .onSubmit(send)
    }

    private var workSendButton: some View {
        Button("Send to \(work.participants.count)", action: send)
            .buttonStyle(.borderedProminent)
            .disabled(prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || sending || work.participants.isEmpty)
    }

    private func send() {
        let text = prompt.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, !sending else { return }
        sending = true
        feedback = nil
        Task {
            defer { sending = false }
            do {
                let count = try await model.prompt(work: work, text: text)
                prompt = ""
                feedback = "Sent to \(count) collaborator\(count == 1 ? "" : "s")"
            } catch {
                feedback = error.localizedDescription
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
        .help("\(alias) · \(state)")
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
                moduleMissing("Fleet pane is no longer available")
            }
        }
    }

    private func moduleMissing(_ text: String) -> some View {
        ConsoleUnavailableView(
            title: "Module unavailable",
            systemImage: "terminal.fill",
            description: text
        )
        .frame(minWidth: 680, minHeight: 480)
    }
}

private func paneNeedsAttention(_ pane: MuxaWatchPane) -> Bool {
    pane.agent.map {
        ["waiting_input", "waiting_choice", "blocked", "error", "failed"].contains($0.state)
    } ?? false
}

private struct ConsoleUnavailableView: View {
    let title: String
    let systemImage: String
    let description: String

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
