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

/// The Settings window's tabs, in toolbar order.
enum TendSettingsTab: String, CaseIterable, Identifiable {
    case general
    case notifications
    case menuBar
    case accounts
    case ask
    case agents
    case automations
    case fleet
    case advanced

    var id: Self { self }

    var title: LocalizedStringKey {
        switch self {
        case .general: "General"
        case .notifications: "Notifications"
        case .menuBar: "Menu Bar"
        case .accounts: "Accounts"
        case .ask: "Ask"
        case .agents: "Agents"
        case .automations: "Automations"
        case .fleet: "Fleet"
        case .advanced: "Advanced"
        }
    }

    /// The toolbar symbol, with an older one where the preferred symbol is
    /// newer than the running macOS.
    var systemImage: String {
        let preferred = preferredSystemImage
        guard let fallback = fallbackSystemImage else { return preferred }
        return NSImage(systemSymbolName: preferred, accessibilityDescription: nil) == nil ? fallback : preferred
    }

    var preferredSystemImage: String {
        switch self {
        case .general: "gearshape"
        case .notifications: "bell.badge"
        case .menuBar: "menubar.rectangle"
        case .accounts: "person.2"
        case .ask: "bubble.left.and.text.bubble.right"
        case .agents: "person.3.sequence"
        case .automations: "bolt.badge.automatic"
        case .fleet: "server.rack"
        case .advanced: "slider.horizontal.3"
        }
    }

    var fallbackSystemImage: String? {
        switch self {
        case .ask: "bubble.left.and.bubble.right"
        case .agents: "square.stack.3d.up"
        case .automations: "bolt"
        default: nil
        }
    }

    /// The tab a stored selection names. Earlier builds had Providers,
    /// Behaviour, Modules, Dispatch and Runtime tabs; a selection saved by
    /// one of them opens the tab that holds those settings now.
    static func resolve(_ rawValue: String) -> TendSettingsTab {
        if let tab = TendSettingsTab(rawValue: rawValue) { return tab }
        switch rawValue {
        case "providers": return .ask
        case "behaviour": return .notifications
        case "modules": return .accounts
        case "dispatch": return .fleet
        case "runtime": return .advanced
        default: return .general
        }
    }

    /// The Fleet tab's section a stored selection asks for, when it names one.
    static func fleetSection(for rawValue: String) -> TendFleetSettingsSection? {
        rawValue == "dispatch" ? .dispatch : nil
    }
}

/// The Fleet tab's two halves.
enum TendFleetSettingsSection: String, CaseIterable, Identifiable {
    case hosts
    case dispatch

    var id: Self { self }
}

enum TendPreferences {
    static let appearanceKey = "tend.appearance"
    static let showWorkbenchOnLaunchKey = "tend.showWorkbenchOnLaunch"
    static let homeAnimatesMovesKey = "tend.home.animatesMoves"
    static let settingsTabKey = "tend.settings.selectedTab"
    static let fleetSettingsSectionKey = "tend.settings.fleetSection"
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

/// The Settings window's size: resizable, never smaller than the controls
/// need, and long tabs scroll inside their form.
enum TendSettingsWindowSize {
    static let minWidth: CGFloat = 720
    static let idealWidth: CGFloat = 820
    static let minHeight: CGFloat = 560
    static let idealHeight: CGFloat = 680
}

struct TendSettingsView: View {
    @ObservedObject var model: AppModel
    @AppStorage(TendPreferences.settingsTabKey) private var storedTab = TendSettingsTab.general.rawValue
    @AppStorage(TendPreferences.fleetSettingsSectionKey) private var fleetSection = TendFleetSettingsSection.hosts.rawValue

    private var selection: Binding<TendSettingsTab> {
        Binding(
            get: { TendSettingsTab.resolve(storedTab) },
            set: { storedTab = $0.rawValue }
        )
    }

