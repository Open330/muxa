import AppKit
import CryptoKit
import Foundation
import Security

/// Muxa.app updating itself, the same shape `muxa upgrade` gives the CLI.
///
/// The release pipeline already publishes everything an updater needs: the
/// `macos-app` job attaches `Muxa-<version>.dmg` and a `Muxa-<version>.dmg
/// .sha256` sidecar to the GitHub release, and `tap-bump` rewrites the
/// `muxa-app` cask from that same checksum. Nothing here adds a hosting
/// surface, a second signing key, or an appcast — it reads the assets that
/// are already there.
///
/// **Channel awareness**, mirroring `upgrade.rs`: a Homebrew-managed copy
/// delegates to `brew upgrade --cask muxa-app` rather than fighting the
/// package manager over `/Applications`, and everything else swaps its own
/// bundle from the release DMG.
///
/// **What makes the swap safe**: the DMG is checked against its published
/// SHA-256 *and* the app inside it is checked against this build's own
/// Developer ID team through the Security framework, so a mirror that serves
/// a different binary under the right filename gets rejected before anything
/// is moved. The exchange itself is `replaceItemAt`, which is a rename on the
/// same volume and leaves the old bundle in place if the new one cannot land.
///
/// **The daemon takes care of itself**: the bundle carries `muxad` in
/// `Contents/Helpers`, and `binary_watch` in muxad notices the path it was
/// launched from now resolving to a different file and re-execs onto the new
/// build within about 30 seconds. No daemon restart is issued from here.

// MARK: - Version ordering

/// Component-wise numeric version ordering, shared by the updater and the
/// Welcome guide so "0.8.9 < 0.8.10" means one thing in this app.
enum MuxaVersion {
    /// ("0.1.9" < "0.1.10", "0.2" == "0.2.0"). A leading `v` and any
    /// non-numeric suffix such as `-beta` are ignored.
    static func compare(_ lhs: String, _ rhs: String) -> ComparisonResult {
        let left = components(of: lhs)
        let right = components(of: rhs)
        for index in 0..<max(left.count, right.count) {
            let leftValue = index < left.count ? left[index] : 0
            let rightValue = index < right.count ? right[index] : 0
            if leftValue < rightValue { return .orderedAscending }
            if leftValue > rightValue { return .orderedDescending }
        }
        return .orderedSame
    }

    /// `v0.8.47` → `0.8.47`; anything else is returned unchanged.
    static func stripTagPrefix(_ tag: String) -> String {
        let trimmed = tag.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed.first == "v" || trimmed.first == "V" else { return trimmed }
        return String(trimmed.dropFirst())
    }

    private static func components(of version: String) -> [Int] {
        stripTagPrefix(version)
            .split(separator: ".")
            .map { Int($0.prefix { $0.isNumber }) ?? 0 }
    }
}

// MARK: - Channel

/// Where this copy of Muxa came from, which decides how it updates itself.
enum MuxaUpdateChannel: Equatable, Sendable {
    /// Installed by `brew install --cask open330/tap/muxa-app`. Homebrew owns
    /// the bundle and its Caskroom metadata; replacing the app behind its back
    /// would leave `brew list --cask --versions muxa-app` lying about what is
    /// on disk, so the upgrade is handed to `brew` instead.
    case homebrewCask(brew: String)
    /// Dragged out of the DMG (or installed by any other means). Muxa swaps
    /// its own bundle.
    case directDownload
    /// Running out of a build directory. Never replaced: the next `Scripts/
    /// build-app.sh` would undo it, and the QA flow deliberately runs a Debug
    /// build from `.build/DerivedData`.
    case developmentBuild
    /// Running from the mounted DMG itself, or from the read-only App
    /// Translocation copy Gatekeeper makes for a quarantined app. There is
    /// nothing at a stable path to replace.
    case notInstalled

    /// Cheap, path-only detection — no `brew` process, which takes about a
    /// second on a cold cache and would run on every launch.
    ///
    /// `exists` is injected so the decision table is unit-testable without a
    /// Homebrew install on the machine running the tests.
    static func detect(
        bundlePath: String,
        home: String = FileManager.default.homeDirectoryForCurrentUser.path,
        exists: (String) -> Bool = { FileManager.default.fileExists(atPath: $0) }
    ) -> MuxaUpdateChannel {
        if bundlePath.contains("/DerivedData/") || bundlePath.contains("/.build/")
            || bundlePath.contains("/Build/Products/")
        {
            return .developmentBuild
        }
        if bundlePath.hasPrefix("/Volumes/") || bundlePath.contains("/AppTranslocation/") {
            return .notInstalled
        }
        // The `app` artifact moves the bundle to /Applications and keeps only
        // metadata in the Caskroom, so the app's own path does not identify a
        // Homebrew install on its own — but it does rule one out. A Caskroom
        // entry says some copy is Homebrew's; only a copy sitting at the
        // artifact's target says it is *this* one. Without that second half,
        // a spare build running from ~/Downloads would quit, upgrade the
        // /Applications copy, and reopen itself unchanged, forever.
        let caskTargets = ["/Applications/Muxa.app", "\(home)/Applications/Muxa.app"]
        guard caskTargets.contains(bundlePath) else { return .directDownload }
        for prefix in ["/opt/homebrew", "/usr/local"] {
            let brew = "\(prefix)/bin/brew"
            if exists("\(prefix)/Caskroom/muxa-app"), exists(brew) {
                return .homebrewCask(brew: brew)
            }
        }
        return .directDownload
    }

