import Foundation

/// Losslessly batches terminal input and keeps only the newest pending resize.
/// This prevents a slow/restarting daemon from turning key-repeat or a live
/// window drag into an unbounded number of suspended IPC tasks.
actor TerminalSessionIOPump {
    private static let maximumInputChunkBytes = 64 * 1024

    private let client: MuxaIPCClient
    private let sessionID: String

    private var active = false
    private var draining = false
    private var pendingInput = Data()
    private var pendingResize: (columns: UInt16, rows: UInt16)?

    init(client: MuxaIPCClient, sessionID: String) {
        self.client = client
        self.sessionID = sessionID
    }

    func setActive(_ active: Bool) {
        self.active = active
        if active {
            startDrainIfNeeded()
        } else {
            pendingInput.removeAll(keepingCapacity: false)
        }
    }

    func enqueueInput(_ data: Data) {
        guard active, !data.isEmpty else { return }
        pendingInput.append(data)
        startDrainIfNeeded()
    }

    func enqueueResize(columns: UInt16, rows: UInt16) {
        // Ghostty commonly reports the initial viewport while the SwiftUI
        // surface is mounting, just before polling marks this pump active.
        // Preserve that newest size so a tmux attach does not remain at the
        // spawn fallback of 80x24 until the user manually resizes the window.
        pendingResize = (columns, rows)
        guard active else { return }
        startDrainIfNeeded()
    }

    private func startDrainIfNeeded() {
        guard active, !draining, (!pendingInput.isEmpty || pendingResize != nil) else {
            return
        }
        draining = true
        Task { [weak self] in
            await self?.drain()
        }
    }

    private func drain() async {
        while active {
            if !pendingInput.isEmpty {
                let count = min(pendingInput.count, Self.maximumInputChunkBytes)
                let data = Data(pendingInput.prefix(count))
                pendingInput.removeFirst(count)
                do {
                    try await client.writeSession(id: sessionID, bytes: data)
                } catch {
                    MuxaLog.terminal.error(
                        "terminal input failed: \(error.localizedDescription, privacy: .public)"
                    )
                }
                continue
            }

            if let resize = pendingResize {
                pendingResize = nil
                do {
                    try await client.resizeSession(
                        id: sessionID,
                        columns: resize.columns,
                        rows: resize.rows
                    )
                } catch {
                    MuxaLog.terminal.error(
                        "terminal resize failed: \(error.localizedDescription, privacy: .public)"
                    )
                }
                continue
            }

            draining = false
            return
        }

        draining = false
        startDrainIfNeeded()
    }
}
