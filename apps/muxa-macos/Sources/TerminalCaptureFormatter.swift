import Foundation
import SwiftUI

/// Renders a read-only pane capture (`tmux capture-pane -e` output) for a
/// SwiftUI `Text` while keeping the monitoring safety boundary: only SGR
/// (`ESC [ … m`) sequences become text attributes. Every other escape
/// sequence and every C0/C1 control other than `\n` and `\t` is dropped, the
/// same way `sanitizeTerminalCapture` drops them for the plain-text path.
///
/// Parsing is a single pass over the unicode scalars. The formatter is a
/// value type with no main-actor dependency, so the capture model can run it
/// off the view body and tests can run it directly.
struct TerminalCaptureFormatter: Sendable {
    let palette: TerminalCapturePalette

    init(palette: TerminalCapturePalette) {
        self.palette = palette
    }

    /// Renders raw capture bytes. Invalid UTF-8 becomes U+FFFD; leading
    /// continuation bytes are skipped because the daemon bounds the raw
    /// capture at a byte offset, which can cut the first character in half.
    func render(bytes: Data) -> AttributedString {
        render(text: Self.decode(bytes))
    }

    /// Renders text that may still contain escape sequences.
    func render(text: String) -> AttributedString {
        var output = AttributedString()
        var style = TerminalTextStyle.plain
        var run = String.UnicodeScalarView()

        func flush() {
            guard !run.isEmpty else { return }
            output.append(AttributedString(String(run), attributes: attributes(for: style)))
            run.removeAll(keepingCapacity: true)
        }

        TerminalCaptureScanner.scan(
            text,
            text: { scalar in run.append(scalar) },
            sgr: { parameters in
                var next = style
                next.apply(sgrParameters: parameters)
                guard next != style else { return }
                flush()
                style = next
            }
        )
        flush()
        return output
    }

    /// Decodes raw capture bytes as UTF-8 without a leading partial character.
    static func decode(_ bytes: Data) -> String {
        var start = bytes.startIndex
        while start < bytes.endIndex, (0x80...0xBF).contains(bytes[start]) {
            start = bytes.index(after: start)
        }
        return String(decoding: bytes[start...], as: UTF8.self)
    }

    private func attributes(for style: TerminalTextStyle) -> AttributeContainer {
        var container = AttributeContainer()
        guard !style.isPlain else { return container }

        var foreground = style.foreground.map(palette.color(for:)) ?? palette.foreground
        var background = style.background.map(palette.color(for:))
        if style.reverse {
            let previousForeground = foreground
            foreground = background ?? palette.background
            background = previousForeground
        }
        if style.dim {
            foreground = foreground.opacity(0.6)
        }
        if style.foreground != nil || style.reverse || style.dim {
            container.swiftUI.foregroundColor = foreground
        }
        if let background {
            container.swiftUI.backgroundColor = background
        }

        var intent: InlinePresentationIntent = []
        if style.bold { intent.insert(.stronglyEmphasized) }
        if style.italic { intent.insert(.emphasized) }
        if !intent.isEmpty {
            container.inlinePresentationIntent = intent
        }
        if style.underline {
            container.swiftUI.underlineStyle = .single
        }
        if style.strikethrough {
            container.swiftUI.strikethroughStyle = .single
        }
        return container
    }
}

/// Removes every escape sequence and control character from a capture so
/// only printable text, `\n`, and `\t` remain. This is the plain-text twin of
/// `TerminalCaptureFormatter`: both run the same scanner, so the characters of
/// the rendered `AttributedString` always equal this function's output.
func sanitizeTerminalCapture(_ value: String) -> String {
    var output = String.UnicodeScalarView()
    TerminalCaptureScanner.scan(value, text: { output.append($0) }, sgr: { _ in })
    return String(output)
}

/// A terminal color as named by an SGR sequence, before palette resolution.
enum TerminalColor: Equatable, Sendable {
    /// ANSI 0–15, or an xterm 256-color index (16–255).
    case indexed(UInt8)
    case rgb(UInt8, UInt8, UInt8)
}

/// Text attributes accumulated from SGR sequences.
struct TerminalTextStyle: Equatable, Sendable {
    var foreground: TerminalColor?
    var background: TerminalColor?
    var bold = false
    var dim = false
    var italic = false
    var underline = false
    var reverse = false
    var strikethrough = false

    static let plain = TerminalTextStyle()

    var isPlain: Bool { self == .plain }

