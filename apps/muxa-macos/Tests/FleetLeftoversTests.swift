import Foundation
import Testing
@testable import Muxa

@Test func aWorktreeRowDecodesFromTheHostsJSON() throws {
    let json = """
    [{"id":"r1","repo":"/repo","path":"/Users/me/src/muxa-wt/feat","branch":"feat","base":"main",
      "start_head":"abc","origin":{"kind":"work","workspace":"muxa","work":"feat"},
      "common_dir":"/repo/.git","created_at":"2026-11-10T09:00:00Z","state":"created",
      "candidate":false,"blockers":["a pane is working in it"],"current_branch":"feat","bytes":2048},
     {"id":"r2","repo":"/repo","path":"/tmp/runs/x","branch":null,"base":"abc","start_head":"abc",
      "origin":{"kind":"dispatch","dispatch_id":"d1","workspace":"muxa","work":"fix"},
      "common_dir":null,"created_at":"2026-11-10T09:00:00Z","state":"created",
      "candidate":true,"blockers":[],"current_branch":null,"bytes":null}]
    """
    let rows = try JSONDecoder().decode([MuxaWorktreeRow].self, from: Data(json.utf8))
    #expect(rows.count == 2)
    #expect(rows[0].shortPath == "muxa-wt/feat")
    #expect(rows[1].origin.dispatchID == "d1")
    #expect(FleetLeftovers.sorted(rows).first?.id == "r2")
}
