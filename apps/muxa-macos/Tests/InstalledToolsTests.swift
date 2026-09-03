import AppKit
import Foundation
import Testing
@testable import Muxa

@Test func installedToolsMergesPathEntriesWithoutDuplicates() {
    let merged = InstalledTools.mergedDirectories(
        pathStrings: ["/opt/homebrew/bin:/usr/bin::/Users/me/.cargo/bin", "/usr/bin:/bin"],
        fallback: ["/usr/local/bin", "/opt/homebrew/bin", " ", "/bin"]
    )
    #expect(merged == ["/opt/homebrew/bin", "/usr/bin", "/Users/me/.cargo/bin", "/bin", "/usr/local/bin"])
}

@Test func installedToolsTakesTheFirstNonEmptyVersionLine() {
    #expect(InstalledTools.versionLine(from: "\n  2.1.3 (Claude Code)\nextra\n") == "2.1.3 (Claude Code)")
    #expect(InstalledTools.versionLine(from: "   \n\n") == nil)
    #expect(InstalledTools.versionLine(from: "tmux 3.5a") == "tmux 3.5a")
}

@Test func installedToolsResolvesExecutablesInSearchOrder() throws {
    let root = FileManager.default.temporaryDirectory
        .appendingPathComponent("muxa-installed-tools-\(UUID().uuidString)")
    let first = root.appendingPathComponent("first")
    let second = root.appendingPathComponent("second")
    try FileManager.default.createDirectory(at: first, withIntermediateDirectories: true)
    try FileManager.default.createDirectory(at: second, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: root) }

    let plain = first.appendingPathComponent("codex")
    try Data("not executable".utf8).write(to: plain)
    let executable = second.appendingPathComponent("codex")
    try Data("#!/bin/sh\n".utf8).write(to: executable)
    try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: executable.path)

    #expect(InstalledTools.resolve("codex", in: [first.path, second.path]) == executable.path)
    #expect(InstalledTools.resolve("claude", in: [first.path, second.path]) == nil)
}

/// The guide must close only its own window: the lookup matches the
/// identifier the tracker stamps, never whatever window happens to be key.
@Test @MainActor func onboardingLooksUpItsOwnWindowByIdentifier() {
    #expect(OnboardingPreferences.windowIdentifier == "muxa.onboarding")

    let guideWindow = NSWindow(
        contentRect: NSRect(x: 0, y: 0, width: 200, height: 200),
        styleMask: [.titled, .closable],
        backing: .buffered,
        defer: true
    )
    // A programmatically created NSWindow releases itself on close, which
    // over-releases the ARC reference and crashes the test host.
    guideWindow.isReleasedWhenClosed = false
    guideWindow.identifier = NSUserInterfaceItemIdentifier(OnboardingPreferences.windowIdentifier)
    defer { guideWindow.orderOut(nil) }

    let other = NSWindow(
        contentRect: NSRect(x: 0, y: 0, width: 200, height: 200),
        styleMask: [.titled, .closable],
        backing: .buffered,
        defer: true
    )
    other.isReleasedWhenClosed = false
    other.identifier = NSUserInterfaceItemIdentifier("muxa.main")
    defer { other.orderOut(nil) }

    #expect(OnboardingPreferences.existingWindow() === guideWindow)
}
