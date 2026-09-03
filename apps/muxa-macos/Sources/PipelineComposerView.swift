import SwiftUI

/// "Describe with an agent…" as an in-app conversation: the operator
/// describes the line-up in plain language, muxa asks the configured Ask
/// provider through the daemon's `work_compose`, and the answer lands as a
/// pipeline draft that can be refined, opened in the visual editor, or saved
/// straight into the library. Nothing is written until Save.
@MainActor
struct PipelineComposerView: View {
    let target: MuxaPipelineComposerTarget
    @ObservedObject var model: AppModel
    /// The pre-composer path (`muxa work init` in a Shell tab). The caller
    /// that owns a sheet passes a closure that closes it first.
    var shellFallback: (() -> Void)?
    /// Called with the saved name so a Start Work form can preselect it.
    var onSaved: ((String) -> Void)?

    @StateObject private var session: PipelineComposerSession
    @Environment(\.dismiss) private var dismiss
    @State private var addsCatchAllRoute = false
    @State private var isSaving = false
    @State private var saveError: String?

    init(
        target: MuxaPipelineComposerTarget,
        model: AppModel,
        shellFallback: (() -> Void)? = nil,
        onSaved: ((String) -> Void)? = nil
    ) {
        self.target = target
        self.model = model
        self.shellFallback = shellFallback
        self.onSaved = onSaved
        _session = StateObject(wrappedValue: model.makePipelineComposerSession(host: target.host))
    }

    private var routesAreEmpty: Bool {
        model.workOptions(for: target.host)?.routes.isEmpty ?? true
    }

    var body: some View {
        VStack(spacing: 0) {
            header
                .padding(20)

            Divider()

            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    describeSection
                    if session.hasDraft {
                        Divider()
                        resultSection
                    }
                }
                .padding(20)
                .frame(maxWidth: .infinity, alignment: .topLeading)
            }

            Divider()

