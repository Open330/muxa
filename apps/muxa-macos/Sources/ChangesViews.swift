import AppKit
import SwiftUI

// MARK: - Theme

extension MuxaTheme {
    // The diff editor's tones, after VS Code's: translucent green and red
    // under primary text, so contrast holds in both appearances.

    static func diffAddedBackground(_ scheme: ColorScheme) -> Color {
        scheme == .dark ? diffColor(0x2EA043, alpha: 0.15) : diffColor(0xDAFBE1)
    }

    static func diffRemovedBackground(_ scheme: ColorScheme) -> Color {
        scheme == .dark ? diffColor(0xF85149, alpha: 0.15) : diffColor(0xFFEBE9)
    }

    static func diffAddedGutter(_ scheme: ColorScheme) -> Color {
        scheme == .dark ? diffColor(0x2EA043, alpha: 0.3) : diffColor(0xACEEBB)
    }

    static func diffRemovedGutter(_ scheme: ColorScheme) -> Color {
        scheme == .dark ? diffColor(0xF85149, alpha: 0.3) : diffColor(0xFFCECB)
    }

    static func diffHunkBackground(_ scheme: ColorScheme) -> Color {
        diffColor(0x1F6FEB, alpha: scheme == .dark ? 0.15 : 0.08)
    }

    static func diffCommentedBackground(_ scheme: ColorScheme) -> Color {
        diffColor(0xD29922, alpha: scheme == .dark ? 0.16 : 0.12)
    }

    private static func diffColor(_ hex: UInt32, alpha: Double = 1) -> Color {
        Color(
            .sRGB,
            red: Double((hex >> 16) & 0xFF) / 255,
            green: Double((hex >> 8) & 0xFF) / 255,
            blue: Double(hex & 0xFF) / 255,
            opacity: alpha
        )
    }
}

// MARK: - Workbench hooks

extension Notification.Name {
    /// ⌃⇧G: switch the editor for the posted `MuxaSidebarSelection` to its
    /// Changes module.
    static let muxaShowChanges = Notification.Name("dev.muxa.showChanges")
}

enum ChangesShowRequest {
    static func matches(_ object: Any?, pane: MuxaWatchPane, watchSelection: MuxaWatchPaneIdentity?) -> Bool {
        switch object as? MuxaSidebarSelection {
        case .pane(let id): id == pane.id
        case .watch: watchSelection == pane.id
        default: false
        }
    }

    static func matches(_ object: Any?, work: MuxaWorkGroup) -> Bool {
        if case .work(let id) = object as? MuxaSidebarSelection { return id == work.identity }
        return false
    }
}

extension ChangesSource {
    init(pane: MuxaWatchPane) {
        let path = pane.pane.currentPath.isEmpty ? pane.agent?.cwd : pane.pane.currentPath
        self.init(hostAlias: pane.host.alias, isLocal: pane.host.local, path: path)
    }

    /// A Work is readable here when one of its participants runs on this Mac
    /// (or it has none bound yet and only the pipeline's cwd is known).
    init(work: MuxaWorkGroup) {
        let local = work.participants.first { $0.host.local }
        self.init(
            hostAlias: local?.host.alias ?? work.participants.first?.host.alias ?? "local",
            isLocal: work.participants.isEmpty || local != nil,
            path: work.cwd
        )
    }
}

/// The Changes tab title, with the pending comment count once there is one.
enum ChangesTab {
    static func title(count: Int) -> LocalizedStringKey {
        count > 0 ? "Changes · \(count)" : "Changes"
    }

    @MainActor
    static func title(for pane: MuxaWatchPane, drafts: ReviewDraftStore) -> LocalizedStringKey {
        let source = ChangesSource(pane: pane)
        return title(count: drafts.commentCount(hostAlias: source.hostAlias, path: source.path))
    }
}

enum WorkDetailTab: CaseIterable, Identifiable {
    case overview
    case changes

    var id: Self { self }
}

/// The Work page's Overview / Changes switch, in the breadcrumb row's place.
struct WorkDetailTabBar: View {
    let work: MuxaWorkGroup
    @Binding var tab: WorkDetailTab
    @ObservedObject private var drafts = ReviewDraftStore.shared
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        HStack(spacing: 8) {
            MuxaTextTabs(items: WorkDetailTab.allCases, selection: $tab) { tab in
                switch tab {
                case .overview: "Overview"
                case .changes:
                    ChangesTab.title(count: {
                        let source = ChangesSource(work: work)
                        return drafts.commentCount(hostAlias: source.hostAlias, path: source.path)
                    }())
                }
            }
            Spacer()
        }
        .padding(.horizontal, 12)
        .frame(height: MuxaTheme.breadcrumbHeight)
        .background(MuxaSurfacePalette.workspace(for: colorScheme))
        .overlay(alignment: .bottom) {
            MuxaTheme.border(colorScheme).frame(height: 1)
        }
    }
}

// MARK: - Review target

enum ReviewTarget {
    case pane(MuxaWatchPane)
    case work(MuxaWorkGroup)
}

enum ReviewRecipient: Hashable {
    case all
    case participant(String)
}

enum ReviewSendRules {
    static func canSend(draft: ReviewDraft, previewEdited: Bool, preview: String, sending: Bool, hosts: [MuxaFleetHostIdentity]) -> Bool {
        (!draft.isEmpty || previewEdited)
            && preview.utf8.count <= ReviewSendLimits.maxBytes
            && PromptComposerRules.canSend(prompt: preview, sending: sending, hosts: hosts)
    }
}

// MARK: - Changes workspace

/// The Changes module: the repository at an agent's (or a Work's) working
/// directory, its changed files, a diff for the selected one, and a review
/// draft that goes back to the agent as one prompt.
struct ChangesWorkspaceView: View {
    let target: ReviewTarget
    let agentState: String?
    @ObservedObject var model: AppModel
    @StateObject private var changes: ChangesModel
    @ObservedObject private var drafts = ReviewDraftStore.shared
    @State private var showingReview = false
    @State private var sentNotice = false
    @Environment(\.colorScheme) private var colorScheme

    init(pane: MuxaWatchPane, model: AppModel) {
        target = .pane(pane)
        agentState = pane.agent?.state
        self.model = model
        _changes = StateObject(wrappedValue: ChangesModel(source: ChangesSource(pane: pane)))
    }

    init(work: MuxaWorkGroup, model: AppModel) {
        target = .work(work)
        // Any participant working counts as the Work working, so the list
        // refreshes once they have all settled.
        agentState = work.participants.contains { MuxaAttention.activeStates.contains($0.agent.state) }
            ? "working" : "idle"
        self.model = model
        _changes = StateObject(wrappedValue: ChangesModel(source: ChangesSource(work: work)))
    }

