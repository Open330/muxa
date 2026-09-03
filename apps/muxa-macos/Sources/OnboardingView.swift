import AppKit
import SwiftUI

// MARK: - Preferences and the launch decision

/// Keys and the pure "should the Welcome guide open?" decision. Kept apart
/// from `MuxaPreferences` so the onboarding peer owns its own defaults.
enum OnboardingPreferences {
    /// `CFBundleShortVersionString` of the build whose guide the user
    /// dismissed with "Don't show again"; unset until then.
    static let completedVersionKey = "muxa.onboarding.completedVersion"
    /// Scene id of the Welcome window (`openWindow(id:)`).
    static let windowID = "onboarding"
    /// Identifier stamped on the guide window so it can be found again.
    static let windowIdentifier = "muxa.onboarding"

    /// The running app's marketing version; "0" when the bundle has none
    /// (unit-test hosts), which keeps the comparison well defined.
    static var currentVersion: String {
        let version = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
        return version.flatMap { $0.isEmpty ? nil : $0 } ?? "0"
    }

    /// True when no version was recorded yet or the recorded one is older
    /// than the current build. Equal or newer recorded versions stay quiet,
    /// so a downgrade does not nag either.
    static func shouldPresent(currentVersion: String, completedVersion: String?) -> Bool {
        guard let completedVersion,
              !completedVersion.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        else { return true }
        return compareVersions(completedVersion, currentVersion) == .orderedAscending
    }

    /// Component-wise numeric comparison ("0.1.9" < "0.1.10", "0.2" == "0.2.0").
    /// Non-numeric suffixes such as "-beta" are ignored.
    static func compareVersions(_ lhs: String, _ rhs: String) -> ComparisonResult {
        let left = numericComponents(of: lhs)
        let right = numericComponents(of: rhs)
        for index in 0..<max(left.count, right.count) {
            let leftValue = index < left.count ? left[index] : 0
            let rightValue = index < right.count ? right[index] : 0
            if leftValue < rightValue { return .orderedAscending }
            if leftValue > rightValue { return .orderedDescending }
        }
        return .orderedSame
    }

    private static func numericComponents(of version: String) -> [Int] {
        version
            .trimmingCharacters(in: .whitespacesAndNewlines)
            .split(separator: ".")
            .map { Int($0.prefix { $0.isNumber }) ?? 0 }
    }

    /// The Welcome window must never open inside a test host. Covers the
    /// XCTest runner variables and the Swift Testing ones.
    static func isRunningTests(
        environment: [String: String] = ProcessInfo.processInfo.environment
    ) -> Bool {
        let xctestKeys = ["XCTestConfigurationFilePath", "XCTestBundlePath", "XCTestSessionIdentifier"]
        if xctestKeys.contains(where: { environment[$0] != nil }) { return true }
        return environment.keys.contains { $0.hasPrefix("SWIFT_TESTING") || $0.hasPrefix("XCTesting") }
    }

    /// The full launch decision: not a test host, and the stored version is
    /// missing or older than the running one.
    static func shouldPresentOnLaunch(
        defaults: UserDefaults = .standard,
        currentVersion: String = currentVersion,
        environment: [String: String] = ProcessInfo.processInfo.environment
    ) -> Bool {
        guard !isRunningTests(environment: environment) else { return false }
        return shouldPresent(
            currentVersion: currentVersion,
            completedVersion: defaults.string(forKey: completedVersionKey)
        )
    }

    /// The guide's own window, found by the identifier the tracker stamps on
    /// it. Used when the tracked reference is gone.
    @MainActor
    static func existingWindow() -> NSWindow? {
        NSApp.windows.first { $0.identifier?.rawValue == windowIdentifier }
    }

    static func markCompleted(version: String = currentVersion, defaults: UserDefaults = .standard) {
        defaults.set(version, forKey: completedVersionKey)
    }
}

/// Process-wide guard so a workbench window that is closed and reopened
/// during one run does not offer the guide a second time.
@MainActor
enum OnboardingLaunch {
    static var presentedThisSession = false

