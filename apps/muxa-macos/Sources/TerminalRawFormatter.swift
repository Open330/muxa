import Foundation

/// Converts terminal bytes into a conventional offset/hex/ASCII dump. This is
/// intentionally unlike the decoded Screen view: every byte is visible and no
/// terminal control sequence can be executed.
func terminalRawDescription(_ data: Data) -> String {
    let bytes = Array(data)
    guard !bytes.isEmpty else { return "No bytes captured." }

    var lines = [
        "OFFSET    HEX BYTES                                         ASCII",
        "────────  ───────────────────────────────────────────────  ────────────────",
    ]
    for offset in stride(from: 0, to: bytes.count, by: 16) {
        let end = min(offset + 16, bytes.count)
        let row = Array(bytes[offset..<end])
        let firstHalf = row.prefix(8).map { String(format: "%02X", $0) }.joined(separator: " ")
        let secondHalf = row.dropFirst(8).map { String(format: "%02X", $0) }.joined(separator: " ")
        let hex = (firstHalf + (secondHalf.isEmpty ? "" : "  " + secondHalf))
            .padding(toLength: 49, withPad: " ", startingAt: 0)
        let ascii = String(row.map { byte in
            (0x20...0x7E).contains(byte) ? Character(UnicodeScalar(byte)) : "."
        })
        lines.append(String(format: "%08X  %@ %@", offset, hex, ascii))
    }
    return lines.joined(separator: "\n")
}
