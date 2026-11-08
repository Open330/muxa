import AppKit

/// Rasterizes immediately; no deferred drawing closure retains the controller
/// or touches actor-isolated model state when the menu-bar host draws it.
@MainActor
enum MuxaMenuBarAgentRenderer {
    static let focusScale: CGFloat = 1.1
    static let textGap: CGFloat = 3
    static let trailing: CGFloat = 8
    static let maximumTextWidth: CGFloat = 76
    static var pillFont: NSFont { .monospacedDigitSystemFont(ofSize: 11, weight: .semibold) }

    static func pillWidth(_ agents: [MuxaMenuBarAgent]) -> CGFloat {
        return pillWidth(agents.map(MuxaMenuBarChip.agent))
    }

    static func pillWidth(_ agents: [MuxaMenuBarChip]) -> CGFloat {
        let textWidth = agents.map { ($0.pillLabel as NSString).size(withAttributes: [.font: pillFont]).width }.max() ?? 0
        guard textWidth > 0 else { return MuxaMenuBarGeometry.disc }
        return ceil(MuxaMenuBarGeometry.disc * focusScale + textGap + min(textWidth, maximumTextWidth) + trailing)
    }

    static func image(
        chips: [MuxaMenuBarChip], positions: [CGFloat], focus: Int?, pillWidth: CGFloat,
        lead: CGFloat, markOpacity: CGFloat, overflow: Int, selectedID: String?,
        liveIDs: Set<String>, dark: Bool, mark: String
    ) -> NSImage {
        let geometry = MuxaMenuBarGeometry(count: chips.count)
        let size = NSSize(width: ceil(geometry.width + lead + (overflow > 0 ? MuxaMenuBarGeometry.overflowWidth : 0)), height: 24)
        let image = NSImage(size: size)
        image.lockFocus()
        defer { image.unlockFocus() }
        let ink: NSColor = dark ? .white : .black
        let markRect = NSRect(x: lead + geometry.markX + 3, y: 4, width: 18, height: 16)
        NSImage(systemSymbolName: mark, accessibilityDescription: nil)?.draw(in: markRect, from: .zero, operation: .sourceOver, fraction: markOpacity)
        ink.setFill()
        markRect.fill(using: .sourceAtop)
        NSGraphicsContext.current?.cgContext.beginTransparencyLayer(auxiliaryInfo: nil)
        let paint = MuxaMenuBarPaintGeometry(count: chips.count, positions: positions, focus: focus, pillWidth: pillWidth,
            badged: Set(chips.indices.filter { chips[$0].status.needsAttention || chips[$0].unread }))
        for index in paint.drawOrder {
            let agent = chips[index]
            let face = paint.faces[index].offsetBy(dx: lead, dy: 0)
            let height = face.height
            let isPill = face.width > height
            let shape = NSBezierPath(roundedRect: face, xRadius: height / 2, yRadius: height / 2)
            let alpha: CGFloat = liveIDs.contains(agent.id) ? 1 : 0.3
            let appearance = agent.appearance
            // Clear a narrow moat from the chip layer before painting the
            // next face: overlapping avatars retain a readable silhouette.
            NSGraphicsContext.current?.compositingOperation = .clear
            NSBezierPath(roundedRect: face.insetBy(dx: -0.8, dy: -0.8), xRadius: height / 2 + 0.8, yRadius: height / 2 + 0.8).fill()
            NSGraphicsContext.current?.compositingOperation = .sourceOver
            NSGradient(starting: appearance.top.withAlphaComponent(alpha), ending: appearance.bottom.withAlphaComponent(alpha))?.draw(in: shape, angle: -45)
            NSColor.white.withAlphaComponent(0.18 * alpha).setStroke()
            shape.lineWidth = 0.5
            shape.stroke()
            NSGraphicsContext.saveGraphicsState()
            shape.addClip()
            drawSymbol(appearance.symbol, in: NSRect(x: face.minX + 3, y: face.midY - 6, width: height - 6, height: 12), color: .white.withAlphaComponent(alpha))
            if isPill {
                let text = agent.pillLabel as NSString
                let textSize = text.size(withAttributes: [.font: pillFont])
                let textX = face.minX + height + textGap
                let room = face.maxX - trailing - textX
                if room > 0 {
                    NSRect(x: textX, y: face.minY, width: room, height: height).clip()
                    text.draw(at: NSPoint(x: textX, y: face.midY - textSize.height / 2), withAttributes: [.font: pillFont, .foregroundColor: NSColor.white.withAlphaComponent(alpha)])
                }
            }
            NSGraphicsContext.restoreGraphicsState()
            if selectedID == agent.id {
                ink.setStroke()
                let ring = NSBezierPath(roundedRect: face.insetBy(dx: -1, dy: -1), xRadius: height / 2, yRadius: height / 2)
                ring.lineWidth = 1.5
                ring.stroke()
            }
        }
        // Same order and badge rects as the hit-test model.
        for index in paint.drawOrder {
            if let rect = paint.badges[index] { drawBadge(chips[index], in: rect.offsetBy(dx: lead, dy: 0), dark: dark) }
        }
        NSGraphicsContext.current?.cgContext.endTransparencyLayer()
        if overflow > 0 {
            let text = "+\(overflow)" as NSString
            let attributes: [NSAttributedString.Key: Any] = [.font: NSFont.monospacedDigitSystemFont(ofSize: 10, weight: .semibold), .foregroundColor: ink]
            let x = lead + geometry.width
            let capsule = NSRect(x: x + 1, y: 5, width: MuxaMenuBarGeometry.overflowWidth - 2, height: 14)
            ink.withAlphaComponent(0.1).setFill()
            NSBezierPath(roundedRect: capsule, xRadius: 7, yRadius: 7).fill()
            let textWidth = text.size(withAttributes: attributes).width
            text.draw(at: NSPoint(x: capsule.midX - textWidth / 2, y: 6), withAttributes: attributes)
        }
        return image
    }

