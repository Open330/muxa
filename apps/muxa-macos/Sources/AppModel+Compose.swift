import Foundation

/// What the composer sheet drafts for: the host whose config the result is
/// saved into (nil is this Mac's library).
struct MuxaPipelineComposerTarget: Identifiable, Equatable, Sendable {
    let host: String?

    var id: String { host ?? "local" }
}

/// Presentation state for the composer sheet. `@Published` state cannot live
/// in an `AppModel` extension, so this object holds it: ContentView observes
/// it and presents `PipelineComposerView` for `target`.
@MainActor
final class PipelineComposerPresenter: ObservableObject {
    static let shared = PipelineComposerPresenter()

    @Published var target: MuxaPipelineComposerTarget?
    /// The pre-composer path (`muxa work init` in a Shell tab), offered when
    /// neither the daemon nor the bundled CLI can draft. Set by the caller
    /// that knows how to close its own sheet first.
    var shellFallback: (() -> Void)?

    func dismiss() {
        target = nil
        shellFallback = nil
    }
}

/// One in-app drafting conversation: the description, the provider, the
/// draft the model returned, and every refinement applied to it. Owned by
/// the composer sheet; talks to muxad through the closures it is built with
/// so tests can drive it without a daemon.
@MainActor
final class PipelineComposerSession: ObservableObject {
    enum Phase: Equatable, Sendable {
        case idle
        case drafting
        case ready
        case error(String)
    }

    /// Which path answers a draft request.
    enum Backend: Equatable, Sendable {
        /// Not probed yet.
        case checking
        /// `work_compose` through muxad.
        case daemon
        /// `muxa work compose --json` from the app bundle; the running
        /// daemon predates in-app drafting.
        case bundledCLI
        /// Neither; only the Shell-tab wizard remains.
        case unavailable
    }

    struct Provider: Identifiable, Equatable, Sendable {
        let id: String
        let title: String
    }

    struct Refinement: Identifiable, Equatable, Sendable {
        let id: UUID
        let request: String
        let notes: String

        init(request: String, notes: String) {
            id = UUID()
            self.request = request
            self.notes = notes
        }
    }

    /// One compose request as handed to the backend.
    struct Request: Equatable, Sendable {
        let description: String
        let agent: String?
        let current: MuxaPipelineDefinition?
        let name: String?
        let credential: MuxaWorkComposeCredential?
    }

    typealias Composer = @Sendable (Request) async throws -> MuxaWorkComposeResult
    typealias BackendProbe = @Sendable () async -> Backend
    /// nil when the daemon has no provider list; the built-in pair stays.
    typealias ProviderLoader = @Sendable () async -> [Provider]?
    typealias CredentialLookup = @Sendable (String) -> MuxaWorkComposeCredential?

    let host: String?

    @Published var description = ""
    @Published var providerID: String
    @Published private(set) var providers: [Provider]
    @Published private(set) var phase: Phase = .idle
    @Published var draft: MuxaPipelineDefinition?
    @Published var name = ""
    @Published private(set) var notes = ""
    @Published var refinement = ""
    @Published private(set) var history: [Refinement] = []
    @Published private(set) var backend: Backend

    private let composer: Composer
    private let backendProbe: BackendProbe?
    private let providerLoader: ProviderLoader?
    private let credentialLookup: CredentialLookup
    private var task: Task<Void, Never>?
    private var generation = 0

    static var builtInProviders: [Provider] {
        [
            Provider(id: "claude", title: String(localized: "Claude Code")),
            Provider(id: "codex", title: String(localized: "Codex")),
        ]
    }

    static let examples: [String] = [
        String(localized: "An implementer in Claude, then a Codex reviewer that only reads the tree"),
        String(localized: "Three agents: planner, implementer, tester; tester runs after both"),
        String(localized: "Solo Claude that runs the test suite before reporting done"),
    ]

