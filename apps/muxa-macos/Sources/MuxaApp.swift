import GhosttyTerminal
import SwiftUI

@MainActor
private final class MuxaApplicationDelegate: NSObject, NSApplicationDelegate {
    override init() {
        // Muxa has a persistent menu-bar scene, so AppKit can otherwise treat
        // a previously closed/off-screen workbench as the desired launch
        // state and create no visible window at all.
        MuxaPreferences.registerDefaults()
        UserDefaults.standard.set(true, forKey: "ApplePersistenceIgnoreState")
        // `MUXA_TERMINAL_DEBUG=1 open -a Muxa` (or launching the binary
        // directly) prints libghostty's lifecycle, metrics, and IO tracing to
        // stdout, which is how a terminal that stops following the window
        // size gets diagnosed.
        if ProcessInfo.processInfo.environment["MUXA_TERMINAL_DEBUG"] == "1" {
            TerminalDebugLog.enable(.all)
        }
        // Remember which language override this process started with so the
        // Settings pane can ask for a relaunch only when it actually changed.
        _ = MuxaLanguagePreference.atLaunch
        super.init()
    }

    /// Muxa keeps a menu-bar scene, a daemon connection, and host monitoring
    /// alive without a window, and the workbench is reopened from the menu
    /// bar or the Dock. Closing the last window must not quit the app.
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        // Look for the enabled modules' tools once, here. Settings › Modules
        // used to be the only thing that probed, so until it had been opened
        // every action needing a tool sat disabled saying it was still
        // looking — for a module the operator had already pointed at a
        // working install. A module that is switched off is not probed, so
        // an operator who uses none still pays nothing.
        Task { await MuxaModuleRegistry.shared.probeEnabled() }
        if UserDefaults.standard.bool(forKey: MuxaPreferences.showWorkbenchOnLaunchKey) {
            presentWorkbench(remainingAttempts: 50)
        }
    }

    /// A module may be holding a process of its own — AIR Workbench serves a
    /// loopback page until it is stopped — and quitting must not leave it
    /// running.
    func applicationWillTerminate(_ notification: Notification) {
        MuxaModuleRegistry.shared.shutdown()
    }

    func applicationShouldHandleReopen(
        _ sender: NSApplication,
        hasVisibleWindows flag: Bool
    ) -> Bool {
        presentWorkbench(remainingAttempts: 3)
        return true
    }

    private func presentWorkbench(remainingAttempts: Int) {
        let workbench = NSApp.windows.first(where: {
            $0.identifier?.rawValue == "muxa.main-workbench"
        }) ?? NSApp.windows
            .filter { $0.level == .normal && $0.canBecomeKey && $0.frame.width >= 400 }
            .max { $0.frame.width * $0.frame.height < $1.frame.width * $1.frame.height }
        if let window = workbench {
            window.makeKeyAndOrderFront(nil)
            NSApp.activate(ignoringOtherApps: true)
            return
        }
        guard remainingAttempts > 0 else { return }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.1) { [weak self] in
            self?.presentWorkbench(remainingAttempts: remainingAttempts - 1)
        }
    }
}

/// Ghostty's default keybindings are those of a standalone terminal app —
/// ⌘W close_surface, ⌃Tab next_tab, ⌘1–9 goto_tab, ⌘, open_config, and so
/// on — and a focused terminal claims them in `performKeyEquivalent`, before
/// the menu bar sees the key. Inside the workbench those shortcuts belong to
/// Muxa's editor tabs and windows, so the defaults are cleared and only the
/// bindings that act on the terminal's own contents are kept.
enum MuxaTerminalKeybindings {
    static let bindings = [
        "super+c=copy_to_clipboard",
        "super+v=paste_from_clipboard",
        "super+shift+v=paste_from_selection",
        "super+a=select_all",
        "super+k=clear_screen",
        "super+equal=increase_font_size:1",
        "super+plus=increase_font_size:1",
        "super+minus=decrease_font_size:1",
        "super+zero=reset_font_size",
        "super+up=jump_to_prompt:-1",
        "super+down=jump_to_prompt:1",
        "super+home=scroll_to_top",
        "super+end=scroll_to_bottom",
        "super+page_up=scroll_page_up",
        "super+page_down=scroll_page_down",
    ]

