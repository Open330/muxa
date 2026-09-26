import AppKit
import SwiftUI

/// Window-owned command routing survives first-responder changes into AppKit
/// terminals, text views and WebKit. SwiftUI FocusedValues can be absent there;
/// absence must never turn Close Tab into Close Window.
@MainActor
struct MuxaWorkbenchCommandBridge: NSViewRepresentable {
    let actions: MuxaEditorCommandActions
    private static let windows = NSMapTable<NSWindow, CommandView>.weakToWeakObjects()
    static func actions(for window: NSWindow?) -> MuxaEditorCommandActions? {
        guard let window else { return nil }
        return windows.object(forKey: window)?.actions
    }
    func makeNSView(context: Context) -> CommandView { CommandView() }
    func updateNSView(_ view: CommandView, context: Context) { view.actions = actions }
    static func dismantleNSView(_ view: CommandView, coordinator: ()) { view.removeEventMonitor() }

    final class CommandView: NSView {
        override func hitTest(_ point: NSPoint) -> NSView? { nil }
        var actions = MuxaEditorCommandActions()
        // Mutated on the main actor; deinit only removes the final event token.
        nonisolated(unsafe) private var monitor: Any?
        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            removeEventMonitor()
            guard let window else { return }
            MuxaWorkbenchCommandBridge.windows.setObject(self, forKey: window)
            monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
                guard let self, let window = self.window, event.window === window,
                      NSApp.keyWindow === window, NSApp.modalWindow == nil,
                      window.attachedSheet == nil, self.actions.isEnabled else { return event }
                return MuxaWorkbenchKeymap.handle(event, actions: self.actions) ? nil : event
            }
        }
        func removeEventMonitor() {
            if let monitor { NSEvent.removeMonitor(monitor); self.monitor = nil }
        }
    }
}

@MainActor
enum MuxaWorkbenchKeymap {
    /// Use hardware positions for command equivalents as AppKit does for
    /// non-Latin input sources. Do not intercept unmodified terminal input.
    static func handle(_ event: NSEvent, actions: MuxaEditorCommandActions) -> Bool {
        guard actions.isEnabled else { return false }
        let flags = event.modifierFlags.intersection([.command, .shift, .option, .control])
        switch (event.keyCode, flags) {
        case (13, .command): // W — empty workbench stays open, like an editor.
            actions.close?(); return true
        case (17, [.command, .shift]): return invoke(actions.reopenClosed)
        case (48, [.control, .shift]): return invoke(actions.previous)
        case (48, .control): return invoke(actions.next)
        case (35, .command): return invoke(actions.quickOpen)
        case (35, [.command, .shift]): return invoke(actions.commandPalette)
        case (3, .command): return invoke(actions.findInFile)
        case (31, .command): return invoke(actions.openFile)
        case (14, [.command, .shift]): return invoke(actions.showFiles)
        case (11, .command): return invoke(actions.toggleSidebar)
        case (42, .command): return invoke(actions.splitRight)
        default: return false
        }
    }
    private static func invoke(_ action: (() -> Void)?) -> Bool {
        guard let action else { return false }
        action()
        return true
    }
}

/// Clicking an embedded native view must focus its editor group too. The
/// parent's SwiftUI FocusState does not follow WebKit/NSTextView responders.
@MainActor
struct MuxaEditorGroupFocusBridge: NSViewRepresentable {
    let focus: () -> Void
    func makeNSView(context: Context) -> FocusView { FocusView() }
    func updateNSView(_ view: FocusView, context: Context) { view.focus = focus }
    final class FocusView: NSView {
        override func hitTest(_ point: NSPoint) -> NSView? { nil }
        var focus: (() -> Void)?
        nonisolated(unsafe) private var monitor: Any?
        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            if let monitor { NSEvent.removeMonitor(monitor); self.monitor = nil }
            guard window != nil else { return }
            monitor = NSEvent.addLocalMonitorForEvents(matching: [.leftMouseDown, .rightMouseDown]) { [weak self] event in
                guard let self, let window = self.window, event.window === window,
                      window.attachedSheet == nil, NSApp.modalWindow == nil,
                      self.bounds.contains(self.convert(event.locationInWindow, from: nil)) else { return event }
                self.focus?()
                return event
            }
        }
        deinit { if let monitor { NSEvent.removeMonitor(monitor) } }
    }
}
