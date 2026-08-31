import Darwin
import Foundation

final class QAHelperServer: @unchecked Sendable {
    typealias Handler = @Sendable (QARequest) async -> QAResponse

    static let maximumRequestBytes = 256 * 1024

    let socketPath: String

    private let lock = NSLock()
    private var listener: Int32 = -1
    private var isRunning = false
    private var handler: Handler?

    init(uid: uid_t = getuid()) {
        socketPath = "/tmp/muxa-qa-helper-\(uid).sock"
    }

    func start(handler: @escaping Handler) throws {
        lock.lock()
        defer { lock.unlock() }
        guard !isRunning else { return }

        let descriptor = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard descriptor >= 0 else { throw POSIXError(.init(rawValue: errno) ?? .EIO) }

        var noSigPipe: Int32 = 1
        guard setsockopt(
            descriptor,
            SOL_SOCKET,
            SO_NOSIGPIPE,
            &noSigPipe,
            socklen_t(MemoryLayout.size(ofValue: noSigPipe))
        ) == 0 else {
            Darwin.close(descriptor)
            throw POSIXError(.init(rawValue: errno) ?? .EIO)
        }

        unlink(socketPath)
        var address = try Self.unixAddress(path: socketPath)
        let addressLength = socklen_t(
            MemoryLayout<sa_family_t>.size + socketPath.utf8CString.count
        )
        let bindResult = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.bind(descriptor, $0, addressLength)
            }
        }
        guard bindResult == 0 else {
            let failure = POSIXError(.init(rawValue: errno) ?? .EIO)
            Darwin.close(descriptor)
            throw failure
        }
        guard chmod(socketPath, S_IRUSR | S_IWUSR) == 0 else {
            let failure = POSIXError(.init(rawValue: errno) ?? .EIO)
            Darwin.close(descriptor)
            unlink(socketPath)
            throw failure
        }
        guard Darwin.listen(descriptor, 8) == 0 else {
            let failure = POSIXError(.init(rawValue: errno) ?? .EIO)
            Darwin.close(descriptor)
            unlink(socketPath)
            throw failure
        }

        self.handler = handler
        listener = descriptor
        isRunning = true

        Thread.detachNewThread { [weak self] in
            self?.acceptLoop(descriptor: descriptor)
        }
    }

    func stop() {
        lock.lock()
        let descriptor = listener
        listener = -1
        isRunning = false
        handler = nil
        lock.unlock()

        if descriptor >= 0 {
            Darwin.shutdown(descriptor, SHUT_RDWR)
            Darwin.close(descriptor)
        }
        unlink(socketPath)
    }

    private func acceptLoop(descriptor: Int32) {
        while true {
            let client = Darwin.accept(descriptor, nil, nil)
            if client < 0 {
                if errno == EINTR { continue }
                return
            }
            Thread.detachNewThread { [weak self] in
                self?.handle(client: client)
            }
        }
    }

    private func handle(client: Int32) {
        var peerUID: uid_t = 0
        var peerGID: gid_t = 0
        guard getpeereid(client, &peerUID, &peerGID) == 0, peerUID == getuid() else {
            Self.write(QAResponse.failure("client UID is not allowed"), to: client)
            Darwin.close(client)
            return
        }
        var noSigPipe: Int32 = 1
        _ = setsockopt(
            client,
            SOL_SOCKET,
            SO_NOSIGPIPE,
            &noSigPipe,
            socklen_t(MemoryLayout.size(ofValue: noSigPipe))
        )
        var receiveTimeout = timeval(tv_sec: 5, tv_usec: 0)
        _ = setsockopt(
            client,
            SOL_SOCKET,
            SO_RCVTIMEO,
            &receiveTimeout,
            socklen_t(MemoryLayout.size(ofValue: receiveTimeout))
        )
        var sendTimeout = timeval(tv_sec: 15, tv_usec: 0)
        _ = setsockopt(
            client,
            SOL_SOCKET,
            SO_SNDTIMEO,
            &sendTimeout,
            socklen_t(MemoryLayout.size(ofValue: sendTimeout))
        )

        guard let request = Self.readRequest(from: client) else {
            Self.write(QAResponse.failure("invalid or oversized request"), to: client)
            Darwin.close(client)
            return
        }
        lock.lock()
        let activeHandler = isRunning ? handler : nil
        lock.unlock()
        guard let activeHandler else {
            Self.write(QAResponse.failure("helper is shutting down"), to: client)
            Darwin.close(client)
            return
        }

        Task {
            let response = await activeHandler(request)
            Self.write(response, to: client)
            Darwin.close(client)
        }
    }

    private static func unixAddress(path: String) throws -> sockaddr_un {
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let bytes = path.utf8CString
        let capacity = MemoryLayout.size(ofValue: address.sun_path)
        guard bytes.count <= capacity else {
            throw CocoaError(.fileWriteInvalidFileName)
        }
        withUnsafeMutableBytes(of: &address.sun_path) { destination in
            destination.copyBytes(from: bytes.map { UInt8(bitPattern: $0) })
        }
        return address
    }

    private static func readRequest(from descriptor: Int32) -> QARequest? {
        var data = Data()
        var buffer = [UInt8](repeating: 0, count: 4096)
        while data.count <= maximumRequestBytes {
            let count = Darwin.recv(descriptor, &buffer, buffer.count, 0)
            if count <= 0 { return nil }
            if let newline = buffer[..<count].firstIndex(of: 0x0A) {
                guard data.count + newline <= maximumRequestBytes else { return nil }
                data.append(contentsOf: buffer[..<newline])
                return try? JSONDecoder().decode(QARequest.self, from: data)
            }
            guard data.count + count <= maximumRequestBytes else { return nil }
            data.append(contentsOf: buffer[..<count])
        }
        return nil
    }

    private static func write(_ response: QAResponse, to descriptor: Int32) {
        guard var data = try? JSONEncoder().encode(response) else { return }
        data.append(0x0A)
        data.withUnsafeBytes { rawBuffer in
            guard let base = rawBuffer.baseAddress else { return }
            var sent = 0
            while sent < rawBuffer.count {
                let result = Darwin.send(
                    descriptor,
                    base.advanced(by: sent),
                    rawBuffer.count - sent,
                    0
                )
                if result <= 0 { return }
                sent += result
            }
        }
    }

    deinit {
        stop()
    }
}
