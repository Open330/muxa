import Foundation

#if canImport(FoundationModels)
import FoundationModels
#endif

// muxa-afm: the bridge muxad's `apple` Ask engine spawns.
//
// Apple's Foundation Models are a Swift-only, in-process framework, so the
// Rust daemon cannot call them and there is no vendor CLI to drive the way it
// drives `claude -p`. This helper is that CLI. It keeps no state: muxad owns
// the conversation and replays it on every turn, exactly as it does for the
// HTTPS API engines.
//
//   muxa-afm            one turn: request JSON on stdin, answer JSON on stdout
//   muxa-afm --probe    availability JSON on stdout, for Settings › Providers
//   muxa-afm --version
//
// A failed turn exits non-zero with the reason as the last line of stderr,
// which is the line muxad reports back to the person who asked.

/// Bumped when the stdin/stdout contract changes shape.
let protocolVersion = 1

struct TurnRequest: Decodable {
    struct Exchange: Decodable {
        let prompt: String
        let answer: String
    }

    let prompt: String
    /// Prior turns of the conversation, oldest first.
    var history: [Exchange]?
    /// `on-device` (default) or `private-cloud`.
    var model: String?
    var instructions: String?
    /// How to read the workspace back from muxad. Present on a Global Ask
    /// turn; absent on a one-shot drafting turn, which gets a bare model.
    var muxa: Workspace?
}

/// The daemon this turn came from, as the `muxa` CLI reaches it.
struct Workspace: Decodable, Sendable {
    let socket: String
    var config: String?
    /// The CLI to run; defaults to the `muxa` beside this helper.
    var cli: String?
}

struct TurnResponse: Encodable {
    let result: String
    let model: String
    /// Names of the workspace tools the model called, in order.
    let toolsUsed: [String]

    enum CodingKeys: String, CodingKey {
        case result, model
        case toolsUsed = "tools_used"
    }
}

struct ProbeResponse: Encodable {
    struct Model: Encodable {
        let id: String
        let available: Bool
        /// Stable machine-readable reason when unavailable.
        let reason: String?
        let message: String?
        let contextSize: Int?

        enum CodingKeys: String, CodingKey {
            case id, available, reason, message
            case contextSize = "context_size"
        }
    }

    let protocolVersion: Int
    let models: [Model]

    enum CodingKeys: String, CodingKey {
        case protocolVersion = "protocol_version"
        case models
    }
}

enum ModelChoice: String {
    case onDevice = "on-device"
    case privateCloud = "private-cloud"

    init?(configured: String?) {
        switch configured?.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() {
        case nil, "", "default", "on-device", "ondevice", "system":
            self = .onDevice
        case "private-cloud", "private-cloud-compute", "pcc":
            self = .privateCloud
        default:
            return nil
        }
    }

    static let accepted = "on-device, private-cloud"
}

enum ExitCode: Int32 {
    case failed = 1
    case badRequest = 2
    case unavailable = 3
}

struct HelperFailure: Error {
    let code: ExitCode
    let reason: String
    let message: String
}

func writeJSON(_ value: some Encodable) {
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
    // Encoding these plain value types cannot fail.
    let data = (try? encoder.encode(value)) ?? Data("{}".utf8)
    FileHandle.standardOutput.write(data)
    FileHandle.standardOutput.write(Data("\n".utf8))
}

func fail(_ failure: HelperFailure) -> Never {
    FileHandle.standardError.write(Data((failure.message + "\n").utf8))
    exit(failure.code.rawValue)
}

let osTooOld = HelperFailure(
    code: .unavailable,
    reason: "os_too_old",
    message: "Apple Intelligence needs macOS 26 or later"
)
let privateCloudTooOld = HelperFailure(
    code: .unavailable,
    reason: "os_too_old",
    message: "the private-cloud model needs macOS 27 or later"
)
let builtWithoutSDK = HelperFailure(
    code: .unavailable,
    reason: "sdk_missing",
    message: "this muxa-afm was built without the Foundation Models SDK"
)
let builtWithoutPrivateCloud = HelperFailure(
    code: .unavailable,
    reason: "sdk_missing",
    message: "this muxa-afm was built with a macOS 26 SDK, which has no private-cloud model; use on-device"
)

