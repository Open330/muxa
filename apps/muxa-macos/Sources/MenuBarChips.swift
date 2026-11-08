import Foundation
import SwiftUI

enum MuxaMenuBarGrouping: String, CaseIterable, Identifiable {
    case agent, work, fleet
    var id: Self { self }
    var title: String {
        switch self {
        case .agent: String(localized: "By agent")
        case .work: String(localized: "By Work")
        case .fleet: String(localized: "By Fleet host")
        }
    }

    func key(for hosted: MuxaHostedAgent) -> String {
        let parts: [String]
        switch self {
        case .agent: parts = [rawValue, hosted.host.alias, hosted.agent.agentSessionID]
        case .fleet: parts = [rawValue, hosted.host.alias]
        case .work:
            if let work = hosted.pane?.workIdentity {
                parts = [rawValue, hosted.host.alias, "managed", work.workspaceID, work.workID]
            } else if let pane = hosted.pane {
                // Unmanaged sessions retain their actual window boundary;
                // equal window names or work ids on other hosts cannot merge.
                parts = [rawValue, hosted.host.alias, "window", pane.socket ?? "default", pane.sessionID, pane.windowID]
            } else {
                parts = [rawValue, hosted.host.alias, "unassigned", hosted.agent.agentSessionID]
            }
        }
        return String(data: try! JSONEncoder().encode(parts), encoding: .utf8)!
    }
}

@MainActor
struct MuxaMenuBarScope: Equatable {
    let grouping: MuxaMenuBarGrouping
    let key: String
    let title: String
    func contains(_ item: MuxaMenuBarAgent) -> Bool { grouping.key(for: item.hosted) == key }
}

/// One painted chip can lead to an individual, a real Work/host boundary, or
/// an enabled extension. Reply recipients always remain real agent sessions.
@MainActor
struct MuxaMenuBarChip: Identifiable, Equatable {
    enum Destination: Equatable { case agent(String), group(MuxaMenuBarScope), aas }
    let id: String
    let name: String
    let location: String
    let pillLabel: String
    let menuStatusLabel: String
    let appearance: MuxaMenuBarAgentAppearance
    let status: MuxaAgentStatus
    let unread: Bool
    let memberIDs: Set<String>
    let destination: Destination
    var menuStatusColor: Color { MuxaMenuBarAgentAppearance.statusColor(status, finished: unread && status == .idle) }

    static func agent(_ item: MuxaMenuBarAgent) -> Self {
        Self(id: item.id, name: item.name, location: item.location, pillLabel: item.pillLabel,
            menuStatusLabel: item.menuStatusLabel, appearance: item.appearance, status: item.status,
            unread: item.unread, memberIDs: [item.id], destination: .agent(item.id))
    }

    static func activity(_ agents: [MuxaMenuBarAgent], grouping: MuxaMenuBarGrouping) -> [Self] {
        if grouping == .agent { return agents.map(agent) }
        var order: [String] = []
        var groups: [String: [MuxaMenuBarAgent]] = [:]
        for item in agents {
            let key = grouping.key(for: item.hosted)
            if groups[key] == nil { order.append(key) }
            groups[key, default: []].append(item)
        }
        return order.compactMap { key in
            guard let members = groups[key], let first = members.first else { return nil }
            let title = grouping == .fleet ? first.hosted.host.alias
                : first.hosted.pane?.workIdentity?.workID ?? first.hosted.pane?.windowName ?? String(localized: "Unassigned Work")
            let scope = MuxaMenuBarScope(grouping: grouping, key: key, title: title)
            let count = String(localized: "\(members.count) agents")
            let location = grouping == .fleet ? count : "\(first.hosted.host.alias) › \(first.hosted.pane?.workIdentity?.workspaceID ?? first.hosted.pane?.session ?? "") · \(count)"
            return Self(id: key, name: title, location: location, pillLabel: "\(members.count) · \(first.pillLabel)",
                menuStatusLabel: first.menuStatusLabel,
                appearance: MuxaMenuBarAgentAppearance(identity: key, symbol: grouping == .fleet ? "server.rack" : "square.stack.3d.up", provider: grouping.title),
                status: first.status, unread: members.contains(where: \.unread), memberIDs: Set(members.map(\.id)), destination: .group(scope))
        }
    }

    static func aas(_ store: AasAccountStore, now: Date = .now) -> Self {
        let warning = store.error != nil || AasMenuBarUsage.hasWarning(store.accounts, now: now)
        let value = AasMenuBarUsage.peak(store.accounts, now: now).map(AasFormat.percent) ?? "—"
        return Self(id: "extension:aas", name: "AAS", location: String(localized: "Highest usage among active accounts"),
            pillLabel: value, menuStatusLabel: value,
            appearance: MuxaMenuBarAgentAppearance(identity: "extension:aas", symbol: "gauge.medium", provider: "AAS"),
            status: warning ? .error : .idle, unread: false, memberIDs: [], destination: .aas)
    }
}

struct MuxaMenuBarChipPeek: View {
    let item: MuxaMenuBarChip
    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: item.appearance.symbol).foregroundStyle(.white)
                .frame(width: 26, height: 26).background(item.appearance.gradient, in: Circle())
            VStack(alignment: .leading, spacing: 3) {
                HStack {
                    Text(verbatim: item.name).font(MuxaType.detail.weight(.semibold))
                    Text(verbatim: item.menuStatusLabel).font(MuxaType.meta).foregroundStyle(item.menuStatusColor)
                }
                Text(verbatim: item.location).font(MuxaType.meta).foregroundStyle(.secondary)
            }.lineLimit(1)
        }
        .padding(.horizontal, 12).padding(.vertical, 10).frame(maxWidth: 340, alignment: .leading)
        .fixedSize().background(.regularMaterial, in: RoundedRectangle(cornerRadius: 10))
    }
}
