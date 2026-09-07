import CryptoKit
import Foundation

/// The AIR 1 envelope, and the exact bytes it is digested over.
///
/// AIR artifacts are self-authenticating: `artifact_id` is
/// `urn:air:sha256:<content_digest>`, and both digests are taken over an
/// RFC 8785 (JCS) canonicalization of a *projection* of the envelope, with a
/// domain separator in front. AIR Workbench recomputes them on open, so this
/// file has to agree with it byte for byte — which is why the JSON here is a
/// small explicit value tree rather than `JSONSerialization`, whose escaping
/// and key ordering are not part of its contract.
enum AirJSON: Equatable, Sendable {
    case null
    case bool(Bool)
    case int(Int)
    /// Only ever produced by decoding a foreign document. AIR 1 bodies are
    /// integer-only, so nothing this file emits takes this case.
    case double(Double)
    case string(String)
    case array([AirJSON])
    case object([String: AirJSON])

    // MARK: Reading

    var stringValue: String? {
        if case .string(let value) = self { return value }
        return nil
    }

    var intValue: Int? {
        if case .int(let value) = self { return value }
        return nil
    }

    var arrayValue: [AirJSON]? {
        if case .array(let value) = self { return value }
        return nil
    }

    var objectValue: [String: AirJSON]? {
        if case .object(let value) = self { return value }
        return nil
    }

    subscript(key: String) -> AirJSON? {
        objectValue?[key]
    }

    // MARK: Writing

    /// RFC 8785: object keys sorted, no insignificant whitespace, JSON string
    /// escaping exactly as `JSON.stringify` produces it.
    var canonicalString: String {
        switch self {
        case .null:
            "null"
        case .bool(let value):
            value ? "true" : "false"
        case .int(let value):
            String(value)
        case .double(let value):
            Self.canonicalNumber(value)
        case .string(let value):
            Self.quoted(value)
        case .array(let items):
            "[" + items.map(\.canonicalString).joined(separator: ",") + "]"
        case .object(let members):
            "{" + members.keys.sorted(by: Self.keyOrder)
                .map { "\(Self.quoted($0)):\(members[$0]!.canonicalString)" }
                .joined(separator: ",") + "}"
        }
    }

    /// The same value tree, indented. `.air.json` files on disk are read by
    /// people as well as by Workbench, and the digests are taken over the
    /// canonical form regardless of how the file is laid out.
    func prettyString(indent: Int = 0) -> String {
        let pad = String(repeating: " ", count: indent)
        let inner = String(repeating: " ", count: indent + 2)
        switch self {
        case .array(let items) where !items.isEmpty:
            return "[\n"
                + items.map { inner + $0.prettyString(indent: indent + 2) }.joined(separator: ",\n")
                + "\n\(pad)]"
        case .object(let members) where !members.isEmpty:
            return "{\n"
                + members.keys.sorted(by: Self.keyOrder).map {
                    "\(inner)\(Self.quoted($0)): \(members[$0]!.prettyString(indent: indent + 2))"
                }.joined(separator: ",\n")
                + "\n\(pad)}"
        default:
            return canonicalString
        }
    }

    /// JavaScript compares strings by UTF-16 code unit, and JCS inherits that
    /// ordering from `Object.keys().sort()`.
    private static func keyOrder(_ lhs: String, _ rhs: String) -> Bool {
        var left = Array(lhs.utf16)
        var right = Array(rhs.utf16)
        let shared = min(left.count, right.count)
        for index in 0..<shared where left[index] != right[index] {
            return left[index] < right[index]
        }
        left.removeAll()
        right.removeAll()
        return lhs.utf16.count < rhs.utf16.count
    }

    /// `JSON.stringify`'s escaping: the two mandatory escapes, the five short
    /// control escapes, `\u00xx` for the rest of C0, and every other scalar
    /// verbatim as UTF-8.
    private static func quoted(_ value: String) -> String {
        var out = "\""
        for scalar in value.unicodeScalars {
            switch scalar {
            case "\"": out += "\\\""
            case "\\": out += "\\\\"
            case "\u{08}": out += "\\b"
            case "\u{09}": out += "\\t"
            case "\u{0A}": out += "\\n"
            case "\u{0C}": out += "\\f"
            case "\u{0D}": out += "\\r"
            default:
                if scalar.value < 0x20 {
                    out += String(format: "\\u%04x", scalar.value)
                } else {
                    out.unicodeScalars.append(scalar)
                }
            }
        }
        return out + "\""
    }

    private static func canonicalNumber(_ value: Double) -> String {
        if value == value.rounded(), abs(value) < 9_007_199_254_740_992 {
            return String(Int64(value))
        }
        return "\(value)"
    }

    // MARK: Decoding

    static func decode(_ data: Data) throws -> AirJSON {
        let raw = try JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed])
        return try value(from: raw)
    }

    private static func value(from raw: Any) throws -> AirJSON {
        switch raw {
        case is NSNull:
            return .null
        case let number as NSNumber:
            if CFGetTypeID(number) == CFBooleanGetTypeID() { return .bool(number.boolValue) }
            let double = number.doubleValue
            if double == double.rounded(), let exact = Int(exactly: double) { return .int(exact) }
            return .double(double)
        case let string as String:
            return .string(string)
        case let array as [Any]:
            return .array(try array.map(value(from:)))
        case let object as [String: Any]:
            var members: [String: AirJSON] = [:]
            members.reserveCapacity(object.count)
            for (key, member) in object { members[key] = try value(from: member) }
            return .object(members)
        default:
            throw AirError.malformed(String(localized: "This file is not JSON that AIR can carry."))
        }
    }
}

