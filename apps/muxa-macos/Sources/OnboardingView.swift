import AppKit
import SwiftUI

// The launch decision, checklist evaluation, and other pure pieces live in
// OnboardingLogic.swift; this file is the Welcome window itself.

/// Attached to the workbench root in `MuxaApp`: opens the Welcome window on
/// the first launch, or What's New after an upgrade, once the workbench has
/// finished its own make-key-and-order-front retries so it lands on top.
struct OnboardingLaunchPresenter: ViewModifier {
    @Environment(\.openWindow) private var openWindow

    func body(content: Content) -> some View {
        content.task {
            guard let presentation = OnboardingLaunch.consumeLaunchPresentation() else { return }
            do {
                try await Task.sleep(for: .milliseconds(800))
            } catch {
                OnboardingLaunch.presentedThisSession = false
                return
            }
            switch presentation {
            case .welcomeGuide:
                openWindow(id: OnboardingPreferences.windowID)
            case .whatsNew:
                openWindow(id: OnboardingPreferences.whatsNewWindowID)
            }
            NSApp.activate(ignoringOtherApps: true)
        }
    }
}

extension View {
    /// Opens the Welcome guide on first launch, What's New after an upgrade
    /// (see `OnboardingPreferences`). Never fires inside unit tests.
    func presentsOnboardingOnLaunch() -> some View {
        modifier(OnboardingLaunchPresenter())
    }
}

// MARK: - Help › Welcome Guide…

struct OnboardingMenuCommands: Commands {
    var body: some Commands {
        CommandGroup(after: .help) {
            Divider()
            OnboardingWelcomeGuideMenuItem()
        }
    }
}

private struct OnboardingWelcomeGuideMenuItem: View {
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Button("Welcome Guide…") {
            openWindow(id: OnboardingPreferences.windowID)
            NSApp.activate(ignoringOtherApps: true)
        }
        Button("What's New in Muxa") {
            openWindow(id: OnboardingPreferences.whatsNewWindowID)
            NSApp.activate(ignoringOtherApps: true)
        }
    }
}

// MARK: - Steps

enum OnboardingStep: Int, CaseIterable, Identifiable {
    case welcome
    case checklist
    case firstAgent
    case notifications
    case workbench
    case flow
    case done

    var id: Int { rawValue }
    var next: OnboardingStep? { OnboardingStep(rawValue: rawValue + 1) }
    var previous: OnboardingStep? { OnboardingStep(rawValue: rawValue - 1) }
    var isLast: Bool { next == nil }
}

// MARK: - The Welcome window

struct OnboardingView: View {
    @ObservedObject var model: AppModel
    @Environment(\.openWindow) private var openWindow
    @Environment(\.colorScheme) private var colorScheme
    @State private var step: OnboardingStep = .welcome
    @State private var dontShowAgain = true
    @State private var detectedTools: [InstalledTool]?
    @State private var isDetectingTools = false
    @State private var supportsAgentStart: Bool?
    @State private var authorization: MuxaUserNotifications.Authorization?
    @State private var hostWindow: NSWindow?

    var body: some View {
        VStack(spacing: 0) {
            page
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
                .id(step)
                .transition(.opacity)

            MuxaTheme.border(colorScheme).frame(height: 1)
            footer
        }
        .frame(minWidth: 760, minHeight: 600)
        .background(MuxaTheme.editor(colorScheme))
        .background(OnboardingWindowTracker { window in
            // Keep the last known guide window: the tracker also reports nil
            // while the view is being torn down, and a nil here used to make
            // `finish()` close whatever window happened to be key.
            if let window { hostWindow = window }
        })
        .task { await detectTools() }
        .task { await refreshAuthorization() }
        .task(id: model.isConnected) { await refreshAgentStartSupport() }
        // Coming back from a shell where an install ran: check again without
        // making the operator look for the button.
        .onReceive(NotificationCenter.default.publisher(for: NSWindow.didBecomeKeyNotification)) { note in
            guard let window = note.object as? NSWindow, window === hostWindow else { return }
            Task {
                await refreshAuthorization()
                if step == .checklist || step == .firstAgent { await detectTools() }
            }
        }
    }

    @ViewBuilder
    private var page: some View {
        switch step {
        case .welcome:
            OnboardingScrollPage { OnboardingWelcomePage() }
        case .checklist:
            OnboardingChecklistPage(
                model: model,
                detectedTools: detectedTools,
                isDetecting: isDetectingTools,
                recheck: { Task { await detectTools() } },
                openMainWindow: openMainWindow
            )
            .padding(28)
        case .firstAgent:
            OnboardingScrollPage {
                OnboardingFirstAgentPage(
                    model: model,
                    launcher: model.agentLauncher,
                    detectedTools: detectedTools,
                    supportsAgentStart: supportsAgentStart,
                    onStarted: {
                        MuxaWorkbenchPresenter.present()
                        finish()
                    }
                )
            }
        case .notifications:
            OnboardingScrollPage {
                OnboardingNotificationsPage(authorization: authorization) {
                    Task {
                        await MuxaUserNotifications.shared.requestAuthorization()
                        await refreshAuthorization()
                    }
                }
            }
        case .workbench:
            OnboardingScrollPage { OnboardingWorkbenchPage() }
        case .flow:
            OnboardingScrollPage { OnboardingFlowPage(model: model, openMainWindow: openMainWindow) }
        case .done:
            OnboardingDonePage(dontShowAgain: $dontShowAgain)
                .padding(28)
        }
    }

