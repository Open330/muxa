import AppKit
import SwiftUI

/// The list remains available through the mark, overflow and keyboard, so a
/// partially covered chip is never the only route to an agent.
struct TendMenuBarContent: View {
    @ObservedObject var model: AppModel
    @ObservedObject var state: TendMenuBarState
    @ObservedObject var attention: TendAgentAttentionCenter
    let dismiss: () -> Void
    let select: (String?) -> Void
    let selectScope: (TendMenuBarScope) -> Void
    @AppStorage(TendPreferences.appearanceKey) private var appearance = TendAppearance.system.rawValue
    @AppStorage(TendPreferences.menuBarGroupingKey) private var groupingValue = TendMenuBarGrouping.agent.rawValue
    @ObservedObject private var registry = TendModuleRegistry.shared

    var body: some View {
        if state.showingUsage, registry.isEnabled("aas"), let aas = registry.module(AasModule.self) {
            VStack(alignment: .leading, spacing: 0) {
                Button("Back to activity") { state.showingUsage = false; select(nil) }.buttonStyle(.plain).padding([.top, .horizontal], 16)
                AasMenuBarPanel(store: aas.store)
            }
            .preferredColorScheme(TendAppearance(rawValue: appearance)?.colorScheme)
        } else {
            VStack(alignment: .leading, spacing: 12) {
                HStack {
                    Button { state.scope = nil; select(nil) } label: { Text("Tend").font(.headline) }
                        .buttonStyle(.plain)
                    Spacer()
                    if state.selectedID != nil {
                        Button("All agents") { select(nil) }
                            .buttonStyle(.plain).foregroundStyle(.secondary)
                    }
                }
                if let scope = state.scope {
                    HStack {
                        Text(verbatim: scope.title).font(.subheadline.weight(.semibold))
                        Spacer()
                        Button("All groups") { state.scope = nil; select(nil) }.buttonStyle(.plain).foregroundStyle(.secondary)
                    }
                }
                if let id = state.selectedID {
                    if let item = state.displayedAgents.first(where: { $0.id == id }) {
                        TendMenuBarAgentCard(model: model, attention: attention, item: item, draft: state.draft(for: item), open: { open(item) })
                            .id(item.id)
                    } else {
                        Label("This agent no longer needs attention", systemImage: "checkmark.circle")
                            .foregroundStyle(.secondary)
                    }
                } else {
                    if state.visibleAgents.isEmpty {
                        Label("No agents need attention", systemImage: "checkmark.circle")
                            .foregroundStyle(.secondary)
                    } else {
                        ScrollView {
                            VStack(spacing: 4) {
                                if state.scope == nil, let grouping = TendMenuBarGrouping(rawValue: groupingValue), grouping != .agent {
                                    ForEach(TendMenuBarChip.activity(state.visibleAgents, grouping: grouping)) { chip in
                                        Button {
                                            if case .group(let scope) = chip.destination { selectScope(scope) }
                                        } label: {
                                            HStack {
                                                TendGlyphView(value: chip.appearance.symbol, size: 12)
                                                    .frame(width: 28, height: 28).background(chip.appearance.gradient, in: Circle())
                                                VStack(alignment: .leading) {
                                                    Text(verbatim: chip.name).font(.subheadline.weight(.semibold))
                                                    Text(verbatim: chip.location).font(TendType.meta).foregroundStyle(.secondary)
                                                }
                                                Spacer()
                                                Text(verbatim: chip.menuStatusLabel).font(TendType.meta).foregroundStyle(chip.menuStatusColor)
                                            }.padding(6).contentShape(Rectangle())
                                        }.buttonStyle(.plain)
                                    }
                                } else {
                                    ForEach(state.visibleAgents) { item in
                                        TendMenuBarAgentRow(item: item, replyFailure: state.replyFailure(for: item.id)) { select(item.id) }
                                    }
                                }
                            }
                        }
                        .frame(maxHeight: 280)
                    }
                    Text("\(model.executionSnapshot.hostedAgents.count) agents · \(model.pipelineRuns.count) work items")
                        .font(TendType.detail).foregroundStyle(.secondary)
                }
                if let line = attention.summary.needsYouLine {
                    Button {
                        model.select(.inbox)
                        present()
                    } label: {
                        Label(line, systemImage: "exclamationmark.circle.fill").foregroundStyle(.orange)
                    }
                    .buttonStyle(.plain).help("Open the Inbox")
                }
                if attention.summary.uncoveredLimited > 0,
                   TendSwitchLimitedButton.hostCount(for: model.hostedAgents.filter { $0.agent.status.isLimited }) > 0 {
                    Button {
                        TendSwitchLimitedButton.presentSwitch(
                            for: model.hostedAgents.filter { $0.agent.status.isLimited }, model: model)
                        present()
                    } label: {
                        Label(attention.summary.uncoveredLimited == 1
                            ? "1 agent hit its usage limit: Switch Account…"
                            : "\(attention.summary.uncoveredLimited) agents hit their usage limit: Switch…",
                            systemImage: "arrow.triangle.2.circlepath")
                    }
                }
                if registry.isEnabled("aas") {
                    Button { state.showingUsage = true } label: { Label("Accounts and usage", systemImage: "gauge.medium") }
                        .buttonStyle(.plain).foregroundStyle(Color.accentColor)
                }
                Divider()
                HStack {
                    Button("Open Tend") { present() }
                    Spacer()
                    TendSettingsShortcut(tab: .menuBar, title: "Menu Bar Settings…")
                        .labelStyle(.iconOnly).buttonStyle(.plain)
                    Menu {
                        Button("Start Work…") { model.presentWorkStart(); present() }
                            .disabled(!model.isConnected || model.isStartingWork)
                        Button("Open Live Watch") { model.select(.watch); present() }
                        Button("New Shell") { model.createShell(); dismiss() }
                            .disabled(!model.isConnected || model.isCreatingSession)
                        if #available(macOS 14.0, *) {
                            SettingsLink { Label("Settings…", systemImage: "gearshape") }
                        } else {
                            Button("Settings…") {
                                let opened = NSApp.sendAction(Selector(("showSettingsWindow:")), to: nil, from: nil)
                                if !opened { NSApp.sendAction(Selector(("showPreferencesWindow:")), to: nil, from: nil) }
                            }
                        }
                        Divider()
                        Button("Quit") { NSApp.terminate(nil) }
                    } label: { Image(systemName: "ellipsis.circle") }
                    .menuStyle(.borderlessButton).fixedSize()
                    .accessibilityLabel("Tend actions")
                }
            }
            .padding(16)
            .frame(width: 360)
            .preferredColorScheme(TendAppearance(rawValue: appearance)?.colorScheme)
        }
    }

    private func present() {
        dismiss()
        TendWorkbenchPresenter.present()
    }

    private func open(_ item: TendMenuBarAgent) {
        // Revalidate session identity at action time, including pane reuse.
        guard let current = model.executionSnapshot.hostedAgents.first(where: { $0.id == item.id }) else { return }
        if let pane = TendAgentAttentionCenter.paneIdentity(for: current) {
            model.attention.pendingOpen = pane
        } else {
            model.select(.agent(current.id))
        }
        present()
    }
}