    private var source: ChangesSource {
        switch target {
        case .pane(let pane): ChangesSource(pane: pane)
        case .work(let work): ChangesSource(work: work)
        }
    }

    private var commentCount: Int {
        changes.draftKey.map { drafts.draft(for: $0).comments.count } ?? 0
    }

    var body: some View {
        VStack(spacing: 0) {
            toolbar
            MuxaTheme.border(colorScheme).frame(height: 1)
            if let error = changes.refreshError {
                refreshBanner(error)
            }
            content
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .background(MuxaTheme.editor(colorScheme))
        .onAppear { changes.refresh() }
        .onChange(of: source) { changes.updateSource($0) }
        .onChange(of: agentState) { [agentState] newState in
            changes.agentStateChanged(from: agentState, to: newState)
        }
        .sheet(isPresented: $showingReview) {
            if let key = changes.draftKey, let snapshot = changes.snapshot {
                ReviewSendSheet(
                    key: key,
                    snapshot: snapshot,
                    target: target,
                    model: model,
                    onSent: showSentNotice
                )
            }
        }
    }

    // MARK: Toolbar

    private var toolbar: some View {
        HStack(spacing: 10) {
            if let snapshot = changes.snapshot {
                branchSummary(snapshot)
            } else if changes.isRefreshing {
                Text("Reading git status…")
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
            }
            Spacer(minLength: 8)
            if sentNotice {
                Label("Review sent", systemImage: "checkmark.circle")
                    .font(.system(size: 11))
                    .foregroundStyle(.green)
                    .transition(.opacity)
            }
            if changes.failure == nil {
                MuxaSegmented(
                    selection: Binding(get: { changes.compare }, set: { changes.setCompare($0) }),
                    options: [ChangesCompare.uncommitted, .branch]
                ) { compare in
                    switch compare {
                    case .uncommitted: Text("Uncommitted")
                    case .branch: Text("Branch")
                    }
                }
                .help(compareHelp)
            }
            Button {
                changes.refresh()
            } label: {
                if changes.isRefreshing {
                    ProgressView().controlSize(.small).scaleEffect(0.7)
                } else {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
            }
            .buttonStyle(.muxaIcon)
            .keyboardShortcut("r", modifiers: .command)
            .help("Refresh changes (⌘R)")
            reviewButton
        }
        .padding(.leading, 12)
        .padding(.trailing, 8)
        .frame(height: MuxaTheme.breadcrumbHeight + 4)
    }

    private var compareHelp: String {
        if let base = changes.branchBase {
            String(localized: "Uncommitted: HEAD against the index and working tree. Branch: everything since \(base.ref) (\(base.shortSHA)).")
        } else {
            String(localized: "Branch comparison needs an upstream, origin/HEAD, main, or master to compare with.")
        }
    }

    @ViewBuilder
    private var reviewButton: some View {
        let title: LocalizedStringKey = commentCount > 0 ? "Review (\(commentCount))…" : "Review…"
        if commentCount > 0 {
            Button(title) { showingReview = true }
                .buttonStyle(.muxaPrimary)
                .disabled(changes.draftKey == nil)
        } else {
            Button(title) { showingReview = true }
                .buttonStyle(.muxaSecondary)
                .disabled(changes.draftKey == nil)
        }
    }

    private func branchSummary(_ snapshot: GitChangesSnapshot) -> some View {
        HStack(spacing: 8) {
            Label {
                Text(verbatim: snapshot.branch.displayName)
                    .lineLimit(1)
                    .truncationMode(.middle)
            } icon: {
                Image(systemName: "arrow.triangle.branch")
            }
            .font(.system(size: 12, weight: .medium))
            .help(snapshot.root)
            if snapshot.branch.upstream != nil {
                Text(verbatim: "↑\(snapshot.branch.ahead) ↓\(snapshot.branch.behind)")
                    .font(.system(size: 11).monospacedDigit())
                    .foregroundStyle(.secondary)
                    .help(Text("Ahead / behind \(snapshot.branch.upstream ?? "")"))
            }
            Text("\(snapshot.fileCount) files")
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
            ChangeCountsLabel(added: snapshot.totalAdded, deleted: snapshot.totalDeleted, pending: !snapshot.countsLoaded)
        }
        .lineLimit(1)
        .layoutPriority(-1)
    }

    private func refreshBanner(_ error: String) -> some View {
        HStack(spacing: 8) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
            Text(verbatim: error)
                .lineLimit(2)
                .textSelection(.enabled)
            Spacer()
            Button("Retry") { changes.refresh() }
                .buttonStyle(.muxaGhost)
        }
        .font(.system(size: 11))
        .padding(.horizontal, 12)
        .padding(.vertical, 5)
        .background(Color.orange.opacity(0.1))
    }

    // MARK: Content

    @ViewBuilder
    private var content: some View {
        if let failure = changes.failure {
            failureView(failure)
        } else if let snapshot = changes.snapshot {
            if snapshot.files.isEmpty {
                cleanView(snapshot)
            } else {
                HSplitView {
                    ChangesFileList(changes: changes, viewed: changes.viewed, snapshot: snapshot, commentedPaths: commentedPaths)
                        .frame(minWidth: 200, idealWidth: 260, maxWidth: 420)
                    ChangesDiffPane(changes: changes, drafts: drafts, viewed: changes.viewed)
                        .frame(minWidth: 320, maxWidth: .infinity)
                }
            }
        } else {
            VStack(spacing: 8) {
                ProgressView().controlSize(.small)
                Text("Reading git status…")
                    .font(.system(size: 12))
                    .foregroundStyle(.secondary)
            }
        }
    }

    private var commentedPaths: Set<String> {
        guard let key = changes.draftKey else { return [] }
        return Set(drafts.draft(for: key).comments.map(\.path))
    }

    private func cleanView(_ snapshot: GitChangesSnapshot) -> some View {
        ChangesEmptyState(
            title: snapshot.compare == .uncommitted ? "No uncommitted changes" : "No changes on this branch",
            systemImage: "checkmark.seal",
            message: snapshot.compare == .uncommitted
                ? Text("The working tree matches HEAD.")
                : Text("Nothing differs from \(snapshot.base?.ref ?? "the branch base").")
        ) {
            if snapshot.compare == .uncommitted, changes.branchBase != nil {
                Button("Compare with branch base") { changes.setCompare(.branch) }
                    .buttonStyle(.muxaSecondary)
            }
        }
    }

    @ViewBuilder
    private func failureView(_ failure: ChangesFailure) -> some View {
        switch failure {
        case .noPath:
            ChangesEmptyState(
                title: "No working directory",
                systemImage: "folder.badge.questionmark",
                message: Text("This pane has no working directory Muxa can read.")
            ) { EmptyView() }
        case .remote(let alias):
            ChangesEmptyState(
                title: "Changes are read from this Mac",
                systemImage: "server.rack",
                message: Text("Changes are read from this Mac's disk. \(alias) is a remote host, and muxad cannot run git there yet.")
            ) { EmptyView() }
        case .gitMissing:
            ChangesEmptyState(
                title: "git was not found",
                systemImage: "exclamationmark.triangle",
                message: Text("Install the Xcode Command Line Tools (xcode-select --install) or Homebrew git, then refresh.")
            ) {
                Button("Refresh") { changes.refresh() }.buttonStyle(.muxaSecondary)
            }
        case .notRepository(let path):
            ChangesEmptyState(
                title: "Not a git repository",
                systemImage: "folder",
                message: Text(verbatim: path)
            ) { EmptyView() }
        case .error(let message):
            ChangesEmptyState(
                title: "git failed",
                systemImage: "exclamationmark.triangle",
                message: Text(verbatim: message)
            ) {
                Button("Retry") { changes.refresh() }.buttonStyle(.muxaSecondary)
            }
        }
    }

    private func showSentNotice() {
        withAnimation { sentNotice = true }
        changes.refresh()
        Task {
            try? await Task.sleep(nanoseconds: 3_000_000_000)
            withAnimation { sentNotice = false }
        }
    }
}

struct ChangesEmptyState<Actions: View>: View {
    let title: LocalizedStringKey
    let systemImage: String
    let message: Text
    @ViewBuilder var actions: Actions

