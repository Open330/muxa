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
                        .font(.title2.weight(.semibold))
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
                        Text(
                            routePinsDirectory
                                ? "The route decides the folder."
                                : isLocalHost
                                    ? "Defaults to your home folder because the route names none; choose the project you want the agents to work in."
                                    : "The route on \(host) names no folder; type the project path on that host."
                        )
                        .font(.caption)
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
                    TextField("External issue, for example CAL-1234 (optional)", text: $external)
                    Text("An empty external issue creates a local Muxa Work; the issue never becomes the Work identity.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
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
                        .font(.caption)
                        .foregroundStyle(.red)
                        .textSelection(.enabled)
                    if model.needsWorkConfiguration {
                        HStack {
                            Text("No Work routing is configured yet. Install a preset above, or let an agent write the config in an interactive Shell tab.")
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
                    Text(candidate.local ? "\(candidate.alias) (this Mac)" : candidate.alias)
                        .tag(candidate.local ? "" : candidate.alias)
                }
            }
            if !isLocalHost, !model.supportsHostWorkCommands {
                Label(
                    "Starting Work on \(host) needs the updated muxad on this Mac (Use Bundled muxad), and muxa on \(host) must know `work options`.",
                    systemImage: "exclamationmark.triangle"
                )
                .font(.caption)
                .foregroundStyle(.orange)
            } else if !isLocalHost {
                Text("The pipeline and route come from \(host)'s config; agents start in tmux on that host.")
                    .font(.caption)
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
                        .font(.caption)
                        .foregroundStyle(.secondary)
                } icon: {
                    Image(systemName: "arrow.triangle.branch")
                        .foregroundStyle(.tint)
                }
            } else if options.pipelines.isEmpty {
                EmptyView()
            } else {
                Label("No route matches this Work id; choose a pipeline below.", systemImage: "arrow.triangle.branch")
                    .font(.caption)
                    .foregroundStyle(.orange)
            }
        }
    }

    private func routeDescription(_ route: MuxaWorkOptions.Route) -> String {
        var parts = ["Route \(route.match)"]
        if let name = route.pipeline, !name.isEmpty { parts.append("pipeline \(name)") }
        if let workspace = route.workspace, !workspace.isEmpty { parts.append("workspace \(workspace)") }
        if route.worktree {
            parts.append("own git worktree")
        } else if let cwd = route.cwd, !cwd.isEmpty {
            parts.append("cwd \(cwd)")
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
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                        PipelineStagesView(agents: selected.agents)
                        if let layout = selected.layout, !layout.isEmpty {
                            Text("tmux layout \(layout)")
                                .font(.caption2.monospaced())
                                .foregroundStyle(.tertiary)
                        }
                    }
                } else if pipeline.isEmpty {
                    Text("The route for this Work id names no pipeline. Pick one to launch.")
                        .font(.caption)
                        .foregroundStyle(.orange)
                }
            }
        } else if let error = model.workOptionsError(for: isLocalHost ? nil : host) {
            TextField("Pipeline (use configured route when empty)", text: $pipeline)
            Label(error, systemImage: "exclamationmark.triangle.fill")
                .font(.caption)
                .foregroundStyle(.orange)
                .textSelection(.enabled)
        } else {
            TextField("Pipeline (use configured route when empty)", text: $pipeline)
            Text("Reading pipelines from \(isLocalHost ? "the muxa config" : host)…")
                .font(.caption)
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
            return "Route default (\(name))"
        }
        return work.trimmingCharacters(in: .whitespaces).isEmpty
            ? "Route default"
            : "Route default (none)"
    }

    @ViewBuilder
    private var skillField: some View {
        if let skills = options?.skills, !skills.isEmpty {
            Picker("Message skill", selection: $skill) {
                Text("None").tag("")
                ForEach(skills) { candidate in
                    Text(candidate.summary.map { "\(candidate.name) — \($0)" } ?? candidate.name)
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
                    CommandCenterMetric(title: "Hosts", value: model.fleetHosts.count, color: .mint)
                }

                VStack(alignment: .leading, spacing: 10) {
                    Text("Hosts")
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

                pipelinesSection

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
        .task { await model.loadAllWorkOptions() }
        .onChange(of: model.workCapableHosts.map(\.alias)) { _ in
            // The host list arrives with the first fleet snapshot, usually
            // after this view appeared; read the newly known hosts then.
            Task { await model.loadAllWorkOptions() }
        }
    }

    private let pipelineColumns = [
        GridItem(.adaptive(minimum: 300, maximum: 460), spacing: 12, alignment: .top),
    ]

    /// Host whose routes the Routes editor shows; "" is the local host.
    /// Pipelines are one library kept in sync across hosts; routes carry
    /// host-specific folders, so they stay per host.
    @State private var routesHost = ""
    @State private var syncingPipelines = Set<String>()
    @State private var syncFailures: [String: String] = [:]

    private var routesHostAlias: String? { routesHost.isEmpty ? nil : routesHost }
    private var pipelinesHostAlias: String? { nil }

    /// `muxa work init` opens a local Shell tab, so only the local host gets it.
    private var describeWithAgentAction: (() -> Void)? {
        guard pipelinesHostAlias == nil else { return nil }
        return { Task { await model.configureWork(cwd: nil) } }
    }

    /// The pipeline library drawn as launchable presets. This is where a GUI
    /// earns its place over the CLI: the stage picture is visible before a
    /// single agent exists, every control host shows whether it carries the
    /// same definition, and an empty config offers muxa's built-in presets
    /// instead of an error.
    @ViewBuilder
    private var pipelinesSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text("Pipelines")
                    .font(.title2.weight(.semibold))
                if let path = model.workOptions?.configPath {
                    Text(path)
                        .font(.caption2.monospaced())
                        .foregroundStyle(.tertiary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .help(path)
                }
                Spacer()
                if model.isLoadingWorkOptions {
                    ProgressView().controlSize(.small)
                }
                if libraryNeedsSync {
                    Button {
                        syncAll()
                    } label: {
                        Label("Sync All to Hosts", systemImage: "arrow.triangle.2.circlepath")
                    }
                    .buttonStyle(.borderless)
                    .disabled(!syncingPipelines.isEmpty)
                    .help("Write every library pipeline to the hosts where it is missing or differs")
                }
                Button {
                    model.presentPipelineEditor(host: nil, pipeline: nil)
                } label: {
                    Label("New Pipeline…", systemImage: "plus")
                }
                .buttonStyle(.borderless)
                .disabled(model.workOptions == nil)
                Button {
                    Task { await model.loadAllWorkOptions() }
                } label: {
                    Label("Reload", systemImage: "arrow.clockwise")
                }
                .buttonStyle(.borderless)
                .help("Re-read pipelines and routes from every host")
            }

            if model.workCapableHosts.count > 1, !model.supportsHostWorkCommands {
                Label(
                    "Host sync needs the updated muxad on this Mac (Settings › Runtime › Reload Bundled muxad).",
                    systemImage: "exclamationmark.triangle"
                )
                .font(.caption)
                .foregroundStyle(.orange)
            }

            if let options = model.workOptions(for: pipelinesHostAlias) {
                if options.pipelines.isEmpty {
                    WorkPresetGallery(
                        options: options,
                        host: pipelinesHostAlias,
                        model: model,
                        onInstalled: { installed in
                            model.presentWorkStart(pipeline: installed, host: pipelinesHostAlias)
                        },
                        onDescribe: describeWithAgentAction
                    )
                    .padding(16)
                    .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 14))
                } else {
                    LazyVGrid(columns: pipelineColumns, alignment: .leading, spacing: 12) {
                        ForEach(options.pipelines) { pipeline in
                            WorkPipelineCard(
                                pipeline: pipeline,
                                routes: options.routes.filter { $0.pipeline == pipeline.name },
                                start: { model.presentWorkStart(pipeline: pipeline.name) },
                                edit: { model.presentPipelineEditor(host: nil, pipeline: pipeline) },
                                hostStates: model.pipelineHostStates(for: pipeline),
                                sync: { sync(pipeline) },
                                syncing: syncingPipelines.contains(pipeline.name)
                            )
                        }
                    }
                    remoteOnlyPipelinesRow
                    if !syncFailures.isEmpty {
                        ForEach(syncFailures.keys.sorted(), id: \.self) { key in
                            Label("\(key): \(syncFailures[key] ?? "")", systemImage: "exclamationmark.triangle.fill")
                                .font(.caption)
                                .foregroundStyle(.orange)
                                .textSelection(.enabled)
                        }
                    }
                    routesSection
                    if let error = model.workOptionsError {
                        Label(error, systemImage: "exclamationmark.triangle.fill")
                            .font(.caption)
                            .foregroundStyle(.orange)
                            .textSelection(.enabled)
                    }
                }
            } else if let error = model.workOptionsError(for: pipelinesHostAlias) {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.orange)
                    .textSelection(.enabled)
                    .padding(14)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
            } else {
                Text("Reading pipelines from the muxa config…")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .padding(14)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
            }
        }
    }

    private var libraryNeedsSync: Bool {
        (model.workOptions?.pipelines ?? []).contains { pipeline in
            model.pipelineHostStates(for: pipeline).contains(where: \.needsSync)
        }
    }

    private func sync(_ pipeline: MuxaWorkOptions.Pipeline) {
        guard !syncingPipelines.contains(pipeline.name) else { return }
        syncingPipelines.insert(pipeline.name)
        Task {
            let failures = await model.syncPipeline(pipeline)
            syncFailures = syncFailures.filter { !$0.key.hasSuffix("/\(pipeline.name)") }
            for (host, error) in failures { syncFailures["\(host)/\(pipeline.name)"] = error }
            syncingPipelines.remove(pipeline.name)
        }
    }

    private func syncAll() {
        let names = (model.workOptions?.pipelines ?? []).map(\.name)
        syncingPipelines.formUnion(names)
        Task {
            syncFailures = await model.syncAllPipelines()
            syncingPipelines.subtract(names)
        }
    }

    /// Pipelines that only exist on some host: pull one into the library
    /// (this Mac's config) so it can be synced everywhere.
    @ViewBuilder
    private var remoteOnlyPipelinesRow: some View {
        let remoteOnly = model.remoteOnlyPipelines
        if !remoteOnly.isEmpty {
            VStack(alignment: .leading, spacing: 6) {
                Text("Only on other hosts")
                    .font(.headline)
                ForEach(Array(remoteOnly.enumerated()), id: \.offset) { _, entry in
                    HStack(spacing: 10) {
                        Text(entry.pipeline.name)
                            .font(.callout.weight(.medium))
                        Text("on \(entry.host)")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                        PipelineStagesView(agents: entry.pipeline.agents, compact: true)
                            .frame(maxWidth: 420)
                        Spacer(minLength: 4)
                        Button {
                            Task {
                                _ = await model.savePipeline(
                                    MuxaPipelineDefinition(entry.pipeline),
                                    named: entry.pipeline.name,
                                    host: nil
                                )
                            }
                        } label: {
                            Label("Add to Library", systemImage: "square.and.arrow.down")
                        }
                        .controlSize(.small)
                        .disabled(model.isSavingPipeline)
                        .help("Copy this pipeline into this Mac's config so it can be synced to every host")
                    }
                }
            }
            .padding(14)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
        }
    }

    /// Routes are host-specific (they carry folders and workspaces on that
    /// host), so the editor keeps its own host switcher.
    @ViewBuilder
    private var routesSection: some View {
        VStack(alignment: .leading, spacing: 8) {
            if model.workCapableHosts.count > 1 {
                Picker("Routes on", selection: $routesHost) {
                    ForEach(model.workCapableHosts) { candidate in
                        Text(candidate.local ? "\(candidate.alias) (this Mac)" : candidate.alias)
                            .tag(candidate.local ? "" : candidate.alias)
                    }
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .frame(maxWidth: 560, alignment: .leading)
            }
            if let options = model.workOptions(for: routesHostAlias) {
                WorkRoutesEditor(options: options, host: routesHostAlias, model: model)
            } else if let error = model.workOptionsError(for: routesHostAlias) {
                Label("\(routesHost): \(error)", systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.orange)
                    .textSelection(.enabled)
            } else {
                Text("Reading routes on \(routesHost)…")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
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
                Text(work.pipelineLabel)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                HStack(spacing: 14) {
                    Label("\(work.participants.count)", systemImage: "person.2")
                    if work.pipelineRun != nil {
                        Label("\(work.completedCount)/\(work.totalCount)", systemImage: "checkmark.circle")
                    }
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
                        MuxaCollaborationView(
                            pane: pane,
                            client: model.client,
                            mailboxRevision: model.mailboxRevisions[pane.host.alias]
                        )
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
    }

    private var paneHeader: some View {
        ViewThatFits(in: .horizontal) {
            HStack(spacing: 10) {
                paneIdentity(showsLocation: true)
                Spacer(minLength: 8)
                modulePicker(width: 210)
                Button {
                    Task { await model.refresh() }
                } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
            }

            HStack(spacing: 8) {
                paneIdentity(showsLocation: false)
                Spacer(minLength: 4)
                modulePicker(width: 150)
                Button {
                    Task { await model.refresh() }
                } label: {
                    Image(systemName: "arrow.clockwise")
                        .frame(width: 28, height: 28)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help("Refresh")
            }
        }
        .padding(.horizontal, 14)
        .frame(height: 48)
    }

    private func paneIdentity(showsLocation: Bool) -> some View {
        HStack(spacing: 10) {
            HostIdentityBadge(identity: pane.host, size: 28)
            VStack(alignment: .leading, spacing: 1) {
                Text(pane.agent?.aiTitle ?? pane.pane.agentAlias.map { "@\($0)" } ?? pane.pane.windowName)
                    .font(.headline)
                    .lineLimit(1)
                if showsLocation {
                    Text("\(pane.host.alias) · \(pane.pane.session) › \(pane.pane.windowName) › \(pane.pane.paneID)")
                        .font(.caption2.monospaced())
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    private func modulePicker(width: CGFloat) -> some View {
        Picker("Pane module", selection: $module) {
            ForEach(PaneModule.allCases) { module in
                Text(module.rawValue).tag(module)
            }
        }
        .labelsHidden()
        .pickerStyle(.segmented)
        .frame(width: width)
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

    private var activeConversation: MuxaAskConversation? {
        model.askConversations.first { $0.id == model.activeAskConversationID }
    }

    private var providerConversations: [MuxaAskConversation] {
        model.askConversations.filter { $0.agent == agent }
    }

    private var conversationEntries: [MuxaAskEntry] {
        let entries: [MuxaAskEntry]
        if let conversationID = model.activeAskConversationID {
            entries = model.askEntries.filter { $0.conversationID == conversationID }
        } else {
            // Compatibility with a daemon from before muxa-owned conversation
            // ids: retain the prior provider-filtered history until it reloads.
            entries = model.askEntries.filter { $0.agent == agent }
        }
        return entries.sorted { $0.askedAt < $1.askedAt }
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Label("Global Ask", systemImage: "sparkles")
                    .font(.headline)
                if let activeConversation {
                    Image(systemName: "chevron.right")
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                    Text(activeConversation.title)
                        .font(.subheadline.weight(.medium))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                Spacer()
                Text("Conversations resume their Claude Code or Codex context")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
            }
            .padding(.horizontal, 14)
            .frame(height: 40)

            Divider()

            if model.askEnabled == false {
                HStack(alignment: .center, spacing: 12) {
                    Image(systemName: "sparkles.rectangle.stack")
                        .font(.title3)
                        .foregroundStyle(.tint)
                    VStack(alignment: .leading, spacing: 2) {
                        Text(model.askConfigurationPendingReload ? "Reload to finish enabling Ask" : "Enable Global Ask")
                            .font(.subheadline.weight(.semibold))
                        Text(
                            model.askConfigurationPendingReload
                                ? "The grant is saved. Reload muxad to apply it; tmux sessions will remain running."
                                : "Muxa will run the selected provider CLI headlessly. Provider usage may be billed to your account."
                        )
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    Spacer(minLength: 12)
                    if model.isEnablingAsk {
                        ProgressView()
                            .controlSize(.small)
                    }
                    Button(model.askConfigurationPendingReload ? "Reload muxad" : "Enable & Reload") {
                        Task { await model.enableAsk() }
                    }
                    .buttonStyle(.borderedProminent)
                    .disabled(model.isEnablingAsk)
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 10)
                .background(Color.accentColor.opacity(0.07))

                Divider()
            }

            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 0) {
                        if conversationEntries.isEmpty {
                            ConsoleUnavailableView(
                                title: activeConversation == nil ? "Start a conversation" : "No messages yet",
                                systemImage: "bubble.left.and.bubble.right",
                                description: "Ask a headless agent a question. You can return to this conversation and continue it later."
                            )
                            .frame(minHeight: 150)
                        } else {
                            ForEach(Array(conversationEntries.enumerated()), id: \.element.id) { index, entry in
                                AskConversationTurn(entry: entry)
                                    .id(entry.id)
                                if index < conversationEntries.count - 1 {
                                    Divider().padding(.vertical, 6)
                                }
                            }
                        }
                    }
                    .padding(14)
                    .frame(maxWidth: 980)
                    .frame(maxWidth: .infinity)
                }
                .onAppear { scrollToLatest(proxy) }
                .onChange(of: conversationEntries.last?.id) { _ in scrollToLatest(proxy) }
            }

            Divider()

            VStack(alignment: .leading, spacing: 7) {
                AskComposerEditor(
                    text: $prompt,
                    placeholder: "Ask about work across your hosts…"
                )
                .frame(minHeight: 72, maxHeight: 112)
                .background(Color.primary.opacity(0.055), in: RoundedRectangle(cornerRadius: 8))
                .overlay {
                    RoundedRectangle(cornerRadius: 8)
                        .stroke(Color.primary.opacity(0.08), lineWidth: 1)
                }
                .disabled(model.askEnabled == false || !model.isConnected)
                ViewThatFits(in: .horizontal) {
                    HStack(spacing: 8) {
                        askContextControls
                        Spacer(minLength: 8)
                        askSendStatus
                        sendButton
                    }
                    VStack(alignment: .leading, spacing: 8) {
                        HStack(spacing: 8) {
                            askContextControls
                            Spacer()
                        }
                        HStack {
                            askSendStatus
                            Spacer()
                            sendButton
                        }
                    }
                }
            }
            .padding(12)
        }
        .onAppear { agent = model.askAgent }
        .onChange(of: model.askAgent) { selected in agent = selected }
        .onChange(of: agent) { selected in
            Task { await model.selectAskAgent(selected) }
        }
        .sheet(isPresented: $model.isPresentingAskSettings) {
            AskProviderSettingsView(model: model)
        }
    }

    private var askContextControls: some View {
        HStack(spacing: 7) {
            Picker("Provider", selection: $agent) {
                Text("Claude Code").tag("claude")
                Text("Codex").tag("codex")
            }
            .labelsHidden()
            .frame(width: 132)

            Menu {
                if providerConversations.isEmpty {
                    Text("No previous conversations")
                } else {
                    ForEach(providerConversations) { conversation in
                        Button {
                            Task { await model.selectAskConversation(conversation.id) }
                        } label: {
                            if conversation.id == model.activeAskConversationID {
                                Label(conversation.title, systemImage: "checkmark")
                            } else {
                                Text(conversation.title)
                            }
                        }
                    }
                }
            } label: {
                Label(activeConversation?.title ?? "Conversations", systemImage: "bubble.left.and.bubble.right")
                    .lineLimit(1)
                    .frame(width: 190, alignment: .leading)
            }
            .menuStyle(.borderlessButton)

            Button {
                Task { await model.resetAskConversation() }
            } label: {
                Label("New", systemImage: "plus.bubble")
            }
            .help("Start a new conversation without deleting prior conversations")

            Button {
                model.presentAskSettings()
            } label: {
                Label("Providers", systemImage: "gearshape")
            }
            .help("Configure Claude Code and Codex authentication")
        }
    }

    @ViewBuilder
    private var askSendStatus: some View {
        if let error = model.askError {
            Text(error)
                .font(.caption)
                .foregroundStyle(.red)
                .lineLimit(2)
        } else {
            Text("⌘↩ Send")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        if model.isSendingAsk { ProgressView().controlSize(.small) }
    }

    private var sendButton: some View {
        Button(action: send) {
            Label("Send", systemImage: "paperplane.fill")
        }
        .buttonStyle(.borderedProminent)
        .keyboardShortcut(.return, modifiers: [.command])
        .help("Continue this conversation with the selected provider (⌘↩)")
        .disabled(
            model.isSendingAsk
                || model.askEnabled == false
                || !model.isConnected
                || prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        )
    }

    private func scrollToLatest(_ proxy: ScrollViewProxy) {
        guard let id = conversationEntries.last?.id else { return }
        DispatchQueue.main.async {
            withAnimation(.easeOut(duration: 0.16)) {
                proxy.scrollTo(id, anchor: .bottom)
            }
        }
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

private struct AskComposerEditor: View {
    @Binding var text: String
    let placeholder: String

    var body: some View {
        ZStack(alignment: .topLeading) {
            TextEditor(text: $text)
                .font(.body)
                .scrollContentBackground(.hidden)

            if text.isEmpty {
                Text(placeholder)
                    .font(.body)
                    .foregroundStyle(.tertiary)
                    // NSTextView uses these insets for its first baseline. Keeping
                    // the overlay inside the same padded container also gives it
                    // the exact same wrapping width as the editable text.
                    .padding(.leading, 5)
                    .padding(.trailing, 5)
                    .padding(.top, 8)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
                    .allowsHitTesting(false)
            }
        }
        .padding(7)
    }
}

private struct AskProviderSettingsView: View {
    @ObservedObject var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            VStack(alignment: .leading, spacing: 4) {
                Text("Ask Providers")
                    .font(.title2.weight(.semibold))
                Text("Muxa runs the installed CLIs headlessly. Existing CLI sign-in works unchanged; optional API keys are stored only in the macOS login Keychain.")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
            }

            ForEach(MuxaAskProvider.allCases) { provider in
                AskProviderCredentialRow(provider: provider, model: model)
            }

            if let status = model.askSettingsStatus {
                Label(status, systemImage: "checkmark.circle.fill")
                    .font(.caption)
                    .foregroundStyle(.green)
            }
            if let error = model.askSettingsError {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.red)
                    .textSelection(.enabled)
            }

            HStack {
                Text("API keys apply per Ask without restart. Reload muxad only after installing a CLI in a new PATH; native PTY sessions owned by it will end, while tmux sessions remain.")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                Spacer()
                Button("Reload muxad PATH…") {
                    model.requestDaemonRestartForProviderSettings()
                }
                Button("Done") { model.isPresentingAskSettings = false }
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(width: 660)
    }
}

struct AskProviderCredentialRow: View {
    let provider: MuxaAskProvider
    @ObservedObject var model: AppModel
    @State private var key = ""
    @State private var hasKey = false

    private var executablePath: String? {
        MuxaExecutableResolver.executablePath(provider.executable)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Image(systemName: provider == .claude ? "brain.head.profile" : "chevron.left.forwardslash.chevron.right")
                    .foregroundStyle(.tint)
                    .frame(width: 20)
                Text(provider.title)
                    .font(.headline)
                Text(executablePath == nil ? "CLI not found" : "CLI installed")
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(executablePath == nil ? Color.red : Color.green)
                if hasKey {
                    Text("Keychain API key")
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(.blue)
                } else {
                    Text("CLI sign-in / environment")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Button(provider == .codex ? "Open Login" : "Open Claude Code") {
                    Task { await model.openProviderCLI(provider) }
                }
                .disabled(executablePath == nil)
            }

            if let executablePath {
                Text(executablePath)
                    .font(.caption2.monospaced())
                    .foregroundStyle(.tertiary)
                    .textSelection(.enabled)
            }

            HStack(spacing: 8) {
                SecureField(provider == .claude ? "Anthropic API key" : "OpenAI API key", text: $key)
                    .textFieldStyle(.roundedBorder)
                Button("Save to Keychain") {
                    if model.saveProviderKey(key, provider: provider) {
                        key = ""
                        hasKey = true
                    }
                }
                .disabled(key.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                if hasKey {
                    Button("Remove", role: .destructive) {
                        model.removeProviderKey(provider)
                        hasKey = false
                    }
                }
            }
            Text("Environment: \(provider.environmentKey). The key is never written to muxa config, Ask history, logs, or command arguments.")
                .font(.caption2)
                .foregroundStyle(.tertiary)
        }
        .padding(14)
        .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: 10))
        .onAppear { hasKey = MuxaProviderCredentialStore.hasKey(for: provider) }
    }
}

private struct AskConversationTurn: View {
    let entry: MuxaAskEntry

    private var providerTitle: String {
        entry.agent == "claude" ? "Claude Code" : entry.agent == "codex" ? "Codex" : entry.agent.capitalized
    }

    private var providerIcon: String {
        entry.agent == "claude" ? "brain.head.profile" : "chevron.left.forwardslash.chevron.right"
    }

    private var statusColor: Color {
        switch entry.status {
        case "failed": .red
        case "running": .blue
        default: .green
        }
    }

    private var askedDate: Date? {
        (try? Date(entry.askedAt, strategy: .iso8601))
            ?? ISO8601DateFormatter().date(from: entry.askedAt)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 7) {
                Text(askedDate?.formatted(date: .abbreviated, time: .shortened) ?? compactInboxTimestamp(entry.askedAt))
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.tertiary)
                Spacer()
                Circle().fill(statusColor).frame(width: 6, height: 6)
                Text(entry.status == "running" ? "Thinking" : entry.status.capitalized)
                    .font(.caption.weight(.medium))
                    .foregroundStyle(statusColor)
                if entry.status == "running" { ProgressView().controlSize(.mini) }
            }

            AskMessageBlock(
                role: "You",
                icon: "person.fill",
                source: entry.prompt,
                tint: .accentColor,
                compact: true
            )

            if entry.status == "running", entry.answer.isEmpty {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Waiting for \(providerTitle)…")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                }
                .padding(.horizontal, 4)
            }

            if !entry.answer.isEmpty {
                AskMessageBlock(
                    role: providerTitle,
                    icon: providerIcon,
                    source: entry.answer,
                    tint: .primary,
                    compact: false
                )
            }

            if let error = entry.error, !error.isEmpty {
                Label {
                    Text(error).textSelection(.enabled)
                } icon: {
                    Image(systemName: "exclamationmark.triangle.fill")
                }
                .font(.caption)
                .foregroundStyle(.red)
                .padding(10)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Color.red.opacity(0.07), in: RoundedRectangle(cornerRadius: 7))
            }
        }
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}

private struct AskHistoryCard: View {
    let entry: MuxaAskEntry

    private var providerTitle: String {
        entry.agent == "claude" ? "Claude Code" : entry.agent == "codex" ? "Codex" : entry.agent.capitalized
    }

    private var providerIcon: String {
        entry.agent == "claude" ? "brain.head.profile" : "chevron.left.forwardslash.chevron.right"
    }

    private var statusColor: Color {
        switch entry.status {
        case "failed": .red
        case "running": .blue
        default: .green
        }
    }

    private var statusLabel: String {
        switch entry.status {
        case "running": "Thinking"
        case "failed": "Failed"
        default: "Answered"
        }
    }

    private var askedDate: Date? {
        (try? Date(entry.askedAt, strategy: .iso8601))
            ?? ISO8601DateFormatter().date(from: entry.askedAt)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 8) {
                Image(systemName: providerIcon)
                    .foregroundStyle(.tint)
                Text(providerTitle)
                    .font(.subheadline.weight(.semibold))
                Label(statusLabel, systemImage: entry.status == "failed" ? "exclamationmark.circle.fill" : "circle.fill")
                    .font(.caption2.weight(.semibold))
                    .foregroundStyle(statusColor)
                    .labelStyle(.titleAndIcon)
                if entry.status == "running" {
                    ProgressView()
                        .controlSize(.mini)
                }
                Spacer()
                if let cost = entry.costUSD, cost > 0 {
                    Text(cost, format: .currency(code: "USD"))
                        .font(.caption2.monospacedDigit())
                        .foregroundStyle(.secondary)
                }
                if let askedDate {
                    Text(askedDate, style: .relative)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .help(askedDate.formatted(date: .abbreviated, time: .standard))
                }
            }
            .padding(.horizontal, 14)
            .frame(height: 38)

            Divider()

            VStack(alignment: .leading, spacing: 12) {
                AskMessageBlock(
                    role: "You",
                    icon: "person.fill",
                    source: entry.prompt,
                    tint: .accentColor,
                    compact: true
                )

                if entry.status == "running", entry.answer.isEmpty {
                    HStack(spacing: 8) {
                        ProgressView()
                            .controlSize(.small)
                        Text("Waiting for \(providerTitle)…")
                            .font(.subheadline)
                            .foregroundStyle(.secondary)
                    }
                    .padding(.horizontal, 4)
                }

            if !entry.answer.isEmpty {
                    AskMessageBlock(
                        role: providerTitle,
                        icon: providerIcon,
                        source: entry.answer,
                        tint: .primary,
                        compact: false
                    )
            }

            if let error = entry.error, !error.isEmpty {
                    Label {
                        Text(error)
                            .textSelection(.enabled)
                    } icon: {
                        Image(systemName: "exclamationmark.triangle.fill")
                    }
                    .font(.caption)
                    .foregroundStyle(.red)
                    .padding(10)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(Color.red.opacity(0.07), in: RoundedRectangle(cornerRadius: 7))
                }
            }
            .padding(12)
        }
        .frame(maxWidth: 980, alignment: .leading)
        .background(Color(nsColor: .controlBackgroundColor).opacity(0.76), in: RoundedRectangle(cornerRadius: 10))
        .overlay {
            RoundedRectangle(cornerRadius: 10)
                .stroke(Color.primary.opacity(0.09), lineWidth: 1)
        }
        .frame(maxWidth: .infinity, alignment: .center)
    }
}

private struct AskMessageBlock: View {
    let role: String
    let icon: String
    let source: String
    let tint: Color
    let compact: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
                Label(role, systemImage: icon)
                .font(.caption.weight(.semibold))
                .foregroundStyle(compact ? tint : Color.secondary)

            ReadableMarkdownContent(source: source)
        }
        .padding(12)
        .frame(maxWidth: compact ? 780 : .infinity, alignment: .leading)
        .background(tint.opacity(compact ? 0.075 : 0.035), in: RoundedRectangle(cornerRadius: 8))
        .overlay(alignment: .leading) {
            RoundedRectangle(cornerRadius: 2)
                .fill(tint.opacity(compact ? 0.7 : 0.28))
                .frame(width: 3)
                .padding(.vertical, 8)
        }
        .frame(maxWidth: .infinity, alignment: compact ? .trailing : .leading)
    }
}

