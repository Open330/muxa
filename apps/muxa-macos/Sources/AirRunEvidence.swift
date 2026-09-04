import Foundation

/// What Muxa.app watched a Work do, in the only form it may leave the app in.
///
/// **Metadata only, by construction.** This type has no field that can hold a
/// prompt, an agent's output, a recap, a title, a file's contents or a command
/// line — so the trace exporter, which sees nothing but this, cannot carry one
/// out. That is the whole point of the artifact, so it is a property of the
/// shape rather than a flag somebody could flip later: adding such a field
/// here is the change a reviewer would have to make on purpose.
struct AirRunEvidence: Equatable, Sendable {
    /// One agent in the Work, as the Work console already shows it.
    struct Participant: Equatable, Sendable {
        let alias: String
        let role: String
        /// The agent CLI muxa started: "claude", "codex", "gemini", …
        let program: String
        /// muxad's own agent state ("working", "waiting_input", "stopped", …).
        let state: String
        /// The pipeline run's status for this alias ("done", "blocked", …).
        let status: String
        /// Which launch stage the `after` graph put this agent in, 1-based.
        let stage: Int
        let host: String
        let pane: String
        /// ISO-8601, exactly as muxad reported it; empty when unrecorded.
        let startedAt: String
        let stateEnteredAt: String
        let lastActivityAt: String
        let after: [String]

        init(
            alias: String,
            role: String = "",
            program: String,
            state: String = "",
            status: String = "",
            stage: Int = 1,
            host: String = "",
            pane: String = "",
            startedAt: String = "",
            stateEnteredAt: String = "",
            lastActivityAt: String = "",
            after: [String] = []
        ) {
            self.alias = alias
            self.role = role
            self.program = program
            self.state = state
            self.status = status
            self.stage = stage
            self.host = host
            self.pane = pane
            self.startedAt = startedAt
            self.stateEnteredAt = stateEnteredAt
            self.lastActivityAt = lastActivityAt
            self.after = after
        }
    }

    let workspaceID: String
    let workID: String
    let pipeline: String
    let cwd: String
    let participants: [Participant]

    init(
        workspaceID: String,
        workID: String,
        pipeline: String,
        cwd: String,
        participants: [Participant]
    ) {
        self.workspaceID = workspaceID
        self.workID = workID
        self.pipeline = pipeline
        self.cwd = cwd
        self.participants = participants
    }

    /// The line-up the run followed, as a pipeline — what the trace's
    /// `workflow_content_digest` identifies. Built from the evidence alone,
    /// so it carries no prompt either.
    var lineUp: MuxaPipelineDefinition {
        MuxaPipelineDefinition(
            agents: participants.map {
                MuxaPipelineDefinition.Agent(
                    alias: $0.alias,
                    program: $0.program,
                    role: $0.role,
                    after: $0.after
                )
            }
        )
    }
}

/// muxa run evidence → AIR 1 `trace`.
///
/// AIR 1's native-run trace profile describes **one** provider CLI run: it
/// names a single `claude` or `codex` agent, its safety posture, and the exit
/// of its process. A muxa Work is several agent panes at once, so one Work
/// becomes one trace per participant, and the Work's identity, stage and
/// line-up travel in each trace's `x-muxa` extension so the set reassembles.
///
/// Three envelope fields the profile requires are things muxa never observed,
/// and each is written at the value that claims the least, with an `info`
/// diagnostic in the artifact saying so:
///
/// * `safety` — muxa does not set or read the provider's sandbox or
///   permission mode, so the block carries AIR's read-only values.
/// * `process` — muxa watches a pane, not a process exit: no exit code, no
///   signal, no captured output.
/// * `terminal` — therefore always `truncated`/`partial`. A trace this
///   exporter writes can never claim a run completed.
enum AirTrace {
    struct Export: Equatable, Sendable {
        let fileName: String
        let document: AirDocument
    }

    struct Skipped: Equatable, Sendable {
        let alias: String
        let reason: String
    }

    struct Result: Equatable, Sendable {
        let exports: [Export]
        let skipped: [Skipped]
    }

    /// AIR 1 traces name a provider from a closed list. A pane running
    /// something else is left out by name rather than relabelled.
    static let describableProviders = ["claude", "codex"]