    var body: some View {
        TabView(selection: selection) {
            TendGeneralSettingsView()
                .tendSettingsTab(.general, model: model)

            TendNotificationsSettingsPane(model: model, store: TendConfigStore.shared)
                .tendSettingsTab(.notifications, model: model)

            TendMenuBarSettingsView(model: model)
                .tendSettingsTab(.menuBar, model: model)

            TendAccountsSettingsPane(model: model, registry: TendModuleRegistry.shared)
                .tendSettingsTab(.accounts, model: model)

            AskProvidersSettingsPane(model: model, store: AskProviderStore.shared)
                .tendSettingsTab(.ask, model: model)

            TendAgentsSettingsPane(model: model, store: TendConfigStore.shared)
                .tendSettingsTab(.agents, model: model)

            AutomationSettingsPane(
                model: model,
                store: AutomationStore.shared,
                configStore: TendConfigStore.shared
            )
            .tendSettingsTab(.automations, model: model)

            TendFleetSettingsPane(model: model)
                .tendSettingsTab(.fleet, model: model)

            AdvancedSettingsPane(model: model, store: TendConfigStore.shared)
                .tendSettingsTab(.advanced, model: model)
        }
        .onAppear(perform: migrateStoredSelection)
        .onChange(of: storedTab) { _ in migrateStoredSelection() }
    }

    /// A deep link or an older build may have stored a tab that is now a
    /// section of another one; store the tab that exists instead.
    private func migrateStoredSelection() {
        let resolved = TendSettingsTab.resolve(storedTab)
        guard resolved.rawValue != storedTab else { return }
        if let section = TendSettingsTab.fleetSection(for: storedTab) {
            fleetSection = section.rawValue
        }
        storedTab = resolved.rawValue
    }
}

private extension View {
    /// One tab of the Settings window: its toolbar item, the window's size
    /// limits, and the shared restart bar along the bottom.
    func tendSettingsTab(_ tab: TendSettingsTab, model: AppModel) -> some View {
        safeAreaInset(edge: .bottom, spacing: 0) {
            TendRestartBar(model: model, store: TendConfigStore.shared)
        }
        .frame(
            minWidth: TendSettingsWindowSize.minWidth,
            idealWidth: TendSettingsWindowSize.idealWidth,
            maxWidth: .infinity,
            minHeight: TendSettingsWindowSize.minHeight,
            idealHeight: TendSettingsWindowSize.idealHeight,
            maxHeight: .infinity
        )
        .background(TendSettingsWindowResizing())
        .tabItem { Label(tab.title, systemImage: tab.systemImage) }
        .tag(tab)
    }
}

/// SwiftUI's Settings window has a fixed frame; this lets it resize within
/// the minimum the tabs need.
private struct TendSettingsWindowResizing: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView { ResizingView() }
    func updateNSView(_ nsView: NSView, context: Context) {}

    final class ResizingView: NSView {
        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            guard let window else { return }
            window.styleMask.insert(.resizable)
            window.contentMinSize = NSSize(
                width: TendSettingsWindowSize.minWidth,
                height: TendSettingsWindowSize.minHeight
            )
        }
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

// MARK: - Shared pieces

/// The one style for help under a control: detail size, secondary colour,
/// wrapping instead of truncating.
struct TendSettingsCaption: View {
    private let text: Text

    init(_ key: LocalizedStringKey) { text = Text(key) }
    init(verbatim: String) { text = Text(verbatim: verbatim) }

    var body: some View {
        text
            .font(TendType.detail)
            .foregroundStyle(.secondary)
            .fixedSize(horizontal: false, vertical: true)
    }
}

/// An ⓘ button whose popover holds an explanation too long for a caption.
struct TendSettingsInfoButton<Content: View>: View {
    let title: LocalizedStringKey
    var tint: Color = .secondary
    @ViewBuilder let content: Content
    @State private var isShown = false

