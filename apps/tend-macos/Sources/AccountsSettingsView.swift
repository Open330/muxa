import SwiftUI

/// Settings › Accounts: aas, the account switcher Tend wraps; what happens
/// when an agent runs out of usage; and the accounts themselves.
struct TendAccountsSettingsPane: View {
    @ObservedObject var model: AppModel
    @ObservedObject var registry: TendModuleRegistry
    @ObservedObject var swaps: TendSwapCenter
    @ObservedObject private var automations = AutomationStore.shared

    private var aas: AasModule? { registry.module(AasModule.self) }
    /// The daemon's account module is off: nothing here would act.
    private var moduleOff: Bool { swaps.accountModuleOff }
    private var isEnabled: Bool { registry.isEnabled(AasModule.identity.id) && !moduleOff }

    var body: some View {
        Form {
            if moduleOff {
                Section {
                    Label("The account module is off in tendd", systemImage: "person.2.slash")
                        .font(.headline)
                    TendSettingsCaption("Switching agents between accounts is an optional module. Set [modules] accounts = true in config.toml, or configure accounts, then restart tendd. Usage limits are still seen and resumed without it.")
                    if let reason = swaps.health?.reason {
                        TendSettingsCaption(verbatim: reason)
                    }
                }
            }
            Section {
                Toggle(isOn: Binding(
                    get: { isEnabled },
                    set: { registry.setEnabled(AasModule.identity.id, $0) }
                )) {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(verbatim: AasModule.identity.title)
                        // The blurb is the app's own sentence, so it is
                        // translated; the title is a product name and is not.
                        TendSettingsCaption(verbatim: AasModule.identity.blurb)
                    }
                }
                if let aas, !moduleOff {
                    TendModuleAvailabilityRow(module: aas, isEnabled: isEnabled)
                }
            } header: {
                Text(verbatim: "aas")
            }
            .disabled(moduleOff)

            Section("When an agent hits its usage limit") {
                TendLimitPolicyPicker(model: model, store: automations, showsTitle: false)
                if automations.limitPolicy == .switchAccount {
                    TendSwapHealthSection(swaps: model.swaps)
                }
            }

            if isEnabled, let aas {
                aas.settingsPane(model: model)
            }
        }
        .formStyle(.grouped)
        .task(id: registry.probeGeneration == 0) {
            await registry.probeEnabled()
        }
    }
}

/// Whether a module's tool was found, in one row.
private struct TendModuleAvailabilityRow: View {
    let module: any TendModule
    let isEnabled: Bool

    var body: some View {
        switch module.availability {
        case .probing:
            if isEnabled {
                HStack(spacing: 6) {
                    ProgressView().controlSize(.mini)
                    TendSettingsCaption("Looking for it…")
                }
            } else {
                TendSettingsCaption("Switch it on to look for it.")
            }
        case .available(let version, let detail):
            Label(
                [version, detail].compactMap { $0 }.joined(separator: " · "),
                systemImage: "checkmark.circle.fill"
            )
            .font(TendType.detail)
            .foregroundStyle(.green)
            .labelStyle(.titleAndIcon)
        case .missing(let hint):
            VStack(alignment: .leading, spacing: 2) {
                Label(hint, systemImage: "exclamationmark.circle")
                    .font(TendType.detail)
                    .foregroundStyle(.orange)
                if let homepage = module.identity.homepage {
                    Link(destination: homepage) {
                        Text(verbatim: homepage.absoluteString).font(TendType.detail)
                    }
                }
            }
        case .unusable(let reason):
            Label(reason, systemImage: "exclamationmark.triangle.fill")
                .font(TendType.detail)
                .foregroundStyle(.orange)
        }
    }
}

/// A menu of everything the enabled modules offer for one object. Renders
/// nothing when no module contributes, so a call site can place it
/// unconditionally.
struct TendModuleMenu: View {
    let context: TendModuleContext
    @ObservedObject var model: AppModel
    @ObservedObject var registry: TendModuleRegistry
    var label: LocalizedStringKey = "Modules"

    var body: some View {
        let actions = registry.actions(for: context, model: model)
        if !actions.isEmpty {
            Menu {
                ForEach(actions) { action in
                    Button {
                        Task { await action.perform() }
                    } label: {
                        Label(action.title, systemImage: action.symbolName)
                    }
                    .disabled(!action.isEnabled)
                    .help(action.disabledReason ?? "")
                }
            } label: {
                Label(label, systemImage: "puzzlepiece.extension")
            }
            .menuStyle(.borderlessButton)
            .fixedSize()
        }
    }
}
