import AppKit
import Testing
@testable import Tend

@Test func brandMarkLightsOneRoomPerStateKind() {
    let mark = TendBrandMark(statuses: [.working, .working, .needsInput("waiting_input"), .done])
    #expect(mark == TendBrandMark(waiting: true, error: false, working: true, done: true))
}

@Test func brandMarkIsQuietWithNoAgents() {
    #expect(TendBrandMark(statuses: [TendAgentStatus]()) == .quiet)
    #expect(TendBrandMark(statuses: [.idle, .other("pending")]) == .quiet)
}

@Test func brandMarkCountsOnlyUncoveredLimitsAsWaiting() {
    let covered = TendAgentStatus.limited(TendRateLimitCap(scope: .fiveHour, until: nil, covered: true))
    let uncovered = TendAgentStatus.limited(TendRateLimitCap(scope: .fiveHour, until: nil, covered: false))
    #expect(TendBrandMark(statuses: [covered]) == .quiet)
    #expect(TendBrandMark(statuses: [uncovered]).waiting)
}

@Test func brandMarkErrorRoomIsSeparateFromWaiting() {
    let mark = TendBrandMark(statuses: [.error])
    #expect(mark.error && !mark.waiting && !mark.working && !mark.done)
}

@Test func brandMarkDrawsLitRoomsInTheirColours() throws {
    let size = 56
    let rep = try #require(NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size, bitsPerSample: 8,
        samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0))
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    TendBrandMark(waiting: true, error: true, working: false, done: true)
        .draw(in: NSRect(x: 0, y: 0, width: size, height: size), ink: .black)
    NSGraphicsContext.restoreGraphicsState()
    // Sample the middle of each room (design grid 4...60 maps to 0...56, y down).
    func color(_ x: Int, _ y: Int) -> NSColor? { rep.colorAt(x: x, y: y)?.usingColorSpace(.sRGB) }
    let waiting = try #require(color(40, 6))   // top right
    let error = try #require(color(8, 16))     // top left
    let working = try #require(color(44, 40))  // bottom right, dimmed
    let done = try #require(color(10, 48))     // bottom left
    #expect(waiting.redComponent > 0.9 && waiting.blueComponent < 0.3)
    #expect(error.redComponent > 0.9 && error.greenComponent < 0.45)
    #expect(done.greenComponent > 0.6 && done.redComponent < 0.4)
    #expect(working.alphaComponent < 0.4)
}
