import AppKit
import SwiftUI

// The workbench window has no title bar of its own (`.hiddenTitleBar`): like
// Safari's compact tabs, the top row of each column doubles as the title bar.
// Over the side bar it holds the traffic lights and the view's title; over
// the editors it is the tab strip, with the window's actions trailing. These
// helpers give that row the title bar's height, keep the traffic lights clear,
// and let its empty parts move the window.

struct TitleBarMetrics: Equatable {
    /// The hidden title bar's own height; the traffic lights are centered in
    /// it.
    var height: CGFloat
    /// Where content may start without running under the traffic lights.
    var leadingInset: CGFloat

    /// The top row: the title bar's height, but never tighter than a tab.
    var rowHeight: CGFloat { max(height, 32) }

    static let fallback = TitleBarMetrics(height: 32, leadingInset: 80)
}

/// Measures the hidden title bar's height and how far the traffic lights
/// reach, and measures again whenever the window resizes or enters or leaves
/// full screen (where the traffic lights are gone from the bar).
struct TitleBarMetricsReader: NSViewRepresentable {
    @Binding var metrics: TitleBarMetrics

    func makeNSView(context: Context) -> ReaderView {
        let view = ReaderView()
        view.onChange = { metrics = $0 }
        return view
    }

    func updateNSView(_ nsView: ReaderView, context: Context) {
        nsView.onChange = { metrics = $0 }
    }

    final class ReaderView: NSView {
        var onChange: ((TitleBarMetrics) -> Void)?
        private var observers: [NSObjectProtocol] = []

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            observers.forEach(NotificationCenter.default.removeObserver)
            observers = []
            guard let window else { return }
            let names: [Notification.Name] = [
                NSWindow.didResizeNotification,
                NSWindow.didEnterFullScreenNotification,
                NSWindow.didExitFullScreenNotification,
            ]
            observers = names.map { name in
                NotificationCenter.default.addObserver(
                    forName: name, object: window, queue: .main
                ) { [weak self] _ in
                    MainActor.assumeIsolated { self?.measure() }
                }
            }
            measure()
        }

        private func measure() {
            guard let window else { return }
            let titleBarHeight = window.frame.height - window.contentLayoutRect.height
            let fullScreen = window.styleMask.contains(.fullScreen)
            var leading: CGFloat = 12
            if !fullScreen, let zoom = window.standardWindowButton(.zoomButton), let superview = zoom.superview {
                leading = superview.convert(zoom.frame, to: nil).maxX + 12
            }
            let measured = TitleBarMetrics(
                height: fullScreen ? TitleBarMetrics.fallback.height : max(28, titleBarHeight.rounded()),
                leadingInset: leading.rounded()
            )
            DispatchQueue.main.async { [weak self] in
                self?.onChange?(measured)
            }
        }
    }
}

/// The empty parts of the title bar move the window, and a double-click does
/// what the operator chose in System Settings › Desktop & Dock.
struct WindowDragArea: NSViewRepresentable {
    func makeNSView(context: Context) -> DragView { DragView() }
    func updateNSView(_ nsView: DragView, context: Context) {}

    final class DragView: NSView {
        override var mouseDownCanMoveWindow: Bool { true }

        override func mouseDown(with event: NSEvent) {
            guard let window else { return }
            if event.clickCount == 2 {
                switch UserDefaults.standard.string(forKey: "AppleActionOnDoubleClick") {
                case "Minimize": window.performMiniaturize(nil)
                case "None": break
                default: window.performZoom(nil)
                }
                return
            }
            window.performDrag(with: event)
        }
    }
}
