// Inbox: commands sent to agents, peers' requests, and collaboration.
import AppKit
import SwiftUI

struct MuxaOperatorInboxView: View {
    private enum Scope: CaseIterable, Identifiable {
        case all
        case replies
        case waiting
        case action
        var id: Self { self }

        var title: LocalizedStringKey {
            switch self {
            case .all: "All"
            case .replies: "Replies"
            case .waiting: "Waiting"
            case .action: "Needs Action"
            }
        }
    }

    @ObservedObject var model: AppModel
    @State private var scope: Scope = .all
    @State private var search = ""
    @State private var selectedMessageID: String?
    @State private var compactShowingDetail = false
    @State private var showingHostFailureDetails = false

    private var visibleMessages: [MuxaOperatorMessage] {
        let filtered = model.operatorMessages.filter { message in
            // Waiting shows only requests the agent can still answer; a
            // blocked/declined/failed request without a reply is an operator
            // decision and lives under Needs Action instead.
            let scopeMatches = switch scope {
            case .all: true
            case .replies: message.request.reply != nil
            case .waiting: message.isAwaitingAgentReply
            case .action: message.needsHumanDecision
            }
            guard scopeMatches else { return false }
            guard !search.isEmpty else { return true }
            let request = message.request
            return [
                request.body,
                request.reply?.body,
                request.to.label,
                request.to.windowName,
                request.to.sessionName,
                request.workspaceID,
                request.workID,
                message.host.alias,
            ].compactMap { $0 }.contains {
                $0.localizedCaseInsensitiveContains(search)
            }
        }
        // Needs Action is a queue: unread decisions first, then the most
        // recently changed conversation. Other scopes keep the model's
        // sent-time order so the list does not reorder while reading.
        guard scope == .action else { return filtered }
        return filtered.sorted(by: MuxaOperatorMessage.needsActionOrder)
    }


    private var unreadReplies: Int {
        model.operatorMessages.lazy.filter(\.hasUnreadReply).count
    }

    private var waitingReplies: Int {
        model.operatorMessages.lazy.filter(\.isAwaitingAgentReply).count
    }

    private var humanDecisions: Int {
        model.operatorMessages.lazy.filter(\.needsHumanDecision).count
    }

    private var selectedMessage: MuxaOperatorMessage? {
        visibleMessages.first { $0.id == selectedMessageID }
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                Label("Operator Inbox", systemImage: "tray.full")
                    .font(.headline)
                inboxMetric(Text("\(unreadReplies) New"), unreadReplies, color: .orange)
                inboxMetric(Text("\(waitingReplies) Waiting"), waitingReplies, color: .blue)
                inboxMetric(Text("\(humanDecisions) Action"), humanDecisions, color: .red)
                Spacer(minLength: 8)
                if model.isRefreshingInbox { ProgressView().controlSize(.small) }
                Button {
                    Task { await model.refreshOperatorInbox(force: true) }
                } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
            }
            .padding(.horizontal, 14)
            .frame(height: 42)

            Divider()

            ViewThatFits(in: .horizontal) {
                HStack(spacing: 10) {
                    inboxScopePicker.frame(maxWidth: 420)
                    inboxSearchField
                }
                VStack(spacing: 8) {
                    inboxScopePicker
                    inboxSearchField
                }
            }
            .padding(10)

            if let summary = model.inboxHostFailureSummary {
                inboxHostFailureLine(summary)
            }

            if let error = model.inboxError {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(MuxaType.detail)
                    .foregroundStyle(.orange)
                    .textSelection(.enabled)
                    .padding(.horizontal, 12)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }

