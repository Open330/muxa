import AppKit
import SwiftUI

/// The surfaces for `MuxaUpdater`: a Software Update window, the Settings row
/// that opens it, the app-menu item macOS users look for first, and the
/// menu-bar line that is the only visible sign of a background check.

// MARK: - Window

struct MuxaSoftwareUpdateView: View {
    @ObservedObject var updater: MuxaUpdater
    @AppStorage(MuxaUpdatePreferences.automaticCheckKey) private var automaticCheck = true
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Divider()
            content
                .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            Divider()
            footer
        }
        .frame(minWidth: 520, minHeight: 380)
        .task {
            // Opening the window is itself a request to know, but a check
            // that just ran does not need repeating.
            if case .idle = updater.phase { await updater.checkIfDue() }
        }
    }

    private var header: some View {
        HStack(spacing: 14) {
            Image(nsImage: NSApp.applicationIconImage)
                .resizable()
                .frame(width: 56, height: 56)
            VStack(alignment: .leading, spacing: 3) {
                Text("Muxa").font(.title2.weight(.semibold))
                Text("Version \(updater.currentVersion)")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
                Text(channelDescription)
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }
            Spacer()
        }
        .padding(20)
    }

    @ViewBuilder
    private var content: some View {
        switch updater.phase {
        case .idle, .upToDate, .checking, .failed:
            statusOnly
        case .available(let release):
            availableUpdate(release)
        case .downloading(let release, let fraction):
            progress(
                title: String(localized: "Downloading Muxa \(release.version)"),
                detail: String(localized: "\(Int(fraction * 100))% of the release disk image"),
                fraction: fraction
            )
        case .verifying(let release):
            progress(
                title: String(localized: "Verifying Muxa \(release.version)"),
                detail: String(localized: "Checking the published SHA-256 and the Developer ID signature."),
                fraction: nil
            )
        case .installing(let release):
            progress(
                title: String(localized: "Installing Muxa \(release.version)"),
                detail: String(localized: "Replacing the application bundle."),
                fraction: nil
            )
        case .relaunching(let release):
            progress(
                title: String(localized: "Restarting Muxa \(release.version)"),
                detail: relaunchDetail,
                fraction: nil
            )
        }
    }

    private var statusOnly: some View {
        VStack(alignment: .leading, spacing: 10) {
            switch updater.phase {
            case .checking:
                HStack(spacing: 8) {
                    ProgressView().controlSize(.small)
                    Text("Looking for a newer release…")
                }
            case .upToDate:
                Label("Muxa \(updater.currentVersion) is the newest release.", systemImage: "checkmark.circle.fill")
                    .foregroundStyle(.green)
            case .failed(let detail):
                Label("The update check did not finish.", systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange)
                Text(detail)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            default:
                Text("Muxa checks for a new release once a day and never installs one without being asked.")
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            if let lastChecked = updater.lastChecked {
                Text("Last checked \(lastChecked.formatted(date: .abbreviated, time: .shortened))")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }
        }
        .padding(20)
    }

    private func availableUpdate(_ release: MuxaRelease) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text("Muxa \(release.version) is available")
                    .font(.headline)
                if let published = release.publishedAt {
                    Text(published.formatted(date: .abbreviated, time: .omitted))
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Link(destination: release.page) {
                    Label("Release Notes", systemImage: "arrow.up.right.square")
                        .font(.caption)
                }
            }
            if !updater.channel.canInstall {
                Label(
                    MuxaUpdateError.channelCannotInstall(updater.channel).localizedDescription,
                    systemImage: "info.circle"
                )
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            } else if case .homebrewCask = updater.channel {
                Label(
                    "Homebrew installed this copy, so Muxa hands the upgrade to it. Muxa quits, `\(MuxaUpdater.homebrewUpgradeCommand)` runs, and Muxa reopens.",
                    systemImage: "shippingbox"
                )
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            }
            if release.notes.isEmpty {
                Text("This release published no notes.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            } else {
                ScrollView {
                    ReadableMarkdownContent(source: release.notes)
                        .padding(14)
                }
                .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: 8))
            }
        }
        .padding(20)
    }

    private func progress(title: String, detail: String, fraction: Double?) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(title).font(.headline)
            if let fraction {
                ProgressView(value: fraction)
            } else {
                ProgressView().progressViewStyle(.linear)
            }
            Text(detail)
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(20)
    }

    private var footer: some View {
        HStack(spacing: 12) {
            Toggle("Check automatically", isOn: $automaticCheck)
                .toggleStyle(.checkbox)
            Spacer()
            actions
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 14)
    }

    @ViewBuilder
    private var actions: some View {
        switch updater.phase {
        case .available(let release):
            Button("Skip This Version") { updater.skip(release) }
            Button("Later") { dismiss() }
            if updater.channel.canInstall {
                Button(installTitle) {
                    Task { await updater.install(release) }
                }
                .buttonStyle(.borderedProminent)
                .keyboardShortcut(.defaultAction)
            } else {
                Link(destination: release.page) {
                    Text("Download…")
                }
                .buttonStyle(.borderedProminent)
            }
        case .downloading, .verifying, .installing, .relaunching:
            Button("Close") { dismiss() }.disabled(true)
        default:
            Button("Close") {
                // Clear the finished result on the way out, so reopening the
                // window does not start on last week's error or on an "up to
                // date" that was true three releases ago.
                updater.dismiss()
                dismiss()
            }
            Button("Check Now") {
                Task { await updater.check(userInitiated: true) }
            }
            .buttonStyle(.borderedProminent)
            .disabled(updater.phase.isBusy)
        }
    }

    private var installTitle: LocalizedStringKey {
        if case .homebrewCask = updater.channel { return "Upgrade with Homebrew" }
        return "Install and Relaunch"
    }

    private var relaunchDetail: String {
        if case .homebrewCask = updater.channel {
            return String(localized: "Muxa quits so Homebrew can replace it, then reopens. Output is kept in ~/Library/Logs/Muxa/update.log.")
        }
        return String(localized: "Muxa quits and reopens on the new version. tmux sessions keep running, and the bundled muxad re-execs onto the new build on its own.")
    }

    private var channelDescription: String {
        switch updater.channel {
        case .homebrewCask:
            String(localized: "Installed by Homebrew (cask muxa-app)")
        case .directDownload:
            String(localized: "Installed from the release disk image")
        case .developmentBuild:
            String(localized: "Local build from a source checkout")
        case .notInstalled:
            String(localized: "Running from a read-only copy")
        }
    }
}

