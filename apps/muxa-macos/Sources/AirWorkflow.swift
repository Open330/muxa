import Foundation

/// muxa pipeline ⇄ AIR 1 `workflow`.
///
/// An AIR workflow is not a free graph: its authoritative content is a
/// Markdown Skill document, and the graph must be re-derivable from those
/// bytes — Workbench reparses the source on open and refuses an artifact
/// whose graph disagrees with it. So the converter *renders* the pipeline as
/// Markdown and then describes that rendering exactly: one `### @alias`
/// heading per agent, byte ranges that partition the file, and a
/// `workflow-studio:managed` footer that restores muxa's real `after` graph
/// in place of the linear chain plain headings would imply.
///
/// The Markdown is a rendering for foreign readers. Everything muxa runs on
/// and AIR does not model — alias, program, role, task, direction, layout and
/// the prompts — travels verbatim in the `x-muxa` extension, so a round trip
/// loses nothing even though the prose is sanitized to stay parseable.
///
/// The rendering is deliberately *not* localized: an exported artifact must
/// be the same bytes whatever language the app is running in, because its
/// identity is a digest over those bytes.
enum AirWorkflow {
    // MARK: - Export

    static func document(
        name: String,
        _ definition: MuxaPipelineDefinition,
        producer: String
    ) -> AirDocument {
        let steps = Step.plan(definition.agents)
        let source = markdown(name: name, definition, steps: steps)
        let bytes = source.bytes

        var nodes: [AirJSON] = []
        var maps: [AirJSON] = []
        for (order, step) in steps.enumerated() {
            let span = source.spans[order]
            nodes.append(.object([
                "id": .string(step.nodeID),
                "kind": .string("step"),
                "order": .int(order),
                "title": .string(step.title),
                "body": .string(source.bodyText(order)),
                "assertion": .string("declared"),
                "confidence": confidence("Stable identity restored from Workflow Studio metadata."),
                "evidence_refs": .array([]),
            ]))
            maps.append(.object([
                "node_id": .string(step.nodeID),
                "source_id": .string(sourceID),
                "span": range(span.span),
                "heading": range(span.heading),
                "title": range(span.title),
                "body": range(span.body),
            ]))
        }

        var edges: [AirJSON] = []
        for edge in Step.edges(steps) {
            edges.append(.object([
                "id": .string(edge.id),
                "from": .string(edge.from),
                "to": .string(edge.to),
                "kind": .string("sequence"),
                "assertion": .string("declared"),
                "confidence": confidence("Edge restored from Workflow Studio metadata."),
                "evidence_refs": .array([]),
            ]))
        }

        var opaque: [AirJSON] = []
        if let first = source.spans.first, first.span.lowerBound > 0 {
            opaque.append(opaqueRange(0..<first.span.lowerBound, in: bytes))
        }
        if source.footerStart < bytes.count {
            opaque.append(opaqueRange(source.footerStart..<bytes.count, in: bytes))
        }
        if steps.isEmpty {
            opaque = [opaqueRange(0..<bytes.count, in: bytes)]
        }

        let body = AirJSON.object([
            "source": .object([
                "source_id": .string(sourceID),
                "media_type": .string("text/markdown"),
                "encoding": .string("utf-8"),
                "bytes_base64": .string(bytes.base64EncodedString()),
                "byte_length": .int(bytes.count),
                "sha256": .string(AirDocument.sha256(bytes)),
                "newline": .string("lf"),
                "final_newline": .bool(true),
                "locator": .object([
                    "display": .string("config.toml [pipeline.\(name)]"),
                    "disclosure": .string("local-only"),
                ]),
            ]),
            "graph": .object([
                "entry_node_ids": .array(Step.entryIDs(steps).map(AirJSON.string)),
                "nodes": .array(nodes),
                "edges": .array(edges),
            ]),
            "source_maps": .array(maps),
            "opaque_ranges": .array(opaque),
            "diagnostics": .array([]),
        ])

        let pipelineJSON = (try? definition.jsonString()) ?? ""
        return AirDocument.envelope(
            kind: "workflow",
            profile: AirDocument.Spec.workflowProfile,
            body: body,
            provenance: .object([
                "created_by": .object([
                    "name": .string("muxa"),
                    "version": .string(producer),
                ]),
                "origins": .array([.object([
                    "kind": .string("source"),
                    "format": .string("muxa-pipeline"),
                    "version": .string("1"),
                    "digest": .string(AirDocument.sha256(Data(pipelineJSON.utf8))),
                    "locator": .object([
                        "display": .string("config.toml [pipeline.\(name)]"),
                        "disclosure": .string("local-only"),
                    ]),
                ])]),
                "derived_from": .array([]),
                "migrations": .array([]),
            ]),
            extensions: [AirDocument.Spec.muxaExtension: extensionPayload(
                name: name,
                definition,
                steps: steps
            )]
        )
    }

