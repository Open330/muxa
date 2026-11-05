import Foundation
import Testing
@testable import Muxa

private func agent(_ fields: [String: Any], id: String = "s1", kind: String = "claude_code") throws -> MuxaAgent {
    var object: [String: Any] = ["kind": kind, "agent_session_id": id, "state": "working"]
    object.merge(fields) { _, new in new }
    return try JSONDecoder().decode(MuxaAgent.self, from: JSONSerialization.data(withJSONObject: object))
}

private func target(
    _ fields: [String: Any], kind: String = "claude_code", host: String = "local", local: Bool = true
) throws -> MuxaAgentActionTarget {
    MuxaAgentActionTarget(
        host: MuxaFleetHostIdentity(alias: host, local: local, state: "online", mode: "control"),
        agent: try agent(fields, kind: kind),
        pane: nil
    )
}

/// A cap that lifts long after any test runs.
private var capped: [String: Any] {
    ["state": "error", "rate_limit_scope": "five_hour", "rate_limited_until": "2100-01-01T00:00:00Z"]
}

// MARK: - Swap argv

@Test func swapRequestBuildsTheFleetAllowlistedArgv() {
    var request = MuxaSwapRequest(scope: .agents(["abc-1", "def-2"]))
    request.account = " work@claude "
    request.followUp = "keep going"
    #expect(request.arguments == [
        "swap", "abc-1", "def-2", "--to=work@claude", "--continue-text=keep going", "--json",
    ])
    #expect(request.planArguments.last == "--dry-run")
    #expect(request.startArguments(requestKey: "app-k") .suffix(2) == ["--detach", "--request-key=app-k"])

    // Every value travels as one `--flag=value` word: the allowlist takes
    // no separate value words.
    for word in request.startArguments(requestKey: "app-k") where word.hasPrefix("--") {
        #expect(!word.hasSuffix("="))
    }

    var limited = MuxaSwapRequest(scope: .limited)
    limited.spread = true
    limited.account = "ignored@claude"
    limited.noFollowUp = true
    limited.followUp = "ignored"
    limited.force = true
    #expect(limited.arguments == ["swap", "--limited", "--spread", "--force", "--no-continue", "--json"])

    // Blank account and follow-up fall back to the CLI's defaults.
    var auto = MuxaSwapRequest(scope: .agents(["x"]))
    auto.account = "  "
    auto.followUp = ""
    #expect(auto.arguments == ["swap", "x", "--json"])
}

@Test func swapRequestKeysFitTheCLIRules() {
    let key = MuxaSwapRequest.mintRequestKey()
    #expect(key.count <= 128)
    #expect(key.allSatisfy { $0.isASCII && ($0.isLetter || $0.isNumber || "-_.:@".contains($0)) })
    #expect(key != MuxaSwapRequest.mintRequestKey())
    #expect(MuxaSwapCommand.status(key) == ["swap", "--status=\(key)", "--wait-secs=0", "--json"])
    #expect(MuxaSwapCommand.retry(key, requestKey: "n").contains("--retry=\(key)"))
}

// MARK: - Swap output

@Test func swapPlanDecodesEvenWhenTheCommandFailed() throws {
    let plan = """
    [
      {"pane":"%5","agent_session_id":"abc","kind":"claude_code","from":"a@claude","to":"b@claude",
       "status":"planned","command":"aas e b@claude -- claude --resume abc","continue_reason":"rate_limited"},
      {"pane":"%6","agent_session_id":"def","kind":"codex","status":"skipped","note":"working; pass --force"}
    ]
    """
    let output = MuxaWorkCommandOutput(exitCode: 1, stdout: plan, stderr: "Error: some agents could not be swapped")
    let outcomes = try MuxaSwapCommand.decode([MuxaSwapOutcome].self, from: output)
    #expect(outcomes.count == 2)
    #expect(outcomes[0].status == .planned)
    #expect(outcomes[0].from == "a@claude")
    #expect(outcomes[0].sendsFollowUp)
    #expect(outcomes[1].status == .skipped)
    #expect(outcomes[1].note == "working; pass --force")
    #expect(!outcomes[1].sendsFollowUp)
}