    private var footer: some View {
        HStack(spacing: 8) {
            OnboardingPageIndicator(current: step)
            Spacer()
            if step.previous != nil {
                Button("Back") { move(to: step.previous) }
                    .buttonStyle(.muxaSecondary)
            }
            if step.isLast {
                Button("Get Started") { finish() }
                    .buttonStyle(.muxaPrimary)
                    .keyboardShortcut(.defaultAction)
            } else if step == .firstAgent {
                // The page's own Start button is the primary action here.
                Button("Skip") { move(to: step.next) }
                    .buttonStyle(.muxaSecondary)
            } else {
                Button("Continue") { move(to: step.next) }
                    .buttonStyle(.muxaPrimary)
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 12)
    }

    private func move(to target: OnboardingStep?) {
        guard let target else { return }
        withAnimation(.easeInOut(duration: 0.18)) { step = target }
    }

    private func finish() {
        if dontShowAgain {
            OnboardingPreferences.markCompleted()
        }
        // Only ever close the guide itself. Falling back to `NSApp.keyWindow`
        // could close the workbench, which left the app with no window.
        (hostWindow ?? OnboardingPreferences.existingWindow())?.close()
    }

    private func openMainWindow() {
        openWindow(id: "main")
        NSApp.activate(ignoringOtherApps: true)
    }

    private func detectTools() async {
        guard !isDetectingTools else { return }
        isDetectingTools = true
        let tools = await InstalledTools.detect(OnboardingChecklist.probedPrograms)
        guard !Task.isCancelled else {
            isDetectingTools = false
            return
        }
        detectedTools = tools
        isDetectingTools = false
    }

    private func refreshAuthorization() async {
        authorization = await MuxaUserNotifications.shared.authorization()
    }

    private func refreshAgentStartSupport() async {
        guard model.isConnected else {
            supportsAgentStart = nil
            return
        }
        supportsAgentStart = await model.client.supports(MuxaIPCClient.agentStartCapability)
    }
}

/// A page that scrolls when the window is shorter than its content.
private struct OnboardingScrollPage<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        ScrollView {
            content
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(28)
        }
    }
}

private struct OnboardingPageIndicator: View {
    let current: OnboardingStep

    var body: some View {
        HStack(spacing: 10) {
            HStack(spacing: 6) {
                ForEach(OnboardingStep.allCases) { step in
                    Circle()
                        .fill(step == current ? Color.accentColor : Color.secondary.opacity(0.3))
                        .frame(width: 7, height: 7)
                }
            }
            Text("Step \(current.rawValue + 1) of \(OnboardingStep.allCases.count)")
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
        }
        .accessibilityElement(children: .combine)
    }
}

/// The page title and its one-line explanation.
private struct OnboardingPageHeader<Detail: View>: View {
    let title: LocalizedStringKey
    @ViewBuilder let detail: Detail

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title)
                .font(.title2.weight(.semibold))
            detail
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }
}

/// Grouped content inside a page: the workbench's flat panel with a hairline
/// edge, in place of a floating material slab.
private struct OnboardingPanel: ViewModifier {
    var padding: CGFloat = 14
    @Environment(\.colorScheme) private var colorScheme

    func body(content: Content) -> some View {
        content
            .padding(padding)
            .background(
                RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous)
                    .fill(MuxaTheme.panel(colorScheme))
            )
            .overlay(
                RoundedRectangle(cornerRadius: MuxaTheme.panelRadius, style: .continuous)
                    .strokeBorder(MuxaTheme.border(colorScheme), lineWidth: 1)
            )
    }
}

private extension View {
    func onboardingPanel(padding: CGFloat = 14) -> some View {
        modifier(OnboardingPanel(padding: padding))
    }
}

// MARK: - Page 1: What Muxa is

private struct OnboardingWelcomePage: View {
    private let columns = [
        GridItem(.flexible(), spacing: 12),
        GridItem(.flexible(), spacing: 12),
        GridItem(.flexible(), spacing: 12),
    ]

