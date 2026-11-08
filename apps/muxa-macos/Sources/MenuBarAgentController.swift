import AppKit
import Combine
import SwiftUI

/// A single status item owns the entire cluster. On macOS 26 the real menu
/// bar is rendered by the system: read screen coordinates, not tracking areas
/// or the status button's event.locationInWindow. Polling needs no new privacy
/// permission and also works when global mouse-move monitors receive nothing.
@MainActor
final class MuxaMenuBarAgentController: NSObject, NSPopoverDelegate {
    private let model: AppModel
    private let state = MuxaMenuBarState()
    private let item: NSStatusItem
    private var subscriptions: Set<AnyCancellable> = []
    private var order = MuxaMenuBarAgentOrder()
    private var candidates: [MuxaMenuBarAgent] = []
    private var chips: [MuxaMenuBarChip] = []
    private var availableChips: [MuxaMenuBarChip] = []
    private var overflow = 0
    private var configuredAgentLimit = MuxaPreferences.menuBarAgentLimit()
    private var grouping = MuxaMenuBarGrouping(rawValue: UserDefaults.standard.string(forKey: MuxaPreferences.menuBarGroupingKey) ?? "") ?? .agent
    private let registry = MuxaModuleRegistry.shared
    private var selectedChipID: String?
    private var showAasChip = UserDefaults.standard.object(forKey: MuxaPreferences.menuBarAasChipKey) as? Bool ?? true
    private var focus: Int?
    private var pointerInside = false
    private var lastPoint: NSPoint?
    private var timer: Timer?
    private var popover: NSPopover?
    private var keyMonitor: Any?
    private var peek: NSPanel?
    private var focusSince = Date()
    private var previousLook: NSAppearance.Name?
    private var appearanceObservation: NSKeyValueObservation?
    private var closingClickAt: Date?
    private var animationFrom: [CGFloat] = []
    private var positions: [CGFloat] = []
    private var pillWidth = MuxaMenuBarGeometry.disc
    private var drawnPillWidth = MuxaMenuBarGeometry.disc
    private var lead: CGFloat = 0
    private var animationPillFrom = MuxaMenuBarGeometry.disc
    private var animationLeadFrom: CGFloat = 0
    private var markOpacity: CGFloat = 1
    private var animationMarkFrom: CGFloat = 1
    private var animationStarted: Date?
    private var statusRefreshAt = Date()
    private var observers: [NSObjectProtocol] = []
    private var spaceObserver: NSObjectProtocol?
    private var refreshScheduled = false
    private var stopped = false
    private var pollingInterval: TimeInterval = 0
    private var lastRendered: MuxaMenuBarRenderState?
    private var lastAccessibility: MuxaMenuBarAccessibilityState?
    private var usageRefreshAt = Date()
    private var screenSleeping = false
    private var sleepObservers: [NSObjectProtocol] = []

    private var geometry: MuxaMenuBarGeometry { MuxaMenuBarGeometry(count: chips.count) }

