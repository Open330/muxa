import AppKit
import SwiftUI
import UniformTypeIdentifiers

/// The AIR Workbench module: muxa's pipelines as AIR workflows, and a
/// finished Work as AIR run evidence.
///
/// AIR describes, muxa runs — `docs/AIR.md` holds that judgment, and this
/// module is the whole of the boundary in the app. `config.toml` stays
/// authoritative: an export is a copy, and an import lands in the pipeline
/// editor after `MuxaPipelineDefinition.problems()` has accepted it, so an
/// AIR file can never write a line-up that would not launch.
///
/// Nothing here is on any critical path. Switched off, muxa behaves as if
/// AIR did not exist; switched on with Node missing, the pane says so and
/// contributes no actions.
@MainActor
final class AirModule: MuxaModule, ObservableObject {
    nonisolated static let identity = MuxaModuleIdentity(
        id: "air",
        title: "AIR Workbench",
        blurb: String(localized: "Open a pipeline as a graph you can edit, and hand a finished Work's run evidence to anyone — in AIR, the Agent Intermediate Representation."),
        symbolName: "point.3.connected.trianglepath.dotted",
        executable: "node",
        homepage: URL(string: "https://github.com/jiunbae/air")
    )

    /// Where the operator put their `air-workbench` checkout. AIR has no
    /// standard install location yet, so this is asked for rather than found.
    nonisolated static let workbenchPathKey = "muxa.modules.air.workbenchPath"
    /// AIR Workbench's own floor.
    nonisolated static let minimumNode = (major: 22, minor: 22, patch: 0)

    /// One line of outcome under the module's settings. Failures are shown
    /// here, never in an alert: an export that went wrong is information,
    /// not an interruption.
    struct Notice: Equatable, Sendable {
        enum Tone: Equatable, Sendable { case done, failed }
        let tone: Tone
        let text: String
        /// Extra lines, e.g. the participants a trace could not describe.
        var details: [String] = []
    }

    @Published private(set) var availability: MuxaModuleAvailability = .probing

    /// The converter is Swift: exporting and importing a workflow needs
    /// nothing installed. Only opening Workbench needs Node.
    let worksWithoutTool = true
    @Published private(set) var notice: Notice?
    @Published private(set) var isWorking = false
    @Published private(set) var workbenchURL: URL?

    private let defaults: UserDefaults
    private var workbench: AirWorkbenchProcess?

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    var workbenchPath: String {
        get { defaults.string(forKey: Self.workbenchPathKey) ?? "" }
        set {
            defaults.set(newValue, forKey: Self.workbenchPathKey)
            objectWillChange.send()
            // Through the registry rather than `probe()` directly: the card
            // above this pane draws the availability line, and it watches the
            // registry's probe generation rather than the module.
            Task { await MuxaModuleRegistry.shared.probe(self) }
        }
    }