    static func exports(for evidence: AirRunEvidence, producer: String) -> Result {
        let workflowDigest = AirWorkflow.document(
            name: AirWorkflow.Step.slug(evidence.pipeline).isEmpty
                ? "work" : AirWorkflow.Step.slug(evidence.pipeline),
            evidence.lineUp,
            producer: producer
        ).contentDigest

        var exports: [Export] = []
        var skipped: [Skipped] = []
        var used = Set<String>()
        for participant in evidence.participants {
            let provider = participant.program.trimmingCharacters(in: .whitespaces).lowercased()
            guard describableProviders.contains(provider) else {
                skipped.append(Skipped(
                    alias: participant.alias,
                    reason: String(
                        localized: "AIR 1 traces describe a claude or codex run; @\(participant.alias) runs \(participant.program)."
                    )
                ))
                continue
            }
            var name = "\(slug(evidence.workID))-\(slug(participant.alias))"
            while !used.insert(name).inserted { name += "-\(exports.count + 1)" }
            exports.append(Export(
                fileName: "\(name).air.json",
                document: document(
                    evidence,
                    participant,
                    provider: provider,
                    workflowDigest: workflowDigest,
                    producer: producer
                )
            ))
        }
        return Result(exports: exports, skipped: skipped)
    }

    // MARK: - One participant

    private static func document(
        _ evidence: AirRunEvidence,
        _ participant: AirRunEvidence.Participant,
        provider: String,
        workflowDigest: String,
        producer: String
    ) -> AirDocument {
        let events = self.events(evidence, participant)
        let ids = events.compactMap { $0["id"]?.stringValue }
        var edges: [AirJSON] = []
        for index in 1..<max(ids.count, 1) {
            edges.append(.object([
                "id": .string("edge-\(ids[index - 1])-\(ids[index])"),
                "from": .string(ids[index - 1]),
                "to": .string(ids[index]),
                "kind": .string("temporal"),
                "assertion": .string("inferred"),
                "confidence": .object([
                    "level": .string("structural"),
                    "rule_id": .string("muxa.observation-order"),
                    "reason": .string("Ordered by when muxa recorded each observation."),
                ]),
                "evidence_refs": .array([]),
            ]))
        }

        let body = AirJSON.object([
            "workflow_content_digest": .string(workflowDigest),
            "plan_content_digest": .string(AirDocument.Spec.emptyDigest),
            "agent": .string(provider),
            "cwd": .object([
                "display": .string(evidence.cwd.isEmpty ? "(not recorded)" : evidence.cwd),
                "disclosure": .string("local-only"),
            ]),
            "safety": safety(provider),
            "adapter": .object(["id": .string("muxa"), "version": .string(producer)]),
            "events": .array(events),
            "event_graph": .object([
                "entry_event_ids": .array(ids.prefix(1).map(AirJSON.string)),
                "nodes": .array(ids.map(AirJSON.string)),
                "edges": .array(edges),
            ]),
            "process": .object([
                "exit_code": .null,
                "signal": .null,
                "stderr": .object([
                    "encoding": .string("base64"),
                    "bytes_base64": .string(""),
                    "byte_length": .int(0),
                    "sha256": .string(AirDocument.Spec.emptyDigest),
                ]),
                "stdout_bytes": .int(0),
            ]),
            "terminal": .object([
                "status": .string("truncated"),
                "completeness": .string("partial"),
            ]),
            "diagnostics": .array(disclaimers),
            "hidden_reasoning_recovered": .bool(false),
        ])

        return AirDocument.envelope(
            kind: "trace",
            profile: AirDocument.Spec.traceProfile,
            body: body,
            provenance: .object([
                "created_by": .object([
                    "name": .string("muxa"),
                    "version": .string(producer),
                ]),
                "origins": .array([.object([
                    "kind": .string("session-store"),
                    "format": .string("muxa-work"),
                    "version": .string("1"),
                    "digest": .string(AirDocument.sha256(Data(
                        "\(evidence.workspaceID)/\(evidence.workID)/\(participant.alias)".utf8
                    ))),
                    "locator": .object([
                        "display": .string("muxa work \(evidence.workID) @\(participant.alias)"),
                        "disclosure": .string("local-only"),
                    ]),
                ])]),
                "derived_from": .array([]),
                "migrations": .array([]),
            ]),
            extensions: [AirDocument.Spec.muxaExtension: extensionPayload(evidence, participant)]
        )
    }