    /// Whether Muxa can carry the update out itself.
    var canInstall: Bool {
        switch self {
        case .homebrewCask, .directDownload: true
        case .developmentBuild, .notInstalled: false
        }
    }
}

// MARK: - Release

/// One published release, as far as the Mac app cares about it.
struct MuxaRelease: Equatable, Sendable, Identifiable {
    /// `v0.8.47`
    let tag: String
    /// `0.8.47`
    let version: String
    /// The CHANGELOG section the release workflow publishes as the notes.
    let notes: String
    let publishedAt: Date?
    let dmg: URL
    let checksum: URL

    var id: String { tag }

    /// The release page, for the notes link and for the channels that cannot
    /// install in place.
    var page: URL {
        URL(string: "https://github.com/Open330/muxa/releases/tag/\(tag)")
            ?? URL(string: "https://github.com/Open330/muxa/releases")!
    }

    /// Parse `GET /repos/Open330/muxa/releases/latest`.
    ///
    /// The DMG is optional by design: `release.yml` attaches one only when the
    /// signing secrets are present, and a release cut without them carries the
    /// Rust archives alone. That is reported as "no Mac build in this release"
    /// rather than as a download that would 404.
    static func decode(latest data: Data) throws -> MuxaRelease {
        guard let root = try JSONSerialization.jsonObject(with: data) as? [String: Any],
              let tag = root["tag_name"] as? String, !tag.isEmpty
        else {
            throw MuxaUpdateError.malformedReleaseFeed
        }
        let version = MuxaVersion.stripTagPrefix(tag)
        let assets = root["assets"] as? [[String: Any]] ?? []

        func asset(named name: String) -> URL? {
            assets.lazy
                .first { $0["name"] as? String == name }
                .flatMap { $0["browser_download_url"] as? String }
                .flatMap(URL.init(string:))
        }

        let dmgName = "Muxa-\(version).dmg"
        guard let dmg = asset(named: dmgName), let checksum = asset(named: "\(dmgName).sha256") else {
            throw MuxaUpdateError.noMacAsset(tag: tag)
        }
        return MuxaRelease(
            tag: tag,
            version: version,
            notes: (root["body"] as? String)?.trimmingCharacters(in: .whitespacesAndNewlines) ?? "",
            publishedAt: (root["published_at"] as? String).flatMap {
                ISO8601DateFormatter().date(from: $0)
            },
            dmg: dmg,
            checksum: checksum
        )
    }
}

// MARK: - Errors

enum MuxaUpdateError: LocalizedError, Equatable {
    case malformedReleaseFeed
    case noMacAsset(tag: String)
    case feedFailed(Int)
    case rateLimited
    case httpStatus(Int)
    case checksumMismatch(expected: String, actual: String)
    case emptySidecar
    case mountFailed(String)
    case noAppInImage
    case signatureRejected(String)
    case versionMismatch(expected: String, found: String)
    case destinationNotWritable(String)
    case commandFailed(tool: String, detail: String)
    case channelCannotInstall(MuxaUpdateChannel)

    var errorDescription: String? {
        switch self {
        case .malformedReleaseFeed:
            String(localized: "GitHub returned a release document Muxa could not read.")
        case .noMacAsset(let tag):
            String(localized: "Release \(tag) ships no Mac app. Updating will be offered once a release carries a signed DMG again.")
        case .feedFailed(let code):
            String(localized: "GitHub answered the update check with HTTP \(code).")
        case .rateLimited:
            // 60 unauthenticated requests an hour, per IP — which a shared
            // office address reaches without this app's help. Saying so beats
            // "HTTP 403", which reads like the release is gone.
            String(localized: "GitHub is rate-limiting requests from this network. The next check will try again later.")
        case .httpStatus(let code):
            String(localized: "The download failed with HTTP \(code).")
        case .checksumMismatch(let expected, let actual):
            String(localized: "Checksum mismatch: the release publishes \(expected) and the download hashes to \(actual). Nothing was installed.")
        case .emptySidecar:
            String(localized: "The release's .sha256 sidecar is empty, so the download could not be verified.")
        case .mountFailed(let detail):
            String(localized: "The disk image could not be mounted: \(detail)")
        case .noAppInImage:
            String(localized: "The disk image contains no Muxa.app.")
        case .signatureRejected(let detail):
            String(localized: "The downloaded app is not signed by this build's Developer ID team, so it was discarded: \(detail)")
        case .versionMismatch(let expected, let found):
            String(localized: "The downloaded app reports version \(found) but the release is \(expected). Nothing was installed.")
        case .destinationNotWritable(let path):
            String(localized: "Muxa cannot write to \(path). Move Muxa.app to /Applications, or install the new version from the DMG yourself.")
        case .commandFailed(let tool, let detail):
            String(localized: "\(tool) failed: \(detail)")
        case .channelCannotInstall(.developmentBuild):
            String(localized: "This is a local build from a source checkout, so Muxa will not replace it. Run Scripts/build-app.sh again after pulling.")
        case .channelCannotInstall(.notInstalled):
            String(localized: "Muxa is running from a disk image or a read-only copy. Move Muxa.app to /Applications first.")
        case .channelCannotInstall:
            String(localized: "This install cannot be updated in place.")
        }
    }
}

