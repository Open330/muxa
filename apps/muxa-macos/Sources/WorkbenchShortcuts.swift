import SwiftUI

/// Agents that are stuck on the operator: waiting for input or a choice,
/// blocked, or failed. They come first wherever the workbench lists agents
/// (the ⌘J palette, ⇧⌘J), so a blocked agent surfaces itself instead of being
/// hunted for pane by pane.
enum MuxaAttention {
    static let states: Set<String> = ["waiting_input", "waiting_choice", "blocked", "error", "failed"]
    static let activeStates: Set<String> = ["working", "starting"]

    static func needsAttention(_ pane: MuxaWatchPane) -> Bool {
        pane.agent.map { states.contains($0.state) } ?? false
    }

    /// Attention first, then working, then idle agents; plain shells last.
    static func rank(_ pane: MuxaWatchPane) -> Int {
        guard let state = pane.agent?.state else { return 3 }
        if states.contains(state) { return 0 }
        if activeStates.contains(state) { return 1 }
        return 2
    }

    /// ⇧⌘J: the pane after `current` among those needing attention, wrapping
    /// around; the first one when `current` is not one of them.
    static func next(
        after current: MuxaWatchPaneIdentity?,
        in candidates: [MuxaWatchPaneIdentity]
    ) -> MuxaWatchPaneIdentity? {
        guard !candidates.isEmpty else { return nil }
        guard let current, let index = candidates.firstIndex(of: current) else {
            return candidates.first
        }
        return candidates[(index + 1) % candidates.count]
    }
}

/// Every workbench shortcut in one list, for the ⌘/ reference sheet. The
/// menus define the bindings; keep this list in step with them.
enum MuxaShortcutCatalog {
    struct Entry: Identifiable {
        let keys: String
        let title: String
        var id: String { keys + title }
    }

    struct Section: Identifiable {
        let title: String
        let entries: [Entry]
        var id: String { title }
    }

    static var sections: [Section] {
        [
            Section(title: String(localized: "Go to"), entries: [
                Entry(keys: "⌘P", title: String(localized: "Go to Anything")),
                Entry(keys: "⇧⌘P", title: String(localized: "Command Palette")),
                Entry(keys: "⌘J", title: String(localized: "Jump to Agent (needing attention first)")),
                Entry(keys: "⇧⌘J", title: String(localized: "Next Agent Needing Attention")),
                Entry(keys: "⌘1 … ⌘9", title: String(localized: "Open result in Jump to Agent")),
                Entry(keys: "⌃⇧G", title: String(localized: "Show Changes")), // WS-D
            ]),
            Section(title: String(localized: "Editors"), entries: [
                Entry(keys: "⌘W", title: String(localized: "Close Editor (the window once none is open)")),
                Entry(keys: "⇧⌘T", title: String(localized: "Reopen Closed Editor")),
                Entry(keys: "⌃⇥ / ⌃⇧⇥", title: String(localized: "Next / Previous Editor")),
                Entry(keys: "⌘1 … ⌘8", title: String(localized: "Go to Editor 1–8")),
                Entry(keys: "⌘9", title: String(localized: "Last Editor")),
                Entry(keys: "⌘\\", title: String(localized: "Split Editor Right")),
                Entry(keys: "⌥⌘←  ⌥⌘→", title: String(localized: "Focus Previous / Next Editor Group")),
                Entry(keys: "⌥⌘↩", title: String(localized: "Keep Editor Open")),
            ]),
            Section(title: String(localized: "Views"), entries: [
                Entry(keys: "⌘B", title: String(localized: "Toggle Side Bar")),
                Entry(keys: "⇧⌘F", title: String(localized: "Focus Side Bar Filter")),
                Entry(keys: "⇧⌘1", title: String(localized: "Open Work Command Center")),
                Entry(keys: "⇧⌘2", title: String(localized: "Open Ask")),
                Entry(keys: "⇧⌘3", title: String(localized: "Open Inbox")),
                Entry(keys: "⇧⌘W", title: String(localized: "Open Live Watch")),
            ]),
            Section(title: String(localized: "Work & Shells"), entries: [
                Entry(keys: "⌥⌘N", title: String(localized: "Start Work")),
                Entry(keys: "⌘T", title: String(localized: "New Shell")),
                Entry(keys: "⌘↩", title: String(localized: "Send Prompt from a Composer")),
            ]),
            // WS-D
            Section(title: String(localized: "Changes"), entries: [
                Entry(keys: "⌘R", title: String(localized: "Refresh Changes")),
                Entry(keys: "⌥⌘C", title: String(localized: "Comment on Selected Lines")),
                Entry(keys: "⌘↩", title: String(localized: "Add Comment / Send Review")),
                Entry(keys: "⎋", title: String(localized: "Cancel Comment")),
            ]),
            Section(title: String(localized: "Terminal"), entries: [
                Entry(keys: "⌘C  ⌘V", title: String(localized: "Copy / Paste")),
                Entry(keys: "⌘A", title: String(localized: "Select All")),
                Entry(keys: "⌘K", title: String(localized: "Clear Screen")),
                Entry(keys: "⌘+  ⌘−  ⌘0", title: String(localized: "Bigger / Smaller / Reset Text")),
                Entry(keys: "⌘↑  ⌘↓", title: String(localized: "Previous / Next Prompt")),
            ]),
            Section(title: String(localized: "Help"), entries: [
                Entry(keys: "⌘/", title: String(localized: "Keyboard Shortcuts")),
                Entry(keys: "⌘,", title: String(localized: "Settings")),
            ]),
        ]
    }
}