struct MuxaOperatorInboxView: View {
    private enum Scope: String, CaseIterable, Identifiable {
        case all = "All"
        case replies = "Replies"
        case waiting = "Waiting"
        case action = "Needs Action"
        case ask = "Ask"
        var id: Self { self }
    }

    @ObservedObject var model: AppModel
    @State private var scope: Scope = .all
    @State private var search = ""
    @State private var selectedMessageID: String?
    @State private var compactShowingDetail = false
    @State private var showingHostFailureDetails = false

    private var visibleMessages: [MuxaOperatorMessage] {
        let filtered = model.operatorMessages.filter { message in
            // Waiting shows only requests the agent can still answer; a
            // blocked/declined/failed request without a reply is an operator
            // decision and lives under Needs Action instead.
            let scopeMatches = switch scope {
            case .all: true
            case .replies: message.request.reply != nil
            case .waiting: message.isAwaitingAgentReply
            case .action: message.needsHumanDecision
            case .ask: false
            }
            guard scopeMatches else { return false }
            guard !search.isEmpty else { return true }
            let request = message.request
            return [
                request.body,
                request.reply?.body,
                request.to.label,
                request.to.windowName,
                request.to.sessionName,
                request.workspaceID,
                request.workID,
                message.host.alias,
            ].compactMap { $0 }.contains {
                $0.localizedCaseInsensitiveContains(search)
            }
        }
        // Needs Action is a queue: unread decisions first, then the most
        // recently changed conversation. Other scopes keep the model's
        // sent-time order so the list does not reorder while reading.
        guard scope == .action else { return filtered }
        return filtered.sorted(by: MuxaOperatorMessage.needsActionOrder)
    }