// MARK: - Preferences

enum MuxaUpdatePreferences {
    /// Whether Muxa looks for a new release on its own.
    static let automaticCheckKey = "muxa.updates.automaticCheck"
    /// `timeIntervalSince1970` of the last completed check, successful or not.
    static let lastCheckedKey = "muxa.updates.lastChecked"
    /// A version the operator asked not to be told about again.
    static let skippedVersionKey = "muxa.updates.skippedVersion"
    /// Scene id of the Software Update window (`openWindow(id:)`).
    static let windowID = "software-update"

    /// Once a day. Frequent enough that a release lands within a working day,
    /// rare enough that an app left open for a week makes seven API calls.
    static let checkInterval: TimeInterval = 24 * 60 * 60

    static func registerDefaults(_ defaults: UserDefaults = .standard) {
        defaults.register(defaults: [automaticCheckKey: true])
    }

    /// The running app's marketing version; "0" on a unit-test host, which
    /// keeps every comparison well defined.
    static var currentVersion: String {
        let version = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
        return version.flatMap { $0.isEmpty ? nil : $0 } ?? "0"
    }
}

// MARK: - Updater

@MainActor
final class MuxaUpdater: ObservableObject {
    static let shared = MuxaUpdater()

    /// Where the flow is. One value, so the window, the Settings row, and the
    /// menu bar cannot disagree about whether a download is running.
    enum Phase: Equatable {
        case idle
        case checking
        case upToDate
        case available(MuxaRelease)
        case downloading(MuxaRelease, fraction: Double)
        case verifying(MuxaRelease)
        case installing(MuxaRelease)
        /// Everything is staged and verified; the app quits and comes back.
        case relaunching(MuxaRelease)
        case failed(String)

        var release: MuxaRelease? {
            switch self {
            case .available(let release), .downloading(let release, _), .verifying(let release),
                 .installing(let release), .relaunching(let release):
                release
            case .idle, .checking, .upToDate, .failed:
                nil
            }
        }

        /// True while an install is under way and must not be started twice.
        var isBusy: Bool {
            switch self {
            case .checking, .downloading, .verifying, .installing, .relaunching: true
            case .idle, .upToDate, .available, .failed: false
            }
        }
    }

    @Published private(set) var phase: Phase = .idle
    @Published private(set) var lastChecked: Date?

    let channel: MuxaUpdateChannel
    let currentVersion: String

    private let defaults: UserDefaults
    private let bundleURL: URL
    private let feed: URL
    private var automaticChecks: Task<Void, Never>?
    /// Whether the check currently in flight has to report its failure. A
    /// user-initiated request that arrives while a background check is running
    /// raises this rather than being dropped — otherwise pressing "Check for
    /// Updates…" offline shows a spinner and then nothing at all.
    private var checkIsUserInitiated = false

    /// The release feed, overridable for QA the way `MUXA_SOCKET` and
    /// `MUXA_TERMINAL_DEBUG` are. Pointing this elsewhere still cannot install
    /// anything: the Developer ID team check on the downloaded app is what
    /// decides, not where the JSON came from.
    nonisolated static var defaultFeed: URL {
        let fallback = URL(string: "https://api.github.com/repos/Open330/muxa/releases/latest")!
        guard let override = ProcessInfo.processInfo.environment["MUXA_UPDATE_FEED"],
              let url = URL(string: override), !override.isEmpty
        else { return fallback }
        return url
    }

    init(
        defaults: UserDefaults = .standard,
        bundleURL: URL = Bundle.main.bundleURL,
        feed: URL = MuxaUpdater.defaultFeed
    ) {
        self.defaults = defaults
        self.bundleURL = bundleURL
        self.feed = feed
        currentVersion = MuxaUpdatePreferences.currentVersion
        channel = MuxaUpdateChannel.detect(bundlePath: bundleURL.path)
        let stamp = defaults.double(forKey: MuxaUpdatePreferences.lastCheckedKey)
        lastChecked = stamp > 0 ? Date(timeIntervalSince1970: stamp) : nil
    }

    /// The update the operator has not dismissed, or nil.
    var pendingRelease: MuxaRelease? {
        guard let release = phase.release else { return nil }
        return release.version == skippedVersion ? nil : release
    }

