import SwiftUI

/// A pipeline drawn the way muxa launches it: one column per stage, agents
/// that start together stacked in a column, and an arrow between stages for
/// each `after` edge. This is the picture the CLI's `--dry-run` describes in
/// words, shown before anything is created.
struct PipelineStagesView: View {
    let agents: [MuxaWorkOptions.Agent]
    var compact = false

    private var stages: [[MuxaWorkOptions.Agent]] {
        MuxaPipelineStages.stages(for: agents)
    }

    var body: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(alignment: .top, spacing: compact ? 6 : 10) {
                ForEach(Array(stages.enumerated()), id: \.offset) { index, stage in
                    if index > 0 {
                        Image(systemName: "arrow.right")
                            .font(compact ? .caption2.weight(.semibold) : .caption.weight(.semibold))
                            .foregroundStyle(.tertiary)
                            .padding(.top, compact ? 9 : 14)
                    }
                    VStack(alignment: .leading, spacing: compact ? 4 : 6) {
                        if !compact, stages.count > 1 {
                            Text(index == 0 ? "Starts first" : "Stage \(index + 1)")
                                .font(.caption2.weight(.semibold))
                                .foregroundStyle(.tertiary)
                        }
                        ForEach(stage) { agent in
                            PipelineAgentChip(agent: agent, compact: compact)
                                // Chips keep their natural width inside the
                                // horizontal scroller instead of truncating
                                // the program badge when a card is narrow.
                                .fixedSize(horizontal: true, vertical: false)
                        }
                    }
                }
            }
            .padding(.vertical, 2)
        }
    }
}

struct PipelineAgentChip: View {
    let agent: MuxaWorkOptions.Agent
    var compact = false

    private var subtitle: String? {
        let role = agent.role?.isEmpty == false ? agent.role : nil
        let task = agent.task?.isEmpty == false ? agent.task : nil
        return [role, task].compactMap { $0 }.joined(separator: " · ").nonEmptyValue
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack(spacing: 6) {
                Text("@\(agent.alias)")
                    .font(compact ? .caption.weight(.semibold) : .subheadline.weight(.semibold))
                    .lineLimit(1)
                Text(agent.program)
                    .font(.caption2.weight(.medium))
                    .foregroundStyle(agentProgramTint(agent.program))
                    .padding(.horizontal, 5)
                    .padding(.vertical, 1)
                    .background(agentProgramTint(agent.program).opacity(0.12), in: Capsule())
            }
            if !compact, let subtitle {
                Text(subtitle)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
            }
            if !compact, !agent.after.isEmpty {
                Text("after \(agent.after.map { "@\($0)" }.joined(separator: ", "))")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
            }
        }
        .padding(.horizontal, compact ? 8 : 10)
        .padding(.vertical, compact ? 5 : 7)
        .frame(minWidth: compact ? 0 : 150, alignment: .leading)
        .background(Color.primary.opacity(0.05), in: RoundedRectangle(cornerRadius: 8))
        .overlay {
            RoundedRectangle(cornerRadius: 8)
                .stroke(Color(nsColor: .separatorColor).opacity(0.5), lineWidth: 0.5)
        }
    }
}