    private var visibleAsk: [MuxaAskEntry] {
        guard scope == .ask else { return [] }
        return model.askEntries
            .filter { entry in
                search.isEmpty
                    || entry.prompt.localizedCaseInsensitiveContains(search)
                    || entry.answer.localizedCaseInsensitiveContains(search)
            }
            .sorted { $0.askedAt > $1.askedAt }
    }

    private var unreadReplies: Int {
        model.operatorMessages.lazy.filter(\.hasUnreadReply).count
    }

    private var waitingReplies: Int {
        model.operatorMessages.lazy.filter(\.isAwaitingAgentReply).count
    }

    private var humanDecisions: Int {
        model.operatorMessages.lazy.filter(\.needsHumanDecision).count
    }

    private var selectedMessage: MuxaOperatorMessage? {
        visibleMessages.first { $0.id == selectedMessageID }
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                Label("Operator Inbox", systemImage: "tray.full")
                    .font(.headline)
                inboxMetric("New", unreadReplies, color: .orange)
                inboxMetric("Waiting", waitingReplies, color: .blue)
                inboxMetric("Action", humanDecisions, color: .red)
                Spacer(minLength: 8)
                if model.isRefreshingInbox { ProgressView().controlSize(.small) }
                Button {
                    Task { await model.refreshOperatorInbox(force: true) }
                } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
            }
            .padding(.horizontal, 14)
            .frame(height: 42)

