import AppKit
import Testing
@testable import Tend

@Test func everyAgentMarkResolvesToShippedLogoAssets() throws {
    for mark in TendAgentMark.all {
        let logo = try #require(mark.logo, "\(mark.wire) has no logo")
        #expect(NSImage(named: logo.colorName) != nil, "\(logo.colorName) is missing from the asset catalog")
        let mono = try #require(NSImage(named: logo.monoName), "\(logo.monoName) is missing from the asset catalog")
        #expect(mono.isTemplate, "\(logo.monoName) should render as a template")
    }
}

@Test func everyLogoAssetShips() {
    for logo in TendAgentLogoAsset.allCases {
        #expect(NSImage(named: logo.colorName) != nil, "\(logo.colorName)")
        #expect(NSImage(named: logo.monoName) != nil, "\(logo.monoName)")
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

@Test func menuBarFacesDrawTheMonoLogo() throws {
    let size = 24
    let rep = try #require(NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size, bitsPerSample: 8,
        samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0))
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    TendGlyph.draw(TendGlyph.claude, in: NSRect(x: 0, y: 0, width: size, height: size), color: .white)
    NSGraphicsContext.restoreGraphicsState()
    var inked = 0
    for y in 0..<size { for x in 0..<size where (rep.colorAt(x: x, y: y)?.alphaComponent ?? 0) > 0.5 { inked += 1 } }
    #expect(inked > size * size / 8, "the Claude face drew \(inked) pixels")
}
