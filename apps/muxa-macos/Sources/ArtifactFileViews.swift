import AppKit
import PDFKit
import Quartz
import SwiftUI
import WebKit
import ImageIO
import UniformTypeIdentifiers

struct ArtifactFileSidebar: View {
    @ObservedObject var model: AppModel
    let openFile: (MuxaFileLocation, Bool) -> Void
    @State private var showHidden = false
    @State private var showLocation = false
    @State private var revision = 0
    @AppStorage("muxa.files.lastLocalFolder") private var lastLocalFolder = ""
    private var root: MuxaFileLocation {
        model.fileRoot ?? MuxaFileLocation(hostAlias: "local", path: lastLocalFolder.isEmpty ? FileManager.default.homeDirectoryForCurrentUser.path : lastLocalFolder)
    }
    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Text("EXPLORER").font(.system(size: 10, weight: .semibold)).foregroundStyle(.secondary)
                Spacer()
                Menu {
                    Button("This Mac") { model.fileRoot = .init(hostAlias: "local", path: lastLocalFolder.isEmpty ? "~" : lastLocalFolder) }
                    ForEach(model.fleetHosts.filter { !$0.local }) { host in
                        Button(host.alias) { model.fileRoot = .init(hostAlias: host.alias, path: "~") }
                    }
                } label: { Text(root.hostAlias == "local" ? "This Mac" : root.hostAlias).font(.caption) }
                .menuStyle(.borderlessButton).fixedSize()
                Button { revision += 1 } label: { Image(systemName: "arrow.clockwise") }.help("Refresh files")
                Menu {
                    Button("Go to Folder…") { showLocation = true }
                    Button("Open Folder…", action: chooseFolder)
                    Button("Parent folder") { model.fileRoot = root.parent }.disabled(root.path == "/")
                    Button("Use Active Work Folder") { if let directory = model.activeFileDirectory { model.fileRoot = directory } }
                    Divider()
                    Toggle("Show Hidden Files", isOn: $showHidden)
                } label: { Image(systemName: "ellipsis") }.menuStyle(.borderlessButton).fixedSize()
            }.padding(.horizontal, 12).padding(.vertical, 9)
            Button { showLocation = true } label: {
                HStack(spacing: 6) {
                    Image(systemName: "folder.fill").foregroundStyle(Color.accentColor)
                    Text(root.name.isEmpty ? root.path : root.name).fontWeight(.semibold).lineLimit(1)
                    Spacer()
                    Image(systemName: "chevron.down").font(.system(size: 9)).foregroundStyle(.secondary)
                }.font(.system(size: 12)).padding(.horizontal, 12).padding(.vertical, 8).contentShape(Rectangle())
            }.buttonStyle(.plain).keyboardShortcut("g", modifiers: [.command, .shift]).help(root.path).accessibilityLabel("Go to Folder")
            .popover(isPresented: $showLocation, arrowEdge: .trailing) {
                ArtifactFolderPicker(model: model, root: root) { location in
                    model.fileRoot = location
                    if location.hostAlias == "local" { lastLocalFolder = location.path }
                    showLocation = false
                }.frame(width: 440)
            }
            Divider()
            ArtifactOutline(model: model, root: root, showHidden: showHidden, revision: revision, openFile: openFile)
        }.buttonStyle(.borderless)

    }
    private func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false; panel.canChooseDirectories = true
        guard panel.runModal() == .OK, let url = panel.url else { return }
        lastLocalFolder = url.path
        model.fileRoot = .init(hostAlias: "local", path: url.path)
    }
}

enum ArtifactPreviewKind: Equatable {
    case markdown, html, image, svg, pdf, text, quickLook
    init(path: String) {
        switch (path as NSString).pathExtension.lowercased() {
        case "md", "markdown", "mdown", "mkd", "mkdn": self = .markdown
        case "html", "htm": self = .html
        case "png", "jpg", "jpeg", "jpe", "jfif", "gif", "webp", "heic", "heif", "tif", "tiff", "bmp", "ico", "icns": self = .image
        case "svg": self = .svg
        case "pdf": self = .pdf
        case "docx", "xlsx", "pptx", "pages", "numbers", "key", "rtf": self = .quickLook
        default: self = .text
        }
    }
    var icon: String {
        switch self {
        case .image, .svg: "photo"
        case .pdf, .quickLook: "doc.richtext"
        case .markdown: "doc.text"
        case .html: "globe"
        case .text: "doc.plaintext"
        }
    }
}

