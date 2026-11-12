import AppKit
import SwiftUI

/// AAS keeps separate account limits. Never add percentages across accounts
/// or pretend an expired/missing reading means that an account is unused.
enum AasMenuBarUsage {
    static let staleAfter: TimeInterval = 600

    static func groups(_ accounts: [AasAccount], headings: [AasProviderGroup]) -> [AasProviderGroup] {
        // Flat accounts are authoritative; partial provider metadata must
        // never make an account disappear or replace its current meters.
        AasUsageSnapshot.groups(from: accounts).map { group in
            AasProviderGroup(provider: group.provider,
                title: headings.first(where: { $0.provider == group.provider })?.title ?? group.title,
                accounts: group.accounts)
        }
    }

    static func meters(_ account: AasAccount, now: Date) -> [AasMeter] {
        account.meters.filter {
            $0.usedPct.isFinite && ($0.resetMs.map { Double($0) / 1000 > now.timeIntervalSince1970 } ?? true)
        }
    }

    static func isStale(_ account: AasAccount, now: Date) -> Bool {
        guard let ms = account.fetchedAtMs else { return true }
        return now.timeIntervalSince1970 - Double(ms) / 1000 > staleAfter
    }

    static func peak(_ accounts: [AasAccount], now: Date) -> Double? {
        accounts.filter(\.active).flatMap { meters($0, now: now) }.map(\.usedPct).max()
    }

    static func hasWarning(_ accounts: [AasAccount], now: Date) -> Bool {
        accounts.filter(\.active).contains {
            $0.error?.isEmpty == false || isStale($0, now: now) || meters($0, now: now).isEmpty
        }
    }

    static func title(_ accounts: [AasAccount], failed: Bool, now: Date) -> String {
        let warning = failed || hasWarning(accounts, now: now)
        let value = peak(accounts, now: now).map(AasFormat.percent) ?? "—"
        return "AAS \(value)\(warning ? " !" : "")"
    }
}

struct AasMenuBarPanel: View {
    @ObservedObject var store: AasAccountStore
    @State private var provider = ""
    @State private var query = ""
    @State private var activeOnly = false
    @AppStorage(TendPreferences.appearanceKey) private var appearance = TendAppearance.system.rawValue

    var body: some View {
        TimelineView(.periodic(from: .now, by: 60)) { context in
            VStack(alignment: .leading, spacing: 12) {
                HStack {
                    Label("Accounts and usage", systemImage: "gauge.medium").font(.headline)
                    Spacer()
                    Text(verbatim: "AAS · \(store.accounts.count)").foregroundStyle(.secondary)
                }
                HStack {
                    Button("Refresh") { Task { await store.refresh() } }
                    Button("Refresh live") { Task { await store.refresh(live: true) } }
                    if store.isRefreshing { ProgressView().controlSize(.small) }
                    Spacer()
                    TendSettingsShortcut(tab: .accounts, title: "Open Accounts settings").labelStyle(.iconOnly)
                }
                .controlSize(.small)
                .disabled(store.isRefreshing)
                Text("Highest usage among active accounts")
                    .font(TendType.meta).foregroundStyle(.secondary)
                if let error = store.error {
                    Label(error, systemImage: "exclamationmark.triangle.fill")
                        .font(TendType.detail).foregroundStyle(.orange)
                        .fixedSize(horizontal: false, vertical: true)
                }
                if !store.accounts.isEmpty {
                    HStack {
                        Picker("Provider", selection: $provider) {
                            Text("All providers").tag("")
                            ForEach(allGroups) { group in Text(verbatim: group.title).tag(group.provider) }
                        }
                        .labelsHidden()
                        .pickerStyle(.menu)
                        Toggle("Active accounts only", isOn: $activeOnly).toggleStyle(.checkbox)
                    }
                    TextField("Find account", text: $query).textFieldStyle(.roundedBorder)
                }
                let groups = filteredGroups
                if groups.isEmpty {
                    Text(store.accounts.isEmpty ? String(localized: "No accounts stored yet.") : String(localized: "No matching accounts"))
                        .foregroundStyle(.secondary).padding(.vertical, 12)
                } else {
                    ScrollView {
                        LazyVStack(alignment: .leading, spacing: 12) {
                            ForEach(groups) { group in
                                Text(verbatim: group.title).font(.subheadline.weight(.semibold))
                                ForEach(group.accounts) { account in
                                    AasMenuBarAccountRow(account: account, now: context.date)
                                }
                            }
                        }
                    }
                    .frame(height: min(420, CGFloat(groups.reduce(0) { $0 + $1.accounts.count }) * 132 + CGFloat(groups.count) * 28))
                }
                Divider()
                HStack {
                    if let date = store.lastRefreshedAt {
                        Text("Updated \(date.formatted(date: .omitted, time: .shortened))")
                    }
                    Spacer()
                    Text("Usage is shown per account")
                }
                .font(TendType.meta).foregroundStyle(.secondary)
            }
            .padding(16).frame(width: 420)
        }
        .preferredColorScheme(TendAppearance(rawValue: appearance)?.colorScheme)
        .task { await store.refresh() }
    }

