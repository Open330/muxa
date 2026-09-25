import Foundation
import Testing
@testable import Muxa

// MARK: - What's New

private func release(_ version: String, highlights count: Int = 1) -> MuxaWhatsNew.Release {
    MuxaWhatsNew.Release(
        version: version,
        highlights: (0..<count).map {
            MuxaWhatsNew.Highlight(systemImage: "star", title: "\(version) #\($0)", detail: "")
        }
    )
}

@Test func whatsNewListsReleasesSinceTheRecordedVersionNewestFirst() {
    let releases = [release("0.8.51"), release("0.8.53"), release("0.8.52"), release("0.9.0")]

    let pending = MuxaWhatsNew.releases(after: "0.8.51", through: "0.8.53", in: releases)
    #expect(pending.map(\.version) == ["0.8.53", "0.8.52"])
    // Never a release newer than the running build.
    #expect(!pending.contains { $0.version == "0.9.0" })
    // Capped so a long absence stays a short list.
    #expect(MuxaWhatsNew.releases(after: "0.1.0", through: "0.9.0", in: releases, limit: 2).map(\.version)
        == ["0.9.0", "0.8.53"])
    #expect(MuxaWhatsNew.releases(after: "0.8.53", through: "0.8.53", in: releases).isEmpty)
}

@Test func whatsNewSkipsReleasesWithoutHighlights() {
    let releases = [release("0.8.53", highlights: 0), release("0.8.52")]
    #expect(MuxaWhatsNew.releases(after: "0.8.50", through: "0.8.53", in: releases).map(\.version) == ["0.8.52"])
    #expect(MuxaWhatsNew.releasesToShow(for: "0.8.53", in: releases).map(\.version) == ["0.8.52"])
    #expect(MuxaWhatsNew.releasesToShow(for: "0.8.51", in: releases).isEmpty)
}

@Test func whatsNewShipsTheSeededRelease() throws {
    let seeded = try #require(MuxaWhatsNew.catalog.first { $0.version == "0.8.53" })
    #expect(seeded.highlights.count == 6)
    #expect(Set(seeded.highlights.map(\.id)).count == seeded.highlights.count)
    #expect(OnboardingPreferences.launchPresentation(currentVersion: "0.8.53", completedVersion: "0.8.52")
        == .whatsNew(version: "0.8.53"))
}

// MARK: - Actionable checklist

@Test func checklistOffersInstallCommandsOnlyForWhatIsMissing() {
    #expect(OnboardingChecklist.installCommands(for: nil).tmux == nil)
    #expect(OnboardingChecklist.installCommands(for: nil).agents.isEmpty)

    let nothing = OnboardingChecklist.installCommands(for: [])
    #expect(nothing.tmux?.command == "brew install tmux")
    #expect(nothing.agents.map(\.program) == ["claude", "codex", "gemini", "opencode"])

    let ready = OnboardingChecklist.installCommands(for: [
        InstalledTool(name: "tmux", path: "/opt/homebrew/bin/tmux", version: nil),
        InstalledTool(name: "codex", path: "/opt/homebrew/bin/codex", version: nil),
    ])
    #expect(ready.tmux == nil)
    #expect(ready.agents.isEmpty)
    // Every offered install names an agent the checklist knows about.
    #expect(OnboardingChecklist.agentInstalls.allSatisfy { InstalledTools.agentPrograms.contains($0.program) })
}

@Test func checklistReadsCodexSignInFromItsAuthFile() {
    let home = "/Users/me"
    let signedIn: (String) -> Bool = { $0 == "/Users/me/.codex/auth.json" }

    #expect(OnboardingChecklist.signInStatus(program: "codex", home: home, environment: [:], fileExists: signedIn) == .ready)
    #expect(OnboardingChecklist.signInStatus(program: "codex", home: home, environment: [:]) { _ in false } == .attention)
    // CODEX_HOME moves the file.
    #expect(OnboardingChecklist.signInStatus(
        program: "codex", home: home, environment: ["CODEX_HOME": "/srv/codex"]
    ) { $0 == "/srv/codex/auth.json" } == .ready)
    // Providers whose sign-in cannot be read from a file are never guessed.
    for program in ["claude", "gemini", "agy", "opencode"] {
        #expect(OnboardingChecklist.signInStatus(program: program, home: home, environment: [:]) { _ in false } == nil)
    }
    #expect(OnboardingChecklist.signInCommand(program: "codex")?.command == "codex login")
    #expect(OnboardingChecklist.signInCommand(program: "claude") == nil)
}

