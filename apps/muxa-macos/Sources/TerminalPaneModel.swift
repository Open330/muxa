import Foundation
import GhosttyTerminal

@MainActor
final class TerminalPaneModel: ObservableObject {
    let terminalState = TerminalViewState()

    @Published private(set) var errorMessage: String?
    @Published private(set) var outputWasTruncated = false
    @Published private(set) var exited = false
    @Published private(set) var exitStatus: Int32?
    @Published private(set) var rawOutputText = "Waiting for PTY output…"
    @Published private(set) var rawOutputByteCount = 0

    private let client: MuxaIPCClient
    private let sessionID: String
    private let attachmentClientID = "muxa-macos-view:\(UUID().uuidString)"
    private let ioPump: TerminalSessionIOPump
    private let terminalSession: InMemoryTerminalSession
    private var shouldReplayInitialHistory: Bool
    private var pollingTask: Task<Void, Never>?
    private var detachTask: Task<Void, Never>?
    private var lifecycleGeneration: UInt64 = 0
    private var attachedGeneration: UInt64?
    private var rawOutput = Data()
    private var rawDisplayEnabled = false
    private var lastRawPublish = Date.distantPast

    private static let maximumRawOutputBytes = 256 * 1024

    init(client: MuxaIPCClient, sessionID: String, replayInitialHistory: Bool) {
        self.client = client
        self.sessionID = sessionID
        shouldReplayInitialHistory = replayInitialHistory
        let ioPump = TerminalSessionIOPump(client: client, sessionID: sessionID)
        self.ioPump = ioPump
        terminalSession = InMemoryTerminalSession(
            write: { [ioPump] data in
                Task { await ioPump.enqueueInput(data) }
            },
            resize: { [ioPump] viewport in
                Task {
                    await ioPump.enqueueResize(
                        columns: viewport.columns,
                        rows: viewport.rows
                    )
                }
            },
            suppressesPixelOnlyResizes: true
        )
        terminalState.configuration = TerminalSurfaceOptions(
            backend: .inMemory(terminalSession),
            context: .window
        )
    }

    func start() {
        guard pollingTask == nil else { return }
        lifecycleGeneration &+= 1
        let generation = lifecycleGeneration
        let pendingDetach = detachTask
        pollingTask = Task { [weak self] in
            await pendingDetach?.value
            guard !Task.isCancelled, let self else { return }
            await ioPump.setActive(true)
            await poll(generation: generation)
        }
        terminalState.requestFocus()
    }

    func focus() {
        terminalState.requestFocus()
    }

    func stop() {
        guard let task = pollingTask else { return }
        let generation = lifecycleGeneration
        lifecycleGeneration &+= 1
        task.cancel()
        self.pollingTask = nil
        let previousDetach = detachTask
        detachTask = Task { [client, ioPump, sessionID] in
            await ioPump.setActive(false)
            await previousDetach?.value
            await task.value
            guard attachedGeneration == generation else { return }
            attachedGeneration = nil
            do {
                try await client.setAttached(
                    id: sessionID,
                    clientID: attachmentClientID,
                    attached: false
                )
            } catch {
                MuxaLog.terminal.error(
                    "terminal detach failed: \(error.localizedDescription, privacy: .public)"
                )
            }
        }
    }

    func setRawDisplayEnabled(_ enabled: Bool) {
        rawDisplayEnabled = enabled
        if enabled {
            publishRawOutput()
        } else {
            terminalState.requestFocus()
        }
    }

    private func poll(generation: UInt64) async {
        do {
            try Task.checkCancellation()
            try await client.setAttached(
                id: sessionID,
                clientID: attachmentClientID,
                attached: true
            )
            attachedGeneration = generation
        } catch {
            guard !(error is CancellationError) else { return }
            report(error)
            return
        }

        var offset: UInt64 = 0
        while !Task.isCancelled, lifecycleGeneration == generation {
            do {
                let output = try await client.readSession(id: sessionID, offset: offset)
                guard let bytes = output.bytes else {
                    throw MuxaIPCError.invalidBase64
                }
                guard output.sessionID == sessionID else {
                    throw MuxaIPCError.server("muxad returned output for a different session")
                }
                guard output.nextOffset >= output.offset, output.nextOffset >= offset else {
                    throw MuxaIPCError.server("muxad returned a regressing terminal offset")
                }
                if output.truncated { outputWasTruncated = true }
                if !bytes.isEmpty {
                    appendRawOutput(bytes)
                    if shouldReplayInitialHistory {
                        terminalSession.replay(bytes)
                    } else {
                        terminalSession.receive(bytes)
                    }
                    shouldReplayInitialHistory = false
                }
                // A truncated read may legitimately contain no bytes while
                // advancing to the retained buffer's base. Always accept the
                // server cursor or the client will request offset zero forever.
                offset = output.nextOffset
                errorMessage = nil
                if output.exited {
                    exited = true
                    exitStatus = output.exitStatus
                    // Ghostty does not own this PTY. Reporting a host process
                    // exit through ghostty_surface_process_exit asks Ghostty
                    // to render its own command-error treatment, even for a
                    // normal `exit 0`. Keep the final grid intact and let the
                    // native host present the terminal state instead.
                    await ioPump.setActive(false)
                    if rawDisplayEnabled { publishRawOutput() }
                    break
                }
                try await Task.sleep(for: bytes.isEmpty ? .milliseconds(45) : .milliseconds(8))
            } catch is CancellationError {
                break
            } catch {
                report(error)
                try? await Task.sleep(for: .milliseconds(500))
            }
        }
    }

    private func report(_ error: Error) {
        let message = error.localizedDescription
        if errorMessage != message {
            MuxaLog.terminal.error("terminal polling failed: \(message, privacy: .public)")
        }
        errorMessage = message
    }

    private func appendRawOutput(_ bytes: Data) {
        rawOutput.append(bytes)
        if rawOutput.count > Self.maximumRawOutputBytes {
            rawOutput.removeFirst(rawOutput.count - Self.maximumRawOutputBytes)
        }
        guard rawDisplayEnabled else { return }
        let now = Date()
        if now.timeIntervalSince(lastRawPublish) >= 0.12 {
            publishRawOutput(now: now)
        }
    }

    private func publishRawOutput(now: Date = Date()) {
        rawOutputText = rawOutput.isEmpty
            ? "Waiting for PTY output…"
            : terminalRawDescription(rawOutput)
        rawOutputByteCount = rawOutput.count
        lastRawPublish = now
    }
}