/// ⌘/: every shortcut at a glance, filterable, like herdr's `prefix+?` and
/// VS Code's keyboard shortcuts reference.
struct MuxaShortcutsSheet: View {
    let dismiss: () -> Void
    @State private var filter = ""
    @FocusState private var filterFocused: Bool
    @Environment(\.colorScheme) private var colorScheme

    private var sections: [MuxaShortcutCatalog.Section] {
        let query = filter.trimmingCharacters(in: .whitespaces)
        guard !query.isEmpty else { return MuxaShortcutCatalog.sections }
        return MuxaShortcutCatalog.sections.compactMap { section in
            let entries = section.entries.filter {
                $0.title.localizedCaseInsensitiveContains(query) || $0.keys.contains(query)
            }
            return entries.isEmpty ? nil : .init(title: section.title, entries: entries)
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                Text("Keyboard Shortcuts")
                    .font(.system(size: 13, weight: .semibold))
                Spacer()
                MuxaFilterField(prompt: String(localized: "Filter shortcuts"), text: $filter, focused: $filterFocused) {
                    EmptyView()
                }
                .frame(width: 220)
            }
            .padding(.horizontal, 16)
            .padding(.vertical, 12)

            MuxaTheme.border(colorScheme).frame(height: 1)

            ScrollView {
                LazyVStack(alignment: .leading, spacing: 18) {
                    ForEach(sections) { section in
                        VStack(alignment: .leading, spacing: 2) {
                            Text(verbatim: section.title.uppercased())
                                .font(.system(size: 11, weight: .semibold))
                                .tracking(0.4)
                                .foregroundStyle(.secondary)
                                .padding(.bottom, 4)
                            ForEach(section.entries) { entry in
                                HStack {
                                    Text(verbatim: entry.title)
                                        .font(.system(size: 12))
                                    Spacer(minLength: 12)
                                    Text(verbatim: entry.keys)
                                        .font(.system(size: 11, weight: .medium).monospaced())
                                        .padding(.horizontal, 6)
                                        .frame(minHeight: 20)
                                        .background(
                                            RoundedRectangle(cornerRadius: 4, style: .continuous)
                                                .fill(MuxaTheme.segmentTrack(colorScheme))
                                        )
                                }
                                .frame(minHeight: 26)
                            }
                        }
                    }
                }
                .padding(16)
            }

            MuxaTheme.border(colorScheme).frame(height: 1)

            HStack {
                Text("Shortcuts act on the focused editor; a focused terminal keeps only its own copy, paste, and scroll keys.")
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                Spacer()
                Button("Done", action: dismiss)
                    .buttonStyle(.muxaPrimary)
                    .keyboardShortcut(.defaultAction)
            }
            .padding(12)
        }
        .frame(width: 560, height: 600)
        .background(MuxaTheme.editor(colorScheme))
        .onExitCommand(perform: dismiss)
        .onAppear { filterFocused = true }
    }
}
