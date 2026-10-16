import AppKit
import Darwin
import Foundation
import Testing
@testable import Muxa

private func artifactFixture() throws -> URL {
    let url = FileManager.default.temporaryDirectory.appendingPathComponent("muxa-files-test-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    return url
}

@Test func fileReaderListsFoldersFirstAndKeepsUnicodeNames() async throws {
    let root = try artifactFixture()
    defer { try? FileManager.default.removeItem(at: root) }
    try Data("hello".utf8).write(to: root.appendingPathComponent("산출물.txt"))
    try FileManager.default.createDirectory(at: root.appendingPathComponent("artifacts"), withIntermediateDirectories: false)
    let reader = MuxaFileReader(sshTarget: nil)
    let listing = try await reader.list(root.path)
    #expect(listing.entries.map(\.name) == ["artifacts", "산출물.txt"])
    #expect(listing.entries.first?.directory == true)
    #expect(!listing.truncated)
    let content = try await reader.read(root.appendingPathComponent("산출물.txt").path)
    #expect(content.text == "hello")
}

@Test func fileReaderRejectsLargeFilesAndNonRegularInputs() async throws {
    let root = try artifactFixture()
    defer { try? FileManager.default.removeItem(at: root) }
    let large = root.appendingPathComponent("large.bin")
    FileManager.default.createFile(atPath: large.path, contents: nil)
    let handle = try FileHandle(forWritingTo: large)
    try handle.truncate(atOffset: UInt64(MuxaFileReader.byteLimit + 1)); try handle.close()
    let reader = MuxaFileReader(sshTarget: nil)
    await #expect(throws: MuxaFileError.self) { try await reader.read(large.path) }
    await #expect(throws: MuxaFileError.self) { try await reader.read(root.path) }
    let fifo = root.appendingPathComponent("pipe")
    #expect(mkfifo(fifo.path, 0o600) == 0)
    await #expect(throws: MuxaFileError.self) { try await reader.read(fifo.path) }
    await #expect(throws: MuxaFileError.self) { try await reader.read(root.appendingPathComponent("missing").path) }
}

@Test func fileReaderBoundsDirectoryEntries() async throws {
    let root = try artifactFixture()
    defer { try? FileManager.default.removeItem(at: root) }
    for index in 0...MuxaFileReader.entryLimit {
        FileManager.default.createFile(atPath: root.appendingPathComponent("\(index)").path, contents: Data())
    }
    let listing = try await MuxaFileReader(sshTarget: nil).list(root.path)
    #expect(listing.entries.count == MuxaFileReader.entryLimit)
    #expect(listing.truncated)
}

@Test func remoteFileTransportDoesNotEvaluatePathsAsShellCode() async throws {
    let root = try artifactFixture()
    defer { try? FileManager.default.removeItem(at: root) }
    let name = "a' $(touch SHOULD_NOT_EXIST)\n한국어.txt"
    let file = root.appendingPathComponent(name)
    try Data("exact bytes".utf8).write(to: file)
    let command = try MuxaFileReader.remoteCommand(path: file.path, operation: "read")
    let result = await BoundedProcess.run(executable: URL(fileURLWithPath: "/bin/sh"), arguments: ["-c", command], limit: 4096, timeout: 10)
    #expect(result.status == 0, "\(result.stderr)")
    let json = try #require(JSONSerialization.jsonObject(with: result.stdout) as? [String: Any])
    let encoded = try #require(json["data"] as? String)
    #expect(Data(base64Encoded: encoded) == Data("exact bytes".utf8))
}

@Test func fileContentsDetectBinaryAndUTF16() {
    #expect(MuxaFileContents(data: Data([0, 1, 2]), modified: 0).text == nil)
    let data = "문서".data(using: .utf16)!
    #expect(MuxaFileContents(data: data, modified: 0).text == "문서")
}

@Test func markdownRendersArtifactsWithoutTreatingRawHTMLAsCode() {
    let html = ArtifactMarkdown.html("# Report\n\n<script>alert(1)</script>\n\n| Name | Result |\n| --- | --- |\n| test | **pass** |\n\n```swift\nlet x = \"<tag>\"\n```\n\n[Artifact](./report.pdf)")
    #expect(html.contains("<h1>Report</h1>"))
    #expect(html.contains("<table>"))
    #expect(html.contains("<strong>pass</strong>"))
    #expect(html.contains("&lt;script&gt;"))
    #expect(!html.contains("<script>"))
    #expect(html.contains("href=\"./report.pdf\""))
    #expect(html.contains("&lt;tag&gt;"))
}

@Test @MainActor func fileTabsReplacePreviewPinAndKeepHostIdentity() throws {
    let first = MuxaFileLocation(hostAlias: "local", path: "/work/report.md")
    let remote = MuxaFileLocation(hostAlias: "remote", path: first.path)
    let tabs = MuxaWorkbenchTabs(initial: nil, persistenceKey: nil)
    tabs.openPreview(.file(first)); tabs.openPreview(.file(remote))
    #expect(tabs.groups[0].tabs == [.file(remote)])
    tabs.openPinned(.file(remote)); tabs.openPreview(.file(first))
    #expect(tabs.groups[0].tabs.count == 2)
    let data = try JSONEncoder().encode(MuxaSidebarSelection.file(remote))
    #expect(try JSONDecoder().decode(MuxaSidebarSelection.self, from: data) == .file(remote))
    #expect(MuxaSidebarSelection.file(first).tabIdentifier != MuxaSidebarSelection.file(remote).tabIdentifier)
}

@Test @MainActor func workbenchCommandWClosesOnlyFocusedTabAndConsumesEmptyClose() throws {
    let tabs = MuxaWorkbenchTabs(persistenceKey: nil)
    tabs.openPinned(.ask)
    var actions = MuxaEditorCommandActions(close: { _ = tabs.closeFocused() })
    let event = try #require(NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: .command,
                                             timestamp: 0, windowNumber: 0, context: nil,
                                             characters: "w", charactersIgnoringModifiers: "w", isARepeat: false, keyCode: 13))
    #expect(MuxaWorkbenchKeymap.handle(event, actions: actions))
    #expect(tabs.focusedSelection == .workBoard)
    #expect(MuxaWorkbenchKeymap.handle(event, actions: actions))
    #expect(tabs.focusedSelection == nil)
    actions.close = nil
    #expect(MuxaWorkbenchKeymap.handle(event, actions: actions))
    let closeWindow = try #require(NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: [.command, .shift],
                                                   timestamp: 0, windowNumber: 0, context: nil,
                                                   characters: "W", charactersIgnoringModifiers: "W", isARepeat: false, keyCode: 13))
    #expect(!MuxaWorkbenchKeymap.handle(closeWindow, actions: actions))
}

