import Carbon.HIToolbox
import CoreGraphics
import Foundation

/// Turns a `key` request into the virtual key code and modifier flags that
/// `QACommandHandler` posts to Muxa. A single character is resolved against
/// the current keyboard layout (so the key that really produces `,` is used),
/// with a US QWERTY table as the fallback when the layout data is unavailable.
enum QAKeyResolver {
    struct Resolved: Equatable {
        let virtualKey: CGKeyCode
        let flags: CGEventFlags
    }

    enum Failure: LocalizedError, Equatable {
        case emptyKey
        case unknownNamedKey(String)
        case unknownModifier(String)
        case unresolvableCharacter(String)

        var errorDescription: String? {
            switch self {
            case .emptyKey:
                "key is empty"
            case .unknownNamedKey(let name):
                "unknown key '\(name)'; use a single character or one of \(QAKeyResolver.namedKeyList)"
            case .unknownModifier(let name):
                "unknown modifier '\(name)'; use command, shift, option, or control"
            case .unresolvableCharacter(let character):
                "'\(character)' cannot be typed with a single key on the current keyboard layout"
            }
        }
    }

    static let namedKeys: [String: CGKeyCode] = [
        "return": 36,
        "enter": 36,
        "escape": 53,
        "esc": 53,
        "tab": 48,
        "space": 49,
        "up": 126,
        "down": 125,
        "left": 123,
        "right": 124,
        "delete": 51,
        "backspace": 51,
    ]

    static let namedKeyList = "return, escape, tab, space, up, down, left, right, delete"

    /// Keypad keys also translate to digits and operators; skip them so `*`
    /// resolves to Shift-8 rather than the keypad key.
    private static let keypadKeyCodes: Set<UInt16> = [
        65, 67, 69, 71, 75, 76, 78, 81, 82, 83, 84, 85, 86, 87, 88, 89, 91, 92,
    ]

    static func flags(forModifiers modifiers: [String]) throws -> CGEventFlags {
        var flags: CGEventFlags = []
        for modifier in modifiers {
            switch modifier.lowercased() {
            case "command", "cmd": flags.insert(.maskCommand)
            case "shift": flags.insert(.maskShift)
            case "option", "opt", "alt": flags.insert(.maskAlternate)
            case "control", "ctrl": flags.insert(.maskControl)
            default: throw Failure.unknownModifier(modifier)
            }
        }
        return flags
    }

    static func resolve(key: String, modifiers: [String]) throws -> Resolved {
        guard !key.isEmpty else { throw Failure.emptyKey }
        var flags = try flags(forModifiers: modifiers)
        if key.count == 1 {
            guard let match = currentLayoutMatch(for: key) ?? usLayoutMatch(for: key) else {
                throw Failure.unresolvableCharacter(key)
            }
            flags.formUnion(match.flags)
            return Resolved(virtualKey: match.virtualKey, flags: flags)
        }
        guard let code = namedKeys[key.lowercased()] else {
            throw Failure.unknownNamedKey(key)
        }
        return Resolved(virtualKey: code, flags: flags)
    }

    // MARK: - Current keyboard layout

    /// Key equivalents such as ⌘W are matched by macOS through the current
    /// ASCII-capable layout even while an input method (for example 2-Set
    /// Korean, whose own layout only reaches Latin letters with Option) is
    /// active, so that layout is consulted first and the active layout second.
    private static func currentLayoutMatch(for character: String) -> Resolved? {
        let sources = [
            TISCopyCurrentASCIICapableKeyboardLayoutInputSource(),
            TISCopyCurrentKeyboardLayoutInputSource(),
        ]
        for source in sources {
            guard let source = source?.takeRetainedValue() else { continue }
            if let match = layoutMatch(for: character, in: source) { return match }
        }
        return nil
    }

    private static func layoutMatch(for character: String, in source: TISInputSource) -> Resolved? {
        guard let layoutPointer = TISGetInputSourceProperty(
            source,
            kTISPropertyUnicodeKeyLayoutData
        ) else { return nil }
        let layoutData = Unmanaged<CFData>.fromOpaque(layoutPointer).takeUnretainedValue()
        guard let bytes = CFDataGetBytePtr(layoutData) else { return nil }
        let keyboardType = UInt32(LMGetKbdType())
        // Least-modified chord first so `p` is Shift-free and `P` is Shift-p.
        let chords: [(state: UInt32, flags: CGEventFlags)] = [
            (0, []),
            (UInt32(shiftKey >> 8) & 0xFF, .maskShift),
            (UInt32(optionKey >> 8) & 0xFF, .maskAlternate),
            (UInt32((shiftKey | optionKey) >> 8) & 0xFF, [.maskShift, .maskAlternate]),
        ]

        return bytes.withMemoryRebound(to: UCKeyboardLayout.self, capacity: 1) { layout in
            for chord in chords {
                for code in UInt16(0)..<UInt16(128) where !keypadKeyCodes.contains(code) {
                    var deadKeyState: UInt32 = 0
                    var length = 0
                    var buffer = [UniChar](repeating: 0, count: 4)
                    let status = UCKeyTranslate(
                        layout,
                        code,
                        UInt16(kUCKeyActionDown),
                        chord.state,
                        keyboardType,
                        OptionBits(kUCKeyTranslateNoDeadKeysBit),
                        &deadKeyState,
                        buffer.count,
                        &length,
                        &buffer
                    )
                    guard status == noErr, length > 0 else { continue }
                    if String(utf16CodeUnits: buffer, count: length) == character {
                        return Resolved(virtualKey: CGKeyCode(code), flags: chord.flags)
                    }
                }
            }
            return nil
        }
    }

    // MARK: - US QWERTY fallback

    private static let usUnshifted: [Character: CGKeyCode] = [
        "a": 0, "s": 1, "d": 2, "f": 3, "h": 4, "g": 5, "z": 6, "x": 7, "c": 8, "v": 9,
        "b": 11, "q": 12, "w": 13, "e": 14, "r": 15, "y": 16, "t": 17,
        "1": 18, "2": 19, "3": 20, "4": 21, "6": 22, "5": 23, "=": 24, "9": 25, "7": 26,
        "-": 27, "8": 28, "0": 29, "]": 30, "o": 31, "u": 32, "[": 33, "i": 34, "p": 35,
        "l": 37, "j": 38, "'": 39, "k": 40, ";": 41, "\\": 42, ",": 43, "/": 44, "n": 45,
        "m": 46, ".": 47, "`": 50, " ": 49, "\t": 48, "\r": 36, "\n": 36,
    ]

    private static let usShifted: [Character: Character] = [
        "!": "1", "@": "2", "#": "3", "$": "4", "%": "5", "^": "6", "&": "7", "*": "8",
        "(": "9", ")": "0", "_": "-", "+": "=", "{": "[", "}": "]", "|": "\\", ":": ";",
        "\"": "'", "<": ",", ">": ".", "?": "/", "~": "`",
    ]

    static func usLayoutMatch(for character: String) -> Resolved? {
        guard let value = character.first, character.count == 1 else { return nil }
        if let code = usUnshifted[value] {
            return Resolved(virtualKey: code, flags: [])
        }
        if value.isUppercase, let lower = value.lowercased().first, let code = usUnshifted[lower] {
            return Resolved(virtualKey: code, flags: .maskShift)
        }
        if let base = usShifted[value], let code = usUnshifted[base] {
            return Resolved(virtualKey: code, flags: .maskShift)
        }
        return nil
    }
}
