// A shortcut that works from any app: it brings Muxa forward with the agents
// palette, those waiting on the person first. Carbon's hot keys need no
// Accessibility permission.
import AppKit
import Carbon.HIToolbox
import SwiftUI

enum MuxaHotKeyPreset: String, CaseIterable, Identifiable, Sendable {
    case optionSpace
    case controlOptionSpace
    case commandShiftSpace
    case off

    static let defaultsKey = "muxa.hotkey.agents"

    var id: String { rawValue }

    var title: String {
        switch self {
        case .optionSpace: "⌥ Space"
        case .controlOptionSpace: "⌃⌥ Space"
        case .commandShiftSpace: "⇧⌘ Space"
        case .off: String(localized: "Off")
        }
    }

    /// Carbon modifier flags for the preset; nil when off.
    var carbonModifiers: UInt32? {
        switch self {
        case .optionSpace: UInt32(optionKey)
        case .controlOptionSpace: UInt32(controlKey | optionKey)
        case .commandShiftSpace: UInt32(cmdKey | shiftKey)
        case .off: nil
        }
    }

    static func current(_ defaults: UserDefaults = .standard) -> MuxaHotKeyPreset {
        defaults.string(forKey: defaultsKey).flatMap(MuxaHotKeyPreset.init(rawValue:)) ?? .optionSpace
    }
}

@MainActor
final class MuxaGlobalHotKey: ObservableObject {
    static let shared = MuxaGlobalHotKey()
    /// Posted when the shortcut asks a workbench for the agents palette.
    static let showAgents = Notification.Name("MuxaGlobalHotKeyShowAgents")
    /// How long a press waits for a workbench that can take it.
    static let intentLifetime: TimeInterval = 30
    private static let hotKeyID = EventHotKeyID(signature: OSType(0x4D55_5841), id: 1) // "MUXA"

    /// Why the shortcut is not active, for Settings; nil when it is, or off.
    @Published private(set) var problem: String?
    /// When the shortcut last asked for the agents palette and no workbench
    /// has taken it yet.
    private(set) var pendingSince: Date?

    private var hotKey: EventHotKeyRef?
    private var handler: EventHandlerRef?

    /// Registers the chosen shortcut, replacing any earlier one.
    func apply(_ preset: MuxaHotKeyPreset = .current()) {
        if let hotKey { UnregisterEventHotKey(hotKey) }
        hotKey = nil
        problem = nil
        guard let modifiers = preset.carbonModifiers else { return }
        guard installHandler() else {
            problem = String(localized: "Muxa could not listen for the shortcut. Restart Muxa and try again.")
            return
        }
        var ref: EventHotKeyRef?
        let status = RegisterEventHotKey(UInt32(kVK_Space), modifiers, Self.hotKeyID, GetApplicationEventTarget(), 0, &ref)
        if status == noErr {
            hotKey = ref
        } else if status == OSStatus(eventHotKeyExistsErr) {
            problem = String(localized: "\(preset.title) is taken by another app or by macOS. Pick another shortcut.")
        } else {
            problem = String(localized: "macOS refused \(preset.title) (error \(Int(status))). Pick another shortcut.")
        }
    }

    private func installHandler() -> Bool {
        if handler != nil { return true }
        var type = EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed))
        let status = InstallEventHandler(GetApplicationEventTarget(), { _, event, _ in
            // Only Muxa's own hot key; let any other pass.
            var pressed = EventHotKeyID()
            let read = GetEventParameter(
                event, EventParamName(kEventParamDirectObject), EventParamType(typeEventHotKeyID),
                nil, MemoryLayout<EventHotKeyID>.size, nil, &pressed
            )
            guard read == noErr, pressed.signature == MuxaGlobalHotKey.hotKeyID.signature,
                  pressed.id == MuxaGlobalHotKey.hotKeyID.id else { return OSStatus(eventNotHandledErr) }
            DispatchQueue.main.async { MuxaGlobalHotKey.shared.fire() }
            return noErr
        }, 1, &type, nil, &handler)
        return status == noErr
    }

    private func fire() {
        pendingSince = .now
        // Restores, reopens or brings forward the workbench, as a
        // notification click does; the key workbench takes the request.
        MuxaWorkbenchPresenter.present()
        NotificationCenter.default.post(name: Self.showAgents, object: nil)
    }

    /// Takes the request for one workbench, once; false when there is none
    /// or it has expired.
    func takePending(now: Date = .now) -> Bool {
        guard let since = pendingSince else { return false }
        pendingSince = nil
        return now.timeIntervalSince(since) <= Self.intentLifetime
    }

    var hasPending: Bool { pendingSince != nil }
}

/// The workbench's side of the shortcut: the key workbench window takes the
/// request, once, when it can show the palette. With another sheet open it
/// waits for that sheet to close; an open palette switches to agents.
struct MuxaHotKeyReceiver: NSViewRepresentable {
    let paletteOpen: Bool
    let showAgents: () -> Void

    func makeNSView(context: Context) -> Probe { Probe() }

    func updateNSView(_ probe: Probe, context: Context) {
        probe.paletteOpen = paletteOpen
        probe.showAgents = showAgents
        probe.attempt()
    }

    final class Probe: NSView {
        var paletteOpen = false
        var showAgents: (() -> Void)?
        private var observers: [NSObjectProtocol] = []

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            observers.forEach(NotificationCenter.default.removeObserver)
            observers = []
            guard let window else { return }
            let center = NotificationCenter.default
            for (name, object) in [
                (NSWindow.didBecomeKeyNotification, window as AnyObject?),
                (NSWindow.didEndSheetNotification, window as AnyObject?),
                (MuxaGlobalHotKey.showAgents, nil),
            ] {
                observers.append(center.addObserver(forName: name, object: object, queue: .main) { [weak self] _ in
                    MainActor.assumeIsolated {
                        // Let SwiftUI settle a closing sheet first.
                        DispatchQueue.main.async { self?.attempt() }
                    }
                })
            }
            attempt()
        }

        func attempt() {
            guard let window, window.isKeyWindow, MuxaGlobalHotKey.shared.hasPending else { return }
            guard paletteOpen || window.attachedSheet == nil else { return }
            if MuxaGlobalHotKey.shared.takePending() { showAgents?() }
        }
    }
}
