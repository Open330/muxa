import AppKit
import Foundation

@MainActor
final class QAHelperModel: ObservableObject {
    @Published private(set) var permissions = QAPermissionStatus(
        accessibility: false,
        screenRecording: false
    )
    @Published private(set) var serverError: String?
    @Published private(set) var activityMessage = "Ready"
    @Published private(set) var previewImage: NSImage?

    let server: QAHelperServer

    private let commandHandler = QACommandHandler()
    private var permissionTask: Task<Void, Never>?

    init() {
        server = QAHelperServer()
        refreshPermissions()
        do {
            try server.start { [commandHandler] request in
                await commandHandler.handle(request)
            }
        } catch {
            serverError = error.localizedDescription
        }
        permissionTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(1))
                self?.refreshPermissions()
            }
        }
    }

    func requestPermissions() {
        commandHandler.promptForPermissions()
        refreshPermissions()
        activityMessage = "Permission requests opened. Relaunch this helper after enabling them."
    }

    func refreshPermissions() {
        permissions = commandHandler.permissionStatus()
    }

    func capturePreview() {
        activityMessage = "Capturing Muxa…"
        Task {
            let response = await commandHandler.handle(
                QARequest(command: "capture", text: nil, pressReturn: nil, x: nil, y: nil, width: nil, height: nil)
            )
            if response.ok,
               let encoded = response.pngBase64,
               let data = Data(base64Encoded: encoded),
               let image = NSImage(data: data)
            {
                previewImage = image
                activityMessage = "Captured Muxa window"
            } else {
                activityMessage = response.error ?? "Capture failed"
            }
        }
    }

    deinit {
        permissionTask?.cancel()
        server.stop()
    }
}
