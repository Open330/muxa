import Foundation
import Testing
@testable import Tend

// WS-B: start one agent from the app (New Agent sheet, ⌥⌘T quick start).

private func tmuxPane(
    _ paneID: String = "%12",
    session: String = "callabo",
    sessionID: String = "$3",
    path: String = "/Users/june/src/callabo",
    socket: String? = "default"
) -> TendPaneInfo {
    TendPaneInfo(
        paneID: paneID, sessionID: sessionID, session: session, windowID: "@4",
        windowName: "main", windowIndex: "1", paneIndex: "0",
        currentCommand: "zsh", title: "", currentPath: path,
        socket: socket, workspaceID: nil, workID: nil,
        agentRole: nil, agentAlias: nil
    )
}

private func guide(program: String? = nil, placement: String? = nil, direction: String? = nil) -> TendLaunchSettings.LegacyGuide {
    TendLaunchSettings.LegacyGuide(program: program, options: [], placement: placement, direction: direction)
}

@Test func agentProgramsMatchTheCLIAllowlistAndItsAliases() {
    #expect(TendAgentProgram.allCases.map(\.rawValue) == ["claude", "codex", "agy", "opencode"])
    // The retired Gemini CLI is no longer a launch choice.
    #expect(TendAgentProgram(configValue: "gemini") == nil)
    #expect(TendAgentProgram(configValue: "claude-code") == .claude)
    #expect(TendAgentProgram(configValue: " CX ") == .codex)
    #expect(TendAgentProgram(configValue: "antigravity") == .agy)
    #expect(TendAgentProgram(configValue: "bash") == nil)
    #expect(TendAgentProgram(configValue: nil) == nil)
}

@Test func quickStartProgramPrefersTheRememberedDefaultThenTheGuide() {
    #expect(TendAgentLaunchDefaults().program(guide: nil) == nil)
    #expect(TendAgentLaunchDefaults().program(guide: guide(program: "codex")) == .codex)
    #expect(TendAgentLaunchDefaults(program: .agy).program(guide: guide(program: "codex")) == .agy)
}

