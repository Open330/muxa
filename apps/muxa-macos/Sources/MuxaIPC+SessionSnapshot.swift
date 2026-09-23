import Foundation

// Workspace snapshots: muxad's `mux_snapshot_*` requests. muxad runs
// `muxa snapshot` / `muxa restore --only-missing` with `--json` and passes the
// CLI's documents through untouched, so these types mirror the CLI's output
// (`crates/muxa-cli/src/reload/plan.rs`), not a daemon format of its own. The
// app never works out skip decisions or resume commands itself.

/// Counts shown beside a snapshot in the list and above a plan.
struct MuxSnapshotSummary: Decodable, Hashable, Sendable {
    let takenAt: Date?
    let origin: String
    let host: String
    let socket: String
    let sessions: Int
    let windows: Int
    let panes: Int
    let agents: Int
    let resumable: Int
    let notReplayable: Int

    var isAutomatic: Bool { origin == "auto" }

    enum CodingKeys: String, CodingKey {
        case origin, host, socket, sessions, windows, panes, agents, resumable
        case takenAt = "taken_at"
        case notReplayable = "not_replayable"
    }

    init(
        takenAt: Date? = nil, origin: String = "manual", host: String = "tmux", socket: String = "default",
        sessions: Int = 0, windows: Int = 0, panes: Int = 0, agents: Int = 0,
        resumable: Int = 0, notReplayable: Int = 0
    ) {
        self.takenAt = takenAt
        self.origin = origin
        self.host = host
        self.socket = socket
        self.sessions = sessions
        self.windows = windows
        self.panes = panes
        self.agents = agents
        self.resumable = resumable
        self.notReplayable = notReplayable
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        takenAt = MuxSnapshotDate.parse(try values.decodeIfPresent(String.self, forKey: .takenAt))
        origin = try values.decodeIfPresent(String.self, forKey: .origin) ?? "manual"
        host = try values.decodeIfPresent(String.self, forKey: .host) ?? ""
        socket = try values.decodeIfPresent(String.self, forKey: .socket) ?? ""
        sessions = try values.decodeIfPresent(Int.self, forKey: .sessions) ?? 0
        windows = try values.decodeIfPresent(Int.self, forKey: .windows) ?? 0
        panes = try values.decodeIfPresent(Int.self, forKey: .panes) ?? 0
        agents = try values.decodeIfPresent(Int.self, forKey: .agents) ?? 0
        resumable = try values.decodeIfPresent(Int.self, forKey: .resumable) ?? 0
        notReplayable = try values.decodeIfPresent(Int.self, forKey: .notReplayable) ?? 0
    }

    /// "2 sessions · 6 windows · 32 panes", the list's second line.
    var countsLabel: String {
        let sessionText = sessions == 1
            ? String(localized: "1 session")
            : String(localized: "\(sessions) sessions")
        let windowText = windows == 1
            ? String(localized: "1 window")
            : String(localized: "\(windows) windows")
        let paneText = panes == 1
            ? String(localized: "1 pane")
            : String(localized: "\(panes) panes")
        return "\(sessionText) · \(windowText) · \(paneText)"
    }

    /// "tmux default", which server it came from. Snapshots record the
    /// socket's absolute path; its last component is the server's name.
    var serverLabel: String { "\(host) \((socket as NSString).lastPathComponent)" }
}

/// The CLI writes RFC 3339 through the `time` crate, with or without
/// fractional seconds.
enum MuxSnapshotDate {
    static func parse(_ text: String?) -> Date? {
        guard let text else { return nil }
        let fractional = ISO8601DateFormatter()
        fractional.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        if let date = fractional.date(from: text) { return date }
        let plain = ISO8601DateFormatter()
        plain.formatOptions = [.withInternetDateTime]
        return plain.date(from: text)
    }
}

/// One row of `muxa snapshot --list --json`: a snapshot, or a directory
/// whose `snapshot.json` could not be read (still listed so it can be
/// deleted).
struct MuxSnapshotEntry: Decodable, Hashable, Sendable, Identifiable {
    let id: String
    let summary: MuxSnapshotSummary?
    let error: String?

    var readable: Bool { summary != nil }

    init(id: String, summary: MuxSnapshotSummary? = MuxSnapshotSummary(), error: String? = nil) {
        self.id = id
        self.summary = summary
        self.error = error
    }
}

struct MuxSnapshotListing: Decodable, Sendable {
    let snapshots: [MuxSnapshotEntry]
}

/// `muxa snapshot --json`.
struct MuxSnapshotSaved: Decodable, Sendable {
    let skipped: Bool
    let id: String?
    let reason: String?
    let summary: MuxSnapshotSummary?
}