    var body: some View {
        Button { isShown.toggle() } label: {
            Image(systemName: "info.circle")
                .foregroundStyle(tint)
        }
        .buttonStyle(.borderless)
        .help(title)
        .accessibilityLabel(Text(title))
        .popover(isPresented: $isShown, arrowEdge: .bottom) {
            VStack(alignment: .leading, spacing: 6) {
                Text(title).font(.headline)
                content
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(14)
            .frame(width: 360, alignment: .leading)
        }
    }
}

/// Save and Revert for the two editors that cannot apply as you type
/// (Fleet › Dispatch and the raw configuration file), in one place and one
/// order everywhere.
struct TendSettingsFooter<Status: View, Leading: View>: View {
    var isBusy = false
    let canRevert: Bool
    let canSave: Bool
    var revertTitle: LocalizedStringKey = "Revert"
    let revert: () -> Void
    let save: () -> Void
    @ViewBuilder let status: Status
    @ViewBuilder let leading: Leading

    var body: some View {
        VStack(spacing: 0) {
            Divider()
            HStack(spacing: 10) {
                leading
                status
                    .font(TendType.detail)
                    .fixedSize(horizontal: false, vertical: true)
                Spacer(minLength: 8)
                if isBusy {
                    ProgressView().controlSize(.small)
                }
                Button(revertTitle, action: revert)
                    .disabled(!canRevert || isBusy)
                Button("Save", action: save)
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut("s", modifiers: .command)
                    .disabled(!canSave || isBusy)
            }
            .padding(.horizontal, 20)
            .padding(.vertical, 10)
        }
        .background(.bar)
    }
}

extension TendSettingsFooter where Leading == EmptyView {
    init(
        isBusy: Bool = false,
        canRevert: Bool,
        canSave: Bool,
        revertTitle: LocalizedStringKey = "Revert",
        revert: @escaping () -> Void,
        save: @escaping () -> Void,
        @ViewBuilder status: () -> Status
    ) {
        self.init(
            isBusy: isBusy,
            canRevert: canRevert,
            canSave: canSave,
            revertTitle: revertTitle,
            revert: revert,
            save: save,
            status: status,
            leading: { EmptyView() }
        )
    }
}

/// Which tendd process is answering, as far as the app can tell.
///
/// A tendd with `daemon_boot_id_v1` names its process with a random boot id,
/// which settles the question. An older one does not, and its generation
/// only counts self-re-execs (a fresh process starts at 0 again), so it is
/// paired with the socket owner's pid. Both are paired with the number of
/// replacements this app has completed. A part that is unknown on either
/// side is not used to tell two daemons apart.
struct TendDaemonIdentity: Equatable, Sendable {
    /// The daemon's per-process boot id; nil until a hello carried one.
    var bootID: String?
    /// A hello succeeded without a boot id: an older tendd, told apart by
    /// generation and pid only.
    var lacksBootID = false
    var generation: UInt64?
    /// The socket owner's pid; looked up only for a daemon without a boot id.
    var pid: Int32?
    /// Successful "Reload tendd" replacements since the app started.
    var replacements: UInt64 = 0

    /// Whether anything names this process: a boot id, or knowing it has
    /// none (so generation and pid are the evidence).
    var isAnchored: Bool { bootID != nil || lacksBootID }

    func isSameDaemon(as other: TendDaemonIdentity) -> Bool {
        if replacements != other.replacements { return false }
        if let bootID, let other = other.bootID { return bootID == other }
        // One build names its process and the other cannot: two processes.
        if bootID != nil && other.lacksBootID || lacksBootID && other.bootID != nil { return false }
        if let generation, let other = other.generation, generation != other { return false }
        if let pid, let other = other.pid, pid != other { return false }
        return true
    }
}

/// Whether a saved configuration change still waits for tendd to restart:
/// pending from the save until a different daemon answers. Pressing Restart
/// does not clear it — only a replacement that actually happened does.
struct TendRestartNotice: Equatable, Sendable {
    private(set) var isPending = false
    /// The daemon the first unapplied change was saved under.
    private(set) var savedUnder: TendDaemonIdentity?
    /// Part of `savedUnder` was unknown at the save and was filled in from
    /// the first daemon seen afterwards. That daemon may already be the
    /// replacement, which only keeps the bar longer — never clears it early.
    private(set) var learnedAfterSave = false