    var automaticChecksEnabled: Bool {
        defaults.bool(forKey: MuxaUpdatePreferences.automaticCheckKey)
    }

    private var skippedVersion: String? {
        defaults.string(forKey: MuxaUpdatePreferences.skippedVersionKey)
    }

    // MARK: Checking

    /// Start the once-a-day background check. Called once, from
    /// `applicationDidFinishLaunching`.
    func startAutomaticChecks() {
        guard automaticChecks == nil, !OnboardingPreferences.isRunningTests() else { return }
        let bundleURL = self.bundleURL
        automaticChecks = Task { [weak self] in
            // An update interrupted between `ditto` and the exchange leaves a
            // full copy of the app beside the real one. Nothing else ever
            // cleans that up, and it is ~200 MB sitting in /Applications.
            Self.sweepStaleStaging(beside: bundleURL)
            // Launch is busy enough already — muxad is coming up, modules are
            // probing their tools. The first check waits for that to settle.
            try? await Task.sleep(for: .seconds(20))
            while !Task.isCancelled {
                await self?.checkIfDue()
                try? await Task.sleep(for: .seconds(3600))
            }
        }
    }

    /// The background path: silent, and only when the toggle is on and a day
    /// has passed.
    func checkIfDue() async {
        guard automaticChecksEnabled, !phase.isBusy else { return }
        if let lastChecked, Date().timeIntervalSince(lastChecked) < MuxaUpdatePreferences.checkInterval {
            return
        }
        await check(userInitiated: false)
    }

    /// Look for a newer release.
    ///
    /// `userInitiated` decides what "nothing to do" looks like: a manual check
    /// says "up to date", a background one leaves the previous state alone so
    /// a transient network failure never turns into a visible error.
    func check(userInitiated: Bool) async {
        if case .checking = phase {
            // Adopt the stronger intent instead of dropping this call.
            if userInitiated { checkIsUserInitiated = true }
            return
        }
        guard !phase.isBusy else { return }
        let previous = phase
        checkIsUserInitiated = userInitiated
        phase = .checking
        do {
            let release = try await Self.fetchLatest(from: feed)
            // Only a check that answered is a check that happened. Stamping a
            // failure too would buy another 24 hours of silence for a laptop
            // that was merely closed at the wrong moment.
            stampCheck()
            if let skipped = skippedVersion,
               checkIsUserInitiated || MuxaVersion.compare(release.version, skipped) == .orderedDescending
            {
                // Asking for a check is asking to be told again, and a release
                // newer than the skipped one is news on its own.
                defaults.removeObject(forKey: MuxaUpdatePreferences.skippedVersionKey)
            }
            let outcome = Self.outcome(
                for: release,
                current: currentVersion,
                userInitiated: checkIsUserInitiated,
                skipped: skippedVersion
            )
            if case .available = outcome {
                MuxaLog.update.info(
                    "update available: \(self.currentVersion, privacy: .public) → \(release.version, privacy: .public)"
                )
            }
            // A nil outcome is a background check that found only a version
            // already dismissed: say nothing, and keep whatever was on screen.
            phase = outcome ?? previous
        } catch {
            MuxaLog.update.error("update check failed: \(error.localizedDescription, privacy: .public)")
            // A failed background check must not erase an update an earlier one
            // already found.
            phase = checkIsUserInitiated ? .failed(error.localizedDescription) : previous
        }
        checkIsUserInitiated = false
    }

    /// What a completed check means. Nil is "say nothing", which only a
    /// background check that found an already-skipped release ever gets.
    nonisolated static func outcome(
        for release: MuxaRelease,
        current: String,
        userInitiated: Bool,
        skipped: String?
    ) -> Phase? {
        guard MuxaVersion.compare(release.version, current) == .orderedDescending else {
            return .upToDate
        }
        if !userInitiated, release.version == skipped { return nil }
        return .available(release)
    }

    /// Stop being told about this version until a newer one appears.
    func skip(_ release: MuxaRelease) {
        defaults.set(release.version, forKey: MuxaUpdatePreferences.skippedVersionKey)
        phase = .idle
    }

    /// Clear a failure or an "up to date" result without starting anything.
    func dismiss() {
        guard !phase.isBusy else { return }
        phase = .idle
    }

    private func stampCheck() {
        let now = Date()
        lastChecked = now
        defaults.set(now.timeIntervalSince1970, forKey: MuxaUpdatePreferences.lastCheckedKey)
    }

    // MARK: Installing

