import Darwin
import Foundation

/// A file belongs to a host, even when two hosts use the same absolute path.
struct MuxaFileLocation: Codable, Hashable, Sendable, Identifiable {
    var hostAlias: String
    var path: String
    var id: String { "\(hostAlias.utf8.count):\(hostAlias):\(path)" }
    var name: String { (path as NSString).lastPathComponent }
    var parent: Self { Self(hostAlias: hostAlias, path: (path as NSString).deletingLastPathComponent) }
    func contains(_ location: Self) -> Bool {
        guard hostAlias == location.hostAlias else { return false }
        return location.path.hasPrefix(path.hasSuffix("/") ? path : path + "/")
    }
    func appending(_ name: String) -> Self {
        Self(hostAlias: hostAlias, path: (path as NSString).appendingPathComponent(name))
    }
}

struct MuxaFileEntry: Decodable, Hashable, Sendable, Identifiable {
    let name: String
    let directory: Bool
    let size: Int64
    var id: String { name }
}

struct MuxaFileListing: Decodable, Sendable {
    let path: String
    let entries: [MuxaFileEntry]
    let truncated: Bool
}

struct MuxaFileStamp: Codable, Equatable, Sendable {
    let size: Int64
    let modified: Double
}

struct MuxaFileIndex: Decodable, Sendable {
    let paths: [String]
    let truncated: Bool
    let unreadable: Int
}

struct MuxaFileContents: Sendable {
    let data: Data
    let modified: Double
    var text: String? {
        if data.starts(with: [0xff, 0xfe]) || data.starts(with: [0xfe, 0xff]) {
            return String(data: data, encoding: .utf16)
        }
        guard !data.contains(0) else { return nil }
        return String(data: data, encoding: .utf8)
    }
}

enum MuxaFileError: LocalizedError, Equatable {
    case message(String)
    var errorDescription: String? { if case .message(let message) = self { message } else { nil } }
}

/// Bounded, read-only I/O. Remote paths are encoded as JSON, never interpolated
/// into shell syntax. SSH retains the user's host-key and authentication policy.
struct MuxaFileReader: Sendable {
    static let byteLimit = 16 * 1024 * 1024
    static let textLimit = 2 * 1024 * 1024
    static let entryLimit = 1000
    let sshTarget: String?

    func list(_ path: String) async throws -> MuxaFileListing {
        if sshTarget != nil {
            return try JSONDecoder().decode(MuxaFileListing.self, from: await remote(path, operation: "list"))
        }
        return try await Task.detached(priority: .userInitiated) {
            let path = (path as NSString).expandingTildeInPath
            let url = URL(fileURLWithPath: path).standardizedFileURL
            var isDirectory: ObjCBool = false
            guard FileManager.default.fileExists(atPath: url.path, isDirectory: &isDirectory), isDirectory.boolValue else {
                throw MuxaFileError.message(String(localized: "Folder not found."))
            }
            var failure: String?
            guard let iterator = FileManager.default.enumerator(
                at: url, includingPropertiesForKeys: [.isDirectoryKey, .fileSizeKey],
                options: [.skipsSubdirectoryDescendants], errorHandler: { _, error in
                    failure = error.localizedDescription
                    return false
                }
            ) else { throw MuxaFileError.message(String(localized: "Cannot read this folder.")) }
            var entries: [MuxaFileEntry] = []
            var truncated = false
            while let child = iterator.nextObject() as? URL {
                try Task.checkCancellation()
                if entries.count >= Self.entryLimit { truncated = true; break }
                let values = try child.resourceValues(forKeys: [.isDirectoryKey, .fileSizeKey])
                entries.append(MuxaFileEntry(name: child.lastPathComponent, directory: values.isDirectory == true, size: Int64(values.fileSize ?? 0)))
            }
            if let failure { throw MuxaFileError.message(failure) }
            return MuxaFileListing(path: url.path, entries: Self.sorted(entries), truncated: truncated)
        }.value
    }

