import Foundation
import Testing
@testable import Muxa

// WS-B: start one agent from the app (New Agent sheet, ⌥⌘T quick start).

private func tmuxPane(
    _ paneID: String = "%12",
    session: String = "callabo",
    sessionID: String = "$3",
    path: String = "/Users/june/src/callabo",
    socket: String? = "default"
) -> MuxaPaneInfo {
    MuxaPaneInfo(
        paneID: paneID, sessionID: sessionID, session: session, windowID: "@4",
        windowName: "main", windowIndex: "1", paneIndex: "0",
        currentCommand: "zsh", title: "", currentPath: path,
        socket: socket, workspaceID: nil, workID: nil,
        agentRole: nil, agentAlias: nil
    )
}

private func guide(program: String? = nil, placement: String? = nil, direction: String? = nil) -> MuxaLaunchSettings.LegacyGuide {
    MuxaLaunchSettings.LegacyGuide(program: program, options: [], placement: placement, direction: direction)
}

@Test func agentProgramsMatchTheCLIAllowlistAndItsAliases() {
    #expect(MuxaAgentProgram.allCases.map(\.rawValue) == ["claude", "codex", "gemini", "agy", "opencode"])
    #expect(MuxaAgentProgram(configValue: "claude-code") == .claude)
    #expect(MuxaAgentProgram(configValue: " CX ") == .codex)
    #expect(MuxaAgentProgram(configValue: "antigravity") == .agy)
    #expect(MuxaAgentProgram(configValue: "bash") == nil)
    #expect(MuxaAgentProgram(configValue: nil) == nil)
}

@Test func quickStartProgramPrefersTheRememberedDefaultThenTheGuide() {
    #expect(MuxaAgentLaunchDefaults().program(guide: nil) == nil)
    #expect(MuxaAgentLaunchDefaults().program(guide: guide(program: "codex")) == .codex)
    #expect(MuxaAgentLaunchDefaults(program: .gemini).program(guide: guide(program: "codex")) == .gemini)
}

@Test func launchDefaultsRoundTripThroughUserDefaults() throws {
    let suite = "muxa.tests.agent-launch.\(UUID().uuidString)"
    let defaults = try #require(UserDefaults(suiteName: suite))
    defer { defaults.removePersistentDomain(forName: suite) }
    #expect(MuxaAgentLaunchDefaults.load(from: defaults) == MuxaAgentLaunchDefaults())
    let saved = MuxaAgentLaunchDefaults(program: .codex, placement: .newWindow, options: ["--model", "o3"])
    saved.save(to: defaults)
    #expect(MuxaAgentLaunchDefaults.load(from: defaults) == saved)
}

@Test func placementFollowsRememberedThenGuideAndDegrades() {
    let everything = MuxaAgentPlacementAvailability(splitPane: true, newWindow: true, native: true)
    #expect(everything.resolve(remembered: nil, guidePlacement: nil) == .splitPane)
    #expect(everything.resolve(remembered: nil, guidePlacement: "session") == .newSession)
    #expect(everything.resolve(remembered: .native, guidePlacement: "window") == .native)

    let noFocusedPane = MuxaAgentPlacementAvailability(splitPane: false, newWindow: true, native: true)
    #expect(noFocusedPane.resolve(remembered: nil, guidePlacement: "pane") == .newWindow)

    let noTmux = MuxaAgentPlacementAvailability(splitPane: false, newWindow: false, native: true)
    #expect(noTmux.resolve(remembered: .splitPane, guidePlacement: nil) == .newSession)

    let remote = MuxaAgentPlacementAvailability(splitPane: false, newWindow: true, native: false)
    #expect(remote.resolve(remembered: .native, guidePlacement: nil) == .newSession)
    #expect(remote.kinds == [.newWindow, .newSession])
}

@Test func availabilityNeedsTheFocusedPaneOnTheLaunchHost() {
    let local = MuxaAgentLaunchFocus.pane(tmuxPane(), hostAlias: "mac", isLocalHost: true, agentCwd: nil)
    let here = MuxaAgentPlacementAvailability(focus: local, launchHost: nil, sessionCount: 2)
    #expect(here.splitPane && here.newWindow && here.native)
    let elsewhere = MuxaAgentPlacementAvailability(focus: local, launchHost: "devbox", sessionCount: 0)
    #expect(!elsewhere.splitPane && !elsewhere.newWindow && !elsewhere.native && elsewhere.newSession)
}

