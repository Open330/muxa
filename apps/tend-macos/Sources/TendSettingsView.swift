import AppKit
import SwiftUI

enum TendAppearance: String, CaseIterable, Identifiable {
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

enum TendSettingsTab: String, CaseIterable, Identifiable {
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

enum TendPreferences {
    static let appearanceKey = "tend.appearance"
    static let showWorkbenchOnLaunchKey = "tend.showWorkbenchOnLaunch"
    static let homeAnimatesMovesKey = "tend.home.animatesMoves"
    static let settingsTabKey = "tend.settings.selectedTab"
    static let workDirectoryKey = "nativeWorkDirectory"
    static let menuBarAgentLimitKey = "tend.menuBar.maximumAgentIcons"
    static let defaultMenuBarAgentLimit = 3
    static let menuBarGroupingKey = "tend.menuBar.grouping"
    static let menuBarAasChipKey = "tend.menuBar.showAasUsage"

    static func menuBarAgentLimit(_ defaults: UserDefaults = .standard) -> Int {
        let stored = (defaults.object(forKey: menuBarAgentLimitKey) as? NSNumber)?.intValue ?? defaultMenuBarAgentLimit
        return min(8, max(0, stored))
    }

    static func registerDefaults(_ defaults: UserDefaults = .standard) {
        defaults.register(defaults: [
            appearanceKey: TendAppearance.system.rawValue,
            showWorkbenchOnLaunchKey: true,
            settingsTabKey: TendSettingsTab.general.rawValue,
            menuBarAgentLimitKey: defaultMenuBarAgentLimit,
            menuBarGroupingKey: TendMenuBarGrouping.agent.rawValue,
            menuBarAasChipKey: true,
        ])
    }
}

struct TendSettingsView: View {
    @ObservedObject var model: AppModel
    @AppStorage(TendPreferences.settingsTabKey) private var selectedTab = TendSettingsTab.general.rawValue

    var body: some View {
        TabView(selection: $selectedTab) {
            TendGeneralSettingsView()
                .tabItem { Label("General", systemImage: "gearshape") }
                .tag(TendSettingsTab.general.rawValue)

            TendMenuBarSettingsView(model: model)
                .tabItem { Label("Menu Bar", systemImage: "menubar.rectangle") }
                .tag(TendSettingsTab.menuBar.rawValue)

            AskProvidersSettingsPane(model: model, store: AskProviderStore.shared)
                .tabItem { Label("Providers", systemImage: "brain.head.profile") }
                .tag(TendSettingsTab.providers.rawValue)

            AutomationSettingsPane(
                model: model,
                store: AutomationStore.shared,
                configStore: TendConfigStore.shared
            )
            .tabItem { Label("Automations", systemImage: "wand.and.rays") }
            .tag(TendSettingsTab.automations.rawValue)

            BehaviourSettingsPane(model: model, store: TendConfigStore.shared)
                .tabItem { Label("Behaviour", systemImage: "bell.badge") }
                .tag(TendSettingsTab.behaviour.rawValue)

            ModulesSettingsPane(model: model, registry: TendModuleRegistry.shared)
                .tabItem { Label("Modules", systemImage: "puzzlepiece.extension") }
                .tag(TendSettingsTab.modules.rawValue)

            TendFleetSettingsPane(model: model)
                .tabItem { Label("Hosts", systemImage: "server.rack") }
                .tag(TendSettingsTab.fleet.rawValue)

            FleetDispatchSettingsView(model: model)
                .tabItem { Label("Dispatch", systemImage: "network") }
                .tag(TendSettingsTab.dispatch.rawValue)

            TendRuntimeSettingsPane(model: model)
                .tabItem { Label("Runtime", systemImage: "terminal") }
                .tag(TendSettingsTab.runtime.rawValue)

            AdvancedSettingsPane(model: model, store: TendConfigStore.shared)
                .tabItem { Label("Advanced", systemImage: "gearshape.2") }
                .tag(TendSettingsTab.advanced.rawValue)
        }
        .frame(width: 760, height: 640)
    }
}

/// Works in the AppKit-hosted popover too: invoke the Settings scene's own
/// menu command rather than depending on a SwiftUI scene environment.
struct TendSettingsShortcut: View {
    let tab: TendSettingsTab
    let title: LocalizedStringKey