    /// Index names only, never file contents. Avoid dependency/build trees and
    /// symlink traversal; bound both work and elapsed time on local and SSH hosts.
    static let indexExclusions: Set<String> = [".git", ".build", "node_modules", "target", "vendor", "__pycache__", ".venv"]
    func index(_ path: String) async throws -> MuxaFileIndex {
        if sshTarget != nil {
            return try JSONDecoder().decode(MuxaFileIndex.self, from: await remote(path, operation: "index"))
        }
        let task = Task.detached(priority: .utility) {
            let root = URL(fileURLWithPath: (path as NSString).expandingTildeInPath).standardizedFileURL
            var directory: ObjCBool = false
            guard FileManager.default.fileExists(atPath: root.path, isDirectory: &directory), directory.boolValue else {
                throw MuxaFileError.message(String(localized: "Folder not found."))
            }
            var unreadable = 0
            guard let iterator = FileManager.default.enumerator(at: root,
                includingPropertiesForKeys: [.isDirectoryKey, .isSymbolicLinkKey, .isRegularFileKey],
                options: [], errorHandler: { _, _ in unreadable += 1; return true }) else {
                throw MuxaFileError.message(String(localized: "Cannot read this folder."))
            }
            let deadline = Date().addingTimeInterval(8)
            var paths: [String] = [], visited = 0, truncated = false
            while let url = iterator.nextObject() as? URL {
                try Task.checkCancellation()
                visited += 1
                if visited > 20_000 || Date() >= deadline { truncated = true; break }
                guard let values = try? url.resourceValues(forKeys: [.isDirectoryKey, .isSymbolicLinkKey, .isRegularFileKey]) else { unreadable += 1; continue }
                if values.isDirectory == true {
                    if Self.indexExclusions.contains(url.lastPathComponent) || values.isSymbolicLink == true { iterator.skipDescendants() }
                } else if values.isRegularFile == true, values.isSymbolicLink != true {
                    // DirectoryEnumerator can canonicalize /var to /private/var.
                    // Keep results under the caller's root spelling for reveal.
                    let relative = url.pathComponents.suffix(iterator.level).joined(separator: "/")
                    paths.append(((path as NSString).expandingTildeInPath as NSString).appendingPathComponent(relative))
                }
            }
            return MuxaFileIndex(paths: paths.sorted(), truncated: truncated, unreadable: unreadable)
        }
        return try await withTaskCancellationHandler { try await task.value } onCancel: { task.cancel() }
    }

    func stamp(_ path: String) async throws -> MuxaFileStamp {
        if sshTarget != nil {
            return try JSONDecoder().decode(MuxaFileStamp.self, from: await remote(path, operation: "stat"))
        }
        return try await Task.detached(priority: .utility) {
            let attributes = try FileManager.default.attributesOfItem(atPath: (path as NSString).expandingTildeInPath)
            return MuxaFileStamp(size: (attributes[.size] as? NSNumber)?.int64Value ?? 0,
                                 modified: (attributes[.modificationDate] as? Date)?.timeIntervalSince1970 ?? 0)
        }.value
    }

    func read(_ path: String) async throws -> MuxaFileContents {
        if sshTarget != nil {
            struct Payload: Decodable { let data: String; let modified: Double }
            let payload = try JSONDecoder().decode(Payload.self, from: await remote(path, operation: "read"))
            guard let data = Data(base64Encoded: payload.data), data.count <= Self.byteLimit else {
                throw MuxaFileError.message(String(localized: "Invalid file response."))
            }
            return MuxaFileContents(data: data, modified: payload.modified)
        }
        return try await Task.detached(priority: .userInitiated) {
            let path = (path as NSString).expandingTildeInPath
            // O_NONBLOCK avoids hanging on FIFOs. Inspect the opened descriptor
            // rather than trusting a pre-open stat that can race with a writer.
            let descriptor = Darwin.open(path, O_RDONLY | O_NONBLOCK | O_CLOEXEC)
            guard descriptor >= 0 else { throw MuxaFileError.message(String(cString: strerror(errno))) }
            let handle = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
            defer { try? handle.close() }
            var info = stat()
            guard fstat(descriptor, &info) == 0, (info.st_mode & S_IFMT) == S_IFREG else {
                throw MuxaFileError.message(String(localized: "Only regular files can be previewed."))
            }
            guard info.st_size <= Self.byteLimit else { throw Self.tooLarge }
            let data = try handle.read(upToCount: Self.byteLimit + 1) ?? Data()
            guard data.count <= Self.byteLimit else { throw Self.tooLarge }
            return MuxaFileContents(data: data, modified: Double(info.st_mtimespec.tv_sec) + Double(info.st_mtimespec.tv_nsec) / 1e9)
        }.value
    }

    static var tooLarge: MuxaFileError { .message(String(localized: "This file exceeds the 16 MB preview limit. Open it in another app.")) }
    static func sorted(_ entries: [MuxaFileEntry]) -> [MuxaFileEntry] {
        entries.sorted {
            if $0.directory != $1.directory { return $0.directory }
            return $0.name.localizedStandardCompare($1.name) == .orderedAscending
        }
    }
    static func shellQuote(_ value: String) -> String { "'" + value.replacingOccurrences(of: "'", with: "'\\''") + "'" }
    static func remoteCommand(path: String, operation: String) throws -> String {
        let request = try JSONSerialization.data(withJSONObject: ["path": path, "operation": operation])
        return "python3 -I -c \(shellQuote(remoteScript)) \(shellQuote(request.base64EncodedString()))"
    }
    private func remote(_ path: String, operation: String) async throws -> Data {
        guard let sshTarget, !sshTarget.isEmpty, !sshTarget.hasPrefix("-"), !sshTarget.contains("\n") else {
            throw MuxaFileError.message(String(localized: "This host has no valid SSH target."))
        }
        let output = await BoundedProcess.run(
            executable: URL(fileURLWithPath: "/usr/bin/ssh"),
            arguments: ["-T", "-o", "BatchMode=yes", "-o", "ClearAllForwardings=yes", "-o", "ForwardAgent=no", "-o", "ConnectTimeout=8", "--", sshTarget, try Self.remoteCommand(path: path, operation: operation)],
            limit: Self.byteLimit * 2, timeout: 20
        )
        try Task.checkCancellation()
        guard !output.timedOut else { throw MuxaFileError.message(String(localized: "The SSH file request timed out.")) }
        guard output.status == 0, !output.truncated, output.launchError == nil else {
            throw MuxaFileError.message(output.launchError ?? (output.stderr.isEmpty ? String(localized: "Cannot read this host. SSH and Python 3 are required.") : String(output.stderr.prefix(1000))))
        }
        return output.stdout
    }

