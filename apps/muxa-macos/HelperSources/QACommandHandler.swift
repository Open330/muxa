import AppKit
import ApplicationServices
import CoreGraphics
import Foundation
import ScreenCaptureKit

@MainActor
final class QACommandHandler {
    static let muxaBundleIdentifier = "dev.muxa.mac"

    func handle(_ request: QARequest) async -> QAResponse {
        switch request.command {
        case "status":
            return .success(
                permissions: permissionStatus(),
                socketPath: "/tmp/muxa-qa-helper-\(getuid()).sock"
            )
        case "prompt_permissions":
            promptForPermissions()
            return .success(permissions: permissionStatus())
        case "inspect":
            do {
                let window = try await muxaWindow()
                return .success(window: Self.info(for: window))
            } catch {
                return .failure(error.localizedDescription)
            }
        case "capture":
            guard CGPreflightScreenCaptureAccess() else {
                return .failure("Screen Recording permission is required")
            }
            do {
                let window = try await muxaWindow()
                let png = try await capture(window: window)
                return .success(
                    window: Self.info(for: window),
                    pngBase64: png.base64EncodedString()
                )
            } catch {
                return .failure(error.localizedDescription)
            }
        case "type":
            guard AXIsProcessTrusted() else {
                return .failure("Accessibility permission is required")
            }
            guard let text = request.text, text.utf8.count <= 64 * 1024 else {
                return .failure("text is missing or exceeds 64 KiB")
            }
            do {
                let targetPID = try await focusMuxa()
                try postText(text, targetPID: targetPID)
                if request.pressReturn == true { try postReturn(targetPID: targetPID) }
                return .success()
            } catch {
                return .failure(error.localizedDescription)
            }
        case "new_shell":
            guard AXIsProcessTrusted() else {
                return .failure("Accessibility permission is required")
            }
            do {
                let targetPID = try await focusMuxa()
                try postKey(
                    virtualKey: 45,
                    flags: [.maskCommand, .maskShift],
                    targetPID: targetPID
                )
                return .success()
            } catch {
                return .failure(error.localizedDescription)
            }
        case "click":
            guard AXIsProcessTrusted() else {
                return .failure("Accessibility permission is required")
            }
            guard let x = request.x, let y = request.y else {
                return .failure("x and y are required")
            }
            do {
                _ = try await focusMuxa()
                let window = try await muxaWindow()
                guard x >= 0, y >= 0, x <= window.frame.width, y <= window.frame.height else {
                    return .failure("click point is outside the Muxa window")
                }
                try postClick(
                    at: CGPoint(x: window.frame.minX + x, y: window.frame.minY + y)
                )
                return .success()
            } catch {
                return .failure(error.localizedDescription)
            }
        default:
            return .failure("unsupported command: \(request.command)")
        }
    }

    func permissionStatus() -> QAPermissionStatus {
        QAPermissionStatus(
            accessibility: AXIsProcessTrusted(),
            screenRecording: CGPreflightScreenCaptureAccess()
        )
    }

    func promptForPermissions() {
        _ = AXIsProcessTrustedWithOptions(["AXTrustedCheckOptionPrompt": true] as CFDictionary)
        if !CGPreflightScreenCaptureAccess() {
            _ = CGRequestScreenCaptureAccess()
        }
    }

    private func muxaWindow() async throws -> SCWindow {
        let content = try await SCShareableContent.excludingDesktopWindows(
            false,
            onScreenWindowsOnly: true
        )
        let candidates = content.windows.filter { window in
            window.owningApplication?.bundleIdentifier == Self.muxaBundleIdentifier
                && window.windowLayer == 0
                && window.frame.width >= 200
                && window.frame.height >= 200
        }
        guard let window = candidates.max(by: {
            $0.frame.width * $0.frame.height < $1.frame.width * $1.frame.height
        }) else {
            throw QAHelperError.muxaWindowNotFound
        }
        return window
    }

    private func capture(window: SCWindow) async throws -> Data {
        var lastError: Error?
        for attempt in 0..<3 {
            do {
                return try await captureOnce(window: window)
            } catch {
                lastError = error
                if attempt < 2 {
                    try? await Task.sleep(for: .milliseconds(180))
                }
            }
        }
        throw lastError ?? QAHelperError.captureFailed
    }

    private func captureOnce(window: SCWindow) async throws -> Data {
        let filter = SCContentFilter(desktopIndependentWindow: window)
        let configuration = SCStreamConfiguration()
        let scale = backingScale(for: window.frame)
        configuration.width = min(4096, max(1, Int(window.frame.width * scale)))
        configuration.height = min(4096, max(1, Int(window.frame.height * scale)))
        configuration.showsCursor = false
        configuration.capturesAudio = false

        let image = try await SCScreenshotManager.captureImage(
            contentFilter: filter,
            configuration: configuration
        )
        let bitmap = NSBitmapImageRep(cgImage: image)
        guard let data = bitmap.representation(using: .png, properties: [:]) else {
            throw QAHelperError.pngEncodingFailed
        }
        guard data.count <= 32 * 1024 * 1024 else {
            throw QAHelperError.captureTooLarge
        }
        return data
    }

