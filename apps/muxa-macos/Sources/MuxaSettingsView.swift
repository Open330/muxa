import AppKit
import SwiftUI

enum MuxaAppearance: String, CaseIterable, Identifiable {
    case system
    case light
    case dark

    var id: Self { self }

    var title: String {
        switch self {
        case .system: String(localized: "System")
        case .light: String(localized: "Light")
        case .dark: String(localized: "Dark")
        }
    }

    var colorScheme: ColorScheme? {
        switch self {
        case .system: nil
        case .light: .light
        case .dark: .dark
        }
    }
}

enum MuxaSettingsTab: String, CaseIterable, Identifiable {
    case general
    case menuBar
    case providers
    case automations
    case behaviour
    case modules
    case fleet
    case dispatch
    case runtime
    case advanced

    var id: Self { self }
}

enum MuxaPreferences {
    static let appearanceKey = "muxa.appearance"
    static let showWorkbenchOnLaunchKey = "muxa.showWorkbenchOnLaunch"
    static let settingsTabKey = "muxa.settings.selectedTab"
    static let workDirectoryKey = "nativeWorkDirectory"
    static let menuBarAgentLimitKey = "muxa.menuBar.maximumAgentIcons"
    static let defaultMenuBarAgentLimit = 3
    static let menuBarGroupingKey = "muxa.menuBar.grouping"
    static let menuBarAasChipKey = "muxa.menuBar.showAasUsage"

    static func menuBarAgentLimit(_ defaults: UserDefaults = .standard) -> Int {
        let stored = (defaults.object(forKey: menuBarAgentLimitKey) as? NSNumber)?.intValue ?? defaultMenuBarAgentLimit
        return min(8, max(0, stored))
    }

    static func registerDefaults(_ defaults: UserDefaults = .standard) {
        defaults.register(defaults: [
            appearanceKey: MuxaAppearance.system.rawValue,
            showWorkbenchOnLaunchKey: true,
            settingsTabKey: MuxaSettingsTab.general.rawValue,
            menuBarAgentLimitKey: defaultMenuBarAgentLimit,
            menuBarGroupingKey: MuxaMenuBarGrouping.agent.rawValue,
            menuBarAasChipKey: true,
        ])
    }
}

struct MuxaSettingsView: View {
    @ObservedObject var model: AppModel
    @AppStorage(MuxaPreferences.settingsTabKey) private var selectedTab = MuxaSettingsTab.general.rawValue

    var body: some View {
        TabView(selection: $selectedTab) {
            MuxaGeneralSettingsView()
                .tabItem { Label("General", systemImage: "gearshape") }
                .tag(MuxaSettingsTab.general.rawValue)

            MuxaMenuBarSettingsView()
                .tabItem { Label("Menu Bar", systemImage: "menubar.rectangle") }
                .tag(MuxaSettingsTab.menuBar.rawValue)

            AskProvidersSettingsPane(model: model, store: AskProviderStore.shared)
                .tabItem { Label("Providers", systemImage: "brain.head.profile") }
                .tag(MuxaSettingsTab.providers.rawValue)

            AutomationSettingsPane(
                model: model,
                store: AutomationStore.shared,
                configStore: MuxaConfigStore.shared
            )
            .tabItem { Label("Automations", systemImage: "wand.and.rays") }
            .tag(MuxaSettingsTab.automations.rawValue)

            BehaviourSettingsPane(model: model, store: MuxaConfigStore.shared)
                .tabItem { Label("Behaviour", systemImage: "bell.badge") }
                .tag(MuxaSettingsTab.behaviour.rawValue)

            ModulesSettingsPane(model: model, registry: MuxaModuleRegistry.shared)
                .tabItem { Label("Modules", systemImage: "puzzlepiece.extension") }
                .tag(MuxaSettingsTab.modules.rawValue)

            MuxaFleetSettingsPane(model: model)
                .tabItem { Label("Hosts", systemImage: "server.rack") }
                .tag(MuxaSettingsTab.fleet.rawValue)

            FleetDispatchSettingsView(model: model)
                .tabItem { Label("Dispatch", systemImage: "network") }
                .tag(MuxaSettingsTab.dispatch.rawValue)

            MuxaRuntimeSettingsPane(model: model)
                .tabItem { Label("Runtime", systemImage: "terminal") }
                .tag(MuxaSettingsTab.runtime.rawValue)

            AdvancedSettingsPane(model: model, store: MuxaConfigStore.shared)
                .tabItem { Label("Advanced", systemImage: "gearshape.2") }
                .tag(MuxaSettingsTab.advanced.rawValue)
        }
        .frame(width: 760, height: 640)
    }
}

