import AppKit
import SwiftUI

/// ⌥⇧⌘T: start one coding agent — which provider, where, and with what
/// first task — without a terminal. The quick start (⌥⌘T) opens this sheet
/// too whenever it cannot decide on its own.
struct NewAgentView: View {
    @ObservedObject var model: AppModel
    @ObservedObject var launcher: MuxaAgentLauncher

    @State private var program: MuxaAgentProgram = .claude
    @State private var programTouched = false
    /// "" is this Mac, like the Start Work sheet's host picker.
    @State private var host = ""
    @State private var directory = ""
    @State private var placement: MuxaAgentPlacementKind = .splitPane
    @State private var placementTouched = false
    @State private var windowSessionID = ""
    @State private var prompt = ""
    @State private var name = ""
    @State private var role = ""
    @State private var overridesOptions = false
    @State private var optionsText = ""
    @State private var makeDefault = false
    @State private var seeded = false

    private var hostAlias: String? { host.isEmpty ? nil : host }
    private var isLocalHost: Bool { hostAlias == nil }

    private var sessions: [MuxaWatchSession] { model.agentLaunchSessions(host: hostAlias) }

    private var availability: MuxaAgentPlacementAvailability {
        MuxaAgentPlacementAvailability(
            focus: launcher.focus,
            launchHost: hostAlias,
            sessionCount: sessions.count
        )
    }

    private var guide: MuxaLaunchSettings.LegacyGuide? { launcher.launchSettings?.legacyGuide }