    init(
        host: String?,
        defaultProvider: String,
        providers: [Provider]? = nil,
        backend: Backend = .checking,
        composer: @escaping Composer,
        backendProbe: BackendProbe? = nil,
        providerLoader: ProviderLoader? = nil,
        credentialLookup: @escaping CredentialLookup = { _ in nil }
    ) {
        self.host = host
        self.composer = composer
        self.backendProbe = backendProbe
        self.providerLoader = providerLoader
        self.credentialLookup = credentialLookup
        self.backend = backend
        let list = providers ?? Self.builtInProviders
        self.providers = Self.including(defaultProvider, in: list)
        providerID = defaultProvider.isEmpty ? (list.first?.id ?? "claude") : defaultProvider
    }

    // MARK: Derived state

    var isDrafting: Bool { phase == .drafting }
    var hasDraft: Bool { draft != nil }

    var errorMessage: String? {
        if case let .error(message) = phase { return message }
        return nil
    }

    var providerTitle: String {
        providers.first { $0.id == providerID }?.title ?? providerID
    }

    var trimmedDescription: String {
        description.trimmingCharacters(in: .whitespacesAndNewlines)
    }

    var trimmedName: String { name.trimmingCharacters(in: .whitespaces) }

    var canDraft: Bool {
        backend != .unavailable && backend != .checking && !isDrafting && !trimmedDescription.isEmpty
    }

    var canRefine: Bool {
        hasDraft && !isDrafting && backend != .unavailable
            && !refinement.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    /// Problems the CLI would refuse, name first, the way the editor lists them.
    var problems: [String] {
        guard let draft else { return [] }
        var problems = draft.problems()
        if !MuxaPipelineDefinition.isValidName(trimmedName) {
            problems.insert(String(localized: "Name may only use letters, digits, - and _."), at: 0)
        }
        return problems
    }

    var canSave: Bool { hasDraft && !isDrafting && problems.isEmpty }

    /// The draft as a library pipeline, for the editor and the stage picture.
    var draftAsPipeline: MuxaWorkOptions.Pipeline? {
        guard let draft else { return nil }
        return MuxaWorkOptions.Pipeline(
            name: trimmedName,
            description: draft.description.isEmpty ? nil : draft.description,
            layout: draft.layout.isEmpty ? nil : draft.layout,
            prompt: draft.prompt.isEmpty ? nil : draft.prompt,
            agents: draft.optionsAgents
        )
    }

    // MARK: Actions

    /// Probes which backend answers and refreshes the provider list. Safe to
    /// call more than once; the sheet runs it from `.task`.
    func prepare() async {
        if let backendProbe {
            let probed = await backendProbe()
            if backend != probed { backend = probed }
        } else if backend == .checking {
            backend = .daemon
        }
        if let providerLoader, let loaded = await providerLoader() {
            setProviders(loaded)
        }
    }

    func setProviders(_ loaded: [Provider]) {
        let list = loaded.isEmpty ? Self.builtInProviders : loaded
        providers = Self.including(providerID, in: list)
    }

    func useExample(_ example: String) {
        description = example
    }

    /// First draft from the description; a later call replaces the draft.
    func draftPipeline() {
        guard canDraft else { return }
        submit(text: trimmedDescription, current: nil, refinementRequest: nil)
    }

    /// Asks for a change to the current draft; the draft rides along as
    /// `current` so the model edits instead of starting over.
    func refine() {
        guard canRefine, let draft else { return }
        let text = refinement.trimmingCharacters(in: .whitespacesAndNewlines)
        submit(text: text, current: draft, refinementRequest: text)
    }

    /// Stops waiting for the provider. The exchange in flight may still
    /// finish on the daemon; its answer is dropped.
    func cancel() {
        guard isDrafting else { return }
        generation += 1
        task?.cancel()
        task = nil
        phase = hasDraft ? .ready : .idle
    }

    /// Waits for the request in flight, if any. Tests use it; the sheet
    /// observes `phase` instead.
    func awaitPendingRequest() async {
        await task?.value
    }

    private func submit(text: String, current: MuxaPipelineDefinition?, refinementRequest: String?) {
        generation += 1
        let ticket = generation
        phase = .drafting
        let request = Request(
            description: text,
            agent: providerID,
            current: current,
            name: current == nil ? nil : trimmedName,
            credential: credentialLookup(providerID)
        )
        let composer = self.composer
        task = Task { [weak self] in
            do {
                let result = try await composer(request)
                guard let self, self.generation == ticket else { return }
                self.apply(result, refinementRequest: refinementRequest)
            } catch {
                guard let self, self.generation == ticket else { return }
                self.phase = .error(Self.describe(error))
            }
        }
    }

    private func apply(_ result: MuxaWorkComposeResult, refinementRequest: String?) {
        draft = result.definition
        let proposed = result.pipeline.name.trimmingCharacters(in: .whitespaces)
        if refinementRequest == nil || trimmedName.isEmpty || !MuxaPipelineDefinition.isValidName(trimmedName) {
            name = proposed
        }
        notes = result.notes.trimmingCharacters(in: .whitespacesAndNewlines)
        if let refinementRequest {
            history.append(Refinement(request: refinementRequest, notes: notes))
            refinement = ""
        } else {
            history = []
        }
        phase = .ready
    }

    private static func describe(_ error: Error) -> String {
        if error is CancellationError {
            return String(localized: "Drafting was cancelled.")
        }
        return error.localizedDescription
    }

    private static func including(_ id: String, in list: [Provider]) -> [Provider] {
        guard !id.isEmpty, !list.contains(where: { $0.id == id }) else { return list }
        return list + [Provider(id: id, title: id)]
    }
}

extension AppModel {
    /// The composer's presentation state; ContentView presents its sheet.
    var pipelineComposer: PipelineComposerPresenter { .shared }