    /// Whether the bar shows while `current` is the daemon answering.
    func isShown(current: TendDaemonIdentity) -> Bool {
        guard isPending else { return false }
        guard let savedUnder else { return true }
        return savedUnder.isSameDaemon(as: current)
    }

    /// Records a change that applies on the next restart.
    mutating func noteSaved(under current: TendDaemonIdentity) {
        if !isShown(current: current) {
            savedUnder = current
            learnedAfterSave = false
        }
        isPending = true
    }

    /// Fills what the save could not know from `current`, while `current` is
    /// still indistinguishable from the saved daemon. Without this, a save
    /// made before the first hello would compare unknown against every later
    /// process and never clear.
    mutating func observe(_ current: TendDaemonIdentity) {
        guard isPending, var saved = savedUnder, saved.isSameDaemon(as: current) else { return }
        var learned = false
        if !saved.isAnchored, current.isAnchored {
            saved.bootID = current.bootID
            saved.lacksBootID = current.lacksBootID
            learned = true
        }
        if saved.pid == nil, saved.lacksBootID, current.lacksBootID, let pid = current.pid {
            saved.pid = pid
            learned = true
        }
        if saved.generation == nil, let generation = current.generation {
            saved.generation = generation
            learned = true
        }
        guard learned else { return }
        savedUnder = saved
        learnedAfterSave = true
    }

    mutating func clear() {
        isPending = false
        savedUnder = nil
        learnedAfterSave = false
    }
}

extension AppModel {
    /// The local tendd as the restart bar tells daemons apart.
    var localDaemonIdentity: TendDaemonIdentity {
        TendDaemonIdentity(
            bootID: localDaemonBootID,
            lacksBootID: localDaemonLacksBootID,
            generation: localDaemonGeneration,
            pid: localDaemonPID,
            replacements: daemonReplacements
        )
    }
}

/// "Restart tendd to apply", along the bottom of every tab, while a saved
/// change waits for the daemon. Replaces the per-tab restart sentences.
struct TendRestartBar: View {
    @ObservedObject var model: AppModel
    @ObservedObject var store: TendConfigStore

    var body: some View {
        if store.restartNotice.isShown(current: model.localDaemonIdentity) {
            VStack(spacing: 0) {
                Divider()
                HStack(spacing: 10) {
                    Image(systemName: "arrow.clockwise.circle.fill")
                        .foregroundStyle(.orange)
                        .accessibilityHidden(true)
                    Text("Restart tendd to apply")
                        .font(TendType.detail.weight(.medium))
                    Spacer(minLength: 8)
                    TendDaemonReloadButton(model: model, title: "Restart")
                        .controlSize(.small)
                }
                .padding(.horizontal, 20)
                .padding(.vertical, 8)
            }
            .background(.bar)
        }
    }
}

/// The app's one daemon reload, with its confirmation. Settings › Advanced
/// shows it; the restart bar and the few panes that must reload tendd for
/// a reason of their own reuse it rather than carrying their own alert.
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

// MARK: - Menu Bar

private struct TendMenuBarSettingsView: View {
    @ObservedObject var model: AppModel
    @State private var hostIcons = TendHostIcons.all()
    @State private var customText: [String: String] = [:]
    @AppStorage(TendPreferences.menuBarAgentLimitKey) private var menuBarAgentLimit = TendPreferences.defaultMenuBarAgentLimit
    @AppStorage(TendPreferences.menuBarGroupingKey) private var menuBarGrouping = TendMenuBarGrouping.agent.rawValue
    @AppStorage(TendPreferences.menuBarAasChipKey) private var showAasChip = true

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
                TendSettingsCaption("Additional activity stays in the +N list. Reviewed agents are hidden until new activity arrives.")
                Toggle("Show AAS usage chip", isOn: $showAasChip)
                TendSettingsCaption("Hover to see active account usage. Click to compare all accounts.")
            }