    var body: some View {
        VStack(spacing: 9) {
            Image(systemName: systemImage)
                .font(.system(size: 28))
                .foregroundStyle(.secondary)
            Text(title).font(.headline)
            message
                .font(.system(size: 12))
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .textSelection(.enabled)
                .frame(maxWidth: 420)
            actions
        }
        .padding(24)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

struct ChangeCountsLabel: View {
    let added: Int?
    let deleted: Int?
    var pending = false
    var isBinary = false

    var body: some View {
        HStack(spacing: 4) {
            if isBinary {
                Text("bin").foregroundStyle(.secondary)
            } else if pending, added == nil, deleted == nil {
                Text(verbatim: "…").foregroundStyle(.tertiary)
            } else {
                if let added, added > 0 {
                    Text(verbatim: "+\(added)").foregroundStyle(.green)
                }
                if let deleted, deleted > 0 {
                    Text(verbatim: "−\(deleted)").foregroundStyle(.red)
                }
            }
        }
        .font(.system(size: 11, design: .monospaced))
        .lineLimit(1)
        .fixedSize()
    }
}

// MARK: - File list

private struct ChangesFileList: View {
    @ObservedObject var changes: ChangesModel
    @ObservedObject var viewed: ViewedFilesStore
    let snapshot: GitChangesSnapshot
    let commentedPaths: Set<String>
    @State private var filter = ""
    @FocusState private var filterFocused: Bool
    @Environment(\.colorScheme) private var colorScheme

    private func files(in group: ChangeGroup) -> [ChangedFile] {
        let query = filter.trimmingCharacters(in: .whitespaces)
        let files = snapshot.files(in: group)
        guard !query.isEmpty else { return files }
        return files.filter { $0.path.localizedCaseInsensitiveContains(query) }
    }

    var body: some View {
        VStack(spacing: 0) {
            if snapshot.files.count > 30 {
                MuxaFilterField(prompt: String(localized: "Filter files"), text: $filter, focused: $filterFocused) {
                    EmptyView()
                }
                .padding(8)
            }
            viewedProgress
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    ForEach(snapshot.groups, id: \.self) { group in
                        let files = files(in: group)
                        if !files.isEmpty {
                            MuxaSectionTitle(title: "\(title(for: group, base: snapshot.base)) (\(files.count))") {
                                EmptyView()
                            }
                            ForEach(files) { file in
                                row(file)
                            }
                        }
                    }
                    if snapshot.omittedCount > 0 {
                        Text("\(snapshot.omittedCount) more files not shown")
                            .font(.system(size: 11))
                            .foregroundStyle(.secondary)
                            .padding(.horizontal, 14)
                            .padding(.vertical, 8)
                    }
                }
                .padding(.bottom, 8)
            }
            .changesListKeyboard(
                move: { offset in
                    changes.moveSelection(by: offset, in: snapshot.groups.flatMap(files(in:)))
                },
                toggleViewed: {
                    if let file = changes.selectedFile { changes.toggleViewed(file) }
                }
            )
        }
        .background(MuxaTheme.sideBar(colorScheme))
    }

    /// "N of M viewed" over a thin bar, like GitHub's files-changed header.
    private var viewedProgress: some View {
        let total = snapshot.files.count
        let count = snapshot.files.filter(changes.isViewed).count
        return VStack(alignment: .leading, spacing: 4) {
            Text("\(count) of \(total) viewed")
                .font(.system(size: 11).monospacedDigit())
                .foregroundStyle(.secondary)
            GeometryReader { geometry in
                ZStack(alignment: .leading) {
                    Capsule().fill(MuxaTheme.segmentTrack(colorScheme))
                    Capsule()
                        .fill(Color.accentColor)
                        .frame(width: total > 0 ? geometry.size.width * CGFloat(count) / CGFloat(total) : 0)
                }
            }
            .frame(height: 3)
        }
        .padding(.horizontal, 14)
        .padding(.top, 8)
        .padding(.bottom, 2)
        .help("Mark files viewed as you review them. A mark clears when the file changes again.")
    }

    private func title(for group: ChangeGroup, base: BranchBase?) -> String {
        switch group {
        case .conflicts: String(localized: "Conflicts")
        case .staged: String(localized: "Staged")
        case .unstaged: String(localized: "Changes")
        case .untracked: String(localized: "Untracked")
        case .branch: String(localized: "Changes vs \(base?.ref ?? "base")")
        }
    }

    private func row(_ file: ChangedFile) -> some View {
        let selected = file.id == changes.selectedFileID
        let isViewed = changes.isViewed(file)
        return HStack(spacing: 0) {
            rowButton(file, isViewed: isViewed)
            ViewedCheckbox(isViewed: isViewed) { changes.toggleViewed(file) }
                .padding(.trailing, 6)
        }
        .background(selected ? MuxaTheme.selection(colorScheme) : Color.clear)
    }

