import SwiftUI

@MainActor
private final class MuxaApplicationDelegate: NSObject, NSApplicationDelegate {
    override init() {
        // Muxa has a persistent menu-bar scene, so AppKit can otherwise treat
        // a previously closed/off-screen workbench as the desired launch
        // state and create no visible window at all.
        MuxaPreferences.registerDefaults()
        UserDefaults.standard.set(true, forKey: "ApplePersistenceIgnoreState")
        super.init()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        if UserDefaults.standard.bool(forKey: MuxaPreferences.showWorkbenchOnLaunchKey) {
            presentWorkbench(remainingAttempts: 50)
        }
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
        }
        .defaultSize(width: 1120, height: 760)
        .commands {
            MuxaEditorMenuCommands()
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
    @FocusedValue(\.muxaEditorCommands) private var actions

    var body: some Commands {
        CommandMenu("Editor") {
            Button("Close Editor") { actions?.close() }
                .keyboardShortcut("w", modifiers: .command)
                .disabled(actions == nil)
            Divider()
            Button("Previous Editor") { actions?.previous() }
                .keyboardShortcut(.tab, modifiers: [.control, .shift])
                .disabled(actions == nil)
            Button("Next Editor") { actions?.next() }
                .keyboardShortcut(.tab, modifiers: .control)
                .disabled(actions == nil)
            Divider()
            Button("Keep Editor Open") { actions?.pin() }
                .disabled(actions == nil)
            Button("Split Editor Right") { actions?.splitRight() }
                .keyboardShortcut("\\", modifiers: .command)
                .disabled(actions == nil)
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
            Text("\(liveAgentCount) agent\(liveAgentCount == 1 ? "" : "s") · \(model.pipelineRuns.count) work item\(model.pipelineRuns.count == 1 ? "" : "s")")
                .foregroundStyle(.secondary)
            if attentionCount > 0 {
                Label("\(attentionCount) need attention", systemImage: "exclamationmark.circle.fill")
                    .foregroundStyle(.orange)
            }
            Text("\(liveSessionCount) native shell\(liveSessionCount == 1 ? "" : "s")")
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
