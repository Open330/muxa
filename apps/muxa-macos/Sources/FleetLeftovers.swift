// The worktrees muxa created on each host, and what would stop a cleanup
// from taking each. Read-only: muxa does not know yet whether a Work is
// finished, so nothing is removed from here.
import SwiftUI

/// One receipted worktree as a host reports it (`muxa worktrees --json`).
struct MuxaWorktreeRow: Decodable, Hashable, Identifiable, Sendable {
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

    let id: String
    let repo: String
    let path: String
    let branch: String?
    let origin: Origin
    let createdAt: String?
    let candidate: Bool
    let blockers: [String]
    let currentBranch: String?
    let bytes: UInt64?

    enum CodingKeys: String, CodingKey {
        case id, repo, path, branch, origin, candidate, blockers, bytes
        case createdAt = "created_at"
        case currentBranch = "current_branch"
    }

    /// The last two path components: enough to tell trees apart.
    var shortPath: String {
        let parts = path.split(separator: "/")
        return parts.suffix(2).joined(separator: "/")
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
}

/// A host's answer: the rows it sent and how many receipts it has in all.
struct MuxaWorktreeInventory: Hashable, Sendable {
    let rows: [MuxaWorktreeRow]
    let total: Int

    var isCut: Bool { total > rows.count }
}

enum FleetLeftovers {
    /// Candidates first, then the biggest.
    static func sorted(_ rows: [MuxaWorktreeRow]) -> [MuxaWorktreeRow] {
        rows.sorted {
            if $0.candidate != $1.candidate { return $0.candidate }
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
        case rows(MuxaWorktreeInventory)
        case failed(String)
    }

    @State private var states: [String: HostState] = [:]

    private var hosts: [MuxaFleetHost] {
        model.fleetHosts.filter { $0.state == "online" }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                VStack(alignment: .leading, spacing: 3) {
                    Text("Leftover worktrees")
                        .font(.system(size: 16, weight: .semibold))
                    Text("Worktrees muxa created, by host. Read-only: muxa does not know yet whether a Work is finished, so nothing is removed.")
                        .font(MuxaType.meta)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
                Spacer()
                Button {
                    Task { await load() }
                } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .buttonStyle(.muxaGhost)
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
            HStack {
                Spacer()
                Button("Close") { dismiss() }
                    .keyboardShortcut(.cancelAction)
            }
        }
        .padding(20)
        .frame(width: 620, height: 520)
        .task { await load() }
    }

    @ViewBuilder
    private func hostSection(_ host: MuxaFleetHost) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 6) {
                Image(systemName: MuxaHostIcons.icon(for: host.alias, local: host.local))
                    .foregroundStyle(.secondary)
                Text(host.alias).font(.system(size: 13, weight: .semibold))
                if case .rows(let inventory) = states[host.alias] {
                    let rows = inventory.rows
                    let candidates = rows.filter(\.candidate)
                    Text(String(localized: "\(rows.count) worktrees · \(candidates.count) with no blocker · \(FleetLeftovers.size(candidates.compactMap(\.bytes).reduce(0, +)))"))
                        .font(MuxaType.meta)
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
                Spacer()
            }
            switch states[host.alias] {
            case .none, .loading:
                ProgressView().controlSize(.small)
            case .failed(let message):
                Label(message, systemImage: "exclamationmark.circle")
                    .font(MuxaType.meta)
                    .foregroundStyle(.orange)
            case .rows(let inventory) where inventory.rows.isEmpty:
                Text("No worktrees muxa created on this host.")
                    .font(MuxaType.meta)
                    .foregroundStyle(.secondary)
            case .rows(let inventory):
                if inventory.isCut {
                    Label(
                        String(localized: "Showing \(inventory.rows.count) of \(inventory.total), those still on disk first. Run muxa worktrees on \(host.alias) for all."),
                        systemImage: "info.circle"
                    )
                    .font(MuxaType.meta)
                    .foregroundStyle(.secondary)
                }
                ForEach(FleetLeftovers.sorted(inventory.rows)) { row in
                    worktreeRow(row)
                }
            }
        }
    }

    private func worktreeRow(_ row: MuxaWorktreeRow) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 8) {
                Text(row.candidate ? "No blocker" : "Kept")
                    .font(.system(size: 10, weight: .semibold))
                    .padding(.horizontal, 6)
                    .padding(.vertical, 1)
                    .background((row.candidate ? Color.green : Color.secondary).opacity(0.15), in: Capsule())
                    .foregroundStyle(row.candidate ? Color.green : Color.secondary)
                Text(row.shortPath)
                    .font(MuxaType.body.weight(.medium))
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .help(row.path)
                Spacer(minLength: 6)
                Text(FleetLeftovers.size(row.bytes))
                    .font(MuxaType.meta)
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
            }
            Text([row.currentBranch ?? String(localized: "detached"), row.originText].joined(separator: " · "))
                .font(MuxaType.meta)
                .foregroundStyle(.secondary)
            ForEach(row.blockers, id: \.self) { blocker in
                Text("• \(blocker)")
                    .font(MuxaType.meta)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.secondary.opacity(0.06), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        .textSelection(.enabled)
    }

    /// Asks every online host at once; each fills in as it answers.
    private func load() async {
        let targets = hosts
        let client = model.client
        for host in targets { states[host.alias] = .loading }
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
