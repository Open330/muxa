import SwiftUI

/// Placeholder replaced by the aas module implementation.
@MainActor
final class AasModule: MuxaModule {
    nonisolated static let identity = MuxaModuleIdentity(
        id: "aas",
        title: "aas — Agent Account Switcher",
        blurb: "Placeholder.",
        symbolName: "person.2.badge.key",
        executable: "aas"
    )
    private(set) var availability: MuxaModuleAvailability = .probing
    func probe() async {}
    func settingsPane(model: AppModel) -> AnyView { AnyView(EmptyView()) }
}