@Test func swapCommandFailureNamesStderrNotAdvice() {
    let output = MuxaWorkCommandOutput(
        exitCode: 1,
        stdout: "",
        stderr: "Error: the daemon refused this swap: no account with quota\n"
    )
    #expect(throws: MuxaIPCError.self) {
        try MuxaSwapCommand.decode(MuxaSwapAck.self, from: output)
    }
    #expect(MuxaSwapCommand.failureMessage(output) == "the daemon refused this swap: no account with quota")
}

@Test func swapJobViewDecodesProgressAndFollowUps() throws {
    let job = """
    {"job_id":"j1","node_id":"n","state":"running","revision":4,"reason":null,"fault":null,
     "entries":[
       {"report_index":0,"pane":"%5","disposition":"swapped","reason":null,"uncertain_effects":0,
        "outcome":{"pane":"%5","agent_session_id":"abc","kind":"claude_code","from":"a@claude","to":"b@claude","status":"swapped"},
        "continuation":"allowed","follow_up":{"submission":"delivered","evidence":"prompt_hook"}},
       {"report_index":1,"pane":"%6","disposition":null,"reason":null,"uncertain_effects":0,"outcome":null},
       {"report_index":2,"pane":"%7","disposition":"failed","reason":"relaunch not confirmed","uncertain_effects":1,"outcome":null}
     ]}
    """
    let view = try JSONDecoder().decode(MuxaSwapJobView.self, from: Data(job.utf8))
    #expect(view.isRunning)
    #expect(view.entries.map(\.phase) == [.swapped, .pending, .failed])
    #expect(view.swappedCount == 1)
    #expect(view.failedCount == 1)
    #expect(view.entries[0].followUp?.evidence == "prompt_hook")
    #expect(!view.cancelRequested)

    let ended = try JSONDecoder().decode(
        MuxaSwapJobView.self,
        from: Data(#"{"job_id":"j1","state":"failed","entries":[]}"#.utf8)
    )
    #expect(!ended.isRunning)
}

// MARK: - Agent actions

@MainActor
@Test func cappedAgentsOfferSwitchingFirst() throws {
    let limited = try target(capped)
    let actions = MuxaAgentActions.actions(for: limited, swaps: nil)
    #expect(actions.first?.kind == .switchAccountAndResume)
    #expect(actions.first?.isPrimary == true)

    // aas cannot relaunch other agents, so a capped one gets nothing to do.
    let other = try target(capped, kind: "opencode")
    #expect(MuxaAgentActions.actions(for: other, swaps: nil).isEmpty)

    let idle = try target(["state": "idle"])
    let idleActions = MuxaAgentActions.actions(for: idle, swaps: nil)
    #expect(idleActions.map(\.kind) == [.moveToAccount])
    #expect(idleActions.allSatisfy { !$0.isPrimary })
}

@Test func swapSheetTargetsTheAgentOnItsOwnHost() throws {
    let remote = try target(["state": "working"], host: "rtzr", local: false)
    let sheet = remote.swapSheetTarget
    #expect(sheet.host == "rtzr")
    #expect(sheet.scope == .agents(["s1"]))
    #expect(sheet.interruptsWork)
    #expect(sheet.agentKind == "claude_code")

    let local = try target(capped)
    #expect(local.swapSheetTarget.host == nil)
    #expect(!local.swapSheetTarget.interruptsWork)
}

// MARK: - Limit policy

private func rule(_ name: String, action: MuxaAutomationAction, enabled: Bool, on: MuxaAutomationEvent = .rateLimited) -> MuxaAutomationRule {
    var rule = MuxaAutomationRule(name: name, on: on, action: action)
    rule.enabled = enabled
    return rule
}

@Test func limitPolicyReadsWhatTheLiveRulesDo() {
    let starters = [
        rule("resume-after-limit", action: .sendPrompt, enabled: false),
        rule("notify-waiting-input", action: .notify, enabled: true, on: .waitingInput),
    ]
    #expect(MuxaLimitPolicy.current(enabled: true, rules: starters) == .tellMe)

    let waiting = [rule("resume-after-limit", action: .sendPrompt, enabled: true)]
    #expect(MuxaLimitPolicy.current(enabled: true, rules: waiting) == .waitForReset)
    // The engine switch off means nothing fires, whatever the rules say.
    #expect(MuxaLimitPolicy.current(enabled: false, rules: waiting) == .tellMe)

    let both = waiting + [rule("mine", action: .swapAccount, enabled: true)]
    #expect(MuxaLimitPolicy.current(enabled: true, rules: both) == .switchAccount)
}

@Test func choosingALimitPolicyEditsOnlyWhatItMust() {
    let starters = [
        rule("resume-after-limit", action: .sendPrompt, enabled: false),
        rule("notify-persistent-error", action: .notify, enabled: true, on: .error),
    ]

    // Switching creates the swap rule; the disabled starter is left alone.
    let toSwitch = MuxaLimitPolicy.switchAccount.plan(for: starters)
    #expect(toSwitch.create?.name == MuxaLimitPolicy.swapRuleName)
    #expect(toSwitch.create?.action == .swapAccount)
    #expect(toSwitch.disable.isEmpty)
    #expect(toSwitch.enablesEngine)

    // Waiting enables the installed starter rather than writing a copy.
    let toWait = MuxaLimitPolicy.waitForReset.plan(for: starters)
    #expect(toWait.create == nil)
    #expect(toWait.enable == ["resume-after-limit"])

    // From switching to waiting: the swap rule is turned off, not removed,
    // and an unrelated notify rule is never touched.
    let switching = [
        rule("resume-after-limit", action: .sendPrompt, enabled: false),
        rule(MuxaLimitPolicy.swapRuleName, action: .swapAccount, enabled: true),
        rule("notify-persistent-error", action: .notify, enabled: true, on: .error),
    ]
    let back = MuxaLimitPolicy.waitForReset.plan(for: switching)
    #expect(back.enable == ["resume-after-limit"])
    #expect(back.disable == [MuxaLimitPolicy.swapRuleName])

    // Just tell me: every resuming rule on the cap goes quiet.
    let quiet = MuxaLimitPolicy.tellMe.plan(for: switching + [rule("resume-after-limit-2", action: .sendPrompt, enabled: true)])
    #expect(Set(quiet.disable) == [MuxaLimitPolicy.swapRuleName, "resume-after-limit-2"])
    #expect(!quiet.enablesEngine)

    // The rule the switch choice writes is one muxad accepts.
    let draft = MuxaAutomationRuleDraft.draft(editing: MuxaLimitPolicy.swapRule)
    #expect(draft.issues(existingNames: []).isEmpty)
    #expect(MuxaLimitPolicy.swapRule.wireObject["submit"] == nil)
}

// MARK: - Notifications

@MainActor
@Test func uncoveredCapsNotifyWithSwitchAccount() throws {
    let hosted = MuxaHostedAgent(
        host: MuxaFleetHostIdentity(alias: "rtzr", local: false, state: "online", mode: "control"),
        agent: try agent(capped),
        pane: nil
    )
    let note = MuxaAgentAttentionCenter.notification(for: hosted, event: .limited, pane: nil, sound: false)
    #expect(note.category == MuxaNotificationActions.limitedCategory)
    #expect(note.title.contains("usage limit"))
    #expect(MuxaNotificationActions.route(action: MuxaNotificationActions.switchAccount, userText: nil) == .switchAccount)
    #expect(MuxaNotificationActions.category(for: .attention("waiting_input")) == MuxaNotificationActions.inputCategory)
}

// MARK: - Attention summary

@MainActor
@Test func oneSummaryCountsEachThingOnce() throws {
    let host = MuxaFleetHostIdentity(alias: "local", local: true, state: "online", mode: "control")
    // Ids no other test registers as local, so the shared coverage state
    // cannot leak between tests.
    let blocked = MuxaHostedAgent(host: host, agent: try agent(["state": "waiting_input"], id: "sum-a"), pane: nil)
    let busy = MuxaHostedAgent(host: host, agent: try agent(["state": "working"], id: "sum-b"), pane: nil)
    let capped = MuxaHostedAgent(host: host, agent: try agent(capped, id: "sum-c"), pane: nil)
    let key: (MuxaHostedAgent) -> String = { "agent:\($0.agent.agentSessionID)" }

    // `sum-c` is not a local agent: no policy reaches it.
    var summary = MuxaAttentionSummary.make(
        agents: [blocked, busy, capped], messages: [], unread: ["agent:sum-a", "pane:x"], paneKey: key
    )
    #expect(summary.agents == 2)
    #expect(summary.limited == 1)
    #expect(summary.uncoveredLimited == 1)
    #expect(summary.needsYou == 2)
    #expect(summary.unreadPanes == 2)
    // `sum-a` is both blocked and unread: once on the Dock.
    #expect(summary.dock == 3)
    #expect(summary.needsYouLine == "2 agents need you")
    #expect(MuxaAttentionSummary().needsYouLine == nil)
}

// MARK: - Limit coverage

private func capRule(_ action: MuxaAutomationAction, agents: [String] = [], scope: [String] = []) -> MuxaAutomationRule {
    var rule = MuxaAutomationRule(name: "r", on: .rateLimited, action: action)
    rule.agent = agents
    rule.scope = scope
    return rule
}

@Test func coverageReachesOnlyLocalAgentsTheRulesName() throws {
    let claude = try agent(capped, id: "cov-1")
    let gemini = try agent(capped, id: "cov-2", kind: "gemini")
    let remote = try agent(capped, id: "cov-3")
    var state = MuxaLimitCoverage.State()
    state.localAgents = ["cov-1", "cov-2"]

    // A switch rule reaches Claude Code and Codex only.
    state.grants = MuxaLimitCoverage.grants(enabled: true, rules: [capRule(.swapAccount)])
    #expect(MuxaLimitCoverage.covers(claude, in: state))
    #expect(!MuxaLimitCoverage.covers(gemini, in: state))
    // An agent on a Fleet host follows its own daemon's rules.
    #expect(!MuxaLimitCoverage.covers(remote, in: state))

    // …and nothing while the swap service takes no job.
    state.swapAvailable = false
    #expect(!MuxaLimitCoverage.covers(claude, in: state))
    state.swapAvailable = true

    // A resume rule reaches every kind; a pause covers nothing until it ends.
    state.grants = MuxaLimitCoverage.grants(enabled: true, rules: [capRule(.sendPrompt)])
    #expect(MuxaLimitCoverage.covers(gemini, in: state))
    let now = Date()
    state.pausedUntil = now.addingTimeInterval(60)
    #expect(!MuxaLimitCoverage.covers(gemini, in: state, now: now))
    #expect(MuxaLimitCoverage.covers(gemini, in: state, now: now.addingTimeInterval(120)))
    state.pausedUntil = nil

    // Scopes and kinds narrow a rule; any other filter makes it unknowable.
    state.grants = MuxaLimitCoverage.grants(enabled: true, rules: [capRule(.sendPrompt, agents: ["codex"])])
    #expect(!MuxaLimitCoverage.covers(claude, in: state))
    state.grants = MuxaLimitCoverage.grants(enabled: true, rules: [capRule(.sendPrompt, scope: ["seven_day"])])
    #expect(!MuxaLimitCoverage.covers(claude, in: state), "the fixture is capped on five_hour")
    var filtered = capRule(.sendPrompt)
    filtered.workspace = "api"
    #expect(MuxaLimitCoverage.grants(enabled: true, rules: [filtered]).isEmpty)
    #expect(MuxaLimitCoverage.grants(enabled: false, rules: [capRule(.sendPrompt)]).isEmpty)
}

// MARK: - Swap activity

@Test func aSwapTheDaemonTookKeepsRunningThroughFailedReads() {
    var activity = MuxaSwapActivity(requestKey: "k", host: "mini", startedAt: Date(), title: "t")
    activity.targets = ["abc"]
    #expect(activity.isRunning, "started, no status yet")
    activity.statusError = "host unreachable"
    activity.statusFailures = 3
    #expect(activity.isRunning, "a failed read is not a failed swap")
    #expect(!activity.needsLook)
    activity.statusFailures = MuxaSwapCenter.maxStatusFailures
    #expect(activity.lostTrack)
    #expect(!activity.isRunning, "polling stops")
    #expect(activity.needsLook)

    var refused = MuxaSwapActivity(requestKey: "k2", host: nil, startedAt: Date(), title: "t")
    refused.error = "the daemon refused this swap"
    #expect(!refused.isRunning)
}

@Test func onlyRecentAdmittedSwapsAreFollowed() throws {
    func entry(_ outcome: String, op: String?, at: String) throws -> MuxaAutomationLogEntry {
        var object: [String: Any] = ["rule": "swap-on-limit", "action": "swap_account", "outcome": outcome, "fired_at": at]
        if let op { object["operation"] = op }
        return try JSONDecoder().decode(MuxaAutomationLogEntry.self, from: JSONSerialization.data(withJSONObject: object))
    }
    let now = try #require(MuxaAutomationTime.parse("2026-11-05T10:00:00Z"))
    let entries = [
        try entry("admitted", op: "auto-1", at: "2026-11-05T09:58:00Z"),
        try entry("admitted", op: "auto-2", at: "2026-11-05T08:00:00Z"),
        try entry("admitted", op: "auto-3", at: "2026-11-05T09:59:00Z"),
        try entry("skipped", op: nil, at: "2026-11-05T09:59:30Z"),
    ]
    let follow = AutomationStore.swapsToFollow(entries, followed: ["auto-3"], now: now)
    #expect(follow.compactMap(\.operation) == ["auto-1"])
}

// MARK: - Swap health

@Test func swapHealthSaysWhyItCannotSwitch() throws {
    let halted = try JSONDecoder().decode(MuxaSwapHealth.self, from: Data(#"""
    {"readiness":"halted","reason":"store lost","schema":3,"expected_schema":3,"fault":"store lost","jobs_running":0}
    """#.utf8))
    #expect(!halted.isReady)
    #expect(halted.readiness == .halted)
    #expect(halted.detail?.hasPrefix("store lost") == true)

    let ready = try JSONDecoder().decode(MuxaSwapHealth.self, from: Data(#"""
    {"readiness":"ready","reason":null,"schema":3,"expected_schema":3,"fault":null,"jobs_running":2}
    """#.utf8))
    #expect(ready.isReady)
    #expect(ready.jobsRunning == 2)
    #expect(ready.detail == nil)

    let future = try JSONDecoder().decode(MuxaSwapHealth.self, from: Data(#"{"readiness":"warming","expected_schema":3}"#.utf8))
    #expect(future.readiness == .unknown("warming"))
}

@Test func aSwapFieldOfAnotherShapeDoesNotFailTheReply() throws {
    struct Reply: Decodable { let swap: MuxaLenient<MuxaSwapHealth>? }
    let job = try JSONDecoder().decode(Reply.self, from: Data(#"{"swap":{"job_id":"j1","state":"running"}}"#.utf8))
    #expect(job.swap?.value == nil)
    let health = try JSONDecoder().decode(Reply.self, from: Data(#"{"swap":{"readiness":"ready","jobs_running":0}}"#.utf8))
    #expect(health.swap?.value?.isReady == true)
}