    var body: some View {
        Button {
            TendSettingsOpener.select(tab)
            TendSettingsOpener.openLegacySettingsWindow()
        } label: { Label(title, systemImage: "gearshape") }
    }
}

private struct TendMenuBarSettingsView: View {
    @ObservedObject var model: AppModel
    @State private var hostIcons = TendHostIcons.all()
    @State private var customText: [String: String] = [:]
    @AppStorage(TendPreferences.menuBarAgentLimitKey) private var menuBarAgentLimit = TendPreferences.defaultMenuBarAgentLimit
    @AppStorage(TendPreferences.menuBarGroupingKey) private var menuBarGrouping = TendMenuBarGrouping.agent.rawValue
    @AppStorage(TendPreferences.menuBarAasChipKey) private var showAasChip = true
    @ObservedObject private var registry = TendModuleRegistry.shared

    var body: some View {
        Form {
            Section("Menu Bar") {
                Picker("Group activity", selection: $menuBarGrouping) {
                    ForEach(TendMenuBarGrouping.allCases) { grouping in Text(grouping.title).tag(grouping.rawValue) }
                }
                Picker("Maximum activity icons", selection: $menuBarAgentLimit) {
                    Text("Hide activity icons").tag(0)
                    ForEach(1...8, id: \.self) { count in
                        Text(verbatim: String(count)).tag(count)
                    }
                }
                Text("Additional activity stays in the +N list. Reviewed agents are hidden until new activity arrives.")
                    .font(TendType.detail).foregroundStyle(.secondary)
                Text("AAS appears as a hoverable usage chip when its module is enabled.")
                    .font(TendType.detail).foregroundStyle(.secondary)
            }

            Section("Fleet host icons") {
                Text("Shown when activity is grouped by host. Agents show their provider: a spark for Claude, >_ for Codex.")
                    .font(TendType.detail).foregroundStyle(.secondary)
                if model.fleetHosts.isEmpty {
                    Text("No fleet hosts yet.").font(TendType.detail).foregroundStyle(.secondary)
                }
                ForEach(model.fleetHosts) { host in
                    hostIconRow(host)
                }
            }

            Section("Accounts and usage") {
                Toggle("Show AAS usage chip", isOn: $showAasChip)
                Text("Hover to see active account usage. Click to compare all accounts.")
                    .font(TendType.detail).foregroundStyle(.secondary)
                if !registry.isEnabled("aas") {
                    Button("Enable AAS module") { registry.setEnabled("aas", true) }
                } else if let module = registry.module(AasModule.self), !module.availability.isAvailable {
                    Text(verbatim: module.availability.summary).font(TendType.detail).foregroundStyle(.secondary)
                }
                TendSettingsShortcut(tab: .modules, title: "Open Accounts settings")
            }
        }
        .formStyle(.grouped)
        .padding(.top, 8)
    }