/// A configured pipeline as a launchable card: description, stage diagram,
/// the routes that select it, and a Start button that opens the sheet with
/// this pipeline preselected.
struct WorkPipelineCard: View {
    let pipeline: MuxaWorkOptions.Pipeline
    let routes: [MuxaWorkOptions.Route]
    let start: () -> Void
    var edit: (() -> Void)?
    /// Sync state on each control host; empty when there is no other host.
    var hostStates: [MuxaPipelineHostState] = []
    var sync: (() -> Void)?
    var syncing = false

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline) {
                VStack(alignment: .leading, spacing: 2) {
                    Text(pipeline.name)
                        .font(.headline)
                        .lineLimit(1)
                    if let description = pipeline.description, !description.isEmpty {
                        Text(description)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .lineLimit(2)
                    }
                }
                Spacer(minLength: 6)
                Text("\(pipeline.agents.count) agent\(pipeline.agents.count == 1 ? "" : "s")")
                    .font(.caption2.monospacedDigit())
                    .foregroundStyle(.tertiary)
            }

            PipelineStagesView(agents: pipeline.agents, compact: true)

            if !hostStates.isEmpty {
                PipelineFlowLayout(spacing: 6) {
                    ForEach(hostStates) { state in
                        PipelineHostBadge(state: state)
                    }
                    if syncing {
                        ProgressView().controlSize(.mini)
                    } else if hostStates.contains(where: \.needsSync), let sync {
                        Button(action: sync) {
                            Label("Sync to hosts", systemImage: "arrow.triangle.2.circlepath")
                        }
                        .buttonStyle(.borderless)
                        .controlSize(.small)
                        .help("Write this pipeline to every host where it is missing or differs")
                    }
                }
            }

            HStack(alignment: .firstTextBaseline, spacing: 8) {
                if routes.isEmpty {
                    Text("No route selects it; choose it explicitly when starting Work.")
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                        .lineLimit(2)
                } else {
                    Text(routes.map { routeSummary($0) }.joined(separator: " · "))
                        .font(.caption2.monospaced())
                        .foregroundStyle(.tertiary)
                        .lineLimit(2)
                }
                Spacer(minLength: 4)
                if let edit {
                    Button(action: edit) {
                        Label("Edit…", systemImage: "slider.horizontal.3")
                    }
                    .controlSize(.small)
                    .help("Edit the agents, prompts, and after edges of this pipeline")
                }
                Button(action: start) {
                    Label("Start…", systemImage: "play.fill")
                }
                .buttonStyle(.borderedProminent)
                .controlSize(.small)
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, minHeight: 168, alignment: .topLeading)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
        .overlay {
            RoundedRectangle(cornerRadius: 12)
                .stroke(Color(nsColor: .separatorColor).opacity(0.5), lineWidth: 0.5)
        }
    }

    private func routeSummary(_ route: MuxaWorkOptions.Route) -> String {
        var parts = ["match \(route.match)"]
        if let workspace = route.workspace, !workspace.isEmpty { parts.append("workspace \(workspace)") }
        if route.worktree { parts.append("worktree") }
        return parts.joined(separator: " → ")
    }
}

/// One host's copy of a library pipeline: in sync, different, missing, or
/// not known (host unreachable or running an older muxa).
struct PipelineHostBadge: View {
    let state: MuxaPipelineHostState

    private var symbol: (name: String, tint: Color, help: String) {
        switch state.state {
        case .inSync: ("checkmark.circle.fill", .green, "Same definition on \(state.host)")
        case .differs: ("exclamationmark.circle.fill", .orange, "\(state.host) has a different definition; Sync overwrites it")
        case .missing: ("circle.dashed", .secondary, "Not on \(state.host) yet")
        case .unavailable: ("questionmark.circle", .secondary, "\(state.host)'s config could not be read")
        }
    }

    var body: some View {
        Label(state.host, systemImage: symbol.name)
            .font(.caption2.weight(.medium))
            .foregroundStyle(symbol.tint)
            .labelStyle(.titleAndIcon)
            .lineLimit(1)
            .fixedSize()
            .padding(.horizontal, 6)
            .padding(.vertical, 2)
            .background(symbol.tint.opacity(0.1), in: Capsule())
            .help(symbol.help)
    }
}

/// Left-aligned wrapping row: badges and small buttons keep their natural
/// width and flow onto the next line instead of squeezing mid-word.
struct PipelineFlowLayout: Layout {
    var spacing: CGFloat = 6

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let width = proposal.width ?? .infinity
        return arrange(width: width, subviews: subviews).size
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        let placement = arrange(width: bounds.width, subviews: subviews)
        for (index, origin) in placement.origins.enumerated() {
            subviews[index].place(
                at: CGPoint(x: bounds.minX + origin.x, y: bounds.minY + origin.y),
                proposal: .unspecified
            )
        }
    }

    private func arrange(width: CGFloat, subviews: Subviews) -> (size: CGSize, origins: [CGPoint]) {
        var origins: [CGPoint] = []
        var x: CGFloat = 0
        var y: CGFloat = 0
        var rowHeight: CGFloat = 0
        var maxX: CGFloat = 0
        for subview in subviews {
            let size = subview.sizeThatFits(.unspecified)
            if x > 0, x + size.width > width {
                x = 0
                y += rowHeight + spacing
                rowHeight = 0
            }
            origins.append(CGPoint(x: x, y: y))
            x += size.width + spacing
            rowHeight = max(rowHeight, size.height)
            maxX = max(maxX, x - spacing)
        }
        let height = subviews.isEmpty ? 0 : y + rowHeight
        return (CGSize(width: width.isFinite ? max(width, maxX) : maxX, height: height), origins)
    }
}