    /// Every terminal pane's configuration (each pane owns its controller).
    static let configuration: TerminalConfiguration = bindings.reduce(
        TerminalConfiguration().appending(.custom(key: "keybind", value: "clear"))
    ) { configuration, binding in
        configuration.appending(.custom(key: "keybind", value: binding))
    }

    /// libghostty rejects the whole configuration over one bad line, so a
    /// test loads it once and expects no issue.
    @MainActor
    static func loadIssue() -> String? {
        TerminalController(configuration: configuration).lastConfigurationIssue
    }
}

@main
struct MuxaApp: App {
    @NSApplicationDelegateAdaptor(MuxaApplicationDelegate.self) private var appDelegate
    @StateObject private var model = AppModel()
    @AppStorage(MuxaPreferences.appearanceKey) private var appearance = MuxaAppearance.system.rawValue

    var body: some Scene {
        workbenchWindow

        WindowGroup("Muxa Module", for: MuxaModuleRoute.self) { $route in
            if let route {
                DetachedModuleView(route: route, model: model)
                    .environmentObject(model)
                    .preferredColorScheme(preferredColorScheme)
                    .frame(minWidth: 720, minHeight: 520)
            }
        }
        .defaultSize(width: 980, height: 720)

        Settings {
            MuxaSettingsView(model: model)
                .preferredColorScheme(preferredColorScheme)
        }

        // First-launch Welcome guide; reopened from Help › Welcome Guide…
        // (see OnboardingView.swift for the launch decision).
        Window("Welcome to Muxa", id: OnboardingPreferences.windowID) {
            OnboardingView(model: model)
                .environmentObject(model)
                .preferredColorScheme(preferredColorScheme)
        }
        .defaultSize(width: 720, height: 560)
        .defaultPosition(.center)
        .windowResizability(.contentSize)

        MenuBarExtra("Muxa", systemImage: menuBarIcon) {
            MenuBarContent(model: model)
                .preferredColorScheme(preferredColorScheme)
        }
        .menuBarExtraStyle(.window)
    }

    private var workbenchWindow: some Scene {
        configuredWorkbenchWindow
    }

    private var configuredWorkbenchWindow: some Scene {
        // A WindowGroup recreates the workbench when a menu-bar-only launch or
        // saved closed-window state would otherwise leave no visible window.
        // Detached modules continue to use their typed WindowGroup below.
        WindowGroup("Muxa", id: "main") {
            ContentView()
                .environmentObject(model)
                .preferredColorScheme(preferredColorScheme)
                .presentsOnboardingOnLaunch()
        }
        .defaultSize(width: 1120, height: 760)
        // No title bar of its own: like Safari's compact tabs, the side bar's
        // title row and the editor tab strip double as the title bar (see
        // WorkbenchWindowChrome.swift).
        .windowStyle(.hiddenTitleBar)
        .commands {
            CommandGroup(replacing: .printItem) {}
            MuxaEditorMenuCommands()
            OnboardingMenuCommands()
            CommandGroup(after: .newItem) {
                Button("Start Muxa Work…") { model.presentWorkStart() }
                    .keyboardShortcut("n", modifiers: [.command, .option])
                    .disabled(!model.isConnected || model.isStartingWork)
                Button("Open Live Watch") { model.select(.watch) }
                    .keyboardShortcut("w", modifiers: [.command, .shift])
                // ⌘T opens a terminal tab in every terminal and agent
                // workbench (Terminal, Ghostty, iTerm, Orca).
                Button("New Muxa Shell") { model.createShell() }
                    .keyboardShortcut("t", modifiers: .command)
                    .disabled(!model.isConnected || model.isCreatingSession)
            }
        }
    }

    private var menuBarIcon: String {
        switch model.connectionState {
        case .failed, .upgradeRequired: "terminal.fill"
        case .connecting, .connected: "terminal"
        }
    }