    /// The producer name written into every artifact's provenance.
    var producerVersion: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0.1.0"
    }

    // MARK: - Probe

    func probe() async {
        let output: MuxaModuleProcess.Output
        do {
            output = try await MuxaModuleProcess.run("node", ["--version"], timeout: 10)
        } catch MuxaModuleProcess.Failure.notFound {
            availability = .missing(hint: String(localized: "Install Node.js 22.22 or newer"))
            return
        } catch {
            availability = .unusable(reason: error.localizedDescription)
            return
        }
        guard output.succeeded, let version = Self.nodeVersion(output.stdout) else {
            availability = .unusable(reason: String(localized: "node did not report a version."))
            return
        }
        let spelled = "\(version.major).\(version.minor).\(version.patch)"
        guard Self.meetsMinimum(version) else {
            availability = .unusable(
                reason: String(localized: "node \(spelled) is too old — AIR Workbench needs 22.22 or newer.")
            )
            return
        }
        let folder = workbenchPath
        guard !folder.isEmpty else {
            availability = .missing(hint: String(localized: "Choose your air-workbench folder"))
            return
        }
        guard FileManager.default.isReadableFile(atPath: Self.scriptPath(in: folder)) else {
            availability = .unusable(reason: String(localized: "That folder has no scripts/air.mjs."))
            return
        }
        availability = .available(version: "node \(spelled)", detail: Self.abbreviate(folder))
    }

    /// `node --version` prints `v22.22.0`. A build tag (`v23.0.0-nightly…`)
    /// keeps its three numbers and loses the rest.
    nonisolated static func nodeVersion(_ output: String) -> (major: Int, minor: Int, patch: Int)? {
        let trimmed = output.trimmingCharacters(in: .whitespacesAndNewlines)
        let body = trimmed.hasPrefix("v") ? String(trimmed.dropFirst()) : trimmed
        let numbers = body
            .prefix { $0.isNumber || $0 == "." }
            .split(separator: ".", omittingEmptySubsequences: false)
            .compactMap { Int($0) }
        guard let major = numbers.first else { return nil }
        return (major, numbers.count > 1 ? numbers[1] : 0, numbers.count > 2 ? numbers[2] : 0)
    }

    nonisolated static func meetsMinimum(_ version: (major: Int, minor: Int, patch: Int)) -> Bool {
        (version.major, version.minor, version.patch)
            >= (minimumNode.major, minimumNode.minor, minimumNode.patch)
    }

    nonisolated static func scriptPath(in folder: String) -> String {
        (folder as NSString).appendingPathComponent("scripts/air.mjs")
    }

    nonisolated static func abbreviate(_ path: String) -> String {
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        return path.hasPrefix(home + "/") ? "~" + path.dropFirst(home.count) : path
    }

    // MARK: - Settings

    func settingsPane(model: AppModel) -> AnyView {
        AnyView(AirSettingsPane(module: self))
    }

    // MARK: - Actions

    func actions(for context: MuxaModuleContext, model: AppModel) -> [MuxaModuleAction] {
        let busy = isWorking ? String(localized: "Still finishing the last one.") : nil
        switch context {
        case .pipeline(let pipeline, _):
            return [
                MuxaModuleAction(
                    id: "air.workbench",
                    title: "Open in AIR Workbench",
                    symbolName: "point.3.connected.trianglepath.dotted",
                    disabledReason: busy
                ) { [weak self] in
                    guard let self else { return }
                    await self.openInWorkbench(pipeline)
                },
                MuxaModuleAction(
                    id: "air.export",
                    title: "Export AIR workflow…",
                    symbolName: "square.and.arrow.up",
                    disabledReason: busy
                ) { [weak self] in
                    self?.exportWorkflow(pipeline)
                },
            ]
        case .work(let work):
            return [
                MuxaModuleAction(
                    id: "air.trace",
                    title: "Export run evidence…",
                    symbolName: "doc.badge.clock",
                    disabledReason: busy
                ) { [weak self] in
                    self?.exportEvidence(work)
                },
            ]
        case .app:
            return [
                MuxaModuleAction(
                    id: "air.import",
                    title: "Import AIR workflow…",
                    symbolName: "square.and.arrow.down",
                    disabledReason: busy
                ) { [weak self] in
                    self?.importWorkflow(into: model)
                },
            ]
        case .agent:
            return []
        }
    }

    // MARK: Pipeline → AIR

    private func workflow(for pipeline: MuxaWorkOptions.Pipeline) -> AirDocument {
        AirWorkflow.document(
            name: pipeline.name,
            MuxaPipelineDefinition(pipeline),
            producer: producerVersion
        )
    }

    func exportWorkflow(_ pipeline: MuxaWorkOptions.Pipeline) {
        let panel = NSSavePanel()
        panel.title = String(localized: "Export AIR workflow")
        panel.prompt = String(localized: "Export")
        panel.nameFieldStringValue = "\(pipeline.name).air.json"
        panel.allowedContentTypes = [.json]
        guard panel.runModal() == .OK, let url = panel.url else { return }
        do {
            try workflow(for: pipeline).fileData.write(to: url, options: .atomic)
            notice = Notice(
                tone: .done,
                text: String(localized: "Exported \(pipeline.name) to \(url.lastPathComponent).")
            )
        } catch {
            notice = Notice(tone: .failed, text: error.localizedDescription)
        }
    }

    /// Writes the workflow to a scratch file and asks AIR Workbench to open
    /// it. Workbench serves a loopback page and stays open until it is
    /// stopped, so the first URL it prints is what the browser gets.
    func openInWorkbench(_ pipeline: MuxaWorkOptions.Pipeline) async {
        guard !isWorking else { return }
        let folder = workbenchPath
        guard !folder.isEmpty else {
            notice = Notice(tone: .failed, text: String(localized: "Choose your air-workbench folder first."))
            return
        }
        isWorking = true
        notice = nil
        defer { isWorking = false }
        do {
            let artifact = try scratchFile(named: "\(pipeline.name).air.json")
            try workflow(for: pipeline).fileData.write(to: artifact, options: .atomic)
            stopWorkbench()
            let session = AirWorkbenchProcess()
            let url = try await session.start(
                script: Self.scriptPath(in: folder),
                workingDirectory: folder,
                artifact: artifact.path
            )
            workbench = session
            workbenchURL = url
            NSWorkspace.shared.open(url)
            notice = Notice(
                tone: .done,
                text: String(localized: "AIR Workbench is serving \(pipeline.name) at \(url.host ?? "127.0.0.1").")
            )
        } catch {
            notice = Notice(tone: .failed, text: error.localizedDescription)
        }
    }

    func stopWorkbench() {
        workbench?.stop()
        workbench = nil
        workbenchURL = nil
    }

    // MARK: AIR → pipeline

    func importWorkflow(into model: AppModel) {
        let panel = NSOpenPanel()
        panel.title = String(localized: "Import AIR workflow")
        panel.prompt = String(localized: "Import")
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.allowedContentTypes = [.json]
        guard panel.runModal() == .OK, let url = panel.url else { return }
        do {
            let document = try AirDocument.decode(try Data(contentsOf: url))
            let pipeline = try AirWorkflow.pipeline(from: document)
            notice = Notice(
                tone: .done,
                text: String(localized: "Imported \(pipeline.name) from \(url.lastPathComponent). Review it before saving.")
            )
            model.presentPipelineEditor(draft: pipeline, host: nil)
        } catch let error as AirError {
            notice = Notice(
                tone: .failed,
                text: error.localizedDescription,
                details: {
                    if case .rejected(let problems) = error { return Array(problems.dropFirst()) }
                    return []
                }()
            )
        } catch {
            notice = Notice(tone: .failed, text: error.localizedDescription)
        }
    }

    // MARK: Work → AIR run evidence

    func exportEvidence(_ work: MuxaWorkGroup) {
        let result = AirTrace.exports(for: AirRunEvidence(work), producer: producerVersion)
        guard !result.exports.isEmpty else {
            notice = Notice(
                tone: .failed,
                text: String(localized: "No participant of this Work can be described as an AIR 1 trace."),
                details: result.skipped.map(\.reason)
            )
            return
        }
        let panel = NSOpenPanel()
        panel.title = String(localized: "Choose a folder for the run evidence")
        panel.prompt = String(localized: "Export")
        panel.message = String(localized: "One AIR trace is written per agent. Metadata only: no prompts, no output, no file contents.")
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        guard panel.runModal() == .OK, let folder = panel.url else { return }
        do {
            for export in result.exports {
                try export.document.fileData.write(
                    to: folder.appendingPathComponent(export.fileName),
                    options: .atomic
                )
            }
            notice = Notice(
                tone: .done,
                text: String(localized: "Wrote \(result.exports.count) AIR traces to \(folder.lastPathComponent)."),
                details: result.skipped.map(\.reason)
            )
        } catch {
            notice = Notice(tone: .failed, text: error.localizedDescription)
        }
    }

    // MARK: - Scratch

    private func scratchFile(named name: String) throws -> URL {
        let folder = FileManager.default.temporaryDirectory.appendingPathComponent("muxa-air", isDirectory: true)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        return folder.appendingPathComponent(name)
    }
}