    /// Applies the parameter bytes of one `ESC [ <parameters> m` sequence.
    /// Accepts both `;`-separated parameters and `:`-separated
    /// subparameters (`4:3`, `38:2::r:g:b`) because tmux emits both.
    /// Unknown parameters are ignored; malformed extended colors stop the
    /// sequence without changing the color.
    mutating func apply(sgrParameters parameters: Substring) {
        guard parameters.unicodeScalars.allSatisfy({ scalar in
            scalar == ";" || scalar == ":" || ("0"..."9").contains(scalar)
        }) else { return }

        let groups: [[Int?]] = parameters
            .split(separator: ";", omittingEmptySubsequences: false)
            .map { group in
                group
                    .split(separator: ":", omittingEmptySubsequences: false)
                    .map { Int($0) }
            }

        var index = 0
        while index < groups.count {
            let group = groups[index]
            let code = group.first.flatMap { $0 } ?? 0
            index += 1
            switch code {
            case 0:
                self = .plain
            case 1:
                bold = true
            case 2:
                dim = true
            case 3:
                italic = true
            case 4:
                underline = group.count > 1 ? (group[1] ?? 0) != 0 : true
            case 7:
                reverse = true
            case 9:
                strikethrough = true
            case 22:
                bold = false
                dim = false
            case 23:
                italic = false
            case 24:
                underline = false
            case 27:
                reverse = false
            case 29:
                strikethrough = false
            case 30...37:
                foreground = .indexed(UInt8(code - 30))
            case 39:
                foreground = nil
            case 40...47:
                background = .indexed(UInt8(code - 40))
            case 49:
                background = nil
            case 90...97:
                foreground = .indexed(UInt8(code - 90 + 8))
            case 100...107:
                background = .indexed(UInt8(code - 100 + 8))
            case 38, 48:
                let color: TerminalColor?
                if group.count > 1 {
                    color = Self.extendedColor(Array(group.dropFirst()), colonForm: true)
                } else {
                    let values = groups[index...].map { $0.first.flatMap { $0 } }
                    color = Self.extendedColor(values, colonForm: false)
                    guard let consumed = Self.extendedColorLength(values) else { return }
                    index += consumed
                }
                guard let color else { return }
                if code == 38 { foreground = color } else { background = color }
            default:
                break
            }
        }
    }

    /// Parses the values following `38`/`48`: `5;n` or `2;r;g;b`. The colon
    /// form may carry a color-space id (`2::r:g:b`), which is skipped.
    private static func extendedColor(_ values: [Int?], colonForm: Bool) -> TerminalColor? {
        guard let mode = values.first.flatMap({ $0 }) else { return nil }
        switch mode {
        case 5:
            guard values.count >= 2, let index = values[1] else { return nil }
            return .indexed(UInt8(clamping: index))
        case 2:
            var components = Array(values.dropFirst())
            if colonForm, components.count >= 4 {
                components.removeFirst()
            }
            guard components.count >= 3,
                  let red = components[0], let green = components[1], let blue = components[2]
            else { return nil }
            return .rgb(UInt8(clamping: red), UInt8(clamping: green), UInt8(clamping: blue))
        default:
            return nil
        }
    }

    /// Number of `;`-separated parameters an extended color consumes, or nil
    /// when the sequence is malformed.
    private static func extendedColorLength(_ values: [Int?]) -> Int? {
        guard let mode = values.first.flatMap({ $0 }) else { return nil }
        switch mode {
        case 5: return values.count >= 2 ? 2 : nil
        case 2: return values.count >= 4 ? 4 : nil
        default: return nil
        }
    }
}

/// Colors used to draw a pane capture. The values match the GhosttyTerminal
/// default theme (Afterglow for dark, Alabaster for light) that the
/// interactive attach surface renders with, so the read-only preview and the
/// Ghostty view show the same colors on the `MuxaSurfacePalette.terminal`
/// backgrounds.
struct TerminalCapturePalette: Sendable {
    let foreground: Color
    let background: Color
    /// The 16 ANSI colors: 0–7 normal, 8–15 bright.
    let ansi: [Color]

    static func palette(for colorScheme: ColorScheme) -> TerminalCapturePalette {
        colorScheme == .dark ? .dark : .light
    }