    private var preferredColorScheme: ColorScheme? {
        MuxaAppearance(rawValue: appearance)?.colorScheme
    }
}

private struct MuxaEditorMenuCommands: Commands {
    @FocusedValue(\.muxaEditorCommands) private var focusedActions

    private var actions: MuxaEditorCommandActions? {
        guard let focusedActions, focusedActions.isEnabled else { return nil }
        return focusedActions
    }

    private var dispatchActions: MuxaEditorCommandActions? {
        guard NSApp.modalWindow == nil,
              let window = NSApp.keyWindow,
              window.attachedSheet == nil,
              window.sheetParent == nil else { return nil }
        return actions
    }

    /// ⌘W closes the focused editor tab, like a code editor, and the window
    /// only once no tab is left — or when the key window is not the
    /// workbench (Settings, a detached module) at all. It replaces File ›
    /// Close, which sits ahead of the Editor menu and would otherwise take
    /// ⌘W and close the whole workbench.
    private func closeFrontmost() {
        if let close = dispatchActions?.close {
            close()
        } else {
            NSApp.keyWindow?.performClose(nil)
        }
    }

    var body: some Commands {
        CommandGroup(replacing: .saveItem) {
            Button(actions?.close == nil ? "Close Window" : "Close Editor", action: closeFrontmost)
                .keyboardShortcut("w", modifiers: .command)
        }
        CommandMenu("Editor") {
            Button("Close Editor") { dispatchActions?.close?() }
                .disabled(actions?.close == nil)
            Divider()
            Button("Previous Editor") { dispatchActions?.previous?() }
                .keyboardShortcut(.tab, modifiers: [.control, .shift])
                .disabled(actions?.previous == nil)
            Button("Next Editor") { dispatchActions?.next?() }
                .keyboardShortcut(.tab, modifiers: .control)
                .disabled(actions?.next == nil)
            ForEach(1...8, id: \.self) { number in
                Button("Editor \(number)") { dispatchActions?.activateAt?(number - 1) }
                    .keyboardShortcut(KeyEquivalent(Character(String(number))), modifiers: .command)
                    .disabled(actions?.activateAt == nil)
            }
            Button("Last Editor") { dispatchActions?.activateLast?() }
                .keyboardShortcut("9", modifiers: .command)
                .disabled(actions?.activateLast == nil)
            Divider()
            Button("Focus Previous Editor Group") { dispatchActions?.focusRelativeGroup?(-1) }
                .keyboardShortcut(.leftArrow, modifiers: [.command, .option])
                .disabled(actions?.focusRelativeGroup == nil)
            Button("Focus Next Editor Group") { dispatchActions?.focusRelativeGroup?(1) }
                .keyboardShortcut(.rightArrow, modifiers: [.command, .option])
                .disabled(actions?.focusRelativeGroup == nil)
            Divider()
            Button("Keep Editor Open") { dispatchActions?.pin?() }
                .keyboardShortcut(.return, modifiers: [.command, .option])
                .disabled(actions?.pin == nil)
            Button("Split Editor Right") { dispatchActions?.splitRight?() }
                .keyboardShortcut("\\", modifiers: .command)
                .disabled(actions?.splitRight == nil)
            Divider()
            Button("Reopen Closed Editor") { dispatchActions?.reopenClosed?() }
                .keyboardShortcut("t", modifiers: [.command, .shift])
                .disabled(actions?.reopenClosed == nil)
        }
        CommandGroup(replacing: .sidebar) {
            Button("Toggle Side Bar") { dispatchActions?.toggleSidebar?() }
                .keyboardShortcut("b", modifiers: .command)
                .disabled(actions?.toggleSidebar == nil)
        }
        CommandGroup(before: .help) {
            Button("Keyboard Shortcuts") { dispatchActions?.showShortcuts?() }
                .keyboardShortcut("/", modifiers: .command)
                .disabled(actions?.showShortcuts == nil)
        }
        CommandMenu("Navigate") {
            Button("Quick Open…") { dispatchActions?.quickOpen?() }
                .keyboardShortcut("p", modifiers: .command)
                .disabled(actions?.quickOpen == nil)
            Button("Command Palette…") { dispatchActions?.commandPalette?() }
                .keyboardShortcut("p", modifiers: [.command, .shift])
                .disabled(actions?.commandPalette == nil)
            Divider()
            Button("Jump to Agent…") { dispatchActions?.jumpToAgent?() }
                .keyboardShortcut("j", modifiers: .command)
                .disabled(actions?.jumpToAgent == nil)
            Button("Next Agent Needing Attention") { dispatchActions?.nextAttention?() }
                .keyboardShortcut("j", modifiers: [.command, .shift])
                .disabled(actions?.nextAttention == nil)
            Divider()
            Button("Open Work Command Center") { dispatchActions?.openWorkCommandCenter?() }
                .keyboardShortcut("1", modifiers: [.command, .shift])
                .disabled(actions?.openWorkCommandCenter == nil)
            Button("Open Ask") { dispatchActions?.openAsk?() }
                .keyboardShortcut("2", modifiers: [.command, .shift])
                .disabled(actions?.openAsk == nil)
            Button("Open Inbox") { dispatchActions?.openInbox?() }
                .keyboardShortcut("3", modifiers: [.command, .shift])
                .disabled(actions?.openInbox == nil)
            Divider()
            Menu("Sidebar") {
                ForEach(MuxaSidebarMode.allCases) { mode in
                    Button(mode.title) { dispatchActions?.selectSidebar?(mode) }
                        .disabled(actions?.selectSidebar == nil)
                }
            }
            .disabled(actions?.selectSidebar == nil)
            Button("Focus Sidebar Filter") { dispatchActions?.focusSidebar?() }
                .keyboardShortcut("f", modifiers: [.command, .shift])
                .disabled(actions?.focusSidebar == nil)
        }
    }
}

