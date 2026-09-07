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
                let window = try await muxaWindow(titled: request.window)
                return .success(window: Self.info(for: window))
            } catch {
                return .failure(error.localizedDescription)
            }
        case "capture":
            guard CGPreflightScreenCaptureAccess() else {
                return .failure("Screen Recording permission is required")
            }
            do {
                let window = try await muxaWindow(titled: request.window)
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
                let targetPID = try await focusMuxaIfPossible()
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
                let targetPID = try await focusMuxaIfPossible()
                try postKey(
                    virtualKey: 45,
                    flags: [.maskCommand, .maskShift],
                    targetPID: targetPID
                )
                return .success()
            } catch {
                return .failure(error.localizedDescription)
            }
        case "key":
            guard AXIsProcessTrusted() else {
                return .failure("Accessibility permission is required")
            }
            guard let key = request.key, !key.isEmpty, key.utf8.count <= 32 else {
                return .failure("key is missing or exceeds 32 bytes")
            }
            let modifiers = request.modifiers ?? []
            guard modifiers.count <= 4 else {
                return .failure("at most 4 modifiers are allowed")
            }
            do {
                let resolved = try QAKeyResolver.resolve(key: key, modifiers: modifiers)
                let targetPID = try await focusMuxaIfPossible()
                try postKey(
                    virtualKey: resolved.virtualKey,
                    flags: resolved.flags,
                    targetPID: targetPID
                )
                return .success(keyCode: Int(resolved.virtualKey))
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
                let targetPID = try await focusMuxaIfPossible()
                let window = try await muxaWindow(titled: request.window)
                guard x >= 0, y >= 0, x <= window.frame.width, y <= window.frame.height else {
                    return .failure("click point is outside the Muxa window")
                }
                try postClick(
                    at: CGPoint(x: window.frame.minX + x, y: window.frame.minY + y),
                    targetPID: NSRunningApplication
                        .runningApplications(withBundleIdentifier: Self.muxaBundleIdentifier)
                        .first?.isActive == true ? nil : targetPID
                )
                return .success()
            } catch {
                return .failure(error.localizedDescription)
            }
        case "scroll":
            guard AXIsProcessTrusted() else {
                return .failure("Accessibility permission is required")
            }
            guard let x = request.x, let y = request.y, let deltaY = request.deltaY,
                  abs(deltaY) <= 4000
            else {
                return .failure("x, y, and delta_y (|delta_y| <= 4000) are required")
            }
            do {
                let targetPID = try await focusMuxaIfPossible()
                let window = try await muxaWindow(titled: request.window)
                guard x >= 0, y >= 0, x <= window.frame.width, y <= window.frame.height else {
                    return .failure("scroll point is outside the Muxa window")
                }
                try postScroll(
                    at: CGPoint(x: window.frame.minX + x, y: window.frame.minY + y),
                    deltaY: deltaY,
                    targetPID: NSRunningApplication
                        .runningApplications(withBundleIdentifier: Self.muxaBundleIdentifier)
                        .first?.isActive == true ? nil : targetPID
                )
                return .success()
            } catch {
                return .failure(error.localizedDescription)
            }
        case "resize":
            guard AXIsProcessTrusted() else {
                return .failure("Accessibility permission is required")
            }
            guard let width = request.width, let height = request.height,
                  width >= 200, height >= 200, width <= 8192, height <= 8192
            else {
                return .failure("width and height between 200 and 8192 are required")
            }
            do {
                // Resizing goes through the Accessibility API, which does not
                // need the app to be frontmost. Staying out of the way lets a
                // layout check run while the operator uses another app.
                let targetPID = try muxaProcessIdentifier()
                try resizeMuxaWindow(
                    pid: targetPID,
                    x: request.x,
                    y: request.y,
                    width: width,
                    height: height
                )
                try await Task.sleep(for: .milliseconds(400))
                let window = try await muxaWindow(titled: request.window)
                return .success(window: Self.info(for: window))
            } catch {
                return .failure(error.localizedDescription)
            }
        case "menu":
            guard AXIsProcessTrusted() else {
                return .failure("Accessibility permission is required")
            }
            guard let path = request.path, !path.isEmpty, path.count <= 5 else {
                return .failure("path is required (1-5 menu titles)")
            }
            do {
                // Menu items are pressed through the Accessibility API, so
                // Settings and the Welcome guide open without taking focus.
                let pid = try muxaProcessIdentifier()
                try pressMenuItem(path: path, pid: pid)
                try await Task.sleep(for: .milliseconds(500))
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

    private func muxaWindow(titled title: String? = nil) async throws -> SCWindow {
        let content = try await SCShareableContent.excludingDesktopWindows(
            false,
            onScreenWindowsOnly: true
        )
        let wanted = title?.trimmingCharacters(in: .whitespaces) ?? ""
        let candidates = content.windows.filter { window in
            window.owningApplication?.bundleIdentifier == Self.muxaBundleIdentifier
                && window.windowLayer == 0
                && window.frame.width >= 200
                && window.frame.height >= 200
                && (wanted.isEmpty
                    || (window.title ?? "").localizedCaseInsensitiveContains(wanted))
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

    /// Activates Muxa when macOS allows it, but never fails for a command
    /// whose events are posted straight to the process: key, text, and the
    /// new-shell chord reach Muxa even while the operator works elsewhere.
    /// Clicks and scrolls still need the real focus, because those are HID
    /// events at screen coordinates.
    private func focusMuxaIfPossible() async throws -> pid_t {
        do {
            return try await focusMuxa()
        } catch QAHelperError.muxaActivationFailed {
            return try muxaProcessIdentifier()
        }
    }

    /// Presses a menu item by title path, e.g. ["Muxa", "Settings…"].
    /// Titles match case-insensitively on a prefix, so "Settings" finds
    /// "Settings…" and a localized menu still resolves by its own title.
    private func pressMenuItem(path: [String], pid: pid_t) throws {
        let application = AXUIElementCreateApplication(pid)
        var menuBarValue: CFTypeRef?
        guard AXUIElementCopyAttributeValue(
            application,
            kAXMenuBarAttribute as CFString,
            &menuBarValue
        ) == .success, let menuBarValue else {
            throw QAHelperError.menuItemNotFound(path.joined(separator: " > "))
        }
        // swiftlint:disable:next force_cast
        let menuBar = menuBarValue as! AXUIElement

        var element: AXUIElement = menuBar
        for (index, title) in path.enumerated() {
            guard let match = Self.childElement(of: element, titled: title) else {
                throw QAHelperError.menuItemNotFound(path.prefix(index + 1).joined(separator: " > "))
            }
            if index == path.count - 1 {
                guard AXUIElementPerformAction(match, kAXPressAction as CFString) == .success else {
                    throw QAHelperError.menuItemNotFound(path.joined(separator: " > "))
                }
                return
            }
            // A menu bar item owns one AXMenu child that holds the items.
            var menuValue: CFTypeRef?
            if AXUIElementCopyAttributeValue(match, kAXChildrenAttribute as CFString, &menuValue) == .success,
               let children = menuValue as? [AXUIElement],
               let menu = children.first
            {
                element = menu
            } else {
                element = match
            }
        }
        throw QAHelperError.menuItemNotFound(path.joined(separator: " > "))
    }

    private static func childElement(of element: AXUIElement, titled title: String) -> AXUIElement? {
        var childrenValue: CFTypeRef?
        guard AXUIElementCopyAttributeValue(
            element,
            kAXChildrenAttribute as CFString,
            &childrenValue
        ) == .success, let children = childrenValue as? [AXUIElement] else {
            return nil
        }
        let wanted = title.lowercased()
        for child in children {
            var titleValue: CFTypeRef?
            guard AXUIElementCopyAttributeValue(
                child,
                kAXTitleAttribute as CFString,
                &titleValue
            ) == .success, let childTitle = titleValue as? String else { continue }
            let candidate = childTitle.lowercased()
            if candidate == wanted || candidate.hasPrefix(wanted) {
                return child
            }
        }
        return nil
    }

    /// Muxa's pid without activating it.
    private func muxaProcessIdentifier() throws -> pid_t {
        guard let application = NSRunningApplication
            .runningApplications(withBundleIdentifier: Self.muxaBundleIdentifier)
            .first
        else {
            throw QAHelperError.muxaNotRunning
        }
        return application.processIdentifier
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

    /// Move and resize the largest Muxa window through the Accessibility API.
    /// SwiftUI still enforces the window's minimum size, so the reported
    /// geometry after the change is returned to the caller.
    private func resizeMuxaWindow(
        pid: pid_t,
        x: Double?,
        y: Double?,
        width: Double,
        height: Double
    ) throws {
        let application = AXUIElementCreateApplication(pid)
        var windowsValue: CFTypeRef?
        guard AXUIElementCopyAttributeValue(
            application,
            kAXWindowsAttribute as CFString,
            &windowsValue
        ) == .success,
            let windows = windowsValue as? [AXUIElement]
        else {
            throw QAHelperError.muxaWindowNotFound
        }

        var target: AXUIElement?
        var largestArea = 0.0
        for window in windows {
            var sizeValue: CFTypeRef?
            guard AXUIElementCopyAttributeValue(
                window,
                kAXSizeAttribute as CFString,
                &sizeValue
            ) == .success,
                let axValue = sizeValue,
                CFGetTypeID(axValue) == AXValueGetTypeID()
            else { continue }
            var size = CGSize.zero
            // swiftlint:disable:next force_cast
            AXValueGetValue(axValue as! AXValue, .cgSize, &size)
            let area = size.width * size.height
            if area > largestArea {
                largestArea = area
                target = window
            }
        }
        guard let target else { throw QAHelperError.muxaWindowNotFound }

        if let x, let y {
            var point = CGPoint(x: x, y: y)
            guard let value = AXValueCreate(.cgPoint, &point) else {
                throw QAHelperError.eventCreationFailed
            }
            let moved = AXUIElementSetAttributeValue(target, kAXPositionAttribute as CFString, value)
            guard moved == .success else { throw QAHelperError.resizeFailed(moved.rawValue) }
        }
        var size = CGSize(width: width, height: height)
        guard let value = AXValueCreate(.cgSize, &size) else {
            throw QAHelperError.eventCreationFailed
        }
        let resized = AXUIElementSetAttributeValue(target, kAXSizeAttribute as CFString, value)
        guard resized == .success else { throw QAHelperError.resizeFailed(resized.rawValue) }
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

    /// Posts a pixel-precise scroll wheel event at `point`. Positive
    /// `deltaY` scrolls content up (like dragging the wheel toward you on
    /// a natural-scrolling Mac), negative scrolls it down.
    private func postScroll(at point: CGPoint, deltaY: Double, targetPID: pid_t? = nil) throws {
        guard let event = CGEvent(
            scrollWheelEvent2Source: nil,
            units: .pixel,
            wheelCount: 1,
            wheel1: Int32(deltaY.rounded()),
            wheel2: 0,
            wheel3: 0
        ) else {
            throw QAHelperError.eventCreationFailed
        }
        event.location = point
        if let targetPID {
            event.postToPid(targetPID)
        } else {
            event.post(tap: .cghidEventTap)
        }
    }

    /// Posts a click. With `targetPID` the events go straight to Muxa's event
    /// queue, so a layout check works while another app holds focus; without
    /// it they go through the HID tap, which is what a real user's click is.
    private func postClick(at point: CGPoint, targetPID: pid_t? = nil) throws {
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
        if let targetPID {
            mouseDown.postToPid(targetPID)
            mouseUp.postToPid(targetPID)
        } else {
            mouseDown.post(tap: .cghidEventTap)
            mouseUp.post(tap: .cghidEventTap)
        }
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
    case resizeFailed(Int32)
    case menuItemNotFound(String)

    var errorDescription: String? {
        switch self {
        case .muxaNotRunning: "Muxa is not running"
        case .muxaWindowNotFound: "No onscreen Muxa window was found"
        case .muxaActivationFailed: "Muxa could not be activated"
        case .menuItemNotFound(let path): "menu item not found: \(path)"
        case .pngEncodingFailed: "The captured image could not be encoded as PNG"
        case .captureTooLarge: "The captured PNG exceeds the 32 MiB safety limit"
        case .captureFailed: "Muxa window capture failed"
        case .eventCreationFailed: "A keyboard event could not be created"
        case .resizeFailed(let code): "The Muxa window could not be resized (AXError \(code))"
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
