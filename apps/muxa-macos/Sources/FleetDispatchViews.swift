import SwiftUI

struct FleetDispatchView: View {
    @ObservedObject var model: AppModel
    @ObservedObject private var store = MuxaDispatchStore.shared
    @Environment(\.dismiss) private var dismiss
    @State private var options: MuxaOrchestrationSettings?
    @State private var supported = false
    @State private var lookupID = ""
    private var client: MuxaDispatchClient { MuxaDispatchClient(socketPath: model.client.socketPath) }
    private var owner: String { options?.coordinator ?? "local" }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Label("Fleet Dispatch", systemImage: "network").font(.title2)
                Spacer()
                Button("Close") { dismiss() }
            }
            Text("Muxa selects a host and prepares an isolated worktree. The pipeline runs in an attachable tmux session.")
                .foregroundStyle(.secondary)
            if !supported {
                Text("Fleet dispatch requires an updated CLI and daemon. Configure the coordinator in Settings → Dispatch.")
            }
            if let options { Text("Coordinator: \(options.coordinator ?? "local")").font(.caption) }
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
                            if store.report == nil { Text("Placement is checked again when you dispatch.").font(.caption) }
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
            Text(verbatim: "Dispatch ID: \(store.request.dispatchID)").font(.caption).textSelection(.enabled)
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
                    .disabled(!store.request.isValid || store.busy || !supported || options == nil || (!store.submitted && store.preview == nil))
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
                if preview { store.preview = try JSONDecoder().decode(MuxaDispatchPlan.self, from: data) }
                else { store.report = try MuxaDispatchReport.decode(data) }
            } catch {
                store.error = error.localizedDescription
                // A failed transport is not proof of cancellation. The frozen receipt remains.
                if !preview && store.submitted { store.report = MuxaDispatchReport(state: "unknown", plan: nil, aliases: [:]) }
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

struct FleetDispatchSettingsView: View {
    @ObservedObject var model: AppModel
    @State private var document: MuxaOrchestrationDocument?
    @State private var settings = MuxaOrchestrationSettings()
    @State private var workspace = ""
    @State private var newWorkspace = ""
    @State private var node = ""
    @State private var newNode = ""
    @State private var labelHost: MuxaFleetHost?
    @State private var busy = false
    @State private var supported = false
    @State private var message: String?
    private var client: MuxaDispatchClient { MuxaDispatchClient(socketPath: model.client.socketPath) }
    private var localOwner: Bool { settings.coordinator == nil || settings.coordinator == "local" }
    private var policy: Binding<MuxaWorkspacePolicy> {
        Binding(get: { settings.workspaces[workspace] ?? MuxaWorkspacePolicy() }, set: { settings.workspaces[workspace] = $0 })
    }
    private var nodePaths: Binding<MuxaPathOverrides> {
        Binding(get: { policy.wrappedValue.nodes[node] ?? MuxaPathOverrides() }, set: { policy.wrappedValue.nodes[node] = $0 })
    }
    var body: some View {
        VStack(alignment: .leading) {
            Form {
                Section("Coordinator") {
                    Toggle("Enable Fleet dispatch", isOn: $settings.enabled)
                    TextField("Coordinator host alias", text: optionalText($settings.coordinator), prompt: Text("Empty means this host"))
                    Text("These settings belong to the connected daemon. Other entry hosts use its Fleet alias to share placement and Ask history.")
                        .font(.caption).foregroundStyle(.secondary)
                }
                if localOwner {
                    Section("Node labels") {
                        ForEach(model.fleetHosts) { host in
                            HStack {
                                VStack(alignment: .leading) {
                                    Text(verbatim: host.alias)
                                    Text(verbatim: (host.labels ?? [:]).sorted { $0.key < $1.key }.map { "\($0.key)=\($0.value)" }.joined(separator: ", "))
                                        .font(.caption).foregroundStyle(.secondary).textSelection(.enabled)
                                }
                                Spacer()
                                Button("Edit labels") { labelHost = host }
                            }
                        }
                    }
                    Section("Default execution paths") {
                        paths($settings.paths)
                    }
                    Section("Workspace policy") {
                        Picker("Workspace", selection: $workspace) {
                            Text("Select workspace").tag("")
                            ForEach(settings.workspaces.keys.sorted(), id: \.self) { Text(verbatim: $0).tag($0) }
                        }
                        HStack {
                            TextField("New workspace ID", text: $newWorkspace)
                            Button("Add workspace") {
                                settings.workspaces[newWorkspace] = MuxaWorkspacePolicy()
                                workspace = newWorkspace; newWorkspace = ""
                            }.disabled(newWorkspace.isEmpty || settings.workspaces[newWorkspace] != nil)
                        }
                        if settings.workspaces[workspace] != nil {
                            TextField("Repository ID", text: policy.repo)
                            TextField("Repository URL", text: policy.url)
                            TextField("Pipeline", text: policy.pipeline)
                            TextField("Node selector", text: policy.selector)
                            Text("Empty overrides inherit the defaults above.").font(.caption)
                            overrides(policy.paths)
                            Picker("Node override", selection: $node) {
                                Text("Select node").tag("")
                                ForEach(policy.wrappedValue.nodes.keys.sorted(), id: \.self) { Text(verbatim: $0).tag($0) }
                            }
                            HStack {
                                TextField("NodeId or coordinator-local alias", text: $newNode)
                                Button("Add node") { policy.wrappedValue.nodes[newNode] = MuxaPathOverrides(); node = newNode; newNode = "" }
                                    .disabled(newNode.isEmpty || policy.wrappedValue.nodes[newNode] != nil)
                            }
                            if policy.wrappedValue.nodes[node] != nil {
                                overrides(nodePaths)
                                Button("Remove node override") { policy.wrappedValue.nodes.removeValue(forKey: node); node = "" }
                            }
                        }
                    }
                    Text("Use {repo}, {workspace}, {work}, and {attempt}. Runs and artifacts must include {attempt}. Paths are expanded on the selected worker.")
                        .font(.caption).foregroundStyle(.secondary)
                } else {
                    Text("Edit workspace and node paths on the coordinator. This host forwards requests there.")
                }
            }.formStyle(.grouped).disabled(busy || !supported || document == nil)
            if let message { Text(verbatim: message).foregroundStyle(.orange).textSelection(.enabled).padding(.horizontal) }
            HStack {
                Button("Reload") { load() }.disabled(busy)
                Spacer()
                if busy { ProgressView().controlSize(.small) }
                Button("Save") { save() }.disabled(busy || document == nil || !supported || settings == document?.orchestration)
            }.padding()
        }.task {
            supported = await model.client.supports("config_orchestration_v1")
            if supported { load() } else { message = "Update the CLI and daemon to edit Fleet dispatch settings." }
        }.onChange(of: workspace) { _ in node = "" }
        .sheet(item: $labelHost) { host in FleetLabelEditor(model: model, host: host) }
    }
    @ViewBuilder private func paths(_ value: Binding<MuxaExecutionPaths>) -> some View {
        TextField("Root", text: value.root)
        TextField("Repository path", text: value.repo)
        TextField("Run path", text: value.run)
        TextField("Artifact path", text: value.artifacts)
    }
    @ViewBuilder private func overrides(_ value: Binding<MuxaPathOverrides>) -> some View {
        TextField("Root override", text: optionalText(value.root))
        TextField("Repository path override", text: optionalText(value.repo))
        TextField("Run path override", text: optionalText(value.run))
        TextField("Artifact path override", text: optionalText(value.artifacts))
    }
    private func load() {
        busy = true
        Task {
            defer { busy = false }
            do { let result = try await client.readSettings(); document = result; settings = result.orchestration; message = nil }
            catch { message = error.localizedDescription }
        }
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

private struct FleetLabelEditor: View {
    @ObservedObject var model: AppModel
    let host: MuxaFleetHost
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
            Text("Labels are used by workspace and request selectors. System-managed labels cannot be changed.").font(.caption)
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
            do { try await model.setHostLabel(host: host.alias, key: key, value: value); dismiss() }
            catch { self.error = error.localizedDescription }
        }
    }
}
