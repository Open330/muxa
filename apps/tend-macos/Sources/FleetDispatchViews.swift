import SwiftUI

struct FleetDispatchView: View {
    @ObservedObject var model: AppModel
    @ObservedObject private var store = TendDispatchStore.shared
    @Environment(\.dismiss) private var dismiss
    @State private var options: TendOrchestrationSettings?
    @State private var supported = false
    @State private var lookupID = ""
    private var client: TendDispatchClient { TendDispatchClient(socketPath: model.client.socketPath) }
    private var owner: String { options?.coordinator ?? "local" }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Label("Fleet Dispatch", systemImage: "network").font(.title2)
                Spacer()
                Button("Close") { dismiss() }
            }
            Text("Tend selects a host and prepares an isolated worktree. The pipeline runs in an attachable tmux session.")
                .foregroundStyle(.secondary)
            if !supported {
                Text("Fleet dispatch requires an updated CLI and daemon. Configure the coordinator in Settings → Fleet → Dispatch.")
            }
            if let options { Text("Coordinator: \(options.coordinator ?? "local")").font(TendType.detail) }
            ScrollView {
                Form {
                    Section("Request") {
                        Picker("Workspace", selection: $store.request.workspace) {
                            Text("Select workspace").tag("")
                            ForEach((options?.workspaces.keys.sorted() ?? []), id: \.self) { Text(verbatim: $0).tag($0) }
                            if !store.request.workspace.isEmpty, options?.workspaces[store.request.workspace] == nil {
                                Text(verbatim: store.request.workspace).tag(store.request.workspace)
                            }
                        }
                        TextField("Work ID", text: $store.request.work)
                        TextField("Full commit SHA", text: $store.request.commit)
                        TextField("Node selector (optional)", text: optionalText($store.request.selector))
                        TextField("Host alias or NodeId (optional)", text: optionalText($store.request.host))
                        TextEditor(text: $store.request.body).frame(minHeight: 90)
                            .accessibilityLabel("Task and acceptance criteria")
                    }
                    .disabled(store.submitted || store.busy)
                    if let plan = store.report?.plan ?? store.preview {
                        Section(LocalizedStringKey(store.report == nil ? "Placement preview" : "Assigned work")) {
                            LabeledContent("Host", value: plan.host)
                            LabeledContent("NodeId", value: plan.nodeID)
                            if let worker = model.fleetHosts.first(where: { $0.nodeID == plan.nodeID }) {
                                Button("View worker") { model.select(.host(worker.id)); dismiss() }
                            }
                            LabeledContent("Pipeline", value: plan.pipeline)
                            LabeledContent("Root", value: plan.paths.root)
                            LabeledContent("Repository", value: plan.paths.repo)
                            LabeledContent("Run", value: plan.paths.run)
                            LabeledContent("Artifacts", value: plan.paths.artifacts)
                            if store.report == nil { Text("Placement is checked again when you dispatch.").font(TendType.detail) }
                        }.textSelection(.enabled)
                    }
                    if let report = store.report {
                        Section("Result") {
                            LabeledContent("State", value: report.state)
                            if report.state == "launched" { Text("The pipeline has started. This does not mean the work is complete.") }
                            if report.state == "unknown" { Text("Delivery is uncertain. Check status with this ID before retrying the same request.") }
                            ForEach(report.aliases.keys.sorted(), id: \.self) { alias in
                                LabeledContent(alias, value: report.aliases[alias] ?? "")
                            }
                            if let artifacts = report.artifacts { LabeledContent("Artifact location on worker", value: artifacts).textSelection(.enabled) }
                            if let error = report.error { Text(verbatim: error).foregroundStyle(.orange).textSelection(.enabled) }
                        }
                    }
                }.formStyle(.grouped)
                VStack(alignment: .leading, spacing: 8) {
                    Text("Saved dispatches").font(.headline)
                    ForEach(store.receipts.filter { $0.socket == model.client.socketPath && $0.coordinator == owner }.reversed()) { receipt in
                        Button { store.select(receipt); lookupID = receipt.id; refresh(receipt.id) } label: {
                            Text(verbatim: "\(receipt.request.workspace) / \(receipt.request.work) · \(receipt.id)")
                        }.disabled(store.busy)
                    }
                }.padding(.horizontal)
            }
            if let error = store.error { Text(verbatim: error).foregroundStyle(.orange).textSelection(.enabled) }
            Text(verbatim: "Dispatch ID: \(store.request.dispatchID)").font(TendType.detail).textSelection(.enabled)
            HStack {
                TextField("Dispatch UUID", text: $lookupID)
                Button("Check status") { refresh(lookupID) }.disabled(store.busy || lookupID.isEmpty || !supported)
            }
            HStack {
                Button("New Work") { store.newDraft(); lookupID = "" }.disabled(store.busy)
                Spacer()
                if store.busy { ProgressView().controlSize(.small) }
                Button("Preview placement") { send(preview: true) }.disabled(!store.request.isValid || store.busy || store.submitted || !supported)
                Button(LocalizedStringKey(store.submitted ? "Retry same request" : "Dispatch Work")) { send(preview: false) }
                    .buttonStyle(.borderedProminent)
                    .disabled(!store.request.isValid || store.busy || !supported || options == nil)
            }
        }
        .padding(20).frame(width: 780, height: 760)
        .onChange(of: store.request) { _ in if !store.submitted { store.preview = nil } }
        .task {
            supported = await model.client.supports("config_orchestration_v1")
            guard supported else { return }
            do { options = try await client.options() }
            catch { store.error = error.localizedDescription }
        }
    }

    private func send(preview: Bool) {
        store.busy = true; store.error = nil
        Task {
            defer { store.busy = false }
            do {
                if !preview { try store.reserve(socket: model.client.socketPath, coordinator: owner); lookupID = store.request.dispatchID }
                let data = try await client.dispatch(store.request, preview: preview)
                if preview { store.preview = try JSONDecoder().decode(TendDispatchPlan.self, from: data) }
                else { store.report = try TendDispatchReport.decode(data) }
            } catch {
                store.error = error.localizedDescription
                // A failed transport is not proof of cancellation. The frozen receipt remains.
                if !preview && store.submitted { store.report = TendDispatchReport(state: "unknown", plan: nil, aliases: [:]) }
            }
        }
    }
    private func refresh(_ id: String) {
        store.busy = true; store.error = nil
        Task {
            defer { store.busy = false }
            do {
                let report = try await client.status(id)
                if let plan = report.plan {
                    store.request = plan.request; store.submitted = true; store.preview = nil
                }
                store.report = report
            }
            catch { store.error = error.localizedDescription }
        }
    }
}

