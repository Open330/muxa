import AppKit
import Foundation
import SwiftUI
import Testing
@testable import Muxa

@Test func promptComposerGuardsHostControlForEverySendPath() {
    let local = MuxaFleetHostIdentity(alias: "local", local: true, state: "online", mode: "observe")
    let control = MuxaFleetHostIdentity(alias: "remote", local: false, state: "online", mode: "control")
    let observe = MuxaFleetHostIdentity(alias: "readonly", local: false, state: "online", mode: "observe")
    let unknown = MuxaFleetHostIdentity(alias: "unknown", local: false, state: "online", mode: "")

    #expect(PromptComposerRules.canSend(prompt: "다음 작업", sending: false, hosts: [local]))
    #expect(PromptComposerRules.canSend(prompt: "next", sending: false, hosts: [control]))
    #expect(PromptComposerRules.canSend(prompt: "next", sending: false, hosts: [local, control]))
    #expect(!PromptComposerRules.canSend(prompt: "next", sending: false, hosts: [observe]))
    #expect(!PromptComposerRules.canSend(prompt: "next", sending: false, hosts: [local, observe]))
    #expect(!PromptComposerRules.canSend(prompt: "next", sending: false, hosts: [unknown]))
    #expect(!PromptComposerRules.canSend(prompt: "next", sending: false, hosts: []))
    #expect(!PromptComposerRules.canSend(prompt: "next", sending: true, hosts: [control]))
    #expect(!PromptComposerRules.canSend(prompt: " \n\t ", sending: false, hosts: [local]))
}

@Test func promptComposerOnlyClearsTheUneditedSubmittedDraft() {
    let submittedRevision = UUID()
    let editedRevision = UUID()
    let submitted = "  다음 작업\n"

    #expect(PromptComposerRules.shouldClearDraft(
        current: submitted, submitted: submitted,
        revision: submittedRevision, submittedRevision: submittedRevision
    ))
    #expect(!PromptComposerRules.shouldClearDraft(
        current: "새 초안", submitted: submitted,
        revision: editedRevision, submittedRevision: submittedRevision
    ))
    #expect(!PromptComposerRules.shouldClearDraft(
        current: submitted, submitted: submitted,
        revision: editedRevision, submittedRevision: submittedRevision
    ))
    #expect(!PromptComposerRules.shouldClearDraft(
        current: "external replacement", submitted: submitted,
        revision: submittedRevision, submittedRevision: submittedRevision
    ))
}

@Test func promptComposerCommandReturnRequiresFocusAndNoIMEComposition() {
    #expect(PromptComposerRules.handlesSendKey(keyCode: 36, modifiers: .command, focused: true, markedText: false))
    #expect(!PromptComposerRules.handlesSendKey(keyCode: 36, modifiers: .command, focused: false, markedText: false))
    #expect(!PromptComposerRules.handlesSendKey(keyCode: 36, modifiers: .command, focused: true, markedText: true))
    #expect(PromptComposerRules.handlesSendKey(keyCode: 36, modifiers: [.command, .capsLock], focused: true, markedText: false))
}

@Test func promptComposerLeavesEnterAndFocusTraversalToTheNativeEditor() {
    let returnModifiers: [NSEvent.ModifierFlags] = [[], .shift, .option, .control, [.command, .shift], [.command, .option]]
    for modifiers in returnModifiers {
        #expect(!PromptComposerRules.handlesSendKey(keyCode: 36, modifiers: modifiers, focused: true, markedText: false))
    }
    let tabModifiers: [NSEvent.ModifierFlags] = [[], .shift, .command]
    for modifiers in tabModifiers {
        #expect(!PromptComposerRules.handlesSendKey(keyCode: 48, modifiers: modifiers, focused: true, markedText: false))
    }
    #expect(!PromptComposerRules.handlesSendKey(keyCode: 0, modifiers: .command, focused: true, markedText: false))
}

@Test func promptComposerFeedbackPreservesActionableMultilineDetails() {
    let message = "remote/%12: Permission denied.\nChange the host to control mode, then retry.\n" + String(repeating: "Diagnostic detail. ", count: 100)
    let feedback = PromptFeedback(message: message, succeeded: false)
    #expect(feedback.message == message)
    #expect(!feedback.succeeded)
    #expect(PromptFeedback(message: "전송 완료", succeeded: true).succeeded)
}

@Test @MainActor func promptComposerStatusKeepsTheSameBoundsForEveryOutcome() {
    let outcomes: [PromptFeedback?] = [
        nil,
        PromptFeedback(message: "Sent and submitted", succeeded: true),
        PromptFeedback(message: "Sent to 12 collaborators", succeeded: true),
        PromptFeedback(message: String(repeating: "오류: 호스트 설정을 확인하세요.\n", count: 100), succeeded: false),
    ]
    for feedback in outcomes {
        let status = NSHostingView(rootView: PromptComposerStatus(feedback: feedback))
        status.layoutSubtreeIfNeeded()
        #expect(status.fittingSize == NSSize(width: 132, height: 24))
    }
}
