import Foundation
import Security
import Testing
@testable import Muxa

// MARK: - Version ordering

@Test func versionOrderingIsNumericNotLexicographic() {
    #expect(MuxaVersion.compare("0.8.9", "0.8.10") == .orderedAscending)
    #expect(MuxaVersion.compare("0.8.47", "0.8.46") == .orderedDescending)
    #expect(MuxaVersion.compare("0.2", "0.2.0") == .orderedSame)
    #expect(MuxaVersion.compare("v0.8.47", "0.8.47") == .orderedSame)
    #expect(MuxaVersion.compare("1.0.0-beta", "1.0.0") == .orderedSame)
}

@Test func versionStripsOnlyALeadingTagPrefix() {
    #expect(MuxaVersion.stripTagPrefix("v0.8.47") == "0.8.47")
    #expect(MuxaVersion.stripTagPrefix(" v0.8.47 ") == "0.8.47")
    #expect(MuxaVersion.stripTagPrefix("0.8.47") == "0.8.47")
    // A "v" that is not the tag marker must survive.
    #expect(MuxaVersion.stripTagPrefix("version") == "ersion")
}

@Test func onboardingAndTheUpdaterOrderVersionsIdentically() {
    // They share one implementation; this is the assertion that keeps it so.
    #expect(OnboardingPreferences.compareVersions("0.8.9", "0.8.10") == MuxaVersion.compare("0.8.9", "0.8.10"))
    #expect(OnboardingPreferences.compareVersions("0.9", "0.9.0") == .orderedSame)
}

// MARK: - Channel detection

@Test func channelDetectionReadsTheInstallLocation() {
    let caskroom: Set<String> = ["/opt/homebrew/Caskroom/muxa-app", "/opt/homebrew/bin/brew"]

    #expect(
        MuxaUpdateChannel.detect(bundlePath: "/Applications/Muxa.app", exists: caskroom.contains)
            == .homebrewCask(brew: "/opt/homebrew/bin/brew")
    )
    // Same path, no Caskroom entry: dragged out of the DMG.
    #expect(
        MuxaUpdateChannel.detect(bundlePath: "/Applications/Muxa.app", exists: { _ in false })
            == .directDownload
    )
    // An Intel prefix is the other place Homebrew installs.
    let intel: Set<String> = ["/usr/local/Caskroom/muxa-app", "/usr/local/bin/brew"]
    #expect(
        MuxaUpdateChannel.detect(bundlePath: "/Applications/Muxa.app", exists: intel.contains)
            == .homebrewCask(brew: "/usr/local/bin/brew")
    )
    // A Caskroom entry with no brew next to it is not a Homebrew install.
    #expect(
        MuxaUpdateChannel.detect(
            bundlePath: "/Applications/Muxa.app",
            exists: { $0 == "/opt/homebrew/Caskroom/muxa-app" }
        ) == .directDownload
    )
}

@Test func channelDetectionRefusesBuildAndReadOnlyCopies() {
    let anything: (String) -> Bool = { _ in true }
    // The QA flow runs a Debug build from exactly this shape of path.
    #expect(
        MuxaUpdateChannel.detect(
            bundlePath: "/Users/me/personal/muxa/apps/muxa-macos/.build/DerivedData/Build/Products/Debug/Muxa.app",
            exists: anything
        ) == .developmentBuild
    )
    #expect(
        MuxaUpdateChannel.detect(bundlePath: "/Volumes/Muxa 0.8.47/Muxa.app", exists: anything)
            == .notInstalled
    )
    #expect(
        MuxaUpdateChannel.detect(
            bundlePath: "/private/var/folders/x1/AppTranslocation/2B0F/d/Muxa.app",
            exists: anything
        ) == .notInstalled
    )
    #expect(MuxaUpdateChannel.developmentBuild.canInstall == false)
    #expect(MuxaUpdateChannel.notInstalled.canInstall == false)
    #expect(MuxaUpdateChannel.directDownload.canInstall)
}

@Test func aCaskroomEntryOnlySpeaksForTheCopyHomebrewInstalled() {
    let installed: Set<String> = ["/opt/homebrew/Caskroom/muxa-app", "/opt/homebrew/bin/brew"]
    let home = "/Users/me"

    // A spare build run from ~/Downloads while the cask is installed. Handing
    // this to `brew upgrade` would upgrade the /Applications copy and reopen
    // the unchanged one — the update would appear to do nothing, forever.
    #expect(
        MuxaUpdateChannel.detect(
            bundlePath: "\(home)/Downloads/Muxa.app",
            home: home,
            exists: installed.contains
        ) == .directDownload
    )
    // Both targets the cask's `app` artifact can write to.
    #expect(
        MuxaUpdateChannel.detect(bundlePath: "/Applications/Muxa.app", home: home, exists: installed.contains)
            == .homebrewCask(brew: "/opt/homebrew/bin/brew")
    )
    #expect(
        MuxaUpdateChannel.detect(
            bundlePath: "\(home)/Applications/Muxa.app",
            home: home,
            exists: installed.contains
        ) == .homebrewCask(brew: "/opt/homebrew/bin/brew")
    )
}