// `#available` asks what the Mac running this can do. Whether a symbol exists
// to compile against is a different question, and a release runner answers it
// with an older Xcode than a developer's Mac: Private Cloud Compute,
// `LanguageModelError` and friends arrived with the macOS 27 SDK, which is
// FoundationModels 2.x. Built against a macOS 26 SDK the helper still answers
// on-device and says plainly that it has no private-cloud model.
#if canImport(FoundationModels, _version: 2)
let hasMacOS27SDK = true
#else
let hasMacOS27SDK = false
#endif

// MARK: - Reading the workspace

/// What the model is told when it can read the workspace. Without this it
/// answers, truthfully, that it cannot see any other application.
///
/// The replayed conversation may hold an earlier turn in which the model said,
/// truthfully at the time, that it could not see other applications; a small
/// model then repeats itself rather than reach for a tool it has since been
/// given. So the instructions say outright that such an answer is out of date.
let workspaceInstructions = """
    You are the assistant in Global Ask, a feature of muxa. muxa is an app that \
    tracks the AI coding agents (Claude Code, Codex, Gemini CLI and others) the \
    user runs in terminal panes on this Mac. You have tools that read those \
    agent sessions: list_agent_sessions and read_agent_session. Earlier turns \
    of this conversation may say you cannot access other applications or \
    sessions; that was before you had these tools and is no longer true. \
    Whenever a question is about the user's agents, sessions, panes, or what \
    they are working on, call list_agent_sessions first — do not answer from \
    memory or repeat an earlier refusal — then read_agent_session for more \
    about one pane. You can only read: you cannot send input to an agent or \
    change anything. Answer in the language the user writes in.
    """

/// Whether an earlier answer is the model's own boilerplate for "I cannot
/// see other applications" — true when it was given, before the workspace
/// tools existed, and poison once replayed: a small model keeps the story it
/// told three turns ago sooner than reach for a tool it has since been
/// handed, and no wording of the instructions overrides three of them in a
/// row. Such a turn is dropped from the replay. Only a short answer that is
/// nothing but the refusal qualifies; a long answer that mentions the phrase
/// is an answer, and stays.
func isStaleRefusal(_ answer: String) -> Bool {
    let text = answer.lowercased()
    guard text.count <= 400 else { return false }
    let refusals = [
        "cannot access external applications",
        "cannot directly access external applications",
        "cannot access other applications",
        "can only work with the information provided in this conversation",
        "unable to access external applications",
        "do not have access to external applications",
    ]
    return refusals.contains { text.contains($0) }
}

/// The history a turn replays when the workspace can be read: the same
/// exchanges, minus the stale refusals. Without tools every turn stays —
/// the model's earlier answer was as true as it will be now.
func replayableHistory(_ history: [TurnRequest.Exchange], hasTools: Bool) -> [TurnRequest.Exchange] {
    guard hasTools else { return history }
    return history.filter { !isStaleRefusal($0.answer) }
}

/// One tracked agent, flattened out of `muxa status --json`.
struct AgentSession {
    let pane: String
    let session: String
    let window: String
    let kind: String
    let state: String
    let title: String
    let cwd: String
    let lastPrompt: String
    let lastResponse: String
    let recap: String
}

func clipped(_ text: String, to limit: Int) -> String {
    let flat = text.split(whereSeparator: \.isNewline).joined(separator: " ")
    return flat.count <= limit ? flat : String(flat.prefix(limit)) + "…"
}

/// The agents in a `muxa status --json` snapshot. Panes with no agent — a
/// plain shell — are not sessions and are left out.
func agentSessions(fromStatusJSON data: Data) -> [AgentSession] {
    guard let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
          let sessions = root["sessions"] as? [[String: Any]] else { return [] }
    var found: [AgentSession] = []
    for session in sessions {
        for window in session["windows"] as? [[String: Any]] ?? [] {
            for pane in window["panes"] as? [[String: Any]] ?? [] {
                guard let agent = pane["agent"] as? [String: Any] else { continue }
                let text = { (key: String) in agent[key] as? String ?? "" }
                let paneID = (pane["key"] as? [String: Any])?["pane_id"] as? String ?? text("pane")
                let title = [text("ai_title"), pane["title"] as? String ?? ""].first { !$0.isEmpty } ?? ""
                found.append(AgentSession(
                    pane: paneID,
                    session: session["name"] as? String ?? "",
                    window: window["name"] as? String ?? "",
                    kind: text("kind"),
                    state: text("state"),
                    title: title,
                    cwd: text("cwd"),
                    lastPrompt: text("last_prompt"),
                    lastResponse: text("last_response"),
                    recap: text("recap")
                ))
            }
        }
    }
    return found
}