    init(model: AppModel) {
        self.model = model
        item = NSStatusBar.system.statusItem(withLength: MuxaMenuBarGeometry.markWidth)
        super.init()
        item.autosaveName = "Muxa.AttentionCluster"
        if let button = item.button {
            button.target = self
            button.action = #selector(clicked)
            button.sendAction(on: [.leftMouseUp, .rightMouseUp])
            button.imagePosition = .imageOnly
            button.setAccessibilityElement(false)
            appearanceObservation = button.observe(\.effectiveAppearance, options: [.initial, .new]) { [weak self] _, _ in
                MainActor.assumeIsolated { self?.appearanceChanged() }
            }
        }
        // @Published fires before assignment. Defer the read until the model
        // and its attention center have both consumed the execution snapshot.
        model.$executionSnapshot.sink { [weak self] _ in self?.scheduleRefresh() }.store(in: &subscriptions)
        model.attention.$unreadPanes.sink { [weak self] _ in self?.scheduleRefresh() }.store(in: &subscriptions)
        model.$connectionState.sink { [weak self] _ in self?.scheduleRefresh() }.store(in: &subscriptions)
        observers.append(NotificationCenter.default.addObserver(forName: MuxaLimitCoverage.didChange, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.scheduleRefresh() }
        })
        registry.objectWillChange.sink { [weak self] _ in self?.scheduleRefresh() }.store(in: &subscriptions)
        if let aas = registry.module(AasModule.self) {
            aas.store.objectWillChange.sink { [weak self] _ in self?.scheduleRefresh() }.store(in: &subscriptions)
        }
        observers.append(NotificationCenter.default.addObserver(forName: UserDefaults.didChangeNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self else { return }
                let limit = MuxaPreferences.menuBarAgentLimit()
                let grouping = MuxaMenuBarGrouping(rawValue: UserDefaults.standard.string(forKey: MuxaPreferences.menuBarGroupingKey) ?? "") ?? .agent
                let aas = UserDefaults.standard.object(forKey: MuxaPreferences.menuBarAasChipKey) as? Bool ?? true
                guard limit != self.configuredAgentLimit || grouping != self.grouping || aas != self.showAasChip else { return }
                self.configuredAgentLimit = limit
                self.grouping = grouping
                self.showAasChip = aas
                self.scheduleRefresh()
            }
        })
        observers.append(NotificationCenter.default.addObserver(forName: NSApplication.didChangeScreenParametersNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.dismissForScreenChange() }
        })
        spaceObserver = NSWorkspace.shared.notificationCenter.addObserver(forName: NSWorkspace.activeSpaceDidChangeNotification, object: nil, queue: .main) { [weak self] _ in
            MainActor.assumeIsolated { self?.dismissForScreenChange() }
        }
        for (name, sleeping) in [(NSWorkspace.screensDidSleepNotification, true), (NSWorkspace.screensDidWakeNotification, false)] {
            sleepObservers.append(NSWorkspace.shared.notificationCenter.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated {
                    guard let self else { return }
                    self.screenSleeping = sleeping
                    self.lastPoint = nil
                    if sleeping { self.hidePeek() }
                    self.updatePollingRate()
                    if !sleeping { self.refresh() }
                }
            })
        }
        refresh()
        updatePollingRate()
    }

    // The app delegate explicitly tears down AppKit resources on termination.
    func stop() {
        stopped = true
        timer?.invalidate()
        timer = nil
        subscriptions.removeAll()
        appearanceObservation = nil
        hidePeek()
        popover?.close()
        if let keyMonitor { NSEvent.removeMonitor(keyMonitor) }
        keyMonitor = nil
        for observer in observers { NotificationCenter.default.removeObserver(observer) }
        observers.removeAll()
        if let spaceObserver { NSWorkspace.shared.notificationCenter.removeObserver(spaceObserver) }
        spaceObserver = nil
        for observer in sleepObservers { NSWorkspace.shared.notificationCenter.removeObserver(observer) }
        sleepObservers.removeAll()
        NSStatusBar.system.removeStatusItem(item)
    }

    private func scheduleRefresh() {
        guard !stopped, !refreshScheduled else { return }
        refreshScheduled = true
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            self.refreshScheduled = false
            self.refresh()
        }
    }

    private func refresh() {
        guard !stopped else { return }
        state.updateDraftAgents(model.executionSnapshot.hostedAgents, unread: model.attention.unreadPanes)
        candidates = order.update(model.executionSnapshot.hostedAgents, unread: model.attention.unreadPanes)
        var content = candidates
        if popover?.isShown == true {
            // Keep the reading/keyboard order stable while acknowledging cards.
            // Reviewed rows disappear together when the popover is dismissed.
            let live = Dictionary(uniqueKeysWithValues: model.executionSnapshot.hostedAgents.map { ($0.id, $0) })
            content = state.agents.compactMap { old in
                guard let hosted = live[old.id] else { return nil }
                let unread = MuxaAgentAttentionCenter.paneIdentity(for: hosted).map(model.attention.unreadPanes.contains) ?? false
                return MuxaMenuBarAgent(hosted: hosted, unread: unread)
            }
            let kept = Set(content.map(\.id))
            content.append(contentsOf: candidates.filter { !kept.contains($0.id) })
        }
        if let selected = state.selectedID, !content.contains(where: { $0.id == selected }),
           let hosted = model.executionSnapshot.hostedAgents.first(where: { $0.id == selected }) {
            content.append(MuxaMenuBarAgent(hosted: hosted, unread: false))
        }
        if let scope = state.scope {
            let kept = Set(content.map(\.id))
            content.append(contentsOf: model.executionSnapshot.hostedAgents.compactMap { hosted in
                guard !kept.contains(hosted.id) else { return nil }
                let pane = MuxaAgentAttentionCenter.paneIdentity(for: hosted)
                let item = MuxaMenuBarAgent(hosted: hosted, unread: pane.map(model.attention.unreadPanes.contains) ?? false)
                return scope.contains(item) ? item : nil
            })
        }
        state.agents = content
        let activity = MuxaMenuBarChip.activity(candidates, grouping: grouping)
        let aas = showAasChip && registry.isEnabled("aas") ? registry.module(AasModule.self) : nil
        let usage = aas.flatMap { $0.availability.isAvailable ? MuxaMenuBarChip.aas($0.store) : nil }
        availableChips = activity + (usage.map { [$0] } ?? [])
        // Freeze membership, capsule width and overflow while pointing or reading.
        // Existing chips still receive live status; vanished sessions cannot act.
        if !pointerInside && popover?.isShown != true && animationStarted == nil {
            // Bound physical width rather than agent count. Leave most of the
            // menu bar to the active app and the user's other status items.
            let screen = item.button?.window?.screen ?? NSScreen.main
            let budget = min(320, (screen?.frame.width ?? 1440) * 0.2)
            let count = MuxaMenuBarGeometry.visibleAgentCount(total: activity.count, maximumWidth: budget - (usage == nil ? 0 : 26),
                maximumAgents: configuredAgentLimit)
            chips = Array(activity.prefix(count)) + (usage.map { [$0] } ?? [])
            overflow = max(0, activity.count - count)
            focus = nil
            pillWidth = MuxaMenuBarAgentRenderer.pillWidth(chips)
            positions = geometry.layout(focus: nil, pillWidth: pillWidth)
            drawnPillWidth = MuxaMenuBarGeometry.disc
            lead = 0
            animationStarted = nil
            markOpacity = 1
        } else {
            chips = chips.map { old in
                if let current = availableChips.first(where: { $0.id == old.id }) { return current }
                if case .agent(let id) = old.destination, let item = content.first(where: { $0.id == id }) { return .agent(item) }
                return old
            }
        }
        render()
        updateAccessibility()
    }

    private func appearanceChanged() {
        let look = item.button?.effectiveAppearance.bestMatch(from: [.aqua, .darkAqua])
        guard look != previousLook else { return }
        previousLook = look
        render() // Setting the image can fire KVO again: compare first.
    }

    private func setFocus(_ index: Int?) {
        guard index != focus else { return }
        hidePeek()
        focus = index
        focusSince = Date()
        animationFrom = positions
        animationMarkFrom = markOpacity
        animationPillFrom = index == nil ? drawnPillWidth : MuxaMenuBarGeometry.disc
        drawnPillWidth = animationPillFrom
        animationLeadFrom = lead
        animationStarted = Date()
        updatePollingRate()
        updateAccessibility()
    }

    private func tick() {
        // Caps may expire without a new daemon snapshot.
        if Date().timeIntervalSince(statusRefreshAt) >= 15 {
            statusRefreshAt = Date()
            refresh()
            if showAasChip, registry.isEnabled("aas"), let aas = registry.module(AasModule.self), aas.availability.isAvailable,
               !aas.store.isRefreshing,
               MuxaMenuBarPolling.refreshUsage(chipShown: true, lastAttempt: usageRefreshAt, lastSuccess: aas.store.lastRefreshedAt, now: statusRefreshAt) {
                usageRefreshAt = statusRefreshAt
                Task { await aas.store.refresh() }
            }
        }
        let point = NSEvent.mouseLocation
        if point != lastPoint {
            lastPoint = point
            let inside = itemFrame(near: point).map { frame in
                point.x >= frame.minX && point.x < frame.maxX && point.y >= frame.minY && point.y <= frame.maxY + 1
            } ?? false
            let wasInside = pointerInside
            pointerInside = inside
            if inside, let frame = itemFrame(near: point) {
                setFocus(chipIndex(near: point, frame: frame))
            } else if wasInside {
                hidePeek()
                if popover?.isShown != true {
                    setFocus(nil)
                    if animationStarted == nil { refresh() }
                }
            }
        }
        if let start = animationStarted {
            let t = min(1, Date().timeIntervalSince(start) / 0.16)
            let ease = CGFloat(1 - pow(1 - t, 3))
            let targets = geometry.layout(focus: focus, pillWidth: pillWidth)
            positions = chips.indices.map { index in
                let target = targets[index]
                let from = animationFrom.indices.contains(index) ? animationFrom[index] : target
                return from + (target - from) * ease
            }
            let target: CGFloat = geometry.hidesMark(focus) ? 0 : 1
            markOpacity = animationMarkFrom + (target - animationMarkFrom) * ease
            let pillTarget = focus == nil ? MuxaMenuBarGeometry.disc : pillWidth
            drawnPillWidth = animationPillFrom + (pillTarget - animationPillFrom) * ease
            let leadTarget = focus == nil ? 0 : geometry.hoverLead(pillWidth: pillWidth)
            lead = (animationLeadFrom + (leadTarget - animationLeadFrom) * ease).rounded()
            if t >= 1 {
                animationStarted = nil
                if focus == nil && !pointerInside && popover?.isShown != true { refresh() }
            }
            render()
        }
        if pointerInside, popover?.isShown != true, peek == nil,
           let focus, Date().timeIntervalSince(focusSince) >= 0.3 {
            showPeek(focus)
        }
        updatePollingRate()
    }

    private func updatePollingRate() {
        guard !stopped else { return }
        if screenSleeping {
            timer?.invalidate()
            timer = nil
            pollingInterval = 0
            return
        }
        let interval = MuxaMenuBarPolling.interval(pointerInside: pointerInside, animating: animationStarted != nil, popoverShown: popover?.isShown == true)
        guard interval != pollingInterval else { return }
        pollingInterval = interval
        timer?.invalidate()
        let timer = Timer(timeInterval: interval, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated { self?.tick() }
        }
        timer.tolerance = interval / 4
        RunLoop.main.add(timer, forMode: .common)
        self.timer = timer
    }

    /// Translate the offscreen stand-in to the pointer's display, retaining
    /// the distance from that display's right edge (BarShelf's replicant fix).
    private func itemFrame(near point: NSPoint) -> NSRect? {
        guard let button = item.button, let window = button.window,
              let home = window.screen ?? NSScreen.main,
              let screen = NSScreen.screens.first(where: { NSMouseInRect(point, $0.frame, false) }) else { return nil }
        let rect = window.convertToScreen(button.convert(button.bounds, to: nil))
        let fromRight = home.frame.maxX - rect.minX
        let height = max(screen.frame.maxY - screen.visibleFrame.maxY, NSStatusBar.system.thickness)
        return NSRect(x: screen.frame.maxX - fromRight, y: screen.frame.maxY - height, width: rect.width, height: height)
    }

    @objc private func clicked() {
        // A transient popover may have closed on this click's mouse-down.
        if let closingClickAt, Date().timeIntervalSince(closingClickAt) < 0.6 {
            self.closingClickAt = nil
            return
        }
        let point = NSEvent.mouseLocation
        if NSApp.currentEvent?.type != .rightMouseUp,
           let frame = itemFrame(near: point),
           let index = chipIndex(near: point, frame: frame), chips.indices.contains(index) {
            showChip(chips[index])
        } else {
            state.scope = nil
            state.showingUsage = false
            selectedChipID = nil
            show(selected: nil)
        }
    }

    private func showAgent(_ id: String) {
        guard state.displayedAgents.contains(where: { $0.id == id }) else { show(selected: nil); return }
        show(selected: id)
    }

    private func showChip(_ chip: MuxaMenuBarChip) {
        if popover?.isShown == true { popover?.performClose(nil); return }
        selectedChipID = chip.id
        switch chip.destination {
        case .agent(let id):
            state.scope = nil
            state.showingUsage = false
            showAgent(id)
        case .group(let scope):
            state.scope = scope
            state.showingUsage = false
            refresh()
            show(selected: nil)
        case .aas:
            guard registry.isEnabled("aas") else { return }
            state.scope = nil
            state.showingUsage = true
            show(selected: nil)
        }
    }

    private func show(selected: String?) {
        hidePeek()
        if popover?.isShown == true {
            if state.selectedID == selected { popover?.performClose(nil); return }
            state.selectedID = selected
            acknowledge(selected)
            return
        }
        state.selectedID = selected
        guard let button = item.button else { return }
        let surface = NSPopover()
        surface.behavior = .transient
        surface.animates = false
        surface.delegate = self
        let hosting = NSHostingController(rootView: MuxaMenuBarContent(
            model: model, state: state, attention: model.attention,
            dismiss: { [weak self] in self?.popover?.performClose(nil) },
            select: { [weak self] id in self?.selectInCard(id) },
            selectScope: { [weak self] scope in self?.selectScope(scope) }
        ))
        hosting.sizingOptions = [.preferredContentSize]
        surface.contentViewController = hosting
        popover = surface
        let index = chips.firstIndex { $0.id == selectedChipID }
        if let index { setFocus(index) }
        surface.show(relativeTo: index.map(chipRect) ?? markRect, of: button, preferredEdge: .minY)
        if surface.isShown { acknowledge(selected) }
        surface.contentViewController?.view.window?.makeKey()
        installKeyMonitor()
        render()
    }

    private func selectInCard(_ id: String?) {
        state.selectedID = id
        acknowledge(id)
        state.showingUsage = false
        let index = id.flatMap { id in chips.firstIndex(where: { $0.memberIDs.contains(id) }) }
        selectedChipID = index.map { chips[$0].id }
        setFocus(index)
        refresh()
    }

    private func selectScope(_ scope: MuxaMenuBarScope) {
        state.scope = scope
        state.selectedID = nil
        state.showingUsage = false
        selectedChipID = scope.key
        setFocus(chips.firstIndex { $0.id == scope.key })
        refresh()
    }

    /// Opening a named card counts as viewing that session; hovering does not.
    /// Membership stays frozen until the card/pointer leave, avoiding jumps.
    private func acknowledge(_ id: String?) {
        guard let id, let agent = state.displayedAgents.first(where: { $0.id == id }),
              let pane = agent.pane else { return }
        model.attention.markRead(pane, agentSessionID: agent.hosted.agent.agentSessionID)
    }

    func popoverDidClose(_ notification: Notification) {
        if NSApp.currentEvent?.type == .leftMouseDown || NSApp.currentEvent?.type == .rightMouseDown { closingClickAt = Date() }
        if let keyMonitor { NSEvent.removeMonitor(keyMonitor) }
        keyMonitor = nil
        popover?.contentViewController = nil
        popover = nil
        state.selectedID = nil
        state.scope = nil
        state.showingUsage = false
        selectedChipID = nil
        if !pointerInside { setFocus(nil) }
        if animationStarted == nil { refresh() } else { render() }
    }

    private func installKeyMonitor() {
        keyMonitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { [weak self] event in
            let consumed = MainActor.assumeIsolated {
                guard let self, event.window == self.popover?.contentViewController?.view.window,
                      !(event.window?.firstResponder is NSTextView),
                      event.modifierFlags.intersection([.command, .control, .option, .shift]).isEmpty,
                      !self.state.showingUsage,
                      [123, 124].contains(event.keyCode), !self.state.visibleAgents.isEmpty else { return false }
                let current = self.state.visibleAgents.firstIndex { $0.id == self.state.selectedID }
                let next = current.map { $0 + (event.keyCode == 123 ? -1 : 1) } ?? 0
                if self.state.visibleAgents.indices.contains(next) { self.selectInCard(self.state.visibleAgents[next].id) }
                return true
            }
            return consumed ? nil : event
        }
    }

    private func showPeek(_ index: Int) {
        guard chips.indices.contains(index), let frame = itemFrame(near: NSEvent.mouseLocation) else { return }
        let agent = chips[index]
        let panel = NSPanel(contentRect: .zero, styleMask: [.borderless, .nonactivatingPanel], backing: .buffered, defer: true)
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = true
        panel.ignoresMouseEvents = true
        panel.level = .popUpMenu
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .ignoresCycle]
        let view = NSHostingView(rootView: MuxaMenuBarChipPeek(item: agent))
        let size = view.fittingSize
        let screen = NSScreen.screens.first { NSMouseInRect(NSEvent.mouseLocation, $0.frame, false) }
        let center = frame.minX + chipRect(index).midX
        let x = min(max(center - size.width / 2, screen?.frame.minX ?? 0), (screen?.frame.maxX ?? center + size.width) - size.width)
        panel.contentView = view
        panel.setFrame(NSRect(x: x, y: frame.minY - size.height - 5, width: size.width, height: size.height), display: true)
        panel.orderFrontRegardless()
        peek = panel
    }

    private func hidePeek() {
        peek?.orderOut(nil)
        peek?.contentView = nil
        peek = nil
    }

    private func dismissForScreenChange() {
        hidePeek()
        popover?.close()
        pointerInside = false
        lastPoint = nil
        refresh()
    }

    /// Stack origin inside the real button, including its system-added
    /// padding and the animated extension. Use this for every hit and anchor.
    private var clusterOrigin: CGFloat {
        guard let button = item.button, let image = button.image else { return lead }
        return MuxaMenuBarGeometry.origin(buttonWidth: button.bounds.width, imageWidth: image.size.width, lead: lead)
    }

    private func chipIndex(near point: NSPoint, frame: NSRect) -> Int? {
        let imageHeight = item.button?.image?.size.height ?? 24
        let imagePoint = CGPoint(x: point.x - frame.minX - clusterOrigin,
            y: point.y - frame.minY - (frame.height - imageHeight) / 2)
        return MuxaMenuBarPaintGeometry(count: chips.count, positions: positions, focus: focus, pillWidth: drawnPillWidth,
            badged: Set(chips.indices.filter { chips[$0].status.needsAttention || chips[$0].unread })).index(at: imagePoint)
    }

    private func chipRect(_ index: Int) -> NSRect {
        let xs = geometry.layout(focus: focus, pillWidth: pillWidth)
        let x = xs.indices.contains(index) ? xs[index] : 0
        let width = index == focus ? max(MuxaMenuBarGeometry.slot, pillWidth + 2 * MuxaMenuBarGeometry.inset) : MuxaMenuBarGeometry.slot
        return NSRect(x: clusterOrigin + x, y: 0, width: width, height: item.button?.bounds.height ?? 24)
    }

    private var markRect: NSRect {
        NSRect(x: clusterOrigin + geometry.markX, y: 0, width: MuxaMenuBarGeometry.markWidth, height: item.button?.bounds.height ?? 24)
    }

    private func render() {
        guard !stopped, !screenSleeping, let button = item.button else { return }
        let mark: String
        switch model.connectionState {
        case .failed, .upgradeRequired: mark = "terminal.fill"
        case .connecting, .connected: mark = "terminal"
        }
        let liveAgents = Set(state.agents.map(\.id))
        let liveIDs = Set(availableChips.map(\.id)).union(chips.filter { !$0.memberIDs.isDisjoint(with: liveAgents) }.map(\.id))
        let current = MuxaMenuBarRenderState(chips: chips, positions: positions, focus: focus, pillWidth: drawnPillWidth,
            lead: lead, markOpacity: markOpacity, overflow: overflow, selectedID: selectedChipID,
            liveIDs: liveIDs, dark: previousLook == .darkAqua, mark: mark)
        guard current != lastRendered else { return }
        lastRendered = current
        let image = MuxaMenuBarAgentRenderer.image(
            chips: chips, positions: positions, focus: focus, pillWidth: drawnPillWidth,
            lead: lead, markOpacity: markOpacity, overflow: overflow, selectedID: selectedChipID,
            liveIDs: liveIDs, dark: previousLook == .darkAqua, mark: mark
        )
        item.length = image.size.width
        button.image = image
        updateAccessibility()
        if popover?.isShown == true {
            let selected = chips.firstIndex { $0.id == selectedChipID }
            popover?.positioningRect = selected.map(chipRect) ?? markRect
        }
    }

    private func updateAccessibility() {
        guard let button = item.button else { return }
        let labels = chips.map { "\($0.name), \($0.location), \($0.menuStatusLabel)" }
        let rects = chips.indices.map(chipRect) + [markRect]
        let current = MuxaMenuBarAccessibilityState(ids: chips.map(\.id), labels: labels, rects: rects, overflow: overflow)
        guard current != lastAccessibility else { return }
        lastAccessibility = current
        var elements: [NSAccessibilityElement] = []
        for (index, agent) in chips.enumerated() {
            let element = MuxaMenuBarElement(label: "\(agent.name), \(agent.location), \(agent.menuStatusLabel)", parent: button,
                rect: chipRect(index)) { [weak self] in self?.showChip(agent) }
            elements.append(element)
        }
        elements.append(MuxaMenuBarElement(label: "Muxa", parent: button, rect: markRect) { [weak self] in self?.show(selected: nil) })
        if overflow > 0 {
            elements.append(MuxaMenuBarElement(label: String(localized: "\(overflow) more items"), parent: button,
                rect: NSRect(x: clusterOrigin + geometry.width, y: 0, width: MuxaMenuBarGeometry.overflowWidth, height: 24)) { [weak self] in self?.show(selected: nil) })
        }
        button.setAccessibilityChildren(elements)
        button.toolTip = nil // The hover panel follows the individual chip.
    }
}

@MainActor
private final class MuxaMenuBarElement: NSAccessibilityElement {
    private nonisolated let press: @MainActor @Sendable () -> Void
    init(label: String, parent: NSView, rect: NSRect, press: @escaping @MainActor @Sendable () -> Void) {
        self.press = press
        super.init()
        setAccessibilityRole(.button)
        setAccessibilityLabel(label)
        setAccessibilityParent(parent)
        setAccessibilityFrameInParentSpace(rect)
    }
    nonisolated override func accessibilityPerformPress() -> Bool {
        let action = press
        return MainActor.assumeIsolated { action(); return true }
    }
}
