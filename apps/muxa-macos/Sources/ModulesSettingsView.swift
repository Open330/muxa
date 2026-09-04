import SwiftUI

/// Settings › Modules — the optional integrations, what each needs, and each
/// module's own settings underneath its switch.
struct ModulesSettingsPane: View {
    @ObservedObject var model: AppModel
    @ObservedObject var registry: MuxaModuleRegistry

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                header
                ForEach(registry.modules, id: \.id) { module in
                    ModuleCard(module: module, model: model, registry: registry)
                }
            }
            .padding(20)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .task(id: registry.probeGeneration == 0) {
            await registry.probeEnabled()
        }
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Modules")
                .font(.title2.weight(.semibold))
            Text("Optional integrations with tools you already have. Muxa works with none of them switched on, and a module only ever adds actions — it never takes over what muxa does itself.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }
}

private struct ModuleCard: View {
    let module: any MuxaModule
    @ObservedObject var model: AppModel
    @ObservedObject var registry: MuxaModuleRegistry

    private var isEnabled: Bool { registry.isEnabled(module.id) }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .top, spacing: 10) {
                Image(systemName: module.identity.symbolName)
                    .font(.title3)
                    .foregroundStyle(isEnabled ? Color.accentColor : Color.secondary)
                    .frame(width: 24)
                VStack(alignment: .leading, spacing: 3) {
                    Text(verbatim: module.identity.title)
                        .font(.headline)
                    Text(verbatim: module.identity.blurb)
                        .font(.callout)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                    availabilityLine
                }
                Spacer(minLength: 8)
                Toggle("", isOn: Binding(
                    get: { isEnabled },
                    set: { registry.setEnabled(module.id, $0) }
                ))
                .labelsHidden()
            }

            if isEnabled {
                Divider()
                module.settingsPane(model: model)
            }
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 12))
        .overlay {
            RoundedRectangle(cornerRadius: 12)
                .stroke(Color(nsColor: .separatorColor).opacity(0.5), lineWidth: 0.5)
        }
    }

    @ViewBuilder
    private var availabilityLine: some View {
        switch module.availability {
        case .probing:
            if isEnabled {
                HStack(spacing: 6) {
                    ProgressView().controlSize(.mini)
                    Text("Looking for it…").font(.caption).foregroundStyle(.secondary)
                }
            } else {
                Text("Switch it on to look for it.")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }
        case .available(let version, let detail):
            Label(
                [version, detail].compactMap { $0 }.joined(separator: " · "),
                systemImage: "checkmark.circle.fill"
            )
            .font(.caption)
            .foregroundStyle(.green)
            .labelStyle(.titleAndIcon)
        case .missing(let hint):
            VStack(alignment: .leading, spacing: 2) {
                Label(hint, systemImage: "exclamationmark.circle")
                    .font(.caption)
                    .foregroundStyle(.orange)
                if let homepage = module.identity.homepage {
                    Link(destination: homepage) {
                        Text(verbatim: homepage.absoluteString).font(.caption)
                    }
                }
            }
        case .unusable(let reason):
            Label(reason, systemImage: "exclamationmark.triangle.fill")
                .font(.caption)
                .foregroundStyle(.orange)
        }
    }
}

/// A menu of everything the enabled modules offer for one object. Renders
/// nothing when no module contributes, so a call site can place it
/// unconditionally.
struct MuxaModuleMenu: View {
    let context: MuxaModuleContext
    @ObservedObject var model: AppModel
    @ObservedObject var registry: MuxaModuleRegistry
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
