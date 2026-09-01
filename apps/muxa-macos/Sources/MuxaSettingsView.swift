import AppKit
import SwiftUI

enum MuxaAppearance: String, CaseIterable, Identifiable {
    case system
    case light
    case dark

    var id: Self { self }

    var title: String {
        switch self {
        case .system: "System"
        case .light: "Light"
        case .dark: "Dark"
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
    case fleet
    case runtime

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

            MuxaProviderSettingsPane(model: model)
                .tabItem { Label("Providers", systemImage: "brain.head.profile") }
                .tag(MuxaSettingsTab.providers.rawValue)

            MuxaFleetSettingsPane(model: model)
                .tabItem { Label("Hosts", systemImage: "server.rack") }
                .tag(MuxaSettingsTab.fleet.rawValue)

            MuxaRuntimeSettingsPane(model: model)
                .tabItem { Label("Runtime", systemImage: "terminal") }
                .tag(MuxaSettingsTab.runtime.rawValue)
        }
        .frame(width: 700, height: 560)
    }
}

private struct MuxaGeneralSettingsView: View {
    @AppStorage(MuxaPreferences.appearanceKey) private var appearance = MuxaAppearance.system.rawValue
    @AppStorage(MuxaPreferences.showWorkbenchOnLaunchKey) private var showWorkbenchOnLaunch = true
    @AppStorage(MuxaPreferences.workDirectoryKey) private var workDirectory = ""

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

            Section("Startup") {
                Toggle("Show the Workbench when Muxa launches", isOn: $showWorkbenchOnLaunch)
                Text("When disabled, Muxa starts in the menu bar and keeps host monitoring available.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
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
        panel.title = "Choose the default Muxa Work folder"
        panel.prompt = "Choose"
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

private struct MuxaProviderSettingsPane: View {
    @ObservedObject var model: AppModel
    @State private var confirmsReload = false

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                settingsHeading(
                    "Ask Providers",
                    detail: "Use Claude Code or Codex sign-in, or store an optional API key in the macOS login Keychain."
                )

                if model.askEnabled == false {
                    HStack {
                        Label("Global Ask is disabled in muxa configuration.", systemImage: "exclamationmark.circle")
                            .foregroundStyle(.orange)
                        Spacer()
                        Button("Enable Global Ask") {
                            Task { await model.enableAsk() }
                        }
                        .buttonStyle(.borderedProminent)
                        .disabled(model.isEnablingAsk)
                    }
                    .padding(12)
                    .background(Color.orange.opacity(0.08), in: RoundedRectangle(cornerRadius: 10))
                }

                ForEach(MuxaAskProvider.allCases) { provider in
                    AskProviderCredentialRow(provider: provider, model: model)
                }

                if let status = model.askSettingsStatus {
                    Label(status, systemImage: "checkmark.circle.fill")
                        .font(.caption)
                        .foregroundStyle(.green)
                }
                if let error = model.askSettingsError {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .font(.caption)
                        .foregroundStyle(.red)
                        .textSelection(.enabled)
                }

                HStack {
                    Text("Reload only after installing a provider CLI in a new PATH.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    Spacer()
                    Button("Reload muxad PATH…") { confirmsReload = true }
                }
            }
            .padding(20)
        }
        .alert("Reload the bundled muxad?", isPresented: $confirmsReload) {
            Button("Cancel", role: .cancel) {}
            Button("Reload", role: .destructive) { model.replaceRunningDaemon() }
        } message: {
            Text("Native PTY sessions owned by muxad will end. tmux sessions are not terminated.")
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
                    Text(host.local ? "Local" : host.mode.capitalized)
                        .font(.caption2.weight(.semibold))
                        .foregroundStyle(.secondary)
                }
                Text(host.sshTarget ?? (host.local ? "local://" : "SSH target unavailable"))
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }

            Spacer()

            VStack(alignment: .trailing, spacing: 4) {
                Text(host.muxaVersion.map { "muxa \($0)" } ?? "Version unavailable")
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
    @State private var confirmsReload = false

    private var localHost: MuxaFleetHost? {
        model.fleetHosts.first(where: \.local)
    }

    private var connectionTitle: String {
        switch model.connectionState {
        case .connecting: "Connecting"
        case .connected: "Connected"
        case .upgradeRequired: "Upgrade required"
        case .failed: "Connection failed"
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
                    Text(localHost?.muxaVersion ?? "Unavailable").monospacedDigit()
                }
                LabeledContent("Daemon generation") {
                    Text(localHost?.daemonGeneration.map(String.init) ?? "Unavailable")
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
                    Button("Reload Bundled muxad…") { confirmsReload = true }
                }
                Text("Reloading replaces the process on the owner-only socket. tmux sessions remain, but native PTY sessions end.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .padding(.top, 8)
        .alert("Reload the bundled muxad?", isPresented: $confirmsReload) {
            Button("Cancel", role: .cancel) {}
            Button("Reload", role: .destructive) { model.replaceRunningDaemon() }
        } message: {
            Text("\(model.sessions.lazy.filter { !$0.exited }.count) active native shell(s) will end. tmux sessions are not terminated.")
        }
    }
}

@ViewBuilder
private func settingsHeading(_ title: String, detail: String) -> some View {
    VStack(alignment: .leading, spacing: 3) {
        Text(title).font(.title2.weight(.semibold))
        Text(detail).font(.subheadline).foregroundStyle(.secondary)
    }
}
