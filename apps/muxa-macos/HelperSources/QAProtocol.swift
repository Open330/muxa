import Foundation

struct QARequest: Codable, Sendable {
    let command: String
    let text: String?
    let pressReturn: Bool?
    let x: Double?
    let y: Double?

    enum CodingKeys: String, CodingKey {
        case command
        case text
        case pressReturn = "press_return"
        case x, y
    }
}

struct QAPermissionStatus: Codable, Sendable {
    let accessibility: Bool
    let screenRecording: Bool

    enum CodingKeys: String, CodingKey {
        case accessibility
        case screenRecording = "screen_recording"
    }
}

struct QAWindowInfo: Codable, Sendable {
    let id: UInt32
    let title: String?
    let x: Double
    let y: Double
    let width: Double
    let height: Double
}

struct QAResponse: Codable, Sendable {
    let ok: Bool
    var error: String?
    var permissions: QAPermissionStatus?
    var window: QAWindowInfo?
    var pngBase64: String?
    var socketPath: String?

    enum CodingKeys: String, CodingKey {
        case ok
        case error
        case permissions
        case window
        case pngBase64 = "png_base64"
        case socketPath = "socket_path"
    }

    static func success(
        permissions: QAPermissionStatus? = nil,
        window: QAWindowInfo? = nil,
        pngBase64: String? = nil,
        socketPath: String? = nil
    ) -> QAResponse {
        QAResponse(
            ok: true,
            permissions: permissions,
            window: window,
            pngBase64: pngBase64,
            socketPath: socketPath
        )
    }

    static func failure(_ error: String) -> QAResponse {
        QAResponse(ok: false, error: error)
    }
}