/// Shown when the config has no pipeline yet: muxa's built-in presets with
/// their stage diagrams and one-click install, plus the in-app composer that
/// drafts a custom line-up from a plain-language description.
///
/// The composer sheet is attached here rather than to the main window: the
/// gallery also lives inside the Start Work sheet, where a window-level
/// sheet would stay hidden behind it.
struct WorkPresetGallery: View {
    let options: MuxaWorkOptions
    var host: String?
    @ObservedObject var model: AppModel
    var onInstalled: ((String) -> Void)?
    /// The pre-composer path (`muxa work init` in a Shell tab), kept as the
    /// composer's escape hatch when neither muxad nor the bundled CLI can
    /// draft. Callers that own a sheet close it inside this closure.
    var onDescribe: (() -> Void)?
    @State private var composerTarget: MuxaPipelineComposerTarget?

    private let columns = [
        GridItem(.adaptive(minimum: 250, maximum: 400), spacing: 12, alignment: .top),
    ]

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            VStack(alignment: .leading, spacing: 3) {
                Label("No pipeline is configured yet", systemImage: "point.3.connected.trianglepath.dotted")
                    .font(.headline)
                Text("A pipeline is the set of agents a Work window is staffed with. Install a preset to start now, or describe your own and let an agent write the config.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                if options.routes.isEmpty {
                    Text("Installing also adds a catch-all route so every Work id can use it.")
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                }
            }

            LazyVGrid(columns: columns, alignment: .leading, spacing: 12) {
                ForEach(options.presets) { preset in
                    VStack(alignment: .leading, spacing: 9) {
                        HStack(alignment: .firstTextBaseline) {
                            Text(preset.name)
                                .font(.headline)
                            Spacer(minLength: 4)
                            Text("\(preset.agents.count) agent\(preset.agents.count == 1 ? "" : "s")")
                                .font(.caption2.monospacedDigit())
                                .foregroundStyle(.tertiary)
                        }
                        if let description = preset.description, !description.isEmpty {
                            Text(description)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .lineLimit(2)
                        }
                        PipelineStagesView(agents: preset.agents, compact: true)
                        HStack {
                            Spacer()
                            Button {
                                Task {
                                    if await model.applyWorkPreset(preset.name, host: host) {
                                        onInstalled?(preset.name)
                                    }
                                }
                            } label: {
                                Label("Install", systemImage: "square.and.arrow.down")
                            }
                            .buttonStyle(.borderedProminent)
                            .controlSize(.small)
                            .disabled(model.isApplyingWorkPreset)
                        }
                    }
                    .padding(12)
                    .frame(maxWidth: .infinity, alignment: .topLeading)
                    .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 10))
                    .overlay {
                        RoundedRectangle(cornerRadius: 10)
                            .stroke(Color(nsColor: .separatorColor).opacity(0.5), lineWidth: 0.5)
                    }
                }
            }

            HStack(spacing: 8) {
                if model.isApplyingWorkPreset {
                    ProgressView().controlSize(.small)
                    Text("Writing the preset into \(options.configPath ?? "the muxa config")…")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Button {
                    model.presentPipelineEditor(host: host, pipeline: nil)
                } label: {
                    Label("Design your own…", systemImage: "slider.horizontal.3")
                }
                .help("Compose agents, prompts, and after edges visually")
                Button {
                    composerTarget = MuxaPipelineComposerTarget(host: host)
                } label: {
                    Label("Describe with an agent…", systemImage: "sparkles")
                }
                .help("Describe the line-up in plain language; the Ask provider drafts a pipeline you can refine, edit, or save")
            }
            if let error = model.workOptionsError(for: host) {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.red)
                    .textSelection(.enabled)
            }
        }
        .sheet(item: $composerTarget) { target in
            PipelineComposerView(
                target: target,
                model: model,
                shellFallback: onDescribe,
                onSaved: onInstalled
            )
        }
    }
}

/// The dry-run result of `muxa work up`: every step muxad would take, with
/// the exact prompt each new agent would receive, plus a button to launch
/// the same request for real.
struct WorkPlanView: View {
    let result: MuxaWorkStartResult
    let launch: () -> Void

