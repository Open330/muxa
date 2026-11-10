import Foundation

enum MuxaMenuBarPolling {
    static func interval(pointerInside: Bool, animating: Bool, popoverShown: Bool, waitingToFold: Bool = false) -> TimeInterval {
        // Keep animation/tracking smooth; idle hover detection needs at most
        // 67ms, rather than waking the run loop sixty times every second.
        animating || waitingToFold || (pointerInside && !popoverShown) ? 1.0 / 60 : 1.0 / 15
    }

    static func refreshUsage(chipShown: Bool, lastAttempt: Date, lastSuccess: Date?, now: Date) -> Bool {
        chipShown && now.timeIntervalSince(max(lastAttempt, lastSuccess ?? .distantPast)) >= 120
    }
}

@MainActor
struct MuxaMenuBarRenderState: Equatable {
    let chips: [MuxaMenuBarChip]
    let positions: [CGFloat]
    let focus: Int?
    let pillWidth: CGFloat
    let lead: CGFloat
    let markOpacity: CGFloat
    let overflow: Int
    let selectedID: String?
    let liveIDs: Set<String>
    let dark: Bool
    let mark: String
    let backingScales: [CGFloat]
    private let pixels: [MuxaMenuBarChipPixels]

    init(chips: [MuxaMenuBarChip], positions: [CGFloat], focus: Int?, pillWidth: CGFloat,
         lead: CGFloat, markOpacity: CGFloat, overflow: Int, selectedID: String?,
         liveIDs: Set<String>, dark: Bool, mark: String, backingScales: [CGFloat] = [1]) {
        self.chips = chips
        self.positions = positions
        self.focus = focus
        self.pillWidth = pillWidth
        self.lead = lead
        self.markOpacity = markOpacity
        self.overflow = overflow
        self.selectedID = selectedID
        self.liveIDs = liveIDs
        self.dark = dark
        self.mark = mark
        self.backingScales = backingScales
        pixels = chips.enumerated().map { index, chip in
            let badge = chip.status.needsAttention || chip.unread ? MuxaMenuBarBadgeStyle(status: chip.status) : nil
            return MuxaMenuBarChipPixels(palette: chip.appearance.paletteIndex, symbol: chip.appearance.symbol,
                          text: index == focus && pillWidth > MuxaMenuBarGeometry.disc * 1.1 ? chip.pillLabel : nil,
                          badge: badge, live: liveIDs.contains(chip.id), selected: selectedID == chip.id)
        }
    }
    nonisolated static func == (lhs: Self, rhs: Self) -> Bool {
        guard lhs.positions == rhs.positions, lhs.focus == rhs.focus,
              lhs.pillWidth == rhs.pillWidth, lhs.lead == rhs.lead,
              lhs.markOpacity == rhs.markOpacity, lhs.overflow == rhs.overflow,
              lhs.dark == rhs.dark, lhs.mark == rhs.mark,
              lhs.backingScales == rhs.backingScales else { return false }
        return lhs.pixels == rhs.pixels
    }
}

private struct MuxaMenuBarChipPixels: Equatable, Sendable {
    let palette: Int
    let symbol: String
    let text: String?
    let badge: MuxaMenuBarBadgeStyle?
    let live: Bool
    let selected: Bool
}

struct MuxaMenuBarAccessibilityState: Equatable {
    let ids: [String]
    let labels: [String]
    let rects: [CGRect]
    let overflow: Int
}

/// Screen-space hover intent. A narrow margin preserves a capsule while the
/// pointer travels toward its peek; movement within one exit zone never keeps
/// postponing the fold indefinitely.
enum MuxaMenuBarHoverPolicy {
    enum Zone: Equatable { case item, margin, peek, outside }
    static let exitGrace: TimeInterval = 0.2
    static let peekGrace: TimeInterval = 0.45
    static let unfoldDuration: TimeInterval = 0.16
    static let foldDuration: TimeInterval = 0.26

    static func zone(point: CGPoint, item: CGRect, peek: CGRect?) -> Zone {
        if item.contains(point) { return .item }
        let margin = CGRect(x: item.minX - 4, y: item.minY - 10,
                            width: item.width + 8, height: item.height + 11)
        if margin.contains(point) { return .margin }
        if let peek {
            let bridge = CGRect(x: peek.minX, y: peek.minY, width: peek.width,
                                height: max(0, item.minY - peek.minY))
            if bridge.contains(point) { return .peek }
        }
        return .outside
    }
}

struct MuxaMenuBarFoldIntent {
    private(set) var deadline: Date?
    private var grace: TimeInterval?
    mutating func cancel() { deadline = nil; grace = nil }
    mutating func schedule(now: Date, grace: TimeInterval) {
        guard deadline == nil || self.grace != grace else { return }
        self.grace = grace
        deadline = now.addingTimeInterval(grace)
    }
    mutating func consume(now: Date) -> Bool {
        guard let deadline, now >= deadline else { return false }
        cancel()
        return true
    }
}

/// A transient card can close on mouse-down, before the status item's mouse-up.
/// Suppress only that same destination; another chip must open immediately.
enum MuxaMenuBarCardInteraction {
    enum Action { case open, close, ignore }
    static func action(requested: String?, current: String?, shown: Bool,
                       closed: String?, closedAt: Date?, now: Date) -> Action {
        if shown { return requested == current ? .close : .open }
        if let closedAt, now.timeIntervalSince(closedAt) < 0.6, requested == closed { return .ignore }
        return .open
    }
}
