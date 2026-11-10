import Foundation
import SwiftUI

/// Host + agent session, never the reusable tmux pane id. A frozen chip must
/// not silently start referring to a replacement agent in the same pane.
@MainActor
struct TendMenuBarAgent: Identifiable {
    let hosted: TendHostedAgent
    let unread: Bool
    nonisolated var id: String { hosted.id }
    var pane: TendWatchPaneIdentity? { TendAgentAttentionCenter.paneIdentity(for: hosted) }
    var name: String {
        hosted.pane?.agentAlias.map { "@\($0)" }
            ?? hosted.agent.aiTitle ?? hosted.agent.kind.replacingOccurrences(of: "_", with: " ")
    }
    var initials: String {
        let words = name.trimmingCharacters(in: CharacterSet(charactersIn: "@")).split(whereSeparator: { $0.isWhitespace || $0 == "-" || $0 == "_" })
        if words.count == 1 { return String(words[0].prefix(2)).uppercased() }
        return String(words.prefix(2).compactMap(\.first)).uppercased()
    }
    var status: TendAgentStatus { hosted.agent.status }
    var appearance: TendMenuBarAgentAppearance { TendMenuBarAgentAppearance(identity: id, kind: hosted.agent.kind) }
    var isUnreadCompletion: Bool { unread && hosted.agent.state == "idle" }
    var menuStatusSymbol: String { isUnreadCompletion ? "checkmark.circle.fill" : status.symbol }
    var menuStatusColor: Color { TendMenuBarAgentAppearance.statusColor(status, finished: isUnreadCompletion) }
    var menuStatusLabel: String { isUnreadCompletion ? String(localized: "Finished") : status.label() }
    var location: String {
        [hosted.host.alias, hosted.pane?.session, hosted.pane?.windowName]
            .compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " › ")
    }
    /// A finished, unseen turn is not a request for input. Give its capsule
    /// a useful reading while the card retains the shared detailed status.
    var pillLabel: String {
        if isUnreadCompletion { return String(localized: "Finished") }
        return status.shortLabel
    }
    var connected: Bool { hosted.host.local || ["online", "version_skew"].contains(hosted.host.state) }
    var summary: String? {
        let agent = hosted.agent
        let choices = status.needsAttention
            ? [agent.lastNotification, agent.lastResponse, agent.recap]
            : [agent.lastResponse, agent.recap, agent.lastNotification]
        return choices.compactMap { $0?.trimmingCharacters(in: .whitespacesAndNewlines) }.first { !$0.isEmpty }
    }
}

/// Ranks by local observation order, not clocks from different fleet hosts.
/// Items discovered in one snapshot share a rank and use a stable id tie-break.
@MainActor
struct TendMenuBarAgentOrder {
    private var stamps: [String: String] = [:]
    private var ranks: [String: UInt64] = [:]
    private var revision: UInt64 = 0

    mutating func update(_ hosted: [TendHostedAgent], unread: Set<TendWatchPaneIdentity>) -> [TendMenuBarAgent] {
        revision &+= 1
        let live = Set(hosted.map(\.id))
        stamps = stamps.filter { live.contains($0.key) }
        ranks = ranks.filter { live.contains($0.key) }
        let items = hosted.map { hosted in
            let stamp = TendUnreadRules.stamp(hosted.agent)
            if stamps[hosted.id] != stamp {
                stamps[hosted.id] = stamp
                ranks[hosted.id] = revision
            }
            return TendMenuBarAgent(hosted: hosted, unread: TendAgentAttentionCenter.paneIdentity(for: hosted).map(unread.contains) ?? false)
        }
        return items.filter { $0.unread && ($0.status.needsAttention || !$0.status.isLimited) }.sorted {
            let left = $0.status.needsAttention ? 0 : 1
            let right = $1.status.needsAttention ? 0 : 1
            if left != right { return left < right }
            let a = ranks[$0.id] ?? 0, b = ranks[$1.id] ?? 0
            return a == b ? $0.id < $1.id : a > b
        }
    }
}

