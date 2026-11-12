import Foundation
import Testing
@testable import Tend

private final class LockedBox<Value>: @unchecked Sendable {
    private let lock = NSLock()
    private var stored: Value

    init(_ value: Value) { stored = value }

    var value: Value {
        lock.lock()
        defer { lock.unlock() }
        return stored
    }

    func update(_ body: (inout Value) -> Void) {
        lock.lock()
        body(&stored)
        lock.unlock()
    }
}

/// Far more than a pipe's ~64 KB buffer, on both streams, with stderr
/// written first: the old wait-then-read (and stdout-then-stderr) order
/// hung forever on this.
@Test func cliProcessDrainsLargeOutputOnBothStreamsWithoutHanging() throws {
    let size = 300_000
    let script = """
    n=$(wc -c | tr -d ' ')
    head -c \(size) /dev/zero | tr '\\0' e >&2
    head -c \(size) /dev/zero | tr '\\0' o
    printf '\\n%s' "$n"
    """
    let input = Data(repeating: UInt8(ascii: "i"), count: 250_000)
    let outcome = LockedBox<Result<TendCLIProcessResult, Error>?>(nil)
    let finished = DispatchSemaphore(value: 0)
    DispatchQueue.global().async {
        let result = Result {
            try TendCLIProcess.run(
                executable: URL(fileURLWithPath: "/bin/sh"),
                arguments: ["-c", script],
                input: input
            )
        }
        outcome.update { $0 = result }
        finished.signal()
    }
    #expect(finished.wait(timeout: .now() + 30) == .success, "the CLI run hung on a full pipe")
    let result = try #require(outcome.value).get()
    #expect(result.terminationStatus == 0)
    #expect(result.standardError.count == size)
    let output = String(decoding: result.standardOutput, as: UTF8.self)
    #expect(output.count == size + 1 + "250000".count)
    #expect(output.hasSuffix("\n250000"))
}

@Test func cliProcessReportsFailureStatusAndStderr() throws {
    let result = try TendCLIProcess.run(
        executable: URL(fileURLWithPath: "/bin/sh"),
        arguments: ["-c", "echo nope >&2; exit 3"]
    )
    #expect(result.terminationStatus == 3)
    #expect(String(decoding: result.standardError, as: UTF8.self) == "nope\n")
}

/// Each feature client owns the serial queue that keeps its requests in
/// order, so it must be one instance per `TendIPCClient`, not one per call.
@Test func featureClientsAreBuiltOncePerIPCClient() {
    let client = TendIPCClient(socketPath: "/tmp/tend-cache-test.sock")
    #expect(client.makeAutomationClient() === client.makeAutomationClient())
    #expect(client.makeConfigClient() === client.makeConfigClient())
    #expect(client.makeWorkComposeClient() === client.makeWorkComposeClient())
    #expect(client.makeAgentStartClient() === client.makeAgentStartClient())

    let repointed = TendIPCClient(socketPath: "/tmp/tend-cache-other.sock")
    #expect(repointed.makeConfigClient() !== client.makeConfigClient())
    #expect(repointed.makeConfigClient().socketPath == "/tmp/tend-cache-other.sock")
    #expect(repointed.makeAutomationClient().socketPath == "/tmp/tend-cache-other.sock")
}

/// A daemon whose first `list_sessions` blocks until released, and whose
/// every listing reports how many listings came before it.
private final class GatedRefreshProbe: @unchecked Sendable {
    private let lock = NSLock()
    private let gate = DispatchSemaphore(value: 0)
    private var listings = 0

    var listingCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return listings
    }

    func release() { gate.signal() }

    func request(path _: String, payload: Data) throws -> Data {
        let object = try JSONSerialization.jsonObject(with: payload) as? [String: Any]
        switch object?["kind"] as? String {
        case "hello":
            return try Self.json([
                "ok": true,
                "min_protocol": 1,
                "max_protocol": 6,
                "capabilities": [
                    "session_bytes_v1",
                    "session_attachment_identity_v1",
                    "work_control_v1",
                ],
            ])
        case "list_sessions":
            lock.lock()
            listings += 1
            let number = listings
            lock.unlock()
            if number == 1 { gate.wait() }
            return try Self.json([
                "ok": true,
                "sessions": [[
                    "id": "pty:listing-\(number)",
                    "backend": "pty",
                    "display_name": "Listing \(number)",
                    "cwd": "/tmp",
                    "attached_clients": 0,
                    "has_been_attached": false,
                    "exited": false,
                    "exit_status": NSNull(),
                    "pid": 1234,
                ]],
            ])
        default:
            return try Self.json(["ok": true])
        }
    }

    private static func json(_ object: [String: Any]) throws -> Data {
        try JSONSerialization.data(withJSONObject: object)
    }
}

@MainActor
@Test func refreshDuringAnotherRefreshWaitsForOneFreshFollowUp() async throws {
    let probe = GatedRefreshProbe()
    let model = AppModel(client: TendIPCClient(socketPath: "/tmp/tend-refresh-test.sock", request: probe.request))

    let first = Task { await model.refresh() }
    for _ in 0..<500 where probe.listingCount == 0 {
        try await Task.sleep(for: .milliseconds(10))
    }
    #expect(probe.listingCount == 1)

    // Two user actions refresh while the first listing is still in flight.
    let second = Task { await model.refresh() }
    let third = Task { await model.refresh() }
    try await Task.sleep(for: .milliseconds(100))
    probe.release()

    await second.value
    // The awaited refresh saw state read after it asked, not the stale run.
    #expect(model.sessions.map(\.id) == ["pty:listing-2"])
    await third.value
    await first.value
    #expect(probe.listingCount == 2, "queued refreshes coalesce into exactly one more")
}