// MARK: - What a Work leaves behind

extension AirRunEvidence {
    /// The metadata-only view of a Work. Everything else the app holds about
    /// these agents — prompts, recaps, last responses, titles, captures —
    /// has no field to land in, which is what makes the export safe by
    /// construction rather than by care.
    init(_ group: MuxaWorkGroup) {
        var participants: [Participant] = []
        if let run = group.pipelineRun, !run.desired.isEmpty {
            let stages = MuxaPipelineStages.stages(for: run.desired.map {
                MuxaWorkOptions.Agent(alias: $0.alias, program: $0.program, after: $0.after ?? [])
            })
            var stageOf: [String: Int] = [:]
            for (index, stage) in stages.enumerated() {
                for agent in stage { stageOf[agent.alias] = index + 1 }
            }
            for desired in run.desired {
                let state = run.aliases[desired.alias]
                let hosted = group.participants.first { $0.pane?.agentAlias == desired.alias }
                    ?? group.participants.first { $0.pane?.paneID == state?.pane }
                participants.append(Participant(
                    alias: desired.alias,
                    role: desired.role ?? "",
                    program: desired.program,
                    state: hosted?.agent.state ?? "",
                    status: state?.status ?? "",
                    stage: stageOf[desired.alias] ?? 1,
                    host: hosted?.host.alias ?? "",
                    pane: hosted?.pane?.paneID ?? state?.pane ?? "",
                    startedAt: hosted?.agent.startedAt ?? "",
                    stateEnteredAt: hosted?.agent.stateEnteredAt ?? "",
                    lastActivityAt: hosted?.agent.lastActivityAt ?? "",
                    after: desired.after ?? []
                ))
            }
        } else {
            for (index, hosted) in group.participants.enumerated() {
                let alias = hosted.pane?.agentAlias ?? ""
                participants.append(Participant(
                    alias: alias.isEmpty ? "agent-\(index + 1)" : alias,
                    role: hosted.pane?.agentRole ?? "",
                    program: MuxaAgentMark.known(for: hosted.agent.kind)?.program ?? hosted.agent.kind,
                    state: hosted.agent.state,
                    stage: 1,
                    host: hosted.host.alias,
                    pane: hosted.pane?.paneID ?? "",
                    startedAt: hosted.agent.startedAt ?? "",
                    stateEnteredAt: hosted.agent.stateEnteredAt ?? "",
                    lastActivityAt: hosted.agent.lastActivityAt ?? ""
                ))
            }
        }
        self.init(
            workspaceID: group.workspaceID,
            workID: group.workID,
            pipeline: group.pipelineRun?.pipeline ?? "",
            cwd: group.cwd ?? "",
            participants: participants
        )
    }
}