private struct TendMenuBarAgentCard: View {
    @ObservedObject var model: AppModel
    @ObservedObject var attention: TendAgentAttentionCenter
    let item: TendMenuBarAgent
    @ObservedObject var draft: TendMenuBarReplyDraft
    let open: () -> Void

    private var target: TendWatchPane? {
        guard let pane = item.pane,
              let target = model.executionSnapshot.watchPane(id: pane),
              target.agent?.agentSessionID == item.hosted.agent.agentSessionID else { return nil }
        return target
    }
    private var replyAllowed: Bool {
        target?.agent?.state == "waiting_input" && attention.sendPrompt != nil
            && TendNotificationActions.replyBlocker(target) == nil
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 10) {
                TendMenuBarAvatar(item: item, size: 34)
                VStack(alignment: .leading, spacing: 3) {
                    Text(verbatim: item.name).font(.headline).textSelection(.enabled)
                    Text(verbatim: item.appearance.provider).font(TendType.meta).foregroundStyle(.secondary)
                }
            }
            HStack {
                Label(item.menuStatusLabel, systemImage: item.menuStatusSymbol)
                    .font(TendType.detail.weight(.medium)).foregroundStyle(item.menuStatusColor)
                Spacer()
                if item.unread { Text("Unread").font(TendType.meta).foregroundStyle(Color.accentColor) }
            }
            Text(verbatim: item.location).font(TendType.meta).foregroundStyle(.secondary)
            if target == nil, draft.needsRecovery {
                Label("This pane is no longer available", systemImage: "terminal")
                    .font(TendType.detail).foregroundStyle(.secondary)
            }
            if !item.connected {
                Label("Host disconnected", systemImage: "wifi.slash").foregroundStyle(.secondary)
            }
            ScrollView {
                MarkdownContent(source: item.summary ?? String(localized: "No summary reported"), selectable: true, font: TendType.body)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .frame(maxHeight: 230)
            if replyAllowed || draft.needsRecovery {
                TextField("Reply to agent…", text: $draft.text, axis: .vertical)
                    .lineLimit(1...5).textFieldStyle(.roundedBorder)
                    .disabled(draft.sending)
                HStack {
                    if draft.needsRecovery {
                        Button("Discard Draft", role: .destructive) { draft.discard() }
                            .disabled(draft.sending)
                    }
                    if draft.sending { ProgressView().controlSize(.small) }
                    Spacer()
                    Button("Send Reply") { send() }
                        .disabled(!replyAllowed || draft.sending || draft.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                }
            }
            if let failure = draft.failure { Text(verbatim: failure).font(TendType.detail).foregroundStyle(.red) }
            HStack {
                Button("Open Agent", action: open)
                    .disabled(!model.executionSnapshot.hostedAgents.contains { $0.id == item.id })
                Spacer()
                if item.unread, let pane = item.pane {
                    Button("Mark as Read") { attention.markRead(pane, agentSessionID: item.hosted.agent.agentSessionID) }
                }
            }
        }
    }