/// The list the model reads first. The on-device window is 8,192 tokens for
/// everything, so each agent gets a few short lines and the whole list a
/// ceiling; past it the rest are counted rather than described.
func sessionsDigest(_ sessions: [AgentSession], budget: Int = 2_600) -> String {
    guard !sessions.isEmpty else { return "muxa is tracking no agent sessions right now." }
    var lines = ["\(sessions.count) agent session(s):"]
    var used = 0
    for (index, agent) in sessions.enumerated() {
        var entry = "- pane \(agent.pane) · \(agent.kind) · \(agent.state)"
        if !agent.title.isEmpty { entry += " · \(clipped(agent.title, to: 80))" }
        if !agent.cwd.isEmpty { entry += " · \(agent.cwd)" }
        if !agent.lastPrompt.isEmpty { entry += "\n  last prompt: \(clipped(agent.lastPrompt, to: 160))" }
        if !agent.lastResponse.isEmpty { entry += "\n  last reply: \(clipped(agent.lastResponse, to: 160))" }
        if used + entry.count > budget {
            lines.append("- …and \(sessions.count - index) more; ask for one by pane id.")
            break
        }
        used += entry.count
        lines.append(entry)
    }
    return lines.joined(separator: "\n")
}

/// Everything known about one pane, for the follow-up question.
func sessionDetail(_ agent: AgentSession, recentPrompts: String?) -> String {
    var lines = [
        "pane \(agent.pane) · \(agent.kind) · \(agent.state)",
        "tmux session \(agent.session), window \(agent.window)",
    ]
    if !agent.title.isEmpty { lines.append("title: \(clipped(agent.title, to: 160))") }
    if !agent.cwd.isEmpty { lines.append("working directory: \(agent.cwd)") }
    if !agent.recap.isEmpty { lines.append("recap: \(clipped(agent.recap, to: 700))") }
    if !agent.lastPrompt.isEmpty { lines.append("last prompt: \(clipped(agent.lastPrompt, to: 600))") }
    if !agent.lastResponse.isEmpty { lines.append("last reply: \(clipped(agent.lastResponse, to: 900))") }
    if let recentPrompts, !recentPrompts.isEmpty {
        lines.append("recent prompts:\n\(String(recentPrompts.prefix(1_200)))")
    }
    return lines.joined(separator: "\n")
}

/// `%3` and `3` name the same pane.
func matches(_ agent: AgentSession, pane wanted: String) -> Bool {
    let bare = { (id: String) in id.trimmingCharacters(in: .whitespaces).drop { $0 == "%" } }
    return bare(agent.pane) == bare(wanted)
}

extension Workspace {
    /// The `muxa` to run: the one named, else the one beside this helper —
    /// where it sits in Muxa.app — else whatever PATH offers.
    var cliArguments: [String] {
        if let cli, !cli.isEmpty { return [cli] }
        if let beside = Bundle.main.executableURL?.deletingLastPathComponent().appendingPathComponent("muxa"),
           FileManager.default.isExecutableFile(atPath: beside.path) {
            return [beside.path]
        }
        return ["/usr/bin/env", "muxa"]
    }

    /// Runs the CLI against this turn's daemon; nil when it fails or takes
    /// longer than a status read ever should.
    func run(_ arguments: [String]) -> Data? {
        let command = cliArguments
        let process = Process()
        process.executableURL = URL(fileURLWithPath: command[0])
        process.arguments = Array(command.dropFirst()) + arguments
        var environment = ProcessInfo.processInfo.environment
        environment["MUXA_SOCKET"] = socket
        if let config, !config.isEmpty { environment["MUXA_CONFIG"] = config }
        environment["NO_COLOR"] = "1"
        // Not the pane this helper was started from, if there was one.
        environment["TMUX_PANE"] = nil
        process.environment = environment
        let output = Pipe()
        process.standardOutput = output
        process.standardError = FileHandle.nullDevice
        process.standardInput = FileHandle.nullDevice
        guard (try? process.run()) != nil else { return nil }
        let deadline = DispatchWorkItem { if process.isRunning { process.terminate() } }
        DispatchQueue.global().asyncAfter(deadline: .now() + 8, execute: deadline)
        let data = output.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        deadline.cancel()
        return process.terminationStatus == 0 ? data : nil
    }