// MARK: - Release feed

/// The asset names the `macos-app` job attaches, and the fields the updater
/// reads out of `GET /releases/latest`.
private func releaseFeed(tag: String, assets: [String]) -> Data {
    let base = "https://github.com/Open330/muxa/releases/download/\(tag)"
    let payload: [String: Any] = [
        "tag_name": tag,
        "body": "### Added\n- Software Update in the Mac app\n",
        "published_at": "2026-09-21T09:30:00Z",
        "assets": assets.map { ["name": $0, "browser_download_url": "\(base)/\($0)"] },
    ]
    return try! JSONSerialization.data(withJSONObject: payload)
}

@Test func releaseDecodesTheDmgAndItsSidecar() throws {
    let data = releaseFeed(
        tag: "v0.8.47",
        assets: [
            "muxa-v0.8.47-aarch64-apple-darwin.tar.gz",
            "Muxa-0.8.47.dmg",
            "Muxa-0.8.47.dmg.sha256",
        ]
    )
    let release = try MuxaRelease.decode(latest: data)
    #expect(release.tag == "v0.8.47")
    #expect(release.version == "0.8.47")
    #expect(release.dmg.lastPathComponent == "Muxa-0.8.47.dmg")
    #expect(release.checksum.lastPathComponent == "Muxa-0.8.47.dmg.sha256")
    #expect(release.notes.hasPrefix("### Added"))
    #expect(release.publishedAt != nil)
    #expect(release.page.absoluteString == "https://github.com/Open330/muxa/releases/tag/v0.8.47")
}

@Test func releaseWithoutASignedDmgIsNotOfferedAsAnUpdate() {
    // What a release cut without the signing secrets looks like: the Rust
    // archives are there and the DMG is not.
    let data = releaseFeed(tag: "v0.8.47", assets: ["muxa-v0.8.47-aarch64-apple-darwin.tar.gz"])
    #expect(throws: MuxaUpdateError.noMacAsset(tag: "v0.8.47")) {
        try MuxaRelease.decode(latest: data)
    }
    // A DMG whose sidecar never uploaded is equally unusable.
    let halfUploaded = releaseFeed(tag: "v0.8.47", assets: ["Muxa-0.8.47.dmg"])
    #expect(throws: MuxaUpdateError.noMacAsset(tag: "v0.8.47")) {
        try MuxaRelease.decode(latest: halfUploaded)
    }
}

@Test func releaseRejectsADocumentWithNoTag() {
    #expect(throws: MuxaUpdateError.malformedReleaseFeed) {
        try MuxaRelease.decode(latest: Data(#"{"message":"Not Found"}"#.utf8))
    }
}

@Test func rateLimitingIsNotReportedAsAMissingRelease() {
    // GitHub answers an over-quota unauthenticated check with 403, which as a
    // bare status code reads like the release is gone.
    #expect(MuxaUpdater.feedError(for: 403) == .rateLimited)
    #expect(MuxaUpdater.feedError(for: 429) == .rateLimited)
    #expect(MuxaUpdater.feedError(for: 404) == .feedFailed(404))
    #expect(MuxaUpdater.feedError(for: 500) == .feedFailed(500))
    #expect(MuxaUpdater.feedError(for: 200) == nil)
    #expect(MuxaUpdater.feedError(for: 204) == nil)
}

// MARK: - What a finished check means

private func fixtureRelease(_ version: String) throws -> MuxaRelease {
    try MuxaRelease.decode(
        latest: releaseFeed(
            tag: "v\(version)",
            assets: ["Muxa-\(version).dmg", "Muxa-\(version).dmg.sha256"]
        )
    )
}

@Test func aBackgroundCheckStaysQuietAboutASkippedRelease() throws {
    let release = try fixtureRelease("0.8.47")
    #expect(
        MuxaUpdater.outcome(for: release, current: "0.8.46", userInitiated: false, skipped: "0.8.47")
            == nil
    )
    // Asking for a check is asking to be told again.
    #expect(
        MuxaUpdater.outcome(for: release, current: "0.8.46", userInitiated: true, skipped: "0.8.47")
            == .available(release)
    )
    // A release newer than the skipped one is news on its own.
    #expect(
        MuxaUpdater.outcome(for: release, current: "0.8.46", userInitiated: false, skipped: "0.8.46")
            == .available(release)
    )
    #expect(
        MuxaUpdater.outcome(for: release, current: "0.8.46", userInitiated: false, skipped: nil)
            == .available(release)
    )
}

