// Home's Backlog: Work the person planned and has not started
// (`work_backlog_v1`, `tend work plan`), in the daemon's backlog order.
// One action: Start, which runs `tend work up` from the saved plan.
import SwiftUI

/// Daemons that serve `work_backlog`, `work_edit` and `work_record_show`.
let tendWorkBacklogCapability = "work_backlog_v1"

/// A durable Work record as `work_backlog` returns it.
struct TendWorkRecord: Decodable, Hashable, Identifiable, Sendable {
    struct Metadata: Decodable, Hashable, Sendable {
        let title: String?
        let goal: String?
        let nextAction: String?
        let stage: String

        enum CodingKeys: String, CodingKey {
            case title, goal, stage
            case nextAction = "next_action"
        }

        init(from decoder: Decoder) throws {
            let values = try decoder.container(keyedBy: CodingKeys.self)
            title = try values.decodeIfPresent(String.self, forKey: .title)
            goal = try values.decodeIfPresent(String.self, forKey: .goal)
            nextAction = try values.decodeIfPresent(String.self, forKey: .nextAction)
            stage = try values.decodeIfPresent(String.self, forKey: .stage) ?? "auto"
        }
    }

    struct Plan: Decodable, Hashable, Sendable {
        let cwd: String?
        let pipeline: String?
        let prompt: String?
        let external: String?
        let priority: Int?
    }

    let identity: TendWorkIdentity
    let metadata: Metadata
    let plan: Plan?

    var id: String { identity.id }

    var title: String {
        metadata.title?.trimmingCharacters(in: .whitespacesAndNewlines).nilIfBlank ?? identity.workID
    }

    /// One line on what it is for: the goal, the next action, else the
    /// saved instruction.
    var line: String? {
        [metadata.goal, metadata.nextAction, plan?.prompt]
            .compactMap { $0?.split(whereSeparator: \.isNewline).first.map(String.init)?.trimmingCharacters(in: .whitespaces) }
            .first { !$0.isEmpty }
    }
}

extension TendIPCClient {
    /// The backlog: Work records at `stage` (`queued` for planned Work), in
    /// backlog order. nil from a daemon without `work_backlog_v1`.
    func workBacklog(stage: String = "queued") async throws -> [TendWorkRecord]? {
        guard supports(tendWorkBacklogCapability) else { return nil }
        return try await workRecords(stage: stage)
    }
}

/// Reads the backlog while Home is on screen.
@MainActor
final class FleetBacklogFeed: ObservableObject {
    /// nil while the daemon has no backlog to offer.
    @Published private(set) var records: [TendWorkRecord]?
    @Published private(set) var problem: String?
    @Published private(set) var starting: String?
    @Published var failures: [String: String] = [:]

    static let refreshEvery: Duration = .seconds(15)

    func load(_ client: TendIPCClient) async {
        do {
            let records = try await client.workBacklog()
            if self.records != records { self.records = records }
            problem = nil
        } catch {
            problem = String(localized: "Could not read the backlog: \(error.localizedDescription)")
        }
    }

    /// The planned Work Home shows: queued, not already on the board.
    func shown(on board: [FleetWork]) -> [TendWorkRecord] {
        let running = Set(board.compactMap { work -> TendWorkIdentity? in
            if case .work(let identity) = work.source { return identity }
            return nil
        })
        return (records ?? []).filter { $0.metadata.stage == "queued" && !running.contains($0.identity) }
    }

    /// Starts a planned Work from its plan (`tend work up`), on this Mac.
    func start(_ record: TendWorkRecord, model: AppModel) async {
        guard starting == nil else { return }
        starting = record.id
        failures[record.id] = nil
        defer { starting = nil }
        let started = await model.startWork(TendWorkStartRequest(
            work: record.identity.workID,
            workspace: record.identity.workspaceID,
            pipeline: nil,
            cwd: nil,
            external: record.plan?.external,
            skill: nil,
            body: nil,
            context: nil,
            dryRun: false
        ))
        if !started { failures[record.id] = model.workStartError ?? String(localized: "The Work did not start.") }
        await load(model.client)
    }
}

/// The Backlog column, left of Your turn, shown only when there is planned
/// Work.
struct FleetBacklogColumn: View {
    @ObservedObject var feed: FleetBacklogFeed
    let records: [TendWorkRecord]
    let start: (TendWorkRecord) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text("Backlog")
                    .font(.system(size: 13, weight: .bold))
                Text("\(records.count)")
                    .font(.system(size: 12, weight: .semibold))
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
                Spacer(minLength: 4)
                Text("Planned, not started")
                    .font(TendType.meta)
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
            }
            .padding(.horizontal, 12)
            .padding(.top, 10)
            .padding(.bottom, 8)
            ScrollView {
                LazyVStack(spacing: 8) {
                    if let problem = feed.problem {
                        Label(problem, systemImage: "exclamationmark.circle")
                            .font(TendType.meta)
                            .foregroundStyle(.orange)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                    ForEach(records) { record in
                        FleetBacklogCard(
                            record: record,
                            starting: feed.starting == record.id,
                            busy: feed.starting != nil,
                            failure: feed.failures[record.id],
                            start: { start(record) }
                        )
                    }
                }
                .padding(.horizontal, 8)
                .padding(.bottom, 10)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .background(TendTheme.panelFill, in: RoundedRectangle(cornerRadius: 9, style: .continuous))
    }
}

struct FleetBacklogCard: View {
    let record: TendWorkRecord
    var starting = false
    var busy = false
    var failure: String?
    let start: () -> Void
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(verbatim: record.title)
                    .font(.system(size: 13, weight: .semibold))
                    .lineLimit(2)
                    .frame(maxWidth: .infinity, alignment: .leading)
                if let priority = record.plan?.priority {
                    Text(verbatim: "P\(priority)")
                        .font(.system(size: 10, weight: .semibold))
                        .monospacedDigit()
                        .padding(.horizontal, 5)
                        .padding(.vertical, 1)
                        .background(Color.secondary.opacity(0.15), in: Capsule())
                        .foregroundStyle(.secondary)
                        .help(String(localized: "Backlog priority: lower starts first"))
                }
            }
            if let line = record.line {
                Text(verbatim: line)
                    .font(TendType.body)
                    .foregroundStyle(.secondary)
                    .lineLimit(2)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            HStack(spacing: 6) {
                Text(verbatim: [record.identity.workspaceID, record.plan?.pipeline, record.plan?.external]
                    .compactMap { $0 }.joined(separator: " · "))
                    .lineLimit(1)
                    .truncationMode(.middle)
                Spacer(minLength: 4)
                Button {
                    start()
                } label: {
                    if starting {
                        ProgressView().controlSize(.small).scaleEffect(0.7)
                    } else {
                        Label("Start", systemImage: "play.fill")
                    }
                }
                .buttonStyle(.tendSecondary)
                .disabled(busy)
                .help(String(localized: "Start this Work from its plan (tend work up)"))
            }
            .font(TendType.meta)
            .foregroundStyle(.tertiary)
            if let failure {
                Label(failure, systemImage: "exclamationmark.circle")
                    .font(TendType.meta)
                    .foregroundStyle(.red)
                    .lineLimit(3)
            }
        }
        .padding(12)
        .background(TendSurfacePalette.workspace(for: colorScheme), in: RoundedRectangle(cornerRadius: 8, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: 8, style: .continuous)
                .strokeBorder(TendTheme.border(colorScheme), lineWidth: 1)
        )
    }
}

private extension String {
    var nilIfBlank: String? { isEmpty ? nil : self }
}
