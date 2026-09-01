import Foundation
import Darwin
import Testing
@testable import Muxa

@Test func readableMarkdownKeepsConversationStructure() {
    let document = ReadableMarkdownDocument(source: """
    # Result

    A readable paragraph with **emphasis**.

    - first
    - second

    > important note

    ```swift
    let value = 42
    ```
    """)

    #expect(document.blocks == [
        .heading(level: 1, text: "Result"),
        .paragraph("A readable paragraph with **emphasis**."),
        .list(ordered: false, items: [
            .init(marker: "•", text: "first", depth: 0),
            .init(marker: "•", text: "second", depth: 0),
        ]),
        .quote("important note"),
        .code(language: "swift", source: "let value = 42"),
    ])
}

@Test func readableMarkdownGroupsOrderedListsAndNormalizesNewlines() {
    let document = ReadableMarkdownDocument(source: "1. one\r\n2. two\r\n\r\nnext line")

    #expect(document.blocks == [
        .list(ordered: true, items: [
            .init(marker: "1.", text: "one", depth: 0),
            .init(marker: "2.", text: "two", depth: 0),
        ]),
        .paragraph("next line"),
    ])
}

private final class IPCProbe: @unchecked Sendable {
    private let lock = NSLock()
    private var activeRequests = 0
    private var maximumActiveRequests = 0
    private var attachmentBalance = 0
    private var maximumAttachmentBalance = 0
    private var offsets: [UInt64] = []
    private var resizes: [(columns: Int, rows: Int)] = []
    private var firstAttachDelayMicroseconds: useconds_t = 0
    private var delayedFirstAttach = false
    private var truncateFirstRead = false
    private var spawnedEnvironment: [String: String] = [:]
    private var spawnedColumns = 0
    private var spawnedRows = 0
    private var fleetCaptureAddress: (host: String, backend: String, socket: String, pane: String)?
    private var fleetPrompt: (host: String, pane: String, text: String, submit: Bool)?
    private var workStart: (work: String, workspace: String, cwd: String, noTicket: Bool)?
    private var requestKinds: [String] = []

    init(
        firstAttachDelayMicroseconds: useconds_t = 0,
        truncateFirstRead: Bool = false
    ) {
        self.firstAttachDelayMicroseconds = firstAttachDelayMicroseconds
        self.truncateFirstRead = truncateFirstRead
    }

