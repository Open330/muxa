// Global Ask: one-turn questions to a provider and their conversations.
import AppKit
import SwiftUI

struct TendAskView: View {
    @ObservedObject var model: AppModel
    @ObservedObject private var providers = AskProviderStore.shared
    @State private var prompt = ""
    @State private var agent = "claude"
    /// "Copied" or "Saved …", shown in the header for a moment after an export.
    @State private var exportNotice: String?
    @State private var exportFailed = false

    private var activeConversation: TendAskConversation? {
        model.askConversations.first { $0.id == model.activeAskConversationID }
    }

    private var providerConversations: [TendAskConversation] {
        model.askConversations.filter { $0.agent == agent }
    }

    private var conversationEntries: [TendAskEntry] {
        let entries: [TendAskEntry]
        if let conversationID = model.activeAskConversationID {
            entries = model.askEntries.filter { $0.conversationID == conversationID }
        } else {
            // Compatibility with a daemon from before tend-owned conversation
            // ids: retain the prior provider-filtered history until it reloads.
            entries = model.askEntries.filter { $0.agent == agent }
        }
        return entries.sorted { $0.askedAt < $1.askedAt }
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Label("Global Ask", systemImage: "sparkles")
                    .font(.headline)
                if let activeConversation {
                    Image(systemName: "chevron.right")
                        .font(TendType.meta)
                        .foregroundStyle(.tertiary)
                    Text(activeConversation.title)
                        .font(.subheadline.weight(.medium))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                Spacer()
                if let exportNotice {
                    Label {
                        Text(verbatim: exportNotice)
                    } icon: {
                        Image(systemName: exportFailed ? "exclamationmark.triangle.fill" : "checkmark.circle.fill")
                    }
                    .font(TendType.detail)
                    .foregroundStyle(exportFailed ? Color.orange : Color.secondary)
                    .lineLimit(1)
                } else {
                    Text("Conversations resume where the provider left off")
                        .font(TendType.detail)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                conversationExportMenu
            }
            .padding(.horizontal, 14)
            .frame(height: 40)

            Divider()

            if model.askUsesSnapshots {
                Text("Shared Ask history refreshes periodically from the coordinator.")
                    .font(TendType.detail).foregroundStyle(.secondary).padding(.horizontal, 14)
            }
            if model.askEnabled == false {
                HStack(alignment: .center, spacing: 12) {
                    Image(systemName: "sparkles.rectangle.stack")
                        .font(.title3)
                        .foregroundStyle(.tint)
                    VStack(alignment: .leading, spacing: 2) {
                        Group {
                            if model.askConfigurationPendingReload {
                                Text("Reload to finish enabling Ask")
                            } else {
                                Text("Enable Global Ask")
                            }
                        }
                        .font(.subheadline.weight(.semibold))
                        Group {
                            if model.askConfigurationPendingReload {
                                Text("The grant is saved. Reload tendd to apply it; tmux sessions will remain running.")
                            } else {
                                Text("Tend will run the selected provider CLI headlessly. Provider usage may be billed to your account.")
                            }
                        }
                        .font(TendType.detail)
                        .foregroundStyle(.secondary)
                    }
                    Spacer(minLength: 12)
                    if model.isEnablingAsk {
                        ProgressView()
                            .controlSize(.small)
                    }
                    Button {
                        Task { await model.enableAsk() }
                    } label: {
                        if model.askConfigurationPendingReload {
                            Text("Reload tendd")
                        } else {
                            Text("Enable & Reload")
                        }
                    }
                    .buttonStyle(.tendPrimary)
                    .disabled(model.isEnablingAsk)
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 10)
                .background(Color.accentColor.opacity(0.07))

                Divider()
            }

            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 0) {
                        if conversationEntries.isEmpty {
                            ConsoleUnavailableView(
                                title: activeConversation == nil ? "Start a conversation" : "No messages yet",
                                systemImage: "bubble.left.and.bubble.right",
                                description: "Ask a headless agent a question. You can return to this conversation and continue it later."
                            )
                            .frame(minHeight: 150)
                        } else {
                            ForEach(Array(conversationEntries.enumerated()), id: \.element.id) { index, entry in
                                AskConversationTurn(entry: entry)
                                    .id(entry.id)
                                if index < conversationEntries.count - 1 {
                                    Divider().padding(.vertical, 6)
                                }
                            }
                        }
                    }
                    .padding(14)
                    .frame(maxWidth: 980)
                    .frame(maxWidth: .infinity)
                }
                .onAppear { scrollToLatest(proxy) }
                .onChange(of: conversationEntries.last?.id) { _ in scrollToLatest(proxy) }
            }

            Divider()

            VStack(alignment: .leading, spacing: 7) {
                AskComposerEditor(
                    text: $prompt,
                    placeholder: "Ask about work across your hosts…"
                )
                .frame(minHeight: 72, maxHeight: 112)
                .background(TendTheme.panelFill, in: RoundedRectangle(cornerRadius: TendTheme.panelRadius, style: .continuous))
                .overlay {
                    RoundedRectangle(cornerRadius: TendTheme.panelRadius, style: .continuous)
                        .strokeBorder(Color(nsColor: .separatorColor), lineWidth: 1)
                }
                .disabled(model.askEnabled == false || !model.isConnected)
                ViewThatFits(in: .horizontal) {
                    HStack(spacing: 8) {
                        askContextControls
                        Spacer(minLength: 8)
                        askSendStatus
                        sendButton
                    }
                    VStack(alignment: .leading, spacing: 8) {
                        HStack(spacing: 8) {
                            askContextControls
                            Spacer()
                        }
                        HStack {
                            askSendStatus
                            Spacer()
                            sendButton
                        }
                    }
                }
            }
            .padding(12)
        }
        .onAppear { agent = model.askAgent }
        .onChange(of: model.askAgent) { selected in agent = selected }
        .onChange(of: agent) { selected in
            Task { await model.selectAskAgent(selected) }
        }
        .task(id: model.isConnected) {
            await providers.reload(model: model)
        }
    }

    private var askContextControls: some View {
        HStack(spacing: 7) {
            conversationMenu
            // A borderless menu instead of a bezeled pop-up button, so the
            // composer's controls read as one quiet row.
            Menu {
                Picker("Provider", selection: $agent) {
                    ForEach(providers.providers) { provider in
                        Text(provider.title)
                            .tag(provider.id)
                            .disabled(!providers.isUsable(provider))
                    }
                    if !providers.providers.contains(where: { $0.id == agent }) {
                        Text(providers.title(for: agent)).tag(agent)
                    }
                }
                .pickerStyle(.inline)
            } label: {
                Label(providers.title(for: agent), systemImage: "cpu")
            }
            .menuStyle(.borderlessButton)
            .fixedSize()
            .help("Provider for new conversations; disabled entries need a CLI install or an API key in Settings")

        }
    }

    /// Conversations first: starting a new one is the most common action in
    /// this bar, so it leads. Provider setup lives in Settings › Providers.
    private var conversationMenu: some View {
        Menu {
            Button {
                Task { await model.resetAskConversation() }
            } label: {
                Label("New Conversation", systemImage: "plus.bubble")
            }
            Divider()
            if providerConversations.isEmpty {
                Text("No previous conversations")
            } else {
                ForEach(providerConversations) { conversation in
                    Button {
                        Task { await model.selectAskConversation(conversation.id) }
                    } label: {
                        if conversation.id == model.activeAskConversationID {
                            Label(conversation.title, systemImage: "checkmark")
                        } else {
                            Text(conversation.title)
                        }
                    }
                }
            }
        } label: {
            Label {
                if let activeConversation {
                    Text(activeConversation.title)
                } else {
                    Text("Conversations")
                }
            } icon: {
                Image(systemName: "bubble.left.and.bubble.right")
            }
            .lineLimit(1)
            .frame(width: 190, alignment: .leading)
        }
        .menuStyle(.borderlessButton)
    }

    /// The whole conversation, out of the app: as Markdown for notes, as a
    /// prompt for handing the thread to another agent, or as a file.
    private var conversationExportMenu: some View {
        Menu {
            Button {
                copyConversation { AskExport.markdown($0, title: activeConversation?.title, providerTitle: $1) }
            } label: {
                Label("Copy Conversation as Markdown", systemImage: "doc.on.doc")
            }
            Button {
                copyConversation { AskExport.prompt($0, providerTitle: $1) }
            } label: {
                Label("Copy Conversation as Prompt", systemImage: "arrow.turn.down.right")
            }
            Divider()
            Button(action: saveConversation) {
                Label("Save as Markdown…", systemImage: "square.and.arrow.down")
            }
        } label: {
            Label("Export", systemImage: "square.and.arrow.up")
        }
        .menuStyle(.borderlessButton)
        .fixedSize()
        .disabled(conversationEntries.isEmpty)
        .help("Copy or save this conversation")
    }

    private func copyConversation(_ format: ([TendAskEntry], (String) -> String) -> String) {
        let entries = conversationEntries
        let titles = AskProviderStore.shared.titles(for: entries)
        AskExport.copy(format(entries) { titles[$0] ?? $0 })
        flash(String(localized: "Copied"))
    }

    private func saveConversation() {
        let entries = conversationEntries
        let titles = AskProviderStore.shared.titles(for: entries)
        let markdown = AskExport.markdown(entries, title: activeConversation?.title) { titles[$0] ?? $0 }
        do {
            let name = AskExport.fileName(title: activeConversation?.title)
            if let url = try AskExport.save(markdown, suggestedName: name) {
                flash(String(localized: "Saved \(url.lastPathComponent)"))
            }
        } catch {
            flash(error.localizedDescription, failed: true)
        }
    }

    private func flash(_ notice: String, failed: Bool = false) {
        exportNotice = notice
        exportFailed = failed
        Task {
            // A failure stays long enough to be read.
            try? await Task.sleep(for: .seconds(failed ? 6 : 2))
            if exportNotice == notice { exportNotice = nil }
        }
    }

    @ViewBuilder
    private var askSendStatus: some View {
        if let error = model.askError {
            Text(error)
                .font(TendType.detail)
                .foregroundStyle(.red)
                .lineLimit(2)
        } else {
            Text("⌘↩ Send")
                .font(TendType.detail)
                .foregroundStyle(.secondary)
        }
        if model.isSendingAsk { ProgressView().controlSize(.small) }
    }

    private var sendButton: some View {
        Button(action: send) {
            Label("Send", systemImage: "paperplane.fill")
        }
        .buttonStyle(.tendPrimary)
        .keyboardShortcut(.return, modifiers: [.command])
        .help("Continue this conversation with the selected provider (⌘↩)")
        .disabled(
            model.isSendingAsk
                || model.askEnabled == false
                || !model.isConnected
                || prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        )
    }

    private func scrollToLatest(_ proxy: ScrollViewProxy) {
        guard let id = conversationEntries.last?.id else { return }
        DispatchQueue.main.async {
            withAnimation(.easeOut(duration: 0.16)) {
                proxy.scrollTo(id, anchor: .bottom)
            }
        }
    }

    private func send() {
        let submitted = prompt
        Task {
            if await model.sendAsk(prompt: submitted, agent: agent) {
                prompt = ""
            }
        }
    }
}

