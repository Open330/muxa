import Foundation
import Testing
@testable import Muxa

@Test func anApprovalRuleConfirmsOnlyTheNamedRequest() throws {
    let rule = FleetApprovalRules.make(
        text: "npm test",
        isPattern: false,
        agents: ["codex", "claude_code"],
        work: MuxaWorkIdentity(workspaceID: "ws", workID: "fix-1.2"),
        existingNames: ["approve-npm-test"]
    )
    #expect(rule.name == "approve-npm-test-2")
    #expect(rule.on == .waitingInput)
    #expect(rule.action == .pressEnter)
    #expect(rule.screen == "npm test")
    #expect(rule.agent == ["claude_code", "codex"])
    #expect(rule.workspace == "ws")
    #expect(rule.work == "^fix\\-1\\.2$")
    #expect(FleetApprovalRules.isApproval(rule))
    #expect(MuxaAutomationRuleDraft.draft(editing: rule).issues(existingNames: []).isEmpty)

    let wire = rule.wireObject
    #expect(wire["screen"] as? String == "npm test")
    #expect(wire["action"] as? String == "press_enter")
    #expect(rule.tomlSnippet.contains("screen = \"npm test\""))

    // Editing the rule elsewhere keeps its pattern.
    #expect(MuxaAutomationRuleDraft.draft(editing: rule).rule.screen == "npm test")
}

@Test func literalRequestTextIsEscapedForTheDaemonsRegex() {
    #expect(FleetApprovalRules.escape("rm -rf ./build (all)") == "rm \\-rf \\./build \\(all\\)")
    #expect(FleetApprovalRules.make(text: "a.b", isPattern: true, agents: ["codex"], work: nil, existingNames: []).screen == "a.b")
    #expect(FleetApprovalRules.name(for: "  ", existing: []) == "approve")
    #expect(FleetApprovalRules.name(for: "Bash(git status --short)", existing: []) == "approve-bash-git-status-short")
}

@Test func aStarterPressEnterRuleIsNotAnApprovalRule() {
    let starter = MuxaAutomationRule(name: "confirm-selected-yes", on: .waitingInput, action: .pressEnter)
    #expect(!FleetApprovalRules.isApproval(starter))
    let prompt = MuxaAutomationRule(name: "x", on: .waitingInput, screen: "npm test", action: .sendPrompt, text: "y")
    #expect(!FleetApprovalRules.isApproval(prompt))
    #expect(prompt.wireObject["screen"] == nil, "only press_enter carries a screen pattern")
}

@Test func theDialogPreviewIsTheRequestAboveTheSelectedYes() {
    let claude = """
    earlier: ran rm -rf build
    ╭──────────────────────────────╮
    │ Bash command                 │
    │   npm test                   │
    │ Do you want to proceed?      │
    │ ❯ 1. Yes                     │
    │   2. No                      │
    ╰──────────────────────────────╯
    Enter to confirm · Esc to cancel
    """
    let request = FleetApprovalRules.dialogText(from: claude)
    #expect(request == "Bash command\nnpm test\nDo you want to proceed?")
    #expect(FleetApprovalRules.matches("npm test", isPattern: false, request: request ?? ""))
    #expect(!FleetApprovalRules.matches("rm -rf", isPattern: false, request: request ?? ""))
    #expect(FleetApprovalRules.matches("^npm (test|run lint)$", isPattern: true, request: request ?? ""))
    #expect(FleetApprovalRules.dialogText(from: "npm test\n› Ask anything\n› again") == nil)
}

@Test func handledTodayCountsActionsNotSkips() throws {
    func entry(_ outcome: String, _ at: String) throws -> MuxaAutomationLogEntry {
        try JSONDecoder().decode(MuxaAutomationLogEntry.self, from: JSONSerialization.data(withJSONObject: [
            "rule": "r", "outcome": outcome, "fired_at": at,
        ]))
    }
    let now = try #require(MuxaUsageTimestamp.parse("2026-11-09T12:00:00Z"))
    let log = [
        try entry("fired", "2026-11-09T11:00:00Z"),
        try entry("skipped", "2026-11-09T11:30:00Z"),
        try entry("admitted", "2026-11-09T10:00:00Z"),
        try entry("fired", "2026-11-07T10:00:00Z"),
    ]
    #expect(FleetAutomationSummary.handled(log, now: now).count == 2)
}