    /// Returns true exactly once per process when the guide should open.
    static func consumeLaunchPresentation() -> Bool {
        guard !presentedThisSession, OnboardingPreferences.shouldPresentOnLaunch() else { return false }
        presentedThisSession = true
        return true
    }
}

/// Attached to the workbench root in `MuxaApp`: opens the Welcome window
/// on the first launch of a version, after the workbench has finished its
/// own make-key-and-order-front retries so the guide lands on top.
struct OnboardingLaunchPresenter: ViewModifier {
    @Environment(\.openWindow) private var openWindow

    func body(content: Content) -> some View {
        content.task {
            guard OnboardingLaunch.consumeLaunchPresentation() else { return }
            do {
                try await Task.sleep(for: .milliseconds(800))
            } catch {
                OnboardingLaunch.presentedThisSession = false
                return
            }
            openWindow(id: OnboardingPreferences.windowID)
            NSApp.activate(ignoringOtherApps: true)
        }
    }
}

extension View {
    /// Opens the Welcome guide on first launch of this version (see
    /// `OnboardingPreferences`). Never fires inside unit tests.
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
    }
}

// MARK: - Checklist evaluation (pure, unit-tested)

enum OnboardingCheckStatus: Equatable, Sendable {
    case ready
    case attention
    case unknown

    var systemImage: String {
        switch self {
        case .ready: "checkmark.circle.fill"
        case .attention: "exclamationmark.triangle.fill"
        case .unknown: "questionmark.circle"
        }
    }
}

enum OnboardingChecklist {
    /// Programs the checklist probes: tmux plus every known agent CLI.
    static var probedPrograms: [String] {
        ["tmux"] + InstalledTools.agentPrograms
    }

    static func connectionStatus(_ state: AppModel.ConnectionState) -> OnboardingCheckStatus {
        switch state {
        case .connected: .ready
        case .connecting: .unknown
        case .failed, .upgradeRequired: .attention
        }
    }

    /// `detected == nil` means the probe has not finished yet.
    static func toolStatus(named name: String, in detected: [InstalledTool]?) -> OnboardingCheckStatus {
        guard let detected else { return .unknown }
        return detected.contains { $0.name == name } ? .ready : .attention
    }

    static func agentTools(in detected: [InstalledTool]?) -> [InstalledTool] {
        (detected ?? []).filter { InstalledTools.agentPrograms.contains($0.name) }
    }

    static func agentsStatus(in detected: [InstalledTool]?) -> OnboardingCheckStatus {
        guard detected != nil else { return .unknown }
        return agentTools(in: detected).isEmpty ? .attention : .ready
    }

    static func askStatus(_ enabled: Bool?) -> OnboardingCheckStatus {
        switch enabled {
        case .some(true): .ready
        case .some(false): .attention
        case .none: .unknown
        }
    }

    static func workFolderStatus(path: String, exists: (String) -> Bool) -> OnboardingCheckStatus {
        let trimmed = path.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return .attention }
        return exists(trimmed) ? .ready : .attention
    }

    static func fleetHostsStatus(remoteHostCount: Int) -> OnboardingCheckStatus {
        remoteHostCount > 0 ? .ready : .attention
    }
}

// MARK: - Steps

enum OnboardingStep: Int, CaseIterable, Identifiable {
    case welcome
    case checklist
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
    @State private var step: OnboardingStep = .welcome
    @State private var dontShowAgain = true
    @State private var detectedTools: [InstalledTool]?
    @State private var isDetectingTools = false
    @State private var hostWindow: NSWindow?

    var body: some View {
        VStack(spacing: 0) {
            page
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
                .padding(28)
                .id(step)
                .transition(.opacity)

            Divider()
            footer
        }
        .frame(minWidth: 720, minHeight: 560)
        .background(OnboardingWindowTracker { window in
            // Keep the last known guide window: the tracker also reports nil
            // while the view is being torn down, and a nil here used to make
            // `finish()` close whatever window happened to be key.
            if let window { hostWindow = window }
        })
        .task { await detectTools() }
    }