            Divider()

            ViewThatFits(in: .horizontal) {
                HStack(spacing: 10) {
                    inboxScopePicker.frame(maxWidth: 420)
                    inboxSearchField
                }
                VStack(spacing: 8) {
                    inboxScopePicker
                    inboxSearchField
                }
            }
            .padding(10)

            if let summary = model.inboxHostFailureSummary {
                inboxHostFailureLine(summary)
            }

            if let error = model.inboxError {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.orange)
                    .textSelection(.enabled)
                    .padding(.horizontal, 12)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }

            if scope == .ask {
                askHistory
            } else {
                operatorMasterDetail
            }
        }
        .task {
            await model.refreshOperatorInbox(force: true)
            reconcileMessageSelection()
        }
        .onChange(of: visibleMessages.map(\.id)) { _ in reconcileMessageSelection() }
        .onChange(of: scope) { _ in
            compactShowingDetail = false
            reconcileMessageSelection()
        }
    }

    /// One compact advisory line for hosts whose most recent mailbox read
    /// failed. The list below still shows the last messages received from
    /// them, so this never replaces the list. The full per-host reasons are
    /// available as a tooltip and behind the chevron.
    private func inboxHostFailureLine(_ summary: String) -> some View {
        let details = MuxaInboxHostFailureText.details(model.inboxHostFailures)
        return VStack(alignment: .leading, spacing: 4) {
            Button {
                showingHostFailureDetails.toggle()
            } label: {
                HStack(spacing: 6) {
                    Label(summary, systemImage: "wifi.exclamationmark")
                        .font(.caption)
                        .foregroundStyle(.orange)
                        .lineLimit(1)
                    Image(systemName: showingHostFailureDetails ? "chevron.down" : "chevron.right")
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(.secondary)
                    Text("Showing their last known messages")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help(details.joined(separator: "\n"))
            .accessibilityLabel("Unreachable hosts")
            .accessibilityValue(summary)

            if showingHostFailureDetails {
                ForEach(details, id: \.self) { line in
                    Text(line)
                        .font(.caption.monospaced())
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .lineLimit(2)
                        .padding(.leading, 22)
                }
            }
        }
        .padding(.horizontal, 12)
        .padding(.bottom, 6)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func inboxMetric(_ label: String, _ value: Int, color: Color) -> some View {
        Text("\(value) \(label)")
            .font(.caption2.weight(.semibold).monospacedDigit())
            .foregroundStyle(value > 0 ? color : Color.secondary)
            .padding(.horizontal, 7)
            .padding(.vertical, 3)
            .background((value > 0 ? color : Color.secondary).opacity(0.1), in: Capsule())
    }

    private var askHistory: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 12) {
                VStack(alignment: .leading, spacing: 3) {
                    Text("Global Ask").font(.title3.weight(.semibold))
                    Text("Headless Claude Code and Codex questions owned by muxad")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                if visibleAsk.isEmpty {
                    Text("No matching Ask history")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .padding(.vertical, 18)
                } else {
                    ForEach(visibleAsk) { entry in AskHistoryCard(entry: entry) }
                }
            }
            .padding(14)
            .frame(maxWidth: 1000, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .topLeading)
        }
    }

    private var operatorMasterDetail: some View {
        GeometryReader { geometry in
            if geometry.size.width >= 760 {
                HSplitView {
                    operatorMessageList
                        .frame(minWidth: 300, idealWidth: 380, maxWidth: 470)
                    operatorMessageDetail
                        .frame(minWidth: 420, maxWidth: .infinity, maxHeight: .infinity)
                }
            } else if compactShowingDetail, selectedMessage != nil {
                VStack(spacing: 0) {
                    HStack {
                        Button {
                            compactShowingDetail = false
                        } label: {
                            Label("Inbox", systemImage: "chevron.left")
                        }
                        .buttonStyle(.borderless)
                        Spacer()
                    }
                    .padding(.horizontal, 12)
                    .frame(height: 36)
                    Divider()
                    operatorMessageDetail
                }
            } else {
                operatorMessageList
            }
        }
    }

    private var operatorMessageList: some View {
        VStack(spacing: 0) {
            HStack {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Commands and replies")
                        .font(.subheadline.weight(.semibold))
                    Text("\(visibleMessages.count) conversations across reachable hosts")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 10)
            Divider()

            if visibleMessages.isEmpty {
                ConsoleUnavailableView(
                    title: model.operatorMessages.isEmpty ? "No commands sent yet" : "No matching commands",
                    systemImage: "paperplane",
                    description: "Use Collaborate on an agent pane. Its reply will appear here without reopening that pane."
                )
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ScrollView {
                    LazyVStack(spacing: 6) {
                        ForEach(visibleMessages) { message in
                            Button {
                                selectedMessageID = message.id
                                compactShowingDetail = true
                                if message.hasUnreadReply {
                                    Task { await model.markOperatorMessageRead(message) }
                                }
                            } label: {
                                OperatorMessageRow(
                                    message: message,
                                    selected: message.id == selectedMessageID
                                )
                            }
                            .buttonStyle(.plain)
                            .contentShape(Rectangle())
                        }
                    }
                    .padding(8)
                }
            }
        }
    }

    @ViewBuilder
    private var operatorMessageDetail: some View {
        if let selectedMessage {
            OperatorMessageDetail(message: selectedMessage, model: model)
        } else {
            ConsoleUnavailableView(
                title: "Select a message",
                systemImage: "rectangle.righthalf.inset.filled",
                description: "Choose a compact preview to read the full sent message and reply."
            )
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    private func reconcileMessageSelection() {
        if let selectedMessageID,
           visibleMessages.contains(where: { $0.id == selectedMessageID }) {
            return
        }
        selectedMessageID = visibleMessages.first?.id
    }

    private var inboxScopePicker: some View {
        Picker("Mailbox", selection: $scope) {
            ForEach(Scope.allCases) { value in Text(value.rawValue).tag(value) }
        }
        .pickerStyle(.segmented)
        .labelsHidden()
        .accessibilityLabel("Inbox scope")
    }

    private var inboxSearchField: some View {
        HStack(spacing: 6) {
            Image(systemName: "magnifyingglass")
                .foregroundStyle(.secondary)
            TextField("Search command, reply, Work, or agent", text: $search)
                .textFieldStyle(.plain)
        }
        .padding(.horizontal, 9)
        .padding(.vertical, 6)
        .background(Color.primary.opacity(0.055), in: RoundedRectangle(cornerRadius: 7))
    }
}

private struct OperatorMessageRow: View {
    let message: MuxaOperatorMessage
    let selected: Bool

    private var request: MuxaCollaborationRequest { message.request }
    private var statusColor: Color {
        if request.reply?.status == "completed" { return .green }
        if ["blocked", "failed", "declined", "expired", "cancelled"].contains(request.status) {
            return .red
        }
        if request.reply != nil { return .green }
        return request.expectsReply ? .blue : .secondary
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 7) {
                Circle().fill(statusColor).frame(width: 7, height: 7)
                Text(request.to.label)
                    .font(.subheadline.weight(.semibold))
                    .lineLimit(1)
                Spacer(minLength: 6)
                Text(request.reply?.status.replacingOccurrences(of: "_", with: " ").capitalized
                    ?? request.status.replacingOccurrences(of: "_", with: " ").capitalized)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(statusColor)
                Text(compactInboxTimestamp(request.createdAt))
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
            }

            HStack(spacing: 8) {
                Label(message.host.alias, systemImage: "server.rack")
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                if let work = request.workID {
                    Text([request.workspaceID, work].compactMap { $0 }.joined(separator: " / "))
                        .font(.caption.weight(.medium))
                        .foregroundStyle(Color.accentColor)
                        .lineLimit(1)
                }
            }

            VStack(alignment: .leading, spacing: 6) {
                Label("Sent", systemImage: "paperplane.fill")
                    .font(.subheadline.weight(.semibold))
                    .foregroundStyle(.secondary)
                Text(inboxPreview(request.body))
                    .font(.callout)
                    .foregroundStyle(.primary)
                    .lineLimit(2)
                    .multilineTextAlignment(.leading)
            }

            if let reply = request.reply {
                VStack(alignment: .leading, spacing: 7) {
                    HStack {
                        Label(message.hasUnreadReply ? "New Reply" : "Reply", systemImage: "arrowshape.turn.up.left.fill")
                            .font(.subheadline.weight(.semibold))
                            .foregroundStyle(message.hasUnreadReply ? Color.orange : Color.secondary)
                        Spacer()
                        Text(compactInboxTimestamp(reply.at))
                            .font(.caption.monospacedDigit())
                            .foregroundStyle(.tertiary)
                    }
                    Text(inboxPreview(reply.body))
                        .font(.callout)
                        .foregroundStyle(.primary)
                        .lineLimit(2)
                        .multilineTextAlignment(.leading)
                }
                .padding(11)
                .background(
                    (message.hasUnreadReply ? Color.orange : Color.primary).opacity(0.06),
                    in: RoundedRectangle(cornerRadius: 8)
                )
            } else if message.needsHumanDecision {
                // The request itself is blocked/declined/failed and no reply
                // will arrive, so name the decision instead of a wait.
                Label(
                    "Needs your decision: \(inboxStatusTitle(request.status))",
                    systemImage: "exclamationmark.triangle.fill"
                )
                .font(.callout)
                .foregroundStyle(.red)
            } else if message.isAwaitingAgentReply {
                Label("Waiting for this agent to reply", systemImage: "clock")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(13)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            selected ? Color.accentColor.opacity(0.12) : Color.primary.opacity(0.035),
            in: RoundedRectangle(cornerRadius: 9)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 9)
                .stroke(
                    selected
                        ? Color.accentColor.opacity(0.65)
                        : message.hasUnreadReply
                            ? Color.orange.opacity(0.5)
                            : Color(nsColor: .separatorColor).opacity(0.4),
                    lineWidth: selected || message.hasUnreadReply ? 1 : 0.5
                )
        }
        .contentShape(RoundedRectangle(cornerRadius: 9))
    }
}