    private func rowButton(_ file: ChangedFile, isViewed: Bool) -> some View {
        Button {
            changes.select(fileID: file.id)
        } label: {
            HStack(spacing: 6) {
                Text(verbatim: file.kind.rawValue)
                    .font(.system(size: 11, weight: .semibold, design: .monospaced))
                    .foregroundStyle(kindColor(file.kind))
                    .frame(width: 12)
                if file.kind == .conflicted {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .font(.system(size: 9))
                        .foregroundStyle(.red)
                }
                (Text(verbatim: file.fileName).foregroundColor(.primary)
                    + Text(verbatim: file.directory.isEmpty ? "" : "  \(file.directory)").foregroundColor(.secondary))
                    .font(.system(size: 12))
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer(minLength: 4)
                if commentedPaths.contains(file.path) {
                    Image(systemName: "text.bubble")
                        .font(.system(size: 10))
                        .foregroundStyle(.tint)
                }
                ChangeCountsLabel(
                    added: file.added, deleted: file.deleted,
                    pending: !snapshot.countsLoaded, isBinary: file.isBinary
                )
            }
            // A viewed file steps back so what is left to review stands out.
            .opacity(isViewed ? 0.5 : 1)
            .padding(.leading, 14)
            .padding(.trailing, 4)
            .frame(height: MuxaTheme.rowHeight)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(Text(verbatim: file.origPath.map { "\($0) → \(file.path)" } ?? file.path))
    }

    private func kindColor(_ kind: ChangeKind) -> Color {
        switch kind {
        case .modified, .typeChanged: .orange
        case .added, .untracked: .green
        case .deleted, .conflicted: .red
        case .renamed, .copied: .blue
        }
    }
}

/// GitHub's per-file "Viewed" box, as a flat icon toggle.
private struct ViewedCheckbox: View {
    let isViewed: Bool
    var showsTitle = false
    let toggle: () -> Void

    var body: some View {
        if showsTitle {
            Button(action: toggle) { label }
                .buttonStyle(.muxaGhost)
                .help(help)
        } else {
            Button(action: toggle) { label }
                .buttonStyle(.muxaIcon(size: 20))
                .help(help)
        }
    }

    private var label: some View {
        Label {
            Text("Viewed")
        } icon: {
            // The button styles tint their label; the box keeps its own.
            Image(systemName: isViewed ? "checkmark.square.fill" : "square")
                .foregroundStyle(isViewed ? Color.accentColor : Color.secondary)
        }
    }

    private var help: LocalizedStringKey {
        isViewed ? "Mark as not viewed" : "Mark as viewed"
    }
}

private extension View {
    /// ↑/↓ move through the file list once it has focus; on macOS 14 and
    /// later j/k do too, and v toggles the selected file's Viewed box.
    @ViewBuilder
    func changesListKeyboard(move: @escaping (Int) -> Void, toggleViewed: @escaping () -> Void) -> some View {
        let handled = focusable().onMoveCommand { direction in
            switch direction {
            case .up: move(-1)
            case .down: move(1)
            default: break
            }
        }
        if #available(macOS 14.0, *) {
            handled
                .onKeyPress(characters: CharacterSet(charactersIn: "jkv"), phases: .down) { press in
                    guard press.modifiers.isEmpty || press.modifiers == .shift else { return .ignored }
                    switch press.characters.lowercased() {
                    case "j": move(1)
                    case "k": move(-1)
                    case "v": toggleViewed()
                    default: return .ignored
                    }
                    return .handled
                }
                .focusEffectDisabled()
        } else {
            handled
        }
    }
}

// MARK: - Diff

private enum DiffMetrics {
    static let fontSize: CGFloat = 12
    static let rowHeight: CGFloat = 18
    static let numberWidth: CGFloat = 44
    static let markerWidth: CGFloat = 18
    static var gutterWidth: CGFloat { numberWidth * 2 + markerWidth }
    /// One side of the split view numbers only its own file.
    static var splitGutterWidth: CGFloat { numberWidth + markerWidth }
    static let font = Font.system(size: fontSize, design: .monospaced)

    /// One monospaced cell, for sizing the scrollable width to the longest
    /// line so row backgrounds run edge to edge.
    static let characterWidth: CGFloat = {
        let font = NSFont.monospacedSystemFont(ofSize: fontSize, weight: .regular)
        return ("0" as NSString).size(withAttributes: [.font: font]).width
    }()

    static func display(_ text: String) -> String {
        text.replacingOccurrences(of: "\t", with: "    ")
    }
}

private enum DiffRow: Identifiable {
    case orphanHeader
    case hunk(DiffHunk, collapsed: Bool)
    case line(DiffLine)
    case split(SplitDiffRow)
    case selectionActions(lines: Int)
    case composer(editing: UUID?)
    case comment(ReviewComment)
    case truncated

    var id: String {
        switch self {
        case .orphanHeader: "orphans"
        case .hunk(let hunk, _): "hunk-\(hunk.id)"
        case .line(let line): "line-\(line.id)"
        case .split(let row): "split-\(row.id)"
        case .selectionActions: "selection"
        case .composer(let editing): "composer-\(editing?.uuidString ?? "new")"
        case .comment(let comment): "comment-\(comment.id)"
        case .truncated: "truncated"
        }
    }
}

private struct ChangesDiffPane: View {
    @ObservedObject var changes: ChangesModel
    @ObservedObject var drafts: ReviewDraftStore
    @ObservedObject var viewed: ViewedFilesStore
    @AppStorage(DiffLayout.storageKey) private var layout = DiffLayout.unified
    @State private var composing = false
    @State private var editingCommentID: UUID?
    @State private var composerText = ""
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        VStack(spacing: 0) {
            if let file = changes.selectedFile {
                header(file)
                MuxaTheme.border(colorScheme).frame(height: 1)
            }
            diffBody
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .background(MuxaTheme.editor(colorScheme))
        .onChange(of: changes.selectedFileID) { _ in cancelComposer() }
    }

    private func header(_ file: ChangedFile) -> some View {
        HStack(spacing: 8) {
            Text(verbatim: file.origPath.map { "\($0) → \(file.path)" } ?? file.path)
                .font(.system(size: 12, weight: .medium))
                .lineLimit(1)
                .truncationMode(.middle)
                .textSelection(.enabled)
            ChangeCountsLabel(added: file.added, deleted: file.deleted, isBinary: file.isBinary)
            Spacer()
            if case .loaded(let diff) = changes.diff, diff.hunks.count > 1 {
                let allCollapsed = changes.collapsedHunks.count == diff.hunks.count
                Button(allCollapsed ? "Expand all" : "Collapse all") {
                    changes.collapsedHunks = allCollapsed ? [] : Set(diff.hunks.map(\.id))
                }
                .buttonStyle(.muxaGhost)
            }
            MuxaSegmented(selection: $layout, options: DiffLayout.allCases) { layout in
                switch layout {
                case .unified: Text("Unified")
                case .split: Text("Split")
                }
            }
            .help("Show the diff as one column, or old and new side by side")
            ViewedCheckbox(isViewed: changes.isViewed(file), showsTitle: true) {
                changes.toggleViewed(file)
            }
        }
        .padding(.leading, 12)
        .padding(.trailing, 8)
        .frame(height: MuxaTheme.breadcrumbHeight)
    }