    private var filteredGroups: [AasProviderGroup] {
        allGroups.compactMap { group in
            guard provider.isEmpty || provider == group.provider else { return nil }
            let accounts = group.accounts.filter {
                (!activeOnly || $0.active) && (query.isEmpty || "\($0.name) \($0.email ?? "")".localizedCaseInsensitiveContains(query))
            }.sorted { $0.active != $1.active ? $0.active : $0.name.localizedStandardCompare($1.name) == .orderedAscending }
            return accounts.isEmpty ? nil : AasProviderGroup(provider: group.provider, title: group.title, accounts: accounts)
        }
    }

    private var allGroups: [AasProviderGroup] {
        AasMenuBarUsage.groups(store.accounts, headings: store.groups)
    }
}

private struct AasMenuBarAccountRow: View {
    let account: AasAccount
    let now: Date

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(alignment: .firstTextBaseline) {
                Text(verbatim: account.name).font(.subheadline.weight(.semibold)).lineLimit(1).help(account.email ?? account.name)
                if account.active {
                    Text("Active account").font(TendType.meta).foregroundStyle(.green)
                }
                Spacer(minLength: 4)
                if let plan = account.planText { Text(verbatim: plan).font(TendType.meta).foregroundStyle(.secondary).lineLimit(1) }
            }
            if let error = account.error, !error.isEmpty {
                Label(error, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange).font(TendType.meta).lineLimit(3)
            }
            let meters = AasMenuBarUsage.meters(account, now: now)
            if meters.isEmpty {
                Text("No current usage reported").font(TendType.detail).foregroundStyle(.secondary)
            }
            ForEach(meters) { meter in
                HStack {
                    Text(verbatim: meter.label).font(TendType.meta.monospaced()).frame(width: 26, alignment: .leading)
                    ProgressView(value: meter.fraction).tint(meter.tint).frame(width: 85)
                    Text(verbatim: AasFormat.percent(meter.usedPct)).font(TendType.detail.monospacedDigit()).frame(width: 40, alignment: .trailing)
                    Spacer(minLength: 0)
                    if let reset = AasFormat.resets(atMilliseconds: meter.resetMs, now: now) {
                        Text(verbatim: reset).font(TendType.meta).foregroundStyle(.secondary)
                    }
                }
                .accessibilityElement(children: .combine)
            }
            HStack(spacing: 4) {
                if AasMenuBarUsage.isStale(account, now: now) {
                    Label("Usage may be out of date", systemImage: "clock.badge.exclamationmark").foregroundStyle(.orange)
                } else if account.cached {
                    Text("Cached usage").foregroundStyle(.secondary)
                }
                if let ms = account.fetchedAtMs {
                    Text(verbatim: Date(timeIntervalSince1970: Double(ms) / 1000).formatted(date: .abbreviated, time: .shortened))
                        .foregroundStyle(.secondary)
                }
            }
            .font(TendType.meta)
        }
        .padding(10).frame(maxWidth: .infinity, alignment: .leading)
        .background(.regularMaterial, in: RoundedRectangle(cornerRadius: 10))
    }
}