private struct OperatorMessageDetail: View {
    let message: MuxaOperatorMessage
    @ObservedObject var model: AppModel

    private var request: MuxaCollaborationRequest { message.request }
    private var openDestination: MuxaSidebarSelection? {
        AppModel.operatorSelection(for: message, in: model.executionSnapshot)
    }
    private var openDestinationLabel: String {
        switch openDestination {
        case .agent, .pane: "Open Agent"
        case .fleetWindow: "Open Window"
        case .fleetSession: "Open Session"
        case .host: "Open Host"
        case nil: "Agent Ended"
        default: "Open Context"
        }
    }
    private var statusColor: Color {
        if request.reply?.status == "completed" { return .green }
        if ["blocked", "failed", "declined", "expired", "cancelled"].contains(request.status) {
            return .red
        }
        if request.reply != nil { return .green }
        return request.expectsReply ? .blue : .secondary
    }

    private var location: String {
        [request.to.sessionName, request.to.windowName, request.to.pane]
            .compactMap { $0?.isEmpty == false ? $0 : nil }
            .joined(separator: " › ")
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 9) {
                Circle().fill(statusColor).frame(width: 8, height: 8)
                VStack(alignment: .leading, spacing: 2) {
                    Text(request.to.label)
                        .font(.headline)
                        .lineLimit(1)
                    Text("\(message.host.alias) · \(compactInboxTimestamp(request.createdAt))")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                if message.hasUnreadReply {
                    Button("Mark Read") {
                        Task { await model.markOperatorMessageRead(message) }
                    }
                }
                Button {
                    model.openOperatorMessage(message)
                } label: {
                    Label(openDestinationLabel, systemImage: "rectangle.and.hand.point.up.left")
                }
                .buttonStyle(.borderedProminent)
                .disabled(openDestination == nil)
            }
            .padding(.horizontal, 16)
            .frame(minHeight: 54)

            Divider()

            ScrollView {
                VStack(alignment: .leading, spacing: 20) {
                    HStack(spacing: 10) {
                        Label(message.host.alias, systemImage: "server.rack")
                        if let work = request.workID {
                            Label(
                                [request.workspaceID, work].compactMap { $0 }.joined(separator: " / "),
                                systemImage: "briefcase"
                            )
                        }
                        if !location.isEmpty {
                            Label(location, systemImage: "rectangle.split.3x1")
                                .lineLimit(1)
                        }
                    }
                    .font(.caption)
                    .foregroundStyle(.secondary)

                    OperatorMessageDetailSection(
                        title: "Sent",
                        icon: "paperplane.fill",
                        timestamp: compactInboxTimestamp(request.createdAt),
                        source: request.body,
                        tint: .accentColor
                    )

                    if let reply = request.reply {
                        OperatorMessageDetailSection(
                            title: message.hasUnreadReply ? "New Reply" : "Reply",
                            icon: "arrowshape.turn.up.left.fill",
                            timestamp: compactInboxTimestamp(reply.at),
                            source: reply.body,
                            tint: message.hasUnreadReply ? .orange : .green
                        )
                    } else if message.needsHumanDecision {
                        Label(
                            "Needs your decision: \(inboxStatusTitle(request.status)). The agent will not reply to this request.",
                            systemImage: "exclamationmark.triangle.fill"
                        )
                        .font(.subheadline.weight(.medium))
                        .foregroundStyle(.red)
                        .padding(14)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(Color.red.opacity(0.06), in: RoundedRectangle(cornerRadius: 10))
                    } else if message.isAwaitingAgentReply {
                        Label("Waiting for this agent to reply", systemImage: "clock")
                            .font(.subheadline.weight(.medium))
                            .foregroundStyle(.secondary)
                            .padding(14)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .background(Color.primary.opacity(0.04), in: RoundedRectangle(cornerRadius: 10))
                    }
                }
                .padding(18)
                .frame(maxWidth: 860, alignment: .leading)
                .frame(maxWidth: .infinity, alignment: .topLeading)
            }
        }
        .background(Color(nsColor: .textBackgroundColor).opacity(0.28))
    }
}