    @ViewBuilder
    private var diffBody: some View {
        switch changes.diff {
        case .none:
            ChangesEmptyState(title: "Select a file", systemImage: "doc.text", message: Text("Choose a changed file to see its diff.")) {
                EmptyView()
            }
        case .loading:
            ProgressView().controlSize(.small)
        case .failed(let message):
            ChangesEmptyState(title: "The diff could not be read", systemImage: "exclamationmark.triangle", message: Text(verbatim: message)) {
                Button("Retry") { changes.refresh() }.buttonStyle(.muxaSecondary)
            }
        case .large(_, let lines):
            ChangesEmptyState(
                title: "Large diff",
                systemImage: "doc.text.magnifyingglass",
                message: Text("\(lines) lines. Showing it may be slow.")
            ) {
                Button("Show anyway") { changes.showLargeDiff() }.buttonStyle(.muxaSecondary)
            }
        case .loaded(let file):
            if file.isBinary {
                ChangesEmptyState(title: "Binary file changed", systemImage: "doc.zipper", message: Text("Binary files have no text diff.")) {
                    EmptyView()
                }
            } else if file.hunks.isEmpty {
                ChangesEmptyState(
                    title: "No textual changes",
                    systemImage: "doc",
                    message: file.modeChange.map { Text("File mode changed: \($0)") } ?? Text("Only metadata changed.")
                ) { EmptyView() }
            } else {
                diffList(file)
            }
        }
    }

    private var draftKey: ReviewDraftKey? { changes.draftKey }

    private func comments(for file: DiffFile) -> [ReviewComment] {
        guard let key = draftKey else { return [] }
        return drafts.draft(for: key).comments.filter { $0.path == file.path }
    }

    private func rows(for file: DiffFile) -> (rows: [DiffRow], commented: Set<Int>, maxColumns: Int) {
        let comments = comments(for: file)
        var anchored: [Int: [ReviewComment]] = [:]
        var orphans: [ReviewComment] = []
        var commented = Set<Int>()
        for comment in comments {
            if let lineID = comment.anchorLineID(in: file) {
                anchored[lineID, default: []].append(comment)
                let span = comment.quote.count
                for id in (lineID - span + 1)...lineID { commented.insert(id) }
            } else {
                orphans.append(comment)
            }
        }

        var rows: [DiffRow] = []
        var maxColumns = 0
        func appendComments(_ comments: [ReviewComment]) {
            for comment in comments {
                rows.append(comment.id == editingCommentID ? .composer(editing: comment.id) : .comment(comment))
            }
        }
        if !orphans.isEmpty {
            rows.append(.orphanHeader)
            appendComments(orphans)
        }
        let selectionEnd = changes.selection?.range.upperBound
        // The composer or the selection's actions, under the row holding
        // the selection's last line.
        func appendSelection(after lineIDs: [Int]) {
            guard let selectionEnd, lineIDs.contains(selectionEnd), editingCommentID == nil else { return }
            if composing {
                rows.append(.composer(editing: nil))
            } else if let selection = changes.selection {
                rows.append(.selectionActions(lines: selection.lines(in: file).count))
            }
        }
        for hunk in file.hunks {
            let collapsed = changes.collapsedHunks.contains(hunk.id)
            rows.append(.hunk(hunk, collapsed: collapsed))
            guard !collapsed else { continue }
            if layout == .split, !hunk.isCombined {
                for row in SplitDiffPairer.rows(for: hunk) {
                    rows.append(.split(row))
                    let width = max(
                        row.left.map { DiffMetrics.display($0.text).count } ?? 0,
                        row.right.map { DiffMetrics.display($0.text).count } ?? 0
                    )
                    maxColumns = max(maxColumns, width)
                    appendSelection(after: row.lineIDs)
                    for id in row.lineIDs {
                        if let comments = anchored[id] { appendComments(comments) }
                    }
                }
                continue
            }
            for line in hunk.lines {
                rows.append(.line(line))
                maxColumns = max(maxColumns, DiffMetrics.display(line.text).count)
                appendSelection(after: [line.id])
                if let comments = anchored[line.id] { appendComments(comments) }
            }
        }
        if file.truncated { rows.append(.truncated) }
        return (rows, commented, maxColumns)
    }

    private func diffList(_ file: DiffFile) -> some View {
        let built = rows(for: file)
        let textWidth = CGFloat(built.maxColumns) * DiffMetrics.characterWidth
        return GeometryReader { geometry in
            // Split view: two equal columns, each wide enough for the longest
            // line on either side, scrolling together.
            let contentWidth = layout == .split
                ? max(geometry.size.width, 2 * (DiffMetrics.splitGutterWidth + textWidth + 12) + 1)
                : max(geometry.size.width, DiffMetrics.gutterWidth + textWidth + 24)
            let cardWidth = min(max(geometry.size.width - DiffMetrics.gutterWidth - 24, 240), 640)
            ScrollView([.vertical, .horizontal]) {
                LazyVStack(alignment: .leading, spacing: 0) {
                    ForEach(built.rows) { row in
                        rowView(
                            row, file: file, commented: built.commented, cardWidth: cardWidth,
                            columnWidth: (contentWidth - 1) / 2
                        )
                    }
                }
                .frame(width: contentWidth, alignment: .leading)
                .padding(.bottom, 12)
            }
        }
    }