    private func hostIconRow(_ host: TendFleetHost) -> some View {
        let icon = TendHostIcons.icon(for: host.alias, local: host.local)
        let isCustomText = TendGlyph.kind(icon) != .symbol(icon)
        return HStack(spacing: 10) {
            TendGlyphView(value: icon, size: 12)
                .frame(width: 26, height: 26)
                .background(TendMenuBarAgentAppearance(identity: host.alias, symbol: icon, provider: "").gradient, in: Circle())
            Text(verbatim: host.alias)
            Spacer()
            Menu {
                ForEach(TendHostIcons.choices, id: \.self) { symbol in
                    Button {
                        setIcon(symbol, for: host.alias)
                    } label: {
                        Label(symbol, systemImage: symbol)
                    }
                }
            } label: {
                Image(systemName: isCustomText ? "character.cursor.ibeam" : icon)
            }
            .menuStyle(.borderlessButton)
            .fixedSize()
            .help("Choose a symbol")
            TextField("Text", text: Binding(
                get: { customText[host.alias] ?? (isCustomText ? String(icon.dropFirst(TendGlyph.textPrefix.count)) : "") },
                set: { value in
                    let cleaned = TendGlyph.cleanedText(value)
                    customText[host.alias] = cleaned
                    setIcon(cleaned.isEmpty ? nil : TendGlyph.text(cleaned), for: host.alias)
                }
            ))
            .frame(width: 54)
            .help("Up to two characters or an emoji instead of a symbol")
            if hostIcons[host.alias] != nil {
                Button("Reset") {
                    customText[host.alias] = ""
                    setIcon(nil, for: host.alias)
                }
                .controlSize(.small)
            }
        }
    }

    private func setIcon(_ icon: String?, for alias: String) {
        if let icon, TendGlyph.kind(icon) == .symbol(icon) { customText[alias] = "" }
        TendHostIcons.set(icon, for: alias)
        hostIcons = TendHostIcons.all()
    }
}

private struct TendGeneralSettingsView: View {
    @AppStorage(TendPreferences.appearanceKey) private var appearance = TendAppearance.system.rawValue
    @AppStorage(TendPreferences.showWorkbenchOnLaunchKey) private var showWorkbenchOnLaunch = true
    @AppStorage(TendPreferences.homeAnimatesMovesKey) private var homeAnimatesMoves = true
    @AppStorage(TendHotKeyPreset.defaultsKey) private var agentsHotKey = TendHotKeyPreset.off.rawValue
    @ObservedObject private var globalHotKey = TendGlobalHotKey.shared
    @AppStorage(TendPreferences.workDirectoryKey) private var workDirectory = ""
    @Environment(\.openWindow) private var openWindow

    private var directoryExists: Bool {
        workDirectory.isEmpty || FileManager.default.fileExists(atPath: workDirectory)
    }

    var body: some View {
        Form {
            Section("Appearance") {
                Picker("Theme", selection: $appearance) {
                    ForEach(TendAppearance.allCases) { option in
                        Text(option.title).tag(option.rawValue)
                    }
                }
                .pickerStyle(.segmented)
                Text("System follows the current macOS appearance automatically.")
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
            }

            TendLanguageSettingsSection()

            Section("Home") {
                Toggle("Animate cards moving between lanes", isOn: $homeAnimatesMoves)
                Text("A card glides to its new lane and shows where it came from for a few seconds.")
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
                Picker("Show agents from any app", selection: $agentsHotKey) {
                    ForEach(TendHotKeyPreset.allCases) { preset in
                        Text(preset.title).tag(preset.rawValue)
                    }
                }
                .onChange(of: agentsHotKey) { value in
                    globalHotKey.apply(TendHotKeyPreset(rawValue: value) ?? .off)
                }
                if let problem = globalHotKey.problem {
                    Label(problem, systemImage: "exclamationmark.triangle.fill")
                        .font(TendType.detail)
                        .foregroundStyle(.orange)
                } else {
                    Text("Brings Tend forward with the agents palette, those waiting on you first.")
                        .font(TendType.detail)
                        .foregroundStyle(.secondary)
                }
            }

            Section("Startup") {
                Toggle("Show the Workbench when Tend launches", isOn: $showWorkbenchOnLaunch)
                Text("When disabled, Tend starts in the menu bar and keeps host monitoring available.")
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
                HStack {
                    Button("Show Welcome Guide…") {
                        openWindow(id: OnboardingPreferences.windowID)
                        NSApp.activate(ignoringOtherApps: true)
                    }
                    Text("The first-launch tour of Work, Explore, Inbox, and Shells, with the setup checklist.")
                        .font(TendType.detail)
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
                        .font(TendType.detail)
                        .foregroundStyle(.orange)
                } else {
                    Text("Used as the initial folder in Start Work. Leave empty to use route configuration.")
                        .font(TendType.detail)
                        .foregroundStyle(.secondary)
                }
            }
        }
        .formStyle(.grouped)
        .padding(.top, 8)
    }