@Test func launchDefaultsRoundTripThroughUserDefaults() throws {
    let suite = "tend.tests.agent-launch.\(UUID().uuidString)"
    let defaults = try #require(UserDefaults(suiteName: suite))
    defer { defaults.removePersistentDomain(forName: suite) }
    #expect(TendAgentLaunchDefaults.load(from: defaults) == TendAgentLaunchDefaults())
    let saved = TendAgentLaunchDefaults(program: .codex, placement: .newWindow, options: ["--model", "o3"])
    saved.save(to: defaults)
    #expect(TendAgentLaunchDefaults.load(from: defaults) == saved)

    // A default saved for the retired Gemini CLI keeps its placement but
    // not its program or its Gemini-specific options.
    defaults.set(
        Data(#"{"program":"gemini","placement":"newWindow","options":["--model","gemini-2.5-pro"]}"#.utf8),
        forKey: TendAgentLaunchDefaults.storageKey
    )
    #expect(TendAgentLaunchDefaults.load(from: defaults) == TendAgentLaunchDefaults(placement: .newWindow))
}

@Test func placementFollowsRememberedThenGuideAndDegrades() {
    let everything = TendAgentPlacementAvailability(splitPane: true, newWindow: true, native: true)
    #expect(everything.resolve(remembered: nil, guidePlacement: nil) == .splitPane)
    #expect(everything.resolve(remembered: nil, guidePlacement: "session") == .newSession)
    #expect(everything.resolve(remembered: .native, guidePlacement: "window") == .native)

    let noFocusedPane = TendAgentPlacementAvailability(splitPane: false, newWindow: true, native: true)
    #expect(noFocusedPane.resolve(remembered: nil, guidePlacement: "pane") == .newWindow)

    let noTmux = TendAgentPlacementAvailability(splitPane: false, newWindow: false, native: true)
    #expect(noTmux.resolve(remembered: .splitPane, guidePlacement: nil) == .newSession)

    let remote = TendAgentPlacementAvailability(splitPane: false, newWindow: true, native: false)
    #expect(remote.resolve(remembered: .native, guidePlacement: nil) == .newSession)
    #expect(remote.kinds == [.newWindow, .newSession])
}

@Test func availabilityNeedsTheFocusedPaneOnTheLaunchHost() {
    let local = TendAgentLaunchFocus.pane(tmuxPane(), hostAlias: "mac", isLocalHost: true, agentCwd: nil)
    let here = TendAgentPlacementAvailability(focus: local, launchHost: nil, sessionCount: 2)
    #expect(here.splitPane && here.newWindow && here.native)
    let elsewhere = TendAgentPlacementAvailability(focus: local, launchHost: "devbox", sessionCount: 0)
    #expect(!elsewhere.splitPane && !elsewhere.newWindow && !elsewhere.native && elsewhere.newSession)
}

@Test func focusReadsThePaneDirectoryAndTarget() {
    let local = TendAgentLaunchFocus.pane(tmuxPane(), hostAlias: "mac", isLocalHost: true, agentCwd: "/elsewhere")
    #expect(local.hostAlias == nil)
    #expect(local.cwd == "/Users/june/src/callabo")
    #expect(local.pane == TendAgentLaunchPaneTarget(paneID: "%12", sessionID: "$3", sessionName: "callabo", socket: "default"))

    let noPath = TendAgentLaunchFocus.pane(tmuxPane(path: " "), hostAlias: "devbox", isLocalHost: false, agentCwd: "/srv/app")
    #expect(noPath.hostAlias == "devbox")
    #expect(noPath.cwd == "/srv/app")

    let herdr = TendAgentLaunchFocus.pane(tmuxPane("herdr:p1"), hostAlias: "mac", isLocalHost: true, agentCwd: nil)
    #expect(herdr.pane == nil)
    #expect(herdr.cwd == "/Users/june/src/callabo")

    #expect(TendAgentLaunchFocus.directory("  ") == .empty)
    #expect(TendAgentLaunchFocus.directory("/tmp/x").cwd == "/tmp/x")
}

@Test func splitRequestTargetsTheFocusedPaneAndPinsItsServer() {
    let focus = TendAgentLaunchFocus.pane(tmuxPane(), hostAlias: "mac", isLocalHost: true, agentCwd: nil)
    let request = TendAgentStartRequest.make(
        program: .claude, hostAlias: nil, placement: .splitPane, focus: focus,
        windowSession: nil, cwd: "/Users/june/src/callabo",
        prompt: "  ", name: "", role: "reviewer", options: nil, guideDirection: "down",
        environment: ["PATH": "/opt/homebrew/bin"]
    )
    let payload = request.ipcPayload()
    #expect(payload["agent"] as? String == "claude")
    #expect(payload["placement"] as? String == "pane")
    #expect(payload["target"] as? String == "%12")
    #expect(payload["tmux_socket"] as? String == "default")
    #expect(payload["direction"] as? String == "down")
    #expect(payload["role"] as? String == "reviewer")
    #expect(payload["host"] == nil)
    #expect(payload["prompt"] == nil)
    #expect(payload["name"] == nil)
    #expect(payload["options"] == nil)
    #expect(payload["env"] == nil)
}

@Test func windowRequestOnAFleetHostLeavesTheSocketToThatHost() {
    let focus = TendAgentLaunchFocus.pane(tmuxPane(), hostAlias: "devbox", isLocalHost: false, agentCwd: nil)
    let request = TendAgentStartRequest.make(
        program: .codex, hostAlias: "devbox", placement: .newWindow, focus: focus,
        windowSession: (id: "$9", socket: "default"), cwd: "/srv/app",
        prompt: "fix the flaky test", options: ["--model", "o3"]
    )
    let payload = request.ipcPayload()
    #expect(payload["host"] as? String == "devbox")
    #expect(payload["target"] as? String == "$9")
    #expect(payload["tmux_socket"] == nil)
    #expect(payload["direction"] == nil)
    #expect(payload["prompt"] as? String == "fix the flaky test")
    #expect(payload["options"] as? [String] == ["--model", "o3"])

    // Without a chosen session the focused pane's session is the target.
    let fallback = TendAgentStartRequest.make(
        program: .codex, hostAlias: nil, placement: .newWindow,
        focus: TendAgentLaunchFocus.pane(tmuxPane(), hostAlias: "mac", isLocalHost: true, agentCwd: nil),
        windowSession: nil, cwd: "/srv/app"
    )
    #expect(fallback.target == "$3")
    #expect(fallback.tmuxSocket == "default")
}

@Test func nativeRequestCarriesOnlyTheAllowlistedEnvironment() {
    let request = TendAgentStartRequest.make(
        program: .claude, hostAlias: nil, placement: .native, focus: .empty,
        windowSession: nil, cwd: "/tmp", role: "reviewer",
        environment: ["PATH": "/opt/homebrew/bin", "TERM": "xterm-256color", "HOME": "/Users/june", "SHELL": "/bin/zsh"]
    )
    #expect(request.role == nil)
    #expect(request.target == nil)
    #expect(request.environment == ["PATH": "/opt/homebrew/bin", "TERM": "xterm-256color"])
    #expect(request.ipcPayload()["env"] as? [String: String] == request.environment)
}

@Test func bundledCLIArgumentsMatchTheDaemonsOneWordShape() {
    var request = TendAgentStartRequest(
        program: .claude, hostAlias: nil, placement: .splitPane,
        target: "%12", direction: "down", cwd: "/srv/app",
        prompt: "-v means verbose", role: "reviewer", options: ["--model", "opus"]
    )
    #expect(request.cliArguments() == [
        "agent", "start", "--json", "--agent=claude", "--host=tmux",
        "--placement=pane", "--target=%12", "--direction=down", "--cwd=/srv/app",
        "--prompt=-v means verbose", "--role=reviewer", "--option=--model", "--option=opus",
    ])
    request.placement = .native
    request.target = nil
    request.direction = nil
    request.name = "claude"
    // Native passes `--direction=auto` for CLIs whose default refused it.
    #expect(request.cliArguments() == [
        "agent", "start", "--json", "--agent=claude", "--host=native",
        "--direction=auto", "--cwd=/srv/app", "--prompt=-v means verbose", "--name=claude",
        "--option=--model", "--option=opus",
    ])
}