/// Works in the AppKit-hosted popover too: invoke the Settings scene's own
/// menu command rather than depending on a SwiftUI scene environment.
struct MuxaSettingsShortcut: View {
    let tab: MuxaSettingsTab
    let title: LocalizedStringKey

    var body: some View {
        Button {
            MuxaSettingsOpener.select(tab)
            MuxaSettingsOpener.openLegacySettingsWindow()
        } label: { Label(title, systemImage: "gearshape") }
    }
}

private struct MuxaMenuBarSettingsView: View {
    @AppStorage(MuxaPreferences.menuBarAgentLimitKey) private var menuBarAgentLimit = MuxaPreferences.defaultMenuBarAgentLimit
    @AppStorage(MuxaPreferences.menuBarGroupingKey) private var menuBarGrouping = MuxaMenuBarGrouping.agent.rawValue
    @AppStorage(MuxaPreferences.menuBarAasChipKey) private var showAasChip = true
    @ObservedObject private var registry = MuxaModuleRegistry.shared

    var body: some View {
        Form {
            Section("Menu Bar") {
                Picker("Group activity", selection: $menuBarGrouping) {
                    ForEach(MuxaMenuBarGrouping.allCases) { grouping in Text(grouping.title).tag(grouping.rawValue) }
                }
                Picker("Maximum activity icons", selection: $menuBarAgentLimit) {
                    Text("Hide activity icons").tag(0)
                    ForEach(1...8, id: \.self) { count in
                        Text(verbatim: String(count)).tag(count)
                    }
                }
                Text("Additional activity stays in the +N list. Reviewed agents are hidden until new activity arrives.")
                    .font(MuxaType.detail).foregroundStyle(.secondary)
                Text("AAS appears as a hoverable usage chip when its module is enabled.")
                    .font(MuxaType.detail).foregroundStyle(.secondary)
            }

            Section("Accounts and usage") {
                Toggle("Show AAS usage chip", isOn: $showAasChip)
                Text("Hover to see active account usage. Click to compare all accounts.")
                    .font(MuxaType.detail).foregroundStyle(.secondary)
                if !registry.isEnabled("aas") {
                    Button("Enable AAS module") { registry.setEnabled("aas", true) }
                } else if let module = registry.module(AasModule.self), !module.availability.isAvailable {
                    Text(verbatim: module.availability.summary).font(MuxaType.detail).foregroundStyle(.secondary)
                }
                MuxaSettingsShortcut(tab: .modules, title: "Open Accounts settings")
            }
        }
        .formStyle(.grouped)
        .padding(.top, 8)
    }
}

private struct MuxaGeneralSettingsView: View {
    @AppStorage(MuxaPreferences.appearanceKey) private var appearance = MuxaAppearance.system.rawValue
    @AppStorage(MuxaPreferences.showWorkbenchOnLaunchKey) private var showWorkbenchOnLaunch = true
    @AppStorage(MuxaPreferences.workDirectoryKey) private var workDirectory = ""
    @Environment(\.openWindow) private var openWindow

    private var directoryExists: Bool {
        workDirectory.isEmpty || FileManager.default.fileExists(atPath: workDirectory)
    }

    var body: some View {
        Form {
            Section("Appearance") {
                Picker("Theme", selection: $appearance) {
                    ForEach(MuxaAppearance.allCases) { option in
                        Text(option.title).tag(option.rawValue)
                    }
                }
                .pickerStyle(.segmented)
                Text("System follows the current macOS appearance automatically.")
                    .font(MuxaType.detail)
                    .foregroundStyle(.secondary)
            }

            MuxaLanguageSettingsSection()

            Section("Startup") {
                Toggle("Show the Workbench when Muxa launches", isOn: $showWorkbenchOnLaunch)
                Text("When disabled, Muxa starts in the menu bar and keeps host monitoring available.")
                    .font(MuxaType.detail)
                    .foregroundStyle(.secondary)
                HStack {
                    Button("Show Welcome Guide…") {
                        openWindow(id: OnboardingPreferences.windowID)
                        NSApp.activate(ignoringOtherApps: true)
                    }
                    Text("The first-launch tour of Work, Explore, Inbox, and Shells, with the setup checklist.")
                        .font(MuxaType.detail)
                        .foregroundStyle(.secondary)
                }
            }

            Section("Work") {
                HStack {
                    TextField("Default project folder", text: $workDirectory)
                        .textFieldStyle(.roundedBorder)
                    Button("Choose…", action: chooseWorkDirectory)
                    if !workDirectory.isEmpty {
                        Button("Clear") { workDirectory = "" }
                    }
                }
                if !directoryExists {
                    Label("This folder is not currently available.", systemImage: "exclamationmark.triangle.fill")
                        .font(MuxaType.detail)
                        .foregroundStyle(.orange)
                } else {
                    Text("Used as the initial folder in Start Work. Leave empty to use route configuration.")
                        .font(MuxaType.detail)
                        .foregroundStyle(.secondary)
                }
            }
        }
        .formStyle(.grouped)
        .padding(.top, 8)
    }