    private func chooseWorkDirectory() {
        let panel = NSOpenPanel()
        panel.title = String(localized: "Choose the default Tend Work folder")
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

private struct TendFleetSettingsPane: View {
    @ObservedObject var model: AppModel
    @State private var isRegisteringHost = false

    private var hosts: [TendFleetHost] {
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
                    detail: "Local and SSH hosts registered with the central tendd controller."
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
                    Text("Register an SSH host to monitor its agents from Tend.")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ScrollView {
                    LazyVStack(spacing: 10) {
                        ForEach(hosts) { host in
                            TendFleetSettingsRow(host: host)
                        }
                        if !hosts.contains(where: { !$0.local }) {
                            Label("No SSH hosts yet. Add one to watch its panes and run Work on it from this Mac.", systemImage: "info.circle")
                                .font(TendType.detail)
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

private struct TendFleetSettingsRow: View {
    let host: TendFleetHost

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
                    .font(TendType.meta.weight(.semibold))
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
                .font(TendType.detail.monospaced())
                .foregroundStyle(.secondary)
                .lineLimit(1)
            }

            Spacer()

            VStack(alignment: .trailing, spacing: 4) {
                Group {
                    if let version = host.tendVersion {
                        Text("tend \(version)")
                    } else {
                        Text("Version unavailable")
                    }
                }
                .font(TendType.detail.monospacedDigit())
                Text("\(host.remote?.agents.count ?? 0) agents · \(host.remote?.panes.count ?? 0) panes")
                    .font(TendType.meta)
                    .foregroundStyle(.secondary)
            }
        }
        .padding(12)
        .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: 10))
    }
}

private struct TendRuntimeSettingsPane: View {
    @ObservedObject var model: AppModel

    private static let buildCommit: String? = {
        guard let url = Bundle.main.url(forResource: "tend-build", withExtension: "json"),
              let data = try? Data(contentsOf: url),
              let document = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let commit = document["commit"] as? String else { return nil }
        return String(commit.prefix(12))
    }()

    private var localHost: TendFleetHost? {
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
                    Text(detail).font(TendType.detail).foregroundStyle(.red).textSelection(.enabled)
                }
                if case .upgradeRequired(let detail) = model.connectionState {
                    Text(detail).font(TendType.detail).foregroundStyle(.red).textSelection(.enabled)
                }
            }

            Section("Runtime") {
                if let commit = Self.buildCommit {
                    LabeledContent("App build") {
                        Text(verbatim: commit).font(.callout.monospaced()).textSelection(.enabled)
                    }
                }
                LabeledContent("tend") {
                    Group {
                        if let version = localHost?.tendVersion {
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
                    TendDaemonReloadButton(model: model, title: "Reload Bundled tendd…")
                }
                Text("Reloading replaces the process on the owner-only socket. tmux sessions remain, but native PTY sessions end.")
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .padding(.top, 8)
    }
}

/// The Runtime tab's daemon reload, shared by every pane that has to say a
/// change only applies after tendd restarts. One button, one confirmation.
struct TendDaemonReloadButton: View {
    @ObservedObject var model: AppModel
    var title: LocalizedStringKey = "Reload tendd…"
    @State private var confirmsReload = false

    var body: some View {
        Button(title) { confirmsReload = true }
            .alert("Reload the bundled tendd?", isPresented: $confirmsReload) {
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