@Test func agentStartClientSendsTheRequestAndDecodesBothEnvelopes() async throws {
    let handler: TendIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        #expect(object["kind"] as? String == "agent_start")
        let request = try #require(object["request"] as? [String: Any])
        if request["placement"] as? String == "native" {
            return Data(#"{"ok":true,"agent_start":{"host":"native","agent":"claude","placement":"session","session":"s-7","name":"claude","cwd":"/tmp","prompt_supplied":false}}"#.utf8)
        }
        if request["host"] as? String == "old" {
            return Data(#"{"ok":false,"error":"invalid work command: only `tend work …` may run here, not `agent`"}"#.utf8)
        }
        return Data(#"{"ok":true,"agent_start":{"host":"tmux","agent":"codex","placement":"window","pane":"%42","cwd":"/srv/app","prompt_supplied":true,"fleet_host":"devbox"}}"#.utf8)
    }
    let client = TendAgentStartClient(socketPath: "/tmp/tend-agent-test.sock", request: handler)

    let tmux = try await client.start(TendAgentStartRequest(
        program: .codex, hostAlias: "devbox", placement: .newWindow, target: "$9", cwd: "/srv/app"
    ))
    #expect(tmux.pane == "%42")
    #expect(tmux.fleetHost == "devbox")
    #expect(!tmux.isNative)

    let native = try await client.start(TendAgentStartRequest(
        program: .claude, placement: .native, cwd: "/tmp"
    ))
    #expect(native.isNative)
    #expect(native.session == "s-7")

    await #expect(throws: TendIPCError.self) {
        _ = try await client.start(TendAgentStartRequest(
            program: .claude, hostAlias: "old", placement: .newSession, cwd: "/srv"
        ))
    }
    #expect(AppModel.agentStartFailureMessage(
        "invalid work command: only `tend work …` may run here, not `agent`", hostAlias: "old"
    ).contains("Update tend on old"))
    #expect(AppModel.agentStartFailureMessage("resolve cwd /nope", hostAlias: nil) == "resolve cwd /nope")
}