    private func chooseWorkDirectory() {
        let panel = NSOpenPanel()
        panel.title = String(localized: "Choose the default Muxa Work folder")
        panel.prompt = String(localized: "Choose")
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        if !workDirectory.isEmpty {
            panel.directoryURL = URL(fileURLWithPath: workDirectory, isDirectory: true)
        }
        if panel.runModal() == .OK, let url = panel.url {
            workDirectory = url.path
        }
    }
}

private struct MuxaFleetSettingsPane: View {
    @ObservedObject var model: AppModel
    @State private var isRegisteringHost = false

    private var hosts: [MuxaFleetHost] {
        model.fleetHosts.sorted { left, right in
            if left.local != right.local { return left.local }
            return left.alias.localizedStandardCompare(right.alias) == .orderedAscending
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                settingsHeading(
                    "Hosts",
                    detail: "Local and SSH hosts registered with the central muxad controller."
                )
                Spacer()
                Button {
                    Task { await model.refresh() }
                } label: {
                    Label("Refresh", systemImage: "arrow.clockwise")
                }
                Button {
                    model.prepareHostRegistration()
                    isRegisteringHost = true
                } label: {
                    Label("Add Host…", systemImage: "plus")
                }
                .buttonStyle(.borderedProminent)
            }
            .padding(20)

            Divider()

            if hosts.isEmpty {
                VStack(spacing: 10) {
                    Image(systemName: "server.rack")
                        .font(.system(size: 30))
                        .foregroundStyle(.secondary)
                    Text("No Hosts").font(.headline)
                    Text("Register an SSH host to monitor its agents from Muxa.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ScrollView {
                    LazyVStack(spacing: 10) {
                        ForEach(hosts) { host in
                            MuxaFleetSettingsRow(host: host)
                        }
                        if !hosts.contains(where: { !$0.local }) {
                            Label("No SSH hosts yet. Add one to watch its panes and run Work on it from this Mac.", systemImage: "info.circle")
                                .font(MuxaType.detail)
                                .foregroundStyle(.secondary)
                                .frame(maxWidth: .infinity, alignment: .leading)
                        }
                    }
                    .padding(16)
                }
            }
        }
        .sheet(isPresented: $isRegisteringHost) {
            HostRegistrationView(model: model)
        }
    }
}

private struct MuxaFleetSettingsRow: View {
    let host: MuxaFleetHost

    private var stateColor: Color {
        fleetHostColor(host.state)
    }

    var body: some View {
        HStack(spacing: 12) {
            ZStack {
                RoundedRectangle(cornerRadius: 9)
                    .fill(Color.accentColor.opacity(0.1))
                Image(systemName: host.local ? "desktopcomputer" : "server.rack")
                    .foregroundStyle(.tint)
            }
            .frame(width: 42, height: 42)

            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 7) {
                    Circle().fill(stateColor).frame(width: 7, height: 7)
                    Text(host.alias).font(.headline)
                    Group {
                        if host.local {
                            Text("Local")
                        } else {
                            Text(fleetHostModeLabel(host.mode))
                        }
                    }
                    .font(MuxaType.meta.weight(.semibold))
                    .foregroundStyle(.secondary)
                }
                Group {
                    if let target = host.sshTarget {
                        Text(target)
                    } else if host.local {
                        Text(verbatim: "local://")
                    } else {
                        Text("SSH target unavailable")
                    }
                }
                .font(MuxaType.detail.monospaced())
                .foregroundStyle(.secondary)
                .lineLimit(1)
            }

            Spacer()

            VStack(alignment: .trailing, spacing: 4) {
                Group {
                    if let version = host.muxaVersion {
                        Text("muxa \(version)")
                    } else {
                        Text("Version unavailable")
                    }
                }
                .font(MuxaType.detail.monospacedDigit())
                Text("\(host.remote?.agents.count ?? 0) agents · \(host.remote?.panes.count ?? 0) panes")
                    .font(MuxaType.meta)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(12)
        .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: 10))
    }
}