/// One hovered capsule, with local gaps on either side. Coordinates remain
/// relative to the folded stack: negative x is accommodated by a leading
/// extension of the status item, leaving its right edge in place.
struct TendMenuBarGeometry {
    let count: Int
    init(count: Int) { self.count = max(0, count) }
    static let slot: CGFloat = 26
    static let step: CGFloat = 13
    static let reveal: CGFloat = 21
    static let disc: CGFloat = 18
    static let inset: CGFloat = (slot - disc) / 2
    static let markWidth: CGFloat = 24
    static let gap: CGFloat = 4
    static let overflowWidth: CGFloat = 30
    var stackWidth: CGFloat { count == 0 ? 0 : CGFloat(count - 1) * Self.step + Self.slot }
    var markX: CGFloat { count == 0 ? 0 : stackWidth + Self.gap }
    var width: CGFloat { markX + Self.markWidth }

    /// Use the available menu-bar budget, reserving overflow only when needed.
    /// The app mark remains reachable even when there are no unseen agents.
    static func visibleAgentCount(total: Int, maximumWidth: CGFloat, maximumAgents: Int = .max) -> Int {
        let capped = min(total, max(0, maximumAgents))
        guard capped > 0 else { return 0 }
        if capped == total, Self(count: total).width <= maximumWidth { return total }
        let space = maximumWidth - markWidth - gap - overflowWidth - slot
        return min(capped, max(0, Int(floor(space / step)) + 1))
    }

    func hidesMark(_ focus: Int?) -> Bool { focus.map { (0..<count).contains($0) } == true }

    func layout(focus: Int?, pillWidth: CGFloat) -> [CGFloat] {
        guard hidesMark(focus), let focus else { return (0..<count).map { CGFloat($0) * Self.step } }
        let pill = max(Self.disc, pillWidth)
        var positions: [CGFloat] = []
        var x: CGFloat = 0
        for index in 0..<count {
            positions.append(x)
            if index == focus - 1 { x += Self.reveal }
            else if index == focus { x += pill + Self.reveal - Self.disc }
            else { x += Self.step }
        }
        let end = focus == count - 1 ? positions[focus] + pill + 2 * Self.inset : positions[count - 1] + Self.slot
        // Cover the chip's former place, including a stationary pointer at
        // either side of the visible folded slice.
        let cover = CGFloat(focus) * Self.step - positions[focus]
        let shift = min(0, width - end, cover)
        return positions.map { $0 + shift }
    }

    /// One extension for all chips, independent of the hovered agent.
    func hoverLead(pillWidth: CGFloat) -> CGFloat {
        let lead = (0..<count).map { -(layout(focus: $0, pillWidth: pillWidth).first ?? 0) }.max() ?? 0
        return (max(0, lead) / 4).rounded(.up) * 4
    }

    /// Hit-test in paint order: the focused capsule, then discs from front
    /// to back, then the nearest painted shape for a gap. This intentional
    /// hysteresis keeps the capsule selected while the pointer is on it.
    func index(at x: CGFloat, focus: Int?, pillWidth: CGFloat) -> Int? {
        guard count > 0 else { return nil }
        let positions = layout(focus: focus, pillWidth: pillWidth)
        guard hidesMark(focus), let focus else {
            guard x >= 0, x < stackWidth else { return nil }
            if let front = (0..<count).first(where: {
                let left = positions[$0] + Self.inset
                return left <= x && x <= left + Self.disc
            }) { return front }
            return (0..<count).min {
                abs(x - (positions[$0] + Self.slot / 2)) < abs(x - (positions[$1] + Self.slot / 2))
            }
        }
        let pill = max(Self.disc, pillWidth)
        let end = focus == count - 1 ? positions[focus] + pill + 2 * Self.inset : positions[count - 1] + Self.slot
        guard x >= min(positions[0], 0), x < max(end, stackWidth) else { return nil }
        func span(_ index: Int) -> ClosedRange<CGFloat> {
            let start = positions[index] + Self.inset
            return start...(start + (index == focus ? pill : Self.disc))
        }
        if span(focus).contains(x) { return focus }
        if let front = (0..<count).first(where: { span($0).contains(x) }) { return front }
        func distance(_ index: Int) -> CGFloat {
            let range = span(index)
            return x < range.lowerBound ? range.lowerBound - x : x - range.upperBound
        }
        return (0..<count).min { distance($0) < distance($1) }
    }

