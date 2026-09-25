import SwiftUI

/// A side bar list with nothing in it: what would appear there, and the
/// action that makes something appear. Same shape as the Shells view's
/// empty row, for Explore and Inbox.
struct SidebarGuidedEmptyRow<Actions: View>: View {
    let title: LocalizedStringKey
    let systemImage: String
    let detail: LocalizedStringKey
    @ViewBuilder let actions: Actions

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label(title, systemImage: systemImage)
                .foregroundStyle(.secondary)
            Text(detail)
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            HStack(spacing: 6) {
                actions
            }
            .controlSize(.small)
        }
        .padding(.vertical, 4)
        .listRowBackground(Color.clear)
    }
}

/// Explore with no panes anywhere: start an agent or a shell, and, when only
/// this Mac is registered, add a fleet host.
struct ExploreEmptyState: View {
    @ObservedObject var model: AppModel

    private var hasRemoteHosts: Bool {
        model.fleetHosts.contains { !$0.local }
    }

    var body: some View {
        SidebarGuidedEmptyRow(
            title: "No panes detected",
            systemImage: "terminal",
            detail: "Every tmux pane on this Mac and your fleet hosts shows up here, whoever started it."
        ) {
            Button {
                model.presentNewAgent()
            } label: {
                Label("Start Agent", systemImage: "sparkles.rectangle.stack")
            }
            .buttonStyle(.muxaPrimary)
            .disabled(!model.isConnected || model.isStartingAgent)
            Button {
                model.createShell()
            } label: {
                Label("New Shell", systemImage: "plus")
            }
            .buttonStyle(.muxaSecondary)
            .disabled(!model.isConnected || model.isCreatingSession)
        }
        if !hasRemoteHosts {
            SidebarGuidedEmptyRow(
                title: "No fleet hosts",
                systemImage: "server.rack",
                detail: "Register an SSH host to watch its panes and run Work there."
            ) {
                Button("Register SSH Host…") { model.presentHostRegistration() }
                    .buttonStyle(.muxaSecondary)
                    .disabled(!model.isConnected)
            }
        }
    }
}

/// Inbox with nothing waiting: what lands here, and, before any agent runs,
/// how to get one going.
struct InboxEmptyState: View {
    @ObservedObject var model: AppModel
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        SidebarGuidedEmptyRow(
            title: "Nothing needs attention",
            systemImage: "checkmark.circle",
            detail: "Agents waiting for your input or a choice, blocked, or failing show up here."
        ) {
            if model.hostedAgents.isEmpty {
                Button {
                    model.presentNewAgent()
                } label: {
                    Label("Start Agent", systemImage: "sparkles.rectangle.stack")
                }
                .buttonStyle(.muxaPrimary)
                .disabled(!model.isConnected || model.isStartingAgent)
                Button("Welcome Guide") {
                    openWindow(id: OnboardingPreferences.windowID)
                }
                .buttonStyle(.muxaGhost)
            }
        }
    }
}