    private func send() {
        guard replyAllowed, let target, let sendPrompt = attention.sendPrompt else { return }
        let expectedSession = item.hosted.agent.agentSessionID
        Task { [draft] in
            await draft.send { text in
                // Recheck after the Task begins; pane ids can be reused.
                guard let current = model.executionSnapshot.watchPane(id: target.id),
                      current.agent?.agentSessionID == expectedSession,
                      current.agent?.state == "waiting_input" else {
                    throw TendIPCError.server(String(localized: "This pane is no longer available"))
                }
                if let blocker = TendNotificationActions.replyBlocker(current) { throw TendIPCError.server(blocker) }
                try await sendPrompt(current.host, current.pane, text)
                attention.markRead(current.id, agentSessionID: expectedSession)
            } onFailure: { text, reason in
                // The OS notification remains visible if the card was closed;
                // reopening the retained draft also shows its inline error.
                attention.postReplyFailure(text, to: target.id, reason: reason)
            }
        }
    }

}

private struct TendMenuBarAgentRow: View {
    let item: TendMenuBarAgent
    var replyFailure: String?
    let select: () -> Void
    @State private var hovered = false

    var body: some View {
        Button(action: select) {
            HStack(spacing: 10) {
                TendMenuBarAvatar(item: item)
                VStack(alignment: .leading, spacing: 3) {
                    Text(verbatim: item.name).font(TendType.body.weight(.medium)).lineLimit(1)
                    Text(verbatim: item.location).font(TendType.meta).foregroundStyle(.secondary)
                        .lineLimit(1).truncationMode(.middle)
                    if replyFailure != nil {
                        Label("Your reply was not sent", systemImage: "exclamationmark.circle.fill")
                            .font(TendType.meta).foregroundStyle(.red)
                    } else {
                        Label(item.menuStatusLabel, systemImage: item.menuStatusSymbol)
                            .font(TendType.meta).foregroundStyle(item.menuStatusColor)
                    }
                }
                Spacer(minLength: 4)
                if item.unread {
                    Circle().fill(Color.accentColor).frame(width: 6, height: 6).accessibilityLabel("Unread")
                }
                Image(systemName: "chevron.right").font(TendType.meta).foregroundStyle(.tertiary)
            }
            .padding(10)
            .contentShape(RoundedRectangle(cornerRadius: 8))
        }
        .buttonStyle(.plain)
        .background(Color.primary.opacity(hovered ? 0.06 : 0), in: RoundedRectangle(cornerRadius: 8))
        .onHover { hovered = $0 }
        .accessibilityElement(children: .combine)
    }
}
