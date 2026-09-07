import SwiftUI

/// Visual editor for one `[pipeline.<name>]`: the agents, their programs,
/// roles, prompts, and `after` edges, with the launch-stage picture updating
/// as edges change. Saving goes through `muxa work pipeline set`, so the
/// config file stays the single source of truth for CLI and app alike.
struct PipelineEditorView: View {
    let target: MuxaPipelineEditorTarget
    @ObservedObject var model: AppModel
    @Environment(\.dismiss) private var dismiss

    @State private var name: String
    @State private var definition: MuxaPipelineDefinition
    @State private var confirmsDelete = false
    @State private var expandedPrompts = Set<UUID>()

    init(target: MuxaPipelineEditorTarget, model: AppModel) {
        self.target = target
        self.model = model
        _name = State(initialValue: target.pipeline?.name ?? "")
        _definition = State(initialValue: target.pipeline.map(MuxaPipelineDefinition.init)
            ?? MuxaPipelineDefinition(agents: [MuxaPipelineDefinition.Agent(alias: "impl", role: "implementer")]))
    }

    /// A draft from the composer is prefilled but not saved yet, so it is
    /// created like a new pipeline.
    private var isNew: Bool { target.pipeline == nil || target.isDraft }

    private var problems: [String] {
        var problems = definition.problems()
        if !MuxaPipelineDefinition.isValidName(name.trimmingCharacters(in: .whitespaces)) {
            problems.insert(String(localized: "Name may only use letters, digits, - and _."), at: 0)
        }
        return problems
    }

    private var routesUsingPipeline: [MuxaWorkOptions.Route] {
        model.workOptions(for: target.host)?.routes.filter { $0.pipeline == target.pipeline?.name } ?? []
    }

    /// The TOML section name shown in the header before a name is typed.
    private var sectionName: String { name.isEmpty ? "name" : name }

