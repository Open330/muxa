import Foundation

@main
struct WorkspaceTests {
    static func main() throws {
        // Exercise the CLI boundary without Apple Intelligence or a real daemon.
        let directory = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let cli = directory.appendingPathComponent("muxa")
        try "#!/bin/sh\ncat \"$0.json\"\n".write(to: cli, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: cli.path)
        let status = directory.appendingPathComponent("muxa.json")
        let workspace = Workspace(socket: "/unused", cli: cli.path)
        let request = TurnRequest(
            prompt: "List the current agent sessions",
            history: [.init(prompt: "List agents", answer: "%3 is reviewing code")],
            muxa: workspace
        )

        try #"{"sessions":[]}"#.write(to: status, atomically: true, encoding: .utf8)
        let grounded = try groundedTurn(for: request)
        let replay = try modelInput(for: request, grounded: grounded, history: ArraySlice(request.history ?? []))
        precondition(replay.history.isEmpty)
        precondition(replay.prompt.contains("List agents"))
        precondition(!replay.prompt.contains("%3 is reviewing code"))
        let placeholder = "Earlier answer omitted. Use the current workspace snapshot and tools for session facts."
        let contaminated = TurnRequest(
            prompt: "좋습니다 명령을 내릴수있는지 검토해보세요",
            history: [.init(prompt: "에이전트 세션을 볼 수 있나요?", answer: placeholder)],
            muxa: workspace
        )
        let clean = try modelInput(
            for: contaminated, grounded: groundedTurn(for: contaminated),
            history: ArraySlice(contaminated.history ?? [])
        )
        precondition(clean.history.isEmpty)
        precondition(!clean.prompt.contains(placeholder))
        precondition(clean.prompt.contains(contaminated.prompt))
        precondition(clean.prompt.contains("에이전트 세션을 볼 수 있나요?"))
        let trimmed = try modelInput(for: contaminated, grounded: grounded, history: [])
        precondition(trimmed.prompt == grounded.prompt)
        let empty = grounded.prompt
        precondition(empty.contains("tracking no agent sessions"))
        precondition(empty.contains(request.prompt))
        precondition(!empty.contains("%3 is reviewing code"))

        let snapshot = #"{"sessions":[{"name":"work","windows":[{"name":"editor","panes":[{"key":{"pane_id":"%91"},"title":"shell"},{"key":{"pane_id":"%92"},"agent":{"kind":"codex","state":"idle","cwd":"/tmp/actual-project","last_prompt":"Check parser","last_response":"Parser checked"}}]}]}]}"#
        try snapshot.write(to: status, atomically: true, encoding: .utf8)
        let populated = try groundedTurn(for: request).prompt
        precondition(populated.contains("%92"))
        precondition(populated.contains("/tmp/actual-project"))
        precondition(!populated.contains("Check parser"))
        precondition(!populated.contains("Parser checked"))
        let listed = sessionsDigest(agentSessions(fromStatusJSON: Data(snapshot.utf8))!)
        precondition(listed.contains("Check parser"))
        precondition(listed.contains("Parser checked"))
        precondition(!populated.contains("%91"))
        precondition(!populated.contains("%3 is reviewing code"))

        let missing = try groundedTurn(for: TurnRequest(prompt: "%3 세션은 무엇을 하나요?", muxa: workspace))
        precondition(missing.answer?.contains("%3은 현재 조회 목록에 없습니다") == true)
        precondition(missing.answer?.contains("%92") == true)
        let present = try groundedTurn(for: TurnRequest(prompt: "%92 세션을 알려주세요", muxa: workspace))
        precondition(present.answer == nil)

        for invalid in ["not json", "{}", #"{"sessions":null}"#] {
            try invalid.write(to: status, atomically: true, encoding: .utf8)
            do {
                _ = try groundedTurn(for: request)
                preconditionFailure("Invalid status must fail instead of generating an answer")
            } catch let error as HelperFailure {
                precondition(error.reason == "workspace_unavailable")
            }
        }
        try FileManager.default.removeItem(at: cli)
        do {
            _ = try groundedTurn(for: request)
            preconditionFailure("An unavailable CLI must fail the turn")
        } catch let error as HelperFailure {
            precondition(error.reason == "workspace_unavailable")
        }

        let draft = TurnRequest(prompt: "Draft a reply", history: request.history)
        let draftInput = try modelInput(
            for: draft, grounded: groundedTurn(for: draft), history: ArraySlice(draft.history ?? [])
        )
        precondition(draftInput.history.first?.answer == "%3 is reviewing code")
        let bare = try groundedTurn(for: draft).prompt
        precondition(bare == draft.prompt)
        print("AFM workspace tests passed")
    }
}
