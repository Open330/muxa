// Home's review sheet: the results of work the person asked for, three ways
// through them (one card at a time, an inbox, a digest), one set of actions.
import SwiftUI

struct FleetReviewView: View {
    @ObservedObject var model: AppModel
    /// The board as it is now; the queue is worked out from it.
    let works: () -> [FleetWork]
    @Binding var marks: FleetReviewMarks
    let draft: (String) -> Binding<FleetReplyDraft>
    let submit: (String) -> Void
    let open: (MuxaHostedAgent) -> Void
    let markReviewed: (FleetWork) -> Void
    @Environment(\.dismiss) private var dismiss

    @AppStorage("home.review.mode") private var mode: FleetReviewMode = .cards
    @AppStorage("home.review.scope") private var scope: FleetReviewScope = .day
    /// Units put off with Later in this session: they go to the end.
    @State private var later: [String] = []

    private var queue: [FleetWork] {
        let base = FleetBoard.reviewQueue(works(), marks: marks, scope: scope)
        let deferred = Set(later)
        return base.filter { !deferred.contains($0.id) }
            + later.compactMap { id in base.first { $0.id == id } }
    }

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            let queue = queue
            Group {
                if queue.isEmpty {
                    caughtUp
                } else {
                    switch mode {
                    case .cards: FleetReviewCards(queue: queue, actions: actions)
                    case .inbox: FleetReviewInbox(queue: queue, actions: actions)
                    case .digest: FleetReviewDigest(queue: queue, actions: actions)
                    }
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .frame(minWidth: 860, idealWidth: 960, minHeight: 620, idealHeight: 720)
    }

    private var actions: FleetReviewActions {
        FleetReviewActions(
            looksGood: { work in
                later.removeAll { $0 == work.id }
                markReviewed(work)
            },
            later: { work in
                later.removeAll { $0 == work.id }
                later.append(work.id)
            },
            open: { hosted in
                dismiss()
                open(hosted)
            },
            draft: draft,
            submit: submit
        )
    }

    private var header: some View {
        HStack(spacing: 12) {
            Text("Review")
                .font(.system(size: 17, weight: .semibold))
            Text("\(queue.count)")
                .font(.system(size: 13, weight: .semibold))
                .monospacedDigit()
                .foregroundStyle(.secondary)
            Picker("Mode", selection: $mode) {
                ForEach(FleetReviewMode.allCases) { mode in
                    Label(mode.title, systemImage: mode.systemImage).tag(mode)
                }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .fixedSize()
            .help("Three ways through the same results; pick the one that works best")
            Spacer(minLength: 8)
            Picker("Range", selection: $scope) {
                ForEach(FleetReviewScope.allCases) { scope in
                    Text(scope.title).tag(scope)
                }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .fixedSize()
            Button("Done") { dismiss() }
                .keyboardShortcut(.cancelAction)
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 12)
    }

    private var caughtUp: some View {
        VStack(spacing: 10) {
            Image(systemName: "checkmark.circle")
                .font(.system(size: 40))
                .foregroundStyle(.green)
            Text("All caught up")
                .font(.title3.weight(.semibold))
            Text(scope == .all
                ? String(localized: "Every result you asked for has been reviewed.")
                : String(localized: "Nothing left to review in this range. Widen the range to see older results."))
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
        }
        .padding(32)
    }
}

/// The three things to do with a result, shared by every mode.
struct FleetReviewActions {
    let looksGood: (FleetWork) -> Void
    let later: (FleetWork) -> Void
    let open: (MuxaHostedAgent) -> Void
    let draft: (String) -> Binding<FleetReplyDraft>
    let submit: (String) -> Void
}

// MARK: - The result itself

/// One result: where it ran, what was asked, what came back.
struct FleetReviewDetail: View {
    let work: FleetWork
    var resultLines: Int?

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            VStack(alignment: .leading, spacing: 4) {
                Text(work.title)
                    .font(.system(size: 17, weight: .semibold))
                    .fixedSize(horizontal: false, vertical: true)
                HStack(spacing: 6) {
                    if let repo = work.repo { Text(repo) }
                    if let focus = work.focus { Text("· \(FleetBoard.owner(focus))") }
                    if !work.helpers.isEmpty {
                        Label("\(work.helpers.count)", systemImage: "person.2")
                    }
                    if let changed = work.lastChange ?? work.since {
                        Text("·")
                        Text(changed, format: .relative(presentation: .numeric, unitsStyle: .abbreviated))
                            .monospacedDigit()
                    }
                }
                .font(MuxaType.meta)
                .foregroundStyle(.secondary)
            }
            if let asked = FleetBoard.reviewAsked(work) {
                block("Asked") {
                    Text(asked)
                        .font(MuxaType.body)
                        .foregroundStyle(.secondary)
                        .fixedSize(horizontal: false, vertical: true)
                        .textSelection(.enabled)
                }
            }
            block("Result") {
                if let result = FleetBoard.reviewResult(work) {
                    MarkdownContent(source: result, lineLimit: resultLines, selectable: true, font: MuxaType.body)
                        .frame(maxWidth: .infinity, alignment: .leading)
                } else {
                    Text(work.headline)
                        .font(MuxaType.body)
                }
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    private func block<Content: View>(_ title: LocalizedStringKey, @ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 5) {
            Text(title)
                .font(.system(size: 11, weight: .bold))
                .foregroundStyle(.secondary)
                .textCase(.uppercase)
            content()
        }
    }
}

/// Looks Good, Later, Send Back and Open, with their keys.
private struct FleetReviewButtons: View {
    let work: FleetWork
    let actions: FleetReviewActions
    @Binding var sendingBack: Bool
    var keys: Bool = true

    var body: some View {
        HStack(spacing: 10) {
            button("Later", systemImage: "arrow.left", key: .leftArrow) { actions.later(work) }
            button("Send Back", systemImage: "arrow.uturn.down", key: KeyEquivalent("r")) { sendingBack.toggle() }
            if let focus = work.focus {
                button("Open", systemImage: "arrow.up.forward.square", key: .space) { actions.open(focus) }
            }
            Spacer(minLength: 0)
            Button {
                actions.looksGood(work)
            } label: {
                Label("Looks Good", systemImage: "checkmark")
            }
            .buttonStyle(.muxaPrimary)
            .modifier(Shortcut(key: .rightArrow, enabled: keys))
            .help("Looks Good (→)")
        }
    }

    private func button(_ title: LocalizedStringKey, systemImage: String, key: KeyEquivalent, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Label(title, systemImage: systemImage)
        }
        .buttonStyle(.muxaSecondary)
        .modifier(Shortcut(key: key, enabled: keys))
    }

    /// Keys stay off while the reply field takes typing.
    private struct Shortcut: ViewModifier {
        let key: KeyEquivalent
        let enabled: Bool

        func body(content: Content) -> some View {
            if enabled {
                content.keyboardShortcut(key, modifiers: [])
            } else {
                content
            }
        }
    }
}

/// The reply that sends a result back, when Send Back is open.
private struct FleetReviewSendBack: View {
    let work: FleetWork
    let actions: FleetReviewActions

    var body: some View {
        if FleetBoard.replyTarget(for: work) != nil {
            FleetReplyField(work: work, draft: actions.draft(work.id), submit: { actions.submit(work.id) })
        } else {
            Text("This agent is not at its prompt; open it to send it back.")
                .font(MuxaType.meta)
                .foregroundStyle(.secondary)
        }
    }
}

// MARK: - Cards: one at a time

private struct FleetReviewCards: View {
    let queue: [FleetWork]
    let actions: FleetReviewActions
    @State private var offset: CGFloat = 0
    @State private var sendingBack = false

    var body: some View {
        if let work = queue.first {
            VStack(spacing: 16) {
                HStack {
                    Text("1 of \(queue.count)")
                        .font(MuxaType.meta)
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                    Spacer()
                    Text("← Later   R Send Back   Space Open   → Looks Good")
                        .font(MuxaType.meta)
                        .foregroundStyle(.tertiary)
                }
                ZStack {
                    // The next card, peeking under the current one.
                    if queue.count > 1 {
                        RoundedRectangle(cornerRadius: 14, style: .continuous)
                            .fill(MuxaTheme.panelFill)
                            .padding(.horizontal, 18)
                            .offset(y: 10)
                    }
                    card(work)
                        .offset(x: offset)
                        .rotationEffect(.degrees(Double(offset) / 40))
                        .gesture(swipe(work))
                }
                FleetReviewButtons(work: work, actions: actions, sendingBack: $sendingBack, keys: !sendingBack)
                if sendingBack {
                    FleetReviewSendBack(work: work, actions: actions)
                }
            }
            .padding(24)
            .frame(maxWidth: 760)
            .id(work.id)
            .onChange(of: work.id) { _ in
                offset = 0
                sendingBack = false
            }
        }
    }

    private func card(_ work: FleetWork) -> some View {
        ScrollView {
            FleetReviewDetail(work: work)
                .padding(22)
        }
        .frame(maxHeight: .infinity)
        .background(.background, in: RoundedRectangle(cornerRadius: 14, style: .continuous))
        .overlay(alignment: .topLeading) { stamp }
        .overlay(RoundedRectangle(cornerRadius: 14, style: .continuous).strokeBorder(Color.secondary.opacity(0.25)))
        .shadow(color: .black.opacity(0.12), radius: 12, y: 4)
    }

    /// Which way the card is being swiped.
    @ViewBuilder
    private var stamp: some View {
        if offset > 40 {
            Label("Looks Good", systemImage: "checkmark").foregroundStyle(.green).padding(16)
        } else if offset < -40 {
            Label("Later", systemImage: "clock").foregroundStyle(.orange).padding(16)
        }
    }

    private func swipe(_ work: FleetWork) -> some Gesture {
        DragGesture(minimumDistance: 20)
            .onChanged { offset = $0.translation.width }
            .onEnded { value in
                let distance = value.translation.width
                if distance > 140 {
                    actions.looksGood(work)
                } else if distance < -140 {
                    actions.later(work)
                }
                withAnimation(.spring(response: 0.3, dampingFraction: 0.8)) { offset = 0 }
            }
    }
}

// MARK: - Inbox: a list and the selected result

private struct FleetReviewInbox: View {
    let queue: [FleetWork]
    let actions: FleetReviewActions
    @State private var selectedID: String?
    @State private var sendingBack = false
    @State private var confirmingAll = false

    private var selected: FleetWork? {
        queue.first { $0.id == selectedID } ?? queue.first
    }

    var body: some View {
        HStack(spacing: 0) {
            VStack(spacing: 0) {
                List(selection: Binding(get: { selected?.id }, set: { selectedID = $0 })) {
                    ForEach(queue) { work in
                        row(work).tag(work.id)
                    }
                }
                .listStyle(.inset)
                Divider()
                HStack {
                    if confirmingAll {
                        Text("Mark all \(queue.count) reviewed?")
                            .font(MuxaType.meta)
                        Button("Mark All") {
                            queue.forEach(actions.looksGood)
                            confirmingAll = false
                        }
                        .controlSize(.small)
                        Button("Cancel") { confirmingAll = false }
                            .controlSize(.small)
                    } else {
                        Button("Mark All Reviewed…") { confirmingAll = true }
                            .buttonStyle(.muxaGhost)
                            .controlSize(.small)
                    }
                    Spacer()
                    Text("J/K Move")
                        .font(MuxaType.meta)
                        .foregroundStyle(.tertiary)
                }
                .padding(8)
            }
            .frame(width: 320)
            Divider()
            if let work = selected {
                VStack(spacing: 14) {
                    ScrollView {
                        FleetReviewDetail(work: work)
                            .padding(20)
                    }
                    VStack(spacing: 10) {
                        FleetReviewButtons(work: work, actions: actions, sendingBack: $sendingBack, keys: !sendingBack)
                        if sendingBack {
                            FleetReviewSendBack(work: work, actions: actions)
                        }
                    }
                    .padding([.horizontal, .bottom], 16)
                }
                .id(work.id)
                .onChange(of: work.id) { _ in sendingBack = false }
            }
        }
        .background {
            // J and K move through the list.
            Button("") { move(1) }.keyboardShortcut("j", modifiers: []).opacity(0)
            Button("") { move(-1) }.keyboardShortcut("k", modifiers: []).opacity(0)
        }
    }

    private func move(_ step: Int) {
        guard !sendingBack, let current = selected, let index = queue.firstIndex(where: { $0.id == current.id }) else { return }
        let next = min(max(index + step, 0), queue.count - 1)
        selectedID = queue[next].id
    }

    private func row(_ work: FleetWork) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            HStack {
                Text(work.title)
                    .font(MuxaType.body.weight(.medium))
                    .lineLimit(1)
                Spacer(minLength: 4)
                if let changed = work.lastChange ?? work.since {
                    Text(changed, format: .relative(presentation: .numeric, unitsStyle: .abbreviated))
                        .font(MuxaType.meta)
                        .foregroundStyle(.tertiary)
                        .monospacedDigit()
                }
            }
            Text(FleetQuickLookText.digest(FleetBoard.reviewResult(work), limit: 120) ?? work.headline)
                .font(MuxaType.meta)
                .foregroundStyle(.secondary)
                .lineLimit(2)
            if let repo = work.repo {
                Text(repo)
                    .font(MuxaType.meta)
                    .foregroundStyle(.tertiary)
            }
        }
        .padding(.vertical, 3)
    }
}