    private var configLabel: String {
        model.workOptions(for: target.host)?.configPath ?? String(localized: "the muxa config")
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: "point.3.connected.trianglepath.dotted")
                    .font(.system(size: 26))
                    .foregroundStyle(.tint)
                VStack(alignment: .leading, spacing: 3) {
                    Text(isNew ? "New pipeline" : "Edit pipeline \(target.pipeline?.name ?? "")")
                        .font(.title2.weight(.semibold))
                    Text("Saved as [pipeline.\(sectionName)] in \(configLabel) on \(target.host ?? model.localHostAlias).")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                }
                Spacer()
            }
            .padding(20)

            Divider()

            HSplitView {
                Form {
                    Section("Pipeline") {
                        TextField("Name", text: $name, prompt: Text("for example implement-review"))
                            .disabled(!isNew)
                        TextField("Description", text: $definition.description, prompt: Text("optional"))
                        Picker("tmux layout", selection: $definition.layout) {
                            Text("tmux default").tag("")
                            ForEach(MuxaPipelineDefinition.layouts, id: \.self) { layout in
                                Text(layout).tag(layout)
                            }
                        }
                        VStack(alignment: .leading, spacing: 4) {
                            Text("Shared prompt prefix (optional)")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            TextEditor(text: $definition.prompt)
                                .font(.body)
                                .frame(minHeight: 56, maxHeight: 120)
                            Text("Placeholders: {{work}}, {{workspace}}, {{cwd}}, {{request}}, {{ticket.title}}. Without {{request}} the operator's body is prepended automatically.")
                                .font(.caption2)
                                .foregroundStyle(.tertiary)
                        }
                    }

                    Section {
                        ForEach($definition.agents) { $agent in
                            agentRow($agent)
                        }
                        Button {
                            withAnimation {
                                definition.agents.append(MuxaPipelineDefinition.Agent(alias: nextAlias()))
                            }
                        } label: {
                            Label("Add agent", systemImage: "plus.circle")
                        }
                    } header: {
                        HStack {
                            Text("Agents")
                            Spacer()
                            Text(verbatim: "\(definition.agents.count)")
                                .foregroundStyle(.secondary)
                        }
                    }
                }
                .formStyle(.grouped)
                .frame(minWidth: 480, idealWidth: 560)

                ScrollView {
                VStack(alignment: .leading, spacing: 12) {
                    Text("Launch order")
                        .font(.headline)
                    Text("muxa starts every agent in a stage together and opens the next stage as its after edges report done.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    PipelineStagesView(agents: definition.optionsAgents)
                        .padding(10)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: 10))

                    if !routesUsingPipeline.isEmpty {
                        Text("Selected by \(routesUsingPipeline.map { String(localized: "match \($0.match)") }.joined(separator: ", "))")
                            .font(.caption2.monospaced())
                            .foregroundStyle(.tertiary)
                    }

                    if !problems.isEmpty {
                        VStack(alignment: .leading, spacing: 4) {
                            ForEach(problems, id: \.self) { problem in
                                Label(problem, systemImage: "exclamationmark.circle")
                                    .font(.caption)
                                    .foregroundStyle(.orange)
                            }
                        }
                    }
                    if let error = model.pipelineEditorError {
                        Label(error, systemImage: "exclamationmark.triangle.fill")
                            .font(.caption)
                            .foregroundStyle(.red)
                            .textSelection(.enabled)
                    }
                }
                .padding(18)
                .frame(maxWidth: .infinity, alignment: .topLeading)
                }
                .frame(minWidth: 300, maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            }

            Divider()

            HStack {
                if !isNew {
                    Button("Delete…", role: .destructive) { confirmsDelete = true }
                        .disabled(model.isSavingPipeline)
                }
                Spacer()
                if model.isSavingPipeline { ProgressView().controlSize(.small) }
                Button("Cancel") { close() }
                    .keyboardShortcut(.cancelAction)
                Button(isNew ? "Create Pipeline" : "Save Pipeline") { save() }
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut(.defaultAction)
                    .disabled(!problems.isEmpty || model.isSavingPipeline)
            }
            .padding(16)
        }
        .frame(minWidth: 880, idealWidth: 980, minHeight: 540, idealHeight: 680)
        .alert("Delete pipeline \(name)?", isPresented: $confirmsDelete) {
            Button("Cancel", role: .cancel) {}
            Button("Delete", role: .destructive) { delete() }
        } message: {
            Text(
                routesUsingPipeline.isEmpty
                    ? "The [pipeline.\(name)] section is removed from the config. Running agents are not affected."
                    : "\(routesUsingPipeline.count) routes select this pipeline; they will keep their match but lose the pipeline."
            )
        }
    }

    @ViewBuilder
    private func agentRow(_ agent: Binding<MuxaPipelineDefinition.Agent>) -> some View {
        let others = definition.agents
            .map { $0.alias.trimmingCharacters(in: .whitespaces).lowercased() }
            .filter { !$0.isEmpty && $0 != agent.wrappedValue.alias.trimmingCharacters(in: .whitespaces).lowercased() }
        VStack(alignment: .leading, spacing: 8) {
            // Two short rows instead of one wide one: the Form column is
            // about 460 points, so alias + program + role + direction on a
            // single line overflowed and clipped the role field.
            HStack(spacing: 8) {
                TextField("Alias", text: agent.alias, prompt: Text("alias"))
                    .labelsHidden()
                    .frame(width: 120)
                Picker("Program", selection: agent.program) {
                    ForEach(MuxaPipelineDefinition.allowedPrograms, id: \.self) { program in
                        Text(program).tag(program)
                    }
                }
                .labelsHidden()
                .frame(width: 104)
                Picker("Split direction", selection: agent.direction) {
                    Text("split automatically").tag("")
                    Text("split right").tag("right")
                    Text("split down").tag("down")
                }
                .labelsHidden()
                .frame(width: 150)
                .help("Automatic splits along the pane's longer side, so a growing line-up stays readable")
                Spacer(minLength: 0)
                Menu {
                    Button("Move up") { move(agent.wrappedValue.id, by: -1) }
                    Button("Move down") { move(agent.wrappedValue.id, by: 1) }
                    Divider()
                    Button("Remove agent", role: .destructive) { remove(agent.wrappedValue.id) }
                } label: {
                    Image(systemName: "ellipsis.circle")
                }
                .menuStyle(.borderlessButton)
                .fixedSize()
            }
            TextField("Role", text: agent.role, prompt: Text("role, for example implementer or reviewer"))
                .labelsHidden()
            TextField("Task", text: agent.task, prompt: Text("task label shown in muxa watch (optional)"))
                .labelsHidden()
            if !others.isEmpty {
                ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 6) {
                    Text("after")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    ForEach(others, id: \.self) { other in
                        Toggle(String(other), isOn: Binding(
                            get: { agent.wrappedValue.after.contains(other) },
                            set: { on in
                                if on {
                                    if !agent.wrappedValue.after.contains(other) { agent.wrappedValue.after.append(other) }
                                } else {
                                    agent.wrappedValue.after.removeAll { $0 == other }
                                }
                            }
                        ))
                        .toggleStyle(.button)
                        .controlSize(.small)
                    }
                }
                }
            }
            DisclosureGroup(
                isExpanded: Binding(
                    get: { expandedPrompts.contains(agent.wrappedValue.id) },
                    set: { open in
                        if open { expandedPrompts.insert(agent.wrappedValue.id) } else { expandedPrompts.remove(agent.wrappedValue.id) }
                    }
                )
            ) {
                TextEditor(text: agent.prompt)
                    .font(.body)
                    .frame(minHeight: 70, maxHeight: 160)
            } label: {
                Text(agent.wrappedValue.prompt.isEmpty ? "Prompt (optional)" : "Prompt")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.vertical, 4)
    }

    private func nextAlias() -> String {
        let taken = Set(definition.agents.map { $0.alias.lowercased() })
        for candidate in ["impl", "review", "plan", "test", "docs"] where !taken.contains(candidate) {
            return candidate
        }
        var index = definition.agents.count + 1
        while taken.contains("agent\(index)") { index += 1 }
        return "agent\(index)"
    }

    private func move(_ id: UUID, by offset: Int) {
        guard let index = definition.agents.firstIndex(where: { $0.id == id }) else { return }
        let destination = index + offset
        guard definition.agents.indices.contains(destination) else { return }
        definition.agents.swapAt(index, destination)
    }

    private func remove(_ id: UUID) {
        guard let removed = definition.agents.first(where: { $0.id == id }) else { return }
        definition.agents.removeAll { $0.id == id }
        let alias = removed.alias.trimmingCharacters(in: .whitespaces).lowercased()
        for index in definition.agents.indices {
            definition.agents[index].after.removeAll { $0 == alias }
        }
    }

    private func save() {
        let trimmed = name.trimmingCharacters(in: .whitespaces)
        Task {
            if await model.savePipeline(definition, named: trimmed, host: target.host) {
                close()
            }
        }
    }

    private func delete() {
        Task {
            if await model.removePipeline(named: name, host: target.host, force: !routesUsingPipeline.isEmpty) {
                close()
            }
        }
    }

    private func close() {
        model.pipelineEditorTarget = nil
        dismiss()
    }
}