    var body: some View {
        VStack(alignment: .leading, spacing: 22) {
            HStack(alignment: .center, spacing: 16) {
                Image(nsImage: NSApp.applicationIconImage)
                    .resizable()
                    .frame(width: 64, height: 64)
                VStack(alignment: .leading, spacing: 6) {
                    Text("Welcome to Muxa")
                        .font(.title2.weight(.semibold))
                    Text("Muxa runs your coding agents inside tmux and keeps every pane, every host, and every question they have for you in one window.")
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }

            Text("One workbench for every agent")
                .font(.headline)

            LazyVGrid(columns: columns, spacing: 12) {
                OnboardingSurfaceCard(systemImage: MuxaSidebarMode.work.systemImage) {
                    Text("Work")
                } detail: {
                    Text("Start an outcome and let a pipeline of agents carry it through stage by stage.")
                }
                OnboardingSurfaceCard(systemImage: MuxaSidebarMode.watch.systemImage) {
                    Text("Explore")
                } detail: {
                    Text("Every tmux pane on every host, live, whether or not Muxa started it.")
                }
                OnboardingSurfaceCard(systemImage: MuxaSidebarMode.inbox.systemImage) {
                    Text("Inbox")
                } detail: {
                    Text("Agents waiting on you: a question, a choice, or an error.")
                }
                OnboardingSurfaceCard(systemImage: MuxaSidebarMode.ask.systemImage) {
                    Text("Ask")
                } detail: {
                    Text("A quick question to any provider, without starting an agent.")
                }
                OnboardingSurfaceCard(systemImage: MuxaSidebarMode.shells.systemImage) {
                    Text("Shells")
                } detail: {
                    Text("Native terminals for the moments you want to type alongside your agents.")
                }
                OnboardingSurfaceCard(systemImage: "sparkles.rectangle.stack") {
                    Text("Agents")
                } detail: {
                    Text("Start Claude, Codex, Gemini, or OpenCode in a folder; each opens as a tab.")
                }
            }
        }
    }
}

private struct OnboardingSurfaceCard<Title: View, Detail: View>: View {
    let systemImage: String
    @ViewBuilder let title: Title
    @ViewBuilder let detail: Detail

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Image(systemName: systemImage)
                .font(.title3)
                .foregroundStyle(Color.accentColor)
                .frame(height: 22)
            title
                .font(.headline)
            detail
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
        }
        .frame(maxWidth: .infinity, minHeight: 108, alignment: .topLeading)
        .onboardingPanel(padding: 12)
    }
}

// MARK: - Page 2: Setup checklist

private struct OnboardingChecklistPage: View {
    @ObservedObject var model: AppModel
    let detectedTools: [InstalledTool]?
    let isDetecting: Bool
    let recheck: () -> Void
    let openMainWindow: () -> Void
    @AppStorage(MuxaPreferences.workDirectoryKey) private var workDirectory = ""

    private var remoteHosts: [MuxaFleetHost] {
        model.fleetHosts.filter { !$0.local }
    }

    private var tmuxTool: InstalledTool? {
        detectedTools?.first { $0.name == "tmux" }
    }

    private var agentTools: [InstalledTool] {
        OnboardingChecklist.agentTools(in: detectedTools)
    }

    private var installs: (tmux: OnboardingInstallCommand?, agents: [OnboardingInstallCommand]) {
        OnboardingChecklist.installCommands(for: detectedTools)
    }

    private var workFolderStatus: OnboardingCheckStatus {
        OnboardingChecklist.workFolderStatus(path: workDirectory) {
            FileManager.default.fileExists(atPath: $0)
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack(alignment: .top) {
                OnboardingPageHeader(title: "Set up your workspace") {
                    Text("Muxa checks again whenever you come back to this window. Nothing here is required to look around.")
                }
                Spacer()
                HStack(spacing: 8) {
                    if isDetecting {
                        ProgressView()
                            .controlSize(.small)
                    }
                    Button {
                        recheck()
                    } label: {
                        Label("Check Again", systemImage: "arrow.clockwise")
                    }
                    .buttonStyle(.muxaSecondary)
                    .controlSize(.small)
                    .disabled(isDetecting)
                }
            }

            ScrollView {
                VStack(spacing: 0) {
                    connectionRow
                    OnboardingDivider()
                    tmuxRow
                    OnboardingDivider()
                    agentsRow
                    OnboardingDivider()
                    askRow
                    OnboardingDivider()
                    workFolderRow
                    OnboardingDivider()
                    hostsRow
                }
                .onboardingPanel(padding: 0)
                .padding(.horizontal, 1)
            }
        }
    }

    private var connectionRow: some View {
        OnboardingChecklistRow(status: OnboardingChecklist.connectionStatus(model.connectionState)) {
            Text("muxad connection")
        } detail: {
            switch model.connectionState {
            case .connected:
                Text("Connected to the local muxad daemon.")
            case .connecting:
                Text("Connecting to muxad…")
            case let .failed(message):
                Text("Not connected: \(message)")
            case let .upgradeRequired(message):
                Text("muxad needs an upgrade: \(message)")
            }
        } action: {
            if !model.isConnected {
                OnboardingSettingsButton(tab: .runtime)
            }
        }
    }

    private var tmuxRow: some View {
        OnboardingChecklistRow(status: OnboardingChecklist.toolStatus(named: "tmux", in: detectedTools)) {
            Text("tmux")
        } detail: {
            if let tmuxTool {
                OnboardingToolLine(tool: tmuxTool)
            } else if detectedTools == nil {
                Text("Looking for tmux on your PATH…")
            } else {
                VStack(alignment: .leading, spacing: 6) {
                    Text("tmux was not found. Install it with Homebrew, then come back to this window.")
                    if let install = installs.tmux {
                        OnboardingCommandLine(model: model, command: install.command)
                    }
                }
            }
        } action: {
            EmptyView()
        }
    }

