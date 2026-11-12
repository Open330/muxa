import Darwin
import Foundation

/// What a finished child process left behind.
struct TendCLIProcessResult: Sendable {
    let terminationStatus: Int32
    let standardOutput: Data
    let standardError: Data
}

/// Runs a short-lived child (the bundled `tend` CLI) to completion.
///
/// A pipe holds about 64 KB. Waiting for exit before reading, or reading
/// stdout to EOF before stderr, stalls as soon as the child fills the pipe
/// nobody is draining: the child blocks on write and never exits. Both
/// streams are therefore drained on their own queues while stdin is written
/// from a third, and only then is the exit awaited.
enum TendCLIProcess {
    /// Blocks the calling thread; call it from a detached task.
    static func run(
        executable: URL,
        arguments: [String],
        environment: [String: String]? = nil,
        input: Data? = nil
    ) throws -> TendCLIProcessResult {
        let process = Process()
        process.executableURL = executable
        process.arguments = arguments
        if let environment { process.environment = environment }
        let output = Pipe()
        let errors = Pipe()
        process.standardOutput = output
        process.standardError = errors
        let stdin = input.map { _ in Pipe() }
        process.standardInput = stdin ?? FileHandle.nullDevice
        try process.run()

        let group = DispatchGroup()
        let collected = CollectedOutput()
        let queue = DispatchQueue.global(qos: .userInitiated)
        queue.async(group: group) {
            collected.setOutput(output.fileHandleForReading.readDataToEndOfFile())
        }
        queue.async(group: group) {
            collected.setError(errors.fileHandleForReading.readDataToEndOfFile())
        }
        if let stdin, let input {
            queue.async(group: group) {
                let writer = stdin.fileHandleForWriting
                // A child that exits without reading must surface as a
                // failed write, not a SIGPIPE that kills the app.
                _ = fcntl(writer.fileDescriptor, F_SETNOSIGPIPE, 1)
                try? writer.write(contentsOf: input)
                try? writer.close()
            }
        }
        group.wait()
        process.waitUntilExit()
        return TendCLIProcessResult(
            terminationStatus: process.terminationStatus,
            standardOutput: collected.output,
            standardError: collected.error
        )
    }
}

private final class CollectedOutput: @unchecked Sendable {
    private let lock = NSLock()
    private var storedOutput = Data()
    private var storedError = Data()

    var output: Data {
        lock.lock()
        defer { lock.unlock() }
        return storedOutput
    }

    var error: Data {
        lock.lock()
        defer { lock.unlock() }
        return storedError
    }

    func setOutput(_ data: Data) {
        lock.lock()
        storedOutput = data
        lock.unlock()
    }

    func setError(_ data: Data) {
        lock.lock()
        storedError = data
        lock.unlock()
    }
}


/// Reads a pipe to EOF on a background queue, so a caller draining another
/// pipe of the same child cannot stall on this one filling up.
final class DrainedPipe: @unchecked Sendable {
    private let done = DispatchSemaphore(value: 0)
    // Written once on the reader queue before `done` is signalled, read only
    // after `wait()` returns; the semaphore orders the two.
    private var data = Data()

    init(_ handle: FileHandle) {
        DispatchQueue.global(qos: .utility).async {
            self.data = handle.readDataToEndOfFile()
            self.done.signal()
        }
    }

    /// Blocks until the pipe reaches EOF and returns everything read.
    func wait() -> Data {
        done.wait()
        return data
    }
}
