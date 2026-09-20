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
}

struct TurnResponse: Encodable {
    let result: String
    let model: String
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

#if canImport(FoundationModels)

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

@available(macOS 26.0, *)
func transcript(instructions: String?, history: ArraySlice<TurnRequest.Exchange>) -> Transcript {
    var entries: [Transcript.Entry] = []
    if let instructions, !instructions.isEmpty {
        entries.append(.instructions(Transcript.Instructions(
            segments: [.text(Transcript.TextSegment(content: instructions))],
            toolDefinitions: []
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
    if #available(macOS 27.0, *), case LanguageModelError.contextSizeExceeded = error {
        return true
    }
    return false
}

/// One turn. The replayed history is the only part of the context muxad
/// cannot size exactly — it budgets in characters, the model counts tokens —
/// so an overflow drops the older half of it and tries again, down to the
/// bare prompt, before giving up.
@available(macOS 26.0, *)
func respond(to request: TurnRequest, choice: ModelChoice) async throws -> String {
    var history = ArraySlice(request.history ?? [])
    while true {
        let replay = transcript(instructions: request.instructions, history: history)
        let session: LanguageModelSession
        switch choice {
        case .onDevice:
            let model = SystemLanguageModel.default
            if let failure = onDeviceUnavailable(model) { throw failure }
            session = LanguageModelSession(model: model, transcript: replay)
        case .privateCloud:
            guard #available(macOS 27.0, *) else { throw privateCloudTooOld }
            let model = PrivateCloudComputeLanguageModel()
            if let failure = privateCloudUnavailable(model) { throw failure }
            session = LanguageModelSession(model: model, transcript: replay)
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
    if #available(macOS 27.0, *) { contextSize = onDevice.contextSize }
    var models = [
        ProbeResponse.Model(
            id: ModelChoice.onDevice.rawValue,
            available: onDeviceFailure == nil,
            reason: onDeviceFailure?.reason,
            message: onDeviceFailure?.message,
            contextSize: contextSize
        ),
    ]
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
        let answer = try await respond(to: request, choice: choice)
        writeJSON(TurnResponse(result: answer, model: choice.rawValue))
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
