// A shortcut that works from any app: it brings Muxa forward with the agents
// palette, those waiting on the person first. Carbon's hot keys need no
// Accessibility permission.
import AppKit
import Carbon.HIToolbox

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
    /// Posted when the shortcut asks the workbench for the agents palette.
    static let showAgents = Notification.Name("MuxaGlobalHotKeyShowAgents")

    /// Why the shortcut is not active, for Settings; nil when it is, or off.
    @Published private(set) var problem: String?
    /// Set when the shortcut fired before a workbench was on screen to take
    /// it; the workbench takes it when it appears.
    var pendingShowAgents = false

    private var hotKey: EventHotKeyRef?
    private var handler: EventHandlerRef?

    /// Registers the chosen shortcut, replacing any earlier one.
    func apply(_ preset: MuxaHotKeyPreset = .current()) {
        if let hotKey { UnregisterEventHotKey(hotKey) }
        hotKey = nil
        problem = nil
        guard let modifiers = preset.carbonModifiers else { return }
        installHandler()
        var ref: EventHotKeyRef?
        let id = EventHotKeyID(signature: OSType(0x4D55_5841), id: 1) // "MUXA"
        let status = RegisterEventHotKey(UInt32(kVK_Space), modifiers, id, GetApplicationEventTarget(), 0, &ref)
        if status == noErr {
            hotKey = ref
        } else {
            problem = String(localized: "\(preset.title) is taken by another app or by macOS. Pick another shortcut.")
        }
    }

    private func installHandler() {
        guard handler == nil else { return }
        var type = EventTypeSpec(eventClass: OSType(kEventClassKeyboard), eventKind: UInt32(kEventHotKeyPressed))
        InstallEventHandler(GetApplicationEventTarget(), { _, _, _ in
            DispatchQueue.main.async { MuxaGlobalHotKey.shared.fire() }
            return noErr
        }, 1, &type, nil, &handler)
    }

    private func fire() {
        pendingShowAgents = true
        // Restores, reopens or brings forward the workbench, as a
        // notification click does.
        MuxaWorkbenchPresenter.present()
        // A workbench already on screen takes it now; one that is opening
        // takes `pendingShowAgents` when it appears.
        NotificationCenter.default.post(name: Self.showAgents, object: nil)
    }

    /// The workbench's side: whether it should open the agents palette.
    func takePendingShowAgents() -> Bool {
        defer { pendingShowAgents = false }
        return pendingShowAgents
    }
}