    // Python is used only as a read-only transport; it does not import or run
    // anything in the workspace. -c starts with a script, not a user path.
    static let remoteScript = #"""
import os, sys, json, base64, stat, time
try:
    request = json.loads(base64.b64decode(sys.argv[1]))
    path = os.path.abspath(os.path.expanduser(request['path']))
    limit = 16 * 1024 * 1024
    if request['operation'] == 'list':
        entries = []
        truncated = False
        with os.scandir(path) as iterator:
            for entry in iterator:
                if len(entries) >= 1000:
                    truncated = True
                    break
                try:
                    info = entry.stat()
                    entries.append(dict(name=entry.name, directory=stat.S_ISDIR(info.st_mode), size=info.st_size))
                except OSError:
                    entries.append(dict(name=entry.name, directory=False, size=0))
        entries.sort(key=lambda e: (not e['directory'], e['name'].lower()))
        print(json.dumps(dict(path=path, entries=entries, truncated=truncated)))
    elif request['operation'] == 'index':
        if not os.path.isdir(path):
            raise ValueError('Folder not found.')
        excluded = {'.git', '.build', 'node_modules', 'target', 'vendor', '__pycache__', '.venv'}
        paths, pending, visited, unreadable = [], [path], 0, 0
        deadline, truncated = time.monotonic() + 8, False
        while pending and not truncated:
            directory = pending.pop()
            try:
                with os.scandir(directory) as entries:
                    for entry in entries:
                        visited += 1
                        if visited > 20000 or time.monotonic() >= deadline:
                            truncated = True
                            break
                        try:
                            if entry.is_dir(follow_symlinks=False):
                                if entry.name not in excluded:
                                    pending.append(entry.path)
                            elif entry.is_file(follow_symlinks=False):
                                paths.append(entry.path)
                        except OSError:
                            unreadable += 1
            except OSError:
                unreadable += 1
        print(json.dumps(dict(paths=sorted(paths), truncated=truncated, unreadable=unreadable)))
    elif request['operation'] == 'stat':
        info = os.stat(path)
        print(json.dumps(dict(size=info.st_size, modified=info.st_mtime)))
    elif request['operation'] == 'read':
        fd = os.open(path, os.O_RDONLY | os.O_NONBLOCK)
        with os.fdopen(fd, 'rb') as f:
            info = os.fstat(f.fileno())
            if not stat.S_ISREG(info.st_mode):
                raise ValueError('Only regular files can be previewed.')
            if info.st_size > limit:
                raise ValueError('This file exceeds the 16 MB preview limit.')
            data = f.read(limit + 1)
            if len(data) > limit:
                raise ValueError('This file exceeds the 16 MB preview limit.')
        print(json.dumps(dict(data=base64.b64encode(data).decode('ascii'), modified=info.st_mtime)))
    else:
        raise ValueError('Unsupported file operation.')
except Exception as error:
    print(str(error), file=sys.stderr)
    sys.exit(1)
"""#
}

@MainActor
extension AppModel {
    func rememberBrowsedFiles(_ listing: MuxaFileListing, on host: String) {
        let files = listing.entries.filter { !$0.directory }.map { MuxaFileLocation(hostAlias: host, path: (listing.path as NSString).appendingPathComponent($0.name)) }
        let previous = browsedFiles.filter { !files.contains($0) }
        browsedFiles = Array((files + previous).prefix(3000))
    }

    func fileReader(for location: MuxaFileLocation) throws -> MuxaFileReader {
        if location.hostAlias == "local" { return MuxaFileReader(sshTarget: nil) }
        guard let host = fleetHosts.first(where: { $0.alias == location.hostAlias }), !host.local,
              let target = host.sshTarget, !target.isEmpty else {
            throw MuxaFileError.message(String(localized: "This Fleet host is no longer registered."))
        }
        return MuxaFileReader(sshTarget: target)
    }

    var activeFileDirectory: MuxaFileLocation? {
        switch sidebarSelection {
        case .file(let location): return location.parent
        case .pane(let id):
            guard let pane = executionSnapshot.watchPane(id: id) else { return nil }
            return MuxaFileLocation(hostAlias: pane.host.alias, path: pane.pane.currentPath)
        case .agent(let id):
            guard let agent = hostedAgents.first(where: { $0.id == id }), let path = agent.pane?.currentPath ?? agent.agent.cwd else { return nil }
            return MuxaFileLocation(hostAlias: agent.host.alias, path: path)
        case .work(let id):
            guard let work = workGroups.first(where: { $0.id == id }), let path = work.cwd, work.hostAliases.count <= 1 else { return nil }
            return MuxaFileLocation(hostAlias: work.hostAliases.first ?? "local", path: path)
        default: return nil
        }
    }
}