private struct OperatorMessageDetailSection: View {
    let title: String
    let icon: String
    let timestamp: String
    let source: String
    let tint: Color

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Label(title, systemImage: icon)
                    .font(.title3.weight(.semibold))
                    .foregroundStyle(tint)
                Spacer()
                Text(timestamp)
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.tertiary)
            }
            ReadableMarkdownContent(source: source)
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(tint.opacity(0.055), in: RoundedRectangle(cornerRadius: 11))
        .overlay(alignment: .leading) {
            RoundedRectangle(cornerRadius: 2)
                .fill(tint.opacity(0.7))
                .frame(width: 3)
                .padding(.vertical, 10)
        }
    }
}

private func inboxPreview(_ value: String) -> String {
    MuxaMarkdownText.previewText(markdown: value)
}

/// "waiting_reply" -> "Waiting Reply", matching the status pill wording.
private func inboxStatusTitle(_ status: String) -> String {
    status.replacingOccurrences(of: "_", with: " ").capitalized
}

private func compactInboxTimestamp(_ value: String) -> String {
    let normalized = value.replacingOccurrences(of: "T", with: " ")
    return String(normalized.prefix(16))
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
    let mailboxRevision: UInt64?
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
        .task(id: "\(pane.id)-\(mailboxRevision ?? 0)") { await load() }
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
                        .font(.title2.weight(.semibold))
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
                Text(attachedSessionID == nil ? "Read-only" : "Fitted · Interactive")
                    .font(.caption2.weight(.medium))
                    .foregroundStyle(attachedSessionID == nil ? Color.secondary : Color.green)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(Color.primary.opacity(0.07), in: Capsule())
                    .help(
                        attachedSessionID == nil
                            ? "The preview preserves the pane's current tmux dimensions"
                            : "This pane is temporarily zoomed and follows the Live Pane size; tmux settings are restored on detach"
                    )
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
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .clipped()
            } else {
                PaneCaptureView(
                    client: model.client,
                    target: MuxaPaneTarget(host: pane.host, pane: pane.pane),
                    showsHeader: false
                )
                .id(pane.id)
                .help(canAttach ? "Use Click to Type to enter interactive mode" : "Read-only pane preview")
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
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .clipped()
        .overlay {
            Rectangle()
                .stroke(Color(nsColor: .separatorColor).opacity(0.7), lineWidth: 0.5)
        }
    }
}

/// How strongly one Explore tree row is highlighted. Only the row that matches
/// the active editor (`sidebarSelection`) is `selected`; the path down to the
/// followed pane (`watchSelection`) is a lighter `followed` marker.
enum WatchTreeHighlight: Equatable {
    case selected
    case followed
    case idle
}

/// The two selections the Explore tree reflects, and the rule for combining
/// them so that a row never looks selected because of a stale pane choice.
struct WatchTreeSelection: Equatable {
    let editor: MuxaSidebarSelection?
    let followedPane: MuxaWatchPaneIdentity?

    /// The followed pane path is only meaningful while the active editor
    /// follows that pane: the Live Watch tool or the pane's own editor.
    var showsFollowedPath: Bool {
        switch editor {
        case .watch: true
        case .pane(let id): id == followedPane
        default: false
        }
    }

    func highlight(
        for row: MuxaSidebarSelection,
        containsFollowedPane: Bool
    ) -> WatchTreeHighlight {
        if editor == row { return .selected }
        if showsFollowedPath, containsFollowedPane { return .followed }
        return .idle
    }
}

/// Explore row fill: a strong accent for the active editor row, a lighter
/// tint for the followed pane path, otherwise `idle`.
private func watchHighlightFill(
    _ highlight: WatchTreeHighlight,
    selected: Double = 0.18,
    idle: Color = .clear
) -> Color {
    switch highlight {
    case .selected: Color.accentColor.opacity(selected)
    case .followed: Color.accentColor.opacity(0.06)
    case .idle: idle
    }
}

/// Thin leading accent bar that marks the followed pane without reading as a
/// selection. It never participates in hit testing.
private struct WatchFollowedMarker: View {
    let highlight: WatchTreeHighlight

    var body: some View {
        if highlight == .followed {
            Capsule()
                .fill(Color.accentColor.opacity(0.65))
                .frame(width: 3)
                .padding(.vertical, 9)
                .allowsHitTesting(false)
        }
    }
}

struct WatchHostTree: View {
    let group: MuxaWatchHost
    let selection: WatchTreeSelection
    let selectHost: (String) -> Void
    let selectSession: (MuxaWatchSessionIdentity) -> Void
    let openPinnedSession: (MuxaWatchSessionIdentity) -> Void
    let selectPane: (MuxaWatchPaneIdentity) -> Void
    let openPinnedPane: (MuxaWatchPaneIdentity) -> Void
    let forceExpanded: Bool
    let workLabel: (MuxaWatchWindow) -> String?
    @State private var manualExpansion: Bool?