private func optionalText(_ value: Binding<String?>) -> Binding<String> {
    Binding(get: { value.wrappedValue ?? "" }, set: { value.wrappedValue = $0.isEmpty ? nil : $0 })
}

/// Settings › Fleet: the Hosts and Dispatch halves share one loaded copy of
/// the orchestration settings, because a label edit on a host advances the
/// baseline the Dispatch draft is saved against.
struct FleetDispatchSettingsView: View {
    @ObservedObject var model: AppModel
    let section: TendFleetSettingsSection
    @State private var document: TendOrchestrationDocument?
    @State private var settings = TendOrchestrationSettings()
    @State private var newWorkspace = ""
    @State private var editingWorkspace: FleetWorkspaceEditTarget?
    @State private var labelHost: TendFleetHost?
    @State private var isRegisteringHost = false
    @State private var busy = false
    @State private var supported = false
    @State private var message: String?
    private var client: TendDispatchClient { TendDispatchClient(socketPath: model.client.socketPath) }
    private var localOwner: Bool { settings.coordinator == nil || settings.coordinator == "local" }
    private var isChanged: Bool { document != nil && settings != document?.orchestration }

    private var hosts: [TendFleetHost] {
        model.fleetHosts.sorted { left, right in
            if left.local != right.local { return left.local }
            return left.alias.localizedStandardCompare(right.alias) == .orderedAscending
        }
    }

    var body: some View {
        Group {
            switch section {
            case .hosts: hostsForm
            case .dispatch: dispatchForm
            }
        }
        .task {
            supported = await model.client.supports("config_orchestration_v1")
            if supported { load() } else { message = String(localized: "Update the CLI and daemon to edit Fleet dispatch settings.") }
        }
        .sheet(item: $labelHost) { host in
            FleetLabelEditor(model: model, host: host, onSaved: refreshAfterLabelEdit)
        }
        .sheet(isPresented: $isRegisteringHost) {
            HostRegistrationView(model: model)
        }
        .sheet(item: $editingWorkspace) { target in
            FleetWorkspacePolicyEditor(
                workspace: target.id,
                policy: Binding(
                    get: { settings.workspaces[target.id] ?? TendWorkspacePolicy() },
                    set: { settings.workspaces[target.id] = $0 }
                )
            )
        }
    }

    // MARK: Hosts