private struct AskComposerEditor: View {
    @Binding var text: String
    let placeholder: LocalizedStringKey

    var body: some View {
        ZStack(alignment: .topLeading) {
            TextEditor(text: $text)
                .font(.body)
                .scrollContentBackground(.hidden)

            if text.isEmpty {
                Text(placeholder)
                    .font(.body)
                    .foregroundStyle(.secondary)
                    // NSTextView uses these insets for its first baseline. Keeping
                    // the overlay inside the same padded container also gives it
                    // the exact same wrapping width as the editable text.
                    .padding(.leading, 5)
                    .padding(.trailing, 5)
                    .padding(.top, 8)
                    .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
                    .allowsHitTesting(false)
            }
        }
        .padding(7)
    }
}

private struct AskConversationTurn: View {
    let entry: TendAskEntry

    private var providerTitle: String {
        AskProviderStore.shared.title(for: entry.agent)
    }

    private var providerIcon: String {
        AskProviderStore.shared.symbolName(for: entry.agent)
    }

    private var statusColor: Color {
        switch entry.status {
        case "failed": .red
        case "running": .blue
        default: .green
        }
    }

    private var askedDate: Date? {
        (try? Date(entry.askedAt, strategy: .iso8601))
            ?? ISO8601DateFormatter().date(from: entry.askedAt)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 7) {
                Text(askedDate?.formatted(date: .abbreviated, time: .shortened) ?? compactInboxTimestamp(entry.askedAt))
                    .font(TendType.detail.monospacedDigit())
                    .foregroundStyle(.secondary)
                Spacer()
                Circle().fill(statusColor).frame(width: 6, height: 6)
                Text(askStatusLabel(entry.status))
                    .font(TendType.detail.weight(.medium))
                    .foregroundStyle(statusColor)
                if entry.status == "running" { ProgressView().controlSize(.mini) }
                AskTurnExportMenu(entry: entry)
            }

