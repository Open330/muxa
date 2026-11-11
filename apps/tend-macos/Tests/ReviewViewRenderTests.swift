import AppKit
import Foundation
import SwiftUI
import Testing
@testable import Tend

/// Renders the review sheet's newer pieces off-screen with realistic data,
/// so a layout that fails to render (or renders empty) fails a test. Set
/// TEND_RENDER_DIR (TEST_RUNNER_TEND_RENDER_DIR through xcodebuild) to also
/// write each image there for a look. ImageRenderer draws AppKit-backed
/// controls such as text fields as a placeholder box; that is the renderer,
/// not the view.
@MainActor
private func render<V: View>(_ view: V, name: String, scheme: ColorScheme) throws -> NSImage {
    let renderer = ImageRenderer(content: view
        .frame(width: 720)
        .padding(16)
        .background(scheme == .dark ? Color.black : Color.white)
        .environment(\.colorScheme, scheme))
    renderer.scale = 2
    let image = try #require(renderer.nsImage, "\(name) did not render")
    #expect(image.size.width >= 700 && image.size.height > 40, "\(name) rendered \(image.size)")
    if let dir = ProcessInfo.processInfo.environment["TEND_RENDER_DIR"],
       let tiff = image.tiffRepresentation,
       let png = NSBitmapImageRep(data: tiff)?.representation(using: .png, properties: [:]) {
        try png.write(to: URL(fileURLWithPath: dir).appendingPathComponent("\(name)-\(scheme == .dark ? "dark" : "light").png"))
    }
    return image
}

@MainActor
@Test func reviewRequestRowsAndRecordedCommandsRender() throws {
    let items = try JSONDecoder().decode([TendReviewItem].self, from: Data("""
    [{"id":"rv_1","target":{"kind":"work","workspace":"tend","work":"storage","alias":"impl"},
      "generation":2,"summary":"Check the migration and the retry path before merging","anchor":"commit:1a2b3c4",
      "requested_at":"2026-11-11T01:00:00Z","requested_by":{"kind":"agent","label":"impl"}}]
    """.utf8))
    let evidence = try JSONDecoder().decode(TendTurnEvidence.self, from: Data("""
    {"session_id":"s1","more":true,"complete":true,"pairs":[
      {"tool":"Bash","call":"cargo test -p tend --lib review","result":"running 21 tests\\n.....................\\ntest result: ok. 21 passed; 0 failed","is_error":false,"clipped":false,"call_event":"e1","result_event":"e2"},
      {"tool":"Bash","call":"cargo clippy --workspace --all-targets -- -D warnings","result":"warning: unused variable\\nerror: could not compile `tend`\\n\\nerror: aborting due to 1 previous error\\nexit status 101","is_error":true,"clipped":true,"call_event":"e3","result_event":"e4"},
      {"tool":"Bash","call":"git push origin HEAD","result":null,"is_error":null,"clipped":false,"call_event":"e5","result_event":null}
    ]}
    """.utf8))
    for scheme in [ColorScheme.light, .dark] {
        _ = try render(
            FleetReviewRequestRow(item: items[0], busy: false, returning: .constant(false), note: .constant(""), decide: { _, _ in }),
            name: "request-row", scheme: scheme)
        _ = try render(
            FleetReviewRequestRow(item: items[0], busy: false, returning: .constant(true), note: .constant("tests missing"), decide: { _, _ in }),
            name: "request-row-returning", scheme: scheme)
        _ = try render(FleetEvidenceList(evidence: evidence), name: "recorded-commands", scheme: scheme)
    }
}
