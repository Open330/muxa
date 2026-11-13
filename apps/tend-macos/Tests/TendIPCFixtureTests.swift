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
private let checks: [String: @Sendable (TendIPCResponse) -> Bool] = [
    "hello": { $0.minProtocol != nil && $0.maxProtocol != nil && $0.capabilities?.isEmpty == false && $0.bootID != nil },
    "snapshot": { $0.agents?.count == 1 },
    "list_sessions": { $0.sessions != nil },
    "pipeline_runs": { $0.pipelineRuns != nil },
    "ask_status": { $0.askEnabled != nil },
    "ask_list": { $0.askEntries != nil },
    "ask_providers": { $0.askProviders?.isEmpty == false },
    "review_list": { $0.reviewItems?.count == 1 && $0.reviewHasMore != nil },
    // The queue and history decoders skip items they cannot read and count
    // them in `undecoded`, so check the seeded item came through as a review.
    "decision_queue": { response in
        guard let queue = response.decisionQueue?.value, queue.undecoded == 0, queue.items.count == 1,
              case .review = queue.items[0] else { return false }
        return true
    },
    "decision_history": { response in
        guard let history = response.decisionHistory?.value, history.undecoded == 0,
              history.items.count == 1, case .review = history.items[0] else { return false }
        return true
    },
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