    private var hostsForm: some View {
        Form {
            Section {
                if hosts.isEmpty {
                    VStack(alignment: .leading, spacing: 4) {
                        Text("No Hosts").font(.headline)
                        TendSettingsCaption("Register an SSH host to monitor its agents from Tend.")
                    }
                } else {
                    ForEach(hosts) { host in
                        TendFleetSettingsRow(
                            host: host,
                            onEditLabels: canEditLabels ? { labelHost = host } : nil
                        )
                    }
                    if !hosts.contains(where: { !$0.local }) {
                        TendSettingsCaption("No SSH hosts yet. Add one to watch its panes and run Work on it from this Mac.")
                    }
                }
            } header: {
                HStack {
                    Text("Hosts")
                    Spacer()
                    Button {
                        Task { await model.refresh() }
                    } label: {
                        Label("Refresh", systemImage: "arrow.clockwise")
                    }
                    Button {
                        model.prepareHostRegistration()
                        isRegisteringHost = true
                    } label: {
                        Label("Add Host…", systemImage: "plus")
                    }
                }
                .controlSize(.small)
            } footer: {
                TendSettingsCaption("Local and SSH hosts registered with the central tendd controller. Labels are used by workspace and request selectors.")
            }
        }
        .formStyle(.grouped)
    }

    /// Labels are set on the coordinator, so only it offers to edit them.
    private var canEditLabels: Bool {
        supported && document != nil && localOwner && !busy
    }

    // MARK: Dispatch

    private var dispatchForm: some View {
        Form {
            Section("Coordinator") {
                Toggle("Enable Fleet dispatch", isOn: $settings.enabled)
                TextField("Coordinator host alias", text: optionalText($settings.coordinator), prompt: Text("Empty means this host"))
                TendSettingsCaption("These settings belong to the connected daemon. Other entry hosts use its Fleet alias to share placement and Ask history.")
            }
            if localOwner {
                Section {
                    paths($settings.paths)
                } header: {
                    HStack(spacing: 6) {
                        Text("Default execution paths")
                        TendSettingsInfoButton(title: "Path placeholders") {
                            Text("Use {repo}, {workspace}, {work}, and {attempt}. Runs and artifacts must include {attempt}. Paths are expanded on the selected worker.")
                        }
                    }
                }
                Section("Workspace policies") {
                    if settings.workspaces.isEmpty {
                        TendSettingsCaption("No workspace policies yet.")
                    }
                    ForEach(settings.workspaces.keys.sorted(), id: \.self) { workspace in
                        workspaceRow(workspace)
                    }
                    HStack {
                        TextField("New workspace ID", text: $newWorkspace)
                        Button("Add Workspace…") {
                            let workspace = newWorkspace
                            settings.workspaces[workspace] = TendWorkspacePolicy()
                            newWorkspace = ""
                            editingWorkspace = FleetWorkspaceEditTarget(id: workspace)
                        }
                        .disabled(newWorkspace.isEmpty || settings.workspaces[newWorkspace] != nil)
                    }
                }
            } else {
                Section {
                    TendSettingsCaption("Edit workspace and node paths on the coordinator. This host forwards requests there.")
                }
            }
        }
        .formStyle(.grouped)
        .disabled(busy || !supported || document == nil)
        .safeAreaInset(edge: .bottom, spacing: 0) {
            TendSettingsFooter(
                isBusy: busy,
                canRevert: isChanged,
                canSave: isChanged && supported,
                revert: { if let document { settings = document.orchestration } },
                save: save
            ) {
                if let message {
                    Text(verbatim: message).foregroundStyle(.orange).textSelection(.enabled)
                } else {
                    Text("Saving reconnects Tend to tendd.").foregroundStyle(.secondary)
                }
            }
        }
    }

    private func workspaceRow(_ workspace: String) -> some View {
        let policy = settings.workspaces[workspace] ?? TendWorkspacePolicy()
        let summary = [policy.repo, policy.pipeline, policy.selector].filter { !$0.isEmpty }.joined(separator: " · ")
        return HStack {
            VStack(alignment: .leading, spacing: 2) {
                Text(verbatim: workspace)
                Group {
                    if summary.isEmpty {
                        Text("Inherits every default")
                    } else {
                        Text(verbatim: summary)
                    }
                }
                .font(TendType.detail)
                .foregroundStyle(.secondary)
                .lineLimit(1)
            }
            Spacer()
            if !policy.nodes.isEmpty {
                Text("\(policy.nodes.count) node overrides")
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
            }
            Button("Edit…") { editingWorkspace = FleetWorkspaceEditTarget(id: workspace) }
                .controlSize(.small)
        }
    }

    @ViewBuilder private func paths(_ value: Binding<TendExecutionPaths>) -> some View {
        TextField("Root", text: value.root)
        TextField("Repository path", text: value.repo)
        TextField("Run path", text: value.run)
        TextField("Artifact path", text: value.artifacts)
    }