            Section {
                if model.fleetHosts.isEmpty {
                    TendSettingsCaption("No fleet hosts yet.")
                } else {
                    hostIconTable
                }
            } header: {
                Text("Fleet host icons")
            } footer: {
                TendSettingsCaption("Shown when activity is grouped by host. Agents show their provider: a spark for Claude, >_ for Codex.")
            }
        }
        .formStyle(.grouped)
    }

    private var hostIconTable: some View {
        Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 8) {
            GridRow {
                Text("Host")
                Text("Symbol")
                Text("Text")
                Color.clear.frame(width: 1, height: 1)
            }
            .font(TendType.detail)
            .foregroundStyle(.secondary)
            .accessibilityHidden(true)
            Divider()
            ForEach(model.fleetHosts) { host in
                hostIconRow(host)
            }
        }
    }

    private func hostIconRow(_ host: TendFleetHost) -> some View {
        let icon = TendHostIcons.icon(for: host.alias, local: host.local)
        let isCustomText = TendGlyph.kind(icon) != .symbol(icon)
        return GridRow {
            HStack(spacing: 8) {
                TendGlyphView(value: icon, size: 11)
                    .frame(width: 22, height: 22)
                    .background(TendMenuBarAgentAppearance(identity: host.alias, symbol: icon, provider: "").gradient, in: Circle())
                    .accessibilityHidden(true)
                Text(verbatim: host.alias)
                    .lineLimit(1)
            }
            .gridColumnAlignment(.leading)
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
            .accessibilityLabel(Text("Choose a symbol"))
            TextField("Text", text: Binding(
                get: { customText[host.alias] ?? (isCustomText ? String(icon.dropFirst(TendGlyph.textPrefix.count)) : "") },
                set: { value in
                    let cleaned = TendGlyph.cleanedText(value)
                    customText[host.alias] = cleaned
                    setIcon(cleaned.isEmpty ? nil : TendGlyph.text(cleaned), for: host.alias)
                }
            ))
            .labelsHidden()
            .textFieldStyle(.roundedBorder)
            .multilineTextAlignment(.center)
            .frame(width: 48)
            .accessibilityLabel(Text("Text"))
            .help("Up to two characters or an emoji instead of a symbol")
            Group {
                if hostIcons[host.alias] != nil {
                    Button("Use Default") {
                        customText[host.alias] = ""
                        setIcon(nil, for: host.alias)
                    }
                    .controlSize(.small)
                } else {
                    Color.clear.frame(width: 1, height: 1)
                }
            }
            .gridColumnAlignment(.trailing)
        }
    }

    private func setIcon(_ icon: String?, for alias: String) {
        if let icon, TendGlyph.kind(icon) == .symbol(icon) { customText[alias] = "" }
        TendHostIcons.set(icon, for: alias)
        hostIcons = TendHostIcons.all()
    }
}

// MARK: - General

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
                TendSettingsCaption("System follows the current macOS appearance automatically.")
            }

            TendLanguageSettingsSection()

            Section("Startup") {
                Toggle("Show the Workbench when Tend launches", isOn: $showWorkbenchOnLaunch)
                TendSettingsCaption("When disabled, Tend starts in the menu bar and keeps host monitoring available.")
                Button("Show Welcome Guide…") {
                    openWindow(id: OnboardingPreferences.windowID)
                    NSApp.activate(ignoringOtherApps: true)
                }
                .buttonStyle(.link)
                .help("The first-launch tour of Work, Explore, Inbox, and Shells, with the setup checklist.")
            }

            Section("Home") {
                Toggle("Animate cards moving between lanes", isOn: $homeAnimatesMoves)
                TendSettingsCaption("A card glides to its new lane and shows where it came from for a few seconds.")
            }

            Section("Keyboard shortcut") {
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
                    TendSettingsCaption("Brings Tend forward with the agents palette, those waiting on you first.")
                }
            }

            Section("Default project folder") {
                LabeledContent("Folder") {
                    HStack(spacing: 8) {
                        Group {
                            if workDirectory.isEmpty {
                                Text("Not set")
                                    .foregroundStyle(.secondary)
                            } else {
                                Text(verbatim: workDirectory)
                                    .font(TendType.detail.monospaced())
                                    .textSelection(.enabled)
                            }
                        }
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .help(workDirectory)
                        Button("Choose…", action: chooseWorkDirectory)
                        if !workDirectory.isEmpty {
                            Button("Clear") { workDirectory = "" }
                        }
                    }
                }
                if !directoryExists {
                    Label("This folder is not currently available.", systemImage: "exclamationmark.triangle.fill")
                        .font(TendType.detail)
                        .foregroundStyle(.orange)
                } else {
                    TendSettingsCaption("Used as the initial folder in Start Work. Leave empty to use route configuration.")
                }
            }
        }
        .formStyle(.grouped)
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

