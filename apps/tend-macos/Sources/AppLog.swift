import Foundation
import OSLog

enum TendLog {
    private static let subsystem = Bundle.main.bundleIdentifier ?? "dev.tend.mac"

    static let app = Logger(subsystem: subsystem, category: "app")
    static let daemon = Logger(subsystem: subsystem, category: "daemon")
    static let terminal = Logger(subsystem: subsystem, category: "terminal")
}