    @ViewBuilder
    private func rowView(_ row: DiffRow, file: DiffFile, commented: Set<Int>, cardWidth: CGFloat, columnWidth: CGFloat) -> some View {
        switch row {
        case .orphanHeader:
            Text("Comments on lines no longer in this diff")
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(.secondary)
                .padding(.leading, DiffMetrics.gutterWidth)
                .padding(.vertical, 6)
        case .hunk(let hunk, let collapsed):
            DiffHunkHeaderRow(hunk: hunk, collapsed: collapsed) {
                if collapsed {
                    changes.collapsedHunks.remove(hunk.id)
                } else {
                    changes.collapsedHunks.insert(hunk.id)
                }
            }
        case .line(let line):
            let selectable = !(file.hunk(containing: line.id)?.isCombined ?? true)
            DiffLineRow(
                line: line,
                selected: changes.selection?.range.contains(line.id) ?? false,
                commented: commented.contains(line.id),
                selectable: selectable
            ) { extend in
                select(line: line.id, in: file, extend: extend)
            }
        case .split(let row):
            // Pairs only come from ordinary hunks, which are all selectable.
            let selection = changes.selection?.range
            HStack(spacing: 0) {
                DiffSplitCell(
                    line: row.left,
                    side: .old,
                    selected: row.left.map { selection?.contains($0.id) ?? false } ?? false,
                    commented: row.left.map { commented.contains($0.id) } ?? false
                ) { id, extend in
                    select(line: id, in: file, extend: extend)
                }
                .frame(width: columnWidth)
                MuxaTheme.border(colorScheme).frame(width: 1)
                DiffSplitCell(
                    line: row.right,
                    side: .new,
                    selected: row.right.map { selection?.contains($0.id) ?? false } ?? false,
                    commented: row.right.map { commented.contains($0.id) } ?? false
                ) { id, extend in
                    select(line: id, in: file, extend: extend)
                }
                .frame(width: columnWidth)
            }
        case .selectionActions(let lines):
            let title: LocalizedStringKey = lines == 1 ? "Comment on line" : "Comment on \(lines) lines"
            HStack(spacing: 8) {
                Button {
                    startComposing()
                } label: {
                    Label(title, systemImage: "text.bubble")
                }
                .buttonStyle(.muxaSecondary)
                .keyboardShortcut("c", modifiers: [.command, .option])
                .help("Comment on the selection (⌥⌘C)")
                Button("Clear") { changes.selection = nil }
                    .buttonStyle(.muxaGhost)
                Text("Shift-click to extend within the hunk")
                    .font(.system(size: 11))
                    .foregroundStyle(.tertiary)
            }
            .padding(.leading, DiffMetrics.gutterWidth)
            .padding(.vertical, 5)
        case .composer(let editing):
            ReviewCommentEditor(
                text: $composerText,
                isEditing: editing != nil,
                cancel: cancelComposer,
                save: { saveComment(in: file) }
            )
            .frame(width: cardWidth)
            .padding(.leading, DiffMetrics.gutterWidth)
            .padding(.vertical, 6)
        case .comment(let comment):
            ReviewCommentCard(
                comment: comment,
                edit: {
                    editingCommentID = comment.id
                    composerText = comment.body
                    composing = false
                },
                delete: {
                    if let key = draftKey { drafts.remove(comment.id, from: key) }
                }
            )
            .frame(width: cardWidth)
            .padding(.leading, DiffMetrics.gutterWidth)
            .padding(.vertical, 4)
        case .truncated:
            Label("The diff was cut off: it is larger than Muxa reads at once.", systemImage: "scissors")
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
                .padding(.leading, DiffMetrics.gutterWidth)
                .padding(.vertical, 8)
        }
    }

    private func select(line: Int, in file: DiffFile, extend: Bool) {
        guard !composing, editingCommentID == nil else { return }
        let next = DiffSelection.select(line: line, in: file, current: changes.selection, extend: extend)
        // Clicking the only selected line again clears it.
        if !extend, let current = changes.selection, current.range == line...line {
            changes.selection = nil
        } else {
            changes.selection = next
        }
    }

    private func startComposing() {
        guard changes.selection != nil, draftKey != nil else { return }
        composerText = ""
        editingCommentID = nil
        composing = true
    }

    private func cancelComposer() {
        composing = false
        editingCommentID = nil
        composerText = ""
    }

    private func saveComment(in file: DiffFile) {
        let body = composerText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !body.isEmpty, let key = draftKey else { return }
        if let editingCommentID {
            drafts.update(editingCommentID, body: body, in: key)
        } else if let selection = changes.selection,
                  var comment = ReviewComment(path: file.path, lines: selection.lines(in: file), body: body) {
            comment.group = changes.selectedFile?.group
            drafts.add(comment, to: key)
            changes.selection = nil
        }
        cancelComposer()
    }
}

private struct DiffHunkHeaderRow: View {
    let hunk: DiffHunk
    let collapsed: Bool
    let toggle: () -> Void
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        Button(action: toggle) {
            HStack(spacing: 6) {
                Image(systemName: collapsed ? "chevron.right" : "chevron.down")
                    .font(.system(size: 9, weight: .semibold))
                    .foregroundStyle(.secondary)
                    .frame(width: DiffMetrics.gutterWidth - 12, alignment: .trailing)
                Text(verbatim: hunk.header)
                    .font(DiffMetrics.font)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .fixedSize()
                if collapsed {
                    Text("\(hunk.lines.count) lines")
                        .font(.system(size: 10))
                        .foregroundStyle(.tertiary)
                }
                Spacer(minLength: 0)
            }
            .frame(height: DiffMetrics.rowHeight + 4)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(MuxaTheme.diffHunkBackground(colorScheme))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(collapsed ? "Expand hunk" : "Collapse hunk")
    }
}

private struct DiffLineRow: View {
    let line: DiffLine
    let selected: Bool
    let commented: Bool
    let selectable: Bool
    let tap: (_ extend: Bool) -> Void
    @State private var hovering = false
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        HStack(spacing: 0) {
            number(line.oldNumber)
            number(line.newNumber)
            Text(verbatim: marker)
                .frame(width: DiffMetrics.markerWidth)
                .foregroundStyle(hovering && selectable ? Color.accentColor : Color.secondary)
            Text(verbatim: DiffMetrics.display(line.text))
                .foregroundStyle(line.kind == .noNewline ? Color.secondary : Color.primary)
                .lineLimit(1)
                .fixedSize()
            Spacer(minLength: 0)
        }
        .font(DiffMetrics.font)
        .frame(height: DiffMetrics.rowHeight)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(background)
        .contentShape(Rectangle())
        .onHover { hovering = $0 }
        .onTapGesture {
            guard selectable, line.kind != .noNewline else { return }
            tap(NSEvent.modifierFlags.contains(.shift))
        }
    }

    private var marker: String {
        if hovering && selectable && line.kind != .noNewline { return "+" }
        switch line.kind {
        case .added: return "+"
        case .removed: return "−"
        case .context, .noNewline: return ""
        }
    }

    private func number(_ value: Int?) -> some View {
        Text(verbatim: value.map(String.init) ?? "")
            .foregroundStyle(.tertiary)
            .padding(.trailing, 6)
            .frame(width: DiffMetrics.numberWidth, alignment: .trailing)
            .background(gutterBackground)
    }

    private var gutterBackground: Color {
        switch line.kind {
        case .added: MuxaTheme.diffAddedGutter(colorScheme)
        case .removed: MuxaTheme.diffRemovedGutter(colorScheme)
        default: .clear
        }
    }

