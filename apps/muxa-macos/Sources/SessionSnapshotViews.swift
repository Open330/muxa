import SwiftUI

/// "Save Snapshot" / "Restore Snapshot…" for the Explore side bar's "…"
/// menu. Enabled whenever the daemon is connected, so a muxad too old for
/// snapshots still gets to explain itself in the sheet.
struct SessionSnapshotMenuItems: View {
    @ObservedObject var model: AppModel

    var body: some View {
        Button("Save Snapshot") { model.presentSessionSnapshots(.save) }
            .disabled(!model.isConnected)
        Button("Restore Snapshot…") { model.presentSessionSnapshots(.restore) }
            .disabled(!model.isConnected)
    }
}

/// The sheet `AppModel.sessionSnapshotSheet` asks for.
struct SessionSnapshotSheetHost: View {
    @ObservedObject var model: AppModel
    let sheet: MuxaSessionSnapshotSheet
    @StateObject private var viewModel: SessionSnapshotViewModel

    init(model: AppModel, sheet: MuxaSessionSnapshotSheet) {
        self.model = model
        self.sheet = sheet
        _viewModel = StateObject(
            wrappedValue: SessionSnapshotViewModel(client: model.client.makeSessionSnapshotClient())
        )
    }

    var body: some View {
        Group {
            switch sheet {
            case .save:
                SessionSnapshotSaveSheet(
                    viewModel: viewModel,
                    close: close,
                    showRestore: { model.sessionSnapshotSheet = .restore }
                )
            case .restore:
                SessionSnapshotRestoreSheet(viewModel: viewModel, close: close)
            }
        }
        .task {
            guard await model.client.supports(MuxaIPCClient.muxSnapshotCapability) else {
                viewModel.markUnsupported()
                return
            }
            if sheet == .restore {
                await viewModel.load()
            }
        }
    }

    private func close() {
        model.sessionSnapshotSheet = nil
    }
}

// MARK: - Save

struct SessionSnapshotSaveSheet: View {
    @ObservedObject var viewModel: SessionSnapshotViewModel
    let close: () -> Void
    let showRestore: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            VStack(alignment: .leading, spacing: 3) {
                Text("Save Snapshot")
                    .font(.system(size: 14, weight: .semibold))
                Text("Records every session, window layout, pane directory, and what each pane runs — agents come back on their own conversation.")
                    .font(.system(size: 12))
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let error = viewModel.error {
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.red)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            } else if viewModel.saved != nil, let status = viewModel.statusMessage {
                Label(status, systemImage: "checkmark.circle")
                    .font(.system(size: 12))
                    .foregroundStyle(.green)
            }
            HStack {
                if viewModel.phase == .saving {
                    ProgressView().controlSize(.small)
                    Text("Capturing the workspace…")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                if viewModel.saved != nil {
                    Button("Show in Restore…", action: showRestore)
                        .buttonStyle(.muxaSecondary)
                    Button("Done", action: close)
                        .buttonStyle(.muxaPrimary)
                        .keyboardShortcut(.defaultAction)
                } else {
                    Button("Cancel", action: close)
                        .buttonStyle(.muxaGhost)
                        .keyboardShortcut(.cancelAction)
                    Button("Save") { Task { await viewModel.save() } }
                        .buttonStyle(.muxaPrimary)
                        .keyboardShortcut(.defaultAction)
                        .disabled(viewModel.isBusy || !viewModel.supported)
                }
            }
        }
        .padding(20)
        .frame(width: 440)
    }
}

// MARK: - Restore

