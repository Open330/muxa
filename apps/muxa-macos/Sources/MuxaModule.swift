import SwiftUI

/// An optional integration with a tool that is not muxa.
///
/// Modules live in the app, never in muxad or the `muxa` CLI: they wrap
/// something the operator already has installed (an account switcher, a
/// workflow editor) and surface it where the app can act on it. A module
/// that is not installed is not an error — it is simply not available, and
/// the app says what would make it available.
///
/// Everything a module contributes is optional. The app works with none
/// enabled, and enabling one must never be required to use muxa.
@MainActor
protocol MuxaModule: AnyObject, Identifiable {
    /// Stable identity: the id keys preferences, so it never changes.
    /// `nonisolated` so the id can be read from anywhere, including a
    /// `ForEach` that has not hopped to the main actor yet.
    nonisolated static var identity: MuxaModuleIdentity { get }

    /// What the last probe found. `probing` until `probe()` has run once.
    var availability: MuxaModuleAvailability { get }

    /// Look for the tool and record what was found. Cheap enough to call on
    /// every appearance of the Modules pane; a module that needs more should
    /// cache it itself.
    func probe() async

    /// The module's own settings, shown under Settings › Modules.
    func settingsPane(model: AppModel) -> AnyView

    /// What this module offers to do with a given object. Called only while
    /// the module is enabled and available.
    func actions(for context: MuxaModuleContext, model: AppModel) -> [MuxaModuleAction]
}

extension MuxaModule {
    nonisolated var id: String { Self.identity.id }
    var identity: MuxaModuleIdentity { Self.identity }

    func actions(for context: MuxaModuleContext, model: AppModel) -> [MuxaModuleAction] {
        _ = (context, model)
        return []
    }
}

/// Who a module is, in the words the Modules pane shows.
struct MuxaModuleIdentity: Sendable, Identifiable, Equatable {
    /// Preference key and action namespace; never changes.
    let id: String
    let title: String
    /// One line: what having this module gets the operator.
    let blurb: String
    let symbolName: String
    /// The command the module needs, shown when it is missing.
    let executable: String?
    /// Where to get it.
    let homepage: URL?

    init(
        id: String,
        title: String,
        blurb: String,
        symbolName: String,
        executable: String? = nil,
        homepage: URL? = nil
    ) {
        self.id = id
        self.title = title
        self.blurb = blurb
        self.symbolName = symbolName
        self.executable = executable
        self.homepage = homepage
    }
}

/// Whether a module can do anything right now.
enum MuxaModuleAvailability: Sendable, Equatable {
    case probing
    /// Found, with what was found (a version, a path) for the operator to
    /// confirm they are pointed at the right install.
    case available(version: String?, detail: String?)
    /// Not installed. `hint` says what to do about it, in one line.
    case missing(hint: String)
    /// Installed but unusable — a version that is too old, a broken config.
    case unusable(reason: String)

    var isAvailable: Bool {
        if case .available = self { return true }
        return false
    }

    /// Short status for a list row.
    var summary: String {
        switch self {
        case .probing:
            String(localized: "Checking…")
        case .available(let version, let detail):
            [version, detail].compactMap { $0 }.joined(separator: " · ")
        case .missing(let hint):
            hint
        case .unusable(let reason):
            reason
        }
    }
}

/// What a module is being asked to act on.
enum MuxaModuleContext: Sendable {
    /// A pipeline in the library, and the host it belongs to.
    case pipeline(MuxaWorkOptions.Pipeline, host: String?)
    /// A managed Work, running or finished.
    case work(MuxaWorkGroup)
    /// One agent pane.
    case agent(MuxaHostedAgent)
    /// No object: the module's own top-level actions.
    case app
}

/// One thing a module offers to do. The title is what a menu item reads.
struct MuxaModuleAction: Identifiable {
    let id: String
    let title: LocalizedStringKey
    let symbolName: String
    /// Why it cannot run right now; nil when it can.
    let disabledReason: String?
    let perform: @MainActor () async -> Void

    init(
        id: String,
        title: LocalizedStringKey,
        symbolName: String,
        disabledReason: String? = nil,
        perform: @escaping @MainActor () async -> Void
    ) {
        self.id = id
        self.title = title
        self.symbolName = symbolName
        self.disabledReason = disabledReason
        self.perform = perform
    }

    var isEnabled: Bool { disabledReason == nil }
}