            operatorMasterDetail
        }
        .task {
            await model.refreshOperatorInbox(force: true)
            reconcileMessageSelection()
        }
        .onChange(of: visibleMessages.map(\.id)) { _ in reconcileMessageSelection() }
        .onChange(of: model.pendingOperatorMessageID) { _ in reconcileMessageSelection() }
        .onChange(of: scope) { _ in
            if !visibleMessages.contains(where: { $0.id == selectedMessageID }) {
                compactShowingDetail = false
            }
            reconcileMessageSelection()
        }
    }

    /// One compact advisory line for hosts whose most recent mailbox read
    /// failed. The list below still shows the last messages received from
    /// them, so this never replaces the list. The full per-host reasons are
    /// available as a tooltip and behind the chevron.
    private func inboxHostFailureLine(_ summary: String) -> some View {
        let details = MuxaInboxHostFailureText.details(model.inboxHostFailures)
        return VStack(alignment: .leading, spacing: 4) {
            Button {
                showingHostFailureDetails.toggle()
            } label: {
                HStack(spacing: 6) {
                    Label(summary, systemImage: "wifi.exclamationmark")
                        .font(MuxaType.detail)
                        .foregroundStyle(.orange)
                        .lineLimit(1)
                    Image(systemName: showingHostFailureDetails ? "chevron.down" : "chevron.right")
                        .font(MuxaType.meta.weight(.semibold))
                        .foregroundStyle(.secondary)
                    Text("Showing their last known messages")
                        .font(MuxaType.detail)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help(details.joined(separator: "\n"))
            .accessibilityLabel("Unreachable hosts")
            .accessibilityValue(summary)

            if showingHostFailureDetails {
                ForEach(details, id: \.self) { line in
                    Text(line)
                        .font(MuxaType.detail.monospaced())
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .lineLimit(2)
                        .padding(.leading, 22)
                }
            }
        }
        .padding(.horizontal, 12)
        .padding(.bottom, 6)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func inboxMetric(_ text: Text, _ value: Int, color: Color) -> some View {
        text
            .font(MuxaType.meta.weight(.semibold).monospacedDigit())
            .foregroundStyle(value > 0 ? color : Color.secondary)
            .padding(.horizontal, 7)
            .padding(.vertical, 3)
            .background((value > 0 ? color : Color.secondary).opacity(0.1), in: Capsule())
    }

    private var operatorMasterDetail: some View {
        GeometryReader { geometry in
            if geometry.size.width >= 760 {
                HSplitView {
                    operatorMessageList
                        .frame(minWidth: 300, idealWidth: 380, maxWidth: 470)
                    operatorMessageDetail
                        .frame(minWidth: 420, maxWidth: .infinity, maxHeight: .infinity)
                }
            } else if compactShowingDetail, selectedMessage != nil {
                VStack(spacing: 0) {
                    HStack {
                        Button {
                            compactShowingDetail = false
                        } label: {
                            Label("Inbox", systemImage: "chevron.left")
                        }
                        .buttonStyle(.muxaGhost)
                        Spacer()
                    }
                    .padding(.horizontal, 12)
                    .frame(height: 36)
                    Divider()
                    operatorMessageDetail
                }
            } else {
                operatorMessageList
            }
        }
    }

    private var operatorMessageList: some View {
        VStack(spacing: 0) {
            HStack {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Commands and replies")
                        .font(.subheadline.weight(.semibold))
                    Text("\(visibleMessages.count) conversations across reachable hosts")
                        .font(MuxaType.detail)
                        .foregroundStyle(.secondary)
                }
                Spacer()
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 10)
            Divider()

            if visibleMessages.isEmpty {
                ConsoleUnavailableView(
                    title: model.operatorMessages.isEmpty ? LocalizedStringKey("No commands sent yet") : LocalizedStringKey("No matching commands"),
                    systemImage: "paperplane",
                    description: "Use Collaborate on an agent pane. Its reply will appear here without reopening that pane."
                )
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ScrollView {
                    LazyVStack(spacing: 6) {
                        ForEach(visibleMessages) { message in
                            Button {
                                selectedMessageID = message.id
                                compactShowingDetail = true
                                if message.hasUnreadReply {
                                    Task { await model.markOperatorMessageRead(message) }
                                }
                            } label: {
                                OperatorMessageRow(
                                    message: message,
                                    selected: message.id == selectedMessageID
                                )
                            }
                            .buttonStyle(.plain)
                            .contentShape(Rectangle())
                        }
                    }
                    .padding(8)
                }
            }
        }
    }

    @ViewBuilder
    private var operatorMessageDetail: some View {
        if let selectedMessage {
            OperatorMessageDetail(message: selectedMessage, model: model)
        } else {
            ConsoleUnavailableView(
                title: "Select a message",
                systemImage: "rectangle.righthalf.inset.filled",
                description: "Choose a compact preview to read the full sent message and reply."
            )
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    private func reconcileMessageSelection() {
        if let pending = model.pendingOperatorMessageID,
           model.operatorMessages.contains(where: { $0.id == pending }) {
            scope = .all
            search = ""
            selectedMessageID = pending
            compactShowingDetail = true
            model.pendingOperatorMessageID = nil
            return
        }
        if let selectedMessageID,
           visibleMessages.contains(where: { $0.id == selectedMessageID }) {
            return
        }
        selectedMessageID = visibleMessages.first?.id
    }

    private var inboxScopePicker: some View {
        MuxaSegmented(selection: $scope, options: Array(Scope.allCases)) { Text($0.title) }
            .accessibilityLabel("Inbox scope")
    }

    private var inboxSearchField: some View {
        HStack(spacing: 6) {
            Image(systemName: "magnifyingglass")
                .foregroundStyle(.secondary)
            TextField("Search command, reply, Work, or agent", text: $search)
                .textFieldStyle(.plain)
        }
        .padding(.horizontal, 9)
        .padding(.vertical, 6)
        .background(Color.primary.opacity(0.055), in: RoundedRectangle(cornerRadius: 7))
    }
}

private struct OperatorMessageRow: View {
    let message: MuxaOperatorMessage
    let selected: Bool

    private var request: MuxaCollaborationRequest { message.request }
    private var statusColor: Color {
        if request.needsHumanResponse { return .orange }
        if request.reply?.status == "completed" { return .green }
        if ["blocked", "failed", "declined", "expired", "cancelled"].contains(request.status) {
            return .red
        }
        if request.reply != nil { return .green }
        return request.expectsReply ? .blue : .secondary
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 7) {
                Circle().fill(statusColor).frame(width: 7, height: 7)
                Text(message.counterpart.label)
                    .font(.subheadline.weight(.semibold))
                    .lineLimit(1)
                Spacer(minLength: 6)
                Text(request.needsHumanResponse ? String(localized: "Needs Action") : collaborationStatusLabel(request.reply?.status ?? request.status))
                    .font(MuxaType.detail.weight(.semibold))
                    .foregroundStyle(statusColor)
                Text(compactInboxTimestamp(request.createdAt))
                    .font(MuxaType.detail.monospacedDigit())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }

            HStack(spacing: 8) {
                Label(message.host.alias, systemImage: "server.rack")
                    .font(MuxaType.detail.monospaced())
                    .foregroundStyle(.secondary)
                if let work = request.workID {
                    Text([request.workspaceID, work].compactMap { $0 }.joined(separator: " / "))
                        .font(MuxaType.detail.weight(.medium))
                        .foregroundStyle(Color.accentColor)
                        .lineLimit(1)
                }
            }

            VStack(alignment: .leading, spacing: 6) {
                Label("Sent", systemImage: "paperplane.fill")
                    .font(.subheadline.weight(.semibold))
                    .foregroundStyle(.secondary)
                Text(inboxPreview(request.body))
                    .font(.callout)
                    .foregroundStyle(.primary)
                    .lineLimit(2)
                    .multilineTextAlignment(.leading)
            }

            if let reply = request.reply {
                VStack(alignment: .leading, spacing: 7) {
                    HStack {
                        Label(message.hasUnreadReply ? "New Reply" : "Reply", systemImage: "arrowshape.turn.up.left.fill")
                            .font(.subheadline.weight(.semibold))
                            .foregroundStyle(message.hasUnreadReply ? Color.orange : Color.secondary)
                        Spacer()
                        Text(compactInboxTimestamp(reply.at))
                            .font(MuxaType.detail.monospacedDigit())
                            .foregroundStyle(.secondary)
                    }
                    Text(inboxPreview(reply.body))
                        .font(.callout)
                        .foregroundStyle(.primary)
                        .lineLimit(2)
                        .multilineTextAlignment(.leading)
                }
                .padding(11)
                .background(
                    (message.hasUnreadReply ? Color.orange : Color.primary).opacity(0.06),
                    in: RoundedRectangle(cornerRadius: 8)
                )
            } else if request.needsHumanResponse {
                Label("Your response is required", systemImage: "person.crop.circle.badge.exclamationmark")
                    .font(.callout).foregroundStyle(.orange)
            } else if message.needsHumanDecision {
                // The request itself is blocked/declined/failed and no reply
                // will arrive, so name the decision instead of a wait.
                Label(
                    "Needs your decision: \(inboxStatusTitle(request.status))",
                    systemImage: "exclamationmark.triangle.fill"
                )
                .font(.callout)
                .foregroundStyle(.red)
            } else if message.isAwaitingAgentReply {
                Label("Waiting for this agent to reply", systemImage: "clock")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(13)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            selected ? Color.accentColor.opacity(0.12) : Color.primary.opacity(0.035),
            in: RoundedRectangle(cornerRadius: 9)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 9)
                .stroke(
                    selected
                        ? Color.accentColor.opacity(0.65)
                        : message.hasUnreadReply
                            ? Color.orange.opacity(0.5)
                            : Color(nsColor: .separatorColor).opacity(0.4),
                    lineWidth: selected || message.hasUnreadReply ? 1 : 0.5
                )
        }
        .contentShape(RoundedRectangle(cornerRadius: 9))
    }
}

private struct OperatorMessageDetail: View {
    let message: MuxaOperatorMessage
    @ObservedObject var model: AppModel
    @State private var replying = false

    private var request: MuxaCollaborationRequest { message.request }
    private var openDestination: MuxaSidebarSelection? {
        AppModel.operatorSelection(for: message, in: model.executionSnapshot)
    }
    private var openDestinationLabel: String {
        switch openDestination {
        case .agent, .pane: String(localized: "Open Agent")
        case .fleetWindow: String(localized: "Open Window")
        case .fleetSession: String(localized: "Open Session")
        case .host: String(localized: "Open Host")
        case nil: String(localized: "Agent Ended")
        default: String(localized: "Open Context")
        }
    }
    private var statusColor: Color {
        if request.needsHumanResponse { return .orange }
        if request.reply?.status == "completed" { return .green }
        if ["blocked", "failed", "declined", "expired", "cancelled"].contains(request.status) {
            return .red
        }
        if request.reply != nil { return .green }
        return request.expectsReply ? .blue : .secondary
    }

    private var location: String {
        [message.counterpart.sessionName, message.counterpart.windowName, message.counterpart.pane]
            .compactMap { $0?.isEmpty == false ? $0 : nil }
            .joined(separator: " › ")
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 9) {
                Circle().fill(statusColor).frame(width: 8, height: 8)
                VStack(alignment: .leading, spacing: 2) {
                    Text(message.counterpart.label)
                        .font(.headline)
                        .lineLimit(1)
                    Text(verbatim: "\(message.host.alias) · \(compactInboxTimestamp(request.createdAt))")
                        .font(MuxaType.detail)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                if message.hasUnreadReply {
                    Button("Mark Read") {
                        Task { await model.markOperatorMessageRead(message) }
                    }
                }
                Button {
                    model.openOperatorMessage(message)
                } label: {
                    Label(openDestinationLabel, systemImage: "rectangle.and.hand.point.up.left")
                }
                .buttonStyle(.muxaPrimary)
                .disabled(openDestination == nil)
            }
            .padding(.horizontal, 16)
            .frame(minHeight: 54)

            Divider()

            ScrollView {
                VStack(alignment: .leading, spacing: 20) {
                    HStack(spacing: 10) {
                        Label(message.host.alias, systemImage: "server.rack")
                        if let work = request.workID {
                            Label(
                                [request.workspaceID, work].compactMap { $0 }.joined(separator: " / "),
                                systemImage: "briefcase"
                            )
                        }
                        if !location.isEmpty {
                            Label(location, systemImage: "rectangle.split.3x1")
                                .lineLimit(1)
                        }
                    }
                    .font(MuxaType.detail)
                    .foregroundStyle(.secondary)

                    OperatorMessageDetailSection(
                        title: message.isHumanRequest ? "Request for you" : "Sent",
                        icon: "paperplane.fill",
                        timestamp: compactInboxTimestamp(request.createdAt),
                        source: request.body,
                        tint: .accentColor
                    )

                    if let reply = request.reply {
                        OperatorMessageDetailSection(
                            title: message.hasUnreadReply ? "New Reply" : "Reply",
                            icon: "arrowshape.turn.up.left.fill",
                            timestamp: compactInboxTimestamp(reply.at),
                            source: reply.body,
                            tint: message.hasUnreadReply ? .orange : .green
                        )
                    } else if request.needsHumanResponse {
                        Button("Respond…") { replying = true }.buttonStyle(.muxaPrimary)
                    } else if message.needsHumanDecision {
                        Label(
                            "Needs your decision: \(inboxStatusTitle(request.status)). The agent will not reply to this request.",
                            systemImage: "exclamationmark.triangle.fill"
                        )
                        .font(.subheadline.weight(.medium))
                        .foregroundStyle(.red)
                        .padding(14)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(Color.red.opacity(0.06), in: RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous))
                    } else if message.isAwaitingAgentReply {
                        Label("Waiting for this agent to reply", systemImage: "clock")
                            .font(.subheadline.weight(.medium))
                            .foregroundStyle(.secondary)
                            .padding(14)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .background(Color.primary.opacity(0.04), in: RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous))
                    }
                }
                .padding(18)
                .frame(maxWidth: 860, alignment: .leading)
                .frame(maxWidth: .infinity, alignment: .topLeading)
            }
        }
        .background(Color(nsColor: .textBackgroundColor).opacity(0.28))
        .sheet(isPresented: $replying) {
            CollaborationReplyView(
                request: request,
                pane: MuxaWatchPane(host: message.host, pane: message.routePane, agent: nil),
                client: model.client
            ) {
                replying = false
                Task { await model.refreshOperatorInbox(force: true, hostAliases: [message.host.alias]) }
            }
        }
    }
}

