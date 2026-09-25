import Foundation
import Testing
@testable import Muxa

// Documents shaped like `muxa snapshot --list --json` and
// `muxa restore --only-missing --json`, which muxad passes through verbatim.
private let listingJSON = #"""
{"ok":true,"mux_snapshot":{"root":"/s","snapshots":[
 {"id":"1790000900","dir":"/s/1790000900","summary":{"taken_at":"2026-09-23T05:05:12.249399Z","origin":"auto","host":"tmux","socket":"default","sessions":2,"windows":6,"panes":32,"agents":5,"resumable":4,"not_replayable":1}},
 {"id":"broken","dir":"/s/broken","error":"parsing /s/broken/snapshot.json: expected value"}
]}}
"""#

private let planJSON = #"""
{"id":"1790000900","dir":"/s/1790000900","summary":{"taken_at":"2026-09-23T05:05:12Z","origin":"manual","host":"tmux","socket":"default","sessions":2,"windows":2,"panes":4,"agents":1,"resumable":1,"not_replayable":1},
 "socket":"default","only_missing":true,"layout_only":false,"server_reachable":true,"run":false,
 "sessions":[
  {"name":"work","action":"skip","windows":[{"index":"0","name":"main","layout":"x","panes":[
    {"index":"0","path":"/tmp","action":"shell"}]}]},
  {"name":"side","action":"create","windows":[{"index":"1","name":"dev","layout":"x","panes":[
    {"index":"0","path":"HOME/p","action":"resume","command":"claude --resume abc","agent_kind":"claude_code"},
    {"index":"1","path":"/srv","action":"replay","command":"npm run dev -- --port 5173 --host 0.0.0.0"},
    {"index":"2","path":"/srv","action":"manual","command":"puma 5.6.8 (tcp://0.0.0.0:5072)"},
    {"index":"3","path":"/srv","action":"teleport"},
    {"index":"4","path":"/srv","action":"resume_by_hand","agent_kind":"codex","note":"command line was not captured; resume by hand with `codex resume xyz`"}]}]}
 ]}
"""#

private let reportJSON = #"""
{"id":"1790000900","dir":"/s/1790000900","socket":"default","only_missing":true,"layout_only":false,"server_reachable":true,"run":true,
 "sessions":[
  {"name":"work","action":"skip","result":"skipped","windows":[]},
  {"name":"side","action":"create","result":"created","windows":[{"index":"1","name":"dev","layout":"x","panes":[
    {"index":"0","path":"/srv","action":"resume","command":"claude --resume abc","agent_kind":"claude_code","result":"relaunched"},
    {"index":"1","path":"/srv","action":"replay","command":"make","result":"failed","error":"can't find pane: =side:1.1"},
    {"index":"2","path":"/srv","action":"manual","command":"puma","result":"manual"},
    {"index":"3","path":"/srv","action":"replay","command":"true","result":"unconfirmed","error":"sent, but the pane was still at its shell prompt after 10s"}]}]}
 ],
 "totals":{"sessions_created":1,"sessions_skipped":1,"sessions_failed":0,"panes":4,"relaunched":1,"unconfirmed":1,"shell":0,"manual":1,"failed":1}}
"""#

// `muxa restore --json` without `--only-missing`, compared with a server
// that runs `work` with window 0 (one of its two panes) and a window 5 the
// snapshot never saw, lacks `side` and `docs`, and has gained `scratch`.
private let compareJSON = #"""
{"id":"1790000900","dir":"/s/1790000900","socket":"default","only_missing":false,"layout_only":false,"server_reachable":true,"run":false,
 "sessions":[
  {"name":"work","action":"fill_missing","state":"present",
   "new_windows":[{"index":"5","name":"logs","panes":[{"index":"0","path":"/var/log","command":"tail"}]}],
   "windows":[
    {"index":"0","name":"main","layout":"x","state":"present","panes":[
      {"index":"0","path":"/tmp","action":"shell","state":"present"},
      {"index":"1","path":"/tmp","action":"replay","command":"make watch","state":"missing"}]},
    {"index":"1","name":"edit","layout":"x","state":"missing","panes":[
      {"index":"0","path":"/tmp","action":"shell","state":"missing"}]}]},
  {"name":"side","action":"create","state":"missing","windows":[{"index":"0","name":"dev","layout":"x","state":"missing","panes":[
    {"index":"0","path":"/srv","action":"replay","command":"npm run dev","state":"missing"}]}]},
  {"name":"docs","action":"create","state":"missing","windows":[{"index":"0","name":"d","layout":"x","state":"missing","panes":[
    {"index":"0","path":"/srv","action":"shell","state":"missing"}]}]}
 ],
 "new_sessions":[{"name":"scratch","windows":[{"index":"0","name":"notes","panes":[
   {"index":"0","path":"/home/me","command":"vim"},{"index":"1","path":"/home/me"}]}]}]}