    @ViewBuilder
    private var page: some View {
        switch step {
        case .welcome:
            OnboardingWelcomePage()
        case .checklist:
            OnboardingChecklistPage(
                model: model,
                detectedTools: detectedTools,
                isDetecting: isDetectingTools,
                recheck: { Task { await detectTools() } },
                openMainWindow: openMainWindow
            )
        case .flow:
            OnboardingFlowPage(model: model, openMainWindow: openMainWindow)
        case .done:
            OnboardingDonePage(dontShowAgain: $dontShowAgain)
        }
    }

    private var footer: some View {
        HStack(spacing: 12) {
            OnboardingPageIndicator(current: step)
            Spacer()
            if step.previous != nil {
                Button("Back") { move(to: step.previous) }
            }
            if step.isLast {
                Button("Get Started") { finish() }
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut(.defaultAction)
            } else {
                Button("Continue") { move(to: step.next) }
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 14)
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
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .accessibilityElement(children: .combine)
    }
}

// MARK: - Page 1: What Muxa is

private struct OnboardingWelcomePage: View {
    private let columns = [GridItem(.flexible(), spacing: 14), GridItem(.flexible(), spacing: 14)]

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

            Text("Four surfaces, one workbench")
                .font(.headline)

            LazyVGrid(columns: columns, spacing: 14) {
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
                    Text("Agents waiting on you, plus Global Ask for a quick question to any provider.")
                }
                OnboardingSurfaceCard(systemImage: MuxaSidebarMode.shells.systemImage) {
                    Text("Shells")
                } detail: {
                    Text("Native terminals for the moments you want to type alongside your agents.")
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
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: systemImage)
                .font(.title2)
                .foregroundStyle(Color.accentColor)
                .frame(width: 32, alignment: .center)
            VStack(alignment: .leading, spacing: 4) {
                title
                    .font(.headline)
                detail
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
        }
        .padding(14)
        .frame(maxWidth: .infinity, minHeight: 84, alignment: .topLeading)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
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

    private var workFolderStatus: OnboardingCheckStatus {
        OnboardingChecklist.workFolderStatus(path: workDirectory) {
            FileManager.default.fileExists(atPath: $0)
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 18) {
            HStack(alignment: .top) {
                VStack(alignment: .leading, spacing: 6) {
                    Text("Set up your workspace")
                        .font(.title2.weight(.semibold))
                    Text("Muxa checks these every time this guide opens. Nothing here is required to look around.")
                        .foregroundStyle(.secondary)
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
                    .controlSize(.small)
                    .disabled(isDetecting)
                }
            }

            ScrollView {
                VStack(spacing: 0) {
                    connectionRow
                    Divider()
                    tmuxRow
                    Divider()
                    agentsRow
                    Divider()
                    askRow
                    Divider()
                    workFolderRow
                    Divider()
                    hostsRow
                }
                .padding(.horizontal, 16)
                .padding(.vertical, 4)
                .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
            }
        }
    }

    private var connectionRow: some View {
        OnboardingChecklistRow(status: OnboardingChecklist.connectionStatus(model.connectionState)) {
            Text("muxad connected")
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
                OnboardingSettingsButton()
            }
        }
    }

    private var tmuxRow: some View {
        OnboardingChecklistRow(status: OnboardingChecklist.toolStatus(named: "tmux", in: detectedTools)) {
            Text("tmux installed")
        } detail: {
            if let tmuxTool {
                OnboardingToolLine(tool: tmuxTool)
            } else if detectedTools == nil {
                Text("Looking for tmux on your PATH…")
            } else {
                Text("tmux was not found. Install it with Homebrew (brew install tmux) and check again.")
            }
        } action: {
            EmptyView()
        }
    }

    private var agentsRow: some View {
        OnboardingChecklistRow(status: OnboardingChecklist.agentsStatus(in: detectedTools)) {
            Text("Agent CLIs found")
        } detail: {
            if detectedTools == nil {
                Text("Looking for agent CLIs on your PATH…")
            } else if agentTools.isEmpty {
                Text("No agent CLI was found. Install Claude Code, Codex, Gemini CLI, or OpenCode, then check again.")
            } else {
                VStack(alignment: .leading, spacing: 3) {
                    ForEach(agentTools) { tool in
                        OnboardingToolLine(tool: tool)
                    }
                }
            }
        } action: {
            EmptyView()
        }
    }