private struct OperatorMessageDetailSection: View {
    let title: LocalizedStringKey
    let icon: String
    let timestamp: String
    let source: String
    let tint: Color

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Label(title, systemImage: icon)
                    .font(.title3.weight(.semibold))
                    .foregroundStyle(tint)
                Spacer()
                Text(timestamp)
                    .font(MuxaType.detail.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
            ReadableMarkdownContent(source: source)
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(tint.opacity(0.055), in: RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous))
        .overlay(alignment: .leading) {
            RoundedRectangle(cornerRadius: 2)
                .fill(tint.opacity(0.7))
                .frame(width: 3)
                .padding(.vertical, 10)
        }
    }
}

private func inboxPreview(_ value: String) -> String {
    MuxaMarkdownText.previewText(markdown: value)
}

/// "waiting_reply" -> "Waiting Reply", matching the status pill wording.
private func inboxStatusTitle(_ status: String) -> String {
    collaborationStatusLabel(status)
}

func compactInboxTimestamp(_ value: String) -> String {
    let normalized = value.replacingOccurrences(of: "T", with: " ")
    return String(normalized.prefix(16))
}

struct MuxaCollaborationView: View {
    private enum ModuleTab: CaseIterable, Identifiable {
        case activity
        case compose
        var id: Self { self }

