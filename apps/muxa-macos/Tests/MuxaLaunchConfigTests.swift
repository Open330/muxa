import Foundation
import Testing
@testable import Muxa

private func launchFixture() -> MuxaLaunchSettings {
    MuxaLaunchSettings(
        providers: [
            .init(program: "codex", options: nil, effectiveOptions: ["legacy"]),
            .init(program: "claude", options: [], effectiveOptions: []),
        ],
        pipelines: [
            .init(pipeline: "with.dot", index: 0, name: "review", program: "codex", options: nil, effectiveOptions: ["legacy"]),
        ],
        legacyGuide: .init(program: "codex", options: ["legacy"])
    )
}

@Test func launchConfigPreviewUsesReplacementAndExplicitEmpty() {
    var settings = launchFixture()
    #expect(settings.effectiveOptions(program: "codex", override: nil) == ["legacy"])
    #expect(settings.effectiveOptions(program: "claude", override: nil) == [])
    #expect(settings.effectiveOptions(program: "codex", override: []) == [])
    settings.providers[0].options = ["--model", "default"]
    #expect(settings.effectiveOptions(program: "codex", override: nil) == ["--model", "default"])
    #expect(settings.effectiveOptions(program: "codex", override: ["override"]) == ["override"])
    settings.providers[0].options = []
    #expect(settings.effectiveOptions(program: "codex", override: nil) == [])
    #expect(settings.legacyGuide.options == ["legacy"])
}

@Test func launchConfigEditsPreserveNullEmptyAndArgumentBoundaries() throws {
    let baseline = launchFixture()
    #expect(baseline.edits(against: baseline).isEmpty)
    var settings = baseline
    settings.providers[0].options = []
    settings.providers[1].options = nil
    settings.pipelines[0].options = ["--model", "a b", "", "quote\"\n한글", "#[]"]
    let data = try JSONSerialization.data(withJSONObject: settings.edits(against: baseline))
    let edits = try #require(JSONSerialization.jsonObject(with: data) as? [[String: Any]])
    #expect(edits.count == 3)
    #expect(edits[0]["options"] as? [String] == [])
    #expect(edits[1]["options"] is NSNull)
    #expect(edits[2]["options"] as? [String] == settings.pipelines[0].options)
    #expect(edits[2]["pipeline"] as? String == "with.dot")
    #expect(edits[2]["index"] as? Int == 0)
    #expect(!edits.contains { $0["target"] as? String == "legacy_guide" })
}

