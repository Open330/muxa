// The worktrees tend created on each host, what would stop a cleanup from
// taking each, and the cleanup: set aside (a git move next to where it was;
// nothing is deleted), restore, and delete for good. The host checks every
// change again right before it acts (see `tend::worktree_change`).
import SwiftUI

/// One receipted worktree as a host reports it (`tend worktrees --json`).
struct TendWorktreeRow: Decodable, Hashable, Identifiable, Sendable {
    struct Origin: Decodable, Hashable, Sendable {
        let kind: String
        let workspace: String?
        let work: String?
        let dispatchID: String?

        enum CodingKeys: String, CodingKey {
            case kind, workspace, work
            case dispatchID = "dispatch_id"
        }
    }

    struct SetAside: Decodable, Hashable, Sendable {
        let dest: String
        let pins: [String]?
    }

    let id: String
    let repo: String
    let path: String
    let branch: String?
    let origin: Origin
    let createdAt: String?
    let state: String?
    let generation: UInt64?
    let setAside: SetAside?
    let candidate: Bool
    let blockers: [String]
    let currentBranch: String?
    let currentHead: String?
    let restorable: Bool?
    let bytes: UInt64?

    enum CodingKeys: String, CodingKey {
        case id, repo, path, branch, origin, state, generation, candidate, blockers, bytes, restorable
        case createdAt = "created_at"
        case setAside = "set_aside"
        case currentBranch = "current_branch"
        case currentHead = "current_head"
    }

    var isInPlace: Bool { state == nil || state == "created" }
    var isSetAside: Bool { state == "set_aside" }
    var isDeleted: Bool { state == "deleted" }

    /// Can be set aside now: in place with no blocker.
    var canSetAside: Bool { isInPlace && candidate && currentHead != nil }
    /// Can be deleted for good now: set aside with no blocker.
    var canDelete: Bool { isSetAside && candidate && currentHead != nil }
    var canRestore: Bool { isSetAside && restorable == true }

    /// Where the folder is now.
    var location: String { (isSetAside || isDeleted) ? (setAside?.dest ?? path) : path }

    /// The last two path components: enough to tell trees apart.
    var shortPath: String {
        let parts = location.split(separator: "/")
        return parts.suffix(2).joined(separator: "/")
    }

    var shortHead: String {
        guard let currentHead else { return "–" }
        return String(currentHead.prefix(12))
    }

    var originText: String {
        switch origin.kind {
        case "work":
            return String(localized: "Work \([origin.workspace, origin.work].compactMap { $0 }.joined(separator: "/"))")
        case "dispatch":
            return String(localized: "Dispatch \([origin.workspace, origin.work].compactMap { $0 }.joined(separator: "/"))")
        default:
            return origin.kind
        }
    }

    /// The request item for a change, carrying what was seen.
    var changeItem: TendWorktreeChangeItem {
        TendWorktreeChangeItem(id: id, generation: generation ?? 0, head: currentHead, branch: currentBranch)
    }
}

/// One worktree in a change request, as it was listed.
struct TendWorktreeChangeItem: Hashable, Sendable {
    let id: String
    let generation: UInt64
    let head: String?
    /// nil for a detached checkout; sent as an explicit null.
    let branch: String?

    var json: [String: Any] {
        var item: [String: Any] = ["id": id, "generation": generation]
        if let head { item["head"] = head }
        item["branch"] = branch ?? NSNull()
        return item
    }
}

/// A host's answer: the rows it sent, how many receipts it has in all, and
/// which host answered.
struct TendWorktreeInventory: Hashable, Sendable {
    let rows: [TendWorktreeRow]
    let total: Int
    let node: String?

    var isCut: Bool { total > rows.count }
}

/// What happened to one worktree in a change.
struct TendWorktreeChangeOutcome: Decodable, Hashable, Sendable {
    let id: String
    let outcome: String
    let message: String
}

enum TendWorktreeAction: String, Sendable {
    case setAside = "set_aside"
    case restore
    case delete
}