        var title: LocalizedStringKey {
            switch self {
            case .activity: "Activity"
            case .compose: "Compose"
            }
        }
    }

    private enum MailboxTab: CaseIterable, Identifiable {
        case human
        case incoming
        case sent
        var id: Self { self }
    }

    private enum DisplayMode: CaseIterable, Identifiable {
        case compact
        case detailed
        var id: Self { self }

        var title: LocalizedStringKey {
            switch self {
            case .compact: "Compact"
            case .detailed: "Detailed"
            }
        }
    }

    let pane: MuxaWatchPane
    let client: MuxaIPCClient
    let mailboxRevision: UInt64?
    @State private var mailbox = MuxaCollaborationMailbox(incoming: [], sent: [])
    @State private var module: ModuleTab = .activity
    @State private var tab: MailboxTab = .human
    @State private var displayMode: DisplayMode = .compact
    @State private var kind = "question"
    @State private var workMode = "read_only"
    @State private var message = ""
    @State private var loading = false
    @State private var sending = false
    @State private var error: String?
    @State private var replyingTo: MuxaCollaborationRequest?

    private var requests: [MuxaCollaborationRequest] {
        switch tab {
        case .human: mailbox.incoming.filter { $0.needsHumanResponse }
        case .incoming: mailbox.incoming.filter { $0.to.console != true }
        case .sent: mailbox.sent
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Label("Collaborate", systemImage: "person.2.wave.2")
                    .font(.headline)
                MuxaSegmented(selection: $module, options: Array(ModuleTab.allCases)) { Text($0.title) }
                    .accessibilityLabel("Collaborate module")
                Spacer()
                if loading { ProgressView().controlSize(.mini) }
                Button { Task { await load() } } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .buttonStyle(.muxaIcon)
            }
            .padding(.horizontal, 14)
            .frame(height: 40)

            Divider()

            switch module {
            case .activity:
                HStack(spacing: 10) {
                    MuxaSegmented(selection: $tab, options: [MailboxTab.human, .incoming, .sent]) { tab in
                        switch tab {
                        case .human: Text("Need you \(mailbox.incoming.filter { $0.needsHumanResponse }.count)")
                        case .incoming: Text("Incoming \(mailbox.incoming.count)")
                        case .sent: Text("Sent \(mailbox.sent.count)")
                        }
                    }
                    .accessibilityLabel("Mailbox")
                    Spacer()
                    MuxaSegmented(selection: $displayMode, options: Array(DisplayMode.allCases)) { Text($0.title) }
                        .accessibilityLabel("Density")
                }
                .padding(.horizontal, 12)
                .frame(height: 36)

                Divider()

                ScrollView {
                    LazyVStack(alignment: .leading, spacing: displayMode == .compact ? 4 : 9) {
                        CollaborationFlowGraph(requests: mailbox.incoming + mailbox.sent)
                        if requests.isEmpty {
                            Text(tab == .human ? "No requests need your response." : "No messages in this mailbox.")
                                .foregroundStyle(.secondary)
                                .frame(maxWidth: .infinity, minHeight: 90, alignment: .center)
                        } else {
                            ForEach(requests) { request in
                                HStack {
                                    Text(request.from.label)
                                    Image(systemName: "arrow.right")
                                    Text(request.to.label)
                                    if request.reply != nil {
                                        Image(systemName: "arrow.turn.down.left")
                                        Text("Reply to \(request.from.label)")
                                    }
                                    if request.needsHumanResponse {
                                        Label(request.humanAction ?? "information", systemImage: "person.crop.circle.badge.exclamationmark")
                                            .foregroundStyle(.orange)
                                    }
                                }
                                .font(.caption)
                                CollaborationRequestCard(
                                    request: request,
                                    incoming: tab != .sent,
                                    compact: displayMode == .compact,
                                    claim: { Task { await claim() } },
                                    reply: { replyingTo = request }
                                )
                            }
                        }
                    }
                    .padding(displayMode == .compact ? 8 : 12)
                }
            case .compose:
                collaborationComposer
            }
        }
        .task(id: "\(pane.id)-\(mailboxRevision ?? 0)") { await load() }
        .sheet(item: $replyingTo) { request in
            CollaborationReplyView(request: request, pane: pane, client: client) {
                replyingTo = nil
                Task { await load() }
            }
        }
    }

    private var collaborationComposer: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Picker("Kind", selection: $kind) {
                    Text("Question").tag("question")
                    Text("Review").tag("review")
                    Text("Task").tag("task")
                    Text("Notice").tag("notice")
                }
                .frame(width: 130)
                Picker("Mode", selection: $workMode) {
                    Text("Read only").tag("read_only")
                    Text("Execute").tag("execute")
                }
                .frame(width: 130)
                Text("to \(pane.pane.agentAlias.map { "@\($0)" } ?? pane.pane.paneID)")
                    .font(MuxaType.detail.monospaced())
                    .foregroundStyle(.secondary)
                Spacer()
            }
            TextEditor(text: $message)
                .scrollContentBackground(.hidden)
                .padding(8)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Color.primary.opacity(0.055), in: RoundedRectangle(cornerRadius: 8))
            HStack {
                if let error {
                    Text(error).font(MuxaType.detail).foregroundStyle(.red).lineLimit(2)
                }
                Spacer()
                if sending { ProgressView().controlSize(.small) }
                Button("Send Collaboration") { send() }
                    .buttonStyle(.muxaPrimary)
                    .disabled(sending || message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(14)
    }

    private func load() async {
        guard !loading else { return }
        loading = true
        defer { loading = false }
        do {
            mailbox = try await client.collaborationMailbox(host: pane.host, pane: pane.pane)
            error = nil
        } catch {
            self.error = error.localizedDescription
        }
    }

    private func send() {
        let submitted = message
        sending = true
        error = nil
        Task {
            defer { sending = false }
            do {
                _ = try await client.sendCollaboration(
                    host: pane.host,
                    pane: pane.pane,
                    kind: kind,
                    body: submitted,
                    workMode: workMode
                )
                message = ""
                tab = .sent
                module = .activity
                await load()
            } catch {
                self.error = error.localizedDescription
            }
        }
    }

    private func claim() async {
        do {
            try await client.claimCollaboration(host: pane.host, pane: pane.pane)
            await load()
        } catch {
            self.error = error.localizedDescription
        }
    }
}