    /// The status button may add two points beyond the requested image.
    static func origin(buttonWidth: CGFloat, imageWidth: CGFloat, lead: CGFloat) -> CGFloat {
        ((buttonWidth - imageWidth) / 2).rounded() + lead
    }
}

/// The exact painted faces/badges, shared by rasterization and pointer routing.
/// Positions and pill width may be mid-animation. Coordinates are relative to
/// the folded stack; y is measured from the bottom of the 24pt image.
struct TendMenuBarPaintGeometry {
    let faces: [CGRect]
    let badges: [Int: CGRect]
    let drawOrder: [Int]
    private let bounds: ClosedRange<CGFloat>

    init(count: Int, positions: [CGFloat], focus: Int?, pillWidth: CGFloat, badged: Set<Int>) {
        let geometry = TendMenuBarGeometry(count: count)
        let focus = focus.flatMap { (0..<geometry.count).contains($0) ? $0 : nil }
        var order = Array((0..<geometry.count).reversed())
        if let focus { order.removeAll { $0 == focus }; order.append(focus) }
        drawOrder = order
        faces = (0..<geometry.count).map { index in
            let x = positions.indices.contains(index) ? positions[index] : CGFloat(index) * TendMenuBarGeometry.step
            let height = TendMenuBarGeometry.disc * (index == focus ? 1.1 : 1)
            let pill = index == focus && pillWidth > height
            return CGRect(x: x + (pill ? TendMenuBarGeometry.inset : (TendMenuBarGeometry.slot - height) / 2), y: (24 - height) / 2, width: pill ? pillWidth : height, height: height)
        }
        var badgeRects: [Int: CGRect] = [:]
        for index in badged where faces.indices.contains(index) {
            let face = faces[index]
            badgeRects[index] = CGRect(x: face.maxX - 5.5, y: face.maxY - 6, width: 7, height: 7)
        }
        badges = badgeRects
        bounds = min(0, (faces.map(\.minX).min() ?? 0) - TendMenuBarGeometry.inset)...max(geometry.stackWidth, (faces.map(\.maxX).max() ?? 0) + TendMenuBarGeometry.inset)
    }

    private static func contains(_ point: CGPoint, in rect: CGRect) -> Bool {
        guard rect.contains(point) else { return false }
        let radius = rect.height / 2
        let centerX = min(max(point.x, rect.minX + radius), rect.maxX - radius)
        return pow(point.x - centerX, 2) + pow(point.y - rect.midY, 2) <= radius * radius
    }

    func index(at point: CGPoint) -> Int? {
        // Match each painted layer, front to back. A covered badge must
        // never steal a click from the face visibly painted over it.
        for index in drawOrder.reversed() {
            if let badge = badges[index], Self.contains(point, in: badge.insetBy(dx: -0.8, dy: -0.8)) { return index }
            if Self.contains(point, in: faces[index].insetBy(dx: -0.8, dy: -0.8)) { return index }
        }
        guard !faces.isEmpty, bounds.contains(point.x) else { return nil }
        // Full menu-bar height remains a target; gaps choose the nearest
        // painted face rather than a slot beginning before that face.
        if let index = drawOrder.reversed().first(where: { faces[$0].minX <= point.x && point.x <= faces[$0].maxX }) { return index }
        func distance(_ index: Int) -> CGFloat {
            let face = faces[index]
            return max(face.minX - point.x, point.x - face.maxX, 0)
        }
        return drawOrder.min { distance($0) < distance($1) }
    }
}