// MARK: - Settings

/// The Updates section of Settings › General.
struct MuxaUpdateSettingsSection: View {
    @ObservedObject var updater: MuxaUpdater
    @AppStorage(MuxaUpdatePreferences.automaticCheckKey) private var automaticCheck = true
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Section("Updates") {
            LabeledContent("Installed version") {
                Text(updater.currentVersion).monospacedDigit()
            }
            Toggle("Check for updates automatically", isOn: $automaticCheck)
            Text(summary)
                .font(.caption)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            HStack {
                Button("Check for Updates…") { presentUpdateWindow() }
                if case .homebrewCask = updater.channel {
                    Button("Copy Homebrew Command") {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(MuxaUpdater.homebrewUpgradeCommand, forType: .string)
                    }
                }
                Spacer()
                if let release = updater.pendingRelease {
                    Button("Update to \(release.version)…") { presentUpdateWindow() }
                        .buttonStyle(.borderedProminent)
                }
            }
        }
    }

    private var summary: String {
        var lines = [channelSentence]
        if let lastChecked = updater.lastChecked {
            lines.append(String(localized: "Last checked \(lastChecked.formatted(date: .abbreviated, time: .shortened))."))
        }
        return lines.joined(separator: " ")
    }

    private var channelSentence: String {
        switch updater.channel {
        case .homebrewCask:
            String(localized: "Homebrew installed this copy, so Muxa upgrades it with `\(MuxaUpdater.homebrewUpgradeCommand)` rather than replacing the bundle behind Homebrew's back.")
        case .directDownload:
            String(localized: "Muxa verifies the release's published SHA-256 and Developer ID signature before replacing itself, then reopens.")
        case .developmentBuild:
            String(localized: "This is a local build from a source checkout. Muxa reports new releases but will not replace it.")
        case .notInstalled:
            String(localized: "Muxa is running from a read-only copy. Move Muxa.app to /Applications to update in place.")
        }
    }

    private func presentUpdateWindow() {
        openWindow(id: MuxaUpdatePreferences.windowID)
        NSApp.activate(ignoringOtherApps: true)
        Task { await updater.check(userInitiated: true) }
    }
}

// MARK: - Menus

/// Muxa › Check for Updates…, where macOS users look for it.
struct MuxaUpdateMenuCommands: Commands {
    var body: some Commands {
        CommandGroup(after: .appInfo) {
            Divider()
            MuxaCheckForUpdatesMenuItem()
        }
    }
}

private struct MuxaCheckForUpdatesMenuItem: View {
    @ObservedObject private var updater = MuxaUpdater.shared
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Button(title) {
            openWindow(id: MuxaUpdatePreferences.windowID)
            NSApp.activate(ignoringOtherApps: true)
            Task { await updater.check(userInitiated: true) }
        }
        .disabled(updater.phase.isBusy)
    }

    private var title: LocalizedStringKey {
        if let release = updater.pendingRelease { return "Update to \(release.version)…" }
        return "Check for Updates…"
    }
}

/// The menu-bar line. A background check has no other way to be seen, so this
/// stays out of the way until there is something to say.
struct MuxaUpdateMenuBarItem: View {
    @ObservedObject var updater: MuxaUpdater
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        if let release = updater.pendingRelease {
            Divider()
            Button {
                openWindow(id: MuxaUpdatePreferences.windowID)
                NSApp.activate(ignoringOtherApps: true)
            } label: {
                Label("Update to Muxa \(release.version)…", systemImage: "arrow.down.circle.fill")
            }
        }
    }
}