/// A chronological participant graph. Request and reply arrows stay paired;
/// human attention is a live property, not an interpretation of message text.
private struct CollaborationFlowGraph: View {
    let requests: [MuxaCollaborationRequest]

    private var uniqueRequests: [MuxaCollaborationRequest] {
        var seen = Set<String>()
        return requests.sorted { $0.createdAt < $1.createdAt }
            .filter { seen.insert($0.id).inserted }
            .suffix(20).map { $0 }
    }

    var body: some View {
        let rows = uniqueRequests
        VStack(alignment: .leading, spacing: 6) {
            Text("Conversation flow · latest 20 requests")
                .font(.caption).foregroundStyle(.secondary)
            ScrollView(.horizontal) {
            Canvas { context, size in
                let participants = rows.flatMap { [$0.from, $0.to] }
                var seen = Set<String>()
                let nodes = participants.filter { seen.insert(identity($0)).inserted }
                let step = size.width / CGFloat(max(nodes.count, 1))
                for (index, node) in nodes.enumerated() {
                    let x = step * (CGFloat(index) + 0.5)
                    context.draw(Text(node.label).font(.caption2), at: CGPoint(x: x, y: 12))
                    var lane = Path()
                    lane.move(to: CGPoint(x: x, y: 25))
                    lane.addLine(to: CGPoint(x: x, y: size.height))
                    context.stroke(lane, with: .color(.gray.opacity(0.3)), style: StrokeStyle(lineWidth: 1, dash: [3, 3]))
                }
                for (index, request) in rows.enumerated() {
                    guard let from = nodes.firstIndex(where: { identity($0) == identity(request.from) }),
                          let to = nodes.firstIndex(where: { identity($0) == identity(request.to) }) else { continue }
                    let start = step * (CGFloat(from) + 0.5)
                    let end = step * (CGFloat(to) + 0.5)
                    let y = CGFloat(index) * 48 + 42
                    let color: Color = (request.needsHumanResponse || request.peerInterrupted) ? .orange : .blue
                    context.stroke(arrow(from: start, to: end, y: y), with: .color(color), lineWidth: 1.5)
                    let label = request.needsHumanResponse ? "Need you: \(request.humanAction ?? "information")" : (request.peerInterrupted ? "Interrupted: \(request.interruption?.reason ?? "unknown")" : request.kind)
                    context.draw(Text(label).font(.caption2).foregroundColor(color), at: CGPoint(x: (start + end) / 2, y: y - 9))
                    if request.reply != nil {
                        context.stroke(arrow(from: end, to: start, y: y + 18), with: .color(.green), style: StrokeStyle(lineWidth: 1, dash: [4, 2]))
                    }
                }
            }
            .frame(width: CGFloat(max(Set(rows.flatMap { [identity($0.from), identity($0.to)] }).count, 2)) * 140, height: CGFloat(rows.count) * 48 + 30)
            .accessibilityLabel("Request arrows point to recipients; dashed replies return to senders. Orange requests need your response.")
            }
        }
    }