    func sessions() -> [AgentSession]? {
        run(["status", "--json"]).map(agentSessions(fromStatusJSON:))
    }
}

#if canImport(FoundationModels)

/// Records which tools the model reached for, so the answer can say so.
final class ToolLog: @unchecked Sendable {
    private let lock = NSLock()
    private var names: [String] = []

    func record(_ name: String) {
        lock.lock()
        names.append(name)
        lock.unlock()
    }

    var used: [String] {
        lock.lock()
        defer { lock.unlock() }
        return names
    }
}

@available(macOS 26.0, *)
struct ListAgentSessionsTool: Tool {
    let workspace: Workspace
    let log: ToolLog
    let parameters: GenerationSchema

    let name = "list_agent_sessions"
    let description = """
        Lists the AI coding agent sessions muxa is tracking on this Mac: pane id, \
        agent, state, title, working directory, and the latest prompt and reply of each.
        """

    func call(arguments: GeneratedContent) async throws -> String {
        log.record(name)
        guard let sessions = workspace.sessions() else {
            return "muxa could not be reached, so the agent sessions are unknown right now."
        }
        return sessionsDigest(sessions)
    }
}

@available(macOS 26.0, *)
struct ReadAgentSessionTool: Tool {
    let workspace: Workspace
    let log: ToolLog
    let parameters: GenerationSchema

    let name = "read_agent_session"
    let description = """
        Reads one agent session in detail by pane id (for example %3): its recap, \
        recent prompts, and latest reply. Call list_agent_sessions first to get pane ids.
        """

    func call(arguments: GeneratedContent) async throws -> String {
        log.record(name)
        let wanted = (try? arguments.value(String.self, forProperty: "pane")) ?? ""
        guard let sessions = workspace.sessions() else {
            return "muxa could not be reached, so the agent sessions are unknown right now."
        }
        guard let agent = sessions.first(where: { matches($0, pane: wanted) }) else {
            let known = sessions.map(\.pane).joined(separator: ", ")
            return "There is no agent in pane \(wanted). Panes with agents: \(known.isEmpty ? "none" : known)."
        }
        let recent = workspace.run(["recap", "--pane", agent.pane, "--limit", "5"])
            .map { String(decoding: $0, as: UTF8.self) }
        return sessionDetail(agent, recentPrompts: recent)
    }
}

/// The read-only tools for one turn, or none when the schemas cannot be
/// built — the model then answers as it would have without them.
@available(macOS 26.0, *)
func workspaceTools(for workspace: Workspace, log: ToolLog) -> [any Tool] {
    let nothing = DynamicGenerationSchema(name: "NoArguments", properties: [])
    let onePane = DynamicGenerationSchema(name: "PaneArguments", properties: [
        DynamicGenerationSchema.Property(
            name: "pane",
            description: "The pane id, such as %3",
            schema: DynamicGenerationSchema(type: String.self)
        ),
    ])
    guard let listSchema = try? GenerationSchema(root: nothing, dependencies: []),
          let readSchema = try? GenerationSchema(root: onePane, dependencies: []) else { return [] }
    return [
        ListAgentSessionsTool(workspace: workspace, log: log, parameters: listSchema),
        ReadAgentSessionTool(workspace: workspace, log: log, parameters: readSchema),
    ]
}

@available(macOS 26.0, *)
func onDeviceUnavailable(_ model: SystemLanguageModel) -> HelperFailure? {
    switch model.availability {
    case .available:
        return nil
    case .unavailable(.deviceNotEligible):
        return HelperFailure(
            code: .unavailable,
            reason: "device_not_eligible",
            message: "this Mac does not support Apple Intelligence"
        )
    case .unavailable(.appleIntelligenceNotEnabled):
        return HelperFailure(
            code: .unavailable,
            reason: "apple_intelligence_not_enabled",
            message: "Apple Intelligence is turned off — enable it in System Settings › Apple Intelligence & Siri"
        )
    case .unavailable(.modelNotReady):
        return HelperFailure(
            code: .unavailable,
            reason: "model_not_ready",
            message: "the on-device model is still downloading or not ready — try again shortly"
        )
    case .unavailable:
        return HelperFailure(
            code: .unavailable,
            reason: "unavailable",
            message: "the on-device model is unavailable"
        )
    }
}