/// What went wrong reading or writing an AIR artifact. Every case reads as one
/// line an operator can act on; the module shows them inline, never in an alert.
enum AirError: LocalizedError, Equatable {
    case malformed(String)
    case notAir(String)
    case integrity
    case unsupported(kind: String, profile: String)
    case rejected([String])

    var errorDescription: String? {
        switch self {
        case .malformed(let detail):
            detail
        case .notAir(let detail):
            detail
        case .integrity:
            String(localized: "This file's AIR integrity digest does not match its contents.")
        case .unsupported(let kind, let profile):
            String(localized: "Muxa reads AIR workflow-skill documents; this one is \(kind) · \(profile).")
        case .rejected(let problems):
            problems.first ?? String(localized: "This workflow would not launch.")
        }
    }
}

/// One AIR 1 artifact: the envelope, its digests, and the file bytes.
struct AirDocument: Equatable, Sendable {
    /// Everything about AIR 1 that muxa pins in one place. A newer AIR is a
    /// deliberate change here, not something an import discovers at runtime.
    enum Spec {
        static let schema = "https://open330.github.io/air/schema/1.0.0/air.schema.json"
        static let version = "1.0.0"
        static let workflowProfile = "https://open330.github.io/air/profiles/1.0.0/workflow-skill"
        static let traceProfile = "https://open330.github.io/air/profiles/1.0.0/trace-native-run"
        static let contentDomain = "AIR-CONTENT-V1\n"
        static let envelopeDomain = "AIR-ENVELOPE-V1\n"
        /// muxa's own vendor payload. AIR does not model panes, programs or
        /// prompts, and says so by giving vendors this door.
        static let muxaExtension = "https://github.com/jiunbae/muxa/air/x-muxa/1"
        /// SHA-256 of no bytes: AIR's shape for "there is nothing here".
        static let emptyDigest = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    }

    let json: AirJSON

    var kind: String { json["kind"]?.stringValue ?? "" }
    var profile: String { json["profile"]?.stringValue ?? "" }
    var body: AirJSON { json["body"] ?? .object([:]) }
    var extensions: AirJSON { json["extensions"] ?? .object([:]) }
    var contentDigest: String { json["integrity"]?["content_digest"]?.stringValue ?? "" }
    var artifactID: String { json["artifact_id"]?.stringValue ?? "" }

    var fileData: Data { Data(json.prettyString().utf8) + Data("\n".utf8) }

    /// Builds a finished envelope: the caller supplies the parts AIR calls
    /// content, and the digests follow from them.
    static func envelope(
        kind: String,
        profile: String,
        body: AirJSON,
        provenance: AirJSON,
        extensions: [String: AirJSON]
    ) -> AirDocument {
        let content: [String: AirJSON] = [
            "format": .string("air"),
            "air_version": .string(Spec.version),
            "kind": .string(kind),
            "profile": .string(profile),
            "body": body,
        ]
        let contentDigest = digest(domain: Spec.contentDomain, of: .object(content))
        var envelope = content
        envelope["$schema"] = .string(Spec.schema)
        envelope["artifact_id"] = .string("urn:air:sha256:\(contentDigest)")
        envelope["provenance"] = provenance
        envelope["required_extensions"] = .array([])
        envelope["extensions"] = .object(extensions)
        envelope["integrity"] = .object([
            "canonicalization": .string("RFC8785"),
            "algorithm": .string("sha-256"),
            "content_digest": .string(contentDigest),
        ])
        let envelopeDigest = digest(domain: Spec.envelopeDomain, of: .object(envelope))
        envelope["integrity"] = .object([
            "canonicalization": .string("RFC8785"),
            "algorithm": .string("sha-256"),
            "content_digest": .string(contentDigest),
            "envelope_digest": .string(envelopeDigest),
        ])
        return AirDocument(json: .object(envelope))
    }

    /// Reads a `.air.json` file and checks the two things a reader can check
    /// without trusting the writer: that it is an AIR 1 envelope at all, and
    /// that its content digest still describes its body.
    static func decode(_ data: Data) throws -> AirDocument {
        let json: AirJSON
        do {
            json = try AirJSON.decode(data)
        } catch let error as AirError {
            throw error
        } catch {
            throw AirError.malformed(String(localized: "This file is not valid JSON."))
        }
        guard json.objectValue != nil else {
            throw AirError.notAir(String(localized: "An AIR artifact is a JSON object."))
        }
        let document = AirDocument(json: json)
        guard json["format"]?.stringValue == "air" else {
            throw AirError.notAir(String(localized: "This file is not an AIR artifact."))
        }
        guard json["air_version"]?.stringValue == Spec.version else {
            let found = json["air_version"]?.stringValue ?? String(localized: "none")
            throw AirError.notAir(
                String(localized: "Muxa reads AIR \(Spec.version); this file is AIR \(found).")
            )
        }
        try document.verifyIntegrity()
        return document
    }

    /// Recomputes the content digest the way Workbench does. AIR's content
    /// projection deliberately excludes provenance and extensions, so a
    /// vendor payload — muxa's or anyone else's — never changes this number.
    func verifyIntegrity() throws {
        let content: [String: AirJSON] = [
            "format": json["format"] ?? .null,
            "air_version": json["air_version"] ?? .null,
            "kind": json["kind"] ?? .null,
            "profile": json["profile"] ?? .null,
            "body": body,
        ]
        let recomputed = Self.digest(domain: Spec.contentDomain, of: .object(content))
        guard recomputed == contentDigest, artifactID == "urn:air:sha256:\(recomputed)" else {
            throw AirError.integrity
        }
    }

    static func digest(domain: String, of value: AirJSON) -> String {
        sha256(Data(domain.utf8) + Data(value.canonicalString.utf8))
    }

    static func sha256(_ data: Data) -> String {
        SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
    }
}