    private var trimmedDirectory: String {
        directory.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    /// Nil when the directory is usable; otherwise why not. A remote path
    /// cannot be checked from here, so only its shape is.
    private var directoryProblem: String? {
        let path = (trimmedDirectory as NSString).expandingTildeInPath
        if trimmedDirectory.isEmpty { return String(localized: "Choose the folder the agent works in.") }
        guard path.hasPrefix("/") else { return String(localized: "Use an absolute path.") }
        guard isLocalHost else { return nil }
        var isDirectory: ObjCBool = false
        guard FileManager.default.fileExists(atPath: path, isDirectory: &isDirectory), isDirectory.boolValue else {
            return String(localized: "This folder does not exist on this Mac.")
        }
        return nil
    }

    private var canStart: Bool {
        model.isConnected && !launcher.isStarting && directoryProblem == nil
            && availability.contains(placement)
            && (placement != .newWindow || selectedWindowSession != nil)
    }

    private var selectedWindowSession: MuxaWatchSession? {
        // Keyed by the full identity: `$0`-style session ids repeat across
        // tmux servers.
        sessions.first { $0.id == windowSessionID } ?? sessions.first
    }

    private var effectiveOptions: [String] {
        if overridesOptions { return MuxaAgentLaunchOptionsText.split(optionsText) }
        return launcher.launchSettings?.effectiveOptions(program: program.rawValue, override: nil) ?? []
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            Form {
                Section("Agent") {
                    MuxaSegmented(
                        selection: Binding(
                            get: { program },
                            set: { program = $0; programTouched = true }
                        ),
                        options: MuxaAgentProgram.allCases,
                        label: { Text(verbatim: $0.displayName) }
                    )
                    programCaption
                    Toggle("Override provider options for this launch", isOn: $overridesOptions)
                    if overridesOptions {
                        TextField("For example --model opus", text: $optionsText)
                            .font(.system(.body, design: .monospaced))
                    }
                }

                Section("Where") {
                    hostPicker
                    directoryField
                    placementPicker
                }

                Section("Task") {
                    TextEditor(text: $prompt)
                        .font(.body)
                        .frame(minHeight: 72)
                        .overlay(alignment: .topLeading) {
                            if prompt.isEmpty {
                                Text("First prompt (optional). Leave empty to start an interactive agent.")
                                    .foregroundStyle(.tertiary)
                                    .padding(.top, 7)
                                    .padding(.leading, 5)
                                    .allowsHitTesting(false)
                            }
                        }
                    DisclosureGroup("More") {
                        TextField(
                            placement == .newSession ? "Session name (optional)" : "Window or session name (optional)",
                            text: $name
                        )
                        TextField("Role, for example reviewer (optional)", text: $role)
                            .disabled(placement == .native)
                        if placement == .native {
                            Text("A role is recorded on tmux panes; a Muxa terminal has none.")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                    }
                    Toggle("Use as default for ⌥⌘T", isOn: $makeDefault)
                }
            }
            .formStyle(.grouped)

            statusLine

            Divider()

            HStack {
                Text("Runs the bundled `muxa agent start` through muxad.")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
                Spacer()
                Button("Cancel") { launcher.isPresenting = false }
                    .buttonStyle(.muxaSecondary)
                    .keyboardShortcut(.cancelAction)
                    .disabled(launcher.isStarting)
                Button("Start Agent") { submit() }
                    .buttonStyle(.muxaPrimary)
                    .keyboardShortcut(.defaultAction)
                    .disabled(!canStart)
            }
            .padding(16)
        }
        .frame(width: 560, height: 640)
        .background {
            // ⌘↩ starts from anywhere in the sheet, like the other composers,
            // including while the prompt editor has focus.
            Button { if canStart { submit() } } label: { EmptyView() }
                .keyboardShortcut(.return, modifiers: .command)
                .hidden()
        }
        .onAppear(perform: seed)
        .onChange(of: launcher.launchSettings) { _ in applyConfiguredDefaults() }
        .onChange(of: host) { _ in
            if !placementTouched || !availability.contains(placement) { resolvePlacement() }
            windowSessionID = defaultWindowSessionID()
        }
    }

    private var header: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: "sparkles.rectangle.stack.fill")
                .font(.system(size: 28))
                .foregroundStyle(.tint)
            VStack(alignment: .leading, spacing: 3) {
                Text("New Agent")
                    .font(.system(size: 14, weight: .semibold))
                Text("Start one coding agent in a folder; it opens as a tab.")
                    .foregroundStyle(.secondary)
            }
            Spacer()
        }
        .padding(20)
    }

    @ViewBuilder
    private var programCaption: some View {
        VStack(alignment: .leading, spacing: 3) {
            let command = ([program.rawValue] + effectiveOptions)
            Text(verbatim: MuxaAgentLaunchOptionsText.join(command))
                .font(.system(.caption, design: .monospaced))
                .foregroundStyle(.secondary)
                .textSelection(.enabled)
            if isLocalHost, let installed = launcher.installedPrograms, !installed.contains(program.rawValue) {
                Label("`\(program.rawValue)` was not found on this Mac's PATH.", systemImage: "exclamationmark.triangle")
                    .font(.caption)
                    .foregroundStyle(.orange)
            }
        }
    }

    @ViewBuilder
    private var hostPicker: some View {
        let hosts = model.agentCapableHosts
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
            if !isLocalHost {
                Text("The agent starts in tmux on \(host) with that host's provider options.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private var directoryField: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                TextField(isLocalHost ? "Folder" : "Folder on \(host)", text: $directory)
                if !directorySuggestions.isEmpty {
                    Menu {
                        ForEach(directorySuggestions, id: \.self) { suggestion in
                            Button(suggestion) { directory = suggestion }
                        }
                    } label: {
                        Image(systemName: "clock.arrow.circlepath")
                    }
                    .menuStyle(.borderlessButton)
                    .fixedSize()
                    .help("Focused, recent, and running agents' folders")
                }
                if isLocalHost {
                    Button("Choose…", action: chooseDirectory)
                        .buttonStyle(.muxaGhost)
                }
            }
            if let problem = directoryProblem, !trimmedDirectory.isEmpty {
                Text(problem)
                    .font(.caption)
                    .foregroundStyle(.red)
            }
        }
    }

    @ViewBuilder
    private var placementPicker: some View {
        VStack(alignment: .leading, spacing: 6) {
            MuxaSegmented(
                selection: Binding(
                    get: { placement },
                    set: { placement = $0; placementTouched = true }
                ),
                options: availability.kinds,
                label: { Text(verbatim: $0.title) }
            )
            switch placement {
            case .splitPane:
                if let pane = launcher.focus.pane {
                    Text("Splits \(pane.paneID) in \(pane.sessionName), \(guide?.direction == "down" ? String(localized: "below") : String(localized: "to the right")).")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            case .newWindow:
                Picker("Session", selection: $windowSessionID) {
                    ForEach(sessions) { session in
                        Text(verbatim: session.name.isEmpty ? session.sessionID : session.name)
                            .tag(session.id)
                    }
                }
            case .newSession:
                Text("A detached tmux session named after the folder.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            case .native:
                Text("A terminal owned by muxad on this Mac, like a Muxa shell.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            if !availability.splitPane, placement != .splitPane {
                Text("Split pane needs a focused tmux pane on this host.")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }
        }
    }

    @ViewBuilder
    private var statusLine: some View {
        if let error = launcher.error {
            Label(error, systemImage: "exclamationmark.triangle.fill")
                .font(.caption)
                .foregroundStyle(.red)
                .textSelection(.enabled)
                .padding(.horizontal, 20)
                .padding(.bottom, 8)
                .frame(maxWidth: .infinity, alignment: .leading)
        } else if !model.isConnected {
            Text("Connect to muxad first.")
                .font(.caption)
                .foregroundStyle(.secondary)
                .padding(.horizontal, 20)
                .padding(.bottom, 8)
                .frame(maxWidth: .infinity, alignment: .leading)
        } else if let status = launcher.status {
            HStack(spacing: 8) {
                if launcher.isStarting { ProgressView().controlSize(.small) }
                Text(status)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            .padding(.horizontal, 20)
            .padding(.bottom, 8)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }

    /// The focused folder first, then recent launch folders (this Mac), then
    /// where agents on the chosen host already run.
    private var directorySuggestions: [String] {
        var seen = Set<String>()
        var suggestions: [String] = []
        let focused = launcher.focus.hostAlias == hostAlias ? launcher.focus.cwd : nil
        let recent = isLocalHost ? launcher.recentDirectories : []
        let running = model.hostedAgents
            .filter { isLocalHost ? $0.host.local : $0.host.alias == host }
            .compactMap(\.agent.cwd)
        for candidate in [focused].compactMap({ $0 }) + recent + running
        where !candidate.isEmpty && seen.insert(candidate).inserted {
            suggestions.append(candidate)
        }
        return Array(suggestions.prefix(15))
    }

    private func seed() {
        guard !seeded else { return }
        seeded = true
        let focus = launcher.focus
        let remembered = launcher.rememberedDefaults
        if let focusHost = focus.hostAlias,
           model.agentCapableHosts.contains(where: { $0.alias == focusHost }) {
            host = focusHost
        }
        directory = focus.cwd ?? (isLocalHost ? launcher.recentDirectories.first ?? "" : "")
        if let options = remembered.options {
            overridesOptions = true
            optionsText = MuxaAgentLaunchOptionsText.join(options)
        }
        applyConfiguredDefaults()
        windowSessionID = defaultWindowSessionID()
    }

    /// Remembered choices first, then `[mcp.guide]` — reapplied when the
    /// config arrives, unless the operator already chose.
    private func applyConfiguredDefaults() {
        let remembered = launcher.rememberedDefaults
        if !programTouched, let preferred = remembered.program(guide: guide) {
            program = preferred
        }
        if !placementTouched { resolvePlacement() }
    }

    private func resolvePlacement() {
        placement = availability.resolve(
            remembered: launcher.rememberedDefaults.placement,
            guidePlacement: guide?.placement
        )
    }

    private func defaultWindowSessionID() -> String {
        if let pane = launcher.focus.pane, launcher.focus.hostAlias == hostAlias,
           let session = sessions.first(where: { $0.sessionID == pane.sessionID && $0.socket == pane.socket }) {
            return session.id
        }
        return sessions.first?.id ?? ""
    }

    private func chooseDirectory() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        let current = (trimmedDirectory as NSString).expandingTildeInPath
        panel.directoryURL = current.hasPrefix("/")
            ? URL(fileURLWithPath: current, isDirectory: true)
            : FileManager.default.homeDirectoryForCurrentUser
        if panel.runModal() == .OK, let url = panel.url {
            directory = url.path
        }
    }

    private func submit() {
        guard canStart else { return }
        let options = overridesOptions ? MuxaAgentLaunchOptionsText.split(optionsText) : nil
        if makeDefault {
            MuxaAgentLaunchDefaults(program: program, placement: placement, options: options)
                .save(to: launcher.defaults)
        }
        let window = selectedWindowSession.map { (id: $0.sessionID, socket: $0.socket) }
        let cwd = (trimmedDirectory as NSString).expandingTildeInPath
        let program = program
        let placement = placement
        let hostAlias = hostAlias
        let focus = launcher.focus
        let prompt = prompt
        let name = name
        let role = role
        let direction = guide?.direction
        Task {
            let environment = placement == .native ? await model.nativeAgentEnvironment() : [:]
            let request = MuxaAgentStartRequest.make(
                program: program,
                hostAlias: hostAlias,
                placement: placement,
                focus: focus,
                windowSession: placement == .newWindow ? window : nil,
                cwd: cwd,
                prompt: prompt,
                name: name,
                role: role,
                options: options,
                guideDirection: direction,
                environment: environment
            )
            if await model.startAgent(request) == nil {
                launcher.isPresenting = false
            }
        }
    }
}

/// Presents the New Agent sheet and opens a started agent's editor pinned.
/// ContentView owns the tab model, so it passes its own `openEditor`.
struct MuxaNewAgentPresentation: ViewModifier {
    @ObservedObject var launcher: MuxaAgentLauncher
    let model: AppModel
    let openEditor: (MuxaSidebarSelection) -> Void

    func body(content: Content) -> some View {
        content
            .sheet(isPresented: $launcher.isPresenting) {
                NewAgentView(model: model, launcher: launcher)
            }
            .onChange(of: launcher.pendingEditor) { selection in
                guard let selection else { return }
                launcher.pendingEditor = nil
                openEditor(selection)
            }
    }
}

extension View {
    func muxaNewAgentPresentation(
        model: AppModel,
        openEditor: @escaping (MuxaSidebarSelection) -> Void
    ) -> some View {
        modifier(MuxaNewAgentPresentation(launcher: model.agentLauncher, model: model, openEditor: openEditor))
    }
}