            AskMessageBlock(
                role: String(localized: "You"),
                icon: "person.fill",
                source: entry.prompt,
                tint: .accentColor,
                compact: true
            )

            if entry.status == "running", entry.answer.isEmpty {
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Waiting for \(providerTitle)…")
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                }
                .padding(.horizontal, 4)
            }

            if !entry.answer.isEmpty {
                AskMessageBlock(
                    role: providerTitle,
                    icon: providerIcon,
                    source: entry.answer,
                    tint: .primary,
                    compact: false
                )
            }

            if let error = entry.error, !error.isEmpty {
                Label {
                    Text(error).textSelection(.enabled)
                } icon: {
                    Image(systemName: "exclamationmark.triangle.fill")
                }
                .font(TendType.detail)
                .foregroundStyle(.red)
                .padding(10)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Color.red.opacity(0.07), in: RoundedRectangle(cornerRadius: 7))
            }
        }
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .contentShape(Rectangle())
        .contextMenu { AskTurnExportItems(entry: entry) }
    }
}

/// The ways one exchange leaves the app. Shared by the turn's menu button and
/// its context menu, so the two never drift apart.
private struct AskTurnExportItems: View {
    let entry: TendAskEntry

    var body: some View {
        Button {
            AskExport.copy(entry.answer)
        } label: {
            Label("Copy Answer", systemImage: "doc.on.doc")
        }
        .disabled(entry.answer.isEmpty)
        Button {
            AskExport.copy(entry.prompt)
        } label: {
            Label("Copy Question", systemImage: "person")
        }
        Divider()
        Button {
            let titles = AskProviderStore.shared.titles(for: [entry])
            AskExport.copy(AskExport.markdown([entry]) { titles[$0] ?? $0 })
        } label: {
            Label("Copy as Markdown", systemImage: "doc.plaintext")
        }
        Button {
            let titles = AskProviderStore.shared.titles(for: [entry])
            AskExport.copy(AskExport.prompt([entry]) { titles[$0] ?? $0 })
        } label: {
            Label("Copy as Prompt", systemImage: "arrow.turn.down.right")
        }
    }
}