            footer
                .padding(16)
        }
        .frame(minWidth: 720, idealWidth: 720, minHeight: 560, idealHeight: 560)
        .task { await session.prepare() }
    }

    // MARK: Header

    private var header: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: "sparkles")
                .font(.system(size: 26))
                .foregroundStyle(.tint)
            VStack(alignment: .leading, spacing: 3) {
                Text("Describe a pipeline")
                    .font(.title2.weight(.semibold))
                if let host = target.host {
                    Text("Saved into the muxa config on \(host) when you save; nothing is written while drafting.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                } else {
                    Text("Saved to this Mac's library; Sync to hosts writes it elsewhere.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
            Spacer()
        }
    }

    // MARK: Description

    private var describeSection: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("What should the line-up look like?")
                .font(.headline)
            TextEditor(text: $session.description)
                .font(.body)
                .frame(minHeight: 72, maxHeight: 110)
                .padding(6)
                .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 8))
                .overlay {
                    RoundedRectangle(cornerRadius: 8)
                        .stroke(Color(nsColor: .separatorColor).opacity(0.5), lineWidth: 0.5)
                }
                .disabled(session.isDrafting)

            PipelineFlowLayout(spacing: 6) {
                Text("Try:")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .padding(.vertical, 3)
                ForEach(PipelineComposerSession.examples, id: \.self) { example in
                    Button(example) { session.useExample(example) }
                        .buttonStyle(.bordered)
                        .controlSize(.small)
                        .disabled(session.isDrafting)
                }
            }

            HStack(spacing: 10) {
                Picker("Provider", selection: $session.providerID) {
                    ForEach(session.providers) { provider in
                        Text(provider.title).tag(provider.id)
                    }
                }
                .frame(maxWidth: 280)
                .disabled(session.isDrafting)
                Spacer()
                if session.isDrafting {
                    ProgressView().controlSize(.small)
                    Text("Asking \(session.providerTitle)…")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Button("Cancel") { session.cancel() }
                        .controlSize(.small)
                } else {
                    Button {
                        session.draftPipeline()
                    } label: {
                        Label(session.hasDraft ? "Draft again" : "Draft pipeline", systemImage: "sparkles")
                    }
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut(.return, modifiers: .command)
                    .disabled(!session.canDraft)
                    .help("Ask the provider for a pipeline that matches the description (⌘↩)")
                }
            }

            backendNotice

            if let error = session.errorMessage {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.red)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    @ViewBuilder
    private var backendNotice: some View {
        switch session.backend {
        case .checking:
            Text("Checking whether muxad can draft pipelines…")
                .font(.caption2)
                .foregroundStyle(.tertiary)
        case .daemon:
            EmptyView()
        case .bundledCLI:
            Text("The running muxad predates in-app drafting; the bundled muxa CLI asks the provider instead.")
                .font(.caption2)
                .foregroundStyle(.tertiary)
        case .unavailable:
            HStack(spacing: 8) {
                Label("Update muxad to draft pipelines", systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.orange)
                Spacer()
                Button("Describe in a Shell tab instead…") { describeInShell() }
                    .controlSize(.small)
                    .help("Runs the interactive `muxa work init` wizard in a Shell tab")
            }
        }
    }

    // MARK: Result

    private var resultSection: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Text("Draft")
                    .font(.headline)
                TextField("Name", text: $session.name, prompt: Text("for example implement-review"))
                    .labelsHidden()
                    .font(.body.monospaced())
                    .frame(maxWidth: 260)
                    .disabled(session.isDrafting)
                Spacer()
                if let draft = session.draft {
                    Text("\(draft.agents.count) agents")
                        .font(.caption2.monospacedDigit())
                        .foregroundStyle(.tertiary)
                }
            }
            if let description = session.draft?.description, !description.isEmpty {
                Text(description)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }

            PipelineStagesView(agents: session.draft?.optionsAgents ?? [])
                .padding(10)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: 10))

            VStack(alignment: .leading, spacing: 6) {
                ForEach(session.draft?.agents ?? []) { agent in
                    ComposerAgentRow(agent: agent)
                }
            }

            if !session.notes.isEmpty {
                VStack(alignment: .leading, spacing: 4) {
                    Label("Notes from \(session.providerTitle)", systemImage: "text.quote")
                        .font(.caption.weight(.semibold))
                        .foregroundStyle(.secondary)
                    Text(session.notes)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .padding(10)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 8))
            }

            if !session.problems.isEmpty {
                VStack(alignment: .leading, spacing: 4) {
                    ForEach(session.problems, id: \.self) { problem in
                        Label(problem, systemImage: "exclamationmark.circle")
                            .font(.caption)
                            .foregroundStyle(.orange)
                    }
                }
            }

            HStack(spacing: 8) {
                TextField("Refinement", text: $session.refinement, prompt: Text("Refine: for example make the reviewer use gemini"))
                    .labelsHidden()
                    .onSubmit { session.refine() }
                    .disabled(session.isDrafting)
                Button {
                    session.refine()
                } label: {
                    Label("Refine", systemImage: "arrow.uturn.forward")
                }
                .controlSize(.small)
                .disabled(!session.canRefine)
                .help("Send a follow-up; the current draft goes along so the provider edits it instead of starting over")
            }

            if !session.history.isEmpty {
                VStack(alignment: .leading, spacing: 3) {
                    ForEach(session.history) { step in
                        Label(step.request, systemImage: "checkmark.circle")
                            .font(.caption2)
                            .foregroundStyle(.tertiary)
                            .lineLimit(1)
                    }
                }
            }
        }
    }

    // MARK: Footer

    private var footer: some View {
        VStack(alignment: .leading, spacing: 8) {
            if let saveError {
                Label(saveError, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.red)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            }
            HStack(spacing: 10) {
                if session.hasDraft, routesAreEmpty {
                    Toggle("Also add route .* → this pipeline", isOn: $addsCatchAllRoute)
                        .toggleStyle(.checkbox)
                        .font(.caption)
                        .help("Adds a catch-all route so every Work id uses this pipeline")
                }
                Spacer()
                if isSaving { ProgressView().controlSize(.small) }
                Button("Cancel") { close() }
                    .keyboardShortcut(.cancelAction)
                    .disabled(isSaving)
                Button {
                    openInEditor()
                } label: {
                    Label("Open in Editor", systemImage: "slider.horizontal.3")
                }
                .disabled(!session.hasDraft || session.isDrafting || isSaving)
                .help("Continue in the visual editor; the draft is not saved until you create it there")
                Button {
                    save()
                } label: {
                    Label("Save to Library", systemImage: "square.and.arrow.down")
                }
                .buttonStyle(.borderedProminent)
                .disabled(!session.canSave || isSaving)
            }
        }
    }

    // MARK: Actions

    private func save() {
        guard let draft = session.draft, session.canSave, !isSaving else { return }
        let name = session.trimmedName
        let host = target.host
        let addsRoute = addsCatchAllRoute && routesAreEmpty
        isSaving = true
        saveError = nil
        Task {
            defer { isSaving = false }
            guard await model.savePipeline(draft, named: name, host: host) else {
                saveError = model.pipelineEditorError ?? String(localized: "The pipeline could not be saved.")
                return
            }
            if addsRoute {
                let route = MuxaWorkRouteEdit(match: ".*", pipeline: name)
                guard await model.setRoute(route, host: host) else {
                    saveError = model.workOptionsError(for: host)
                        ?? String(localized: "The pipeline was saved, but the route could not be added.")
                    return
                }
            }
            onSaved?(name)
            close()
        }
    }

    private func openInEditor() {
        guard let pipeline = session.draftAsPipeline else { return }
        let host = target.host
        close()
        // The editor sheet hangs off the main window; a Start Work sheet in
        // front of it would hide the editor, so it steps aside.
        if model.isPresentingWorkStart { model.isPresentingWorkStart = false }
        model.presentPipelineEditor(draft: pipeline, host: host)
    }

    private func describeInShell() {
        close()
        if let shellFallback {
            shellFallback()
        } else {
            Task { await model.configureWork(cwd: nil) }
        }
    }

    private func close() {
        session.cancel()
        model.dismissPipelineComposer()
        dismiss()
    }
}

/// One agent of the draft: alias, program badge, role, and after edges.
private struct ComposerAgentRow: View {
    let agent: MuxaPipelineDefinition.Agent

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Text(verbatim: "@\(agent.alias)")
                .font(.subheadline.weight(.semibold))
                .frame(width: 110, alignment: .leading)
                .lineLimit(1)
            Text(agent.program)
                .font(.caption2.weight(.medium))
                .foregroundStyle(agentProgramTint(agent.program))
                .padding(.horizontal, 5)
                .padding(.vertical, 1)
                .background(agentProgramTint(agent.program).opacity(0.12), in: Capsule())
            if agent.role.isEmpty {
                Text("no role")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            } else {
                Text(agent.role)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 4)
            if !agent.after.isEmpty {
                Text("after \(agent.after.map { "@\($0)" }.joined(separator: ", "))")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
            }
        }
    }
}
