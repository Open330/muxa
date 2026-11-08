import Foundation

enum MuxaMenuBarPolling {
    static func interval(pointerInside: Bool, animating: Bool, popoverShown: Bool) -> TimeInterval {
        // Keep animation/tracking smooth; idle hover detection needs at most
        // 67ms, rather than waking the run loop sixty times every second.
        animating || (pointerInside && !popoverShown) ? 1.0 / 60 : 1.0 / 15
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
}

struct MuxaMenuBarAccessibilityState: Equatable {
    let ids: [String]
    let labels: [String]
    let rects: [CGRect]
    let overflow: Int
}