"""#

private func plan(_ json: String) throws -> MuxSnapshotPlan {
    try JSONDecoder().decode(
        MuxSnapshotPlan.self,
        from: Data(json.replacingOccurrences(of: "HOME", with: NSHomeDirectory()).utf8)
    )
}

@Test func snapshotListingDecodesSummariesAndUnreadableRows() async throws {
    let handler: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        #expect(object["kind"] as? String == "mux_snapshot_list")
        return Data(listingJSON.utf8)
    }
    let client = MuxaSessionSnapshotClient(socketPath: "/tmp/muxa-snapshot-test.sock", request: handler)
    let entries = try await client.list()
    #expect(entries.map(\.id) == ["1790000900", "broken"])
    let summary = try #require(entries[0].summary)
    #expect(summary.isAutomatic)
    #expect(summary.takenAt != nil, "fractional RFC 3339 parses")
    #expect(summary.notReplayable == 1)
    #expect(summary.serverLabel == "tmux default")
    #expect(MuxSnapshotSummary(socket: "/private/tmp/tmux-501/work").serverLabel == "tmux work")
    #expect(summary.countsLabel == "2 sessions · 6 windows · 32 panes")
    #expect(!entries[1].readable)
    #expect(entries[1].error?.contains("expected value") == true)
}

@Test func snapshotClientSendsIdsAndSurfacesServerErrors() async throws {
    let handler: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        switch object["kind"] as? String {
        case "mux_snapshot_plan":
            #expect(object["id"] as? String == "1790000900")
            return Data(#"{"ok":true,"mux_snapshot":\#(planJSON)}"#.utf8)
        case "mux_snapshot_restore":
            #expect(object["layout_only"] as? Bool == true)
            return Data(#"{"ok":true,"mux_snapshot_operation":{"operation_id":"snapshot-restore-1","state":"running","id":"1790000900","message":"Restoring snapshot…"}}"#.utf8)
        case "mux_snapshot_save":
            return Data(#"{"ok":false,"error":"agents span several servers (a, b); pass --mux-socket to name one"}"#.utf8)
        default:
            return Data(#"{"ok":true}"#.utf8)
        }
    }
    let client = MuxaSessionSnapshotClient(socketPath: "/tmp/muxa-snapshot-test.sock", request: handler)
    let plan = try await client.plan(id: "1790000900")
    #expect(plan.sessionsToCreate == 1)
    #expect(plan.sessionsToSkip == 1)
    let operation = try await client.restore(id: "1790000900", layoutOnly: true)
    #expect(operation.state == .running)
    #expect(operation.operationID == "snapshot-restore-1")
    try await client.delete(id: "1790000900")
    await #expect(throws: MuxaIPCError.self) { _ = try await client.save() }
}

@Test func planTreeSaysWhatEachSessionAndPaneWillDo() throws {
    let rows = SessionSnapshotTree.rows(plan: try plan(planJSON))
    let session = rows.filter { $0.depth == 0 }
    #expect(session.map(\.title) == ["work", "side"])
    #expect(session.map(\.badge) == [.exists, .willCreate])

    let panes = rows.filter { $0.depth == 2 }
    // A skipped session's panes are listed for reference, with no action.
    #expect(panes[0].badge == nil)
    #expect(panes[1].badge == .resume("claude"))
    #expect(panes[1].title.hasPrefix("~/"), "home is abbreviated")
    #expect(panes[2].badge == .replay("npm run dev -- --port 5173 --ho…"))
    #expect(panes[2].help == "npm run dev -- --port 5173 --host 0.0.0.0", "full command on hover")
    #expect(panes[3].badge == .notReplayable)
    #expect(panes[4].badge == nil, "an action this build does not know stays unlabelled")
    #expect(panes[5].badge == .resumeByHand)
    #expect(panes[5].help?.contains("codex resume xyz") == true, "the note is the hover text")
    #expect(rows.filter { $0.depth == 1 }.first?.detail == "1 pane")
}

@Test func reportTreeShowsResultsAndErrors() throws {
    let report = try plan(reportJSON)
    let rows = SessionSnapshotTree.rows(plan: report)
    #expect(rows.filter { $0.depth == 0 }.map(\.badge) == [.skipped, .created])
    let panes = rows.filter { $0.depth == 2 }
    #expect(panes.map(\.badge) == [.relaunched, .failed, .startByHand, .unconfirmed])
    #expect(panes[1].message == "can't find pane: =side:1.1")
    #expect(panes[3].message?.hasPrefix("sent, but") == true, "why it is not confirmed")
    let totals = try #require(report.totals)
    #expect(SessionSnapshotTree.totalsLine(totals)
        == "Created 1, skipped 1; relaunched 1 of 4 panes · 1 not confirmed · 1 to start by hand · 1 failed")
}

@Test func summaryLineCountsWhatIsMissingRunningAndNew() throws {
    // A CLI that predates the comparison: presence comes from the action.
    let old = try plan(planJSON)
    #expect(!old.isCompared)
    let unflagged = try plan(planJSON.replacingOccurrences(of: #""only_missing":true,"#, with: ""))
    #expect(unflagged.onlyMissing, "a document without the field left running sessions alone")
    #expect(SessionSnapshotTree.summaryLine(old) == "1 session to recreate, 1 already running")
    let allExist = planJSON.replacingOccurrences(of: #""action":"create""#, with: #""action":"skip""#)
    #expect(SessionSnapshotTree.summaryLine(try plan(allExist)) == "No sessions to recreate, 2 already running")

    let compared = try plan(compareJSON)
    #expect(compared.isCompared)
    #expect(!compared.onlyMissing)
    let counts = SessionSnapshotComparison(plan: compared)
    #expect(counts.toRecreate == 2)
    #expect(counts.running == 1)
    #expect(counts.newSinceSnapshot == 1)
    #expect(counts.newWindows == 1)
    #expect(counts.missingWindows == 1)
    #expect(counts.missingPanes == 2, "the second pane of window 0 and the pane of window 1")
    #expect(SessionSnapshotTree.summaryLine(compared)
        == "2 sessions to recreate, 1 already running, 1 new since snapshot, 2 panes to add to running sessions")
    let onlyMissing = compareJSON.replacingOccurrences(of: #""only_missing":false"#, with: #""only_missing":true"#)
    #expect(SessionSnapshotTree.summaryLine(try plan(onlyMissing))
        == "2 sessions to recreate, 1 already running, 1 new since snapshot", "nothing is added to running sessions")
}

@Test func restoreTitleSaysWhatPressingItDoes() throws {
    let compared = try plan(compareJSON)
    #expect(SessionSnapshotTree.restoreTitle(compared) == "Recreate 2, Fill In 1")
    #expect(SessionSnapshotTree.sessionsToRestore(compared) == 3)
    let onlyMissing = try plan(compareJSON.replacingOccurrences(
        of: #""only_missing":false"#, with: #""only_missing":true"#
    ))
    #expect(SessionSnapshotTree.restoreTitle(onlyMissing) == "Recreate 2 Sessions")
    #expect(SessionSnapshotTree.restoreTitle(try plan(planJSON)) == "Recreate 1 Session")
    let allExist = planJSON.replacingOccurrences(of: #""action":"create""#, with: #""action":"skip""#)
    #expect(SessionSnapshotTree.restoreTitle(try plan(allExist)) == "Nothing to Restore")
    let fillOnly = compareJSON
        .replacingOccurrences(of: #""action":"create""#, with: #""action":"fill_missing""#)
    #expect(SessionSnapshotTree.restoreTitle(try plan(fillOnly)) == "Fill In 3 Sessions")
}

@Test func compareTreeMarksMissingRunningAndNewLines() throws {
    let rows = SessionSnapshotTree.rows(plan: try plan(compareJSON))
    let sessions = rows.filter { $0.depth == 0 }
    #expect(sessions.map(\.title) == ["work", "side", "docs", "scratch"])
    #expect(sessions.map(\.mark) == [.running, .missing, .missing, .new])
    #expect(sessions.map(\.badge) == [.willFillMissing, .willCreate, .willCreate, nil])

    // Under a running session only what differs is marked.
    let work = rows.filter { $0.id.hasPrefix("s:work/") }
    #expect(work.map(\.id) == [
        "s:work/0", "s:work/0/0", "s:work/0/1", "s:work/1", "s:work/1/0", "s:work/+5", "s:work/+5/0",
    ])
    #expect(work.map(\.mark) == [nil, nil, .missing, .missing, nil, .new, nil])
    #expect(work.last?.detail == "tail", "a new window's pane says what it runs")

    // A missing session's lines are all missing; only the session says so.
    #expect(rows.filter { $0.id.hasPrefix("s:side/") }.allSatisfy { $0.mark == nil })

    let scratch = rows.filter { $0.id.hasPrefix("n:scratch/") }
    #expect(scratch.map(\.depth) == [1, 2, 2])
    #expect(scratch.map(\.detail) == ["2 panes", "vim", nil])
    #expect(scratch.allSatisfy { $0.badge == nil }, "a restore does nothing to a new session")
}

@Test func reportTreeDropsTheComparison() throws {
    let report = compareJSON.replacingOccurrences(of: #""run":false"#, with: #""run":true"#)
    let rows = SessionSnapshotTree.rows(plan: try plan(report))
    #expect(rows.allSatisfy { $0.mark == nil }, "the comparison was taken before the restore ran")
    #expect(!rows.contains { $0.id.hasPrefix("n:") })
}

@Test func operationKeepsStateWhenTheReportIsUnreadable() throws {
    let json = #"{"operation_id":"snapshot-restore-2","state":"failed","id":"x","message":"creating session work","result":{"totals":{}}}"#
    let operation = try JSONDecoder().decode(MuxSnapshotOperation.self, from: Data(json.utf8))
    #expect(operation.state == .failed)
    #expect(operation.message == "creating session work")
    #expect(operation.result == nil)
}

/// A muxad stand-in answering the sheet's requests with `planBody` as the
/// dry run and `reportJSON` as the finished restore.
private func sheetHandler(planBody: String) -> MuxaIPCRequestHandler {
    { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        switch object["kind"] as? String {
        case "mux_snapshot_list": return Data(listingJSON.utf8)
        case "mux_snapshot_plan": return Data(#"{"ok":true,"mux_snapshot":\#(planBody)}"#.utf8)
        case "mux_snapshot_restore":
            return Data(#"{"ok":true,"mux_snapshot_operation":{"operation_id":"op","state":"running","id":"1790000900","message":""}}"#.utf8)
        case "mux_snapshot_restore_status":
            return Data(#"{"ok":true,"mux_snapshot_operation":{"operation_id":"op","state":"succeeded","id":"1790000900","message":"Snapshot restored","result":\#(reportJSON)}}"#.utf8)
        default: return Data(#"{"ok":true}"#.utf8)
        }
    }
}

@MainActor
@Test func restoreIsOfferedOnlyWhenThePlanCreatesSomething() async throws {
    let handler = sheetHandler(planBody: planJSON)
    let viewModel = SessionSnapshotViewModel(
        client: MuxaSessionSnapshotClient(socketPath: "/tmp/muxa-snapshot-test.sock", request: handler),
        pollInterval: .milliseconds(1)
    )
    await viewModel.load()
    #expect(viewModel.selection == "1790000900", "the newest readable snapshot is selected")
    #expect(viewModel.plan?.sessionsToCreate == 1)
    #expect(viewModel.canRestore)

    await viewModel.restore()
    #expect(viewModel.phase == .finished)
    #expect(viewModel.plan?.run == true, "the report replaces the plan")
    #expect(!viewModel.canRestore, "a finished restore is not offered again")
    #expect(viewModel.statusMessage?.hasPrefix("Created 1, skipped 1") == true)

    let allExist = planJSON.replacingOccurrences(of: #""action":"create""#, with: #""action":"skip""#)
    let fresh = SessionSnapshotViewModel(
        client: MuxaSessionSnapshotClient(
            socketPath: "/tmp/muxa-snapshot-test.sock", request: sheetHandler(planBody: allExist)
        )
    )
    await fresh.load()
    #expect(!fresh.canRestore, "nothing to create, nothing to press")

    let old = SessionSnapshotViewModel(
        client: MuxaSessionSnapshotClient(socketPath: "/tmp/muxa-snapshot-test.sock", request: handler)
    )
    old.markUnsupported()
    #expect(!old.supported)
    #expect(old.error == SessionSnapshotViewModel.unsupportedMessage)
    #expect(!old.canRestore)
}

@MainActor
@Test func onlyMissingTogglePlansAgainAndRestoresTheSameWay() async throws {
    let requests = MuxSnapshotRequestLog()
    let handler: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        let kind = object["kind"] as? String ?? ""
        let onlyMissing = object["only_missing"] as? Bool
        requests.append("\(kind) \(onlyMissing.map { String($0) } ?? "-")")
        switch kind {
        case "mux_snapshot_list": return Data(listingJSON.utf8)
        case "mux_snapshot_plan":
            let body = compareJSON.replacingOccurrences(
                of: #""only_missing":false"#, with: #""only_missing":\#(onlyMissing ?? true)"#
            )
            return Data(#"{"ok":true,"mux_snapshot":\#(body)}"#.utf8)
        case "mux_snapshot_restore":
            return Data(#"{"ok":true,"mux_snapshot_operation":{"operation_id":"op","state":"succeeded","id":"1790000900","message":"Snapshot restored"}}"#.utf8)
        default: return Data(#"{"ok":true}"#.utf8)
        }
    }
    let viewModel = SessionSnapshotViewModel(
        client: MuxaSessionSnapshotClient(socketPath: "/tmp/muxa-snapshot-test.sock", request: handler),
        pollInterval: .milliseconds(1)
    )
    await viewModel.load()
    #expect(viewModel.onlyMissing)
    #expect(viewModel.restoreButtonTitle == "Recreate 2 Sessions")

    // A muxad without `mux_snapshot_plan_v1` is never asked to fill.
    await viewModel.setOnlyMissing(false)
    #expect(viewModel.onlyMissing)

    viewModel.markCanFillRunningSessions()
    await viewModel.setOnlyMissing(false)
    #expect(!viewModel.onlyMissing)
    #expect(viewModel.plan?.onlyMissing == false, "the preview was planned again")
    #expect(viewModel.restoreButtonTitle == "Recreate 2, Fill In 1")
    #expect(viewModel.canRestore)
    await viewModel.restore()
    #expect(requests.entries == [
        "mux_snapshot_list -",
        "mux_snapshot_plan true",
        "mux_snapshot_plan false",
        "mux_snapshot_restore false",
    ])
}

/// The requests a stand-in muxad saw, in order.
private final class MuxSnapshotRequestLog: @unchecked Sendable {
    private let lock = NSLock()
    private var values: [String] = []

    func append(_ value: String) {
        lock.lock()
        values.append(value)
        lock.unlock()
    }

    var entries: [String] {
        lock.lock()
        defer { lock.unlock() }
        return values
    }
}

@MainActor
@Test func snapshotPaletteCommandsNeedTheDaemon() {
    let model = AppModel()
    #expect(MuxaPaletteCommand.saveSnapshot.disabledReason(model: model) == "Daemon is not connected")
    #expect(MuxaPaletteCommand.restoreSnapshot.disabledReason(model: model) == "Daemon is not connected")
    #expect(MuxaPaletteCommand.saveSnapshot.shortcut == nil)
    #expect(MuxaPaletteCommand.restoreSnapshot.title == "Session: Restore snapshot…")
}