    private var agentsRow: some View {
        OnboardingChecklistRow(status: OnboardingChecklist.agentsStatus(in: detectedTools)) {
            Text("Agent CLIs")
        } detail: {
            if detectedTools == nil {
                Text("Looking for agent CLIs on your PATH…")
            } else if agentTools.isEmpty {
                VStack(alignment: .leading, spacing: 6) {
                    Text("No agent CLI was found. Install at least one, then come back to this window.")
                    ForEach(installs.agents) { install in
                        VStack(alignment: .leading, spacing: 3) {
                            Text(verbatim: install.displayName)
                                .font(.caption.weight(.medium))
                                .foregroundStyle(.primary)
                            OnboardingCommandLine(model: model, command: install.command)
                        }
                    }
                }
            } else {
                VStack(alignment: .leading, spacing: 4) {
                    ForEach(agentTools) { tool in
                        OnboardingAgentToolLine(model: model, tool: tool)
                    }
                }
            }
        } action: {
            EmptyView()
        }
    }

    private var askRow: some View {
        OnboardingChecklistRow(status: OnboardingChecklist.askStatus(model.askEnabled)) {
            Text("Global Ask")
        } detail: {
            switch model.askEnabled {
            case .some(true):
                Text("Ask any configured provider from the Ask view without starting an agent.")
            case .some(false):
                Text("Global Ask is disabled in the muxa configuration. Enable it under Settings › Providers.")
            case .none:
                Text("Waiting for muxad to report the Ask configuration.")
            }
        } action: {
            switch model.askEnabled {
            case .some(true):
                Button("Open Global Ask") {
                    openMainWindow()
                    model.select(.ask)
                }
                .buttonStyle(.muxaSecondary)
            case .some(false):
                OnboardingSettingsButton(tab: .providers)
            case .none:
                EmptyView()
            }
        }
    }

    private var workFolderRow: some View {
        OnboardingChecklistRow(status: workFolderStatus) {
            Text("Work folder")
        } detail: {
            if workDirectory.isEmpty {
                Text("Optional. Choose a default project folder so Start Work and your first agent open in the right place.")
            } else if workFolderStatus == .ready {
                Text(verbatim: workDirectory)
            } else {
                Text("\(workDirectory) is not currently available.")
            }
        } action: {
            OnboardingSettingsButton(tab: .general)
        }
    }

    private var hostsRow: some View {
        OnboardingChecklistRow(status: OnboardingChecklist.fleetHostsStatus(remoteHostCount: remoteHosts.count)) {
            Text("Fleet hosts")
        } detail: {
            if remoteHosts.isEmpty {
                Text("Optional. Register SSH hosts to watch panes and run Work on other machines.")
            } else {
                Text("^[\(remoteHosts.count) SSH host](inflect: true) registered: \(remoteHosts.map(\.alias).joined(separator: ", "))")
            }
        } action: {
            Button("Register SSH Host…") {
                openMainWindow()
                model.presentHostRegistration()
            }
            .buttonStyle(.muxaSecondary)
            .disabled(!model.isConnected)
        }
    }
}

private struct OnboardingDivider: View {
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        MuxaTheme.border(colorScheme)
            .frame(height: 1)
            .padding(.leading, 48)
    }
}

private struct OnboardingChecklistRow<Title: View, Detail: View, Action: View>: View {
    let status: OnboardingCheckStatus
    @ViewBuilder let title: Title
    @ViewBuilder let detail: Detail
    @ViewBuilder let action: Action

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            OnboardingStatusIcon(status: status)
                .font(.title3)
                .frame(width: 22, alignment: .center)
                .padding(.top, 1)
            VStack(alignment: .leading, spacing: 3) {
                title
                    .fontWeight(.medium)
                detail
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 12)
            action
                .controlSize(.small)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }
}

private struct OnboardingStatusIcon: View {
    let status: OnboardingCheckStatus

    var body: some View {
        Image(systemName: status.systemImage)
            .foregroundStyle(tint)
    }

    private var tint: Color {
        switch status {
        case .ready: .green
        case .attention: .orange
        case .unknown: .secondary
        }
    }
}

/// One detected tool: name, version, and the resolved path as a tooltip.
private struct OnboardingToolLine: View {
    let tool: InstalledTool

    var body: some View {
        HStack(spacing: 6) {
            Text(verbatim: tool.name)
                .font(.caption.weight(.medium))
                .foregroundStyle(.primary)
            if let version = tool.version {
                Text(verbatim: version)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                Text("version unknown")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }
        }
        .help(Text(verbatim: tool.path))
        .lineLimit(1)
    }
}

/// An installed agent CLI, plus a sign-in hint where one can be read
/// reliably from disk (see `OnboardingChecklist.signInStatus`).
private struct OnboardingAgentToolLine: View {
    @ObservedObject var model: AppModel
    let tool: InstalledTool