struct SessionSnapshotRestoreSheet: View {
    @ObservedObject var viewModel: SessionSnapshotViewModel
    let close: () -> Void
    @Environment(\.colorScheme) private var colorScheme
    @State private var pendingDelete: MuxSnapshotEntry?

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            if !viewModel.supported {
                unsupportedState
            } else if viewModel.hasLoaded && viewModel.snapshots.isEmpty && viewModel.error == nil {
                emptyState
            } else {
                HStack(spacing: 0) {
                    snapshotList
                        .frame(width: 250)
                    Divider()
                    preview
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                }
            }
            Divider()
            footer
        }
        .frame(minWidth: 720, idealWidth: 780, minHeight: 460, idealHeight: 540)
        .background(MuxaTheme.editor(colorScheme))
        .confirmationDialog(
            "Delete snapshot?",
            isPresented: Binding(
                get: { pendingDelete != nil },
                set: { if !$0 { pendingDelete = nil } }
            ),
            presenting: pendingDelete
        ) { entry in
            Button("Delete", role: .destructive) {
                Task { await viewModel.delete(entry.id) }
            }
        } message: { entry in
            if let date = entry.summary?.takenAt {
                Text("The snapshot from \(date.formatted(date: .abbreviated, time: .shortened)) is removed. Running sessions are not affected.")
            } else {
                Text("The snapshot \(entry.id) is removed. Running sessions are not affected.")
            }
        }
    }

    private var header: some View {
        HStack(alignment: .top) {
            VStack(alignment: .leading, spacing: 3) {
                Text("Restore Snapshot")
                    .font(.system(size: 14, weight: .semibold))
                Text("Recreates the sessions that are missing and relaunches what their panes ran. Sessions that already exist are left untouched.")
                    .font(.system(size: 12))
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer()
            Button(action: close) {
                Image(systemName: "xmark")
            }
            .buttonStyle(.muxaIcon)
            .keyboardShortcut(.cancelAction)
            .help("Close")
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 14)
    }

    private var emptyState: some View {
        VStack(spacing: 10) {
            Image(systemName: "camera.on.rectangle")
                .font(.system(size: 28))
                .foregroundStyle(.secondary)
            Text("No snapshots yet")
                .font(.system(size: 13, weight: .semibold))
            Text("muxad takes one automatically every 15 minutes when the workspace changes. You can also save one now from Explore \"…\" → Save Snapshot.")
                .font(.system(size: 12))
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 380)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var unsupportedState: some View {
        VStack(spacing: 10) {
            Image(systemName: "arrow.up.circle")
                .font(.system(size: 28))
                .foregroundStyle(.secondary)
            Text("muxad is too old for snapshots")
                .font(.system(size: 13, weight: .semibold))
            Text(SessionSnapshotViewModel.unsupportedMessage)
                .font(.system(size: 12))
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .textSelection(.enabled)
                .frame(maxWidth: 380)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var snapshotList: some View {
        VStack(spacing: 0) {
            MuxaSectionTitle(title: String(localized: "Snapshots")) {
                Button {
                    Task { await viewModel.load() }
                } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .buttonStyle(.muxaIcon)
                .disabled(viewModel.isBusy)
                .help("Refresh")
            }
            ScrollView {
                LazyVStack(spacing: 1) {
                    ForEach(viewModel.snapshots) { entry in
                        SessionSnapshotListRow(
                            entry: entry,
                            selected: entry.id == viewModel.selection,
                            select: { Task { await viewModel.select(entry.id) } },
                            delete: { pendingDelete = entry }
                        )
                        .disabled(viewModel.phase == .restoring)
                    }
                }
                .padding(.horizontal, 6)
            }
        }
        .background(MuxaTheme.sideBar(colorScheme))
    }

    @ViewBuilder
    private var preview: some View {
        let rows = SessionSnapshotTree.rows(plan: viewModel.plan)
        VStack(alignment: .leading, spacing: 0) {
            MuxaSectionTitle(title: viewModel.plan?.run == true
                ? String(localized: "Restore results")
                : String(localized: "Restore plan")) {
                EmptyView()
            }
            if let entry = viewModel.selectedEntry, !entry.readable {
                Text(entry.error ?? String(localized: "This snapshot cannot be read."))
                    .font(.system(size: 12))
                    .foregroundStyle(.red)
                    .textSelection(.enabled)
                    .padding(14)
                Spacer()
            } else if rows.isEmpty {
                Spacer()
                if viewModel.selection != nil && viewModel.error == nil {
                    ProgressView().controlSize(.small).frame(maxWidth: .infinity)
                }
                Spacer()
            } else {
                if let plan = viewModel.plan, !plan.serverReachable {
                    Text("The multiplexer is not running, so every session will be created.")
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                        .padding(.horizontal, 14)
                        .padding(.bottom, 4)
                }
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 0) {
                        ForEach(rows) { row in
                            SessionSnapshotTreeRowView(row: row)
                        }
                    }
                    .padding(.horizontal, 8)
                    .padding(.bottom, 8)
                }
            }
        }
    }

    private var footer: some View {
        VStack(alignment: .leading, spacing: 8) {
            if let error = viewModel.error, viewModel.supported {
                Text(error)
                    .font(.caption)
                    .foregroundStyle(.red)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            } else if viewModel.phase == .restoring, let started = viewModel.restoreStartedAt {
                HStack(spacing: 6) {
                    ProgressView().controlSize(.small)
                    Text("Restoring…")
                    Text(timerInterval: started...Date.distantFuture, countsDown: false)
                        .monospacedDigit()
                }
                .font(.caption)
                .foregroundStyle(.secondary)
            } else if let status = viewModel.statusMessage {
                Text(status)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
            } else if let plan = viewModel.plan, !plan.run {
                Text(SessionSnapshotTree.planLine(plan))
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            HStack(spacing: 10) {
                Toggle("Rebuild layout only", isOn: $viewModel.layoutOnly)
                    .toggleStyle(.checkbox)
                    .font(.system(size: 12))
                    .disabled(viewModel.isBusy || viewModel.phase == .finished || !viewModel.supported)
                    .help("Recreate sessions, windows and directories, but leave every pane as an empty shell")
                if viewModel.isBusy && viewModel.phase != .restoring {
                    ProgressView().controlSize(.small)
                }
                Spacer()
                if viewModel.phase == .finished {
                    Button("Done", action: close)
                        .buttonStyle(.muxaPrimary)
                        .keyboardShortcut(.defaultAction)
                } else {
                    Button("Cancel", action: close)
                        .buttonStyle(.muxaGhost)
                        .disabled(viewModel.phase == .restoring)
                    Button("Restore") { Task { await viewModel.restore() } }
                        .buttonStyle(.muxaPrimary)
                        .keyboardShortcut(.defaultAction)
                        .disabled(!viewModel.canRestore)
                }
            }
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 12)
    }
}

private struct SessionSnapshotListRow: View {
    let entry: MuxSnapshotEntry
    let selected: Bool
    let select: () -> Void
    let delete: () -> Void
    @Environment(\.colorScheme) private var colorScheme
    @State private var hovering = false

    var body: some View {
        Button(action: select) {
            HStack(alignment: .top, spacing: 4) {
                VStack(alignment: .leading, spacing: 2) {
                    HStack(spacing: 6) {
                        title
                            .font(.system(size: 12, weight: .medium))
                            .lineLimit(1)
                        if entry.summary?.isAutomatic == true {
                            Text("Auto")
                                .font(.system(size: 10, weight: .medium))
                                .foregroundStyle(.secondary)
                                .padding(.horizontal, 4)
                                .background(
                                    RoundedRectangle(cornerRadius: 3)
                                        .fill(MuxaTheme.segmentTrack(colorScheme))
                                )
                        }
                        Spacer(minLength: 0)
                    }
                    if let summary = entry.summary {
                        Text(verbatim: summary.serverLabel)
                            .font(.system(size: 11))
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                            .truncationMode(.middle)
                        Text(verbatim: summary.countsLabel)
                            .font(.system(size: 11))
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                    } else {
                        Text("Unreadable")
                            .font(.system(size: 11))
                            .foregroundStyle(.red)
                    }
                }
                if hovering || selected {
                    Button(action: delete) {
                        Image(systemName: "trash")
                    }
                    .buttonStyle(.muxaIcon(size: 20))
                    .help("Delete snapshot…")
                }
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 5)
            .frame(maxWidth: .infinity, alignment: .leading)
            .background(
                RoundedRectangle(cornerRadius: MuxaTheme.controlRadius)
                    .fill(selected
                        ? MuxaTheme.selection(colorScheme)
                        : (hovering ? MuxaTheme.hover(colorScheme) : Color.clear))
            )
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .help(entry.summary?.takenAt.map { $0.formatted(date: .abbreviated, time: .standard) } ?? entry.id)
        .contextMenu {
            Button("Delete…", role: .destructive, action: delete)
        }
    }

    @ViewBuilder
    private var title: some View {
        if let date = entry.summary?.takenAt {
            Text(date, format: .relative(presentation: .named))
        } else {
            Text(verbatim: entry.id)
        }
    }
}

private struct SessionSnapshotTreeRowView: View {
    let row: SessionSnapshotTreeRow

    var body: some View {
        VStack(alignment: .leading, spacing: 1) {
            HStack(spacing: 6) {
                Image(systemName: row.symbol)
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
                    .frame(width: 14)
                Text(verbatim: row.title)
                    .font(.system(size: 12, weight: row.depth == 0 ? .semibold : .regular))
                    .lineLimit(1)
                    .truncationMode(.middle)
                if let detail = row.detail {
                    Text(verbatim: detail)
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                Spacer(minLength: 8)
                if let badge = row.badge {
                    SessionSnapshotBadge(badge: badge)
                        .help(row.help ?? "")
                }
            }
            .frame(minHeight: MuxaTheme.rowHeight)
            if let message = row.message {
                Text(verbatim: message)
                    .font(.system(size: 11))
                    .foregroundStyle(.red)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.leading, 20)
                    .padding(.bottom, 3)
            }
        }
        .padding(.leading, CGFloat(row.depth) * MuxaTheme.treeIndent + 6)
        .padding(.trailing, 6)
    }
}

private struct SessionSnapshotBadge: View {
    let badge: SessionSnapshotTreeRow.Badge

    var body: some View {
        Label {
            title
        } icon: {
            Image(systemName: symbol)
        }
        .labelStyle(.titleAndIcon)
        .font(.system(size: 10, weight: .medium))
        .foregroundStyle(color)
        .lineLimit(1)
    }

    private var title: Text {
        switch badge {
        case .willCreate: Text("Create")
        case .exists: Text("Exists — skipped")
        case .willFillMissing: Text("Exists — missing parts added")
        case .created: Text("Created")
        case .filled: Text("Missing parts added")
        case .skipped: Text("Skipped")
        case .failed: Text("Failed")
        case .resume(let agent): Text("resume \(agent)")
        case .replay(let command): Text("replay \(command)")
        case .shell: Text("shell")
        case .notReplayable: Text("not replayable")
        case .resumeByHand: Text("resume by hand")
        case .relaunched: Text("relaunched")
        case .startByHand: Text("start by hand")
        }
    }

    private var symbol: String {
        switch badge {
        case .willCreate, .willFillMissing: "plus.circle"
        case .exists, .skipped: "minus.circle"
        case .created, .filled, .relaunched: "checkmark.circle"
        case .failed: "xmark.octagon"
        case .resume: "arrow.clockwise"
        case .replay: "play"
        case .shell: "terminal"
        case .notReplayable, .startByHand, .resumeByHand: "hand.raised"
        }
    }

    private var color: Color {
        switch badge {
        case .willCreate, .willFillMissing, .resume, .replay: .accentColor
        case .created, .filled, .relaunched: .green
        case .notReplayable, .startByHand, .resumeByHand: .orange
        case .exists, .skipped, .shell: .secondary
        case .failed: .red
        }
    }
}