private struct AskTurnExportMenu: View {
    let entry: TendAskEntry

    var body: some View {
        Menu {
            AskTurnExportItems(entry: entry)
        } label: {
            Image(systemName: "square.and.arrow.up")
                .font(TendType.detail)
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(.hidden)
        .fixedSize()
        .help("Copy this exchange")
    }
}

/// Display wording for a Global Ask entry `status` as tendd reports it.
private func askStatusLabel(_ status: String) -> String {
    switch status {
    case "running": String(localized: "Thinking")
    case "failed": String(localized: "Failed")
    default: String(localized: "Answered")
    }
}

private struct AskMessageBlock: View {
    let role: String
    let icon: String
    let source: String
    let tint: Color
    let compact: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
                Label(role, systemImage: icon)
                .font(TendType.detail.weight(.semibold))
                .foregroundStyle(compact ? tint : Color.secondary)

            ReadableMarkdownContent(source: source)
        }
        .padding(12)
        .frame(maxWidth: compact ? 780 : .infinity, alignment: .leading)
        .background(tint.opacity(compact ? 0.075 : 0.035), in: RoundedRectangle(cornerRadius: 8))
        .overlay(alignment: .leading) {
            RoundedRectangle(cornerRadius: 2)
                .fill(tint.opacity(compact ? 0.7 : 0.28))
                .frame(width: 3)
                .padding(.vertical, 8)
        }
        .frame(maxWidth: .infinity, alignment: compact ? .trailing : .leading)
    }
}

private extension String {
    var nonEmpty: String? { isEmpty ? nil : self }
}