    private func identity(_ participant: MuxaCollaborationParticipant) -> String {
        if participant.console == true { return "operator" }
        return "\(participant.room.host)|\(participant.socket ?? participant.room.socket ?? "default")|\(participant.agentSessionID)"
    }

    private func arrow(from: CGFloat, to: CGFloat, y: CGFloat) -> Path {
        let direction: CGFloat = to >= from ? 1 : -1
        var path = Path()
        path.move(to: CGPoint(x: from, y: y))
        path.addLine(to: CGPoint(x: to, y: y))
        path.move(to: CGPoint(x: to - direction * 6, y: y - 4))
        path.addLine(to: CGPoint(x: to, y: y))
        path.addLine(to: CGPoint(x: to - direction * 6, y: y + 4))
        return path
    }
}

private struct CollaborationRequestCard: View {
    let request: MuxaCollaborationRequest
    let incoming: Bool
    let compact: Bool
    let claim: () -> Void
    let reply: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 7) {
            HStack(spacing: 7) {
                Text(collaborationKindLabel(request.kind))
                    .font(MuxaType.detail.weight(.semibold))
                Text(request.workMode == "execute" ? "Execute" : "Read only")
                    .font(MuxaType.meta)
                    .foregroundStyle(request.workMode == "execute" ? Color.orange : Color.secondary)
                Text(collaborationStatusLabel(request.status))
                    .font(MuxaType.meta.weight(.medium))
                    .foregroundStyle(collaborationStatusColor(request.status))
                Spacer()
                Text(verbatim: "\(request.from.label) → \(request.to.label)")
                    .font(MuxaType.meta.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            if let recovery = request.interruption {
                VStack(alignment: .leading, spacing: 4) {
                    Label(recovery.active ? "Peer interrupted: \(recovery.reason)" : "Peer interruption cleared", systemImage: "exclamationmark.triangle")
                        .foregroundStyle(recovery.active ? .orange : .secondary)
                    Text("Original request: \(recovery.requestID)").textSelection(.enabled)
                    Text("Reset: \(recovery.resetAt ?? "unknown")")
                    if let action = recovery.actionRequestID {
                        Text("Recovery decision: \(action)").textSelection(.enabled)
                    }
                    if let decision = recovery.decision { Text("Decision (\(decision.status)): \(decision.body)") }
                    Text("A decision does not automatically restart or reassign work.")
                        .foregroundStyle(.secondary)
                }.font(.caption)
            }
            MarkdownContent(source: request.body, lineLimit: compact ? 2 : nil)
            if let response = request.reply {
                if !compact {
                    Divider()
                    MarkdownContent(source: response.body)
                }
                Label(collaborationStatusLabel(response.status), systemImage: "arrowshape.turn.up.left.fill")
                    .font(MuxaType.meta)
                    .foregroundStyle(collaborationStatusColor(response.status))
            }
            if incoming, request.reply == nil {
                HStack {
                    Spacer()
                    if request.status == "queued" && !request.needsHumanResponse {
                        Button("Claim", action: claim)
                    }
                    if request.status == "claimed" || request.needsHumanResponse {
                        Button("Reply…", action: reply)
                            .buttonStyle(.muxaPrimary)
                    }
                }
                .controlSize(.small)
            }
        }
        .padding(compact ? 8 : 11)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            Color.primary.opacity(compact ? 0.035 : 0.065),
            in: RoundedRectangle(cornerRadius: 8)
        )
    }
}

