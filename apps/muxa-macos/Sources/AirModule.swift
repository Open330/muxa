import SwiftUI

/// Placeholder replaced by the AIR module implementation.
@MainActor
final class AirModule: MuxaModule {
    nonisolated static let identity = MuxaModuleIdentity(
        id: "air",
        title: "AIR Workbench",
        blurb: "Placeholder.",
        symbolName: "point.3.connected.trianglepath.dotted",
        executable: "node"
    )
    private(set) var availability: MuxaModuleAvailability = .probing
    func probe() async {}
    func settingsPane(model: AppModel) -> AnyView { AnyView(EmptyView()) }
}