/// A value from the CLI that this build may not know yet. Decoding never
/// fails on a new value; it lands in `.unknown` and the UI shows it plainly.
protocol MuxSnapshotOpenEnum: Decodable, Hashable, Sendable {
    init(raw: String)
}

extension MuxSnapshotOpenEnum {
    init(from decoder: Decoder) throws {
        self.init(raw: try decoder.singleValueContainer().decode(String.self))
    }
}

/// `muxa restore --json`: the plan, and after `--run` what happened.
struct MuxSnapshotPlan: Decodable, Hashable, Sendable {
    enum SessionAction: MuxSnapshotOpenEnum {
        case create, skip, fillMissing, unknown(String)
        init(raw: String) {
            switch raw {
            case "create": self = .create
            case "skip": self = .skip
            case "fill_missing": self = .fillMissing
            default: self = .unknown(raw)
            }
        }
    }

    enum SessionResult: MuxSnapshotOpenEnum {
        case created, skipped, filled, failed, unknown(String)
        init(raw: String) {
            switch raw {
            case "created": self = .created
            case "skipped": self = .skipped
            case "filled": self = .filled
            case "failed": self = .failed
            default: self = .unknown(raw)
            }
        }
    }

    enum PaneAction: MuxSnapshotOpenEnum {
        case resume, replay, shell, manual, resumeByHand, unknown(String)
        init(raw: String) {
            switch raw {
            case "resume": self = .resume
            case "replay": self = .replay
            case "shell": self = .shell
            case "manual": self = .manual
            case "resume_by_hand": self = .resumeByHand
            default: self = .unknown(raw)
            }
        }
    }

    enum PaneResult: MuxSnapshotOpenEnum {
        case relaunched, shell, manual, failed, unknown(String)
        init(raw: String) {
            switch raw {
            case "relaunched": self = .relaunched
            case "shell": self = .shell
            case "manual": self = .manual
            case "failed": self = .failed
            default: self = .unknown(raw)
            }
        }
    }

    struct Session: Decodable, Hashable, Sendable {
        let name: String
        let action: SessionAction
        let result: SessionResult?
        let error: String?
        let windows: [Window]
    }

    struct Window: Decodable, Hashable, Sendable {
        let index: String
        let name: String
        let panes: [Pane]
    }

    struct Pane: Decodable, Hashable, Sendable {
        let index: String
        let path: String
        let action: PaneAction
        let command: String?
        let agentKind: String?
        /// What a person has to do, for a pane muxa cannot relaunch itself.
        let note: String?
        let result: PaneResult?
        let error: String?

        enum CodingKeys: String, CodingKey {
            case index, path, action, command, note, result, error
            case agentKind = "agent_kind"
        }
    }

    struct Totals: Decodable, Hashable, Sendable {
        let sessionsCreated: Int
        let sessionsSkipped: Int
        let sessionsFailed: Int
        let panes: Int
        let relaunched: Int
        let shell: Int
        let manual: Int
        let failed: Int

        enum CodingKeys: String, CodingKey {
            case panes, relaunched, shell, manual, failed
            case sessionsCreated = "sessions_created"
            case sessionsSkipped = "sessions_skipped"
            case sessionsFailed = "sessions_failed"
        }
    }

    let id: String
    let summary: MuxSnapshotSummary?
    let serverReachable: Bool
    let run: Bool
    let sessions: [Session]
    let totals: Totals?

    enum CodingKeys: String, CodingKey {
        case id, summary, run, sessions, totals
        case serverReachable = "server_reachable"
    }

    var sessionsToCreate: Int { sessions.filter { $0.action == .create }.count }
    var sessionsToSkip: Int { sessions.filter { $0.action == .skip }.count }
}

/// A restore muxad runs in the background and the sheet polls.
struct MuxSnapshotOperation: Decodable, Hashable, Sendable {
    enum State: MuxSnapshotOpenEnum {
        case running, succeeded, failed, unknown(String)
        init(raw: String) {
            switch raw {
            case "running": self = .running
            case "succeeded": self = .succeeded
            case "failed": self = .failed
            default: self = .unknown(raw)
            }
        }
    }

    let operationID: String
    let state: State
    let id: String
    let message: String
    /// The `restore --run --json` report; also present on a failure the CLI
    /// got far enough to describe.
    let result: MuxSnapshotPlan?

    enum CodingKeys: String, CodingKey {
        case state, id, message, result
        case operationID = "operation_id"
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        operationID = try values.decode(String.self, forKey: .operationID)
        state = try values.decode(State.self, forKey: .state)
        id = try values.decodeIfPresent(String.self, forKey: .id) ?? ""
        message = try values.decodeIfPresent(String.self, forKey: .message) ?? ""
        // A report this build cannot read must not hide the state and message.
        result = try? values.decodeIfPresent(MuxSnapshotPlan.self, forKey: .result)
    }
}

