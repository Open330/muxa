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