// MARK: - Digest: everything at a glance, by project

private struct FleetReviewDigest: View {
    let queue: [FleetWork]
    let actions: FleetReviewActions
    @State private var expandedID: String?

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 18) {
                ForEach(FleetBoard.byProject(queue)) { group in
                    VStack(alignment: .leading, spacing: 6) {
                        HStack {
                            Label(group.project ?? String(localized: "No project"), systemImage: "folder")
                                .font(.system(size: 13, weight: .semibold))
                            Text("\(group.works.count)")
                                .foregroundStyle(.secondary)
                                .monospacedDigit()
                            Spacer()
                            Button("Mark Project Reviewed") { group.works.forEach(actions.looksGood) }
                                .buttonStyle(.muxaGhost)
                                .controlSize(.small)
                        }
                        ForEach(group.works) { work in
                            row(work)
                        }
                    }
                }
            }
            .padding(20)
        }
    }

    private func row(_ work: FleetWork) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Button {
                    actions.looksGood(work)
                } label: {
                    Image(systemName: "checkmark.circle")
                }
                .buttonStyle(.muxaGhost)
                .help("Looks Good")
                VStack(alignment: .leading, spacing: 2) {
                    Text(work.title)
                        .font(MuxaType.body.weight(.medium))
                        .lineLimit(1)
                    Text(FleetQuickLookText.digest(FleetBoard.reviewResult(work), limit: 160) ?? work.headline)
                        .font(MuxaType.meta)
                        .foregroundStyle(.secondary)
                        .lineLimit(expandedID == work.id ? nil : 1)
                }
                Spacer(minLength: 6)
                if let changed = work.lastChange ?? work.since {
                    Text(changed, format: .relative(presentation: .numeric, unitsStyle: .abbreviated))
                        .font(MuxaType.meta)
                        .foregroundStyle(.tertiary)
                        .monospacedDigit()
                }
                Button {
                    expandedID = expandedID == work.id ? nil : work.id
                } label: {
                    Image(systemName: expandedID == work.id ? "chevron.up" : "chevron.down")
                }
                .buttonStyle(.muxaGhost)
                .help("Show the whole result")
            }
            if expandedID == work.id {
                FleetReviewDetail(work: work, resultLines: 30)
                    .padding(.leading, 34)
                if let focus = work.focus {
                    HStack {
                        Spacer()
                        Button("Open") { actions.open(focus) }
                            .buttonStyle(.muxaSecondary)
                            .controlSize(.small)
                    }
                }
            }
        }
        .padding(10)
        .background(MuxaTheme.panelFill, in: RoundedRectangle(cornerRadius: 8, style: .continuous))
    }
}
