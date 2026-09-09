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
                Button("New Muxa Shell") { model.createShell() }
                    .keyboardShortcut("n", modifiers: [.command, .shift])
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

    var body: some Commands {
        CommandMenu("Editor") {
            Button("Close Editor") { dispatchActions?.close?() }
                .keyboardShortcut("w", modifiers: .command)
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
        }
        CommandMenu("Navigate") {
            Button("Quick Open…") { dispatchActions?.quickOpen?() }
                .keyboardShortcut("p", modifiers: .command)
                .disabled(actions?.quickOpen == nil)
            Button("Command Palette…") { dispatchActions?.commandPalette?() }
                .keyboardShortcut("p", modifiers: [.command, .shift])
                .disabled(actions?.commandPalette == nil)
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