    static let dark = TerminalCapturePalette(
        foreground: Color(hex: 0xD0D0D0),
        background: Color(hex: 0x212121),
        ansi: [
            0x151515, 0xAC4142, 0x7E8E50, 0xE4B567, 0x6C99BB, 0x9F4E86, 0x7DD5CF, 0xD0D0D0,
            0x505050, 0xAC4142, 0x7E8E50, 0xE4B567, 0x6C99BB, 0x9F4E86, 0x7DD5CF, 0xF5F5F5,
        ].map(Color.init(hex:))
    )

    static let light = TerminalCapturePalette(
        foreground: Color(hex: 0x000000),
        background: Color(hex: 0xF7F7F7),
        ansi: [
            0x000000, 0xAA3731, 0x448C27, 0xCB8800, 0x325CC0, 0x7A3E9D, 0x0083B2, 0xF7F7F7,
            0x777777, 0xF03E31, 0x60CB00, 0xFFBC5D, 0x007ACC, 0xE64CE6, 0x00AACB, 0xF7F7F7,
        ].map(Color.init(hex:))
    )

    func color(for terminalColor: TerminalColor) -> Color {
        switch terminalColor {
        case .indexed(let index) where index < 16:
            return ansi[Int(index)]
        case .indexed(let index):
            let (red, green, blue) = Self.xtermComponents(index)
            return Color(red: red, green: green, blue: blue)
        case .rgb(let red, let green, let blue):
            return Color(
                red: Double(red) / 255,
                green: Double(green) / 255,
                blue: Double(blue) / 255
            )
        }
    }

    /// sRGB components (0–1) of xterm 256-color indexes 16–255: a 6×6×6 cube
    /// followed by a 24-step gray ramp.
    static func xtermComponents(_ index: UInt8) -> (red: Double, green: Double, blue: Double) {
        if index >= 232 {
            let gray = Double(8 + 10 * (Int(index) - 232)) / 255
            return (gray, gray, gray)
        }
        let offset = Int(index) - 16
        func level(_ value: Int) -> Double {
            value == 0 ? 0 : Double(55 + 40 * value) / 255
        }
        return (level(offset / 36), level((offset / 6) % 6), level(offset % 6))
    }
}

private extension Color {
    init(hex: UInt32) {
        self.init(
            red: Double((hex >> 16) & 0xFF) / 255,
            green: Double((hex >> 8) & 0xFF) / 255,
            blue: Double(hex & 0xFF) / 255
        )
    }
}

/// Splits capture text into printable scalars and SGR parameter strings.
///
/// Everything else is dropped: other CSI sequences, OSC/DCS/SOS/PM/APC strings
/// (through BEL or ST), two-byte and intermediate `ESC` sequences, C0
/// controls other than `\n` and `\t`, DEL, and C1 controls (U+0080–U+009F).
enum TerminalCaptureScanner {
    private enum State {
        case text
        case escape
        case escapeIntermediate
        case csi
        case string
        case stringEscape
    }

    static func scan(
        _ value: String,
        text emit: (Unicode.Scalar) -> Void,
        sgr: (Substring) -> Void
    ) {
        var state = State.text
        var parameters = String()

        for scalar in value.unicodeScalars {
            let code = scalar.value
            switch state {
            case .text:
                if code == 0x1B {
                    state = .escape
                } else if scalar == "\n" || scalar == "\t" {
                    emit(scalar)
                } else if code >= 0x20, !(0x7F...0x9F).contains(code) {
                    emit(scalar)
                }
            case .escape:
                switch code {
                case 0x5B: // [
                    parameters.removeAll(keepingCapacity: true)
                    state = .csi
                case 0x5D, 0x50, 0x58, 0x5E, 0x5F: // ] P X ^ _
                    state = .string
                case 0x20...0x2F:
                    state = .escapeIntermediate
                case 0x1B:
                    break
                default:
                    state = .text
                }
            case .escapeIntermediate:
                switch code {
                case 0x20...0x2F:
                    break
                case 0x1B:
                    state = .escape
                default:
                    state = .text
                }
            case .csi:
                switch code {
                case 0x20...0x3F:
                    parameters.unicodeScalars.append(scalar)
                case 0x40...0x7E:
                    if scalar == "m" {
                        sgr(parameters[...])
                    }
                    state = .text
                case 0x1B:
                    state = .escape
                default:
                    break
                }
            case .string:
                switch code {
                case 0x07:
                    state = .text
                case 0x1B:
                    state = .stringEscape
                default:
                    break
                }
            case .stringEscape:
                state = scalar == "\\" ? .text : .string
            }
        }
    }
}