    private static func drawSymbol(_ name: String, in rect: NSRect, color: NSColor) {
        guard let source = NSImage(systemSymbolName: name, accessibilityDescription: nil) else { return }
        let configured = source.withSymbolConfiguration(.init(pointSize: 11, weight: .semibold)) ?? source
        let scale = min(rect.width / configured.size.width, rect.height / configured.size.height)
        let size = NSSize(width: configured.size.width * scale, height: configured.size.height * scale)
        let tinted = NSImage(size: size)
        tinted.lockFocus()
        let bounds = NSRect(origin: .zero, size: size)
        configured.draw(in: bounds, from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: true, hints: nil)
        color.setFill()
        bounds.fill(using: .sourceAtop)
        tinted.unlockFocus()
        tinted.draw(in: NSRect(x: rect.midX - size.width / 2, y: rect.midY - size.height / 2, width: size.width, height: size.height))
    }

    private static func drawBadge(_ agent: MuxaMenuBarChip, in rect: NSRect, dark: Bool) {
        let background: NSColor = dark ? NSColor(white: 0.12, alpha: 1) : .white
        background.setFill()
        NSBezierPath(ovalIn: rect.insetBy(dx: -0.8, dy: -0.8)).fill()
        let fill: NSColor
        let glyph: String
        switch agent.status {
        case .error: fill = NSColor(srgbRed: 0.77, green: 0.16, blue: 0.22, alpha: 1); glyph = "exclamationmark"
        case .needsInput: fill = NSColor(srgbRed: 0.96, green: 0.73, blue: 0.22, alpha: 1); glyph = "questionmark"
        case .limited: fill = NSColor(srgbRed: 0.49, green: 0.30, blue: 0.68, alpha: 1); glyph = "hourglass"
        default: fill = NSColor(srgbRed: 0.16, green: 0.53, blue: 0.33, alpha: 1); glyph = "checkmark"
        }
        fill.setFill()
        NSBezierPath(ovalIn: rect).fill()
        let color: NSColor = agent.status.needsAttention && glyph == "questionmark" ? NSColor(white: 0.12, alpha: 1) : .white
        drawSymbol(glyph, in: rect.insetBy(dx: 1.5, dy: 1.4), color: color)
    }

}
