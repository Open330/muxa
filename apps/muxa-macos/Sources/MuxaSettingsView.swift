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
    case providers
    case automations
    case behaviour
    case fleet
    case runtime
    case advanced

    var id: Self { self }
}

enum MuxaPreferences {
    static let appearanceKey = "muxa.appearance"
    static let showWorkbenchOnLaunchKey = "muxa.showWorkbenchOnLaunch"
    static let settingsTabKey = "muxa.settings.selectedTab"
    static let workDirectoryKey = "nativeWorkDirectory"

    static func registerDefaults(_ defaults: UserDefaults = .standard) {
        defaults.register(defaults: [
            appearanceKey: MuxaAppearance.system.rawValue,
            showWorkbenchOnLaunchKey: true,
            settingsTabKey: MuxaSettingsTab.general.rawValue,
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

            MuxaFleetSettingsPane(model: model)
                .tabItem { Label("Hosts", systemImage: "server.rack") }
                .tag(MuxaSettingsTab.fleet.rawValue)

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
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            MuxaLanguageSettingsSection()

            Section("Startup") {
                Toggle("Show the Workbench when Muxa launches", isOn: $showWorkbenchOnLaunch)
                Text("When disabled, Muxa starts in the menu bar and keeps host monitoring available.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                HStack {
                    Button("Show Welcome Guide…") {
                        openWindow(id: OnboardingPreferences.windowID)
                        NSApp.activate(ignoringOtherApps: true)
                    }
                    Text("The first-launch tour of Work, Explore, Inbox, and Shells, with the setup checklist.")
                        .font(.caption)
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
                        .font(.caption)
                        .foregroundStyle(.orange)
                } else {
                    Text("Used as the initial folder in Start Work. Leave empty to use route configuration.")
                        .font(.caption)
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
                    .font(.caption2.weight(.semibold))
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
                .font(.caption.monospaced())
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
                .font(.caption.monospacedDigit())
                Text("\(host.remote?.agents.count ?? 0) agents · \(host.remote?.panes.count ?? 0) panes")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(12)
        .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: 10))
    }
}

private struct MuxaRuntimeSettingsPane: View {
    @ObservedObject var model: AppModel

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
                    Text(detail).font(.caption).foregroundStyle(.red).textSelection(.enabled)
                }
                if case .upgradeRequired(let detail) = model.connectionState {
                    Text(detail).font(.caption).foregroundStyle(.red).textSelection(.enabled)
                }
            }

            Section("Runtime") {
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
                    .font(.caption)
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