extension Notification.Name { static let muxaFindInFile = Notification.Name("muxa.findInFile") }

struct ArtifactPreviewView: View {
    let location: MuxaFileLocation
    @ObservedObject var model: AppModel
    let isFocused: Bool
    let openFile: (MuxaFileLocation, Bool) -> Void
    @State private var contents: MuxaFileContents?
    @State private var cachedURL: URL?
    @State private var error: String?
    @State private var loading = true
    @State private var source = false
    @State private var revision = UUID()
    @State private var zoom: CGFloat = 1
    @State private var previewImage: NSImage?
    @State private var search = ""
    @State private var searchStep = 0
    @State private var showingFind = false
    @FocusState private var findFocused: Bool
    private var kind: ArtifactPreviewKind { ArtifactPreviewKind(path: location.path) }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                Image(systemName: location.hostAlias == "local" ? "doc" : "server.rack")
                Text("\(location.hostAlias) · \(location.path)")
                    .font(.system(size: 11)).lineLimit(1).truncationMode(.middle).textSelection(.enabled)
                Spacer(minLength: 0)
                if kind == .markdown || kind == .html || kind == .svg {
                    Picker("Display", selection: $source) {
                        Text("Preview").tag(false); Text("Source").tag(true)
                    }.pickerStyle(.segmented).labelsHidden().frame(width: 135)
                }
                Button { revision = UUID() } label: { Image(systemName: "arrow.clockwise") }.help("Refresh preview")
                Menu {
                    Button("Show in Files") { model.show(.files); model.fileRoot = location.parent }
                    Button("Copy Path") { NSPasteboard.general.clearContents(); NSPasteboard.general.setString(location.path, forType: .string) }
                    if location.hostAlias == "local" {
                        Button("Reveal in Finder") { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: location.path)]) }
                        Button("Open in Default App") { NSWorkspace.shared.open(URL(fileURLWithPath: location.path)) }
                    }
                } label: { Image(systemName: "ellipsis") }.menuStyle(.borderlessButton).fixedSize()
            }.padding(10).buttonStyle(.borderless)
            if showingFind {
                HStack {
                    TextField("Find in file", text: $search)
                        .textFieldStyle(.roundedBorder).focused($findFocused)
                        .onSubmit { searchStep += 1 }
                    Button { searchStep -= 1 } label: { Image(systemName: "chevron.up") }.help("Previous match")
                    Button { searchStep += 1 } label: { Image(systemName: "chevron.down") }.help("Next match")
                    Button { showingFind = false; search = "" } label: { Image(systemName: "xmark") }.help("Close find")
                }.padding(.horizontal, 10).padding(.bottom, 8).buttonStyle(.borderless)
            }
            Divider()
            if loading { Spacer(); ProgressView("Loading preview…"); Spacer() }
            else if let error {
                Spacer()
                Image(systemName: "doc.badge.ellipsis").font(.largeTitle).foregroundStyle(.secondary)
                Text("Preview unavailable").font(.headline).padding(.top, 8)
                Text(error).foregroundStyle(.secondary).textSelection(.enabled).padding().frame(maxWidth: 520)
                Button("Retry") { revision = UUID() }
                Spacer()
            } else if let contents {
                preview(contents)
                Divider()
                HStack {
                    Text("Read-only")
                    Spacer()
                    Text(ByteCountFormatter.string(fromByteCount: Int64(contents.data.count), countStyle: .file))
                    Text("Updated \(Date(timeIntervalSince1970: contents.modified).formatted(date: .omitted, time: .shortened))")
                }.font(.caption2).foregroundStyle(.secondary).padding(6)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .task(id: revision) { await load() }
        .task(id: location) {
            var previous = await fileStamp()
            while !Task.isCancelled {
                do { try await Task.sleep(for: .seconds(location.hostAlias == "local" ? 2 : 10)) } catch { return }
                let current = await fileStamp()
                if current != previous { previous = current; revision = UUID() }
            }
        }
        .onReceive(NotificationCenter.default.publisher(for: .muxaFindInFile)) { _ in
            guard isFocused else { return }
            showingFind = true
            findFocused = true
        }
        .onDisappear { removeCache() }
    }

    @ViewBuilder private func preview(_ contents: MuxaFileContents) -> some View {
        if kind == .image, let image = previewImage {
            VStack {
                HStack {
                    Button { zoom = max(0.25, zoom / 1.25) } label: { Image(systemName: "minus.magnifyingglass") }
                    Text("\(Int(zoom * 100))%")
                    Button { zoom = min(4, zoom * 1.25) } label: { Image(systemName: "plus.magnifyingglass") }
                    Button("Fit") { zoom = 1 }
                }.buttonStyle(.borderless).padding(8)
                GeometryReader { geometry in
                    ScrollView([.horizontal, .vertical]) {
                        Image(nsImage: image).resizable().scaledToFit()
                            .frame(width: max(1, geometry.size.width * zoom), height: max(1, geometry.size.height * zoom))
                    }
                }
            }
        } else if kind == .svg, !source {
            ArtifactWebPreview(html: Self.svgDocument(contents.data), baseURL: nil, reader: nil, search: "", step: 0, openLink: { _ in })
        } else if kind == .pdf {
            ArtifactPDFView(data: contents.data, search: search, step: searchStep)
        } else if kind == .quickLook, let cachedURL {
            ArtifactQuickLookView(url: cachedURL)
        } else if let text = contents.text, contents.data.count <= MuxaFileReader.textLimit {
            if !source && (kind == .markdown || kind == .html) {
                ArtifactWebPreview(html: kind == .markdown ? ArtifactMarkdown.html(text) : text,
                                   baseURL: previewBaseURL,
                                   reader: try? model.fileReader(for: location),
                                   search: search, step: searchStep, openLink: openLink)
            } else { ArtifactTextView(text: text, search: search, step: searchStep) }
        } else {
            VStack(spacing: 12) {
                Image(systemName: "doc").font(.largeTitle)
                Text(contents.data.count > MuxaFileReader.textLimit ? "Text preview is limited to 2 MB." : "This binary file has no inline preview.")
            }.foregroundStyle(.secondary).frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    /// An SVG loaded as an image is inert: no document scripts, links or external
    /// subresources. Base64 also prevents source text escaping the HTML wrapper.
    static func svgDocument(_ data: Data) -> String {
        """
        <!doctype html><html><head><meta name="viewport" content="width=device-width, initial-scale=1">
        <style>html,body{margin:0;width:100%;height:100%;color-scheme:light dark}body{display:flex;align-items:center;justify-content:center}img{max-width:100%;max-height:100%;object-fit:contain}</style>
        </head><body><img alt="SVG preview" src="data:image/svg+xml;base64,\(data.base64EncodedString())"></body></html>
        """
    }

    private func thumbnail(_ data: Data) -> NSImage? {
        guard let source = CGImageSourceCreateWithData(data as CFData, nil),
              let image = CGImageSourceCreateThumbnailAtIndex(source, 0, [
                kCGImageSourceCreateThumbnailFromImageAlways: true,
                kCGImageSourceThumbnailMaxPixelSize: 4096,
                kCGImageSourceCreateThumbnailWithTransform: true
              ] as CFDictionary) else { return nil }
        return NSImage(cgImage: image, size: .zero)
    }
    private func fileStamp() async -> MuxaFileStamp? {
        try? await model.fileReader(for: location).stamp(location.path)
    }
    private func load() async {
        loading = contents == nil; error = nil
        do {
            let value = try await model.fileReader(for: location).read(location.path)
            try Task.checkCancellation()
            removeCache()
            if kind == .quickLook {
                let directory = FileManager.default.temporaryDirectory.appendingPathComponent("muxa-preview-\(UUID().uuidString)")
                try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
                let url = directory.appendingPathComponent(location.name)
                try value.data.write(to: url, options: .atomic)
                cachedURL = url
            }
            if kind == .image {
                guard let image = thumbnail(value.data) else {
                    throw MuxaFileError.message(String(localized: "This image could not be decoded."))
                }
                previewImage = image
            }
            contents = value
        } catch is CancellationError { return }
        catch { self.error = error.localizedDescription; contents = nil }
        loading = false
    }
    private func removeCache() {
        if let cachedURL { try? FileManager.default.removeItem(at: cachedURL.deletingLastPathComponent()) }
        cachedURL = nil
    }
    private var previewBaseURL: URL? {
        var components = URLComponents()
        components.scheme = "muxa-artifact"; components.host = "workspace"
        components.path = location.parent.path + "/"
        return components.url
    }
    private func openLink(_ url: URL) {
        if url.isFileURL || url.scheme == "muxa-artifact" {
            openFile(MuxaFileLocation(hostAlias: location.hostAlias, path: url.path), false)
        } else if ["https", "http", "mailto"].contains(url.scheme?.lowercased() ?? "") {
            NSWorkspace.shared.open(url)
        }
    }
}

private struct ArtifactPDFView: NSViewRepresentable {
    let data: Data
    let search: String
    let step: Int
    final class Coordinator { var data: Data?; var search = ""; var step = 0; var matches: [PDFSelection] = [] }

    func makeCoordinator() -> Coordinator { Coordinator() }
    func makeNSView(context: Context) -> PDFView {
        let view = PDFView(); view.autoScales = true; view.displayMode = .singlePageContinuous
        view.document = PDFDocument(data: data)
        context.coordinator.data = data
        return view
    }
    func updateNSView(_ view: PDFView, context: Context) {
        let documentChanged = context.coordinator.data != data
        if documentChanged {
            context.coordinator.data = data
            view.document = PDFDocument(data: data)
        }
        if documentChanged || context.coordinator.search != search {
            context.coordinator.search = search
            context.coordinator.matches = search.isEmpty ? [] : (view.document?.findString(search, withOptions: .caseInsensitive) ?? [])
            context.coordinator.step = step
        }
        let matches = context.coordinator.matches
        if !matches.isEmpty {
            let index = ((step % matches.count) + matches.count) % matches.count
            view.setCurrentSelection(matches[index], animate: false)
            view.go(to: matches[index])
        } else { view.clearSelection() }
    }
}

private struct ArtifactQuickLookView: NSViewRepresentable {
    let url: URL
    func makeNSView(context: Context) -> QLPreviewView {
        let view = QLPreviewView(frame: .zero, style: .normal)!
        view.autostarts = true; view.previewItem = url as NSURL
        return view
    }
    func updateNSView(_ view: QLPreviewView, context: Context) { view.previewItem = url as NSURL }
}

private struct ArtifactTextView: NSViewRepresentable {
    let text: String
    let search: String
    let step: Int
    final class Coordinator { var search = ""; var step = 0 }
    func makeCoordinator() -> Coordinator { Coordinator() }
    func makeNSView(context: Context) -> NSScrollView {
        let scroll = NSTextView.scrollableTextView()
        scroll.hasVerticalScroller = true; scroll.hasHorizontalScroller = false
        let view = scroll.documentView as! NSTextView
        view.isEditable = false; view.isSelectable = true
        view.isRichText = false; view.usesFindBar = true; view.isIncrementalSearchingEnabled = true
        view.font = .monospacedSystemFont(ofSize: 12, weight: .regular)
        view.textColor = .textColor; view.backgroundColor = .textBackgroundColor
        view.textContainerInset = NSSize(width: 14, height: 12)
        view.autoresizingMask = [.width]
        view.isHorizontallyResizable = false
        view.isVerticallyResizable = true
        view.textContainer?.widthTracksTextView = true
        view.string = text
        return scroll
    }
    func updateNSView(_ scroll: NSScrollView, context: Context) {
        guard let view = scroll.documentView as? NSTextView else { return }
        if view.string != text { view.string = text }
        let changed = context.coordinator.search != search
        guard changed || context.coordinator.step != step else { return }
        let backwards = !changed && step < context.coordinator.step
        context.coordinator.search = search; context.coordinator.step = step
        guard !search.isEmpty else { return }
        let value = text as NSString
        let selected = view.selectedRange()
        let start = changed ? 0 : min(value.length, selected.location + selected.length)
        let range = backwards ? NSRange(location: 0, length: min(selected.location, value.length)) : NSRange(location: start, length: value.length - start)
        let options: NSString.CompareOptions = backwards ? [.caseInsensitive, .backwards] : [.caseInsensitive]
        var found = value.range(of: search, options: options, range: range)
        if found.location == NSNotFound { found = value.range(of: search, options: options) }
        if found.location != NSNotFound { view.setSelectedRange(found); view.scrollRangeToVisible(found) }
    }
}

private struct ArtifactWebPreview: NSViewRepresentable {
    let html: String
    let baseURL: URL?
    let reader: MuxaFileReader?
    let search: String
    let step: Int
    let openLink: (URL) -> Void
    func makeCoordinator() -> Coordinator { Coordinator(openLink: openLink) }
    func makeNSView(context: Context) -> WKWebView {
        let configuration = WKWebViewConfiguration()
        configuration.websiteDataStore = .nonPersistent()
        configuration.defaultWebpagePreferences.allowsContentJavaScript = false
        if let reader, let baseURL {
            configuration.setURLSchemeHandler(ArtifactResourceHandler(reader: reader, root: baseURL.path), forURLScheme: "muxa-artifact")
        }
        let view = WKWebView(frame: .zero, configuration: configuration)
        view.navigationDelegate = context.coordinator
        view.setValue(false, forKey: "drawsBackground")
        return view
    }
    func updateNSView(_ view: WKWebView, context: Context) {
        context.coordinator.openLink = openLink
        if context.coordinator.search != search || context.coordinator.step != step {
            let config = WKFindConfiguration()
            config.backwards = step < context.coordinator.step
            config.wraps = true
            context.coordinator.search = search; context.coordinator.step = step
            view.find(search, configuration: config) { _ in }
        }
        guard context.coordinator.html != html else { return }
        context.coordinator.html = html
        // No workspace scripts, frames, forms or outbound resource requests.
        let policy = "<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src data: muxa-artifact:; style-src 'unsafe-inline'; font-src data:; form-action 'none'; base-uri 'none'\">"
        context.coordinator.expectsInitialNavigation = true
        let document = html.replacingOccurrences(of: "href=\"file://", with: "href=\"muxa-artifact://workspace")
        view.loadHTMLString(policy + document, baseURL: baseURL)
    }
    final class Coordinator: NSObject, WKNavigationDelegate {
        var html: String?
        var expectsInitialNavigation = false
        var search = ""
        var step = 0
        var openLink: (URL) -> Void
        init(openLink: @escaping (URL) -> Void) { self.openLink = openLink }
        func webView(_ webView: WKWebView, decidePolicyFor action: WKNavigationAction, decisionHandler: @escaping @MainActor (WKNavigationActionPolicy) -> Void) {
            if action.navigationType == .linkActivated, let url = action.request.url {
                if url.fragment != nil, url.absoluteString.components(separatedBy: "#").first == webView.url?.absoluteString.components(separatedBy: "#").first {
                    decisionHandler(.allow); return
                }
                openLink(url); decisionHandler(.cancel)
            } else if action.navigationType == .other && expectsInitialNavigation {
                expectsInitialNavigation = false
                decisionHandler(.allow)
            } else { decisionHandler(.cancel) }
        }
    }
}

/// Images referenced by a document use the same bounded reader and host as the
/// document. A remote Markdown image must never read a similarly named local file.
@MainActor
private final class ArtifactResourceHandler: NSObject, WKURLSchemeHandler {
    let reader: MuxaFileReader
    let root: String
    private var pending: [ObjectIdentifier: Task<Void, Never>] = [:]
    init(reader: MuxaFileReader, root: String) {
        self.reader = reader
        self.root = (root as NSString).standardizingPath
    }
    func webView(_ webView: WKWebView, start task: any WKURLSchemeTask) {
        let id = ObjectIdentifier(task)
        guard let url = task.request.url, url.host == "workspace", pending.count < 16 else {
            task.didFailWithError(URLError(.unsupportedURL)); return
        }
        let path = (url.path as NSString).standardizingPath
        let resolved = reader.sshTarget == nil ? URL(fileURLWithPath: path).resolvingSymlinksInPath().path : path
        let root = reader.sshTarget == nil ? URL(fileURLWithPath: root).resolvingSymlinksInPath().path : root
        guard resolved.hasPrefix(root.hasSuffix("/") ? root : root + "/") else {
            task.didFailWithError(URLError(.noPermissionsToReadFile)); return
        }
        pending[id] = Task {
            defer { pending[id] = nil }
            do {
                let contents = try await reader.read(path)
                try Task.checkCancellation()
                let mime = UTType(filenameExtension: url.pathExtension)?.preferredMIMEType ?? "application/octet-stream"
                task.didReceive(URLResponse(url: url, mimeType: mime, expectedContentLength: contents.data.count, textEncodingName: nil))
                task.didReceive(contents.data)
                task.didFinish()
            } catch is CancellationError {} catch { task.didFailWithError(error) }
        }
    }
    func webView(_ webView: WKWebView, stop task: any WKURLSchemeTask) {
        pending.removeValue(forKey: ObjectIdentifier(task))?.cancel()
    }
}
