import AppKit

/// The Tend mark: one square split by a canted passage into four rooms.
///
/// Each room stands for one kind of agent state and lights when at least
/// one agent in the fleet is in that state; the other rooms stay dimmed.
/// The geometry and colours match `assets/brand/tend/mark-rooms.svg` and
/// `assets/brand/tend/STATES.md`.
struct TendBrandMark: Equatable, Sendable {
    var waiting = false
    var error = false
    var working = false
    var done = false

    /// Nothing to report: every room dimmed.
    static let quiet = TendBrandMark()

    init(waiting: Bool = false, error: Bool = false, working: Bool = false, done: Bool = false) {
        self.waiting = waiting
        self.error = error
        self.working = working
        self.done = done
    }

    /// Lights the rooms for the states present among `statuses`. A rate
    /// limit counts as waiting only when no limit policy will act on it,
    /// the same rule `TendAgentStatus.needsAttention` uses.
    init(statuses: some Sequence<TendAgentStatus>) {
        for status in statuses {
            switch status {
            case .error: error = true
            case .needsInput: waiting = true
            case .limited(let cap): if !cap.covered { waiting = true }
            case .working: working = true
            case .done: done = true
            case .idle, .other: break
            }
        }
    }

    enum Room: CaseIterable { case waiting, error, working, done }

    func isLit(_ room: Room) -> Bool {
        switch room {
        case .waiting: waiting
        case .error: error
        case .working: working
        case .done: done
        }
    }

    static let waitingColor = NSColor(srgbRed: 1, green: 0.667, blue: 0.169, alpha: 1)   // #FFAA2B
    static let errorColor = NSColor(srgbRed: 1, green: 0.353, blue: 0.235, alpha: 1)     // #FF5A3C
    static let doneColor = NSColor(srgbRed: 0.247, green: 0.749, blue: 0.518, alpha: 1)  // #3FBF84

    /// Draws the mark into `rect` of the current graphics context. Working
    /// lights in `ink`; dimmed rooms use `ink` at `dimAlpha`.
    func draw(in rect: NSRect, ink: NSColor, opacity: CGFloat = 1, dimAlpha: CGFloat = 0.26) {
        let flipped = NSGraphicsContext.current?.isFlipped ?? false
        for room in Room.allCases {
            let color: NSColor
            if isLit(room) {
                switch room {
                case .waiting: color = Self.waitingColor
                case .error: color = Self.errorColor
                case .done: color = Self.doneColor
                case .working: color = ink
                }
                color.withAlphaComponent(color.alphaComponent * opacity).setFill()
            } else {
                ink.withAlphaComponent(dimAlpha * opacity).setFill()
            }
            Self.path(room, in: rect, flipped: flipped).fill()
        }
    }

    private enum Segment { case move(CGPoint), line(CGPoint), quad(control: CGPoint, to: CGPoint) }

    /// Room outlines in the 64-unit design grid (the square spans 4...60, y down).
    private static func outline(_ room: Room) -> [Segment] {
        func p(_ x: CGFloat, _ y: CGFloat) -> CGPoint { CGPoint(x: x, y: y) }
        switch room {
        case .waiting: // top right, small
            return [.move(p(26.266, 4)), .line(p(57, 4)), .quad(control: p(60, 4), to: p(60, 7)),
                    .line(p(60, 17.25)), .line(p(27.685, 17.25))]
        case .error: // top left
            return [.move(p(4, 7)), .quad(control: p(4, 4), to: p(7, 4)), .line(p(20.734, 4)),
                    .line(p(24.082, 35.25)), .line(p(4, 35.25))]
        case .working: // bottom right, large
            return [.move(p(28.275, 22.75)), .line(p(60, 22.75)), .line(p(60, 57)),
                    .quad(control: p(60, 60), to: p(57, 60)), .line(p(32.266, 60))]
        case .done: // bottom left
            return [.move(p(4, 40.75)), .line(p(24.672, 40.75)), .line(p(26.734, 60)),
                    .line(p(7, 60)), .quad(control: p(4, 60), to: p(4, 57))]
        }
    }

    private static func path(_ room: Room, in rect: NSRect, flipped: Bool) -> NSBezierPath {
        let scale = min(rect.width, rect.height) / 56
        func map(_ point: CGPoint) -> CGPoint {
            let x = rect.minX + (point.x - 4) * scale
            let y = flipped ? rect.minY + (point.y - 4) * scale : rect.maxY - (point.y - 4) * scale
            return CGPoint(x: x, y: y)
        }
        let path = NSBezierPath()
        var current = CGPoint.zero
        for segment in outline(room) {
            switch segment {
            case .move(let point):
                current = point
                path.move(to: map(point))
            case .line(let point):
                current = point
                path.line(to: map(point))
            case .quad(let control, let to):
                // An exact cubic for the quadratic corner.
                let c1 = CGPoint(x: current.x + 2 / 3 * (control.x - current.x), y: current.y + 2 / 3 * (control.y - current.y))
                let c2 = CGPoint(x: to.x + 2 / 3 * (control.x - to.x), y: to.y + 2 / 3 * (control.y - to.y))
                path.curve(to: map(to), controlPoint1: map(c1), controlPoint2: map(c2))
                current = to
            }
        }
        path.close()
        return path
    }
}