    private func load() {
        busy = true
        Task {
            defer { busy = false }
            do { let result = try await client.readSettings(); document = result; settings = result.orchestration; message = nil }
            catch { message = error.localizedDescription }
        }
    }
    private func refreshAfterLabelEdit() async {
        guard let document else { return }
        busy = true
        defer { busy = false }
        do {
            let latest = try await client.readSettings()
            self.document = try document.refreshedAfterLabelEdit(latest)
            // Keep the user's policy draft; only the conflict-detection baseline advances.
            message = nil
        } catch { message = error.localizedDescription }
    }
    private func save() {
        guard let document else { return }
        busy = true
        Task {
            defer { busy = false }
            do {
                let result = try await client.saveSettings(settings, expected: document.config.text)
                self.document = result; settings = result.orchestration; message = nil
                model.retryConnection()
            } catch { message = error.localizedDescription }
        }
    }
}

private struct FleetWorkspaceEditTarget: Identifiable {
    let id: String
}

/// One workspace's policy, edited in place in the Dispatch draft: Save in
/// the Dispatch footer writes it with everything else.
private struct FleetWorkspacePolicyEditor: View {
    let workspace: String
    @Binding var policy: TendWorkspacePolicy
    @Environment(\.dismiss) private var dismiss
    @State private var node = ""
    @State private var newNode = ""

    private var nodePaths: Binding<TendPathOverrides> {
        Binding(get: { policy.nodes[node] ?? TendPathOverrides() }, set: { policy.nodes[node] = $0 })
    }

    var body: some View {
        VStack(spacing: 0) {
            Form {
                Section {
                    TextField("Repository ID", text: $policy.repo)
                    TextField("Repository URL", text: $policy.url)
                    TextField("Pipeline", text: $policy.pipeline)
                    TextField("Node selector", text: $policy.selector)
                } header: {
                    Text(verbatim: workspace)
                }
                Section {
                    overrides($policy.paths)
                } header: {
                    Text("Path overrides")
                } footer: {
                    TendSettingsCaption("Empty overrides inherit the defaults above.")
                }
                Section("Node overrides") {
                    Picker("Node override", selection: $node) {
                        Text("Select node").tag("")
                        ForEach(policy.nodes.keys.sorted(), id: \.self) { Text(verbatim: $0).tag($0) }
                    }
                    HStack {
                        TextField("NodeId or coordinator-local alias", text: $newNode)
                        Button("Add node") { policy.nodes[newNode] = TendPathOverrides(); node = newNode; newNode = "" }
                            .disabled(newNode.isEmpty || policy.nodes[newNode] != nil)
                    }
                    if policy.nodes[node] != nil {
                        overrides(nodePaths)
                        Button("Remove node override", role: .destructive) { policy.nodes.removeValue(forKey: node); node = "" }
                    }
                }
            }
            .formStyle(.grouped)
            Divider()
            HStack {
                TendSettingsCaption("Changes are saved with Save in Dispatch.")
                Spacer()
                Button("Done") { dismiss() }
                    .keyboardShortcut(.defaultAction)
            }
            .padding(14)
        }
        .frame(minWidth: 520, idealWidth: 600, minHeight: 480, idealHeight: 620)
    }

    @ViewBuilder private func overrides(_ value: Binding<TendPathOverrides>) -> some View {
        TextField("Root override", text: optionalText(value.root))
        TextField("Repository path override", text: optionalText(value.repo))
        TextField("Run path override", text: optionalText(value.run))
        TextField("Artifact path override", text: optionalText(value.artifacts))
    }
}

private struct FleetLabelEditor: View {
    @ObservedObject var model: AppModel
    let host: TendFleetHost
    let onSaved: () async -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var key = ""
    @State private var value = ""
    @State private var busy = false
    @State private var error: String?
    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Node labels").font(.headline)
            Text(verbatim: host.alias)
            ForEach((host.labels ?? [:]).keys.sorted(), id: \.self) { name in
                Button { key = name; value = host.labels?[name] ?? "" } label: {
                    Text(verbatim: "\(name)=\(host.labels?[name] ?? "")")
                }
            }
            TextField("Label key", text: $key)
            TextField("Label value", text: $value)
            Text("Labels are used by workspace and request selectors. System-managed labels cannot be changed.").font(TendType.detail)
            if let error { Text(verbatim: error).foregroundStyle(.orange) }
            HStack {
                Button("Close") { dismiss() }
                Spacer()
                Button("Remove label") { save(nil) }.disabled(host.labels?[key] == nil || busy)
                Button("Save label") { save(value) }.disabled(key.isEmpty || busy)
            }
        }.padding(20).frame(width: 540).disabled(busy)
    }
    private func save(_ value: String?) {
        busy = true; error = nil
        Task {
            defer { busy = false }
            do { try await model.setHostLabel(host: host.alias, key: key, value: value); await onSaved(); dismiss() }
            catch { self.error = error.localizedDescription }
        }
    }
}