enum FleetLeftovers {
    /// Ready to set aside first, then set aside, then the biggest.
    static func sorted(_ rows: [TendWorktreeRow]) -> [TendWorktreeRow] {
        rows.sorted {
            if $0.canSetAside != $1.canSetAside { return $0.canSetAside }
            if $0.isSetAside != $1.isSetAside { return $0.isSetAside }
            if $0.isDeleted != $1.isDeleted { return !$0.isDeleted }
            return ($0.bytes ?? 0) > ($1.bytes ?? 0)
        }
    }

    static func size(_ bytes: UInt64?) -> String {
        guard let bytes else { return "–" }
        return ByteCountFormatter.string(fromByteCount: Int64(clamping: bytes), countStyle: .file)
    }
}

struct FleetLeftoversView: View {
    @ObservedObject var model: AppModel
    @Environment(\.dismiss) private var dismiss

    private enum HostState {
        case loading
        case rows(TendWorktreeInventory)
        case failed(String)
    }

    /// A change waiting for the person to confirm it.
    private struct Confirmation: Identifiable {
        let id = UUID()
        let host: TendFleetHost
        let action: TendWorktreeAction
        let rows: [TendWorktreeRow]
        let node: String?
    }

    private enum Tab: Hashable { case worktrees, servers }

    @State private var tab: Tab = .worktrees
    @State private var states: [String: HostState] = [:]
    /// Selected rows to set aside, per host.
    @State private var selected: [String: Set<String>] = [:]
    @State private var confirming: Confirmation?
    @State private var running = false
    /// The last change's result per row id.
    @State private var outcomes: [String: TendWorktreeChangeOutcome] = [:]