    // MARK: - Import

    /// Reads an AIR workflow back into a pipeline. A foreign document — one
    /// with no `x-muxa` payload, or with somebody else's extensions beside
    /// it — still yields the fields muxa can see: the graph is the line-up,
    /// the titles are the aliases, and the programs fall back to the editor's
    /// default. The result is offered to the pipeline editor only after
    /// `problems()` accepts it, because an AIR file is untrusted input.
    static func pipeline(from document: AirDocument) throws -> MuxaWorkOptions.Pipeline {
        guard document.kind == "workflow",
              document.profile == AirDocument.Spec.workflowProfile
        else {
            throw AirError.unsupported(kind: document.kind, profile: document.profile)
        }
        let nodes = document.body["graph"]?["nodes"]?.arrayValue ?? []
        let payload = document.extensions[AirDocument.Spec.muxaExtension]

        var definition = MuxaPipelineDefinition()
        var name = ""
        if let payload, payload["version"]?.intValue == 1 {
            let pipeline = payload["pipeline"]
            name = pipeline?["name"]?.stringValue ?? ""
            definition.description = pipeline?["description"]?.stringValue ?? ""
            definition.layout = pipeline?["layout"]?.stringValue ?? ""
            definition.prompt = pipeline?["prompt"]?.stringValue ?? ""
            definition.agents = (payload["steps"]?.arrayValue ?? []).map { step in
                MuxaPipelineDefinition.Agent(
                    alias: step["alias"]?.stringValue ?? "",
                    program: step["program"]?.stringValue ?? "claude",
                    role: step["role"]?.stringValue ?? "",
                    task: step["task"]?.stringValue ?? "",
                    prompt: step["prompt"]?.stringValue ?? "",
                    direction: step["direction"]?.stringValue ?? "",
                    after: (step["after"]?.arrayValue ?? []).compactMap(\.stringValue)
                )
            }
        }
        if definition.agents.isEmpty {
            definition.agents = agentsFromGraph(document, nodes: nodes)
        }
        if name.isEmpty { name = derivedName(from: document) }

        let problems = definition.problems()
        guard problems.isEmpty else { throw AirError.rejected(problems) }
        return MuxaWorkOptions.Pipeline(
            name: name,
            description: definition.description.isEmpty ? nil : definition.description,
            layout: definition.layout.isEmpty ? nil : definition.layout,
            prompt: definition.prompt.isEmpty ? nil : definition.prompt,
            agents: definition.optionsAgents
        )
    }

    /// The line-up a reader with no muxa extension can still see: one agent
    /// per step, in graph order, waiting on whoever points at it.
    private static func agentsFromGraph(
        _ document: AirDocument,
        nodes: [AirJSON]
    ) -> [MuxaPipelineDefinition.Agent] {
        var aliasByNode: [String: String] = [:]
        var order: [(id: String, alias: String)] = []
        var used = Set<String>()
        for (index, node) in nodes.enumerated() {
            let id = node["id"]?.stringValue ?? "step-\(index + 1)"
            let title = node["title"]?.stringValue ?? ""
            var alias = Step.slug(title.hasPrefix("@") ? String(title.dropFirst()) : title)
            if alias.isEmpty { alias = "step-\(index + 1)" }
            while !used.insert(alias).inserted { alias += "-\(index + 1)" }
            aliasByNode[id] = alias
            order.append((id, alias))
        }
        var after: [String: [String]] = [:]
        for edge in document.body["graph"]?["edges"]?.arrayValue ?? [] {
            guard let from = edge["from"]?.stringValue.flatMap({ aliasByNode[$0] }),
                  let to = edge["to"]?.stringValue,
                  let target = aliasByNode[to],
                  from != target
            else { continue }
            if after[target]?.contains(from) != true { after[target, default: []].append(from) }
        }
        return order.map {
            MuxaPipelineDefinition.Agent(alias: $0.alias, after: after[$0.alias] ?? [])
        }
    }