    private var background: Color {
        if selected { return MuxaTheme.selection(colorScheme) }
        if commented { return MuxaTheme.diffCommentedBackground(colorScheme) }
        switch line.kind {
        case .added: return MuxaTheme.diffAddedBackground(colorScheme)
        case .removed: return MuxaTheme.diffRemovedBackground(colorScheme)
        case .context, .noNewline: return .clear
        }
    }
}

/// One side of a split-view row: the old file's line on the left, the new
/// file's on the right, or a blank filler where that side has no line.
private struct DiffSplitCell: View {
    let line: DiffLine?
    let side: ReviewSide
    let selected: Bool
    let commented: Bool
    let tap: (_ lineID: Int, _ extend: Bool) -> Void
    @State private var hovering = false
    @Environment(\.colorScheme) private var colorScheme

    private var selectable: Bool {
        guard let line else { return false }
        return line.kind != .noNewline
    }

    var body: some View {
        HStack(spacing: 0) {
            Text(verbatim: number.map(String.init) ?? "")
                .foregroundStyle(.tertiary)
                .padding(.trailing, 6)
                .frame(width: DiffMetrics.numberWidth, alignment: .trailing)
                .frame(maxHeight: .infinity)
                .background(gutterBackground)
            Text(verbatim: marker)
                .frame(width: DiffMetrics.markerWidth)
                .foregroundStyle(hovering && selectable ? Color.accentColor : Color.secondary)
            if let line {
                Text(verbatim: DiffMetrics.display(line.text))
                    .foregroundStyle(line.kind == .noNewline ? Color.secondary : Color.primary)
                    .lineLimit(1)
                    .fixedSize()
            }
            Spacer(minLength: 0)
        }
        .font(DiffMetrics.font)
        .frame(height: DiffMetrics.rowHeight)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(background)
        .clipped()
        .contentShape(Rectangle())
        .onHover { hovering = $0 }
        .onTapGesture {
            guard selectable, let line else { return }
            tap(line.id, NSEvent.modifierFlags.contains(.shift))
        }
    }

    /// A context line carries both numbers; each side shows its own.
    private var number: Int? {
        side == .old ? line?.oldNumber : line?.newNumber
    }

    private var marker: String {
        guard let line else { return "" }
        if hovering && selectable { return "+" }
        switch line.kind {
        case .added: return "+"
        case .removed: return "−"
        case .context, .noNewline: return ""
        }
    }

    private var gutterBackground: Color {
        switch line?.kind {
        case .added: MuxaTheme.diffAddedGutter(colorScheme)
        case .removed: MuxaTheme.diffRemovedGutter(colorScheme)
        default: .clear
        }
    }

    private var background: Color {
        guard let line else {
            // Filler opposite a line the other side added or removed.
            return Color.primary.opacity(colorScheme == .dark ? 0.04 : 0.035)
        }
        if selected { return MuxaTheme.selection(colorScheme) }
        if commented { return MuxaTheme.diffCommentedBackground(colorScheme) }
        switch line.kind {
        case .added: return MuxaTheme.diffAddedBackground(colorScheme)
        case .removed: return MuxaTheme.diffRemovedBackground(colorScheme)
        case .context, .noNewline: return .clear
        }
    }
}

private struct ReviewCommentEditor: View {
    @Binding var text: String
    let isEditing: Bool
    let cancel: () -> Void
    let save: () -> Void
    @FocusState private var focused: Bool

    var body: some View {
        VStack(alignment: .trailing, spacing: 6) {
            TextField("Leave a comment for the agent", text: $text, axis: .vertical)
                .lineLimit(2...8)
                .focused($focused)
                .muxaFieldChrome(focused: focused)
                .onSubmit(save)
            HStack(spacing: 6) {
                Button("Cancel", action: cancel)
                    .buttonStyle(.muxaGhost)
                    .keyboardShortcut(.cancelAction)
                Button(isEditing ? "Save" : "Add comment", action: save)
                    .buttonStyle(.muxaPrimary)
                    .keyboardShortcut(.return, modifiers: .command)
                    .disabled(text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(10)
        .background(MuxaTheme.panelFill, in: RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous)
                .stroke(Color.accentColor.opacity(0.6), lineWidth: 1)
        }
        .onAppear { focused = true }
    }
}

private struct ReviewCommentCard: View {
    let comment: ReviewComment
    let edit: () -> Void
    let delete: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 6) {
                Image(systemName: "text.bubble.fill")
                    .font(.system(size: 10))
                    .foregroundStyle(.tint)
                Text(verbatim: comment.location)
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
                if comment.outdated {
                    Text("Outdated")
                        .font(.system(size: 10, weight: .semibold))
                        .foregroundStyle(.orange)
                }
                Spacer()
                Button(action: edit) {
                    Label("Edit comment", systemImage: "pencil")
                }
                .buttonStyle(.muxaIcon(size: 20))
                .help("Edit comment")
                Button(action: delete) {
                    Label("Delete comment", systemImage: "trash")
                }
                .buttonStyle(.muxaIcon(size: 20))
                .help("Delete comment")
            }
            Text(verbatim: comment.body)
                .font(.system(size: 12))
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 7)
        .background(MuxaTheme.panelFill, in: RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous))
        .overlay {
            RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous)
                .stroke(.separator.opacity(0.55), lineWidth: 0.5)
        }
    }
}

// MARK: - Send sheet

private struct ReviewSendSheet: View {
    let key: ReviewDraftKey
    let snapshot: GitChangesSnapshot
    let target: ReviewTarget
    @ObservedObject var model: AppModel
    let onSent: () -> Void
    @ObservedObject private var drafts = ReviewDraftStore.shared
    @State private var preview = ""
    @State private var previewEdited = false
    @State private var recipient: ReviewRecipient = .all
    @State private var sending = false
    @State private var feedback: PromptFeedback?
    @FocusState private var noteFocused: Bool
    @Environment(\.dismiss) private var dismiss
    @Environment(\.colorScheme) private var colorScheme

    private var draft: ReviewDraft { drafts.draft(for: key) }

    private var generated: String {
        ReviewPromptFormatter.format(
            draft: draft,
            repositoryName: snapshot.repositoryName,
            branch: snapshot.branch.displayName,
            base: snapshot.compare == .branch ? snapshot.base : nil
        )
    }

    private var workRecipients: [MuxaHostedAgent] {
        guard case .work(let work) = target else { return [] }
        return work.participants.filter { $0.pane != nil }
    }

    private var hosts: [MuxaFleetHostIdentity] {
        switch target {
        case .pane(let pane):
            return [pane.host]
        case .work:
            switch recipient {
            case .all: return workRecipients.map(\.host)
            case .participant(let id): return workRecipients.filter { $0.id == id }.map(\.host)
            }
        }
    }

