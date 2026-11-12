import Foundation
import Testing
@testable import Tend

// Golden daemon replies, recorded by the Rust test
// `crates/tend/src/ipc/tests/swift_fixtures.rs` from a real in-memory
// tendd. Each one is decoded with the app's own `TendIPCResponse`, and the
// fields the app reads from that reply must come through. `TendLenient`
// fields read as nil on a shape mismatch, so they are checked for a value,
// not just for decoding.

private let fixtureDirectory = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent()
    .appendingPathComponent("Fixtures/IPC")

private func reply(_ name: String) throws -> TendIPCResponse {
    let data = try Data(contentsOf: fixtureDirectory.appendingPathComponent("\(name).json"))
    let response = try JSONDecoder().decode(TendIPCResponse.self, from: data)
    #expect(response.ok, "\(name) is not an ok reply")
    return response
}

/// Every fixture the Rust side records, with what the app needs from it.
private let checks: [String: (TendIPCResponse) -> Bool] = [
    "hello": { $0.minProtocol != nil && $0.maxProtocol != nil && $0.capabilities?.isEmpty == false && $0.bootID != nil },
    "snapshot": { $0.agents?.count == 1 },
    "list_sessions": { $0.sessions != nil },
    "pipeline_runs": { $0.pipelineRuns != nil },
    "ask_status": { $0.askEnabled != nil },
    "ask_list": { $0.askEntries != nil },
    "ask_providers": { $0.askProviders?.isEmpty == false },
    "review_list": { $0.reviewItems?.count == 1 && $0.reviewHasMore != nil },
    "decision_queue": { $0.decisionQueue?.value != nil },
    "decision_history": { $0.decisionHistory?.value != nil },
    "work_backlog": { ($0.workRecords ?? []).compactMap(\.value).count == 1 },
]

@Test(arguments: checks.keys.sorted())
func goldenDaemonReplyDecodes(name: String) throws {
    let response = try reply(name)
    #expect(checks[name]?(response) == true, "the app cannot read what it needs from \(name).json")
}

@Test func everyGoldenReplyHasACheck() throws {
    let files = try FileManager.default.contentsOfDirectory(atPath: fixtureDirectory.path)
        .filter { $0.hasSuffix(".json") }
        .map { String($0.dropLast(".json".count)) }
    #expect(Set(files) == Set(checks.keys), "add a check for each new fixture in TendIPCFixtureTests")
}