    private var hosts: [TendFleetHost] {
        model.fleetHosts.filter { $0.state == "online" }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("Leftovers")
                    .font(.system(size: 16, weight: .semibold))
                Spacer()
                TendSegmented(selection: $tab, options: [.worktrees, .servers]) { tab in
                    switch tab {
                    case .worktrees: Text("Worktrees")
                    case .servers: Text("Servers")
                    }
                }
                .disabled(running || confirming != nil)
            }
            if tab == .servers {
                FleetServersList(model: model)
            } else {
                worktreesBody
            }
            if let confirming {
                confirmationPanel(confirming)
            } else {
                HStack {
                    Spacer()
                    Button("Close") { dismiss() }
                        .keyboardShortcut(.cancelAction)
                }
            }
        }
        .padding(20)
        .frame(width: 660, height: 560)
        .task { await load() }
    }

    private var worktreesBody: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                VStack(alignment: .leading, spacing: 3) {
                    Text("Worktrees tend created, by host. Set aside moves a folder into .tend-set-aside next to it with git; nothing is deleted until you delete it for good.")
                        .font(TendType.meta)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                Spacer()
                Button {
                    Task { await load() }
                } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .buttonStyle(.tendGhost)
                .disabled(running)
                .help("Check again")
            }
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    if hosts.isEmpty {
                        Text("No host is online.")
                            .foregroundStyle(.secondary)
                    }
                    ForEach(hosts) { host in
                        hostSection(host)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }

    private func canChange(_ host: TendFleetHost) -> Bool {
        host.local || host.mode == "control"
    }

    @ViewBuilder
    private func hostSection(_ host: TendFleetHost) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 6) {
                Image(systemName: TendHostIcons.icon(for: host.alias, local: host.local))
                    .foregroundStyle(.secondary)
                Text(host.alias).font(.system(size: 13, weight: .semibold))
                if case .rows(let inventory) = states[host.alias] {
                    let ready = inventory.rows.filter(\.canSetAside)
                    Text(String(localized: "\(inventory.rows.count) worktrees · \(ready.count) can be set aside · \(FleetLeftovers.size(ready.compactMap(\.bytes).reduce(0, +)))"))
                        .font(TendType.meta)
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
                Spacer()
                if case .rows(let inventory) = states[host.alias], canChange(host) {
                    let chosen = inventory.rows.filter { selected[host.alias, default: []].contains($0.id) && $0.canSetAside }
                    if !chosen.isEmpty {
                        Button(String(localized: "Set Aside \(chosen.count)…")) {
                            confirming = Confirmation(host: host, action: .setAside, rows: chosen, node: inventory.node)
                        }
                        .buttonStyle(.tendSecondary)
                        .disabled(running || confirming != nil)
                    }
                }
            }
            switch states[host.alias] {
            case .none, .loading:
                ProgressView().controlSize(.small)
            case .failed(let message):
                Label(message, systemImage: "exclamationmark.circle")
                    .font(TendType.meta)
                    .foregroundStyle(.orange)
            case .rows(let inventory) where inventory.rows.isEmpty:
                Text("No worktrees tend created on this host.")
                    .font(TendType.meta)
                    .foregroundStyle(.secondary)
            case .rows(let inventory):
                if !canChange(host) {
                    Label("Observe-only host: changes are made on that host or with control access.", systemImage: "eye")
                        .font(TendType.meta)
                        .foregroundStyle(.secondary)
                }
                if inventory.isCut {
                    Label(
                        String(localized: "Showing \(inventory.rows.count) of \(inventory.total), those still on disk first. Run tend worktrees on \(host.alias) for all."),
                        systemImage: "info.circle"
                    )
                    .font(TendType.meta)
                    .foregroundStyle(.secondary)
                }
                ForEach(FleetLeftovers.sorted(inventory.rows)) { row in
                    worktreeRow(row, host: host, node: inventory.node)
                }
            }
        }
    }

    private func chip(_ row: TendWorktreeRow) -> (String, Color) {
        if row.isDeleted { return (String(localized: "Deleted"), .secondary) }
        if row.isSetAside { return (String(localized: "Set aside"), .blue) }
        if row.canSetAside { return (String(localized: "No blocker"), .green) }
        return (String(localized: "Kept"), .secondary)
    }

    private func worktreeRow(_ row: TendWorktreeRow, host: TendFleetHost, node: String?) -> some View {
        let (label, color) = chip(row)
        let changeable = canChange(host) && !running && confirming == nil
        return VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 8) {
                if row.canSetAside && canChange(host) {
                    Toggle(isOn: Binding(
                        get: { selected[host.alias, default: []].contains(row.id) },
                        set: { on in
                            if on { selected[host.alias, default: []].insert(row.id) }
                            else { selected[host.alias, default: []].remove(row.id) }
                        }
                    )) { EmptyView() }
                    .toggleStyle(.checkbox)
                    .labelsHidden()
                    .disabled(!changeable)
                    .help("Select to set aside")
                }
                Text(label)
                    .font(.system(size: 10, weight: .semibold))
                    .padding(.horizontal, 6)
                    .padding(.vertical, 1)
                    .background(color.opacity(0.15), in: Capsule())
                    .foregroundStyle(color)
                Text(row.shortPath)
                    .font(TendType.body.weight(.medium))
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .help(row.location)
                Spacer(minLength: 6)
                if row.isSetAside && canChange(host) {
                    if row.canRestore {
                        Button("Restore") {
                            confirming = Confirmation(host: host, action: .restore, rows: [row], node: node)
                        }
                        .buttonStyle(.tendGhost)
                        .disabled(!changeable)
                    }
                    if row.canDelete {
                        Button("Delete for Good…") {
                            confirming = Confirmation(host: host, action: .delete, rows: [row], node: node)
                        }
                        .buttonStyle(.tendGhost)
                        .disabled(!changeable)
                    }
                }
                Text(FleetLeftovers.size(row.bytes))
                    .font(TendType.meta)
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
            }
            Text([row.currentBranch ?? String(localized: "detached"), row.shortHead, row.originText].joined(separator: " · "))
                .font(TendType.meta)
                .foregroundStyle(.secondary)
            if let outcome = outcomes[row.id] {
                Label(outcome.message, systemImage: outcome.outcome == "done" ? "checkmark.circle" : "exclamationmark.triangle")
                    .font(TendType.meta)
                    .foregroundStyle(outcome.outcome == "done" ? Color.green : Color.orange)
            }
            if !row.isDeleted {
                ForEach(row.blockers, id: \.self) { blocker in
                    Text("• \(blocker)")
                        .font(TendType.meta)
                        .foregroundStyle(.secondary)
                }
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.secondary.opacity(0.06), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        .textSelection(.enabled)
    }

    private func confirmationPanel(_ confirmation: Confirmation) -> some View {
        let (title, note, button): (String, String, String) = switch confirmation.action {
        case .setAside:
            (
                String(localized: "Set aside \(confirmation.rows.count) worktrees on \(confirmation.host.alias)?"),
                String(localized: "Each folder moves into .tend-set-aside next to it, with its files and git data. Nothing is deleted; Restore moves the preserved folder back. Branches stay."),
                String(localized: "Set Aside")
            )
        case .restore:
            (
                String(localized: "Restore this worktree on \(confirmation.host.alias)?"),
                String(localized: "Moves the preserved folder back to where it was."),
                String(localized: "Restore")
            )
        case .delete:
            (
                String(localized: "Delete this worktree for good on \(confirmation.host.alias)?"),
                String(localized: "Deletes the folder with git. Its local files and worktree-specific git data cannot be restored; only its last commit stays in the repository, under refs/tend/set-aside/."),
                String(localized: "Delete for Good")
            )
        }
        return VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.system(size: 13, weight: .semibold))
            Text(note)
                .font(TendType.meta)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            ScrollView {
                VStack(alignment: .leading, spacing: 2) {
                    ForEach(confirmation.rows) { row in
                        Text(verbatim: "\(row.location)  \(row.currentBranch ?? String(localized: "detached")) \(row.shortHead)")
                            .font(.system(size: 11, design: .monospaced))
                            .lineLimit(1)
                            .truncationMode(.middle)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .frame(maxHeight: 90)
            HStack {
                if running { ProgressView().controlSize(.small) }
                Spacer()
                Button("Cancel") { confirming = nil }
                    .keyboardShortcut(.cancelAction)
                    .disabled(running)
                Button(button, role: confirmation.action == .delete ? .destructive : nil) {
                    Task { await apply(confirmation) }
                }
                .buttonStyle(.tendPrimary)
                .disabled(running)
            }
        }
        .padding(12)
        .background(Color.secondary.opacity(0.08), in: RoundedRectangle(cornerRadius: 10, style: .continuous))
    }

    private func apply(_ confirmation: Confirmation) async {
        running = true
        defer {
            running = false
            confirming = nil
        }
        let host = confirmation.host
        do {
            let results = try await model.client.fleetChangeWorktrees(
                host: host.identity,
                action: confirmation.action,
                items: confirmation.rows.map(\.changeItem),
                node: confirmation.node
            )
            for result in results { outcomes[result.id] = result }
        } catch {
            // A lost or late answer is not a refusal: the host may have
            // made the change. Checking again shows where each one is.
            let message = String(localized: "No answer from \(host.alias) (\(error.localizedDescription)). It may have finished; check again to see.")
            for row in confirmation.rows {
                outcomes[row.id] = TendWorktreeChangeOutcome(id: row.id, outcome: "uncertain", message: message)
            }
        }
        selected[host.alias] = []
        await load(only: host)
    }

    /// Asks every online host (or one) at once; each fills in as it answers.
    private func load(only: TendFleetHost? = nil) async {
        let targets = only.map { [$0] } ?? hosts
        let client = model.client
        for host in targets {
            states[host.alias] = .loading
            selected[host.alias] = []
        }
        if only == nil { outcomes = [:] }
        await withTaskGroup(of: Void.self) { group in
            for host in targets {
                let alias = host.alias
                let identity = host.identity
                group.addTask {
                    let state: HostState
                    do {
                        state = .rows(try await client.fleetWorktrees(host: identity))
                    } catch {
                        state = .failed(error.localizedDescription)
                    }
                    await MainActor.run { states[alias] = state }
                }
            }
        }
    }
}