@Test func anOlderOrEqualReleaseIsAlwaysUpToDate() throws {
    let release = try fixtureRelease("0.8.47")
    #expect(MuxaUpdater.outcome(for: release, current: "0.8.47", userInitiated: true, skipped: nil) == .upToDate)
    #expect(MuxaUpdater.outcome(for: release, current: "0.9.0", userInitiated: false, skipped: nil) == .upToDate)
}

// MARK: - Checksum sidecar

@Test func sidecarDigestIsTheFirstFieldLowercased() throws {
    let digest = String(repeating: "AB", count: 32)
    #expect(try MuxaUpdater.expectedDigest(inSidecar: "\(digest)  Muxa-0.8.47.dmg\n") == digest.lowercased())
}

@Test func sidecarWithoutAUsableDigestIsRefused() {
    #expect(throws: MuxaUpdateError.emptySidecar) {
        try MuxaUpdater.expectedDigest(inSidecar: "   \n")
    }
    // Truncated mid-upload: 63 hex characters is not a SHA-256.
    #expect(throws: MuxaUpdateError.emptySidecar) {
        try MuxaUpdater.expectedDigest(inSidecar: String(repeating: "a", count: 63) + "  Muxa.dmg")
    }
}

// MARK: - hdiutil

@Test func mountPointIsReadFromTheMountablePartition() {
    let plist = """
    <?xml version="1.0" encoding="UTF-8"?>
    <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
    <plist version="1.0">
    <dict>
      <key>system-entities</key>
      <array>
        <dict>
          <key>content-hint</key><string>GUID_partition_scheme</string>
          <key>dev-entry</key><string>/dev/disk6</string>
        </dict>
        <dict>
          <key>content-hint</key><string>Apple_HFS</string>
          <key>dev-entry</key><string>/dev/disk6s1</string>
          <key>mount-point</key><string>/Volumes/Muxa 0.8.47</string>
        </dict>
      </array>
    </dict>
    </plist>
    """
    #expect(MuxaUpdater.mountPoint(inAttachPlist: Data(plist.utf8)) == "/Volumes/Muxa 0.8.47")
    #expect(MuxaUpdater.mountPoint(inAttachPlist: Data("not a plist".utf8)) == nil)
}

@Test func staleStagingDirectoriesAreSweptAndNothingElseIs() throws {
    let root = FileManager.default.temporaryDirectory
        .appendingPathComponent("muxa-sweep-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: root) }

    let leftover = root.appendingPathComponent(".muxa-update-abc123")
    try FileManager.default.createDirectory(at: leftover, withIntermediateDirectories: true)
    // Two things that live next to an installed app and must survive.
    let neighbour = root.appendingPathComponent("Safari.app")
    try FileManager.default.createDirectory(at: neighbour, withIntermediateDirectories: true)
    let app = root.appendingPathComponent("Muxa.app")
    try FileManager.default.createDirectory(at: app, withIntermediateDirectories: true)

    MuxaUpdater.sweepStaleStaging(beside: app)

    #expect(!FileManager.default.fileExists(atPath: leftover.path))
    #expect(FileManager.default.fileExists(atPath: neighbour.path))
    #expect(FileManager.default.fileExists(atPath: app.path))
}

@Test func swappingReportsWhereTheNewBundleLandedAndClearsStaging() throws {
    let root = FileManager.default.temporaryDirectory
        .appendingPathComponent("muxa-swap-\(UUID().uuidString)")
    let staging = root.appendingPathComponent(".muxa-update-1")
    try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: root) }

    let destination = root.appendingPathComponent("Muxa.app")
    try FileManager.default.createDirectory(at: destination, withIntermediateDirectories: true)
    try Data("old".utf8).write(to: destination.appendingPathComponent("marker"))

    let staged = staging.appendingPathComponent("Muxa.app")
    try FileManager.default.createDirectory(at: staged, withIntermediateDirectories: true)
    try Data("new".utf8).write(to: staged.appendingPathComponent("marker"))

    let landed = try MuxaUpdater.swap(staged: staged, into: destination)

    // The relaunch opens whatever this returns, so it has to be the bundle
    // that actually exists.
    #expect(FileManager.default.fileExists(atPath: landed.path))
    #expect(
        try String(decoding: Data(contentsOf: landed.appendingPathComponent("marker")), as: UTF8.self) == "new"
    )
    #expect(!FileManager.default.fileExists(atPath: staging.path))
}