    private var sendTitle: String {
        switch target {
        case .pane(let pane):
            String(localized: "Send to \(Self.label(pane: pane))")
        case .work:
            switch recipient {
            case .all: String(localized: "Send to \(workRecipients.count) collaborators")
            case .participant(let id):
                String(localized: "Send to \(workRecipients.first { $0.id == id }.map(Self.label(participant:)) ?? "")")
            }
        }
    }

    private var canSend: Bool {
        ReviewSendRules.canSend(draft: draft, previewEdited: previewEdited, preview: preview, sending: sending, hosts: hosts)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("Send review")
                    .font(.system(size: 15, weight: .semibold))
                Spacer()
                recipientControl
            }

            TextField("General note (optional)", text: Binding(
                get: { draft.note },
                set: { drafts.setNote($0, for: key) }
            ), axis: .vertical)
                .lineLimit(1...4)
                .focused($noteFocused)
                .muxaFieldChrome(focused: noteFocused)

            if !draft.comments.isEmpty {
                commentList
            }

            HStack {
                Text("Preview")
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(.secondary)
                Spacer()
                if previewEdited {
                    Button("Reset preview") {
                        preview = generated
                        previewEdited = false
                    }
                    .buttonStyle(.muxaGhost)
                }
            }
            TextEditor(text: Binding(
                get: { preview },
                set: {
                    preview = $0
                    previewEdited = $0 != generated
                }
            ))
            .font(.system(size: 11, design: .monospaced))
            .scrollContentBackground(.hidden)
            .padding(6)
            .background(
                RoundedRectangle(cornerRadius: 5, style: .continuous)
                    .fill(MuxaTheme.inputBackground(colorScheme))
            )
            .overlay(
                RoundedRectangle(cornerRadius: 5, style: .continuous)
                    .strokeBorder(MuxaTheme.inputBorder(colorScheme), lineWidth: 1)
            )
            .frame(minHeight: 200, maxHeight: .infinity)

            HStack(spacing: 8) {
                sizeLabel
                Spacer()
                PromptComposerStatus(feedback: feedback)
                Button("Cancel") { dismiss() }
                    .buttonStyle(.muxaSecondary)
                    .keyboardShortcut(.cancelAction)
                Button(sendTitle, action: send)
                    .buttonStyle(.muxaPrimary)
                    .keyboardShortcut(.return, modifiers: .command)
                    .disabled(!canSend)
            }
            if case .pane(let pane) = target, !pane.host.local, pane.host.mode != "control" {
                Text("This host is registered in observe mode. Change it to control to send prompts.")
                    .font(.caption2)
                    .foregroundStyle(.orange)
            }
        }
        .padding(18)
        .frame(width: 640, height: 620)
        .background(MuxaTheme.editor(colorScheme))
        .onAppear {
            preview = generated
            if let first = workRecipients.first { recipient = .participant(first.id) }
        }
        .onChange(of: generated) { newValue in
            if !previewEdited { preview = newValue }
        }
    }

    @ViewBuilder
    private var recipientControl: some View {
        switch target {
        case .pane(let pane):
            Label {
                Text(verbatim: "\(Self.label(pane: pane)) · \(pane.host.alias)/\(pane.pane.paneID)")
            } icon: {
                Image(systemName: "person.crop.circle")
            }
            .font(.system(size: 12))
            .foregroundStyle(.secondary)
        case .work:
            Picker("Send to", selection: $recipient) {
                ForEach(workRecipients) { participant in
                    Text(verbatim: Self.label(participant: participant))
                        .tag(ReviewRecipient.participant(participant.id))
                }
                if workRecipients.count > 1 {
                    Divider()
                    Text("All collaborators").tag(ReviewRecipient.all)
                }
            }
            .pickerStyle(.menu)
            .fixedSize()
        }
    }

    private var commentList: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 4) {
                ForEach(draft.comments) { comment in
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        Text(verbatim: comment.location)
                            .font(.system(size: 11, design: .monospaced))
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                            .truncationMode(.middle)
                            .frame(maxWidth: 220, alignment: .leading)
                        Text(verbatim: comment.body)
                            .font(.system(size: 12))
                            .lineLimit(1)
                        if comment.outdated {
                            Text("Outdated")
                                .font(.system(size: 10, weight: .semibold))
                                .foregroundStyle(.orange)
                        }
                        Spacer()
                        Button {
                            drafts.remove(comment.id, from: key)
                        } label: {
                            Label("Delete comment", systemImage: "trash")
                        }
                        .buttonStyle(.muxaIcon(size: 20))
                        .help("Delete comment")
                    }
                }
            }
        }
        .frame(maxHeight: 120)
    }

    private var sizeLabel: some View {
        let bytes = preview.utf8.count
        let size = ByteCountFormatter.string(fromByteCount: Int64(bytes), countStyle: .file)
        let comments = draft.comments.count
        return HStack(spacing: 6) {
            Text("\(size) · \(comments) comments")
                .foregroundStyle(.secondary)
            if bytes > ReviewSendLimits.maxBytes {
                Label("Too large to send: remove comments or shorten the preview.", systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.red)
            } else if bytes > ReviewSendLimits.warnBytes {
                Label("Large review: consider fewer or shorter quotes.", systemImage: "exclamationmark.triangle")
                    .foregroundStyle(.orange)
            }
        }
        .font(.system(size: 11))
        .lineLimit(1)
    }

    private func send() {
        guard canSend else { return }
        let text = preview.trimmingCharacters(in: .whitespacesAndNewlines)
        sending = true
        feedback = nil
        Task {
            defer { sending = false }
            do {
                switch target {
                case .pane(let pane):
                    try await model.client.sendFleetPrompt(host: pane.host, pane: pane.pane, text: text)
                case .work(let work):
                    switch recipient {
                    case .all:
                        _ = try await model.prompt(work: work, text: text)
                    case .participant(let id):
                        guard let participant = workRecipients.first(where: { $0.id == id }),
                              let pane = participant.pane else { return }
                        try await model.client.sendFleetPrompt(host: participant.host, pane: pane, text: text)
                    }
                }
                drafts.clear(key)
                onSent()
                dismiss()
            } catch {
                feedback = PromptFeedback(message: error.localizedDescription, succeeded: false)
            }
        }
    }

    private static func label(pane: MuxaWatchPane) -> String {
        if let alias = pane.pane.agentAlias, !alias.isEmpty { return "@\(alias)" }
        if let kind = pane.agent?.kind { return kind.replacingOccurrences(of: "_", with: " ") }
        return pane.pane.paneID
    }

    private static func label(participant: MuxaHostedAgent) -> String {
        if let alias = participant.pane?.agentAlias, !alias.isEmpty { return "@\(alias)" }
        return participant.agent.kind.replacingOccurrences(of: "_", with: " ")
    }
}