@Test @MainActor func folderCompletionResolvesRelativeUnicodeAndHomePaths() {
    let relative = ArtifactFolderPicker.completionQuery("산출물/보", relativeTo: "/work")
    #expect(relative.directory == "/work/산출물")
    #expect(relative.prefix == "보")
    let home = ArtifactFolderPicker.completionQuery("~/Documents/", relativeTo: "/work")
    #expect(home.directory == "~/Documents/")
    #expect(home.prefix.isEmpty)
    let absolute = ArtifactFolderPicker.completionQuery("/tmp/ar", relativeTo: "/work")
    #expect(absolute.directory == "/tmp")
    #expect(absolute.prefix == "ar")
}

@Test func knownArtifactExtensionsSelectRenderedPreviews() {
    for ext in ["png", "JPG", "jpeg", "gif", "webp", "heic", "tiff", "bmp", "ico", "ICNS"] {
        #expect(ArtifactPreviewKind(path: "image." + ext) == .image)
    }
    #expect(ArtifactPreviewKind(path: "drawing.SVG") == .svg)
    for ext in ["md", "MD", "markdown", "mdown", "mkd", "mkdn"] {
        #expect(ArtifactPreviewKind(path: "report." + ext) == .markdown)
    }
    #expect(ArtifactPreviewKind(path: "report.pdf") == .pdf)
    #expect(ArtifactPreviewKind(path: "index.html") == .html)
    #expect(ArtifactPreviewKind(path: "code.swift") == .text)
}

@Test @MainActor func svgPreviewEmbedsSourceAsAnInertImage() {
    let source = Data("<svg><script>alert(1)</script></svg>".utf8)
    let html = ArtifactPreviewView.svgDocument(source)
    #expect(html.contains("data:image/svg+xml;base64," + source.base64EncodedString()))
    #expect(!html.contains("<script>"))
    #expect(!html.contains("<svg>"))
}

@Test func workspaceIndexFindsUnbrowsedFilesAndSkipsBuildTreesAndSymlinkLoops() async throws {
    let root = try artifactFixture()
    defer { try? FileManager.default.removeItem(at: root) }
    let nested = root.appendingPathComponent("artifacts/보고서")
    try FileManager.default.createDirectory(at: nested, withIntermediateDirectories: true)
    try Data("result".utf8).write(to: nested.appendingPathComponent("unvisited.md"))
    try FileManager.default.createDirectory(at: root.appendingPathComponent("node_modules"), withIntermediateDirectories: true)
    try Data().write(to: root.appendingPathComponent("node_modules/ignored.txt"))
    try FileManager.default.createSymbolicLink(at: nested.appendingPathComponent("loop"), withDestinationURL: root)
    let index = try await MuxaFileReader(sshTarget: nil).index(root.path)
    #expect(index.paths == [nested.appendingPathComponent("unvisited.md").standardizedFileURL.path])
    #expect(!index.truncated)
    let command = try MuxaFileReader.remoteCommand(path: root.path, operation: "index")
    let output = await BoundedProcess.run(executable: URL(fileURLWithPath: "/bin/sh"), arguments: ["-c", command], limit: 4096, timeout: 10)
    #expect(output.status == 0)
    let remote = try JSONDecoder().decode(MuxaFileIndex.self, from: output.stdout)
    #expect(remote.paths.map { ($0 as NSString).lastPathComponent } == ["unvisited.md"])
    #expect(!remote.truncated)
}