// MARK: - The Workbench server

/// `air.mjs workbench` is not a command that finishes: it binds a loopback
/// port, prints one tokenized URL, and serves until it is stopped. So it
/// cannot go through `MuxaModuleProcess.run`, which waits for an exit — this
/// reads the URL out of the running process and leaves it running.
final class AirWorkbenchProcess: @unchecked Sendable {
    enum Failure: LocalizedError {
        case noURL(String)
        case timedOut(TimeInterval)

        var errorDescription: String? {
            switch self {
            case .noURL(let detail):
                detail.isEmpty
                    ? String(localized: "AIR Workbench stopped without printing an address.")
                    : String(localized: "AIR Workbench stopped: \(detail)")
            case .timedOut(let seconds):
                String(localized: "AIR Workbench printed no address within \(Int(seconds)) seconds.")
            }
        }
    }

    private let process = Process()
    private let lock = NSLock()
    private var transcript = ""
    private var finished = false

    var isRunning: Bool { process.isRunning }

    func start(
        script: String,
        workingDirectory: String,
        artifact: String,
        timeout: TimeInterval = 45
    ) async throws -> URL {
        guard let node = await MuxaModuleProcess.resolve("node") else {
            throw MuxaModuleProcess.Failure.notFound("node")
        }
        return try await withCheckedThrowingContinuation { continuation in
            let out = Pipe()
            let err = Pipe()
            process.executableURL = URL(fileURLWithPath: node)
            process.arguments = [script, "workbench", artifact]
            process.currentDirectoryURL = URL(fileURLWithPath: workingDirectory, isDirectory: true)
            var environment = ProcessInfo.processInfo.environment
            environment["NO_COLOR"] = "1"
            process.environment = environment
            process.standardOutput = out
            process.standardError = err
            process.standardInput = FileHandle.nullDevice

            // Both pipes are drained for the life of the process: a server
            // that fills its stdout buffer stops serving.
            let scan: @Sendable (FileHandle) -> Void = { [weak self] handle in
                guard let self else { return }
                let data = handle.availableData
                guard !data.isEmpty else { handle.readabilityHandler = nil; return }
                self.lock.lock()
                if self.transcript.count < 64_000 {
                    self.transcript += String(decoding: data, as: UTF8.self)
                }
                let url = self.finished ? nil : Self.loopbackURL(in: self.transcript)
                if url != nil { self.finished = true }
                self.lock.unlock()
                if let url { continuation.resume(returning: url) }
            }
            out.fileHandleForReading.readabilityHandler = scan
            err.fileHandleForReading.readabilityHandler = scan

            process.terminationHandler = { [weak self] _ in
                guard let self else { return }
                self.lock.lock()
                let alreadyDone = self.finished
                self.finished = true
                let tail = self.transcript
                    .split(separator: "\n").last.map(String.init) ?? ""
                self.lock.unlock()
                if !alreadyDone { continuation.resume(throwing: Failure.noURL(tail)) }
            }

            do {
                try process.run()
            } catch {
                lock.lock()
                let alreadyDone = finished
                finished = true
                lock.unlock()
                if !alreadyDone {
                    continuation.resume(throwing: MuxaModuleProcess.Failure.launch(error.localizedDescription))
                }
                return
            }

            DispatchQueue.global(qos: .utility).asyncAfter(deadline: .now() + timeout) { [weak self] in
                guard let self else { return }
                self.lock.lock()
                let alreadyDone = self.finished
                self.finished = true
                self.lock.unlock()
                guard !alreadyDone else { return }
                self.stop()
                continuation.resume(throwing: Failure.timedOut(timeout))
            }
        }
    }

