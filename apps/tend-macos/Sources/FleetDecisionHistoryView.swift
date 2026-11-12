// What the person already decided, newest first, from tendd's record
// (`decision_history`): review dispositions, Home's Looks Good, Send Back
// and answers, and answered human requests. Read-only.
import SwiftUI

/// One decided item as the history lists it.
struct FleetDecidedRow: Identifiable, Hashable, Sendable {
    enum Verdict: Hashable, Sendable {
        case looksGood, sentBack, answered, accepted, returned, withdrawn
        /// A human request's reply status other than `completed`.
        case replied(String)
    }

    let id: String
    let verdict: Verdict
    /// What was decided: the question, or the result's summary.
    let title: String
    /// What the person said: the answer, or the note.
    let said: String?
    /// Who asked, or what it was about.
    let context: String?
    let at: Date?

    init(_ item: TendDecidedItem) {
        switch item {
        case .review(let review):
            id = "review:\(review.id)"
            let disposition = review.disposition
            switch (FleetDecisionRecord.kind(ofKey: review.idempotencyKey), disposition?.kind) {
            case (.looksGood?, _): verdict = .looksGood
            case (.sentBack?, _): verdict = .sentBack
            case (.answered?, _): verdict = .answered
            case (nil, "returned"): verdict = .returned
            case (nil, "withdrawn"): verdict = .withdrawn
            default: verdict = .accepted
            }
            title = review.decision?.needed ?? review.summary
            said = disposition?.note
            context = FleetDecisionRecord.kind(ofKey: review.idempotencyKey) == nil
                ? review.askedBy + " · " + review.target.text
                : review.target.text
            at = (disposition?.at).flatMap(TendUsageTimestamp.parse)
        case .humanAnswer(let answer):
            id = "human:\(answer.request.requestID)"
            verdict = answer.status == "completed" ? .answered : .replied(answer.status)
            title = answer.request.decision?.needed ?? answer.request.body
            said = answer.answer
            context = answer.request.fromAlias.map { "@\($0)" } ?? answer.request.fromPane
            at = TendUsageTimestamp.parse(answer.answeredAt)
        }
    }

    var label: String {
        switch verdict {
        case .looksGood: String(localized: "Looks Good")
        case .sentBack: String(localized: "Sent back")
        case .answered: String(localized: "Answered")
        case .accepted: String(localized: "Accepted")
        case .returned: String(localized: "Returned")
        case .withdrawn: String(localized: "Withdrawn")
        case .replied(let status): collaborationStatusLabel(status)
        }
    }

    var systemImage: String {
        switch verdict {
        case .looksGood, .accepted: "checkmark.circle.fill"
        case .sentBack, .returned: "arrow.uturn.backward.circle.fill"
        case .answered: "text.bubble.fill"
        case .withdrawn: "minus.circle.fill"
        case .replied: "xmark.circle.fill"
        }
    }

    var tint: Color {
        switch verdict {
        case .looksGood, .accepted: .green
        case .sentBack, .returned: .orange
        case .answered: .blue
        case .withdrawn, .replied: .secondary
        }
    }
}

struct FleetDecisionHistoryView: View {
    @ObservedObject var feed: FleetDecisionFeed
    let reload: () async -> Void
    @State private var filter = ""

    private var rows: [FleetDecidedRow] {
        let rows = (feed.history?.items ?? []).map(FleetDecidedRow.init)
        let query = filter.trimmingCharacters(in: .whitespaces)
        guard !query.isEmpty else { return rows }
        return rows.filter { row in
            [row.title, row.said, row.context, row.label].compactMap { $0 }
                .contains { $0.localizedCaseInsensitiveContains(query) }
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            if let history = feed.history {
                HStack(spacing: 8) {
                    TextField("Filter", text: $filter)
                        .textFieldStyle(.roundedBorder)
                        .frame(maxWidth: 260)
                    if history.truncated {
                        Text("Showing the latest \(history.items.count)")
                            .font(TendType.meta)
                            .foregroundStyle(.secondary)
                    }
                    Spacer()
                    Button {
                        Task { await reload() }
                    } label: {
                        Image(systemName: "arrow.clockwise")
                    }
                    .buttonStyle(.tendGhost)
                    .help("Check again")
                }
                .padding(.horizontal, 20)
                .padding(.vertical, 10)
                if !history.unavailable.isEmpty {
                    Label(String(localized: "Some decisions could not be read: \(history.unavailable.joined(separator: "; "))"),
                          systemImage: "exclamationmark.circle")
                        .font(TendType.meta)
                        .foregroundStyle(.orange)
                        .padding(.horizontal, 20)
                }
                let rows = rows
                if rows.isEmpty {
                    empty(history.items.isEmpty ? String(localized: "Nothing decided yet.") : String(localized: "No matching decisions."))
                } else {
                    ScrollView {
                        LazyVStack(alignment: .leading, spacing: 0) {
                            ForEach(rows) { row in
                                FleetDecidedRowView(row: row)
                                Divider().padding(.leading, 46)
                            }
                        }
                        .padding(.vertical, 4)
                    }
                }
            } else {
                empty(String(localized: "This tendd keeps no decision history. Update tend to record decisions."))
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .task { await reload() }
    }

    private func empty(_ text: String) -> some View {
        VStack(spacing: 8) {
            Image(systemName: "clock.arrow.circlepath")
                .font(.system(size: 32))
                .foregroundStyle(.secondary)
            Text(verbatim: text)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        .padding(32)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

struct FleetDecidedRowView: View {
    let row: FleetDecidedRow

    var body: some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: row.systemImage)
                .font(.system(size: 16))
                .foregroundStyle(row.tint)
                .frame(width: 20)
            VStack(alignment: .leading, spacing: 3) {
                HStack(alignment: .firstTextBaseline, spacing: 6) {
                    Text(verbatim: row.label)
                        .font(TendType.meta.weight(.semibold))
                        .foregroundStyle(row.tint)
                    if let context = row.context, !context.isEmpty {
                        Text(verbatim: context)
                            .font(TendType.meta)
                            .foregroundStyle(.secondary)
                            .lineLimit(1)
                            .truncationMode(.middle)
                    }
                    Spacer(minLength: 4)
                    if let at = row.at {
                        Text(at, format: .relative(presentation: .numeric, unitsStyle: .abbreviated))
                            .font(TendType.meta)
                            .monospacedDigit()
                            .foregroundStyle(.tertiary)
                            .help(at.formatted(date: .abbreviated, time: .shortened))
                    }
                }
                Text(verbatim: row.title)
                    .font(TendType.body.weight(.medium))
                    .lineLimit(2)
                    .textSelection(.enabled)
                if let said = row.said?.trimmingCharacters(in: .whitespacesAndNewlines), !said.isEmpty {
                    Text(verbatim: said)
                        .font(TendType.detail)
                        .foregroundStyle(.secondary)
                        .lineLimit(3)
                        .textSelection(.enabled)
                }
            }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 8)
        .accessibilityElement(children: .combine)
    }
}