    /// Download, verify, and swap — or hand the job to Homebrew.
    ///
    /// Returns having either terminated the app (the relaunch script takes
    /// over) or left `phase` on a failure the window can show.
    func install(_ release: MuxaRelease) async {
        guard !phase.isBusy else { return }
        guard channel.canInstall else {
            phase = .failed(MuxaUpdateError.channelCannotInstall(channel).localizedDescription)
            return
        }
        if case .homebrewCask(let brew) = channel {
            installViaHomebrew(brew: brew, release: release)
            return
        }

        let destination = bundleURL
        let parent = destination.deletingLastPathComponent()
        guard FileManager.default.isWritableFile(atPath: parent.path) else {
            phase = .failed(MuxaUpdateError.destinationNotWritable(parent.path).localizedDescription)
            return
        }

        phase = .downloading(release, fraction: 0)
        let expectedTeam = Self.runningTeamIdentifier()
        do {
            let staged = try await Task.detached(priority: .userInitiated) { [weak self] in
                try await Self.stage(
                    release: release,
                    nextTo: destination,
                    expectedTeam: expectedTeam,
                    progress: { fraction in
                        Task { @MainActor in self?.report(fraction: fraction, for: release) }
                    },
                    verifying: {
                        Task { @MainActor in self?.phase = .verifying(release) }
                    }
                )
            }.value

            phase = .installing(release)
            let installed = try Self.swap(staged: staged, into: destination)
            MuxaLog.update.info("installed \(release.tag, privacy: .public); relaunching")
            phase = .relaunching(release)
            do {
                try Self.scheduleRelaunch(of: installed)
            } catch {
                // Past this point the new bundle is already in place, so
                // reporting this as a failed update would be a lie — the only
                // thing that did not happen is the reopen.
                MuxaLog.update.error(
                    "relaunch could not be scheduled: \(error.localizedDescription, privacy: .public)"
                )
                phase = .failed(String(localized: "Muxa \(release.version) is installed. The automatic restart could not be scheduled — quit and reopen Muxa to finish."))
                return
            }
            NSApp.terminate(nil)
        } catch {
            MuxaLog.update.error("update failed: \(error.localizedDescription, privacy: .public)")
            phase = .failed(error.localizedDescription)
        }
    }

    private func report(fraction: Double, for release: MuxaRelease) {
        guard case .downloading = phase else { return }
        phase = .downloading(release, fraction: fraction)
    }

    /// Homebrew owns the bundle, so Homebrew performs the swap.
    ///
    /// It cannot run while this process is alive: the cask carries
    /// `uninstall quit: "dev.muxa.mac"`, so `brew upgrade` quits Muxa out from
    /// under whatever is watching it. The work is handed to a detached script
    /// that waits for this process to exit first, and its output is kept so a
    /// failed upgrade is not silent.
    private func installViaHomebrew(brew: String, release: MuxaRelease) {
        phase = .relaunching(release)
        do {
            try Self.scheduleHomebrewUpgrade(brew: brew, app: bundleURL)
            NSApp.terminate(nil)
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }

    /// `brew upgrade --cask muxa-app`, for the Settings row's copy button.
    nonisolated static let homebrewUpgradeCommand = "brew upgrade --cask muxa-app"

    /// Where the detached scripts write, so a failed unattended upgrade can be
    /// read afterwards.
    nonisolated static var logURL: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/Logs/Muxa/update.log")
    }
}

// MARK: - The work, off the main actor

extension MuxaUpdater {
    nonisolated fileprivate static func fetchLatest(from feed: URL) async throws -> MuxaRelease {
        var request = URLRequest(url: feed, cachePolicy: .reloadIgnoringLocalCacheData, timeoutInterval: 20)
        request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
        // GitHub rejects an API request without one.
        request.setValue("Muxa/\(MuxaUpdatePreferences.currentVersion) (macOS)", forHTTPHeaderField: "User-Agent")
        let (data, response) = try await URLSession.shared.data(for: request)
        if let http = response as? HTTPURLResponse, let error = feedError(for: http.statusCode) {
            throw error
        }
        return try MuxaRelease.decode(latest: data)
    }

    /// How an update check reads a status code; nil when the answer is usable.
    nonisolated static func feedError(for statusCode: Int) -> MuxaUpdateError? {
        switch statusCode {
        case 200..<300: nil
        // 60 unauthenticated requests an hour, per IP. GitHub reports both the
        // hourly limit and secondary rate limiting this way.
        case 403, 429: .rateLimited
        default: .feedFailed(statusCode)
        }
    }