    private var steps: [MuxaWorkPlanStep] { result.plan?.steps ?? [] }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Label("Work plan is ready", systemImage: "checkmark.seal")
                    .font(.headline)
                Spacer()
                Text(planSummary)
                    .font(.caption2.monospaced())
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            if steps.isEmpty {
                Text("Nothing would change: the Work window already matches its pipeline.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            ForEach(steps) { step in
                WorkPlanStepRow(step: step)
            }
            HStack {
                Text("Nothing was created. Launch runs the same request without Plan only.")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
                Spacer()
                Button(action: launch) {
                    Label("Launch now", systemImage: "play.fill")
                }
                .buttonStyle(.borderedProminent)
                .controlSize(.small)
                .disabled(steps.isEmpty)
            }
        }
    }

    private var planSummary: String {
        var parts = ["\(result.workspace) / \(result.work)"]
        if let pipeline = result.pipeline, !pipeline.isEmpty { parts.append("pipeline \(pipeline)") }
        if let cwd = result.cwd, !cwd.isEmpty { parts.append(cwd) }
        return parts.joined(separator: " · ")
    }
}

private struct WorkPlanStepRow: View {
    let step: MuxaWorkPlanStep
    @State private var showsPrompt = false

    private var symbol: (name: String, tint: Color) {
        switch step.action {
        case "launch": ("play.circle.fill", .green)
        case "reprompt": ("arrow.uturn.forward.circle.fill", .blue)
        case "keep": ("checkmark.circle", .secondary)
        case "waiting": ("clock", .orange)
        case "attention": ("exclamationmark.triangle.fill", .red)
        default: ("circle", .secondary)
        }
    }

    private var headline: String {
        switch step.action {
        case "launch": "Launch @\(step.alias)"
        case "reprompt": "Send the request to @\(step.alias)"
        case "keep": "Keep @\(step.alias) as is"
        case "waiting": "@\(step.alias) waits for \(step.waitingOn.map { "@\($0)" }.joined(separator: ", "))"
        case "attention": "@\(step.alias) needs a person first"
        default: "@\(step.alias): \(step.action)"
        }
    }

    private var detail: String? {
        let parts = [step.program, step.role, step.task, step.pane.map { "pane \($0)" }, step.state]
            .compactMap { $0 }
            .filter { !$0.isEmpty }
        return parts.isEmpty ? nil : parts.joined(separator: " · ")
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Image(systemName: symbol.name)
                    .foregroundStyle(symbol.tint)
                    .frame(width: 16)
                VStack(alignment: .leading, spacing: 2) {
                    Text(headline)
                        .font(.subheadline.weight(.medium))
                    if let detail {
                        Text(detail)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                Spacer(minLength: 4)
                if let prompt = step.prompt, !prompt.isEmpty {
                    Button(showsPrompt ? "Hide prompt" : "Show prompt") {
                        showsPrompt.toggle()
                    }
                    .buttonStyle(.borderless)
                    .font(.caption)
                }
            }
            if showsPrompt, let prompt = step.prompt {
                ReadableMarkdownContent(source: prompt)
                    .padding(10)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(Color.primary.opacity(0.04), in: RoundedRectangle(cornerRadius: 7))
            }
        }
        .padding(.vertical, 2)
    }
}

/// The `[[route]]` table as an editable list: which Work ids select which
/// pipeline, workspace, and folder. Rows save through `muxa work route set`
/// and `muxa work route remove`, so the order and the untouched fields
/// (worktree, prepare) stay exactly as written in the file.
///
/// The draft is a plain value plus an `editing` flag rather than an optional
/// bound with `Binding($optional)`: SwiftUI force-unwraps such a binding on
/// the next update, and clearing the draft after a save while its text
/// fields were still alive crashed the app.
struct WorkRoutesEditor: View {
    let options: MuxaWorkOptions
    let host: String?
    @ObservedObject var model: AppModel
    @State private var draft = MuxaWorkRouteEdit()
    @State private var editing = false
    @State private var saving = false