    /// A name for a document that carries no muxa payload. A Skill names
    /// itself in its front matter, which is what muxa writes too, so that is
    /// read before falling back to the file AIR recorded as its source.
    private static func derivedName(from document: AirDocument) -> String {
        let encoded = document.body["source"]?["bytes_base64"]?.stringValue ?? ""
        let markdown = String(
            decoding: Data(base64Encoded: encoded) ?? Data(),
            as: UTF8.self
        )
        if let name = frontMatterName(markdown), !Step.slug(name).isEmpty {
            return Step.slug(name)
        }
        let display = document.body["source"]?["locator"]?["display"]?.stringValue ?? ""
        var candidate = (display as NSString).lastPathComponent
        while let dot = candidate.firstIndex(of: "."), dot != candidate.startIndex {
            candidate = String(candidate[candidate.startIndex..<dot])
        }
        let slug = Step.slug(candidate)
        return slug.isEmpty ? "imported" : slug
    }

    /// `name:` from a leading `---` block, unquoted. Deliberately small: this
    /// is a courtesy for naming an imported draft the operator will rename,
    /// not a YAML reader.
    private static func frontMatterName(_ markdown: String) -> String? {
        var lines = markdown.split(separator: "\n", omittingEmptySubsequences: false)
        guard lines.first?.trimmingCharacters(in: .whitespaces) == "---" else { return nil }
        lines.removeFirst()
        for line in lines {
            if line.trimmingCharacters(in: .whitespaces) == "---" { return nil }
            guard line.hasPrefix("name:") else { continue }
            var value = line.dropFirst("name:".count).trimmingCharacters(in: .whitespaces)
            if value.count > 1, value.hasPrefix("\""), value.hasSuffix("\"") {
                value = String(value.dropFirst().dropLast())
            }
            return value.isEmpty ? nil : value
        }
        return nil
    }

    // MARK: - The Markdown rendering

    private static let sourceID = "source-skill"

    /// One agent as the document sees it.
    struct Step: Equatable, Sendable {
        let nodeID: String
        let title: String
        let alias: String
        let agent: MuxaPipelineDefinition.Agent
        /// `after` entries that name an agent this pipeline actually has.
        let after: [String]

        static func plan(_ agents: [MuxaPipelineDefinition.Agent]) -> [Step] {
            let aliases = agents.map { $0.alias.trimmingCharacters(in: .whitespaces).lowercased() }
            var identifiers: [String] = []
            var used = Set<String>()
            for (index, alias) in aliases.enumerated() {
                var candidate = slug(alias)
                if candidate.isEmpty { candidate = "step-\(index + 1)" }
                var nodeID = "step-\(candidate)"
                while !used.insert(nodeID).inserted { nodeID = "step-\(candidate)-\(index + 1)" }
                identifiers.append(nodeID)
            }
            let known = Set(aliases.filter { !$0.isEmpty })
            return agents.enumerated().map { index, agent in
                var seen = Set<String>()
                let after = agent.after
                    .map { $0.trimmingCharacters(in: .whitespaces).lowercased() }
                    .filter { known.contains($0) && $0 != aliases[index] && seen.insert($0).inserted }
                return Step(
                    nodeID: identifiers[index],
                    title: title(aliases[index], index: index),
                    alias: aliases[index],
                    agent: agent,
                    after: after
                )
            }
        }

        struct Edge: Equatable, Sendable {
            let id: String
            let from: String
            let to: String
        }