    private var askRow: some View {
        OnboardingChecklistRow(status: OnboardingChecklist.askStatus(model.askEnabled)) {
            Text("Global Ask enabled")
        } detail: {
            switch model.askEnabled {
            case .some(true):
                Text("Ask any configured provider from the Inbox without starting a pipeline.")
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
            case .some(false):
                OnboardingSettingsButton()
            case .none:
                EmptyView()
            }
        }
    }

    private var workFolderRow: some View {
        OnboardingChecklistRow(status: workFolderStatus) {
            Text("Work folder set")
        } detail: {
            if workDirectory.isEmpty {
                Text("Optional. Choose a default project folder so Start Work opens in the right place.")
            } else if workFolderStatus == .ready {
                Text(verbatim: workDirectory)
            } else {
                Text("\(workDirectory) is not currently available.")
            }
        } action: {
            OnboardingSettingsButton()
        }
    }

    private var hostsRow: some View {
        OnboardingChecklistRow(status: OnboardingChecklist.fleetHostsStatus(remoteHostCount: remoteHosts.count)) {
            Text("Fleet hosts registered")
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
            .disabled(!model.isConnected)
        }
    }
}

private struct OnboardingChecklistRow<Title: View, Detail: View, Action: View>: View {
    let status: OnboardingCheckStatus
    @ViewBuilder let title: Title
    @ViewBuilder let detail: Detail
    @ViewBuilder let action: Action

    private var tint: Color {
        switch status {
        case .ready: .green
        case .attention: .orange
        case .unknown: .secondary
        }
    }

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: status.systemImage)
                .font(.title3)
                .foregroundStyle(tint)
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
        .padding(.vertical, 10)
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

/// Opens Settings: `SettingsLink` on macOS 14, the AppKit selector on 13.
private struct OnboardingSettingsButton: View {
    var body: some View {
        if #available(macOS 14.0, *) {
            SettingsLink {
                Text("Open Settings…")
            }
        } else {
            Button("Open Settings…") {
                let opened = NSApp.sendAction(Selector(("showSettingsWindow:")), to: nil, from: nil)
                if !opened {
                    NSApp.sendAction(Selector(("showPreferencesWindow:")), to: nil, from: nil)
                }
            }
        }
    }
}

// MARK: - Page 3: How work flows

private struct OnboardingFlowPage: View {
    @ObservedObject var model: AppModel
    let openMainWindow: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 22) {
            VStack(alignment: .leading, spacing: 6) {
                Text("How work flows")
                    .font(.title2.weight(.semibold))
                Text("You describe the outcome. Agents do the work in stages. Muxa tells you the moment one of them needs you.")
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
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
                Label("Press ⌘⇧P for the command palette: every action, one keystroke away.", systemImage: "command")
                Label("Start Work lives in the toolbar of the main window, ready whenever you are.", systemImage: "play.square.stack")
                Label("Open Live Watch with ⌘⇧W to browse every pane on every host.", systemImage: "sidebar.left")
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
        }
        .padding(14)
        .frame(maxWidth: .infinity, minHeight: 132, alignment: .topLeading)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
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

// MARK: - Page 4: Done

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
            Text("Start Work from the toolbar, keep an eye on the Inbox, and open Live Watch whenever you want to see an agent's pane for yourself.")
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
private struct OnboardingWindowTracker: NSViewRepresentable {
    let onWindowChange: (NSWindow?) -> Void

    func makeNSView(context: Context) -> OnboardingWindowTrackingView {
        let view = OnboardingWindowTrackingView(frame: .zero)
        view.onWindowChange = onWindowChange
        return view
    }

    func updateNSView(_ nsView: OnboardingWindowTrackingView, context: Context) {
        nsView.onWindowChange = onWindowChange
    }
}

private final class OnboardingWindowTrackingView: NSView {
    var onWindowChange: ((NSWindow?) -> Void)?

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        if let window {
            window.identifier = NSUserInterfaceItemIdentifier(OnboardingPreferences.windowIdentifier)
            window.isRestorable = false
            window.makeKeyAndOrderFront(nil)
        }
        onWindowChange?(window)
    }
}