    /// The observations themselves, in the order muxa made them. Each event's
    /// `source` is the metadata the Work console shows beside that row.
    private static func events(
        _ evidence: AirRunEvidence,
        _ participant: AirRunEvidence.Participant
    ) -> [AirJSON] {
        var events: [(type: String, status: String, source: [String: AirJSON])] = []
        events.append((
            "muxa.work.observed",
            "observed",
            [
                "workspace": .string(evidence.workspaceID),
                "work": .string(evidence.workID),
                "pipeline": .string(evidence.pipeline),
                "alias": .string(participant.alias),
                "host": .string(participant.host),
                "pane": .string(participant.pane),
            ]
        ))
        if !participant.startedAt.isEmpty {
            events.append(("muxa.agent.started", "observed", ["at": .string(participant.startedAt)]))
        }
        events.append((
            "muxa.pipeline.stage",
            participant.status.isEmpty ? "unreported" : participant.status,
            [
                "stage": .int(participant.stage),
                "waits_for": .array(participant.after.map(AirJSON.string)),
            ]
        ))
        events.append((
            "muxa.agent.state",
            participant.state.isEmpty ? "unreported" : participant.state,
            ["at": .string(participant.stateEnteredAt), "role": .string(participant.role)]
        ))
        if !participant.lastActivityAt.isEmpty {
            events.append((
                "muxa.agent.activity",
                "observed",
                ["at": .string(participant.lastActivityAt)]
            ))
        }
        return events.enumerated().map { order, event in
            .object([
                "id": .string("event-\(order)"),
                "order": .int(order),
                "type": .string(event.type),
                "status": .string(event.status),
                "assertion": .string("observed"),
                "confidence": .object([
                    "level": .string("explicit"),
                    "rule_id": .string("muxa.daemon-observation"),
                    "reason": .string("Recorded by the muxa daemon and shown in Muxa.app."),
                ]),
                "evidence_refs": .array([]),
                "source": .object(event.source),
            ])
        }
    }

    /// AIR 1 requires a safety block naming the provider's own posture. muxa
    /// neither sets nor reads it, so these are the profile's least-privilege
    /// values, and the diagnostics below say they are not a measurement.
    private static func safety(_ provider: String) -> AirJSON {
        provider == "codex"
            ? .object([
                "intent": .string("read-only"),
                "provider": .string("codex"),
                "sandbox": .string("read-only"),
                "boundary": .string("os-sandbox"),
            ])
            : .object([
                "intent": .string("read-only"),
                "provider": .string("claude"),
                "permission_mode": .string("plan"),
                "boundary": .string("tool-permission-policy-not-os-sandbox"),
            ])
    }

    /// Written into every trace, so the three fields muxa could not observe
    /// are never mistaken for measurements. English, like the rest of the
    /// artifact: an exported file must read the same in every install.
    private static let disclaimers: [AirJSON] = [
        .object([
            "severity": .string("info"),
            "code": .string("AIR_MUXA_PROCESS_UNOBSERVED"),
            "message": .string(
                "muxa observes an agent's pane, not the agent process's exit. "
                    + "The process record carries no exit code, signal or output, and the "
                    + "terminal record says only that this evidence is partial."
            ),
            "targets": .array([]),
        ]),
        .object([
            "severity": .string("info"),
            "code": .string("AIR_MUXA_SAFETY_UNOBSERVED"),
            "message": .string(
                "muxa neither sets nor reads the provider's sandbox or permission mode. "
                    + "The safety block carries AIR 1's read-only values because the profile "
                    + "requires one, not because a posture was measured."
            ),
            "targets": .array([]),
        ]),
        .object([
            "severity": .string("info"),
            "code": .string("AIR_MUXA_PLAN_ABSENT"),
            "message": .string(
                "This run was launched from a muxa pipeline rather than an approved AIR plan, "
                    + "so the plan digest is the digest of no bytes."
            ),
            "targets": .array([]),
        ]),
    ]

    private static func extensionPayload(
        _ evidence: AirRunEvidence,
        _ participant: AirRunEvidence.Participant
    ) -> AirJSON {
        .object([
            "version": .int(1),
            "work": .object([
                "workspace": .string(evidence.workspaceID),
                "id": .string(evidence.workID),
                "pipeline": .string(evidence.pipeline),
                "cwd": .string(evidence.cwd),
            ]),
            "participant": member(participant),
            "line_up": .array(evidence.participants.map(member)),
        ])
    }

    private static func member(_ participant: AirRunEvidence.Participant) -> AirJSON {
        .object([
            "alias": .string(participant.alias),
            "role": .string(participant.role),
            "program": .string(participant.program),
            "state": .string(participant.state),
            "status": .string(participant.status),
            "stage": .int(participant.stage),
            "host": .string(participant.host),
            "pane": .string(participant.pane),
            "started_at": .string(participant.startedAt),
            "state_entered_at": .string(participant.stateEnteredAt),
            "last_activity_at": .string(participant.lastActivityAt),
            "after": .array(participant.after.map(AirJSON.string)),
        ])
    }

    private static func slug(_ value: String) -> String {
        let cleaned = AirWorkflow.Step.slug(value)
        return cleaned.isEmpty ? "work" : cleaned
    }
}