        static func edges(_ steps: [Step]) -> [Edge] {
            let nodeByAlias = Dictionary(
                steps.filter { !$0.alias.isEmpty }.map { ($0.alias, $0.nodeID) },
                uniquingKeysWith: { first, _ in first }
            )
            var edges: [Edge] = []
            var pairs = Set<String>()
            for step in steps {
                for dependency in step.after {
                    guard let from = nodeByAlias[dependency], from != step.nodeID else { continue }
                    guard pairs.insert("\(from)\u{0}\(step.nodeID)").inserted else { continue }
                    edges.append(Edge(id: "edge-\(from)-\(step.nodeID)", from: from, to: step.nodeID))
                }
            }
            return edges
        }

        static func entryIDs(_ steps: [Step]) -> [String] {
            let inbound = Set(edges(steps).map(\.to))
            return steps.map(\.nodeID).filter { !inbound.contains($0) }
        }

        /// The heading text. `@alias` is unambiguous to a foreign reader and
        /// survives AIR's own title cleaning, which strips leading step
        /// numbering from a title that starts with a digit.
        static func title(_ alias: String, index: Int) -> String {
            let cleaned = alias
                .components(separatedBy: .whitespacesAndNewlines)
                .filter { !$0.isEmpty }
                .joined(separator: "-")
            return cleaned.isEmpty || cleaned.hasSuffix("#") ? "@step-\(index + 1)" : "@\(cleaned)"
        }

        /// TOML bare-key characters, which is also what a node id may use.
        static func slug(_ value: String) -> String {
            String(value.lowercased().map { character in
                character.isASCII
                    && (character.isLetter || character.isNumber || character == "-" || character == "_")
                    ? character : "-"
            }).trimmingCharacters(in: CharacterSet(charactersIn: "-"))
        }
    }

    /// Byte ranges of one rendered step, in the source's own bytes.
    struct StepSpan: Equatable, Sendable {
        let span: Range<Int>
        let heading: Range<Int>
        let title: Range<Int>
        let body: Range<Int>
    }

    struct Source: Equatable, Sendable {
        let text: String
        let bytes: Data
        let spans: [StepSpan]
        /// Where the managed footer begins; the last step's span ends here.
        let footerStart: Int

        func bodyText(_ index: Int) -> String {
            String(decoding: bytes[spans[index].body], as: UTF8.self)
        }
    }

    /// Renders the pipeline and records where every piece landed. Offsets are
    /// counted while the string is built, so the ranges are the rendering's
    /// own arithmetic rather than a second parse that could disagree with it.
    static func markdown(
        name: String,
        _ definition: MuxaPipelineDefinition,
        steps: [Step]? = nil
    ) -> Source {
        let steps = steps ?? Step.plan(definition.agents)
        var text = "---\nname: \(yaml(name))\n"
        if !inline(definition.description).isEmpty {
            text += "description: \(yaml(definition.description))\n"
        }
        text += "---\n\n## Workflow\n\n"

        var spans: [StepSpan] = []
        var headingStarts: [Int] = []
        var bodyStarts: [Int] = []
        var headingEnds: [Int] = []
        for step in steps {
            let headingStart = text.utf8.count
            text += "### \(step.title)"
            let headingEnd = text.utf8.count
            text += "\n"
            bodyStarts.append(text.utf8.count)
            text += "\n"
            for line in bullets(step) { text += "- \(line)\n" }
            text += "\n"
            headingStarts.append(headingStart)
            headingEnds.append(headingEnd)
        }
        // The blank line the last step ends with belongs to the footer: AIR
        // widens a trailing managed comment backwards over it.
        let footerStart = steps.isEmpty ? text.utf8.count : text.utf8.count - 1
        for index in steps.indices {
            let end = index + 1 < headingStarts.count ? headingStarts[index + 1] : footerStart
            spans.append(StepSpan(
                span: headingStarts[index]..<end,
                heading: headingStarts[index]..<headingEnds[index],
                title: (headingStarts[index] + 4)..<headingEnds[index],
                body: bodyStarts[index]..<end
            ))
        }
        if !steps.isEmpty {
            text += "<!-- workflow-studio:managed:start\n"
            text += managedPayload(steps).canonicalString
            text += "\nworkflow-studio:managed:end -->\n"
        }
        return Source(text: text, bytes: Data(text.utf8), spans: spans, footerStart: footerStart)
    }

