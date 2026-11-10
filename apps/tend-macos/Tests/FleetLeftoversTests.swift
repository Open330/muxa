import Foundation
import Testing
@testable import Tend

@Test func aWorktreeRowDecodesFromTheHostsJSON() throws {
    let json = """
    [{"id":"r1","repo":"/repo","path":"/Users/me/src/tend-wt/feat","branch":"feat","base":"main",
      "start_head":"abc","origin":{"kind":"work","workspace":"tend","work":"feat"},
      "common_dir":"/repo/.git","created_at":"2026-11-10T09:00:00Z","state":"created",
      "candidate":false,"blockers":["a pane is working in it"],"current_branch":"feat","bytes":2048},
     {"id":"r2","repo":"/repo","path":"/tmp/runs/x","branch":null,"base":"abc","start_head":"abc",
      "origin":{"kind":"dispatch","dispatch_id":"d1","workspace":"tend","work":"fix"},
      "common_dir":null,"created_at":"2026-11-10T09:00:00Z","state":"created",
      "candidate":true,"blockers":[],"current_branch":null,"current_head":"abc","bytes":null}]
    """
    let rows = try JSONDecoder().decode([TendWorktreeRow].self, from: Data(json.utf8))
    #expect(rows.count == 2)
    #expect(rows[0].shortPath == "tend-wt/feat")
    #expect(rows[1].origin.dispatchID == "d1")
    #expect(FleetLeftovers.sorted(rows).first?.id == "r2")
}

@Test func setAsideRowsOfferRestoreAndDeleteAndCarryWhatWasSeen() throws {
    let json = """
    {"id":"r3","repo":"/repo","path":"/src/wt","branch":"feat","base":"main","start_head":null,
     "origin":{"kind":"work","workspace":null,"work":"feat"},"common_dir":"/repo/.git",
     "created_at":"2026-11-10T09:00:00Z","state":"set_aside","generation":3,
     "set_aside":{"dest":"/src/.tend-set-aside/wt.r3","head":"0123456789abcdef","branch":"feat",
                  "pins":["refs/tend/set-aside/r3/0123456789abcdef"],"at":"2026-11-10T10:00:00Z"},
     "candidate":true,"blockers":[],"current_branch":"feat","current_head":"0123456789abcdef",
     "restorable":true,"bytes":10}
    """
    let row = try JSONDecoder().decode(TendWorktreeRow.self, from: Data(json.utf8))
    #expect(row.isSetAside)
    #expect(!row.canSetAside)
    #expect(row.canDelete)
    #expect(row.canRestore)
    #expect(row.location == "/src/.tend-set-aside/wt.r3")
    #expect(row.shortHead == "0123456789ab")
    let item = row.changeItem.json
    #expect(item["generation"] as? UInt64 == 3)
    #expect(item["head"] as? String == "0123456789abcdef")
    #expect(item["branch"] as? String == "feat")

    // A detached checkout sends an explicit null branch, so the host can
    // tell "detached" from "not said".
    let detached = try JSONDecoder().decode(TendWorktreeRow.self, from: Data(json.replacingOccurrences(
        of: "\"current_branch\":\"feat\"", with: "\"current_branch\":null").utf8))
    #expect(detached.changeItem.json["branch"] is NSNull)
    #expect(JSONSerialization.isValidJSONObject(detached.changeItem.json))
}