@Test func focusReadsThePaneDirectoryAndTarget() {
    let local = MuxaAgentLaunchFocus.pane(tmuxPane(), hostAlias: "mac", isLocalHost: true, agentCwd: "/elsewhere")
    #expect(local.hostAlias == nil)
    #expect(local.cwd == "/Users/june/src/callabo")
    #expect(local.pane == MuxaAgentLaunchPaneTarget(paneID: "%12", sessionID: "$3", sessionName: "callabo", socket: "default"))

    let noPath = MuxaAgentLaunchFocus.pane(tmuxPane(path: " "), hostAlias: "devbox", isLocalHost: false, agentCwd: "/srv/app")
    #expect(noPath.hostAlias == "devbox")
    #expect(noPath.cwd == "/srv/app")

    let herdr = MuxaAgentLaunchFocus.pane(tmuxPane("herdr:p1"), hostAlias: "mac", isLocalHost: true, agentCwd: nil)
    #expect(herdr.pane == nil)
    #expect(herdr.cwd == "/Users/june/src/callabo")

    #expect(MuxaAgentLaunchFocus.directory("  ") == .empty)
    #expect(MuxaAgentLaunchFocus.directory("/tmp/x").cwd == "/tmp/x")
}

@Test func splitRequestTargetsTheFocusedPaneAndPinsItsServer() {
    let focus = MuxaAgentLaunchFocus.pane(tmuxPane(), hostAlias: "mac", isLocalHost: true, agentCwd: nil)
    let request = MuxaAgentStartRequest.make(
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
    let focus = MuxaAgentLaunchFocus.pane(tmuxPane(), hostAlias: "devbox", isLocalHost: false, agentCwd: nil)
    let request = MuxaAgentStartRequest.make(
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
    let fallback = MuxaAgentStartRequest.make(
        program: .codex, hostAlias: nil, placement: .newWindow,
        focus: MuxaAgentLaunchFocus.pane(tmuxPane(), hostAlias: "mac", isLocalHost: true, agentCwd: nil),
        windowSession: nil, cwd: "/srv/app"
    )
    #expect(fallback.target == "$3")
    #expect(fallback.tmuxSocket == "default")
}

@Test func nativeRequestCarriesOnlyTheAllowlistedEnvironment() {
    let request = MuxaAgentStartRequest.make(
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
    var request = MuxaAgentStartRequest(
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
    let handler: MuxaIPCRequestHandler = { _, payload in
        let object = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        #expect(object["kind"] as? String == "agent_start")
        let request = try #require(object["request"] as? [String: Any])
        if request["placement"] as? String == "native" {
            return Data(#"{"ok":true,"agent_start":{"host":"native","agent":"claude","placement":"session","session":"s-7","name":"claude","cwd":"/tmp","prompt_supplied":false}}"#.utf8)
        }
        if request["host"] as? String == "old" {
            return Data(#"{"ok":false,"error":"invalid work command: only `muxa work …` may run here, not `agent`"}"#.utf8)
        }
        return Data(#"{"ok":true,"agent_start":{"host":"tmux","agent":"codex","placement":"window","pane":"%42","cwd":"/srv/app","prompt_supplied":true,"fleet_host":"devbox"}}"#.utf8)
    }
    let client = MuxaAgentStartClient(socketPath: "/tmp/muxa-agent-test.sock", request: handler)

    let tmux = try await client.start(MuxaAgentStartRequest(
        program: .codex, hostAlias: "devbox", placement: .newWindow, target: "$9", cwd: "/srv/app"
    ))
    #expect(tmux.pane == "%42")
    #expect(tmux.fleetHost == "devbox")
    #expect(!tmux.isNative)

    let native = try await client.start(MuxaAgentStartRequest(
        program: .claude, placement: .native, cwd: "/tmp"
    ))
    #expect(native.isNative)
    #expect(native.session == "s-7")

    await #expect(throws: MuxaIPCError.self) {
        _ = try await client.start(MuxaAgentStartRequest(
            program: .claude, hostAlias: "old", placement: .newSession, cwd: "/srv"
        ))
    }
    #expect(AppModel.agentStartFailureMessage(
        "invalid work command: only `muxa work …` may run here, not `agent`", hostAlias: "old"
    ).contains("Update muxa on old"))
    #expect(AppModel.agentStartFailureMessage("resolve cwd /nope", hostAlias: nil) == "resolve cwd /nope")
}

@Test func pendingTabMatchesTheHostAndPaneOrTheNativeSession() throws {
    let tmux = try JSONDecoder().decode(MuxaAgentStartResult.self, from: Data(
        #"{"host":"tmux","agent":"codex","placement":"pane","pane":"%42","cwd":"/srv"}"#.utf8
    ))
    let otherHost = MuxaWatchPaneIdentity(hostAlias: "devbox", socket: "default", paneID: "%42")
    let here = MuxaWatchPaneIdentity(hostAlias: "mac", socket: "work", paneID: "%42")
    #expect(MuxaPendingAgentTab.selection(for: tmux, hostAlias: "mac", panes: [otherHost], sessionIDs: []) == nil)
    #expect(MuxaPendingAgentTab.selection(for: tmux, hostAlias: "mac", panes: [otherHost, here], sessionIDs: []) == .pane(here))

    let native = try JSONDecoder().decode(MuxaAgentStartResult.self, from: Data(
        #"{"host":"native","agent":"claude","placement":"session","session":"s-7","cwd":"/tmp"}"#.utf8
    ))
    #expect(MuxaPendingAgentTab.selection(for: native, hostAlias: "mac", panes: [here], sessionIDs: ["s-1"]) == nil)
    #expect(MuxaPendingAgentTab.selection(for: native, hostAlias: "mac", panes: [], sessionIDs: ["s-7"]) == .shell("s-7"))
    #expect(MuxaPendingAgentTab.refreshDelays.reduce(0, +) >= 15)
}

@Test func recentDirectoriesAreNewestFirstDedupedAndCapped() {
    var list = MuxaRecentAgentDirectories.adding("/srv/a/", to: [])
    list = MuxaRecentAgentDirectories.adding("/srv/b", to: list)
    list = MuxaRecentAgentDirectories.adding("/srv/./a", to: list)
    #expect(list == ["/srv/a", "/srv/b"])
    #expect(MuxaRecentAgentDirectories.adding("  ", to: list) == list)
    for index in 0..<20 { list = MuxaRecentAgentDirectories.adding("/p/\(index)", to: list) }
    #expect(list.count == MuxaRecentAgentDirectories.limit)
    #expect(list.first == "/p/19")
}

@Test func optionsTextSplitsLikeAShellAndJoinsBack() {
    #expect(MuxaAgentLaunchOptionsText.split("--model opus") == ["--model", "opus"])
    #expect(MuxaAgentLaunchOptionsText.split(#"  --append-system-prompt 'be brief' --x "a b" c\ d '' "#)
        == ["--append-system-prompt", "be brief", "--x", "a b", "c d", ""])
    #expect(MuxaAgentLaunchOptionsText.split("") == [])
    let words = ["claude", "--dangerously-skip-permissions", "--note", "it's fine"]
    #expect(MuxaAgentLaunchOptionsText.split(MuxaAgentLaunchOptionsText.join(words)) == words)
}

@Test func launchSettingsDecodeTheGuidePlacementWhenPresent() throws {
    let current = try JSONDecoder().decode(MuxaLaunchSettings.self, from: Data(
        #"{"providers":[],"pipelines":[],"legacy_guide":{"program":"codex","options":[],"placement":"window","direction":"down"}}"#.utf8
    ))
    #expect(current.legacyGuide.placement == "window")
    #expect(current.legacyGuide.direction == "down")
    let older = try JSONDecoder().decode(MuxaLaunchSettings.self, from: Data(
        #"{"providers":[],"pipelines":[],"legacy_guide":{"program":null,"options":[]}}"#.utf8
    ))
    #expect(older.legacyGuide.placement == nil)
}