    /// Opens the in-app composer for `host`'s library. `shellFallback` is
    /// the caller's way to the `muxa work init` wizard (closing its own
    /// sheet first); without it the composer opens the wizard directly.
    func presentPipelineComposer(host: String?, shellFallback: (() -> Void)? = nil) {
        let presenter = pipelineComposer
        presenter.shellFallback = shellFallback
        presenter.target = MuxaPipelineComposerTarget(host: isLocalHost(host) ? nil : host)
    }

    func dismissPipelineComposer() {
        pipelineComposer.dismiss()
    }

    /// Opens the visual editor on an unsaved draft: the name stays editable
    /// and saving creates the pipeline.
    func presentPipelineEditor(draft: MuxaWorkOptions.Pipeline, host: String?) {
        presentPipelineEditor(host: host, pipeline: draft)
        pipelineEditorTarget = MuxaPipelineEditorTarget(
            host: isLocalHost(host) ? nil : host,
            pipeline: draft,
            isDraft: true
        )
    }

    /// Falls back to the Shell-tab wizard when drafting is unavailable.
    func openDescribeInShell() {
        if let fallback = pipelineComposer.shellFallback {
            fallback()
        } else {
            Task { await configureWork(cwd: nil) }
        }
    }

    /// The session the composer sheet owns. Drafting goes through muxad's
    /// `work_compose` when it advertises it, else through the bundled CLI's
    /// `muxa work compose`, which asks the provider itself.
    func makePipelineComposerSession(host: String?) -> PipelineComposerSession {
        let client = self.client
        let bundledCLI = Self.bundledMuxaCLI()
        let composer: PipelineComposerSession.Composer = { request in
            if await client.supports(MuxaIPCClient.workComposeCapability) {
                return try await client.composeWork(
                    description: request.description,
                    agent: request.agent,
                    current: request.current,
                    name: request.name,
                    credential: request.credential
                )
            }
            guard let bundledCLI else {
                throw MuxaIPCError.server(String(localized: "Update muxad to draft pipelines"))
            }
            return try await Self.composeWithBundledCLI(
                bundledCLI,
                socketPath: client.socketPath,
                request: request
            )
        }
        let probe: PipelineComposerSession.BackendProbe = {
            if await client.supports(MuxaIPCClient.workComposeCapability) { return .daemon }
            return bundledCLI == nil ? .unavailable : .bundledCLI
        }
        let loader: PipelineComposerSession.ProviderLoader = {
            guard let listed = try? await client.workComposeProviders() else { return nil }
            return listed.map { PipelineComposerSession.Provider(id: $0.id, title: $0.title) }
        }
        return PipelineComposerSession(
            host: isLocalHost(host) ? nil : host,
            defaultProvider: askAgent,
            composer: composer,
            backendProbe: probe,
            providerLoader: loader,
            credentialLookup: { Self.composeCredential(for: $0) }
        )
    }