    /// Download, checksum, mount, signature-check, and copy the new app to a
    /// folder beside the one it will replace — same volume, so the exchange
    /// that follows is a rename rather than a copy that can half-finish.
    ///
    /// Everything this returns has already been verified; the caller only has
    /// to move it.
    nonisolated fileprivate static func stage(
        release: MuxaRelease,
        nextTo destination: URL,
        expectedTeam: String?,
        progress: @escaping @Sendable (Double) -> Void,
        verifying: @escaping @Sendable () -> Void
    ) async throws -> URL {
        let work = FileManager.default.temporaryDirectory
            .appendingPathComponent("muxa-update-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: work, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: work) }

        let image = work.appendingPathComponent(release.dmg.lastPathComponent)
        try await download(release.dmg, to: image, progress: progress)
        let sidecar = work.appendingPathComponent(release.checksum.lastPathComponent)
        try await download(release.checksum, to: sidecar, progress: { _ in })

        verifying()
        let expected = try expectedDigest(inSidecar: String(decoding: Data(contentsOf: sidecar), as: UTF8.self))
        let actual = try sha256(ofFileAt: image)
        guard actual == expected else {
            throw MuxaUpdateError.checksumMismatch(expected: expected, actual: actual)
        }

        let mount = try attach(image)
        defer { detach(mount) }
        let mounted = mount.appendingPathComponent("Muxa.app")
        guard FileManager.default.fileExists(atPath: mounted.path) else {
            throw MuxaUpdateError.noAppInImage
        }
        try verifySignature(of: mounted, expectedTeam: expectedTeam)
        try verifyVersion(of: mounted, matches: release.version)

        // Staged beside the destination rather than in /tmp: `replaceItemAt`
        // is only atomic within one volume, and /tmp is not guaranteed to be
        // the volume /Applications lives on.
        let staging = destination.deletingLastPathComponent()
            .appendingPathComponent("\(stagingDirectoryPrefix())\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: true)
        let staged = staging.appendingPathComponent("Muxa.app")
        do {
            // ditto, not FileManager.copyItem: it preserves the extended
            // attributes a signed bundle's resources carry, and a copy that
            // drops them fails its own signature check on first launch.
            try run("/usr/bin/ditto", [mounted.path, staged.path])
            // The DMG was fetched by URLSession, which does not quarantine on
            // an unsandboxed app — but a stray `com.apple.quarantine` from any
            // other source would put a "downloaded from the internet" panel in
            // front of an app whose signature and notarization were just
            // checked here. Strip it and stay quiet.
            try? run("/usr/bin/xattr", ["-dr", "com.apple.quarantine", staged.path])
            // The staged copy is what actually gets installed, so it is what
            // has to pass — not the mounted original it was copied from.
            try verifySignature(of: staged, expectedTeam: expectedTeam)
        } catch {
            try? FileManager.default.removeItem(at: staging)
            throw error
        }
        return staged
    }

    /// Remove staging directories a previous run did not get to delete.
    ///
    /// Named so they are recognisable and so this can never walk into
    /// anything else that happens to live next to the app.
    nonisolated static func stagingDirectoryPrefix() -> String { ".muxa-update-" }

    nonisolated static func sweepStaleStaging(beside bundle: URL) {
        let parent = bundle.deletingLastPathComponent()
        let entries = (try? FileManager.default.contentsOfDirectory(atPath: parent.path)) ?? []
        for entry in entries where entry.hasPrefix(stagingDirectoryPrefix()) {
            try? FileManager.default.removeItem(at: parent.appendingPathComponent(entry))
        }
    }

    /// Exchange the bundles and report where the new one landed. On failure
    /// the original is still in place.
    ///
    /// The returned URL is usually `destination`, but `replaceItemAt` is
    /// documented as free to put the replacement elsewhere — and the relaunch
    /// that follows has exactly one chance to open the right path.
    nonisolated static func swap(staged: URL, into destination: URL) throws -> URL {
        let staging = staged.deletingLastPathComponent()
        defer { try? FileManager.default.removeItem(at: staging) }
        let landed = try FileManager.default.replaceItemAt(
            destination,
            withItemAt: staged,
            backupItemName: nil,
            options: [.usingNewMetadataOnly]
        )
        return landed ?? destination
    }

    /// Hand the relaunch to a script that outlives this process.
    ///
    /// `open` on a bundle that is still running reactivates the old copy
    /// instead of starting the new one, so the script waits for this pid to
    /// disappear first.
    nonisolated fileprivate static func scheduleRelaunch(of app: URL) throws {
        let script = """
        #!/bin/sh
        # Written by Muxa's updater; removes itself when done.
        pid="$1"
        app="$2"
        while kill -0 "$pid" 2>/dev/null; do sleep 0.2; done
        sleep 0.3
        /usr/bin/open "$app"
        rm -f "$0"
        """
        try spawnDetached(script: script, arguments: [String(ProcessInfo.processInfo.processIdentifier), app.path])
    }

    /// The Homebrew equivalent: quit, upgrade, reopen, keeping the output.
    nonisolated fileprivate static func scheduleHomebrewUpgrade(brew: String, app: URL) throws {
        let log = logURL
        try? FileManager.default.createDirectory(
            at: log.deletingLastPathComponent(),
            withIntermediateDirectories: true
        )
        let script = """
        #!/bin/sh
        # Written by Muxa's updater; removes itself when done.
        pid="$1"
        brew="$2"
        app="$3"
        log="$4"
        # A GUI launch inherits a minimal PATH, and brew shells out to git,
        # curl, and the system tools throughout an upgrade.
        PATH="$(dirname "$brew"):/usr/bin:/bin:/usr/sbin:/sbin"
        export PATH
        while kill -0 "$pid" 2>/dev/null; do sleep 0.2; done
        {
            echo "=== $(date '+%Y-%m-%d %H:%M:%S') brew upgrade --cask muxa-app"
            "$brew" upgrade --cask muxa-app 2>&1
            echo "=== exit $?"
        } >> "$log"
        /usr/bin/open "$app"
        rm -f "$0"
        """
        try spawnDetached(script: script, arguments: [
            String(ProcessInfo.processInfo.processIdentifier), brew, app.path, log.path,
        ])
    }

    nonisolated private static func spawnDetached(script: String, arguments: [String]) throws {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("muxa-relaunch-\(UUID().uuidString).sh")
        try Data(script.utf8).write(to: url)
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/sh")
        process.arguments = [url.path] + arguments
        process.standardInput = FileHandle.nullDevice
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        try process.run()
    }

    // MARK: Primitives

    nonisolated private static func download(
        _ url: URL,
        to destination: URL,
        progress: @escaping @Sendable (Double) -> Void
    ) async throws {
        let session = URLSession(
            configuration: .ephemeral,
            delegate: DownloadProgressDelegate(onProgress: progress),
            delegateQueue: nil
        )
        defer { session.finishTasksAndInvalidate() }
        try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Void, Error>) in
            // The completion-handler form, deliberately: it still drives the
            // delegate's progress callbacks, and it hands over the temporary
            // file before URLSession deletes it, which the async `download`
            // variant and a task-level delegate disagree about.
            let task = session.downloadTask(with: url) { location, response, error in
                if let error {
                    continuation.resume(throwing: error)
                    return
                }
                if let http = response as? HTTPURLResponse, !(200..<300).contains(http.statusCode) {
                    continuation.resume(throwing: MuxaUpdateError.httpStatus(http.statusCode))
                    return
                }
                guard let location else {
                    continuation.resume(throwing: MuxaUpdateError.httpStatus(0))
                    return
                }
                do {
                    try? FileManager.default.removeItem(at: destination)
                    try FileManager.default.moveItem(at: location, to: destination)
                    continuation.resume()
                } catch {
                    continuation.resume(throwing: error)
                }
            }
            task.resume()
        }
    }