#if canImport(FoundationModels, _version: 2)
@available(macOS 27.0, *)
func privateCloudUnavailable(_ model: PrivateCloudComputeLanguageModel) -> HelperFailure? {
    switch model.availability {
    case .available:
        return nil
    case .unavailable(.deviceNotEligible):
        return HelperFailure(
            code: .unavailable,
            reason: "device_not_eligible",
            message: "this Mac does not support Private Cloud Compute"
        )
    case .unavailable(.systemNotReady):
        return HelperFailure(
            code: .unavailable,
            reason: "system_not_ready",
            message: "Private Cloud Compute is not ready — check that Apple Intelligence is enabled"
        )
    case .unavailable:
        return HelperFailure(
            code: .unavailable,
            reason: "unavailable",
            message: "Private Cloud Compute is unavailable"
        )
    }
}
#endif

@available(macOS 26.0, *)
func transcript(
    instructions: String?,
    tools: [any Tool],
    history: ArraySlice<TurnRequest.Exchange>
) -> Transcript {
    var entries: [Transcript.Entry] = []
    if let instructions, !instructions.isEmpty {
        entries.append(.instructions(Transcript.Instructions(
            segments: [.text(Transcript.TextSegment(content: instructions))],
            toolDefinitions: tools.map {
                Transcript.ToolDefinition(name: $0.name, description: $0.description, parameters: $0.parameters)
            }
        )))
    }
    for exchange in history {
        entries.append(.prompt(Transcript.Prompt(
            segments: [.text(Transcript.TextSegment(content: exchange.prompt))]
        )))
        entries.append(.response(Transcript.Response(
            assetIDs: [],
            segments: [.text(Transcript.TextSegment(content: exchange.answer))]
        )))
    }
    return Transcript(entries: entries)
}

@available(macOS 26.0, *)
func isContextOverflow(_ error: any Error) -> Bool {
    if case LanguageModelSession.GenerationError.exceededContextWindowSize = error {
        return true
    }
    #if canImport(FoundationModels, _version: 2)
    if #available(macOS 27.0, *), case LanguageModelError.contextSizeExceeded = error {
        return true
    }
    #endif
    return false
}

/// One turn. The replayed history is the only part of the context muxad
/// cannot size exactly — it budgets in characters, the model counts tokens —
/// so an overflow drops the older half of it and tries again, down to the
/// bare prompt, before giving up.
@available(macOS 26.0, *)
func respond(to request: TurnRequest, choice: ModelChoice, log: ToolLog) async throws -> String {
    let tools = request.muxa.map { workspaceTools(for: $0, log: log) } ?? []
    var history = ArraySlice(replayableHistory(request.history ?? [], hasTools: !tools.isEmpty))
    // The caller's instructions win; the workspace ones are only a default
    // for a turn that can actually use the tools they describe.
    let instructions = request.instructions ?? (tools.isEmpty ? nil : workspaceInstructions)
    while true {
        let replay = transcript(instructions: instructions, tools: tools, history: history)
        let session: LanguageModelSession
        switch choice {
        case .onDevice:
            let model = SystemLanguageModel.default
            if let failure = onDeviceUnavailable(model) { throw failure }
            session = LanguageModelSession(model: model, tools: tools, transcript: replay)
        case .privateCloud:
            #if canImport(FoundationModels, _version: 2)
            guard #available(macOS 27.0, *) else { throw privateCloudTooOld }
            let model = PrivateCloudComputeLanguageModel()
            if let failure = privateCloudUnavailable(model) { throw failure }
            session = LanguageModelSession(model: model, tools: tools, transcript: replay)
            #else
            throw builtWithoutPrivateCloud
            #endif
        }
        do {
            return try await session.respond(to: request.prompt).content
        } catch where isContextOverflow(error) {
            guard !history.isEmpty else {
                throw HelperFailure(
                    code: .failed,
                    reason: "context_size_exceeded",
                    message: "the prompt is larger than the \(choice.rawValue) model's context window"
                )
            }
            history = history.dropFirst((history.count + 1) / 2)
        }
    }
}