// MARK: - Fleet

/// Settings › Fleet: the hosts this controller watches, and how Fleet
/// dispatch places work on them.
private struct TendFleetSettingsPane: View {
    @ObservedObject var model: AppModel
    @AppStorage(TendPreferences.fleetSettingsSectionKey) private var section = TendFleetSettingsSection.hosts.rawValue

    private var selection: Binding<TendFleetSettingsSection> {
        Binding(
            get: { TendFleetSettingsSection(rawValue: section) ?? .hosts },
            set: { section = $0.rawValue }
        )
    }

    var body: some View {
        VStack(spacing: 0) {
            Picker("Fleet settings", selection: selection) {
                Text("Hosts").tag(TendFleetSettingsSection.hosts)
                Text("Dispatch").tag(TendFleetSettingsSection.dispatch)
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .fixedSize()
            .padding(.top, 14)
            .padding(.bottom, 2)
            FleetDispatchSettingsView(model: model, section: selection.wrappedValue)
        }
    }
}

/// One host in Settings › Fleet › Hosts.
struct TendFleetSettingsRow: View {
    let host: TendFleetHost
    /// Nil when labels cannot be edited from here.
    var onEditLabels: (() -> Void)?

    private var labelText: String {
        (host.labels ?? [:]).sorted { $0.key < $1.key }.map { "\($0.key)=\($0.value)" }.joined(separator: ", ")
    }

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: host.local ? "desktopcomputer" : "server.rack")
                .foregroundStyle(.tint)
                .frame(width: 28, height: 28)
                .background(Color.accentColor.opacity(0.1), in: RoundedRectangle(cornerRadius: TendTheme.panelRadius))
                .accessibilityHidden(true)

            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 7) {
                    // The dot's colour is backed by the state in words, so
                    // the state never rests on colour alone.
                    Circle().fill(fleetHostColor(host.state)).frame(width: 7, height: 7)
                        .accessibilityHidden(true)
                    Text(host.alias).font(.headline)
                        .lineLimit(1)
                        .layoutPriority(1)
                    Group {
                        Text(verbatim: fleetHostStateLabel(host.state))
                        if host.local {
                            Text("Local")
                        } else {
                            Text(fleetHostModeLabel(host.mode))
                        }
                    }
                    .font(TendType.meta.weight(.semibold))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .fixedSize(horizontal: true, vertical: false)
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
                Group {
                    if labelText.isEmpty {
                        Text("No labels")
                    } else {
                        Text(verbatim: labelText).textSelection(.enabled)
                    }
                }
                .font(TendType.detail)
                .foregroundStyle(.secondary)
                .lineLimit(2)
            }

            Spacer(minLength: 8)

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
                    .font(TendType.detail)
                    .foregroundStyle(.secondary)
                if let onEditLabels {
                    Button("Edit Labels…", action: onEditLabels)
                        .controlSize(.small)
                }
            }
            .lineLimit(1)
            .fixedSize(horizontal: true, vertical: false)
        }
        .padding(.vertical, 2)
    }
}
