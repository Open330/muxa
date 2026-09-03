import Foundation

struct QARequest: Codable, Sendable {
    let command: String
    let text: String?
    let pressReturn: Bool?
    let x: Double?
    let y: Double?
    let width: Double?
    let height: Double?
    let deltaY: Double?
    /// `key` command: a single character or a named key
    /// (return/escape/tab/space/up/down/left/right/delete).
    let key: String?
    /// `key` command: any of command/shift/option/control.
    let modifiers: [String]?

    enum CodingKeys: String, CodingKey {
        case command
        case text
        case pressReturn = "press_return"
        case x, y, width, height
        case deltaY = "delta_y"
        case key
        case modifiers
    }

    init(
        command: String,
        text: String? = nil,
        pressReturn: Bool? = nil,
        x: Double? = nil,
        y: Double? = nil,
        width: Double? = nil,
        height: Double? = nil,
        key: String? = nil,
        modifiers: [String]? = nil,
        deltaY: Double? = nil
    ) {
        self.command = command
        self.text = text
        self.pressReturn = pressReturn
        self.x = x
        self.y = y
        self.width = width
        self.height = height
        self.deltaY = deltaY
        self.key = key
        self.modifiers = modifiers
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
    /// Virtual key code that a `key` request resolved to.
    var keyCode: Int?

    enum CodingKeys: String, CodingKey {
        case ok
        case error
        case permissions
        case window
        case pngBase64 = "png_base64"
        case socketPath = "socket_path"
        case keyCode = "key_code"
    }

    static func success(
        permissions: QAPermissionStatus? = nil,
        window: QAWindowInfo? = nil,
        pngBase64: String? = nil,
        socketPath: String? = nil,
        keyCode: Int? = nil
    ) -> QAResponse {
        QAResponse(
            ok: true,
            permissions: permissions,
            window: window,
            pngBase64: pngBase64,
            socketPath: socketPath,
            keyCode: keyCode
        )
    }

    static func failure(_ error: String) -> QAResponse {
        QAResponse(ok: false, error: error)
    }
}