    func request(path _: String, payload: Data) throws -> Data {
        lock.lock()
        activeRequests += 1
        maximumActiveRequests = max(maximumActiveRequests, activeRequests)
        lock.unlock()
        defer {
            lock.lock()
            activeRequests -= 1
            lock.unlock()
        }

        let object = try JSONSerialization.jsonObject(with: payload) as? [String: Any]
        let kind = object?["kind"] as? String
        lock.lock()
        if let kind { requestKinds.append(kind) }
        lock.unlock()
        switch kind {
        case "hello":
            usleep(20_000)
            return Self.json([
                "ok": true,
                "min_protocol": 1,
                "max_protocol": 6,
                "capabilities": [
                    "session_bytes_v1",
                    "session_attachment_identity_v1",
                    "session_wait_v1",
                    "pipeline_runs_v1",
                    "pipeline_subscribe",
                    "fleet_v1",
                    "fleet_raw_capture_v1",
                    "fleet_subscribe",
                    "work_control_v1",
                    "ask_subscribe",
                ],
            ])
        case "spawn_session":
            let pairs = object?["env"] as? [[String]] ?? []
            lock.lock()
            spawnedEnvironment = Dictionary(uniqueKeysWithValues: pairs.compactMap { pair in
                guard pair.count == 2 else { return nil }
                return (pair[0], pair[1])
            })
            spawnedColumns = object?["cols"] as? Int ?? 0
            spawnedRows = object?["rows"] as? Int ?? 0
            lock.unlock()
            return Self.json([
                "ok": true,
                "session": [
                    "id": "pty:spawned",
                    "backend": "pty",
                    "display_name": "Muxa Shell",
                    "cwd": "/tmp",
                    "attached_clients": 0,
                    "has_been_attached": false,
                    "exited": false,
                    "exit_status": NSNull(),
                    "pid": 1234,
                ],
            ])
        case "pipeline_runs":
            return Self.json([
                "ok": true,
                "pipeline_runs": [[
                    "identity": ["workspace_id": "muxa", "work_id": "native-app"],
                    "pipeline": "implement-review",
                    "desired": [[
                        "alias": "impl",
                        "program": "codex",
                        "role": "implementer",
                    ]],
                    "cwd": "/tmp/muxa",
                    "generation": 3,
                    "window_id": "@4",
                    "aliases": [
                        "impl": [
                            "alias": "impl",
                            "status": "running",
                            "generation": 3,
                            "pane": "%17",
                        ],
                    ],
                ]],
            ])
        case "work_up":
            let request = object?["request"] as? [String: Any]
            lock.lock()
            workStart = (
                request?["work"] as? String ?? "",
                request?["workspace"] as? String ?? "",
                request?["cwd"] as? String ?? "",
                request?["no_ticket"] as? Bool ?? false
            )
            lock.unlock()
            return Self.json([
                "ok": true,
                "work_operation": [
                    "operation_id": "native-work-1",
                    "state": "running",
                    "work": request?["work"] as? String ?? "",
                    "workspace": request?["workspace"] as? String ?? "",
                    "message": "Starting configured Work pipeline…",
                ],
            ])
        case "work_up_status":
            return Self.json([
                "ok": true,
                "work_operation": [
                    "operation_id": object?["operation_id"] as? String ?? "",
                    "state": "succeeded",
                    "work": "native-app",
                    "workspace": "muxa",
                    "message": "Work pipeline started",
                    "result": [
                        "work": "native-app",
                        "workspace": "muxa",
                        "pipeline": "implement-review",
                        "cwd": "/tmp/muxa",
                        "dry_run": false,
                    ],
                ],
            ])
        case "fleet_snapshot":
            return Self.json([
                "ok": true,
                "fleet": [
                    "hosts": [[
                        "alias": "local",
                        "local": true,
                        "mode": "control",
                        "state": "online",
                        "remote": [
                            "agents": [
                                [
                                    "kind": "codex",
                                    "agent_session_id": "agent-17",
                                    "pane": "%17",
                                    "tmux_socket": "default",
                                    "tmux_session": "muxa",
                                    "state": "working",
                                    "last_prompt": "Implement native window reporting",
                                    "last_prompt_at": "2026-08-31T09:59:00Z",
                                    "last_response": "Implemented the native window report.",
                                    "model": "gpt-5-codex",
                                    "context_used_pct": 42.5,
                                    "cost_usd": 0.12,
                                    "started_at": "2026-08-31T09:00:00Z",
                                    "last_activity_at": "2026-08-31T10:00:00Z",
                                    "state_entered_at": "2026-08-31T09:59:00Z",
                                    "workload": [
                                        "process_count": 3,
                                        "shell_count": 1,
                                        "subagent_count": 1,
                                        "helper_count": 1,
                                        "preview": [],
                                    ],
                                    "subagents": [[
                                        "kind": "reviewer",
                                        "description": "Review the window report",
                                        "started_at": "2026-08-31T09:58:00Z",
                                    ]],
                                ],
                                [
                                    "kind": "claude_code",
                                    "agent_session_id": "agent-local-deploy",
                                    "pane": "%18",
                                    "tmux_socket": "default",
                                    "tmux_session": "platform",
                                    "state": "idle",
                                    "last_prompt": "## Deploy\nShip the release",
                                ],
                            ],
                            "panes": [
                                [
                                    "pane_id": "%17",
                                    "session_id": "$4",
                                    "session": "muxa",
                                    "window_id": "@4",
                                    "window_name": "native-app",
                                    "window_index": "1",
                                    "pane_index": "0",
                                    "current_command": "codex",
                                    "title": "muxa",
                                    "current_path": "/tmp/muxa",
                                    "socket": "default",
                                    "workspace_id": "muxa",
                                    "work_id": "native-app",
                                ],
                                [
                                    "pane_id": "%18",
                                    "session_id": "$8",
                                    "session": "platform",
                                    "window_id": "@8",
                                    "window_name": "deploy",
                                    "window_index": "2",
                                    "pane_index": "0",
                                    "current_command": "claude",
                                    "title": "deploy",
                                    "current_path": "/tmp/platform",
                                    "socket": "default",
                                ],
                            ],
                        ],
                    ], [
                        "alias": "dev",
                        "local": false,
                        "mode": "observe",
                        "state": "online",
                        "latency_ms": 14,
                        "remote": [
                            "agents": [[
                                "kind": "codex",
                                "agent_session_id": "agent-remote-deploy",
                                "pane": "%2",
                                "tmux_socket": "default",
                                "tmux_session": "platform",
                                "state": "working",
                            ]],
                            "panes": [[
                                "pane_id": "%2",
                                "session_id": "$2",
                                "session": "platform",
                                "window_id": "@2",
                                "window_name": "deploy",
                                "window_index": "0",
                                "pane_index": "0",
                                "current_command": "codex",
                                "title": "deploy",
                                "current_path": "/srv/platform",
                                "socket": "default",
                                "workspace_id": "platform",
                                "work_id": "DEPLOY-1",
                            ]],
                        ],
                    ]],
                ],
            ])
        case "fleet_command":
            let operation = object?["operation"] as? [String: Any]
            let pane = operation?["pane"] as? [String: Any]
            let window = pane?["window"] as? [String: Any]
            let session = window?["session"] as? [String: Any]
            lock.lock()
            fleetCaptureAddress = (
                object?["host"] as? String ?? "",
                session?["host"] as? String ?? "",
                session?["socket"] as? String ?? "",
                pane?["pane_id"] as? String ?? ""
            )
            if operation?["kind"] as? String == "send_prompt" {
                fleetPrompt = (
                    object?["host"] as? String ?? "",
                    pane?["pane_id"] as? String ?? "",
                    operation?["text"] as? String ?? "",
                    operation?["submit"] as? Bool ?? false
                )
            }
            lock.unlock()
            if operation?["kind"] as? String == "send_prompt" {
                return Self.json([
                    "ok": true,
                    "fleet_result": [
                        "accepted": true,
                        "message": "sent",
                    ],
                ])
            }
            return Self.json([
                "ok": true,
                "fleet_result": [
                    "accepted": true,
                    "capture": "\u{001B}[31m$ echo 안녕\u{001B}[0m\n안녕",
                    "capture_raw_base64": Data("\u{001B}[31m$ echo 안녕\u{001B}[0m\r\n안녕".utf8).base64EncodedString(),
                ],
            ])
        case "set_session_attached":
            let attached = object?["attached"] as? Bool ?? false
            var delay: useconds_t = 0
            lock.lock()
            if attached, !delayedFirstAttach {
                delayedFirstAttach = true
                delay = firstAttachDelayMicroseconds
            }
            attachmentBalance += attached ? 1 : -1
            maximumAttachmentBalance = max(maximumAttachmentBalance, attachmentBalance)
            lock.unlock()
            if delay > 0 { usleep(delay) }
            return Self.json(["ok": true])
        case "read_session", "read_session_wait":
            let offset = (object?["offset"] as? NSNumber)?.uint64Value ?? 0
            lock.lock()
            offsets.append(offset)
            let readIndex = offsets.count
            lock.unlock()
            if truncateFirstRead, readIndex == 1 {
                return Self.output(offset: 50, nextOffset: 50, exited: false, truncated: true)
            }
            return Self.output(offset: offset, nextOffset: offset, exited: true, truncated: false)
        case "resize_session":
            lock.lock()
            resizes.append((object?["cols"] as? Int ?? 0, object?["rows"] as? Int ?? 0))
            lock.unlock()
            return Self.json(["ok": true])
        default:
            return Self.json(["ok": true])
        }
    }