@Test func workspaceIndexRejectsMissingRootsAndCancellation() async throws {
    let root = try artifactFixture()
    defer { try? FileManager.default.removeItem(at: root) }
    await #expect(throws: MuxaFileError.self) { try await MuxaFileReader(sshTarget: nil).index(root.appendingPathComponent("missing").path) }
    for index in 0..<100 { try Data().write(to: root.appendingPathComponent("file-\(index)")) }
    let task = Task { try await MuxaFileReader(sshTarget: nil).index(root.path) }
    task.cancel()
    await #expect(throws: CancellationError.self) { try await task.value }
}

@Test @MainActor func explorerContainmentRespectsHostAndPathBoundaries() {
    let root = MuxaFileLocation(hostAlias: "local", path: "/work")
    #expect(ArtifactOutline.Coordinator.contains(root.appending("nested/file.md"), in: root))
    #expect(!ArtifactOutline.Coordinator.contains(.init(hostAlias: "local", path: "/workspace/file.md"), in: root))
    #expect(!ArtifactOutline.Coordinator.contains(.init(hostAlias: "other", path: "/work/file.md"), in: root))
}

@Test @MainActor func explorerRefreshRetainsNodesAndRecoversFromReadFailure() async throws {
    let root = try artifactFixture()
    defer { try? FileManager.default.removeItem(at: root) }
    let model = AppModel()
    let location = MuxaFileLocation(hostAlias: "local", path: root.path)
    let coordinator = ArtifactOutline.Coordinator(ArtifactOutline(model: model, root: location, showHidden: false, revision: 0, openFile: { _, _ in }))
    defer { coordinator.stop() }
    coordinator.reload()
    let node = try #require(coordinator.rootNode)
    await node.task?.value
    try Data().write(to: root.appendingPathComponent("new.md"))
    coordinator.load(node, refresh: true); await node.task?.value
    let file = try #require(node.children?.first { $0.title == "new.md" })
    coordinator.load(node, refresh: true); await node.task?.value
    #expect(node.children?.first { $0.title == "new.md" } === file)
    try FileManager.default.removeItem(at: root)
    coordinator.load(node, refresh: true); await node.task?.value
    #expect(node.failed)
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    try Data().write(to: root.appendingPathComponent("restored.md"))
    coordinator.load(node, refresh: true); await node.task?.value
    #expect(!node.failed)
    #expect(node.children?.first?.title == "restored.md")
}

@Test @MainActor func activeFileRevealExpandsAncestorsWithoutOpeningAnotherTab() async throws {
    let root = try artifactFixture().standardizedFileURL
    defer { try? FileManager.default.removeItem(at: root) }
    let folder = root.appendingPathComponent("one/two")
    try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
    let file = folder.appendingPathComponent("report.md")
    try Data().write(to: file)
    let location = MuxaFileLocation(hostAlias: "local", path: root.path)
    var opened = 0
    let coordinator = ArtifactOutline.Coordinator(ArtifactOutline(model: AppModel(), root: location, showHidden: false, revision: 0, openFile: { _, _ in opened += 1 }))
    let outline = NSOutlineView()
    outline.addTableColumn(NSTableColumn(identifier: .init("test")))
    outline.delegate = coordinator; outline.dataSource = coordinator; coordinator.outline = outline
    defer { coordinator.stop() }
    coordinator.reload()
    await coordinator.reveal(.init(hostAlias: "local", path: file.path))
    #expect(outline.selectedRow >= 0)
    #expect((outline.item(atRow: outline.selectedRow) as? ArtifactOutline.Node)?.title == "report.md")
    #expect(opened == 0)
}

@Test func fileMetadataTracksWritesWithoutReadingContents() async throws {
    let root = try artifactFixture()
    defer { try? FileManager.default.removeItem(at: root) }
    let file = root.appendingPathComponent("artifact.txt")
    try Data("one".utf8).write(to: file)
    let reader = MuxaFileReader(sshTarget: nil)
    let before = try await reader.stamp(file.path)
    try Data("updated result".utf8).write(to: file)
    let after = try await reader.stamp(file.path)
    #expect(before != after)
    let command = try MuxaFileReader.remoteCommand(path: file.path, operation: "stat")
    let output = await BoundedProcess.run(executable: URL(fileURLWithPath: "/bin/sh"), arguments: ["-c", command], limit: 4096, timeout: 10)
    let remote = try JSONDecoder().decode(MuxaFileStamp.self, from: output.stdout)
    #expect(remote.size == after.size)
}

@Test @MainActor func activatingFileTabsKeepsWorkspaceOrSwitchesHostRoot() {
    let model = AppModel()
    let root = MuxaFileLocation(hostAlias: "local", path: "/work")
    model.fileRoot = root
    model.activateEditor(.file(root.appending("nested/report.md")))
    #expect(model.fileRoot == root)
    let remote = MuxaFileLocation(hostAlias: "remote", path: "/work/other/report.md")
    model.activateEditor(.file(remote))
    #expect(model.fileRoot == remote.parent)
    #expect(model.sidebarMode == .files)
}