    /// The sidecar is the `shasum -a 256` line the release job writes:
    /// `<digest>  <file>`.
    nonisolated static func expectedDigest(inSidecar contents: String) throws -> String {
        guard let digest = contents.split(whereSeparator: \.isWhitespace).first, digest.count == 64 else {
            throw MuxaUpdateError.emptySidecar
        }
        return digest.lowercased()
    }

    nonisolated private static func sha256(ofFileAt url: URL) throws -> String {
        let handle = try FileHandle(forReadingFrom: url)
        defer { try? handle.close() }
        var hasher = SHA256()
        while let chunk = try handle.read(upToCount: 1 << 20), !chunk.isEmpty {
            hasher.update(data: chunk)
        }
        return hasher.finalize().map { String(format: "%02x", $0) }.joined()
    }

    nonisolated private static func attach(_ image: URL) throws -> URL {
        let result = try run("/usr/bin/hdiutil", [
            "attach", image.path, "-nobrowse", "-readonly", "-noverify", "-plist",
        ])
        guard let mount = mountPoint(inAttachPlist: result) else {
            throw MuxaUpdateError.mountFailed(
                String(decoding: result, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
            )
        }
        return URL(fileURLWithPath: mount, isDirectory: true)
    }

    /// `hdiutil attach -plist` lists one entity per partition and only the
    /// mountable one carries `mount-point`.
    nonisolated static func mountPoint(inAttachPlist data: Data) -> String? {
        guard let plist = try? PropertyListSerialization.propertyList(from: data, options: [], format: nil),
              let root = plist as? [String: Any],
              let entities = root["system-entities"] as? [[String: Any]]
        else { return nil }
        return entities.lazy
            .compactMap { $0["mount-point"] as? String }
            .first { !$0.isEmpty }
    }

    nonisolated private static func detach(_ mount: URL) {
        // A detach immediately after a ditto can lose to a still-open file;
        // -force is what `hdiutil` itself recommends for an unattended eject.
        try? run("/usr/bin/hdiutil", ["detach", mount.path, "-force"])
    }

    /// The team this build is signed by, so the update is held to the same
    /// identity rather than to a name hard-coded here. Nil on an unsigned
    /// local build.
    nonisolated static func runningTeamIdentifier() -> String? {
        var code: SecCode?
        guard SecCodeCopySelf([], &code) == errSecSuccess, let code else { return nil }
        var staticCode: SecStaticCode?
        guard SecCodeCopyStaticCode(code, [], &staticCode) == errSecSuccess, let staticCode else {
            return nil
        }
        var information: CFDictionary?
        guard SecCodeCopySigningInformation(staticCode, SecCSFlags(rawValue: kSecCSSigningInformation), &information) == errSecSuccess,
              let dictionary = information as? [String: Any]
        else { return nil }
        return dictionary[kSecCodeInfoTeamIdentifier as String] as? String
    }

    /// The code requirement an update has to satisfy.
    ///
    /// Two clauses beyond the anchor, both from Apple's published Developer ID
    /// marker OIDs: certificate 1 is the Developer ID intermediate and the
    /// leaf is a Developer ID Application certificate. With a team identifier
    /// known, the leaf's OU pins it to this build's own team, which is what
    /// stops a validly-signed app from somebody else being installed as an
    /// update to this one.
    nonisolated static func requirementText(teamIdentifier: String?) -> String {
        var clauses = [
            "anchor apple generic",
            "certificate 1[field.1.2.840.113635.100.6.2.6] exists",
            "certificate leaf[field.1.2.840.113635.100.6.1.13] exists",
        ]
        if let teamIdentifier, !teamIdentifier.isEmpty {
            clauses.append("certificate leaf[subject.OU] = \"\(teamIdentifier)\"")
        }
        return clauses.joined(separator: " and ")
    }

    nonisolated private static func verifySignature(of app: URL, expectedTeam: String?) throws {
        var staticCode: SecStaticCode?
        guard SecStaticCodeCreateWithPath(app as CFURL, [], &staticCode) == errSecSuccess,
              let staticCode
        else {
            throw MuxaUpdateError.signatureRejected(String(localized: "the bundle carries no code signature"))
        }
        var requirement: SecRequirement?
        let text = requirementText(teamIdentifier: expectedTeam) as CFString
        guard SecRequirementCreateWithString(text, [], &requirement) == errSecSuccess, let requirement
        else {
            throw MuxaUpdateError.signatureRejected(String(localized: "the code requirement could not be built"))
        }
        // kSecCSCheckNestedCode walks Contents/Helpers too, which is where the
        // muxa and muxad this app runs live. A bundle whose signature is
        // intact but whose embedded daemon is not is exactly the shape this
        // check exists to refuse.
        let flags = SecCSFlags(rawValue: kSecCSCheckAllArchitectures | kSecCSCheckNestedCode)
        var error: Unmanaged<CFError>?
        let status = SecStaticCodeCheckValidityWithErrors(staticCode, flags, requirement, &error)
        guard status == errSecSuccess else {
            let detail = error?.takeRetainedValue().localizedDescription
                ?? String(localized: "OSStatus \(Int(status))")
            throw MuxaUpdateError.signatureRejected(detail)
        }
    }

    /// A release whose DMG carries a different version than the feed
    /// advertised means the two came from different places. Refuse it.
    nonisolated private static func verifyVersion(of app: URL, matches version: String) throws {
        let plist = app.appendingPathComponent("Contents/Info.plist")
        let data = (try? Data(contentsOf: plist)) ?? Data()
        let root = (try? PropertyListSerialization.propertyList(from: data, options: [], format: nil))
            as? [String: Any]
        let found = root?["CFBundleShortVersionString"] as? String ?? ""
        guard MuxaVersion.compare(found, version) == .orderedSame else {
            throw MuxaUpdateError.versionMismatch(expected: version, found: found.isEmpty ? "unknown" : found)
        }
    }

    @discardableResult
    nonisolated private static func run(_ executable: String, _ arguments: [String]) throws -> Data {
        let process = Process()
        let output = Pipe()
        let errors = Pipe()
        process.executableURL = URL(fileURLWithPath: executable)
        process.arguments = arguments
        process.standardOutput = output
        process.standardError = errors
        process.standardInput = FileHandle.nullDevice
        try process.run()
        // Read before waiting: hdiutil's plist is large enough to fill a pipe
        // buffer, and a full pipe would deadlock a wait-then-read.
        let data = output.fileHandleForReading.readDataToEndOfFile()
        let failure = errors.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        guard process.terminationStatus == 0 else {
            let detail = String(decoding: failure, as: UTF8.self)
                .trimmingCharacters(in: .whitespacesAndNewlines)
            throw MuxaUpdateError.commandFailed(
                tool: (executable as NSString).lastPathComponent,
                detail: detail.isEmpty ? String(localized: "exit \(Int(process.terminationStatus))") : detail
            )
        }
        return data
    }
}

/// URLSession calls a session delegate's methods serially on its own queue,
/// and this stores nothing but one immutable `@Sendable` closure — the whole
/// of what the unchecked conformance is claiming.
private final class DownloadProgressDelegate: NSObject, URLSessionDownloadDelegate, @unchecked Sendable {
    private let onProgress: @Sendable (Double) -> Void

    init(onProgress: @escaping @Sendable (Double) -> Void) {
        self.onProgress = onProgress
    }

    func urlSession(
        _ session: URLSession,
        downloadTask: URLSessionDownloadTask,
        didWriteData bytesWritten: Int64,
        totalBytesWritten: Int64,
        totalBytesExpectedToWrite: Int64
    ) {
        guard totalBytesExpectedToWrite > 0 else { return }
        onProgress(Double(totalBytesWritten) / Double(totalBytesExpectedToWrite))
    }

    /// Required by the protocol. The completion-handler task form is what
    /// takes delivery of the file, so this is never the one that matters.
    func urlSession(
        _ session: URLSession,
        downloadTask: URLSessionDownloadTask,
        didFinishDownloadingTo location: URL
    ) {}
}
