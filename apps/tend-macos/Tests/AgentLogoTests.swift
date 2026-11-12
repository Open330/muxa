import AppKit
import Testing
@testable import Tend

@Test func everyAgentMarkResolvesToAShippedLogoAsset() throws {
    for mark in TendAgentMark.all {
        let logo = try #require(mark.logo, "\(mark.wire) has no logo")
        #expect(NSImage(named: logo.assetName) != nil, "\(logo.assetName) is missing from the asset catalog")
    }
}

@Test func logosShipAsPublishedWithNoDerivedVariants() throws {
    for logo in TendAgentLogoAsset.allCases {
        let image = try #require(NSImage(named: logo.assetName), "\(logo.assetName)")
        // Only the blossom, which OpenAI publishes in black and in white, is a
        // template; every other mark keeps its own colours.
        #expect(image.isTemplate == logo.isMonochrome, "\(logo.assetName)")
        #expect(NSImage(named: logo.assetName + "-mono") == nil, "a derived mono variant is back for \(logo)")
    }
}

@Test func agentKindsInEverySpellingFindTheirLogo() {
    #expect(TendAgentLogoAsset(kind: "claude_code") == .claude)
    #expect(TendAgentLogoAsset(kind: "claude") == .claude)
    #expect(TendAgentLogoAsset(kind: "codex") == .openai)
    #expect(TendAgentLogoAsset(kind: "gemini_cli") == .gemini)
    #expect(TendAgentLogoAsset(kind: "antigravity") == .antigravity)
    #expect(TendAgentLogoAsset(kind: "agy") == .antigravity)
    #expect(TendAgentLogoAsset(kind: "opencode") == .opencode)
    #expect(TendAgentLogoAsset(kind: "aider") == nil)
    #expect(AskProviderEngine.anthropic.logo == .anthropic)
    #expect(AskProviderEngine.apple.logo == nil)
    #expect(TendAgentMark.productName(forKind: "claude_code") == "Claude Code")
    #expect(TendAgentMark.productName(forKind: "agy") == "Antigravity")
    #expect(TendAgentMark.productName(forKind: "some_agent") == "some agent")
}

@Test func openCodeKeepsItsMinimumSizeStepsAndClearSpace() {
    // 16×20 is the smallest mark, with two quarter-width strokes of clear
    // space on every side.
    #expect(!TendAgentLogoAsset.opencode.fits(35))
    #expect(TendAgentLogoAsset.opencode.fits(36))
    #expect(TendAgentLogoAsset.openCodeMark(in: 36) == CGSize(width: 16, height: 20))
    #expect(TendAgentLogoAsset.openCodeMark(in: 46) == CGSize(width: 20, height: 25))
    for slot in stride(from: CGFloat(36), through: 120, by: 1) {
        let mark = TendAgentLogoAsset.openCodeMark(in: slot)
        #expect(mark.width * 5 == mark.height * 4 && mark.width.truncatingRemainder(dividingBy: 4) == 0)
        #expect(mark.height + mark.width <= slot, "no clear space at \(slot)")
    }
    // Every other mark draws at any size.
    for logo in TendAgentLogoAsset.allCases where logo != .opencode { #expect(logo.fits(12)) }
    // The menu bar never shrinks OpenCode below its minimum: its face is a
    // symbol on a disc.
    #expect(!TendGlyph.drawsOwnFace(TendGlyph.opencode, slot: 14))
    #expect(TendGlyph.drawsOwnFace(TendGlyph.claude, slot: 14))
}

/// Pixels that look like Claude's Clay (#D97757), and pixels that are
/// pure white, in a bitmap.
private func clayAndWhite(_ rep: NSBitmapImageRep) -> (clay: Int, white: Int) {
    var clay = 0, white = 0
    for y in 0..<rep.pixelsHigh {
        for x in 0..<rep.pixelsWide {
            guard let c = rep.colorAt(x: x, y: y)?.usingColorSpace(.sRGB), c.alphaComponent > 0.9 else { continue }
            if abs(c.redComponent - 0.851) < 0.08, abs(c.greenComponent - 0.467) < 0.08, abs(c.blueComponent - 0.341) < 0.08 { clay += 1 }
            if c.redComponent > 0.97, c.greenComponent > 0.97, c.blueComponent > 0.97 { white += 1 }
        }
    }
    return (clay, white)
}

@Test func glyphDrawsTheLogoInItsOwnColours() throws {
    let size = 24
    let rep = try #require(NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size, bitsPerSample: 8,
        samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0))
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    TendGlyph.draw(TendGlyph.claude, in: NSRect(x: 0, y: 0, width: size, height: size), color: .white)
    NSGraphicsContext.restoreGraphicsState()
    let ink = clayAndWhite(rep)
    #expect(ink.clay > size * size / 10, "the Claude face drew \(ink.clay) Clay pixels")
    #expect(ink.white == 0, "the Claude face was tinted white")
}

@Test @MainActor func menuBarImageDrawsLogosAsPublishedWithAndWithoutTheGlyphCache() throws {
    let chip = TendMenuBarChip(id: "a", name: "Claude", location: "here", pillLabel: "", menuStatusLabel: "",
        appearance: TendMenuBarAgentAppearance(identity: "a", kind: "claude_code"), status: .idle,
        unread: false, memberIDs: ["a"], destination: .agent("a"))
    for cache in [TendMenuBarGlyphCache(), nil] as [TendMenuBarGlyphCache?] {
        for dark in [false, true] {
            let image = TendMenuBarAgentRenderer.image(chips: [chip], positions: [0], focus: nil, pillWidth: 18,
                lead: 0, markOpacity: 0, overflow: 0, selectedID: nil, liveIDs: ["a"], dark: dark,
                mark: .quiet, scales: [2], glyphCache: cache)
            let rep = try #require(image.representations.first as? NSBitmapImageRep)
            let ink = clayAndWhite(rep)
            let path = cache == nil ? "uncached" : "cached"
            #expect(ink.clay > 40, "\(path) \(dark ? "dark" : "light"): \(ink.clay) Clay pixels")
            #expect(ink.white == 0, "\(path) \(dark ? "dark" : "light"): the logo was tinted white")
        }
    }
    // A host's text face reaches TendGlyph on the uncached path too.
    let text = TendMenuBarChip(id: "h", name: "mini", location: "", pillLabel: "", menuStatusLabel: "",
        appearance: TendMenuBarAgentAppearance(identity: "h", symbol: TendGlyph.text("MB"), provider: ""), status: .idle,
        unread: false, memberIDs: [], destination: .agent("h"))
    for cache in [TendMenuBarGlyphCache(), nil] as [TendMenuBarGlyphCache?] {
        let image = TendMenuBarAgentRenderer.image(chips: [text], positions: [0], focus: nil, pillWidth: 18,
            lead: 0, markOpacity: 0, overflow: 0, selectedID: nil, liveIDs: ["h"], dark: false,
            mark: .quiet, scales: [2], glyphCache: cache)
        let rep = try #require(image.representations.first as? NSBitmapImageRep)
        #expect(clayAndWhite(rep).white > 10, "\(cache == nil ? "uncached" : "cached") text face drew no text")
    }
}