    private var containsSelection: Bool {
        selection.followedPane.map { selected in
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
            HStack(spacing: 0) {
                Button { manualExpansion = !expanded } label: {
                    hierarchyChevron(expanded)
                        .frame(width: 30, height: 36)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)

                Button { selectHost(group.host.alias) } label: {
                    HStack(spacing: 6) {
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
                    .padding(.horizontal, 7)
                    .frame(maxWidth: .infinity, minHeight: 36, alignment: .leading)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .simultaneousGesture(
                    TapGesture(count: 2).onEnded { manualExpansion = !expanded }
                )
            }
            .background(
                watchHighlightFill(
                    selection.highlight(
                        for: .host(group.host.alias),
                        containsFollowedPane: containsSelection
                    ),
                    selected: 0.14,
                    idle: expanded ? Color.primary.opacity(0.035) : Color.clear
                )
            )

            if expanded {
                ForEach(group.sessions) { session in
                    WatchSessionTree(
                        session: session,
                        selection: selection,
                        selectSession: selectSession,
                        openPinnedSession: openPinnedSession,
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
    let selection: WatchTreeSelection
    let selectSession: (MuxaWatchSessionIdentity) -> Void
    let openPinnedSession: (MuxaWatchSessionIdentity) -> Void
    let selectPane: (MuxaWatchPaneIdentity) -> Void
    let openPinnedPane: (MuxaWatchPaneIdentity) -> Void
    let forceExpanded: Bool
    let workLabel: (MuxaWatchWindow) -> String?
    @State private var manualExpansion: Bool?

    private var selectedPath: Bool {
        selection.followedPane.map { selected in
            session.windows.contains { window in
                window.panes.contains { $0.id == selected }
            }
        } ?? false
    }

    private var highlight: WatchTreeHighlight {
        selection.highlight(
            for: .fleetSession(session.identity),
            containsFollowedPane: selectedPath
        )
    }

    private var expanded: Bool {
        forceExpanded || (manualExpansion ?? (singleWindow != nil || selectedPath))
    }

    private var singleWindow: MuxaWatchWindow? {
        session.windows.count == 1 ? session.windows.first : nil
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 0) {
                explorerIndent(depth: 1)
                Button { manualExpansion = !expanded } label: {
                    hierarchyChevron(expanded)
                        .frame(width: 30, height: 34)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                Button {
                    selectSession(session.identity)
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
                    .padding(.trailing, 7)
                    .frame(maxWidth: .infinity, minHeight: 34, alignment: .leading)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .simultaneousGesture(
                    TapGesture(count: 2).onEnded {
                        openPinnedSession(session.identity)
                    }
                )
            }
            .background(watchHighlightFill(highlight))

            if expanded {
                if let singleWindow {
                    ForEach(singleWindow.panes) { pane in
                        WatchPaneRow(
                            pane: pane,
                            highlight: selection.highlight(
                                for: .pane(pane.id),
                                containsFollowedPane: selection.followedPane == pane.id
                            ),
                            depth: 2,
                            selectPane: selectPane,
                            openPinnedPane: openPinnedPane
                        )
                    }
                } else {
                    ForEach(session.windows) { window in
                        WatchWindowTree(
                            window: window,
                            selection: selection,
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
    let selection: WatchTreeSelection
    let selectPane: (MuxaWatchPaneIdentity) -> Void
    let openPinnedPane: (MuxaWatchPaneIdentity) -> Void
    let forceExpanded: Bool
    let logicalWork: String?
    @State private var manualExpansion: Bool?

    private var containsSelection: Bool {
        selection.followedPane.map { selected in window.panes.contains { $0.id == selected } } ?? false
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
                        .frame(width: 30, height: 34)
                        .contentShape(Rectangle())
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
                    .padding(.trailing, 7)
                    .frame(maxWidth: .infinity, minHeight: 34, alignment: .leading)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .simultaneousGesture(
                    TapGesture(count: 2).onEnded {
                        if let pane = window.panes.first { openPinnedPane(pane.id) }
                    }
                )
            }
            .background(
                watchHighlightFill(
                    selection.highlight(
                        for: .fleetWindow(window.identity),
                        containsFollowedPane: containsSelection
                    )
                )
            )

            if expanded {
                ForEach(window.panes) { pane in
                    WatchPaneRow(
                        pane: pane,
                        highlight: selection.highlight(
                            for: .pane(pane.id),
                            containsFollowedPane: selection.followedPane == pane.id
                        ),
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
    let highlight: WatchTreeHighlight
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
                Color.clear.frame(width: 30)
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
                .padding(.trailing, 8)
            }
            .frame(maxWidth: .infinity, minHeight: 34, alignment: .leading)
            .background(watchHighlightFill(highlight))
            .overlay(alignment: .leading) { WatchFollowedMarker(highlight: highlight) }
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

/// A globally sortable Explore row. Unlike the topology tree it carries its
/// host and tmux path in the row, so removing host grouping does not remove the
/// context needed to identify the pane.
struct WatchFlatPaneRow: View {
    let pane: MuxaWatchPane
    let highlight: WatchTreeHighlight
    let selectPane: (MuxaWatchPaneIdentity) -> Void
    let openPinnedPane: (MuxaWatchPaneIdentity) -> Void

    private var title: String {
        pane.pane.agentAlias.map { "@\($0)" }
            ?? pane.agent?.aiTitle
            ?? pane.pane.title.nonEmpty
            ?? pane.pane.currentCommand.nonEmpty
            ?? pane.pane.paneID
    }

    private var location: String {
        let window = pane.pane.windowName.nonEmpty ?? pane.pane.stableWindowID
        return "\(pane.host.alias) · \(pane.pane.session) › \(window)"
    }

    var body: some View {
        Button { selectPane(pane.id) } label: {
            HStack(alignment: .top, spacing: 8) {
                HostIdentityBadge(identity: pane.host, size: 22)
                    .padding(.top, 1)
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        Circle()
                            .fill(pane.agent.map { agentStateColor($0.state) } ?? Color.secondary)
                            .frame(width: 6, height: 6)
                        Text(title)
                            .font(.callout.weight(.medium))
                            .lineLimit(1)
                        Spacer(minLength: 3)
                        Text(pane.pane.paneID)
                            .font(.caption2.monospaced())
                            .foregroundStyle(.tertiary)
                    }
                    Text(location)
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                    if let agent = pane.agent {
                        Text(agentStateLabel(agent.state))
                            .font(.caption2.weight(.medium))
                            .foregroundStyle(agentStateColor(agent.state))
                    } else {
                        Text(pane.pane.currentCommand.nonEmpty ?? "Shell")
                            .font(.caption2)
                            .foregroundStyle(.tertiary)
                    }
                }
            }
            .padding(.horizontal, 7)
            .padding(.vertical, 7)
            .frame(maxWidth: .infinity, minHeight: 48, alignment: .leading)
            .background(
                watchHighlightFill(highlight),
                in: RoundedRectangle(cornerRadius: 6)
            )
            .overlay(alignment: .leading) { WatchFollowedMarker(highlight: highlight) }
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
    .frame(width: CGFloat(depth) * 14, height: 34, alignment: .trailing)
}

private func hierarchyChevron(_ expanded: Bool) -> some View {
    Image(systemName: "chevron.right")
        .font(.caption2.weight(.semibold))
        .foregroundStyle(.secondary)
        .rotationEffect(.degrees(expanded ? 90 : 0))
        .frame(width: 12, height: 20)
}

private struct FleetPaneInspector: View {
    let pane: MuxaWatchPane
    @ObservedObject var model: AppModel
    let compact: Bool
    let openInShell: () -> Void
    @State private var showsMetadata = false
    @State private var agentsExpanded = true

    private var window: MuxaWatchWindow? {
        model.executionSnapshot.watchWindow(containing: pane.id)
    }

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

                selectedPaneOverview

                if let window {
                    windowOverview(window)
                }

                if let error = model.attachError {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .font(.caption)
                        .foregroundStyle(.red)
                        .textSelection(.enabled)
                }

            }
            .padding(compact ? 12 : 18)
            .frame(maxWidth: 1100, alignment: .leading)
            .frame(maxWidth: .infinity, alignment: .topLeading)
        }
    }

    @ViewBuilder
    private func windowOverview(_ window: MuxaWatchWindow) -> some View {
        DisclosureGroup(isExpanded: $agentsExpanded) {
            VStack(spacing: 1) {
                ForEach(window.panes) { item in
                    Button {
                        model.selectWatchPane(item.id)
                    } label: {
                        HStack(alignment: .top, spacing: 9) {
                            Circle()
                                .fill(item.agent.map { agentStateColor($0.state) } ?? Color.secondary)
                                .frame(width: 7, height: 7)
                                .padding(.top, 5)
                            VStack(alignment: .leading, spacing: 2) {
                                HStack(spacing: 6) {
                                    Text(overviewTitle(item))
                                        .font(.subheadline.weight(item.id == pane.id ? .semibold : .regular))
                                    if let agent = item.agent {
                                        Text(agentStateLabel(agent.state))
                                            .font(.caption2.weight(.medium))
                                            .foregroundStyle(agentStateColor(agent.state))
                                    }
                                    Spacer(minLength: 4)
                                    Text(item.pane.paneID)
                                        .font(.caption2.monospaced())
                                        .foregroundStyle(.tertiary)
                                }
                                Text(overviewSummary(item) ?? "No task summary has been reported for this pane yet.")
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                                    .lineLimit(2)
                                    .multilineTextAlignment(.leading)
                            }
                        }
                        .padding(.horizontal, 10)
                        .padding(.vertical, 8)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(
                            item.id == pane.id ? Color.accentColor.opacity(0.1) : Color.primary.opacity(0.025),
                            in: RoundedRectangle(cornerRadius: 7)
                        )
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                }
            }
            .padding(.top, 9)
        } label: {
            HStack(spacing: 8) {
                Label("Agents in this window", systemImage: "person.2")
                    .font(.headline)
                if let identity = windowWorkIdentity(window) {
                    Text("\(identity.workspaceID) / \(identity.workID)")
                        .font(.caption.weight(.medium))
                        .foregroundStyle(Color.accentColor)
                        .padding(.horizontal, 7)
                        .padding(.vertical, 3)
                        .background(Color.accentColor.opacity(0.1), in: Capsule())
                }
                Spacer(minLength: 6)
                Text("\(window.panes.count) panes · \(window.panes.compactMap(\.agent).count) agents")
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
        }
        .padding(12)
        .background(Color.primary.opacity(0.025), in: RoundedRectangle(cornerRadius: 10))
        .overlay {
            RoundedRectangle(cornerRadius: 10)
                .stroke(Color(nsColor: .separatorColor).opacity(0.4), lineWidth: 0.5)
        }
    }

    @ViewBuilder
    private var selectedPaneOverview: some View {
        if let agent = pane.agent {
            VStack(alignment: .leading, spacing: 10) {
                HStack(spacing: 8) {
                    Label("Current context", systemImage: "text.alignleft")
                        .font(.headline.weight(.semibold))
                    agentStatus(agent)
                }
                .font(.caption)

                if let summary = agent.recap?.nonEmpty,
                   let response = agent.lastResponse?.nonEmpty,
                   response != summary {
                    ViewThatFits(in: .horizontal) {
                        HStack(alignment: .top, spacing: 12) {
                            overviewSection(
                                "Summary",
                                systemImage: "list.bullet.rectangle",
                                summary,
                                lineLimit: compact ? 5 : 8
                            )
                            .frame(maxWidth: .infinity, alignment: .topLeading)
                            overviewSection(
                                "Latest response",
                                systemImage: "text.bubble",
                                response,
                                lineLimit: compact ? 6 : 12
                            )
                            .frame(maxWidth: .infinity, alignment: .topLeading)
                        }
                        VStack(alignment: .leading, spacing: 10) {
                            overviewSection(
                                "Summary",
                                systemImage: "list.bullet.rectangle",
                                summary,
                                lineLimit: compact ? 5 : 8
                            )
                            overviewSection(
                                "Latest response",
                                systemImage: "text.bubble",
                                response,
                                lineLimit: compact ? 6 : 12
                            )
                        }
                    }
                } else if let summary = agent.recap?.nonEmpty {
                    overviewSection(
                        "Summary",
                        systemImage: "list.bullet.rectangle",
                        summary,
                        lineLimit: compact ? 5 : 9
                    )
                } else if let response = agent.lastResponse?.nonEmpty {
                    overviewSection(
                        "Latest response",
                        systemImage: "text.bubble",
                        response,
                        lineLimit: compact ? 6 : 12
                    )
                }

                if let activity = latestActivity(agent) {
                    overviewSection(
                        activity.title,
                        systemImage: activity.systemImage,
                        activity.source,
                        lineLimit: compact ? 4 : 7
                    )
                }
            }
        } else {
            Label(
                "No agent session is currently associated with this pane.",
                systemImage: "person.crop.circle.badge.questionmark"
            )
            .font(.caption)
            .foregroundStyle(.secondary)
        }
    }

    private func overviewSection(
        _ title: String,
        systemImage: String,
        _ source: String,
        lineLimit: Int
    ) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            HStack(spacing: 6) {
                Label(title, systemImage: systemImage)
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(.secondary)
                Spacer(minLength: 4)
                Button {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(source, forType: .string)
                } label: {
                    Image(systemName: "doc.on.doc")
                        .frame(width: 24, height: 24)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help("Copy \(title.lowercased())")
            }
            MarkdownContent(source: source, lineLimit: lineLimit, selectable: false)
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: 8))
    }

    private func latestActivity(_ agent: MuxaAgent) -> (title: String, systemImage: String, source: String)? {
        if let notice = agent.lastNotification?.nonEmpty,
           notice != agent.recap,
           notice != agent.lastResponse {
            return ("Latest notice", "bell", notice)
        }
        if agent.recap == nil,
           agent.lastResponse == nil,
           let prompt = agent.lastPrompt?.nonEmpty {
            return ("Latest activity", "clock.arrow.circlepath", humanReadablePrompt(prompt))
        }
        return nil
    }

    private func metadataRow(_ label: String, _ value: String) -> some View {
        GridRow {
            Text(label)
                .foregroundStyle(.secondary)
            Text(value)
                .fontDesign(.monospaced)
        }
    }

    private func windowWorkIdentity(_ window: MuxaWatchWindow) -> MuxaWorkIdentity? {
        let identities = Set(window.panes.compactMap(\.pane.workIdentity))
        return identities.count == 1 ? identities.first : nil
    }

    private func overviewTitle(_ item: MuxaWatchPane) -> String {
        item.pane.agentAlias.map { "@\($0)" }
            ?? item.agent?.aiTitle?.nonEmpty
            ?? item.pane.title.nonEmpty
            ?? item.pane.currentCommand.nonEmpty
            ?? item.pane.paneID
    }

    private func overviewSummary(_ item: MuxaWatchPane) -> String? {
        guard let agent = item.agent else { return item.pane.currentPath.nonEmpty }
        return agent.recap?.nonEmpty
            ?? agent.lastResponse?.nonEmpty
            ?? agent.lastNotification?.nonEmpty
            ?? agent.lastPrompt?.nonEmpty.map(humanReadablePrompt)
    }

    private func humanReadablePrompt(_ prompt: String) -> String {
        if prompt.hasPrefix("[muxa:req_") {
            if prompt.contains("Completed reply") {
                return "A collaborator reply is ready for this agent."
            }
            if prompt.contains("New ") && prompt.contains(" request") {
                return "A collaboration request is waiting for this agent."
            }
            return "Recent Muxa collaboration activity."
        }
        if prompt.hasPrefix("<task-notification>") {
            return "A background task reported an update."
        }
        return prompt
    }

    private var inspectorIdentity: some View {
        HStack(alignment: .top, spacing: 10) {
            HostIdentityBadge(identity: pane.host, size: 36)
            VStack(alignment: .leading, spacing: 2) {
                Text(pane.agent?.aiTitle ?? pane.pane.agentAlias.map { "@\($0)" } ?? pane.pane.title.nonEmpty ?? pane.pane.currentCommand)
                    .font(.title2.weight(.semibold))
                    .lineLimit(2)
                HStack(spacing: 7) {
                    Text(pane.host.alias)
                    if let agent = pane.agent {
                        Text("·")
                        Text(agentStateLabel(agent.state))
                            .foregroundStyle(agentStateColor(agent.state))
                    }
                    if let identity = pane.pane.workIdentity {
                        Text("·")
                        Text("\(identity.workspaceID) / \(identity.workID)")
                            .foregroundStyle(Color.accentColor)
                    }
                }
                .font(.caption.weight(.medium))
                .foregroundStyle(.secondary)
                .lineLimit(1)
            }
        }
    }

    private var inspectorActions: some View {
        HStack(spacing: 8) {
            metadataButton(label: "Details")
            Button {
                openInShell()
            } label: {
                Label("Open in Shell", systemImage: "rectangle.on.rectangle")
            }
        }
    }

    private var compactInspectorActions: some View {
        HStack(spacing: 8) {
            metadataButton(label: "Info")
            Button {
                openInShell()
            } label: {
                Label("Shell", systemImage: "rectangle.on.rectangle")
            }
        }
    }

    private func metadataButton(label: String) -> some View {
        Button {
            showsMetadata.toggle()
        } label: {
            Label(label, systemImage: "info.circle")
        }
        .popover(isPresented: $showsMetadata, arrowEdge: .bottom) {
            VStack(alignment: .leading, spacing: 12) {
                Text("Execution location")
                    .font(.headline)
                Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 8) {
                    metadataRow("Host", pane.host.alias)
                    metadataRow("Session", pane.pane.session.nonEmpty ?? pane.pane.stableSessionID)
                    metadataRow("Window", pane.pane.windowName.nonEmpty ?? pane.pane.stableWindowID)
                    metadataRow("Pane", "\(pane.pane.paneID) · \(pane.pane.currentCommand)")
                    if let path = pane.pane.currentPath.nonEmpty {
                        metadataRow("Directory", path)
                    }
                }
                .font(.caption)
                .textSelection(.enabled)
            }
            .padding(16)
            .frame(minWidth: 420, maxWidth: 560, alignment: .leading)
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
                moduleMissing("This pane is no longer available")
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
