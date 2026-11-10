// TCP listeners on each host associated with tend (`tend servers`):
// under a pane where tend tracks an agent, or inside a worktree tend made.
// Read-only. An association is not proof an agent started a process, and
// nothing here says a process is safe to stop.
import SwiftUI

struct TendServerRow: Decodable, Hashable, Identifiable, Sendable {
    struct Endpoint: Decodable, Hashable, Sendable {
        let address: String
        let port: UInt16
        let bind: String
    }

    struct Association: Decodable, Hashable, Sendable {
        let kind: String
        let endpoint: String?
        let paneID: String?
        let agentKind: String?
        let path: String?
        let origin: String?

        enum CodingKeys: String, CodingKey {
            case kind, endpoint, path, origin
            case paneID = "pane_id"
            case agentKind = "agent_kind"
        }
    }

    let pid: UInt32
    let processStart: String?
    let processName: String
    let cwd: String?
    let endpoints: [Endpoint]
    let association: Association
    let status: String
    let stateForSecs: Int64?

    var id: String { "\(pid)-\(processStart ?? "")" }

    enum CodingKeys: String, CodingKey {
        case pid, cwd, endpoints, association, status
        case processStart = "process_start"
        case processName = "process_name"
        case stateForSecs = "state_for_secs"
    }

    var portsText: String {
        endpoints.map { endpoint in
            switch endpoint.bind {
            case "wildcard": return String(localized: "\(endpoint.address):\(String(endpoint.port)) (all interfaces)")
            case "interface": return String(localized: "\(endpoint.address):\(String(endpoint.port)) (non-loopback)")
            default: return "\(endpoint.address):\(endpoint.port)"
            }
        }.joined(separator: ", ")
    }

    var statusText: String {
        let span = stateForSecs.map(FleetServers.duration)
        switch status {
        case "work_completed": return String(localized: "Its Work completed")
        case "agent_stopped": return span.map { String(localized: "Agent stopped for \($0)") } ?? String(localized: "Agent stopped")
        case "agent_idle": return span.map { String(localized: "Agent idle for \($0)") } ?? String(localized: "Agent idle")
        case "agent_error": return String(localized: "Agent in error")
        case "agent_needs_input": return String(localized: "Agent needs input")
        case "agent_working": return String(localized: "Agent working")
        default: return String(localized: "Status unknown")
        }
    }

    var viaText: String {
        switch association.kind {
        case "pane":
            return String(localized: "Under \(association.agentKind ?? "agent") pane \(association.paneID ?? "?")")
        default:
            return String(localized: "In worktree \(association.origin ?? "")")
        }
    }
}

/// How much of a scan saw what it needed.
struct TendServerCoverage: Decodable, Hashable, Sendable {
    let state: String
    let reason: String?
}

struct TendServerInventory: Decodable, Hashable, Sendable {
    let rows: [TendServerRow]
    let total: Int
    let collection: TendServerCoverage
    let attribution: TendServerCoverage

    var isCut: Bool { total > rows.count }
}

enum FleetServers {
    static func duration(_ seconds: Int64) -> String {
        let formatter = DateComponentsFormatter()
        formatter.unitsStyle = .abbreviated
        formatter.maximumUnitCount = 1
        formatter.allowedUnits = [.day, .hour, .minute, .second]
        return formatter.string(from: TimeInterval(seconds)) ?? "\(seconds)s"
    }
}

struct FleetServersList: View {
    @ObservedObject var model: AppModel

    private enum HostState {
        case loading
        case rows(TendServerInventory)
        case failed(String)
    }

    @State private var states: [String: HostState] = [:]

    private var hosts: [TendFleetHost] {
        model.fleetHosts.filter { $0.state == "online" }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text("Servers still listening that tend's agents or worktrees are associated with. Association is not proof an agent started them; nothing is stopped from here.")
                    .font(TendType.meta)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                Spacer()
                Button {
                    Task { await load() }
                } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .buttonStyle(.tendGhost)
                .help("Check again")
            }
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    if hosts.isEmpty {
                        Text("No host is online.").foregroundStyle(.secondary)
                    }
                    ForEach(hosts) { host in
                        hostSection(host)
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .task { await load() }
    }

    @ViewBuilder
    private func hostSection(_ host: TendFleetHost) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(spacing: 6) {
                Image(systemName: TendHostIcons.icon(for: host.alias, local: host.local))
                    .foregroundStyle(.secondary)
                Text(host.alias).font(.system(size: 13, weight: .semibold))
                if case .rows(let inventory) = states[host.alias], inventory.collection.state == "complete" {
                    Text(String(localized: "\(inventory.total) listening"))
                        .font(TendType.meta)
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
                    .font(TendType.meta)
                    .foregroundStyle(.orange)
            case .rows(let inventory):
                if inventory.collection.state != "complete" {
                    // Never read an unsuccessful scan as "no servers".
                    Label(
                        String(localized: "Could not list servers: \(inventory.collection.reason ?? "unknown reason")"),
                        systemImage: "exclamationmark.circle"
                    )
                    .font(TendType.meta)
                    .foregroundStyle(.orange)
                } else {
                    if inventory.attribution.state != "complete", let reason = inventory.attribution.reason {
                        Label(reason, systemImage: "info.circle")
                            .font(TendType.meta)
                            .foregroundStyle(.secondary)
                    }
                    if inventory.rows.isEmpty {
                        Text("No listening servers associated with tend on this host.")
                            .font(TendType.meta)
                            .foregroundStyle(.secondary)
                    }
                    if inventory.isCut {
                        Label(
                            String(localized: "Showing \(inventory.rows.count) of \(inventory.total). Run tend servers on \(host.alias) for all."),
                            systemImage: "info.circle"
                        )
                        .font(TendType.meta)
                        .foregroundStyle(.secondary)
                    }
                    ForEach(inventory.rows) { row in
                        serverRow(row)
                    }
                }
            }
        }
    }

    private func serverRow(_ row: TendServerRow) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 8) {
                Text(row.statusText)
                    .font(.system(size: 10, weight: .semibold))
                    .padding(.horizontal, 6)
                    .padding(.vertical, 1)
                    .background(Color.secondary.opacity(0.15), in: Capsule())
                    .foregroundStyle(.secondary)
                Text(verbatim: row.processName)
                    .font(TendType.body.weight(.medium))
                    .lineLimit(1)
                Text(verbatim: row.portsText)
                    .font(TendType.meta)
                    .monospacedDigit()
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer(minLength: 6)
                Text(verbatim: "pid \(row.pid)")
                    .font(TendType.meta)
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
            }
            Text(row.viaText)
                .font(TendType.meta)
                .foregroundStyle(.secondary)
            if let cwd = row.cwd {
                Text(verbatim: cwd)
                    .font(TendType.meta)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Color.secondary.opacity(0.06), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        .textSelection(.enabled)
    }

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
                        state = .rows(try await client.fleetServers(host: identity))
                    } catch {
                        state = .failed(error.localizedDescription)
                    }
                    await MainActor.run { states[alias] = state }
                }
            }
        }
    }
}