    func snapshot() -> (maximumActive: Int, attachmentBalance: Int, maximumAttachment: Int, offsets: [UInt64]) {
        lock.lock()
        defer { lock.unlock() }
        return (
            maximumActiveRequests,
            attachmentBalance,
            maximumAttachmentBalance,
            offsets
        )
    }

    func lastSpawn() -> (environment: [String: String], columns: Int, rows: Int) {
        lock.lock()
        defer { lock.unlock() }
        return (spawnedEnvironment, spawnedColumns, spawnedRows)
    }

    func lastFleetCaptureAddress() -> (host: String, backend: String, socket: String, pane: String)? {
        lock.lock()
        defer { lock.unlock() }
        return fleetCaptureAddress
    }

    func lastFleetPrompt() -> (host: String, pane: String, text: String, submit: Bool)? {
        lock.lock()
        defer { lock.unlock() }
        return fleetPrompt
    }

    func lastWorkStart() -> (work: String, workspace: String, cwd: String, noTicket: Bool)? {
        lock.lock()
        defer { lock.unlock() }
        return workStart
    }

    func lastResize() -> (columns: Int, rows: Int)? {
        lock.lock()
        defer { lock.unlock() }
        return resizes.last
    }

    func didRequest(_ kind: String) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return requestKinds.contains(kind)
    }

    private static func output(
        offset: UInt64,
        nextOffset: UInt64,
        exited: Bool,
        truncated: Bool
    ) -> Data {
        json([
            "ok": true,
            "output": [
                "session_id": "pty:test",
                "offset": offset,
                "next_offset": nextOffset,
                "data": "",
                "data_base64": "",
                "truncated": truncated,
                "exited": exited,
                "exit_status": exited ? 0 : NSNull(),
            ],
        ])
    }

    private static func json(_ object: [String: Any]) -> Data {
        (try? JSONSerialization.data(withJSONObject: object))
            ?? Data(#"{"ok":false,"error":"test encoding failed"}"#.utf8)
    }
}