private struct MenuBarContent: View {
    @ObservedObject var model: AppModel
    @Environment(\.openWindow) private var openWindow

    private var liveSessionCount: Int {
        model.sessions.lazy.filter { !$0.exited }.count
    }

    private var liveAgentCount: Int {
        model.agents.lazy.filter { $0.state != "stopped" }.count
    }

    private var attentionCount: Int {
        model.agents.lazy.filter {
            $0.state == "waiting_input" || $0.state == "waiting_choice" || $0.state == "error"
        }.count
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Muxa")
                .font(.headline)
            Text("\(liveAgentCount) agents · \(model.pipelineRuns.count) work items")
                .foregroundStyle(.secondary)
            if attentionCount > 0 {
                Label("\(attentionCount) need attention", systemImage: "exclamationmark.circle.fill")
                    .foregroundStyle(.orange)
            }
            Text("\(liveSessionCount) native shells")
                .font(.caption)
                .foregroundStyle(.tertiary)
            Divider()
            Button("Start Work…") {
                model.presentWorkStart()
                openWindow(id: "main")
                NSApp.activate(ignoringOtherApps: true)
            }
            .disabled(!model.isConnected || model.isStartingWork)
            Button("Open Live Watch") {
                model.select(.watch)
                openWindow(id: "main")
                NSApp.activate(ignoringOtherApps: true)
            }
            Button("New Shell") { model.createShell() }
                .disabled(!model.isConnected || model.isCreatingSession)
            Button("Open Muxa") {
                openWindow(id: "main")
                NSApp.activate(ignoringOtherApps: true)
            }
            Divider()
            if #available(macOS 14.0, *) {
                SettingsLink {
                    Label("Settings…", systemImage: "gearshape")
                }
            } else {
                Button {
                    let opened = NSApp.sendAction(
                        Selector(("showSettingsWindow:")),
                        to: nil,
                        from: nil
                    )
                    if !opened {
                        NSApp.sendAction(
                            Selector(("showPreferencesWindow:")),
                            to: nil,
                            from: nil
                        )
                    }
                } label: {
                    Label("Settings…", systemImage: "gearshape")
                }
            }
            Button("Quit") { NSApp.terminate(nil) }
        }
        .padding(12)
        .frame(width: 240)
    }
}