@Test func launchConfigDecodesMissingVersusEmptyOptions() throws {
    let data = Data(#"{"providers":[{"program":"codex","options":null,"effective_options":["legacy"]},{"program":"claude","options":[],"effective_options":[]}],"pipelines":[],"legacy_guide":{"program":"codex","options":["legacy"]}}"#.utf8)
    let settings = try JSONDecoder().decode(MuxaLaunchSettings.self, from: data)
    #expect(settings.providers[0].options == nil)
    #expect(settings.providers[1].options == [])
    #expect(settings.legacyGuide.options == ["legacy"])
}

@Test func launchConfigClientUsesPinnedSnapshotAndTypedResponse() async throws {
    let baseline = launchFixture()
    let encodedSettings = try JSONEncoder().encode(baseline)
    let client = MuxaConfigClient(socketPath: "/unused") { _, payload in
        let request = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        let version = try #require(request["protocol"] as? Int)
        #expect(version == Int(MuxaIPCClient.protocolVersion))
        if request["kind"] as? String == "config_launch_write" {
            #expect(request["expected_text"] as? String == "")
            let edits = try #require(request["edits"] as? [[String: Any]])
            #expect(edits.count == 1)
            #expect(edits[0]["options"] as? [String] == [])
        } else {
            #expect(request["kind"] as? String == "config_launch_read")
        }
        return try JSONSerialization.data(withJSONObject: [
            "ok": true,
            "config": ["path": "/config.toml", "text": "", "exists": false],
            "launch": JSONSerialization.jsonObject(with: encodedSettings),
        ])
    }
    let document = try await client.readLaunch()
    #expect(document.launch == baseline)
    #expect(!document.config.exists)
    var changed = baseline
    changed.providers[0].options = []
    _ = try await client.writeLaunch(expectedText: document.config.text, settings: changed, baseline: baseline)
}

@Test func launchConfigClientDistinguishesConflictValidationAndMissingCapability() async throws {
    let baseline = launchFixture()
    let conflict = MuxaConfigClient(socketPath: "/unused") { _, _ in
        Data(##"{"ok":false,"error":"changed","config":{"path":"/config.toml","text":"# current","exists":true}}"##.utf8)
    }
    do {
        _ = try await conflict.writeLaunch(expectedText: "", settings: baseline, baseline: baseline)
        Issue.record("stale writes must throw")
    } catch let error as MuxaConfigConflict {
        #expect(error.current.text == "# current")
        #expect(error.message == "changed")
    }
    for message in ["invalid options", "unknown request config_launch_read"] {
        let client = MuxaConfigClient(socketPath: "/unused") { _, _ in
            try JSONSerialization.data(withJSONObject: ["ok": false, "error": message])
        }
        do {
            _ = try await client.readLaunch()
            Issue.record("server errors must throw")
        } catch {
            #expect(!(error is MuxaConfigConflict))
            #expect(error.localizedDescription.contains(message))
        }
    }
}

@Test @MainActor func launchConfigStoreBlocksRawDirtyAndRequiresReloadAfterConflict() async {
    let baseline = launchFixture()
    let store = MuxaConfigStore(document: .init(path: "/config.toml", text: "# baseline", exists: true), launch: baseline)
    store.launchDraft?.providers[0].options = []
    store.draft = "# unsaved raw"
    let unused = MuxaConfigClient(socketPath: "/unused") { _, _ in
        Issue.record("a blocked save must not contact the daemon")
        return Data()
    }
    #expect(await store.saveLaunch(client: unused) == false)
    store.draft = "# baseline"
    let conflict = MuxaConfigClient(socketPath: "/unused") { _, _ in
        Data(##"{"ok":false,"error":"changed","config":{"path":"/config.toml","text":"# reordered pipeline","exists":true}}"##.utf8)
    }
    #expect(await store.saveLaunch(client: conflict) == false)
    #expect(store.launchNeedsReload)
    #expect(store.launchDraft?.providers[0].options == [])
    #expect(store.document?.text == "# baseline")
    #expect(await store.saveLaunch(client: unused) == false)
}

@Test @MainActor func launchConfigStorePreservesEditsMadeDuringSaveAndPinsMissingFile() async throws {
    let baseline = launchFixture()
    let store = MuxaConfigStore(document: .init(path: "/config.toml", text: "", exists: false), launch: baseline)
    store.launchDraft?.providers[0].options = []
    let submitted = try #require(store.launchDraft)
    let encodedSettings = try JSONEncoder().encode(submitted)
    let release = DispatchSemaphore(value: 0)
    let client = MuxaConfigClient(socketPath: "/unused") { _, payload in
        let request = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        #expect(request["expected_text"] as? String == "")
        #expect(release.wait(timeout: .now() + 5) == .success)
        return try JSONSerialization.data(withJSONObject: [
            "ok": true,
            "config": ["path": "/config.toml", "text": "# saved", "exists": true],
            "launch": JSONSerialization.jsonObject(with: encodedSettings),
        ])
    }
    let task = Task { await store.saveLaunch(client: client) }
    while !store.isSaving { await Task.yield() }
    #expect(await store.saveLaunch(client: client) == false)
    store.launchDraft?.providers[0].options = ["newer draft"]
    store.draft = "# newer raw draft"
    release.signal()
    #expect(await task.value)
    #expect(store.document?.text == "# saved")
    #expect(store.draft == "# newer raw draft")
    #expect(store.launchDraft?.providers[0].options == ["newer draft"])
    #expect(store.launch?.providers[0].options == [])
    #expect(store.isLaunchDirty)
    #expect(store.isDirty)
}

@Test @MainActor func rawConfigStorePinsMissingFileAndPreservesTypingDuringSave() async {
    let store = MuxaConfigStore(document: .init(path: "/config.toml", text: "", exists: false))
    store.draft = "# submitted"
    let release = DispatchSemaphore(value: 0)
    let client = MuxaConfigClient(socketPath: "/unused") { _, payload in
        let request = try #require(JSONSerialization.jsonObject(with: payload) as? [String: Any])
        #expect(request["kind"] as? String == "config_write")
        #expect(request["expected_text"] as? String == "")
        #expect(request["text"] as? String == "# submitted")
        #expect(release.wait(timeout: .now() + 5) == .success)
        return Data(##"{"ok":true,"config":{"path":"/config.toml","text":"# submitted","exists":true}}"##.utf8)
    }
    let task = Task { await store.save(client: client) }
    while !store.isSaving { await Task.yield() }
    store.draft = "# newer typing"
    release.signal()
    #expect(await task.value)
    #expect(store.document?.text == "# submitted")
    #expect(store.draft == "# newer typing")
    #expect(store.isDirty)
    #expect(store.launchNeedsReload)
}

@Test @MainActor func rawConfigStoreCannotOverwriteUnsavedLaunchOptions() async {
    let store = MuxaConfigStore(document: .init(path: "/config.toml", text: "", exists: false), launch: launchFixture())
    store.launchDraft?.providers[0].options = []
    store.draft = "# raw"
    let unused = MuxaConfigClient(socketPath: "/unused") { _, _ in
        Issue.record("a blocked raw save must not contact the daemon")
        return Data()
    }
    #expect(await store.save(client: unused) == false)
    #expect(store.isDirty)
    #expect(store.isLaunchDirty)
}
