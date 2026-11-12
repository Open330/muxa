// Decisions the person makes from Home, recorded by tendd rather than in
// this Mac's settings: Looks Good, Send Back, and answers typed or chosen
// for a waiting agent. Each is a review record (`review_record_v1`), a
// claim and its disposition in one step, so `tend review history` and the
// history view show what was decided, by whom and when.
import CryptoKit
import Foundation

enum FleetDecisionRecord {
    /// Idempotency-key prefixes, one per kind of Home decision. The history
    /// view tells the kinds apart by them.
    static let looksGoodPrefix = "home:"
    static let sendBackPrefix = "home-sendback:"
    static let answerPrefix = "home-answer:"

    enum Kind: Equatable, Sendable {
        case looksGood
        case sentBack
        case answered
    }

    static func kind(ofKey key: String?) -> Kind? {
        guard let key else { return nil }
        if key.hasPrefix(sendBackPrefix) { return .sentBack }
        if key.hasPrefix(answerPrefix) { return .answered }
        if key.hasPrefix(looksGoodPrefix) { return .looksGood }
        return nil
    }

    /// The keys of results accepted from Home, which Home hides.
    static func reviewedKeys(in history: TendDecisionHistory) -> Set<String> {
        Set(history.items.compactMap { item -> String? in
            guard case .review(let review) = item, review.disposition?.kind == "accepted",
                  let key = review.idempotencyKey, kind(ofKey: key) == .looksGood else { return nil }
            return key
        })
    }

    static func summary(_ work: FleetWork) -> String {
        let headline = work.headline.trimmingCharacters(in: .whitespacesAndNewlines)
        return headline.isEmpty ? work.title : headline
    }

    /// One key per instruction: the agent, the exact wait it answered, and
    /// what was said, so a retry of the same send records once.
    static func instructionKey(_ reply: FleetReply, sessionID: String, waitSince: String?, text: String) -> String {
        let prefix = reply == .answer ? answerPrefix : sendBackPrefix
        let digest = SHA256.hash(data: Data("\(sessionID)\n\(waitSince ?? "")\n\(text)".utf8))
        return prefix + digest.map { String(format: "%02x", $0) }.joined()
    }

    /// Records an instruction the person sent from Home: an answer to the
    /// agent's question, or a finished result sent back with more to do.
    /// Both are `returned` (the work goes on) with the words as the note.
    /// Best effort: the message already went; a failed record is logged.
    static func recordInstruction(
        client: TendIPCClient,
        target: TendReviewTargetRef,
        reply: FleetReply,
        sessionID: String,
        waitSince: String?,
        question: String?,
        text: String
    ) async {
        let asked = question?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        let summary = !asked.isEmpty ? asked : (reply == .answer ? String(localized: "Answered in Home") : String(localized: "Sent back from Home"))
        do {
            try await client.recordDecision(
                target: target,
                key: instructionKey(reply, sessionID: sessionID, waitSince: waitSince, text: text),
                summary: summary,
                disposition: .returned,
                note: text
            )
        } catch {
            TendLog.app.warning("recording a Home decision failed: \(error.localizedDescription, privacy: .public)")
        }
    }
}

extension AppModel {
    /// The review target for an agent Home shows: its unit's Work, else its
    /// session.
    func homeReviewTarget(for hosted: TendHostedAgent) -> TendReviewTargetRef {
        if let work = homeWorks.first(where: { ($0.agents + $0.helpers).contains { $0.id == hosted.id } }),
           let target = FleetBoard.reviewTarget(work) {
            return target
        }
        return .session(provider: hosted.agent.kind, sessionID: hosted.agent.agentSessionID)
    }

    /// Records an answer the person chose or typed for a waiting agent.
    func recordHomeAnswer(_ hosted: TendHostedAgent, question: String?, answer: String) {
        let target = homeReviewTarget(for: hosted)
        let client = client
        let session = hosted.agent.agentSessionID
        let since = hosted.agent.stateEnteredAt
        Task {
            await FleetDecisionRecord.recordInstruction(
                client: client, target: target, reply: .answer, sessionID: session,
                waitSince: since, question: question, text: answer
            )
        }
    }
}