    /// What a reader who has never heard of muxa still learns about a step.
    /// Every value is flattened to one line: the authoritative copy is in the
    /// extension, so the prose may be sanitized without losing anything.
    private static func bullets(_ step: Step) -> [String] {
        var lines = ["program: \(inline(step.agent.program))"]
        if !inline(step.agent.role).isEmpty { lines.append("role: \(inline(step.agent.role))") }
        if !inline(step.agent.task).isEmpty { lines.append("task: \(inline(step.agent.task))") }
        let direction = MuxaPipelineDefinition.Agent.canonicalDirection(step.agent.direction)
        if !direction.isEmpty { lines.append("split: \(direction)") }
        if !step.after.isEmpty {
            lines.append("waits for: " + step.after.map { "@\($0)" }.joined(separator: ", "))
        }
        if !step.agent.prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
            lines.append("prompt: carried in the muxa extension, not in this document")
        }
        return lines
    }

    /// The footer AIR reads to restore stable node identity and the real
    /// dependency edges. Without it the graph would be the linear chain that
    /// heading order implies, which is not the graph muxa runs.
    private static func managedPayload(_ steps: [Step]) -> AirJSON {
        .object([
            "ir_version": .string("1.0"),
            "nodes": .array(steps.enumerated().map { order, step in
                .object([
                    "id": .string(step.nodeID),
                    "order": .int(order),
                    "title_sha256": .string(AirDocument.sha256(Data(step.title.utf8))),
                ])
            }),
            "edges": .array(Step.edges(steps).map { edge in
                .object([
                    "id": .string(edge.id),
                    "from": .string(edge.from),
                    "to": .string(edge.to),
                    "kind": .string("sequence"),
                ])
            }),
        ])
    }

    private static func extensionPayload(
        name: String,
        _ definition: MuxaPipelineDefinition,
        steps: [Step]
    ) -> AirJSON {
        var pipeline: [String: AirJSON] = ["name": .string(name)]
        if !definition.description.isEmpty { pipeline["description"] = .string(definition.description) }
        if !definition.layout.isEmpty { pipeline["layout"] = .string(definition.layout) }
        if !definition.prompt.isEmpty { pipeline["prompt"] = .string(definition.prompt) }
        return .object([
            "version": .int(1),
            "pipeline": .object(pipeline),
            "steps": .array(steps.map { step in
                var member: [String: AirJSON] = [
                    "node_id": .string(step.nodeID),
                    "alias": .string(step.agent.alias),
                    "program": .string(step.agent.program),
                    "after": .array(step.agent.after.map(AirJSON.string)),
                ]
                if !step.agent.role.isEmpty { member["role"] = .string(step.agent.role) }
                if !step.agent.task.isEmpty { member["task"] = .string(step.agent.task) }
                if !step.agent.direction.isEmpty { member["direction"] = .string(step.agent.direction) }
                if !step.agent.prompt.isEmpty { member["prompt"] = .string(step.agent.prompt) }
                return .object(member)
            }),
        ])
    }

    // MARK: - Small pieces

    private static func confidence(_ reason: String) -> AirJSON {
        .object([
            "level": .string("explicit"),
            "rule_id": .string("managed.v1"),
            "reason": .string(reason),
        ])
    }

    private static func range(_ value: Range<Int>) -> AirJSON {
        .object(["start_byte": .int(value.lowerBound), "end_byte": .int(value.upperBound)])
    }

    private static func opaqueRange(_ value: Range<Int>, in bytes: Data) -> AirJSON {
        .object([
            "start_byte": .int(value.lowerBound),
            "end_byte": .int(value.upperBound),
            "sha256": .string(AirDocument.sha256(bytes[value])),
            "reason": .string("unparsed-or-unsupported-source"),
        ])
    }

    /// One line, no runs of whitespace: a Markdown rendering must not grow a
    /// heading, a fence or a list item out of somebody's task description.
    private static func inline(_ value: String) -> String {
        value.components(separatedBy: .whitespacesAndNewlines)
            .filter { !$0.isEmpty }
            .joined(separator: " ")
    }

    /// A double-quoted YAML scalar, which is JSON string syntax.
    private static func yaml(_ value: String) -> String {
        AirJSON.string(inline(value)).canonicalString
    }
}
