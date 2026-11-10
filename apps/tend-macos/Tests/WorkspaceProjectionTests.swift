import Testing
@testable import Tend

@Test @MainActor func workspaceProjectionBurstBuildsOnlyTheLatestPendingInput() async {
    let projector = TendCoalescedProjection<Int>()
    var release: CheckedContinuation<Void, Never>?
    var builds = 0
    var active = 0
    var maximumActive = 0
    var submitted = 0
    var latest = 0
    var published: [Int] = []
    func build(_ value: Int) async -> Int {
        builds += 1
        active += 1
        maximumActive = max(maximumActive, active)
        if value == 0 {
            await withCheckedContinuation { release = $0 }
        }
        active -= 1
        return value
    }
    let first = Task {
        await projector.refresh(build: { await build(0) }, publish: { published.append($0) })
    }
    while release == nil { await Task.yield() }
    let requests = (1...50).map { value in
        Task { @MainActor in
            latest = value
            submitted += 1
            await projector.refresh(build: { await build(value) }, publish: { published.append($0) })
        }
    }
    while submitted < 50 { await Task.yield() }
    #expect(builds == 1)
    #expect(published.isEmpty)
    release?.resume()
    await first.value
    for request in requests { await request.value }
    #expect(builds == 2)
    #expect(maximumActive == 1)
    #expect(published == [latest])

    // A completed flight must not swallow the next request.
    await projector.refresh(build: { await build(99) }, publish: { published.append($0) })
    #expect(builds == 3)
    #expect(published == [latest, 99])
}