    /// The Keychain key for a provider, the way `sendAsk` attaches it.
    nonisolated static func composeCredential(for agent: String) -> MuxaWorkComposeCredential? {
        guard let provider = MuxaAskProvider(rawValue: agent),
              let key = MuxaProviderCredentialStore.key(for: provider) else { return nil }
        return MuxaWorkComposeCredential(agent: agent, apiKey: key)
    }

    /// The `muxa` CLI embedded in the app bundle, if the build embedded one.
    nonisolated static func bundledMuxaCLI() -> URL? {
        let bundled = Bundle.main.bundleURL
            .appendingPathComponent("Contents", isDirectory: true)
            .appendingPathComponent("Helpers", isDirectory: true)
            .appendingPathComponent("muxa")
        return FileManager.default.isExecutableFile(atPath: bundled.path) ? bundled : nil
    }

    /// `muxa work compose "<description>" --json [--agent id] [--current -]`;
    /// the current draft goes in on stdin.
    nonisolated static func composeCLIArguments(for request: PipelineComposerSession.Request) -> [String] {
        var arguments = ["work", "compose", request.description, "--json"]
        if let agent = request.agent, !agent.isEmpty { arguments += ["--agent", agent] }
        if request.current != nil { arguments += ["--current", "-"] }
        return arguments
    }

    /// The environment variable a provider reads its API key from; the
    /// bundled CLI has no credential argument, so the Keychain key is
    /// handed over the way a terminal would.
    nonisolated static func composeCredentialEnvironmentKey(for agent: String) -> String? {
        switch agent {
        case "claude", "anthropic": "ANTHROPIC_API_KEY"
        case "codex": "CODEX_API_KEY"
        case "openai": "OPENAI_API_KEY"
        case "gemini": "GEMINI_API_KEY"
        default: nil
        }
    }

    nonisolated private static func composeWithBundledCLI(
        _ executable: URL,
        socketPath: String,
        request: PipelineComposerSession.Request
    ) async throws -> MuxaWorkComposeResult {
        let arguments = composeCLIArguments(for: request)
        let input: String? = try request.current.map { current in
            let object = try MuxaWorkComposeClient.currentPayload(current, name: request.name)
            return String(decoding: try JSONSerialization.data(withJSONObject: object), as: UTF8.self)
        }
        var environment = MuxaProviderCredentialStore.augmentPath(ProcessInfo.processInfo.environment)
        environment["MUXA_SOCKET"] = socketPath
        if let credential = request.credential,
           let key = composeCredentialEnvironmentKey(for: credential.agent),
           environment[key]?.isEmpty != false {
            environment[key] = credential.apiKey
        }
        let resolvedEnvironment = environment
        return try await Task.detached {
            let process = Process()
            process.executableURL = executable
            process.arguments = arguments
            process.environment = resolvedEnvironment
            let output = Pipe()
            let errors = Pipe()
            process.standardOutput = output
            process.standardError = errors
            let stdin = Pipe()
            process.standardInput = input == nil ? FileHandle.nullDevice : stdin
            try process.run()
            if let input {
                stdin.fileHandleForWriting.write(Data(input.utf8))
                try? stdin.fileHandleForWriting.close()
            }
            let standardOutput = output.fileHandleForReading.readDataToEndOfFile()
            let standardError = errors.fileHandleForReading.readDataToEndOfFile()
            process.waitUntilExit()
            guard process.terminationStatus == 0 else {
                let reason = String(decoding: standardError, as: UTF8.self)
                    .trimmingCharacters(in: .whitespacesAndNewlines)
                let fallback = String(decoding: standardOutput, as: UTF8.self)
                    .trimmingCharacters(in: .whitespacesAndNewlines)
                let detail = reason.isEmpty ? fallback : reason
                throw MuxaIPCError.server(
                    detail.isEmpty ? "muxa work compose exited with \(process.terminationStatus)" : detail
                )
            }
            return try MuxaWorkComposeResult.decode(standardOutput)
        }.value
    }
}