    private var signIn: OnboardingCheckStatus? {
        OnboardingChecklist.signInStatus(
            program: tool.name,
            home: FileManager.default.homeDirectoryForCurrentUser.path,
            environment: ProcessInfo.processInfo.environment,
            fileExists: { FileManager.default.fileExists(atPath: $0) }
        )
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            OnboardingToolLine(tool: tool)
            if signIn == .attention, let login = OnboardingChecklist.signInCommand(program: tool.name) {
                Label("\(login.displayName) has not been signed in on this Mac yet.", systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange)
                OnboardingCommandLine(model: model, command: login.command)
            }
        }
    }
}

/// A shell command with Copy and Run in Shell. Run in Shell opens a new
/// Muxa shell tab with the command typed but not run, so the operator sees
/// exactly what will execute and presses Return themselves.
private struct OnboardingCommandLine: View {
    @ObservedObject var model: AppModel
    let command: String
    @Environment(\.colorScheme) private var colorScheme
    @State private var copied = false

    var body: some View {
        HStack(spacing: 6) {
            Text(verbatim: command)
                .font(.system(size: 11).monospaced())
                .foregroundStyle(.primary)
                .textSelection(.enabled)
                .lineLimit(1)
                .truncationMode(.middle)
                .padding(.horizontal, 7)
                .frame(minHeight: 22)
                .background(
                    RoundedRectangle(cornerRadius: MuxaTheme.controlRadius, style: .continuous)
                        .fill(MuxaTheme.inputBackground(colorScheme))
                )
                .overlay(
                    RoundedRectangle(cornerRadius: MuxaTheme.controlRadius, style: .continuous)
                        .strokeBorder(MuxaTheme.inputBorder(colorScheme), lineWidth: 1)
                )
            Button {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(command, forType: .string)
                copied = true
            } label: {
                if copied {
                    Label("Copied", systemImage: "checkmark")
                } else {
                    Label("Copy", systemImage: "doc.on.doc")
                }
            }
            .buttonStyle(.muxaGhost)
            .controlSize(.small)
            .task(id: copied) {
                guard copied else { return }
                try? await Task.sleep(for: .seconds(1.5))
                copied = false
            }
            Button {
                model.createShell(typing: command)
                MuxaWorkbenchPresenter.present()
            } label: {
                Label("Run in Shell", systemImage: "terminal")
            }
            .buttonStyle(.muxaSecondary)
            .controlSize(.small)
            .disabled(!model.isConnected || model.isCreatingSession)
            .help("Opens a new shell tab with this command typed; press Return there to run it.")
        }
    }
}

/// Opens Settings on the tab that fixes the row: the `openSettings` action on
/// macOS 14, the AppKit selector on 13. A bare `SettingsLink` has no hook to
/// pick the tab first, so it reopened whichever tab was shown last.
private struct OnboardingSettingsButton: View {
    let tab: MuxaSettingsTab
    var title: LocalizedStringKey = "Open Settings…"

    var body: some View {
        if #available(macOS 14.0, *) {
            OnboardingOpenSettingsButton(tab: tab, title: title)
        } else {
            Button(title) {
                MuxaSettingsOpener.select(tab)
                MuxaSettingsOpener.openLegacySettingsWindow()
            }
            .buttonStyle(.muxaSecondary)
        }
    }
}

@available(macOS 14.0, *)
private struct OnboardingOpenSettingsButton: View {
    @Environment(\.openSettings) private var openSettings
    let tab: MuxaSettingsTab
    let title: LocalizedStringKey

    var body: some View {
        Button(title) {
            MuxaSettingsOpener.select(tab)
            openSettings()
            NSApp.activate(ignoringOtherApps: true)
        }
        .buttonStyle(.muxaSecondary)
    }
}

// MARK: - Page 3: Your first agent

private struct OnboardingFirstAgentPage: View {
    @ObservedObject var model: AppModel
    @ObservedObject var launcher: MuxaAgentLauncher
    let detectedTools: [InstalledTool]?
    let supportsAgentStart: Bool?
    let onStarted: () -> Void
    @AppStorage(MuxaPreferences.workDirectoryKey) private var workDirectory = ""
    @State private var folder = ""
    @State private var program: MuxaAgentProgram?
    @State private var seeded = false

    private var installed: [MuxaAgentProgram]? {
        OnboardingFirstAgent.installedPrograms(in: detectedTools)
    }

    private var tmuxInstalled: Bool {
        detectedTools?.contains { $0.name == "tmux" } ?? true
    }

    private var expandedFolder: String {
        (folder.trimmingCharacters(in: .whitespacesAndNewlines) as NSString).expandingTildeInPath
    }

    private var folderExists: Bool {
        var isDirectory: ObjCBool = false
        return expandedFolder.hasPrefix("/")
            && FileManager.default.fileExists(atPath: expandedFolder, isDirectory: &isDirectory)
            && isDirectory.boolValue
    }

    private var blocker: OnboardingFirstAgent.Blocker? {
        OnboardingFirstAgent.blocker(
            isConnected: model.isConnected,
            supportsAgentStart: supportsAgentStart,
            installed: installed,
            folderExists: folderExists
        )
    }