    private func backingScale(for frame: CGRect) -> CGFloat {
        NSScreen.screens
            .filter { $0.frame.intersects(frame) }
            .max(by: { $0.backingScaleFactor < $1.backingScaleFactor })?
            .backingScaleFactor ?? NSScreen.main?.backingScaleFactor ?? 2
    }

    private func focusMuxa() async throws -> pid_t {
        guard let application = NSRunningApplication
            .runningApplications(withBundleIdentifier: Self.muxaBundleIdentifier)
            .first
        else {
            throw QAHelperError.muxaNotRunning
        }

        let element = AXUIElementCreateApplication(application.processIdentifier)
        _ = AXUIElementSetAttributeValue(
            element,
            kAXFrontmostAttribute as CFString,
            kCFBooleanTrue
        )
        var windowsValue: CFTypeRef?
        if AXUIElementCopyAttributeValue(
            element,
            kAXWindowsAttribute as CFString,
            &windowsValue
        ) == .success,
            let windows = windowsValue as? [AXUIElement],
            let first = windows.first
        {
            _ = AXUIElementPerformAction(first, kAXRaiseAction as CFString)
        }
        let activated = application.activate(options: [.activateAllWindows])
        guard activated || application.isActive else {
            throw QAHelperError.muxaActivationFailed
        }
        try await Task.sleep(for: .milliseconds(250))
        guard !application.isTerminated else {
            throw QAHelperError.muxaNotRunning
        }
        return application.processIdentifier
    }

    private func postText(_ text: String, targetPID: pid_t) throws {
        for chunk in text.qaChunks(maxCharacters: 64) {
            let utf16 = Array(chunk.utf16)
            guard
                let keyDown = CGEvent(
                    keyboardEventSource: nil,
                    virtualKey: 0,
                    keyDown: true
                ),
                let keyUp = CGEvent(
                    keyboardEventSource: nil,
                    virtualKey: 0,
                    keyDown: false
                )
            else {
                throw QAHelperError.eventCreationFailed
            }
            utf16.withUnsafeBufferPointer { buffer in
                guard let base = buffer.baseAddress else { return }
                keyDown.keyboardSetUnicodeString(
                    stringLength: buffer.count,
                    unicodeString: base
                )
                keyUp.keyboardSetUnicodeString(
                    stringLength: buffer.count,
                    unicodeString: base
                )
            }
            keyDown.postToPid(targetPID)
            keyUp.postToPid(targetPID)
        }
    }

    private func postReturn(targetPID: pid_t) throws {
        try postKey(virtualKey: 36, targetPID: targetPID)
    }

    private func postClick(at point: CGPoint) throws {
        guard
            let mouseDown = CGEvent(
                mouseEventSource: nil,
                mouseType: .leftMouseDown,
                mouseCursorPosition: point,
                mouseButton: .left
            ),
            let mouseUp = CGEvent(
                mouseEventSource: nil,
                mouseType: .leftMouseUp,
                mouseCursorPosition: point,
                mouseButton: .left
            )
        else {
            throw QAHelperError.eventCreationFailed
        }
        mouseDown.post(tap: .cghidEventTap)
        mouseUp.post(tap: .cghidEventTap)
    }

    private func postKey(
        virtualKey: CGKeyCode,
        flags: CGEventFlags = [],
        targetPID: pid_t
    ) throws {
        guard
            let keyDown = CGEvent(
                keyboardEventSource: nil,
                virtualKey: virtualKey,
                keyDown: true
            ),
            let keyUp = CGEvent(
                keyboardEventSource: nil,
                virtualKey: virtualKey,
                keyDown: false
            )
        else {
            throw QAHelperError.eventCreationFailed
        }
        keyDown.flags = flags
        keyUp.flags = flags
        keyDown.postToPid(targetPID)
        keyUp.postToPid(targetPID)
    }

    private static func info(for window: SCWindow) -> QAWindowInfo {
        QAWindowInfo(
            id: window.windowID,
            title: window.title,
            x: window.frame.origin.x,
            y: window.frame.origin.y,
            width: window.frame.width,
            height: window.frame.height
        )
    }
}

private enum QAHelperError: LocalizedError {
    case muxaNotRunning
    case muxaWindowNotFound
    case muxaActivationFailed
    case pngEncodingFailed
    case captureTooLarge
    case captureFailed
    case eventCreationFailed

    var errorDescription: String? {
        switch self {
        case .muxaNotRunning: "Muxa is not running"
        case .muxaWindowNotFound: "No onscreen Muxa window was found"
        case .muxaActivationFailed: "Muxa could not be activated"
        case .pngEncodingFailed: "The captured image could not be encoded as PNG"
        case .captureTooLarge: "The captured PNG exceeds the 32 MiB safety limit"
        case .captureFailed: "Muxa window capture failed"
        case .eventCreationFailed: "A keyboard event could not be created"
        }
    }
}

private extension String {
    func qaChunks(maxCharacters: Int) -> [Substring] {
        guard !isEmpty else { return [] }
        var result: [Substring] = []
        var start = startIndex
        while start < endIndex {
            let end = index(start, offsetBy: maxCharacters, limitedBy: endIndex) ?? endIndex
            result.append(self[start..<end])
            start = end
        }
        return result
    }
}