@available(macOS 26.0, *)
func probeModels() async -> [ProbeResponse.Model] {
    let onDevice = SystemLanguageModel.default
    let onDeviceFailure = onDeviceUnavailable(onDevice)
    var contextSize: Int?
    #if canImport(FoundationModels, _version: 2)
    if #available(macOS 27.0, *) { contextSize = onDevice.contextSize }
    #endif
    var models = [
        ProbeResponse.Model(
            id: ModelChoice.onDevice.rawValue,
            available: onDeviceFailure == nil,
            reason: onDeviceFailure?.reason,
            message: onDeviceFailure?.message,
            contextSize: contextSize
        ),
    ]
    #if canImport(FoundationModels, _version: 2)
    if #available(macOS 27.0, *) {
        let privateCloud = PrivateCloudComputeLanguageModel()
        let failure = privateCloudUnavailable(privateCloud)
        models.append(ProbeResponse.Model(
            id: ModelChoice.privateCloud.rawValue,
            available: failure == nil,
            reason: failure?.reason,
            message: failure?.message,
            contextSize: failure == nil ? try? await privateCloud.contextSize : nil
        ))
    }
    #else
    // Say so rather than leave the row out: on macOS 27 the model exists, and
    // only this build of the helper cannot reach it.
    if #available(macOS 27.0, *) {
        models.append(ProbeResponse.Model(
            id: ModelChoice.privateCloud.rawValue,
            available: false,
            reason: builtWithoutPrivateCloud.reason,
            message: builtWithoutPrivateCloud.message,
            contextSize: nil
        ))
    }
    #endif
    return models
}

#endif

func unavailableProbe(_ failure: HelperFailure) -> ProbeResponse {
    ProbeResponse(protocolVersion: protocolVersion, models: [
        ProbeResponse.Model(
            id: ModelChoice.onDevice.rawValue,
            available: false,
            reason: failure.reason,
            message: failure.message,
            contextSize: nil
        ),
    ])
}

func runProbe() async {
    #if canImport(FoundationModels)
    if #available(macOS 26.0, *) {
        writeJSON(ProbeResponse(protocolVersion: protocolVersion, models: await probeModels()))
    } else {
        writeJSON(unavailableProbe(osTooOld))
    }
    #else
    writeJSON(unavailableProbe(builtWithoutSDK))
    #endif
}

func runTurn() async {
    let input = FileHandle.standardInput.readDataToEndOfFile()
    let request: TurnRequest
    do {
        request = try JSONDecoder().decode(TurnRequest.self, from: input)
    } catch {
        fail(HelperFailure(
            code: .badRequest,
            reason: "bad_request",
            message: "muxa-afm expects one JSON request on stdin: \(error.localizedDescription)"
        ))
    }
    guard let choice = ModelChoice(configured: request.model) else {
        fail(HelperFailure(
            code: .badRequest,
            reason: "unknown_model",
            message: "unknown model \"\(request.model ?? "")\" — use one of: \(ModelChoice.accepted)"
        ))
    }
    #if canImport(FoundationModels)
    guard #available(macOS 26.0, *) else { fail(osTooOld) }
    do {
        let log = ToolLog()
        let answer = try await respond(to: request, choice: choice, log: log)
        writeJSON(TurnResponse(result: answer, model: choice.rawValue, toolsUsed: log.used))
    } catch let failure as HelperFailure {
        fail(failure)
    } catch {
        let detail = (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
        fail(HelperFailure(code: .failed, reason: "generation_failed", message: detail))
    }
    #else
    fail(builtWithoutSDK)
    #endif
}

@main
struct MuxaAFM {
    static func main() async {
        let arguments = CommandLine.arguments.dropFirst()
        switch arguments.first {
        case nil:
            await runTurn()
        case "--probe":
            await runProbe()
        case "--version":
            print("muxa-afm \(protocolVersion)")
        default:
            FileHandle.standardError.write(Data("usage: muxa-afm [--probe | --version]\n".utf8))
            exit(ExitCode.badRequest.rawValue)
        }
    }
}