private struct MuxaRuntimeSettingsPane: View {
    @ObservedObject var model: AppModel

    private static let buildCommit: String? = {
        guard let url = Bundle.main.url(forResource: "muxa-build", withExtension: "json"),
              let data = try? Data(contentsOf: url),
              let document = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let commit = document["commit"] as? String else { return nil }
        return String(commit.prefix(12))
    }()

    private var localHost: MuxaFleetHost? {
        model.fleetHosts.first(where: \.local)
    }

    private var connectionTitle: String {
        switch model.connectionState {
        case .connecting: String(localized: "Connecting")
        case .connected: String(localized: "Connected")
        case .upgradeRequired: String(localized: "Upgrade required")
        case .failed: String(localized: "Connection failed")
        }
    }

    private var connectionColor: Color {
        switch model.connectionState {
        case .connected: .green
        case .connecting: .orange
        case .upgradeRequired, .failed: .red
        }
    }

    var body: some View {
        Form {
            Section("Connection") {
                LabeledContent("Status") {
                    Label(connectionTitle, systemImage: "circle.fill")
                        .foregroundStyle(connectionColor)
                }
                LabeledContent("Socket") {
                    Text(model.client.socketPath)
                        .font(.callout.monospaced())
                        .textSelection(.enabled)
                }
                if case .failed(let detail) = model.connectionState {
                    Text(detail).font(MuxaType.detail).foregroundStyle(.red).textSelection(.enabled)
                }
                if case .upgradeRequired(let detail) = model.connectionState {
                    Text(detail).font(MuxaType.detail).foregroundStyle(.red).textSelection(.enabled)
                }
            }

            Section("Runtime") {
                if let commit = Self.buildCommit {
                    LabeledContent("App build") {
                        Text(verbatim: commit).font(.callout.monospaced()).textSelection(.enabled)
                    }
                }
                LabeledContent("muxa") {
                    Group {
                        if let version = localHost?.muxaVersion {
                            Text(version)
                        } else {
                            Text("Unavailable")
                        }
                    }
                    .monospacedDigit()
                }
                LabeledContent("Daemon generation") {
                    Group {
                        if let generation = localHost?.daemonGeneration {
                            Text(verbatim: String(generation))
                        } else {
                            Text("Unavailable")
                        }
                    }
                    .monospacedDigit()
                }
                LabeledContent("Native shells") {
                    Text("\(model.sessions.lazy.filter { !$0.exited }.count) active")
                        .monospacedDigit()
                }
                LabeledContent("Hosts") {
                    Text("\(model.fleetHosts.lazy.filter { $0.state == "online" }.count) / \(model.fleetHosts.count) online")
                        .monospacedDigit()
                }
            }

            Section {
                HStack {
                    Button("Retry Connection") { model.retryConnection() }
                        .disabled(model.connectionState == .connecting)
                    Spacer()
                    MuxaDaemonReloadButton(model: model, title: "Reload Bundled muxad…")
                }
                Text("Reloading replaces the process on the owner-only socket. tmux sessions remain, but native PTY sessions end.")
                    .font(MuxaType.detail)
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .padding(.top, 8)
    }
}

/// The Runtime tab's daemon reload, shared by every pane that has to say a
/// change only applies after muxad restarts. One button, one confirmation.
struct MuxaDaemonReloadButton: View {
    @ObservedObject var model: AppModel
    var title: LocalizedStringKey = "Reload muxad…"
    @State private var confirmsReload = false

    var body: some View {
        Button(title) { confirmsReload = true }
            .alert("Reload the bundled muxad?", isPresented: $confirmsReload) {
                Button("Cancel", role: .cancel) {}
                Button("Reload", role: .destructive) { model.replaceRunningDaemon() }
            } message: {
                Text("\(model.sessions.lazy.filter { !$0.exited }.count) active native shells will end. tmux sessions are not terminated.")
            }
    }
}

/// Shared by the settings panes, including `AskProvidersSettingsPane`.
@ViewBuilder
func settingsHeading(_ title: LocalizedStringKey, detail: LocalizedStringKey) -> some View {
    VStack(alignment: .leading, spacing: 3) {
        Text(title).font(.title2.weight(.semibold))
        Text(detail).font(.subheadline).foregroundStyle(.secondary)
    }
}