struct MuxaIPCTests {
    @Test
    func detectsXCTestHostEnvironment() {
        #expect(
            AppModel.isRunningTests(
                environment: ["XCTestConfigurationFilePath": "/tmp/Muxa.xctestconfiguration"]
            )
        )
        #expect(!AppModel.isRunningTests(environment: [:]))
    }

    @Test
    func environmentSocketOverridesDefault() {
        #expect(MuxaIPCClient.defaultSocketPath(environment: ["MUXA_SOCKET": "/tmp/custom.sock"]) == "/tmp/custom.sock")
    }

    @Test
    func emptyEnvironmentSocketUsesUIDFallback() {
        #expect(MuxaIPCClient.defaultSocketPath(environment: [:]).hasPrefix("/tmp/muxa-"))
    }

    @Test
    func outputDecodesByteSafePayload() throws {
        let source = Data([0x1B, 0x5B, 0x31, 0x6D, 0xFF, 0x00])
        let json = """
        {
          "session_id": "pty:test",
          "offset": 0,
          "next_offset": 6,
          "data": "lossy",
          "data_base64": "\(source.base64EncodedString())",
          "truncated": false,
          "exited": false,
          "exit_status": null
        }
        """
        let output = try JSONDecoder().decode(MuxaSessionOutput.self, from: Data(json.utf8))
        #expect(output.bytes == source)
    }

    @Test
    func terminalReadUsesEventDrivenWaitWhenDaemonAdvertisesIt() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()

        _ = try await client.readSession(id: "pty:test", offset: 0, waitForChanges: true)

        #expect(probe.didRequest("read_session_wait"))
        #expect(!probe.didRequest("read_session"))
    }

    @Test
    func rawTerminalFormatterMakesControlsVisibleWithoutExecutingThem() {
        let bytes = Data("A\u{001B}[31mB\r\n\t한글".utf8)
        let dump = terminalRawDescription(bytes)
        #expect(dump.contains("00000000"))
        #expect(dump.contains("41 1B 5B 33 31 6D 42 0D"))
        #expect(dump.contains("A.[31mB."))
        #expect(!dump.contains("\u{001B}"))
    }

    @Test
    func shellSpawnCarriesTerminalContractAndConservativeInitialGrid() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        _ = try await client.spawnShell(
            command: "/bin/zsh",
            cwd: "/tmp",
            name: "Muxa Shell",
            environment: [
                "TERM": "xterm-256color",
                "COLORTERM": "truecolor",
                "LC_CTYPE": "en_US.UTF-8",
            ]
        )

        let spawn = probe.lastSpawn()
        #expect(spawn.environment["TERM"] == "xterm-256color")
        #expect(spawn.environment["COLORTERM"] == "truecolor")
        #expect(spawn.environment["LC_CTYPE"] == "en_US.UTF-8")
        #expect(spawn.columns == 80)
        #expect(spawn.rows == 24)
    }

    @Test
    func keepsManagedWorkSeparateFromIndependentFleetAgents() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()

        let runs = try await client.listPipelineRuns()
        let execution = try await client.executionSnapshot()

        #expect(runs.count == 1)
        #expect(runs[0].identity.id == "muxa/native-app")
        #expect(runs[0].aliases["impl"]?.pane == "%17")
        #expect(execution.agents.count == 3)
        #expect(execution.agents[0].lastActivityAt == "2026-08-31T10:00:00Z")
        #expect(execution.agents[0].lastPromptAt == "2026-08-31T09:59:00Z")
        #expect(execution.agents[0].workload?.processCount == 3)
        #expect(execution.agents[0].subagents?.first?.kind == "reviewer")
        #expect(execution.pane(for: execution.agents[0])?.windowID == "@4")

        let workGroups = execution.workGroups(pipelineRuns: runs)
        #expect(workGroups.count == 2)
        #expect(workGroups[0].participants.count == 1)
        #expect(workGroups[0].pipelineRun != nil)
        #expect(workGroups[1].identity.id == "platform/DEPLOY-1")
        #expect(workGroups[1].pipelineRun == nil)
        #expect(workGroups[1].hostAliases == ["dev"])
        #expect(execution.hostedAgents.count == 3)
        #expect(Set(execution.hostedAgents.map(\.id)).count == 3)
        #expect(execution.hostedAgents.filter { $0.pane?.windowName == "deploy" }.count == 2)

        let watchHosts = execution.watchHosts
        #expect(watchHosts.map(\.host.alias) == ["local", "dev"])
        #expect(watchHosts[0].sessions.count == 2)
        #expect(watchHosts[0].paneCount == 2)
        #expect(watchHosts[1].sessions.first?.windows.first?.panes.first?.host.alias == "dev")
        let remoteWindow = try #require(watchHosts[1].sessions.first?.windows.first)
        let remoteWatchPane = try #require(remoteWindow.panes.first)
        #expect(execution.watchWindow(containing: remoteWatchPane.id)?.id == remoteWindow.id)

        let remotePane = remoteWatchPane.pane
        let exactAddress = try AppModel.exactPaneAddressJSON(remotePane)
        let address = try #require(
            JSONSerialization.jsonObject(with: Data(exactAddress.utf8)) as? [String: Any]
        )
        let window = try #require(address["window"] as? [String: Any])
        let session = try #require(window["session"] as? [String: Any])
        #expect(session["host"] as? String == "tmux")
        #expect(session["socket"] as? String == "default")
        #expect(address["pane_id"] as? String == "%2")
    }

    @Test
    func nativeWorkControlStartsAndPollsCanonicalOperation() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        let started = try await client.startWork(
            MuxaWorkStartRequest(
                work: "native-app",
                workspace: "muxa",
                pipeline: "implement-review",
                cwd: "/tmp/muxa",
                external: nil,
                skill: nil,
                body: "Build the native console",
                context: nil,
                dryRun: false
            )
        )
        #expect(started.state == .running)
        let completed = try await client.workOperation(id: started.operationID)
        #expect(completed.state == .succeeded)
        #expect(completed.result?.workspace == "muxa")
        #expect(completed.result?.work == "native-app")
        let request = try #require(probe.lastWorkStart())
        #expect(request.work == "native-app")
        #expect(request.workspace == "muxa")
        #expect(request.cwd == "/tmp/muxa")
        #expect(request.noTicket)
    }

    @Test
    func askIPCExposesDurableHistoryAndAgentSelection() async throws {
        let handler: MuxaIPCRequestHandler = { _, payload in
            let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
            switch object["kind"] as? String {
            case "hello":
                return try JSONSerialization.data(withJSONObject: [
                    "ok": true,
                    "min_protocol": 1,
                    "max_protocol": 6,
                    "capabilities": [
                        "session_bytes_v1",
                        "session_attachment_identity_v1",
                        "work_control_v1",
                        "ask_one_turn_credential_v1",
                        "ask_conversations_v1",
                    ],
                ])
            case "ask_agent":
                return try JSONSerialization.data(withJSONObject: [
                    "ok": true,
                    "ask_agent": object["agent"] as? String ?? "claude",
                ])
            case "ask_status":
                return try JSONSerialization.data(withJSONObject: [
                    "ok": true,
                    "ask_enabled": true,
                ])
            case "ask_send":
                let credential = try #require(object["credential"] as? [String: Any])
                #expect(credential["agent"] as? String == "codex")
                #expect(credential["api_key"] as? String == "test-only-secret")
                return try JSONSerialization.data(withJSONObject: [
                    "ok": true,
                    "ask_entry": [
                        "id": "ask-1",
                        "prompt": object["prompt"] as? String ?? "",
                        "answer": "",
                        "status": "running",
                        "agent": "codex",
                        "cwd": "/tmp/muxa",
                        "asked_at": "2026-08-31T10:00:00Z",
                    ],
                ])
            case "ask_list":
                return try JSONSerialization.data(withJSONObject: [
                    "ok": true,
                    "ask_entries": [[
                        "id": "ask-1",
                        "prompt": "Review the plan",
                        "answer": "Looks good.",
                        "status": "answered",
                        "agent": "codex",
                        "cwd": "/tmp/muxa",
                        "asked_at": "2026-08-31T10:00:00Z",
                        "answered_at": "2026-08-31T10:00:03Z",
                    ]],
                ])
            case "ask_conversation_list":
                let conversation: [String: Any] = [
                    "id": "conversation-1",
                    "title": "Review the plan",
                    "agent": "codex",
                    "agent_session_id": "codex-session-1",
                    "created_at": "2026-08-31T10:00:00Z",
                    "updated_at": "2026-08-31T10:00:03Z",
                ]
                return try JSONSerialization.data(withJSONObject: [
                    "ok": true,
                    "ask_conversations": [conversation],
                    "ask_conversation": conversation,
                ])
            case "ask_conversation_select":
                #expect(object["conversation_id"] as? String == "conversation-1")
                return try JSONSerialization.data(withJSONObject: [
                    "ok": true,
                    "ask_conversation": [
                        "id": "conversation-1",
                        "title": "Review the plan",
                        "agent": "codex",
                        "agent_session_id": "codex-session-1",
                        "created_at": "2026-08-31T10:00:00Z",
                        "updated_at": "2026-08-31T10:00:03Z",
                    ],
                ])
            default:
                return try JSONSerialization.data(withJSONObject: ["ok": true])
            }
        }
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-ask-test.sock", request: handler)
        try await client.hello()

        #expect(try await client.askStatus())
        #expect(try await client.selectAskAgent("codex") == "codex")
        let pending = try await client.sendAsk(
            "Review the plan",
            agent: "codex",
            apiKey: "test-only-secret"
        )
        #expect(pending.prompt == "Review the plan")
        #expect(pending.status == "running")
        let history = try await client.listAskEntries()
        #expect(history.first?.answer == "Looks good.")
        let conversations = try await client.listAskConversations()
        #expect(conversations.active?.id == "conversation-1")
        #expect(conversations.conversations.first?.agentSessionID == "codex-session-1")
        let resumed = try await client.selectAskConversation("conversation-1")
        #expect(resumed.title == "Review the plan")
    }

    @Test
    func collaborationIPCUsesSelectedFleetPaneAndDecodesMailbox() async throws {
        let handler: MuxaIPCRequestHandler = { _, payload in
            let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
            if object["kind"] as? String == "hello" {
                return try JSONSerialization.data(withJSONObject: [
                    "ok": true,
                    "min_protocol": 1,
                    "max_protocol": 6,
                    "capabilities": [
                        "session_bytes_v1",
                        "session_attachment_identity_v1",
                        "work_control_v1",
                        "fleet_v1",
                    ],
                ])
            }
            let operation = try #require(object["operation"] as? [String: Any])
            let pane = try #require(operation["pane"] as? [String: Any])
            let paneID = pane["pane_id"] as? String ?? ""
            let participant: [String: Any] = [
                "agent_kind": "codex",
                "agent_session_id": "agent-17",
                "pane": paneID,
                "room": ["host": "tmux", "window_id": "@4"],
                "state": "working",
                "alias": "impl",
            ]
            let console: [String: Any] = [
                "agent_kind": "unknown",
                "agent_session_id": "__muxa_console__",
                "pane": "console",
                "room": ["host": "tmux", "window_id": "@4"],
                "state": "idle",
                "console": true,
            ]
            let requestBody = operation["request"] as? [String: Any]
            let request: [String: Any] = [
                "id": "req-1",
                "from": console,
                "to": participant,
                "kind": requestBody?["kind"] as? String ?? "question",
                "body": requestBody?["body"] as? String ?? "Review this",
                "expects_reply": true,
                "work_mode": requestBody?["work_mode"] as? String ?? "read_only",
                "status": "queued",
                "created_at": "2026-08-31T10:00:00Z",
            ]
            let result: [String: Any]
            if operation["kind"] as? String == "collaboration_mailbox" {
                result = ["accepted": true, "collaboration_incoming": [request], "collaboration_sent": [request]]
            } else {
                result = ["accepted": true, "collaboration_request": request]
            }
            return try JSONSerialization.data(withJSONObject: ["ok": true, "fleet_result": result])
        }
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-collab-test.sock", request: handler)
        try await client.hello()
        let host = MuxaFleetHostIdentity(alias: "local", local: true, state: "online", mode: "control")
        let pane = MuxaPaneInfo(
            paneID: "%17",
            sessionID: "$4",
            session: "muxa",
            windowID: "@4",
            windowName: "native-app",
            windowIndex: "1",
            paneIndex: "0",
            currentCommand: "codex",
            title: "muxa",
            currentPath: "/tmp/muxa",
            socket: "default",
            workspaceID: nil,
            workID: nil,
            agentRole: "implementer",
            agentAlias: "impl"
        )

        let sent = try await client.sendCollaboration(
            host: host,
            pane: pane,
            kind: "review",
            body: "Review this",
            workMode: "read_only"
        )
        #expect(sent.to.pane == "%17")
        #expect(sent.kind == "review")
        let mailbox = try await client.collaborationMailbox(host: host, pane: pane)
        #expect(mailbox.incoming.first?.to.label == "@impl")
        #expect(mailbox.sent.first?.from.label == "operator")
    }

    @Test @MainActor
    func operatorInboxOpensStableAgentWhenItsRecordedPaneIsStale() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        let execution = try await client.executionSnapshot()
        let route = try #require(
            execution.watchHosts.first(where: { $0.host.alias == "local" })?
                .sessions.first?.windows.first?.panes.first
        )
        let request = try JSONDecoder().decode(
            MuxaCollaborationRequest.self,
            from: Data(#"""
            {
              "id":"request-stale-pane",
              "from":{"agent_kind":"unknown","agent_session_id":"__muxa_console__","pane":"console","room":{"host":"tmux","window_id":"@4"},"console":true},
              "to":{"agent_kind":"codex","agent_session_id":"agent-17","pane":"%999","socket":"old-socket","room":{"host":"tmux","window_id":"@999"},"alias":"impl"},
              "kind":"question","body":"Choose a release target","expects_reply":true,"work_mode":"read_only","status":"queued","created_at":"2026-08-31T10:00:00Z"
            }
            """#.utf8)
        )
        let message = MuxaOperatorMessage(
            host: route.host,
            routePane: route.pane,
            request: request
        )

        let selection = AppModel.operatorSelection(for: message, in: execution)

        #expect(selection == .agent("local:agent-17"))
    }

    @Test @MainActor
    func operatorInboxOpensRecordedSessionWhenAgentAndWindowEnded() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        let execution = try await client.executionSnapshot()
        let route = try #require(
            execution.watchHosts.first(where: { $0.host.alias == "local" })?
                .sessions.first?.windows.first?.panes.first
        )
        let request = try JSONDecoder().decode(
            MuxaCollaborationRequest.self,
            from: Data(#"""
            {
              "id":"request-ended-agent",
              "from":{"agent_kind":"unknown","agent_session_id":"__muxa_console__","pane":"console","room":{"host":"tmux","window_id":"@4"},"console":true},
              "to":{"agent_kind":"claude_code","agent_session_id":"ended-agent","pane":"%999","socket":"default","room":{"host":"tmux","socket":"default","window_id":"@999"},"alias":"claude","tmux_session_id":"$4","tmux_session_name":"muxa","window_name":"ended"},
              "kind":"task","body":"Historical request","expects_reply":true,"work_mode":"read_only","status":"completed","created_at":"2026-08-31T10:00:00Z"
            }
            """#.utf8)
        )
        let message = MuxaOperatorMessage(
            host: route.host,
            routePane: route.pane,
            request: request
        )

        let selection = AppModel.operatorSelection(for: message, in: execution)

        #expect(selection == .fleetSession(MuxaWatchSessionIdentity(
            hostAlias: "local",
            socket: "default",
            sessionID: "$4"
        )))
    }

    @Test
    func operatorInboxSeparatesHumanDecisionsFromOrdinaryReplies() throws {
        func message(replyStatus: String) throws -> MuxaOperatorMessage {
            let request = try JSONDecoder().decode(
                MuxaCollaborationRequest.self,
                from: Data("""
                {
                  "id":"request-\(replyStatus)",
                  "from":{"agent_kind":"unknown","agent_session_id":"__muxa_console__","pane":"console","room":{"host":"tmux","window_id":"@4"},"console":true},
                  "to":{"agent_kind":"codex","agent_session_id":"agent-17","pane":"%17","room":{"host":"tmux","window_id":"@4"},"alias":"impl"},
                  "kind":"review","body":"Review this","expects_reply":true,"work_mode":"read_only","status":"\(replyStatus)","created_at":"2026-08-31T10:00:00Z",
                  "reply":{"status":"\(replyStatus)","body":"Result","at":"2026-08-31T10:01:00Z"}
                }
                """.utf8)
            )
            return MuxaOperatorMessage(
                host: MuxaFleetHostIdentity(alias: "local", local: true, state: "online", mode: "control"),
                routePane: MuxaPaneInfo(
                    paneID: "%17",
                    sessionID: "$4",
                    session: "muxa",
                    windowID: "@4",
                    windowName: "native-app",
                    windowIndex: "1",
                    paneIndex: "0",
                    currentCommand: "codex",
                    title: "muxa",
                    currentPath: "/tmp/muxa",
                    socket: "default",
                    workspaceID: nil,
                    workID: nil,
                    agentRole: "implementer",
                    agentAlias: "impl"
                ),
                request: request
            )
        }

        #expect(try message(replyStatus: "blocked").needsHumanDecision)
        #expect(try message(replyStatus: "declined").needsHumanDecision)
        #expect(!(try message(replyStatus: "completed").needsHumanDecision))
    }

    @Test
    func fleetShellModuleSendsToExactHostPane() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        let execution = try await client.executionSnapshot()
        let target = try #require(
            execution.watchHosts
                .first(where: { $0.host.alias == "dev" })?
                .sessions.first?.windows.first?.panes.first
        )
        try await client.sendFleetPrompt(
            host: target.host,
            pane: target.pane,
            text: "continue with the review"
        )
        let sent = try #require(probe.lastFleetPrompt())
        #expect(sent.host == "dev")
        #expect(sent.pane == "%2")
        #expect(sent.text == "continue with the review")
        #expect(sent.submit)
    }

    @Test
    func fleetCaptureUsesTheExactGlobalPaneAddress() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        let execution = try await client.executionSnapshot()
        let target = try #require(
            execution.hostedAgents.first(where: { $0.host.alias == "dev" })
        )
        let pane = try #require(target.pane)

        let capture = try await client.captureFleetPane(host: target.host, pane: pane)

        #expect(capture.screenText?.contains("안녕") == true)
        #expect(capture.rawBytes == Data("\u{001B}[31m$ echo 안녕\u{001B}[0m\r\n안녕".utf8))
        let address = try #require(probe.lastFleetCaptureAddress())
        #expect(address.host == "dev")
        #expect(address.backend == "tmux")
        #expect(address.socket == "default")
        #expect(address.pane == "%2")
    }

    @Test
    func parsesOwnerOnlyMuxadFromLsofFieldOutput() {
        let records = DaemonSocketOwner.parseLsof(
            "p4798\ncmuxad\nu501\nf10\np9999\nczsh\nu501\nf4\n"
        )
        #expect(records.count == 2)
        #expect(records[0].pid == 4798)
        #expect(records[0].command == "muxad")
        #expect(records[0].uid == 501)
        #expect(records[1].command == "zsh")
    }

    @Test
    func recognizesHomebrewDaemonLocations() {
        #expect(
            DaemonSocketOwner.homebrewExecutablePath(
                for: "/opt/homebrew/Cellar/muxa/0.8.35/bin/muxad"
            ) == "/opt/homebrew/bin/brew"
        )
        #expect(
            DaemonSocketOwner.homebrewExecutablePath(
                for: "/Users/test/.cargo/bin/muxad"
            ) == nil
        )
        #expect(DaemonSocketOwner.legacyLaunchAgentLabels.contains("dev.open330.muxad"))
    }

    @Test
    func serializesConcurrentIPCRequests() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)

        try await withThrowingTaskGroup(of: Void.self) { group in
            for _ in 0..<24 {
                group.addTask {
                    try await client.hello()
                }
            }
            try await group.waitForAll()
        }

        #expect(probe.snapshot().maximumActive == 1)
    }

    @Test @MainActor
    func rapidPaneRestartBalancesAttachmentLifecycle() async throws {
        let probe = IPCProbe(firstAttachDelayMicroseconds: 80_000)
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        let pane = TerminalPaneModel(
            client: client,
            sessionID: "pty:test",
            replayInitialHistory: false
        )

        pane.start()
        try await Task.sleep(for: .milliseconds(10))
        pane.stop()
        pane.start()
        try await Task.sleep(for: .milliseconds(250))
        pane.stop()
        try await Task.sleep(for: .milliseconds(150))

        let snapshot = probe.snapshot()
        #expect(snapshot.attachmentBalance == 0)
        #expect(snapshot.maximumAttachment == 1)
    }

    @Test @MainActor
    func emptyTruncatedReadAdvancesTerminalCursor() async throws {
        let probe = IPCProbe(truncateFirstRead: true)
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        let pane = TerminalPaneModel(
            client: client,
            sessionID: "pty:test",
            replayInitialHistory: true
        )

        pane.start()
        try await Task.sleep(for: .milliseconds(180))
        pane.stop()
        try await Task.sleep(for: .milliseconds(100))

        #expect(Array(probe.snapshot().offsets.prefix(2)) == [0, 50])
    }

    @Test @MainActor
    func normalShellExitIsAHostStateNotATerminalError() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        let pane = TerminalPaneModel(
            client: client,
            sessionID: "pty:test",
            replayInitialHistory: false
        )

        pane.start()
        try await Task.sleep(for: .milliseconds(120))

        #expect(pane.exited)
        #expect(pane.exitStatus == 0)
        #expect(pane.errorMessage == nil)
        pane.stop()
    }

    @Test
    func initialTerminalResizeSurvivesPumpActivation() async throws {
        let probe = IPCProbe()
        let client = MuxaIPCClient(socketPath: "/tmp/muxa-test.sock", request: probe.request)
        try await client.hello()
        let pump = TerminalSessionIOPump(client: client, sessionID: "pty:test")

        await pump.enqueueResize(columns: 132, rows: 41)
        await pump.setActive(true)
        try await Task.sleep(for: .milliseconds(80))
        let resize = try #require(probe.lastResize())

        #expect(resize.columns == 132)
        #expect(resize.rows == 41)
        await pump.setActive(false)
    }

    @Test @MainActor
    func workbenchPreviewReplacesOnlyThePreviousPreview() throws {
        let tabs = MuxaWorkbenchTabs(persistenceKey: nil)
        let groupID = tabs.focusedGroupID

        tabs.openPreview(.host("host-a"))
        tabs.openPreview(.host("host-b"))
        var group = try #require(tabs.group(id: groupID))
        #expect(group.tabs == [.workBoard, .host("host-b")])
        #expect(group.preview == .host("host-b"))

        tabs.pin(.host("host-b"), groupID: groupID)
        tabs.openPreview(.shell("pty:1"))
        group = try #require(tabs.group(id: groupID))
        #expect(group.tabs == [.workBoard, .host("host-b"), .shell("pty:1")])
        #expect(group.preview == .shell("pty:1"))
    }

    @Test @MainActor
    func closingEditorReturnsToMostRecentlyUsedOpenTab() {
        let tabs = MuxaWorkbenchTabs(persistenceKey: nil)
        let groupID = tabs.focusedGroupID
        tabs.openPinned(.host("host-a"))
        tabs.openPinned(.shell("pty:1"))
        tabs.activate(.host("host-a"), groupID: groupID)

        let next = tabs.close(.host("host-a"), groupID: groupID)

        #expect(next == .shell("pty:1"))
        #expect(tabs.focusedSelection == .shell("pty:1"))
    }

    @Test @MainActor
    func splittingEditorCreatesIndependentPinnedGroup() throws {
        let tabs = MuxaWorkbenchTabs(persistenceKey: nil)
        let sourceID = tabs.focusedGroupID
        tabs.openPreview(.shell("pty:1"))

        let targetID = tabs.splitRight(selection: .shell("pty:1"), from: sourceID)
        let target = try #require(tabs.group(id: targetID))

        #expect(tabs.groups.count == 2)
        #expect(target.tabs == [.shell("pty:1")])
        #expect(target.preview == nil)
        #expect(tabs.focusedSelection == .shell("pty:1"))
    }

    @Test @MainActor
    func workbenchRestoresPinnedTabsAndEditorGroups() throws {
        let suite = "dev.muxa.tests.workbench.\(UUID().uuidString)"
        let defaults = try #require(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }
        let key = "tabs"
        let tabs = MuxaWorkbenchTabs(persistenceKey: key, defaults: defaults)
        tabs.openPinned(.host("host-a"))
        _ = tabs.splitRight(selection: .shell("pty:1"), from: tabs.focusedGroupID)

        let restored = MuxaWorkbenchTabs(persistenceKey: key, defaults: defaults)

        #expect(restored.groups.count == 2)
        #expect(restored.groups.flatMap(\.tabs).contains(.host("host-a")))
        #expect(restored.focusedSelection == .shell("pty:1"))
    }

    @Test @MainActor
    func activityBarSeparatesOutcomesTopologyInboxAndShells() {
        let model = AppModel()
        #expect(MuxaSidebarMode.allCases == [.work, .watch, .inbox, .shells])

        model.select(.host("rtzr"))

        #expect(model.sidebarMode == .watch)
        #expect(model.sidebarSelection == .host("rtzr"))
    }

    @Test @MainActor
    func watchTreeSelectionOpensAPaneEditor() {
        let model = AppModel()
        let pane = MuxaWatchPaneIdentity(
            hostAlias: "local",
            socket: "/tmp/tmux.sock",
            paneID: "%7"
        )

        model.selectWatchPane(pane)

        #expect(model.watchSelection == pane)
        #expect(model.sidebarSelection == .pane(pane))
        #expect(model.sidebarMode == .watch)
    }

    @Test @MainActor
    func globalAskIsAnIndependentEditorRoute() {
        let model = AppModel()

        model.select(.ask)

        #expect(model.sidebarSelection == .ask)
        #expect(model.sidebarMode == .inbox)
    }

    @Test @MainActor
    func operatorInboxIsAnIndependentEditorRoute() {
        let model = AppModel()

        model.select(.inbox)

        #expect(model.sidebarSelection == .inbox)
        #expect(model.sidebarMode == .inbox)
    }

    @Test @MainActor
    func activatingAnOpenPaneTabRestoresItsExplorerSelection() {
        let model = AppModel()
        let first = MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%1")
        let second = MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%2")
        model.selectWatchPane(second)

        model.activateEditor(.pane(first))

        #expect(model.sidebarSelection == .pane(first))
        #expect(model.watchSelection == first)
        #expect(model.sidebarMode == .watch)
    }

    @Test @MainActor
    func agentSelectionUsesTheAttentionInbox() {
        let model = AppModel()
        model.select(.agent("local:agent-17"))

        #expect(model.sidebarMode == .inbox)
        #expect(model.sidebarSelection == .agent("local:agent-17"))
        #expect(!MuxaSidebarMode.allCases.map(\.rawValue).contains("agents"))
    }

    @Test @MainActor
    func fleetSessionSelectionOpensAResourceSummaryWithoutChangingPane() {
        let model = AppModel()
        let pane = MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%1")
        let session = MuxaWatchSessionIdentity(
            hostAlias: "rtzr",
            socket: "default",
            sessionID: "$107"
        )
        model.selectWatchPane(pane)

        model.selectWatchSession(session)

        #expect(model.sidebarMode == .watch)
        #expect(model.sidebarSelection == .fleetSession(session))
        #expect(model.watchSelection == pane)
    }

    @Test @MainActor
    func fleetWindowSelectionOpensAResourceSummaryWithoutChangingPane() {
        let model = AppModel()
        let pane = MuxaWatchPaneIdentity(hostAlias: "local", socket: "default", paneID: "%1")
        let window = MuxaWatchWindowIdentity(
            hostAlias: "rtzr",
            socket: "default",
            sessionID: "$107",
            windowID: "@213"
        )
        model.selectWatchPane(pane)

        model.selectWatchWindow(window)

        #expect(model.sidebarMode == .watch)
        #expect(model.sidebarSelection == .fleetWindow(window))
        #expect(model.watchSelection == pane)
    }
}