    func stop() {
        if process.isRunning { process.terminate() }
    }

    /// The first loopback address the server printed, token and all.
    static func loopbackURL(in output: String) -> URL? {
        let pattern = "https?://(?:127\\.0\\.0\\.1|localhost|\\[::1\\]):[0-9]+[^\\s]*"
        guard let regex = try? NSRegularExpression(pattern: pattern),
              let match = regex.firstMatch(
                  in: output,
                  range: NSRange(output.startIndex..., in: output)
              ),
              let range = Range(match.range, in: output)
        else { return nil }
        return URL(string: String(output[range]))
    }
}

// MARK: - The module's own settings

private struct AirSettingsPane: View {
    @ObservedObject var module: AirModule

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            VStack(alignment: .leading, spacing: 4) {
                Text("AIR Workbench folder")
                    .font(.subheadline.weight(.medium))
                HStack(spacing: 8) {
                    Text(verbatim: module.workbenchPath.isEmpty
                        ? String(localized: "Not chosen")
                        : AirModule.abbreviate(module.workbenchPath))
                        .font(.callout)
                        .foregroundStyle(module.workbenchPath.isEmpty ? .secondary : .primary)
                        .lineLimit(1)
                        .truncationMode(.middle)
                    Spacer(minLength: 8)
                    Button("Choose…") { choose() }
                        .controlSize(.small)
                    if !module.workbenchPath.isEmpty {
                        Button("Forget") { module.workbenchPath = "" }
                            .controlSize(.small)
                    }
                }
                Text("AIR has no standard install location yet, so point Muxa at the air-workbench folder of your AIR checkout — the one holding scripts/air.mjs.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }

            if let url = module.workbenchURL {
                Divider()
                HStack(spacing: 8) {
                    Label("Workbench is running", systemImage: "dot.radiowaves.left.and.right")
                        .font(.caption)
                        .foregroundStyle(.green)
                    Link(destination: url) {
                        Text("Open the page").font(.caption)
                    }
                    Spacer(minLength: 8)
                    Button("Stop") { module.stopWorkbench() }
                        .controlSize(.small)
                }
            }

            if let notice = module.notice {
                Divider()
                VStack(alignment: .leading, spacing: 3) {
                    Label(
                        notice.text,
                        systemImage: notice.tone == .done ? "checkmark.circle" : "exclamationmark.triangle.fill"
                    )
                    .font(.caption)
                    .foregroundStyle(notice.tone == .done ? Color.green : Color.orange)
                    .fixedSize(horizontal: false, vertical: true)
                    ForEach(notice.details, id: \.self) { detail in
                        Text(verbatim: detail)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
            }

            Text("Exports are copies. config.toml stays the pipeline muxa runs, and an import is checked the way the editor checks a pipeline before it reaches your config.")
                .font(.caption)
                .foregroundStyle(.tertiary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.title = String(localized: "Choose your air-workbench folder")
        panel.prompt = String(localized: "Choose")
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        if !module.workbenchPath.isEmpty {
            panel.directoryURL = URL(fileURLWithPath: module.workbenchPath, isDirectory: true)
        }
        if panel.runModal() == .OK, let url = panel.url {
            module.workbenchPath = url.path
        }
    }
}