private struct MuxSnapshotEnvelope<Payload: Decodable>: Decodable {
    let ok: Bool?
    let error: String?
    let muxSnapshot: Payload?
    let muxSnapshotOperation: MuxSnapshotOperation?

    enum CodingKeys: String, CodingKey {
        case ok, error
        case muxSnapshot = "mux_snapshot"
        case muxSnapshotOperation = "mux_snapshot_operation"
    }
}

/// An envelope whose `mux_snapshot` payload is ignored.
private struct MuxSnapshotIgnored: Decodable {}

/// Sends `mux_snapshot_*` over its own connection and queue: muxad answers a
/// list, plan or save only once the CLI child has finished, and the control
/// transport (keys, resizes, polling) must not wait behind it.
final class MuxaSessionSnapshotClient: Sendable {
    /// muxad bounds a save at 60 s and the others at 30 s; a restore is
    /// started and then polled, so no exchange waits on it.
    static let requestTimeout: TimeInterval = 75

    let socketPath: String
    private let transport: SerializedIPCTransport

    init(socketPath: String) {
        self.socketPath = socketPath
        transport = SerializedIPCTransport(label: "dev.muxa.mac.ipc-snapshot") { path, payload in
            try UnixSocket.request(path: path, payload: payload, timeout: Self.requestTimeout)
        }
    }

    /// Test seam: exchanges go through `request` instead of the socket.
    init(socketPath: String, request: @escaping MuxaIPCRequestHandler) {
        self.socketPath = socketPath
        transport = SerializedIPCTransport(label: "dev.muxa.mac.ipc-snapshot-test", handler: request)
    }

    static func requestObject(_ kind: String, _ fields: [String: Any] = [:]) -> [String: Any] {
        var object: [String: Any] = [
            "protocol": MuxaIPCClient.protocolVersion,
            "kind": kind,
        ]
        object.merge(fields) { _, new in new }
        return object
    }

    func list() async throws -> [MuxSnapshotEntry] {
        let listing: MuxSnapshotListing = try await payload(Self.requestObject("mux_snapshot_list"))
        return listing.snapshots
    }

    func save() async throws -> MuxSnapshotSaved {
        try await payload(Self.requestObject("mux_snapshot_save"))
    }

    func plan(id: String) async throws -> MuxSnapshotPlan {
        try await payload(Self.requestObject("mux_snapshot_plan", ["id": id]))
    }

    func delete(id: String) async throws {
        let _: MuxSnapshotEnvelope<MuxSnapshotIgnored> = try await call(
            Self.requestObject("mux_snapshot_delete", ["id": id])
        )
    }

    func restore(id: String, layoutOnly: Bool) async throws -> MuxSnapshotOperation {
        try await operation(Self.requestObject("mux_snapshot_restore", ["id": id, "layout_only": layoutOnly]))
    }

    func status(operationID: String) async throws -> MuxSnapshotOperation {
        try await operation(Self.requestObject("mux_snapshot_restore_status", ["operation_id": operationID]))
    }

    private func payload<Payload: Decodable>(_ object: [String: Any]) async throws -> Payload {
        let response: MuxSnapshotEnvelope<Payload> = try await call(object)
        guard let payload = response.muxSnapshot else {
            throw MuxaIPCError.missingField("mux_snapshot")
        }
        return payload
    }

    private func operation(_ object: [String: Any]) async throws -> MuxSnapshotOperation {
        let response: MuxSnapshotEnvelope<MuxSnapshotIgnored> = try await call(object)
        guard let operation = response.muxSnapshotOperation else {
            throw MuxaIPCError.missingField("mux_snapshot_operation")
        }
        return operation
    }

    private func call<Payload: Decodable>(_ object: [String: Any]) async throws -> MuxSnapshotEnvelope<Payload> {
        let payload = try JSONSerialization.data(withJSONObject: object)
        let data = try await transport.request(path: socketPath, payload: payload, timeout: Self.requestTimeout)
        let response = try JSONDecoder().decode(MuxSnapshotEnvelope<Payload>.self, from: data)
        if response.ok == false {
            throw MuxaIPCError.server(response.error ?? "muxad rejected the request")
        }
        return response
    }
}

extension MuxaIPCClient {
    static let muxSnapshotCapability = "mux_snapshot_v1"

    /// A snapshot client bound to this daemon's socket.
    nonisolated func makeSessionSnapshotClient() -> MuxaSessionSnapshotClient {
        MuxaSessionSnapshotClient(socketPath: socketPath)
    }
}