// MARK: - First agent

@Test func firstAgentExplainsTheFirstThingInTheWay() {
    typealias First = OnboardingFirstAgent
    #expect(First.blocker(isConnected: false, supportsAgentStart: true, installed: [.claude], folderExists: true) == .notConnected)
    #expect(First.blocker(isConnected: true, supportsAgentStart: nil, installed: [.claude], folderExists: true) == .checking)
    #expect(First.blocker(isConnected: true, supportsAgentStart: true, installed: nil, folderExists: true) == .checking)
    #expect(First.blocker(isConnected: true, supportsAgentStart: false, installed: [.claude], folderExists: true) == .daemonTooOld)
    #expect(First.blocker(isConnected: true, supportsAgentStart: true, installed: [], folderExists: true) == .noAgentCLI)
    #expect(First.blocker(isConnected: true, supportsAgentStart: true, installed: [.codex], folderExists: false) == .missingFolder)
    #expect(First.blocker(isConnected: true, supportsAgentStart: true, installed: [.codex], folderExists: true) == nil)
}

@Test func firstAgentDefaultsToTheInstalledDefaultAgentAndWorkFolder() {
    let detected = [
        InstalledTool(name: "tmux", path: "/opt/homebrew/bin/tmux", version: nil),
        InstalledTool(name: "opencode", path: "/opt/homebrew/bin/opencode", version: nil),
        InstalledTool(name: "codex", path: "/opt/homebrew/bin/codex", version: nil),
    ]
    let installed = OnboardingFirstAgent.installedPrograms(in: detected)
    #expect(installed == [.codex, .opencode])
    #expect(OnboardingFirstAgent.installedPrograms(in: nil) == nil)

    #expect(OnboardingFirstAgent.defaultProgram(preferred: .opencode, installed: [.codex, .opencode]) == .opencode)
    #expect(OnboardingFirstAgent.defaultProgram(preferred: .claude, installed: [.codex, .opencode]) == .codex)
    #expect(OnboardingFirstAgent.defaultProgram(preferred: nil, installed: []) == nil)

    #expect(OnboardingFirstAgent.defaultFolder(workDirectory: " /code/app ", recent: ["/r"], home: "/h") == "/code/app")
    #expect(OnboardingFirstAgent.defaultFolder(workDirectory: "", recent: ["/r"], home: "/h") == "/r")
    #expect(OnboardingFirstAgent.defaultFolder(workDirectory: "", recent: [], home: "/h") == "/h")

    #expect(OnboardingFirstAgent.placement(remembered: nil, tmuxInstalled: true) == .newSession)
    #expect(OnboardingFirstAgent.placement(remembered: .splitPane, tmuxInstalled: true) == .newSession)
    #expect(OnboardingFirstAgent.placement(remembered: .native, tmuxInstalled: true) == .native)
    #expect(OnboardingFirstAgent.placement(remembered: .newWindow, tmuxInstalled: false) == .native)
}

// MARK: - Shortcuts card

@Test func onboardingShortcutsComeFromTheCatalog() {
    let highlights = OnboardingShortcuts.highlights()
    // All of them still exist in the ⌘/ catalog; a renamed key fails here.
    #expect(highlights.map(\.keys) == OnboardingShortcuts.highlightedKeys)
    let catalog = MuxaShortcutCatalog.sections.flatMap(\.entries)
    for entry in highlights {
        #expect(catalog.contains { $0.keys == entry.keys && $0.title == entry.title })
    }
}