// MARK: - Code requirement

@Test func codeRequirementPinsTheUpdateToThisBuildsTeam() {
    let pinned = MuxaUpdater.requirementText(teamIdentifier: "AB12CD34EF")
    #expect(pinned.contains("anchor apple generic"))
    // The Developer ID intermediate and leaf marker OIDs, so a Mac App Store
    // or development signature cannot satisfy it.
    #expect(pinned.contains("certificate 1[field.1.2.840.113635.100.6.2.6] exists"))
    #expect(pinned.contains("certificate leaf[field.1.2.840.113635.100.6.1.13] exists"))
    #expect(pinned.contains("certificate leaf[subject.OU] = \"AB12CD34EF\""))

    // An unsigned local build still produces a syntactically valid
    // requirement — just without the team clause.
    let unpinned = MuxaUpdater.requirementText(teamIdentifier: nil)
    #expect(!unpinned.contains("subject.OU"))
    #expect(!MuxaUpdater.requirementText(teamIdentifier: "").contains("subject.OU"))

    // Whatever the text says, the Security framework has to accept it.
    for text in [pinned, unpinned] {
        var requirement: SecRequirement?
        #expect(SecRequirementCreateWithString(text as CFString, [], &requirement) == errSecSuccess)
        #expect(requirement != nil)
    }
}

// MARK: - Updater state

@MainActor
private func isolatedUpdater(_ name: String = UUID().uuidString) -> (MuxaUpdater, UserDefaults) {
    let defaults = UserDefaults(suiteName: "muxa.updater.tests.\(name)")!
    defaults.removePersistentDomain(forName: "muxa.updater.tests.\(name)")
    MuxaUpdatePreferences.registerDefaults(defaults)
    return (MuxaUpdater(defaults: defaults, bundleURL: URL(fileURLWithPath: "/Applications/Muxa.app")), defaults)
}

@Test @MainActor func automaticChecksAreOnByDefault() {
    let (updater, _) = isolatedUpdater()
    #expect(updater.automaticChecksEnabled)
    #expect(updater.lastChecked == nil)
    #expect(updater.phase == .idle)
    #expect(updater.pendingRelease == nil)
}

@Test @MainActor func aSkippedVersionStopsBeingOffered() throws {
    let (updater, defaults) = isolatedUpdater()
    let release = try MuxaRelease.decode(
        latest: releaseFeed(tag: "v0.8.47", assets: ["Muxa-0.8.47.dmg", "Muxa-0.8.47.dmg.sha256"])
    )
    updater.skip(release)
    #expect(defaults.string(forKey: MuxaUpdatePreferences.skippedVersionKey) == "0.8.47")
    #expect(updater.phase == .idle)
    #expect(updater.pendingRelease == nil)
}

@Test @MainActor func phaseReportsWhatIsInFlight() throws {
    let release = try MuxaRelease.decode(
        latest: releaseFeed(tag: "v0.8.47", assets: ["Muxa-0.8.47.dmg", "Muxa-0.8.47.dmg.sha256"])
    )
    #expect(MuxaUpdater.Phase.idle.isBusy == false)
    #expect(MuxaUpdater.Phase.available(release).isBusy == false)
    #expect(MuxaUpdater.Phase.failed("nope").isBusy == false)
    #expect(MuxaUpdater.Phase.downloading(release, fraction: 0.5).isBusy)
    #expect(MuxaUpdater.Phase.verifying(release).isBusy)
    #expect(MuxaUpdater.Phase.relaunching(release).isBusy)
    #expect(MuxaUpdater.Phase.downloading(release, fraction: 0.5).release == release)
    #expect(MuxaUpdater.Phase.upToDate.release == nil)
}

@Test @MainActor func installIsRefusedForABuildMuxaDoesNotOwn() async throws {
    let defaults = UserDefaults(suiteName: "muxa.updater.tests.\(UUID().uuidString)")!
    let updater = MuxaUpdater(
        defaults: defaults,
        bundleURL: URL(fileURLWithPath: "/Volumes/Muxa 0.8.47/Muxa.app")
    )
    #expect(updater.channel == .notInstalled)
    let release = try MuxaRelease.decode(
        latest: releaseFeed(tag: "v0.8.47", assets: ["Muxa-0.8.47.dmg", "Muxa-0.8.47.dmg.sha256"])
    )
    await updater.install(release)
    // No download was started; the refusal is the whole outcome.
    guard case .failed(let detail) = updater.phase else {
        Issue.record("expected a refusal, got \(updater.phase)")
        return
    }
    #expect(detail == MuxaUpdateError.channelCannotInstall(.notInstalled).localizedDescription)
}