    private var selectedProgram: MuxaAgentProgram? {
        guard let installed else { return nil }
        if let program, installed.contains(program) { return program }
        return OnboardingFirstAgent.defaultProgram(
            preferred: launcher.rememberedDefaults.program(guide: launcher.launchSettings?.legacyGuide),
            installed: installed
        )
    }

    private var placement: MuxaAgentPlacementKind {
        OnboardingFirstAgent.placement(
            remembered: launcher.rememberedDefaults.placement,
            tmuxInstalled: tmuxInstalled
        )
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 20) {
            OnboardingPageHeader(title: "Start your first agent") {
                Text("Pick a folder and an agent. Muxa starts it and opens it as a tab in the workbench. You can skip this and press ⌥⌘T later.")
            }

            VStack(alignment: .leading, spacing: 16) {
                VStack(alignment: .leading, spacing: 6) {
                    Text("Folder")
                        .font(.system(size: 12, weight: .semibold))
                    HStack(spacing: 6) {
                        TextField("Folder", text: $folder)
                            .muxaFieldChrome()
                        Button("Choose…", action: chooseFolder)
                            .buttonStyle(.muxaSecondary)
                    }
                    if !folder.isEmpty, !folderExists {
                        Text("This folder does not exist on this Mac.")
                            .font(.caption)
                            .foregroundStyle(.red)
                    }
                }

                VStack(alignment: .leading, spacing: 6) {
                    Text("Agent")
                        .font(.system(size: 12, weight: .semibold))
                    if let installed, !installed.isEmpty, let selectedProgram {
                        MuxaSegmented(
                            selection: Binding(get: { selectedProgram }, set: { program = $0 }),
                            options: installed,
                            label: { Text(verbatim: $0.displayName) }
                        )
                    } else if installed == nil {
                        HStack(spacing: 6) {
                            ProgressView().controlSize(.small)
                            Text("Looking for agent CLIs…")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                    } else {
                        Text("No agent CLI was found on this Mac.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    placementCaption
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .onboardingPanel(padding: 16)

            HStack(spacing: 10) {
                Button {
                    start()
                } label: {
                    Label("Start Agent", systemImage: "play.fill")
                }
                .buttonStyle(.muxaPrimary)
                .keyboardShortcut(.defaultAction)
                .disabled(blocker != nil || launcher.isStarting || selectedProgram == nil)
                if launcher.isStarting {
                    ProgressView().controlSize(.small)
                    if let status = launcher.status {
                        Text(status)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                } else if let blocker {
                    Label { blockerMessage(blocker) } icon: { Image(systemName: "info.circle") }
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }

            if let error = launcher.error, !launcher.isStarting {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .font(.caption)
                    .foregroundStyle(.red)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .onAppear(perform: seed)
        .task {
            if launcher.launchSettings == nil { await model.loadAgentLaunchSettings() }
        }
    }

    private var placementCaption: Text {
        switch placement {
        case .native where tmuxInstalled:
            Text("It runs in a Muxa terminal on this Mac and opens as a tab.")
        case .native:
            Text("tmux was not found, so it runs in a Muxa terminal on this Mac and opens as a tab.")
        default:
            Text("It runs in a new tmux session named after the folder and opens as a tab.")
        }
    }

    private func blockerMessage(_ blocker: OnboardingFirstAgent.Blocker) -> Text {
        switch blocker {
        case .notConnected: Text("Available once muxad is connected. See the previous step.")
        case .checking: Text("Checking muxad and your agent CLIs…")
        case .noAgentCLI: Text("Install an agent CLI on the previous step first.")
        case .daemonTooOld: Text("The running muxad cannot start agents. Update Muxa and restart muxad.")
        case .missingFolder: Text("Choose a folder that exists on this Mac.")
        }
    }

    private func seed() {
        launcher.error = nil
        guard !seeded else { return }
        seeded = true
        folder = OnboardingFirstAgent.defaultFolder(
            workDirectory: workDirectory,
            recent: launcher.recentDirectories,
            home: FileManager.default.homeDirectoryForCurrentUser.path
        )
    }

    private func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.directoryURL = folderExists
            ? URL(fileURLWithPath: expandedFolder, isDirectory: true)
            : FileManager.default.homeDirectoryForCurrentUser
        if panel.runModal() == .OK, let url = panel.url {
            folder = url.path
        }
    }

    /// The same path ⌥⌘T takes (`defaultAgentStartRequest` + `startAgent`),
    /// with the folder and agent chosen here. `startAgent` opens the new
    /// agent's tab once its pane shows up.
    private func start() {
        guard blocker == nil, let program = selectedProgram else { return }
        let cwd = expandedFolder
        let placement = placement
        Task {
            let request = await model.defaultAgentStartRequest(
                program: program,
                placement: placement,
                focus: .directory(cwd),
                windowSession: nil,
                cwd: cwd
            )
            if await model.startAgent(request) == nil {
                onStarted()
            }
        }
    }
}

// MARK: - Page 4: Notifications

private struct OnboardingNotificationsPage: View {
    let authorization: MuxaUserNotifications.Authorization?
    let requestAuthorization: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 20) {
            OnboardingPageHeader(title: "Know when an agent needs you") {
                Text("Agents run while you do something else. Muxa tells you when one of them needs you.")
            }

            VStack(alignment: .leading, spacing: 12) {
                Text("Muxa notifies you when an agent")
                    .font(.system(size: 12, weight: .semibold))
                Label("is waiting for your input or a choice", systemImage: "questionmark.bubble")
                Label("hit an error or is blocked", systemImage: "exclamationmark.octagon")
                Label("finished a turn", systemImage: "checkmark.circle")
                Text("Never for the pane you are looking at. An agent you have not opened since keeps a dot, and the Dock icon counts them.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .font(.callout)
            .frame(maxWidth: .infinity, alignment: .leading)
            .onboardingPanel(padding: 16)

            authorizationRow

            HStack(spacing: 8) {
                Text("Choose what Muxa notifies about in Settings › Behaviour.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                OnboardingSettingsButton(tab: .behaviour)
                    .controlSize(.small)
            }
        }
    }

    @ViewBuilder
    private var authorizationRow: some View {
        switch authorization {
        case nil:
            ProgressView().controlSize(.small)
        case .notDetermined:
            HStack(spacing: 10) {
                Button {
                    requestAuthorization()
                } label: {
                    Label("Allow Notifications", systemImage: "bell.badge")
                }
                .buttonStyle(.muxaPrimary)
                Text("macOS asks once; you can change it later in System Settings.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        case .allowed:
            Label("Notifications are allowed.", systemImage: "checkmark.circle.fill")
                .foregroundStyle(.green)
        case .denied:
            HStack(spacing: 10) {
                Label("Notifications for Muxa are off in System Settings.", systemImage: "bell.slash")
                    .foregroundStyle(.orange)
                Button("Open System Settings") {
                    if let url = URL(string: "x-apple.systempreferences:com.apple.Notifications-Settings.extension") {
                        NSWorkspace.shared.open(url)
                    }
                }
                .buttonStyle(.muxaSecondary)
            }
        }
    }
}

// MARK: - Page 5: Find your way around

private struct OnboardingWorkbenchPage: View {
    @Environment(\.colorScheme) private var colorScheme

    var body: some View {
        VStack(alignment: .leading, spacing: 20) {
            OnboardingPageHeader(title: "Find your way around") {
                Text("The workbench is laid out like a code editor, so every part has one job.")
            }

            VStack(alignment: .leading, spacing: 14) {
                OnboardingAnatomyRow(systemImage: "rectangle.topthird.inset.filled") {
                    Text("Title row")
                } detail: {
                    Text("Open tabs, then the window's actions at the right end: Go to Anything, Start Work, New Shell, New Agent, and a … menu for the rest.")
                }
                OnboardingAnatomyRow(systemImage: "sidebar.squares.left") {
                    Text("Activity bar")
                } detail: {
                    Text("The strip on the left edge switches between Work, Explore, Inbox, Ask, and Shells.")
                }
                OnboardingAnatomyRow(systemImage: "sidebar.left") {
                    Text("Side bar")
                } detail: {
                    Text("Lists what the chosen view holds, with a filter on top. Hide or show it with ⌘B.")
                }
                OnboardingAnatomyRow(systemImage: "rectangle.bottomthird.inset.filled") {
                    Text("Status bar")
                } detail: {
                    Text("The muxad connection, provider usage, and how many hosts, agents, and shells are running.")
                }
            }
            .onboardingPanel(padding: 16)

            VStack(alignment: .leading, spacing: 4) {
                Text("Keyboard shortcuts")
                    .font(.system(size: 12, weight: .semibold))
                    .padding(.bottom, 4)
                ForEach(OnboardingShortcuts.highlights()) { entry in
                    HStack {
                        Text(verbatim: entry.title)
                            .font(.system(size: 12))
                        Spacer(minLength: 12)
                        Text(verbatim: entry.keys)
                            .font(.system(size: 11, weight: .medium).monospaced())
                            .padding(.horizontal, 6)
                            .frame(minHeight: 20)
                            .background(
                                RoundedRectangle(cornerRadius: 4, style: .continuous)
                                    .fill(MuxaTheme.segmentTrack(colorScheme))
                            )
                    }
                    .frame(minHeight: 24)
                }
                Text("Press ⌘/ in the workbench for every shortcut.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .padding(.top, 4)
            }
            .onboardingPanel(padding: 16)
        }
    }
}

private struct OnboardingAnatomyRow<Title: View, Detail: View>: View {
    let systemImage: String
    @ViewBuilder let title: Title
    @ViewBuilder let detail: Detail

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: systemImage)
                .font(.title3)
                .foregroundStyle(Color.accentColor)
                .frame(width: 24)
            VStack(alignment: .leading, spacing: 2) {
                title
                    .font(.system(size: 12, weight: .semibold))
                detail
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }
}

// MARK: - Page 6: How work flows

private struct OnboardingFlowPage: View {
    @ObservedObject var model: AppModel
    let openMainWindow: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 22) {
            OnboardingPageHeader(title: "How work flows") {
                Text("For bigger outcomes, describe what done looks like. Agents do the work in stages, and Muxa tells you the moment one of them needs you.")
            }

            HStack(alignment: .top, spacing: 6) {
                OnboardingFlowNode(systemImage: "play.square.stack") {
                    Text("Start Work")
                } detail: {
                    Text("Pick a pipeline and say what done looks like.")
                }
                OnboardingFlowArrow()
                OnboardingFlowNode(systemImage: "rectangle.3.group") {
                    Text("Pipeline")
                } detail: {
                    Text("Agents run in stages and hand their results to the next stage.")
                }
                OnboardingFlowArrow()
                OnboardingFlowNode(systemImage: MuxaSidebarMode.inbox.systemImage) {
                    Text("Inbox")
                } detail: {
                    Text("An agent that needs a decision or an answer shows up here.")
                }
                OnboardingFlowArrow()
                OnboardingFlowNode(systemImage: "eye") {
                    Text("Live Watch")
                } detail: {
                    Text("Open the pane to see exactly what the agent is doing.")
                }
            }

            VStack(alignment: .leading, spacing: 10) {
                Label("Start Work is in the title row's actions, or press ⌥⌘N.", systemImage: "play.square.stack")
                Label("Press ⇧⌘P for the command palette: every action, one keystroke away.", systemImage: "command")
                Label("Open Live Watch with ⇧⌘W to see every pane on every host side by side.", systemImage: "rectangle.on.rectangle")
            }
            .font(.callout)
            .foregroundStyle(.secondary)

            HStack {
                Button {
                    openMainWindow()
                    model.presentWorkStart()
                } label: {
                    Label("Start Work…", systemImage: "play.square.stack")
                }
                .buttonStyle(.muxaSecondary)
                .disabled(!model.isConnected || model.isStartingWork)
                if !model.isConnected {
                    Text("Available once muxad is connected.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
        }
    }
}

private struct OnboardingFlowNode<Title: View, Detail: View>: View {
    let systemImage: String
    @ViewBuilder let title: Title
    @ViewBuilder let detail: Detail

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Image(systemName: systemImage)
                .font(.title2)
                .foregroundStyle(Color.accentColor)
                .frame(height: 28)
            title
                .font(.headline)
            detail
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
        }
        .frame(maxWidth: .infinity, minHeight: 132, alignment: .topLeading)
        .onboardingPanel()
    }
}

private struct OnboardingFlowArrow: View {
    var body: some View {
        Image(systemName: "arrow.right")
            .font(.body.weight(.semibold))
            .foregroundStyle(.tertiary)
            .frame(width: 18, height: 132)
            .accessibilityHidden(true)
    }
}

// MARK: - Page 7: Done

private struct OnboardingDonePage: View {
    @Binding var dontShowAgain: Bool

    var body: some View {
        VStack(spacing: 18) {
            Spacer(minLength: 0)
            Image(systemName: "checkmark.seal.fill")
                .font(.system(size: 60))
                .foregroundStyle(Color.accentColor)
            Text("You're ready")
                .font(.title2.weight(.semibold))
            Text("Start an agent with ⌥⌘T or Work with ⌥⌘N, jump to whichever agent needs you with ⌘J, and keep an eye on the Inbox.")
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            Label("Show this guide again from Help › Welcome Guide.", systemImage: "questionmark.circle")
                .font(.caption)
                .foregroundStyle(.secondary)
            Toggle("Don't show this guide again", isOn: $dontShowAgain)
                .toggleStyle(.checkbox)
                .padding(.top, 6)
            Spacer(minLength: 0)
        }
        .frame(maxWidth: 480)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

// MARK: - Window plumbing

/// Reports the hosting NSWindow so the guide can close itself on macOS 13
/// (no `dismissWindow`), and raises the window once it exists so it lands
/// above the workbench, which re-asserts itself right after launch.
struct OnboardingWindowTracker: NSViewRepresentable {
    var identifier = OnboardingPreferences.windowIdentifier
    let onWindowChange: (NSWindow?) -> Void

    func makeNSView(context: Context) -> OnboardingWindowTrackingView {
        let view = OnboardingWindowTrackingView(frame: .zero)
        view.windowIdentifier = identifier
        view.onWindowChange = onWindowChange
        return view
    }

    func updateNSView(_ nsView: OnboardingWindowTrackingView, context: Context) {
        nsView.onWindowChange = onWindowChange
    }
}

final class OnboardingWindowTrackingView: NSView {
    var windowIdentifier = OnboardingPreferences.windowIdentifier
    var onWindowChange: ((NSWindow?) -> Void)?

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        if let window {
            window.identifier = NSUserInterfaceItemIdentifier(windowIdentifier)
            window.isRestorable = false
            window.makeKeyAndOrderFront(nil)
        }
        onWindowChange?(window)
    }
}