@Test func pendingTabMatchesTheHostAndPaneOrTheNativeSession() throws {
    let tmux = try JSONDecoder().decode(TendAgentStartResult.self, from: Data(
        #"{"host":"tmux","agent":"codex","placement":"pane","pane":"%42","cwd":"/srv"}"#.utf8
    ))
    let otherHost = TendWatchPaneIdentity(hostAlias: "devbox", socket: "default", paneID: "%42")
    let here = TendWatchPaneIdentity(hostAlias: "mac", socket: "work", paneID: "%42")
    #expect(TendPendingAgentTab.selection(for: tmux, hostAlias: "mac", panes: [otherHost], sessionIDs: []) == nil)
    #expect(TendPendingAgentTab.selection(for: tmux, hostAlias: "mac", panes: [otherHost, here], sessionIDs: []) == .pane(here))

    let native = try JSONDecoder().decode(TendAgentStartResult.self, from: Data(
        #"{"host":"native","agent":"claude","placement":"session","session":"s-7","cwd":"/tmp"}"#.utf8
    ))
    #expect(TendPendingAgentTab.selection(for: native, hostAlias: "mac", panes: [here], sessionIDs: ["s-1"]) == nil)
    #expect(TendPendingAgentTab.selection(for: native, hostAlias: "mac", panes: [], sessionIDs: ["s-7"]) == .shell("s-7"))
    #expect(TendPendingAgentTab.refreshDelays.reduce(0, +) >= 15)
}

@Test func recentDirectoriesAreNewestFirstDedupedAndCapped() {
    var list = TendRecentAgentDirectories.adding("/srv/a/", to: [])
    list = TendRecentAgentDirectories.adding("/srv/b", to: list)
    list = TendRecentAgentDirectories.adding("/srv/./a", to: list)
    #expect(list == ["/srv/a", "/srv/b"])
    #expect(TendRecentAgentDirectories.adding("  ", to: list) == list)
    for index in 0..<20 { list = TendRecentAgentDirectories.adding("/p/\(index)", to: list) }
    #expect(list.count == TendRecentAgentDirectories.limit)
    #expect(list.first == "/p/19")
}

@Test func optionsTextSplitsLikeAShellAndJoinsBack() {
    #expect(TendAgentLaunchOptionsText.split("--model opus") == ["--model", "opus"])
    #expect(TendAgentLaunchOptionsText.split(#"  --append-system-prompt 'be brief' --x "a b" c\ d '' "#)
        == ["--append-system-prompt", "be brief", "--x", "a b", "c d", ""])
    #expect(TendAgentLaunchOptionsText.split("") == [])
    let words = ["claude", "--dangerously-skip-permissions", "--note", "it's fine"]
    #expect(TendAgentLaunchOptionsText.split(TendAgentLaunchOptionsText.join(words)) == words)
}

@Test func launchSettingsDecodeTheGuidePlacementWhenPresent() throws {
    let current = try JSONDecoder().decode(TendLaunchSettings.self, from: Data(
        #"{"providers":[],"pipelines":[],"legacy_guide":{"program":"codex","options":[],"placement":"window","direction":"down"}}"#.utf8
    ))
    #expect(current.legacyGuide.placement == "window")
    #expect(current.legacyGuide.direction == "down")
    let older = try JSONDecoder().decode(TendLaunchSettings.self, from: Data(
        #"{"providers":[],"pipelines":[],"legacy_guide":{"program":null,"options":[]}}"#.utf8
    ))
    #expect(older.legacyGuide.placement == nil)
}
