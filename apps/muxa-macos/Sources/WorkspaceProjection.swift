import Foundation

/// At most one projection is built at a time. A burst replaces the pending
/// input, and an outdated result is never published. All callers await the
/// latest result, including requests that arrived during the current build.
@MainActor
final class MuxaCoalescedProjection<Value: Sendable> {
    private struct Request {
        let build: @MainActor () async -> Value
        let publish: @MainActor (Value) -> Void
    }
    private var pending: Request?
    private var task: Task<Void, Never>?

    func refresh(build: @escaping @MainActor () async -> Value,
                 publish: @escaping @MainActor (Value) -> Void) async {
        pending = Request(build: build, publish: publish)
        if let task { await task.value; return }
        let worker = Task { @MainActor [self] in
            while let request = pending {
                pending = nil
                let value = await request.build()
                if pending == nil { request.publish(value) }
            }
            task = nil
        }
        task = worker
        await worker.value
    }
}

struct MuxaWorkspaceProjection: Sendable {
    let agents: [MuxaHostedAgent]
    let groups: [MuxaWorkGroup]
    let works: [FleetWork]
    let builtAt: Date
    let connection: UInt64
}