private struct CollaborationReplyView: View {
    let request: MuxaCollaborationRequest
    let pane: MuxaWatchPane
    let client: MuxaIPCClient
    let completed: () -> Void
    @State private var status = "completed"
    @State private var replyText = ""
    @State private var sending = false
    @State private var error: String?

    var replyBody: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Reply to \(request.from.label)")
                .font(.system(size: 14, weight: .semibold))
            MarkdownContent(source: request.body)
                .padding(10)
                .background(Color.primary.opacity(0.05), in: RoundedRectangle(cornerRadius: 7))
            if request.needsHumanResponse && request.interruption != nil {
                HStack {
                    Button("Wait and recheck") { replyText = "Wait and recheck the original peer. Verify fresh health and progress before resuming." }
                    Button("Safe handoff") { replyText = "Arrange a safe handoff within the existing scope. Verify no overlapping execution before reassignment." }
                    Button("Stop attempt") { replyText = "Stop this work attempt using authorized controls and preserve existing artifacts." }
                }
                Text("Choose a draft, edit it, then Reply. This records your decision only.").font(.caption)
            }
            Picker("Outcome", selection: $status) {
                Text("Completed").tag("completed")
                Text("Blocked").tag("blocked")
                Text("Declined").tag("declined")
                Text("Failed").tag("failed")
            }
            TextEditor(text: $replyText)
                .frame(minHeight: 120)
                .padding(7)
                .background(Color.primary.opacity(0.05), in: RoundedRectangle(cornerRadius: 7))
            if let error { Text(error).font(MuxaType.detail).foregroundStyle(.red) }
            HStack {
                Spacer()
                Button("Cancel", action: completed)
                Button("Reply") { send() }
                    .buttonStyle(.muxaPrimary)
                    .disabled(sending || replyText.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(20)
        .frame(width: 520, height: 430)
    }

    var body: some View { replyBody }

    private func send() {
        sending = true
        Task {
            defer { sending = false }
            do {
                try await client.replyCollaboration(
                    host: pane.host,
                    pane: pane.pane,
                    requestID: request.id,
                    status: status,
                    body: replyText
                )
                completed()
            } catch {
                self.error = error.localizedDescription
            }
        }
    }
}

private func collaborationStatusColor(_ status: String) -> Color {
    switch status {
    case "completed": .green
    case "blocked", "declined", "failed", "expired", "cancelled": .red
    case "claimed": .blue
    default: .secondary
    }
}

/// Display wording for a collaboration request or reply `status`.
func collaborationStatusLabel(_ status: String) -> String {
    switch status {
    case "queued": String(localized: "Queued")
    case "claimed": String(localized: "Claimed")
    case "completed": String(localized: "Completed")
    case "blocked": String(localized: "Blocked")
    case "declined": String(localized: "Declined")
    case "failed": String(localized: "Failed")
    case "expired": String(localized: "Expired")
    case "cancelled": String(localized: "Cancelled")
    case "waiting_reply": String(localized: "Waiting Reply")
    default: status.replacingOccurrences(of: "_", with: " ").capitalized
    }
}

/// Display wording for a collaboration request `kind`.
func collaborationKindLabel(_ kind: String) -> String {
    switch kind {
    case "question": String(localized: "Question")
    case "review": String(localized: "Review")
    case "task": String(localized: "Task")
    case "notice": String(localized: "Notice")
    default: kind.capitalized
    }
}

private extension String {
    var nonEmpty: String? { isEmpty ? nil : self }
}