    /// Match text of the existing route being edited, or nil while adding.
    private var editingMatch: String? {
        editing && draft.existing ? draft.match : nil
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline) {
                Text("Routes")
                    .font(.headline)
                Text("First match wins; a Work id that matches no route needs an explicit pipeline.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Spacer()
                Button {
                    draft = MuxaWorkRouteEdit(match: "", pipeline: options.pipelines.first?.name ?? "")
                    editing = true
                } label: {
                    Label("Add Route", systemImage: "plus")
                }
                .buttonStyle(.borderless)
                .disabled(editing)
            }

            if options.routes.isEmpty, !editing {
                Text("No routes yet. Add one with match .* to send every Work id to a pipeline.")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }

            ForEach(Array(options.routes.enumerated()), id: \.offset) { index, route in
                if editingMatch == route.match {
                    routeForm(position: index)
                } else {
                    routeRow(route, position: index)
                }
            }
            if editing, !draft.existing {
                routeForm(position: options.routes.count)
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
        .overlay {
            RoundedRectangle(cornerRadius: 12)
                .stroke(Color(nsColor: .separatorColor).opacity(0.5), lineWidth: 0.5)
        }
        .onChange(of: host) { _ in
            // A different host has different routes; never carry a draft over.
            editing = false
        }
    }

    private func routeRow(_ route: MuxaWorkOptions.Route, position: Int) -> some View {
        HStack(spacing: 10) {
            Text("\(position + 1)")
                .font(.caption2.monospacedDigit())
                .foregroundStyle(.tertiary)
                .frame(width: 18, alignment: .trailing)
            Text(route.match)
                .font(.callout.monospaced())
                .lineLimit(1)
            Image(systemName: "arrow.right")
                .font(.caption2)
                .foregroundStyle(.tertiary)
            Text(route.pipeline ?? "no pipeline")
                .font(.callout.weight(route.pipeline == nil ? .regular : .medium))
                .foregroundStyle(route.pipeline == nil ? Color.orange : Color.primary)
            if let workspace = route.workspace, !workspace.isEmpty {
                Text("workspace \(workspace)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            if route.worktree {
                Text("worktree")
                    .font(.caption2.weight(.medium))
                    .padding(.horizontal, 5)
                    .padding(.vertical, 1)
                    .background(Color.primary.opacity(0.07), in: Capsule())
            } else if let cwd = route.cwd, !cwd.isEmpty {
                Text(cwd)
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            if route.prepare {
                Text("prepare")
                    .font(.caption2.weight(.medium))
                    .padding(.horizontal, 5)
                    .padding(.vertical, 1)
                    .background(Color.primary.opacity(0.07), in: Capsule())
            }
            Spacer(minLength: 6)
            Button {
                draft = MuxaWorkRouteEdit(route)
                editing = true
            } label: {
                Image(systemName: "pencil")
            }
            .buttonStyle(.borderless)
            .disabled(editing || saving)
            .help("Edit this route")
            Button(role: .destructive) {
                saving = true
                Task {
                    _ = await model.removeRoute(match: route.match, host: host)
                    saving = false
                }
            } label: {
                Image(systemName: "trash")
            }
            .buttonStyle(.borderless)
            .disabled(editing || saving)
            .help("Remove this route")
        }
        .padding(.vertical, 3)
    }

    private func routeForm(position: Int) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 8) {
                TextField("", text: $draft.match, prompt: Text("match (regex, for example ^cal- or .*)"))
                    .labelsHidden()
                    .font(.callout.monospaced())
                    .disabled(draft.existing)
                Picker("Pipeline", selection: $draft.pipeline) {
                    Text("no pipeline").tag("")
                    ForEach(options.pipelines) { pipeline in
                        Text(pipeline.name).tag(pipeline.name)
                    }
                }
                .labelsHidden()
                .frame(width: 170)
            }
            HStack(spacing: 8) {
                TextField("", text: $draft.workspace, prompt: Text("workspace (optional)"))
                    .labelsHidden()
                TextField("", text: $draft.cwd, prompt: Text("folder on the host (optional)"))
                    .labelsHidden()
            }
            HStack {
                if let error = model.workOptionsError(for: host) {
                    Text(error)
                        .font(.caption)
                        .foregroundStyle(.red)
                        .lineLimit(2)
                }
                Spacer()
                if saving { ProgressView().controlSize(.small) }
                Button("Cancel") { editing = false }
                    .disabled(saving)
                Button(draft.existing ? "Save Route" : "Add Route") {
                    var route = draft
                    if !route.existing { route.position = position }
                    saving = true
                    Task {
                        let saved = await model.setRoute(route, host: host)
                        saving = false
                        if saved { editing = false }
                    }
                }
                .buttonStyle(.borderedProminent)
                .controlSize(.small)
                .disabled(saving || draft.match.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        }
        .padding(10)
        .background(Color.accentColor.opacity(0.06), in: RoundedRectangle(cornerRadius: 8))
    }
}

func agentProgramTint(_ program: String) -> Color {
    switch program.lowercased() {
    case "claude": .orange
    case "codex": .green
    case "gemini": .blue
    case "opencode": .purple
    case "agy": .teal
    default: .secondary
    }
}

private extension String {
    var nonEmptyValue: String? { isEmpty ? nil : self }
}
