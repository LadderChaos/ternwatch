import AppKit
import Combine
import SwiftUI

let ternAccent = Color(red: 0.36, green: 0.86, blue: 0.92)

/// Identity hues ordered so neighbouring slots contrast. Pure red is left out: it marks failed messages.
private let agentPalette: [Color] = [190, 28, 268, 140, 335, 52, 218, 300, 165, 12, 245, 88].map {
    Color(hue: Double($0) / 360, saturation: 0.62, brightness: 0.98)
}

/// Hands out identity colors so agents on screen do not share one. Working agents always get
/// distinct colors (up to the palette size); idle agents take the least-used remaining slot. A pane
/// keeps its color while it stays in the stream, unless it starts working with a color another
/// working agent already has. Only touched from the main thread (view bodies building a roster).
enum AgentColors {
    private static var slots: [UInt64: Int] = [:]

    static func sync(active: [TernAgent], idle: [TernAgent]) {
        let live = Set((active + idle).map(\.pane))
        slots = slots.filter { live.contains($0.key) }
        var activeSlots = Set<Int>()
        for agent in active {
            if let slot = slots[agent.pane], !activeSlots.contains(slot) {
                activeSlots.insert(slot)
                continue
            }
            let slot = leastUsed(avoiding: activeSlots)
            slots[agent.pane] = slot
            activeSlots.insert(slot)
        }
        for agent in idle where slots[agent.pane] == nil {
            slots[agent.pane] = leastUsed(avoiding: activeSlots)
        }
    }

    static func color(_ pane: UInt64) -> Color {
        agentPalette[slot(pane)]
    }

    /// A subagent never shares its parent's color: it takes one of the other slots, chosen by a
    /// stable hash of its name so it keeps that color across snapshots and launches.
    static func color(_ pane: UInt64, subagent: String) -> Color {
        let count = agentPalette.count
        let step = Int(stableHash(subagent) % UInt64(count - 1))
        return agentPalette[(slot(pane) + 1 + step) % count]
    }

    private static func slot(_ pane: UInt64) -> Int {
        slots[pane] ?? Int(pane % UInt64(agentPalette.count))
    }

    /// 64-bit FNV-1a over the UTF-8 bytes. Unlike `hashValue`, it is the same in every process.
    private static func stableHash(_ text: String) -> UInt64 {
        var hash: UInt64 = 0xcbf2_9ce4_8422_2325
        for byte in text.utf8 { hash = (hash ^ UInt64(byte)) &* 0x0000_0100_0000_01b3 }
        return hash
    }

    /// Prefers slots outside `avoiding`, then the fewest current holders, then palette order.
    private static func leastUsed(avoiding: Set<Int>) -> Int {
        var counts = [Int](repeating: 0, count: agentPalette.count)
        for slot in slots.values { counts[slot] += 1 }
        return counts.indices.min { a, b in
            let keyA = (avoiding.contains(a) ? 1 : 0, counts[a], a)
            let keyB = (avoiding.contains(b) ? 1 : 0, counts[b], b)
            return keyA < keyB
        }!
    }
}

/// Identity color shared by an agent's avatar, bubbles, and chips. A subagent (non-empty
/// name) gets a color distinct from its parent's.
func agentHue(_ pane: UInt64, subagent: String? = nil) -> Color {
    guard let subagent = subagent, !subagent.isEmpty else { return AgentColors.color(pane) }
    return AgentColors.color(pane, subagent: subagent)
}

// MARK: - Preferences

enum TernPreferences {
    /// `@AppStorage` key: subagents take part in conversations on their own. Off by default.
    static let showSubagents = "ternShowSubagents"
}

extension UserDefaults {
    /// KVO-observable mirror of `TernPreferences.showSubagents`; the property name must equal the key.
    @objc dynamic var ternShowSubagents: Bool { bool(forKey: TernPreferences.showSubagents) }
}

// MARK: - Roster

/// Working agents (or orchestrators waiting on running subagents) first, then idle, then closed.
/// Order inside each group is stable so colors and counts do not shuffle on every snapshot.
struct TernRoster {
    let active: [TernAgent]
    let idle: [TernAgent]
    private let byPane: [UInt64: TernAgent]

    init(_ agents: [TernAgent]) {
        let stable: (TernAgent, TernAgent) -> Bool = { $0.tab == $1.tab ? $0.pane < $1.pane : $0.tab < $1.tab }
        let isActive: (TernAgent) -> Bool = { $0.open && ($0.working || $0.subagents.contains(where: \.running)) }
        active = agents.filter(isActive).sorted(by: stable)
        idle = agents.filter { $0.open && !isActive($0) }.sorted(by: stable) + agents.filter { !$0.open }.sorted(by: stable)
        AgentColors.sync(active: active, idle: idle)
        byPane = Dictionary(agents.map { ($0.pane, $0) }, uniquingKeysWith: { first, _ in first })
    }

    func agent(_ pane: UInt64) -> TernAgent? { byPane[pane] }

    /// Short name: the subagent's own name, the agent's readable name, or "External" for panes outside the stream.
    func name(_ endpoint: TernEndpoint) -> String {
        endpoint.subagent ?? agent(endpoint.pane).map(agentName) ?? "External"
    }

    /// Name qualified by the parent pane for subagents ("parent/subagent"), for help text and VoiceOver.
    func fullName(_ endpoint: TernEndpoint) -> String {
        let parent = agent(endpoint.pane).map(agentName) ?? "External"
        return endpoint.subagent.map { "\(parent)/\($0)" } ?? parent
    }

    func hue(_ endpoint: TernEndpoint) -> Color {
        agent(endpoint.pane) == nil ? Color.white.opacity(0.55) : agentHue(endpoint.pane, subagent: endpoint.subagent)
    }

    /// Reported state only: a subagent works while its parent lists it as running.
    func mood(_ endpoint: TernEndpoint) -> AgentMood {
        guard let agent = agent(endpoint.pane) else { return .closed }
        guard let name = endpoint.subagent else { return agentMood(agent) }
        if !agent.open { return .closed }
        return agent.subagents.contains { $0.name == name && $0.running } ? .working : .idle
    }
}

// MARK: - Conversations

/// One conversation participant: a top-level pane, or one of its subagents while subagents are shown.
struct TernEndpoint: Hashable {
    let pane: UInt64
    /// Nil for the pane itself; never empty.
    let subagent: String?

    init(pane: UInt64, subagent: String? = nil) {
        self.pane = pane
        self.subagent = subagent.flatMap { $0.isEmpty ? nil : $0 }
    }

    init(_ node: TernNode) {
        self.init(pane: node.pane, subagent: node.subagent)
    }

    fileprivate func precedes(_ other: TernEndpoint) -> Bool {
        pane != other.pane ? pane < other.pane : (subagent ?? "") < (other.subagent ?? "")
    }
}

/// The messages exchanged between one pair of endpoints, oldest to newest.
struct TernConversation: Identifiable {
    /// Unordered endpoint pair: messages A → B and B → A belong to one conversation.
    struct ID: Hashable {
        let first: TernEndpoint
        let second: TernEndpoint

        init(_ a: TernEndpoint, _ b: TernEndpoint) {
            (first, second) = a.precedes(b) ? (a, b) : (b, a)
        }
    }

    let id: ID
    /// Sender of the earliest loaded message; the sides never swap as replies arrive.
    let left: TernEndpoint
    let right: TernEndpoint
    /// Oldest to newest; never empty.
    fileprivate(set) var entries: [TrafficEntry]
    fileprivate(set) var groups: [TrafficGroup] = []

    var countLabel: String {
        "\(groups.count) message\(groups.count == 1 ? "" : "s")"
    }
    /// Position of the latest message in the traffic list; orders conversations newest first.
    fileprivate var latestIndex: Int

    var latest: TrafficEntry { entries[entries.count - 1] }

    /// When the latest message was sent (or delivered, when no send time was reported).
    var latestDate: Date? {
        (latest.item.sentMs ?? latest.item.deliveredMs).map { Date(timeIntervalSince1970: Double($0) / 1000) }
    }

    /// The conversation a message from `from` to `to` belongs to, or nil when the filter drops it.
    /// With subagents hidden, conversations are between distinct top-level panes only; with them
    /// shown, any two distinct endpoints, so a pane can talk to its own subagent.
    static func id(_ from: TernEndpoint, _ to: TernEndpoint, subagents: Bool) -> ID? {
        if !subagents && (from.subagent != nil || to.subagent != nil || from.pane == to.pane) { return nil }
        return from == to ? nil : ID(from, to)
    }

    /// Groups traffic (oldest to newest) into conversations, newest first, and counts the messages
    /// dropped only because an endpoint is a subagent while subagents are hidden.
    static func index(_ entries: [TrafficEntry], subagents: Bool) -> (conversations: [TernConversation], hiddenSubagentMessages: Int) {
        var conversations: [TernConversation] = []
        var positions: [ID: Int] = [:]
        var hidden = 0
        for (index, entry) in entries.enumerated() {
            let from = TernEndpoint(entry.item.from)
            let to = TernEndpoint(entry.item.to)
            if !subagents && (from.subagent != nil || to.subagent != nil) { hidden += 1 }
            guard let id = id(from, to, subagents: subagents) else { continue }
            if let position = positions[id] {
                conversations[position].entries.append(entry)
                conversations[position].latestIndex = index
            } else {
                positions[id] = conversations.count
                conversations.append(TernConversation(id: id, left: from, right: to, entries: [entry], latestIndex: index))
            }
        }
        for index in conversations.indices {
            conversations[index].groups = TrafficGroup.group(conversations[index].entries)
        }
        return (conversations.sorted { $0.latestIndex > $1.latestIndex }, hidden)
    }
}

/// A presentation group only: every original receipt stays in the traffic and agent lists.
struct TrafficGroup: Identifiable {
    var id: TrafficEntry.ID { root.id }
    let root: TrafficEntry
    var receipts: [TrafficEntry]

    static func group(_ entries: [TrafficEntry]) -> [TrafficGroup] {
        var roots: [UInt64: TrafficEntry] = [:]
        for entry in entries where entry.item.repeatOf == nil {
            if let id = entry.item.id { roots[id] = entry }
        }
        var groups: [TrafficGroup] = []
        var positions: [TrafficEntry.ID: Int] = [:]
        for entry in entries {
            var root = entry
            if let repeatOf = entry.item.repeatOf, let candidate = roots[repeatOf],
               TernEndpoint(candidate.item.from) == TernEndpoint(entry.item.from),
               TernEndpoint(candidate.item.to) == TernEndpoint(entry.item.to) {
                root = candidate
            }
            if let index = positions[root.id] {
                groups[index].receipts.append(entry)
            } else {
                positions[root.id] = groups.count
                groups.append(TrafficGroup(root: root, receipts: [entry]))
            }
        }
        return groups
    }
}

// MARK: - Traffic

/// Owns message identity, fresh-arrival detection, and the conversation index for both notch
/// states, so opening the notch never replays loaded history and delivery updates keep their
/// identity instead of reading as new traffic.
final class TernTraffic: ObservableObject {
    static let pulseDuration: TimeInterval = 1.4

    /// Oldest to newest.
    @Published private(set) var entries: [TrafficEntry] = []
    /// Newly observed messages, alive for `pulseDuration`.
    @Published private(set) var pulses: [TrafficPulse] = []
    /// Increments only when fresh traffic arrives; drives insertion animation.
    @Published private(set) var arrivals = 0
    /// Conversations under the Subagents filter, newest first. Rebuilt only when `entries` or the filter change.
    @Published private(set) var conversations: [TernConversation] = []
    /// Messages hidden only because an endpoint is a subagent; 0 while subagents are shown.
    @Published private(set) var hiddenSubagentMessages = 0
    /// Current value of the Subagents filter (`TernPreferences.showSubagents`).
    private(set) var showSubagents = false

    private var observed: [TrafficEntry.ID: Int64] = [:]
    private var hasBaseline = false
    private var observingSince = Date()
    private var expiry: DispatchWorkItem?
    private var subscriptions: Set<AnyCancellable> = []

    init(store: TernStreamStore) {
        store.$messages
            .sink { [weak self, weak store] messages in
                self?.rebuild(messages, hasAgents: !(store?.agents.isEmpty ?? true))
            }
            .store(in: &subscriptions)
        store.$isRunning
            .removeDuplicates()
            .sink { [weak self] running in
                if !running { self?.resetBaseline() }
            }
            .store(in: &subscriptions)
        $entries
            .combineLatest(UserDefaults.standard.publisher(for: \.ternShowSubagents).removeDuplicates())
            .sink { [weak self] entries, subagents in
                self?.reindex(entries, subagents: subagents)
            }
            .store(in: &subscriptions)
    }

    func isFresh(_ id: TrafficEntry.ID) -> Bool { pulses.contains { $0.entry == id } }

    func isFresh(_ id: TernConversation.ID) -> Bool { pulses.contains { conversationID(of: $0) == id } }

    /// The pinned conversation while it exists, otherwise the one with the latest message.
    func conversation(pinned: TernConversation.ID?) -> TernConversation? {
        pinned.flatMap { id in conversations.first { $0.id == id } } ?? conversations.first
    }

    func conversationID(of pulse: TrafficPulse) -> TernConversation.ID? {
        TernConversation.id(TernEndpoint(pane: pulse.from, subagent: pulse.fromSubagent),
                            TernEndpoint(pane: pulse.to, subagent: pulse.toSubagent), subagents: showSubagents)
    }

    /// A fresh message from this endpoint is in flight. Pulses expire, so this is finite.
    func speaking(_ endpoint: TernEndpoint) -> Bool {
        pulses.contains { matches(pane: $0.from, subagent: $0.fromSubagent, endpoint) }
    }

    /// A fresh message to this endpoint just arrived.
    func listening(_ endpoint: TernEndpoint) -> Bool {
        pulses.contains { matches(pane: $0.to, subagent: $0.toSubagent, endpoint) }
    }

    /// With subagents hidden a pane speaks for its subagents too; shown, each endpoint is its own.
    private func matches(pane: UInt64, subagent: String?, _ endpoint: TernEndpoint) -> Bool {
        pane == endpoint.pane && (!showSubagents || subagent == endpoint.subagent)
    }

    private func reindex(_ entries: [TrafficEntry], subagents: Bool) {
        let index = TernConversation.index(entries, subagents: subagents)
        showSubagents = subagents
        conversations = index.conversations
        if hiddenSubagentMessages != index.hiddenSubagentMessages { hiddenSubagentMessages = index.hiddenSubagentMessages }
    }

    private func resetBaseline() {
        hasBaseline = false
        observingSince = Date()
        observed.removeAll(keepingCapacity: true)
        expiry?.cancel()
        expiry = nil
        if !pulses.isEmpty { pulses.removeAll() }
    }
    private func rebuild(_ messages: [TernMessage], hasAgents: Bool) {
        let now = Date()
        var occurrences: [TrafficKey: Int] = [:]
        let next = messages.enumerated().map { offset, item in
            let id: TrafficEntry.ID
            if let sequence = item.id {
                id = .receipt(sequence)
            } else {
                let key = TrafficKey(item)
                let occurrence = occurrences[key, default: 0]
                occurrences[key] = occurrence + 1
                id = .legacy(key: key, occurrence: occurrence)
            }
            return TrafficEntry(id: id, item: item, offset: offset)
        }.sorted {
            let left = $0.item.sentMs ?? $0.item.deliveredMs ?? Int64.min
            let right = $1.item.sentMs ?? $1.item.deliveredMs ?? Int64.min
            return left == right ? $0.offset < $1.offset : left < right
        }
        var fresh: [TrafficEntry] = []
        if hasBaseline {
            let nowMs = now.timeIntervalSince1970 * 1000
            let earliest = max(observingSince.timeIntervalSince1970 * 1000, nowMs - 5000)
            fresh = next.filter { entry in
                guard observed[entry.id] == nil, entry.item.repeatOf == nil,
                      let sent = entry.item.sentMs else { return false }
                return Double(sent) >= earliest && Double(sent) <= nowMs + 1000
            }
        } else if !messages.isEmpty || hasAgents {
            // Suppress loaded history, including the first snapshot after reconnect.
            hasBaseline = true
            observingSince = now
        }
        // Retain recently seen identities across transient snapshot omissions.
        let recentCutoff = Int64((now.timeIntervalSince1970 - 6) * 1000)
        observed = observed.filter { $0.value >= recentCutoff }
        for entry in next { observed[entry.id] = entry.item.sentMs ?? entry.item.deliveredMs ?? Int64.min }
        if Self.changed(entries, next) { entries = next }
        guard !fresh.isEmpty else { return }
        let live = pulses.filter { now.timeIntervalSince($0.started) < Self.pulseDuration }
        let added = fresh.suffix(12).map {
            TrafficPulse(entry: $0.id, from: $0.item.from.pane, fromSubagent: Self.subagent($0.item.from),
                         to: $0.item.to.pane, toSubagent: Self.subagent($0.item.to), failed: $0.item.failed, started: now)
        }
        pulses = Array((live + added).suffix(16))
        arrivals += fresh.count
        scheduleExpiry()
    }

    private static func subagent(_ node: TernNode) -> String? {
        node.subagent.flatMap { $0.isEmpty ? nil : $0 }
    }

    private static func changed(_ old: [TrafficEntry], _ new: [TrafficEntry]) -> Bool {
        guard old.count == new.count else { return true }
        return zip(old, new).contains { left, right in
            left.id != right.id || left.item.deliveredMs != right.item.deliveredMs || left.item.failed != right.item.failed
                || left.item.outcome != right.item.outcome || left.item.replied != right.item.replied
                || left.item.repeatOf != right.item.repeatOf || left.item.sentMs != right.item.sentMs
                || left.item.text != right.item.text || left.item.kind != right.item.kind
                || left.item.reply != right.item.reply || left.item.deliveryObservable != right.item.deliveryObservable
                || TernEndpoint(left.item.from) != TernEndpoint(right.item.from)
                || TernEndpoint(left.item.to) != TernEndpoint(right.item.to)
        }
    }

    private func scheduleExpiry() {
        expiry?.cancel()
        guard let oldest = pulses.map(\.started).min() else { expiry = nil; return }
        let delay = max(0.05, Self.pulseDuration - Date().timeIntervalSince(oldest))
        let task = DispatchWorkItem { [weak self] in
            guard let self = self else { return }
            let now = Date()
            self.pulses.removeAll { now.timeIntervalSince($0.started) >= Self.pulseDuration }
            self.scheduleExpiry()
        }
        expiry = task
        DispatchQueue.main.asyncAfter(deadline: .now() + delay, execute: task)
    }
}

/// Pinned conversation, selection, and draft survive collapsing the notch.
final class TernViewState: ObservableObject {
    /// Conversation shown in both notch states; nil follows the conversation with the latest message.
    @Published var pinned: TernConversation.ID?
    @Published var selectedPane: UInt64?
    @Published var selectedMessage: TrafficEntry.ID?
    @Published var expandedReceipts: Set<TrafficEntry.ID> = []
    @Published var draft = ""
    /// The Message recipient list is open over the detail area.
    @Published var pickingRecipient = false

    /// Steps back one level: recipient list, message, then agent, then pinned conversation.
    /// Returns false when already following the live conversation.
    @discardableResult
    func back() -> Bool {
        if pickingRecipient { pickingRecipient = false; return true }
        if selectedMessage != nil { selectedMessage = nil; return true }
        if selectedPane != nil { selectedPane = nil; return true }
        if pinned != nil { pinned = nil; return true }
        return false
    }
}

// MARK: - Expanded notch body

/// The open notch: a strip of recent conversations, then the shown conversation as two agents
/// facing each other above a mirrored transcript, or an agent or message detail.
struct TernView: View {
    @ObservedObject var store: TernStreamStore
    @ObservedObject var traffic: TernTraffic
    @ObservedObject var state: TernViewState
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var copied: String?
    @FocusState private var composing: Bool

    /// Bubbles never run wider than this share of the transcript, so the two sides stay apart.
    private let bubbleShare: CGFloat = 0.62
    /// Newest groups shown; disclosure preserves every original message within each group.
    private let transcriptLimit = 150
    /// Conversation chips in the switcher, newest first (plus the pinned one if older).
    private let chipLimit = 12

    var body: some View {
        let roster = TernRoster(store.agents)
        let conversation = traffic.conversation(pinned: state.pinned)
        VStack(alignment: .leading, spacing: 8) {
            if let error = store.error { banner("Watcher", error, offerRestart: true) }
            if let error = store.actionError { banner("Action failed", error, offerRestart: false) }
            if !traffic.conversations.isEmpty || !store.agents.isEmpty { switcher(roster, shown: conversation) }
            if state.pickingRecipient { recipientList(roster) } else { detail(roster, conversation) }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .onReceive(traffic.$entries) { entries in
            if let id = state.selectedMessage, !entries.contains(where: { $0.id == id }) { state.selectedMessage = nil }
        }
        .onChange(of: store.agents.map(\.pane)) { _, panes in
            if let pane = state.selectedPane, !panes.contains(pane) { state.selectedPane = nil }
        }
        .task(id: copied) {
            guard copied != nil else { return }
            try? await Task.sleep(for: .seconds(1.4))
            if !Task.isCancelled { copied = nil }
        }
    }

    // MARK: Conversation switcher

    private func switcher(_ roster: TernRoster, shown: TernConversation?) -> some View {
        var chips = Array(traffic.conversations.prefix(chipLimit))
        if let shown = shown, !chips.contains(where: { $0.id == shown.id }) { chips.append(shown) }
        return ScrollView(.horizontal) {
            HStack(spacing: 6) {
                messageMenu(roster)
                liveChip
                ForEach(chips) { conversationChip($0, roster, shown: $0.id == shown?.id) }
            }
            .padding(.vertical, 1)
        }
        .scrollIndicators(.never)
        .frame(height: 26)
    }

    private var liveChip: some View {
        let live = state.pinned == nil
        return Button { show(nil) } label: {
            HStack(spacing: 5) {
                Rectangle().fill(store.isRunning ? Color.green : Color.orange).frame(width: 6, height: 6)
                Text("Live").font(.pixel())
            }
            .foregroundStyle(live ? Color.white : Color.white.opacity(0.6))
            .padding(.horizontal, 9)
            .frame(height: 24)
            .background(PixelBox(1).fill(live ? Color.green.opacity(0.18) : Color.white.opacity(0.06)))
            .overlay(PixelBox(1).strokeBorder(Color.green.opacity(live ? 0.55 : 0), lineWidth: 1))
            .contentShape(PixelBox(1))
        }
        .buttonStyle(.plain)
        .help("Follow the latest conversation")
        .accessibilityLabel("Follow live conversation")
        .accessibilityAddTraits(live ? .isSelected : [])
    }

    /// Opens a pixel list of every agent in the stream, with or without a conversation.
    private func messageMenu(_ roster: TernRoster) -> some View {
        let selected = state.pickingRecipient || (state.selectedPane != nil && state.selectedMessage == nil)
        return Button {
            withAnimation(reduceMotion ? nil : .easeInOut(duration: 0.18)) { state.pickingRecipient.toggle() }
        } label: {
            HStack(spacing: 5) {
                PixelIcon(glyph: .compose)
                Text("Message").font(.pixel())
            }
            .foregroundStyle(selected ? Color.white : Color.white.opacity(0.6))
            .padding(.horizontal, 9)
            .frame(height: 24)
            .background(PixelBox(1).fill(selected ? ternAccent.opacity(0.18) : Color.white.opacity(0.06)))
            .overlay(PixelBox(1).strokeBorder(ternAccent.opacity(selected ? 0.55 : 0), lineWidth: 1))
            .contentShape(PixelBox(1))
        }
        .buttonStyle(.plain)
        .disabled(store.agents.isEmpty)
        .help("Message any agent directly")
        .accessibilityLabel("Message an agent")
        .accessibilityAddTraits(state.pickingRecipient ? .isSelected : [])
    }

    /// Recipient rows, working agents first; closed panes are listed but cannot be chosen.
    /// Choosing one opens its detail with the composer focused.
    private func recipientList(_ roster: TernRoster) -> some View {
        let agents = roster.active + roster.idle
        return VStack(alignment: .leading, spacing: 6) {
            Text("MESSAGE WHICH AGENT?").font(.pixel()).foregroundStyle(Color.white.opacity(0.45))
            ScrollView {
                LazyVGrid(columns: [GridItem(.adaptive(minimum: 230), spacing: 6)], alignment: .leading, spacing: 6) {
                    ForEach(agents, id: \.pane) { agent in recipientRow(agent) }
                }
                .padding(1)
            }
            .scrollIndicators(.never)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .transition(.opacity)
    }

    private func recipientRow(_ agent: TernAgent) -> some View {
        let hue = agentHue(agent.pane)
        let status = agent.open ? (agent.working ? "working" : "idle") : "closed"
        return Button {
            withAnimation(reduceMotion ? nil : .easeInOut(duration: 0.18)) { state.pickingRecipient = false }
            openAgent(agent.pane)
            // The composer appears with the detail; focus it once it exists.
            DispatchQueue.main.async { composing = true }
        } label: {
            HStack(spacing: 8) {
                AgentAvatar(hue: hue, mood: agentMood(agent), size: 18)
                Text(agentName(agent)).foregroundStyle(agent.open ? hue : Color.white.opacity(0.35)).lineLimit(1)
                Spacer(minLength: 4)
                Text(status).foregroundStyle(Color.white.opacity(agent.working ? 0.75 : 0.4))
            }
            .font(.pixel())
            .padding(.horizontal, 8)
            .frame(height: 30)
            .background(PixelBox(2).fill(agent.open ? hue.opacity(0.08) : Color.white.opacity(0.03)))
            .overlay(PixelBox(2).strokeBorder(agent.open ? hue.opacity(0.4) : Color.white.opacity(0.1), lineWidth: 1))
            .contentShape(PixelBox(2))
        }
        .buttonStyle(.plain)
        .disabled(!agent.open)
        .help(agent.open ? "Write to \(agentName(agent))" : "Pane is closed")
        .accessibilityLabel("\(agentName(agent)), \(status)")
    }

    /// Mirrored like the header: left participant, message count, right participant.
    private func conversationChip(_ conversation: TernConversation, _ roster: TernRoster, shown: Bool) -> some View {
        let pinned = state.pinned == conversation.id
        let fresh = !reduceMotion && traffic.isFresh(conversation.id)
        let leftHue = roster.hue(conversation.left)
        let rightHue = roster.hue(conversation.right)
        let pair = LinearGradient(gradient: PixelArt.bands(leftHue, rightHue), startPoint: .leading, endPoint: .trailing)
        let count = conversation.groups.count
        let names = "\(roster.fullName(conversation.left)) and \(roster.fullName(conversation.right))"
        return Button { show(conversation.id) } label: {
            HStack(spacing: 5) {
                AgentAvatar(hue: leftHue, mood: roster.mood(conversation.left), size: 13)
                Text(chipName(roster.name(conversation.left))).foregroundStyle(leftHue)
                Text("\(count)")
                    .font(.pixel())
                    .foregroundStyle(Color.white.opacity(0.7))
                    .padding(.horizontal, 5).padding(.vertical, 1)
                    .background(PixelBox(1).fill(Color.white.opacity(0.1)))
                Text(chipName(roster.name(conversation.right))).foregroundStyle(rightHue)
                AgentAvatar(hue: rightHue, mood: roster.mood(conversation.right), size: 13)
                if pinned {
                    PixelIcon(glyph: .pin).foregroundStyle(Color.white.opacity(0.6))
                }
            }
            .font(.pixel())
            .lineLimit(1)
            .fixedSize()
            .padding(.horizontal, 7)
            .frame(height: 24)
            .background(
                ZStack {
                    PixelBox(1).fill(Color.white.opacity(shown ? 0.1 : 0.05))
                    PixelBox(1).fill(pair).opacity(fresh ? 0.3 : 0)
                }
            )
            .overlay(PixelBox(1).strokeBorder(pair, lineWidth: 1).opacity(shown ? 0.7 : fresh ? 0.5 : 0))
            .animation(reduceMotion ? nil : .easeOut(duration: 0.9), value: fresh)
            .contentShape(PixelBox(1))
        }
        .buttonStyle(.plain)
        .help("\(names) · \(conversation.countLabel)\(pinned ? " · pinned" : "")")
        .accessibilityLabel("Conversation between \(names), \(conversation.countLabel)")
        .accessibilityHint(pinned ? "Pinned" : "Pins this conversation")
        .accessibilityAddTraits(pinned ? .isSelected : [])
    }

    // MARK: Detail

    private func detail(_ roster: TernRoster, _ conversation: TernConversation?) -> some View {
        let entry = state.selectedMessage.flatMap { id in traffic.entries.first { $0.id == id } }
        let agent = state.selectedPane.flatMap { roster.agent($0) }
        let mode: DetailMode = entry.map { DetailMode.message($0.id) } ?? agent.map { DetailMode.agent($0.pane) }
            ?? conversation.map { DetailMode.conversation($0.id) } ?? .empty
        return ZStack(alignment: .topLeading) {
            Group {
                if let entry = entry {
                    messageDetail(entry, roster)
                } else if let agent = agent {
                    agentDetail(agent, roster)
                } else if let conversation = conversation {
                    conversationPanel(conversation, roster)
                } else {
                    emptyState(roster)
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            .id(mode)
            .transition(.opacity)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .animation(reduceMotion ? nil : .easeInOut(duration: 0.18), value: mode)
    }

    private func emptyState(_ roster: TernRoster) -> some View {
        let working = roster.active.count
        let hidden = traffic.hiddenSubagentMessages
        return VStack(spacing: 8) {
            PixelIcon(glyph: store.isRunning ? .chat : .bolt, scale: 2)
                .foregroundStyle(ternAccent)
            Text(store.isRunning ? "No conversations yet" : "Watcher offline")
                .font(.pixel())
            Group {
                if !store.isRunning {
                    Text("Tern Notch retries every 5 seconds.")
                } else if store.agents.isEmpty {
                    Text("Waiting for agents.")
                } else {
                    Text("\(working) agent\(working == 1 ? "" : "s") working. Messages between two agents appear here as their conversation.")
                }
            }
            .font(.pixel()).foregroundStyle(Color.white.opacity(0.55))
            if store.isRunning && hidden > 0 {
                Text("\(hidden) subagent message\(hidden == 1 ? " is" : "s are") hidden. Turn on Subagents in the header to see them.")
                    .font(.pixel()).foregroundStyle(ternAccent.opacity(0.85))
            }
            if !store.isRunning {
                Button("Restart watcher") { store.restart() }.buttonStyle(PixelButtonStyle())
            }
        }
        .multilineTextAlignment(.center)
        .frame(maxWidth: 380)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    // MARK: Conversation

    private func conversationPanel(_ conversation: TernConversation, _ roster: TernRoster) -> some View {
        VStack(spacing: 8) {
            pairHeader(conversation, roster)
            Rectangle().fill(Color.white.opacity(0.07)).frame(height: 1)
            transcript(conversation, roster)
        }
    }

    /// The two participants mirrored around the conversation's message count.
    private func pairHeader(_ conversation: TernConversation, _ roster: TernRoster) -> some View {
        return HStack(spacing: 10) {
            participant(conversation.left, roster, leading: true)
                .frame(maxWidth: .infinity, alignment: .leading)
            Text(conversation.countLabel)
                .font(.pixel())
                .foregroundStyle(Color.white.opacity(0.38))
                .lineLimit(1)
                .frame(width: 120)
            participant(conversation.right, roster, leading: false)
                .frame(maxWidth: .infinity, alignment: .trailing)
        }
        .frame(height: 46)
    }

    /// One side of the pair header; the right side mirrors the left. Opens the agent's detail.
    private func participant(_ endpoint: TernEndpoint, _ roster: TernRoster, leading: Bool) -> some View {
        let hue = roster.hue(endpoint)
        let known = roster.agent(endpoint.pane) != nil
        let status = participantStatus(endpoint, roster)
        let avatar = AgentAvatar(hue: hue, mood: roster.mood(endpoint), speaking: speaking(endpoint),
                                 listening: listening(endpoint), size: 34)
        let text = VStack(alignment: leading ? .leading : .trailing, spacing: 2) {
            Text(roster.name(endpoint))
                .font(.pixel())
                .foregroundStyle(hue)
                .lineLimit(1)
            Text(status)
                .font(.pixel())
                .foregroundStyle(Color.white.opacity(0.68))
                .lineLimit(2)
                .multilineTextAlignment(leading ? .leading : .trailing)
        }
        return Button { openAgent(endpoint.pane) } label: {
            HStack(spacing: 9) {
                if leading {
                    avatar
                    text
                } else {
                    text
                    avatar
                }
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(!known)
        .help(known ? "\(roster.fullName(endpoint)) · pane \(endpoint.pane)\n\(status)" : "Pane \(endpoint.pane) is not in the stream")
        .accessibilityLabel("\(roster.fullName(endpoint)), \(status)")
        .accessibilityHint("Shows agent details")
    }

    private func participantStatus(_ endpoint: TernEndpoint, _ roster: TernRoster) -> String {
        guard let agent = roster.agent(endpoint.pane) else { return "External pane \(endpoint.pane)" }
        guard let name = endpoint.subagent else { return agent.open ? intentLine(agent) : "Pane closed" }
        let subagent = agent.subagents.first { $0.name == name }
        let state = subagent.map { $0.running ? "running" : $0.done ? "done" : "idle" } ?? "not listed"
        return "Subagent of \(agentName(agent)) · \(state)"
    }

    /// Oldest to newest, scrolled to the newest message on open and on each arrival.
    private func transcript(_ conversation: TernConversation, _ roster: TernRoster) -> some View {
        let groups = conversation.groups.suffix(transcriptLimit)
        let newest = conversation.groups[conversation.groups.count - 1].id
        return ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(spacing: 6) {
                    ForEach(groups) { group in
                        receiptGroup(group, conversation, roster)
                            .id(group.id)
                            .transition(.asymmetric(insertion: .move(edge: .bottom).combined(with: .opacity), removal: .opacity))
                    }
                }
                .padding(.vertical, 2)
                // Only fresh arrivals animate; loaded history and delivery updates do not change `arrivals`.
                .animation(reduceMotion ? nil : .spring(response: 0.45, dampingFraction: 0.78), value: traffic.arrivals)
            }
            .scrollIndicators(.never)
            .defaultScrollAnchor(.bottom)
            .onAppear { proxy.scrollTo(newest, anchor: .bottom) }
            .onChange(of: newest) { _, id in
                withAnimation(reduceMotion ? nil : .easeOut(duration: 0.3)) { proxy.scrollTo(id, anchor: .bottom) }
            }
        }
        .frame(maxHeight: .infinity)
    }

    private func receiptGroup(_ group: TrafficGroup, _ conversation: TernConversation, _ roster: TernRoster) -> some View {
        let expanded = state.expandedReceipts.contains(group.id)
        let leading = TernEndpoint(group.root.item.from) == conversation.left
        return LazyVStack(spacing: 5) {
            if group.receipts.count > 1 {
                repeatDisclosure(group)
                    .frame(maxWidth: .infinity, alignment: leading ? .leading : .trailing)
            }
            if expanded && group.receipts.count > 1 {
                ForEach(group.receipts) { entry in bubble(entry, conversation, roster) }
            } else {
                bubble(group.root, conversation, roster)
            }
        }
    }

    private func repeatDisclosure(_ group: TrafficGroup) -> some View {
        let expanded = state.expandedReceipts.contains(group.id)
        return Button {
            if expanded { state.expandedReceipts.remove(group.id) }
            else { state.expandedReceipts.insert(group.id) }
        } label: {
            HStack(spacing: 5) {
                PixelIcon(glyph: .chevronUp)
                    .rotationEffect(.degrees(expanded ? 0 : 180))
                Text("Possible repeat · \(group.receipts.count) copies")
                    .font(.pixel())
            }
            .foregroundStyle(Color.white.opacity(0.65))
            .padding(.vertical, 3)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(expanded ? "Collapse original messages" : "Show every original message; click a message for full text")
        .accessibilityValue(expanded ? "Expanded" : "Collapsed")
    }

    /// One message as a mirrored chat bubble: the left participant's messages hug the left edge and
    /// the right participant's the right edge, each in the sender's color, at most `bubbleShare` wide.
    private func bubble(_ entry: TrafficEntry, _ conversation: TernConversation, _ roster: TernRoster) -> some View {
        let item = entry.item
        let sender = TernEndpoint(item.from)
        let leading = sender == conversation.left
        let alignment: Alignment = leading ? .leading : .trailing
        let hue = roster.hue(sender)
        let edge = item.failed ? Color.red : hue
        let fresh = !reduceMotion && traffic.isFresh(entry.id)
        let selected = state.selectedMessage == entry.id
        let shape = PixelBox(topLeading: leading ? 0 : 2, topTrailing: leading ? 2 : 0, bottomLeading: 2, bottomTrailing: 2)
        return Button { state.selectedMessage = entry.id } label: {
            VStack(alignment: leading ? .leading : .trailing, spacing: 3) {
                HStack(spacing: 5) {
                    if item.reply { Text("reply") }
                    Text(entry.time)
                    deliveryGlyph(item)
                }
                .font(.pixel())
                .foregroundStyle(hue.opacity(0.8))
                Text(entry.preview)
                    .font(.pixel())
                    .foregroundStyle(Color.white.opacity(0.88))
                    .lineLimit(4)
                    .multilineTextAlignment(.leading)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(.horizontal, 10).padding(.vertical, 6)
            .background(shape.fill(hue.opacity(fresh ? 0.3 : selected ? 0.24 : 0.13)))
            .overlay(shape.strokeBorder(edge.opacity(fresh ? 0.8 : selected || item.failed ? 0.6 : 0.22), lineWidth: 1))
            .animation(reduceMotion ? nil : .easeOut(duration: 0.9), value: fresh)
            .contentShape(shape)
        }
        .buttonStyle(.plain)
        .containerRelativeFrame(.horizontal, alignment: alignment) { length, _ in length * bubbleShare }
        .frame(maxWidth: .infinity, alignment: alignment)
        .help("\(route(item, roster)) · \(delivery(item))\n\(entry.preview)")
        .accessibilityLabel("\(route(item, roster, spoken: true)), \(entry.time), \(delivery(item)), \(entry.preview)")
        .accessibilityAddTraits(selected ? .isSelected : [])
    }

    // MARK: Agent detail

    private func agentDetail(_ agent: TernAgent, _ roster: TernRoster) -> some View {
        let hue = agentHue(agent.pane)
        let endpoint = TernEndpoint(pane: agent.pane)
        let related = TrafficGroup.group(traffic.entries.filter { $0.item.from.pane == agent.pane || $0.item.to.pane == agent.pane })
        return VStack(alignment: .leading, spacing: 9) {
            HStack(spacing: 8) {
                backButton
                AgentAvatar(hue: hue, mood: agentMood(agent), speaking: speaking(endpoint), listening: listening(endpoint), size: 30)
                VStack(alignment: .leading, spacing: 1) {
                    Text(agentName(agent)).font(.pixel()).lineLimit(1)
                    Text(agentHeadline(agent))
                        .font(.pixel())
                        .foregroundStyle(agent.open && agent.working ? Color.green.opacity(0.85) : Color.white.opacity(0.5))
                        .lineLimit(1)
                }
                Spacer(minLength: 6)
                Button { store.focus(agent.pane) } label: { Text("Focus") }
                    .disabled(!agent.open)
                    .help("Focus pane \(agent.pane) in Tern")
                Button { copy(String(agent.pane), token: "pane") } label: {
                    PixelIcon(glyph: copied == "pane" ? .check : .copy)
                }
                .help("Copy pane ID \(agent.pane)")
                .accessibilityLabel("Copy pane ID")
            }
            .buttonStyle(PixelButtonStyle())
            .font(.pixel())
            if !agent.description.isEmpty {
                Text(agent.description)
                    .font(.pixel())
                    .foregroundStyle(Color.white.opacity(0.82))
                    .lineLimit(2)
                    .textSelection(.enabled)
            }
            if agentMood(agent) == .working || agentMood(agent) == .waiting || agent.todo != nil { nowCard(agent, hue: hue) }
            if !agent.subagents.isEmpty { subagentChips(agent, roster) }
            Text(locationLine(agent))
                .font(.pixel())
                .foregroundStyle(Color.white.opacity(0.42))
                .lineLimit(1)
                .truncationMode(.head)
                .textSelection(.enabled)
                .help(agent.cwd)
            Rectangle().fill(Color.white.opacity(0.07)).frame(height: 1)
            HStack(spacing: 6) {
                sectionLabel("Exchanges")
                countBadge(related.count, color: hue)
                Spacer(minLength: 0)
            }
            exchangeList(related, roster, empty: "No exchanges with \(agentName(agent)) yet.")
            composer(agent, hue: hue)
        }
    }

    /// What the agent is doing right now, in its own words (the tool intent it reported).
    private func nowCard(_ agent: TernAgent, hue: Color) -> some View {
        let mood = agentMood(agent)
        return HStack(alignment: .top, spacing: 9) {
            Rectangle().fill(hue).frame(width: 2)
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 6) {
                    Text(mood == .working ? "WORKING ON" : mood == .waiting ? "ORCHESTRATING" : "TASKS")
                        .font(.pixel())
                        .tracking(1)
                        .foregroundStyle(hue)
                    if let tag = toolTagName(agent) { toolTag(tag, hue: hue) }
                }
                if mood == .working || mood == .waiting {
                    Text(intentLine(agent))
                        .font(.pixel())
                        .foregroundStyle(Color.white.opacity(0.86))
                        .lineLimit(3)
                        .textSelection(.enabled)
                }
                if let todo = agent.todo, todo.total > 0 {
                    HStack(spacing: 8) {
                        ProgressTrack(fraction: Double(todo.done) / Double(todo.total), color: hue).frame(height: 4)
                        Text("\(todo.done)/\(todo.total) tasks")
                            .font(.pixel())
                            .foregroundStyle(Color.white.opacity(0.55))
                            .fixedSize()
                    }
                }
            }
            Spacer(minLength: 0)
        }
        .padding(8)
        .background(PixelBox(2).fill(Color.white.opacity(0.05)))
        .fixedSize(horizontal: false, vertical: true)
    }

    private func subagentChips(_ agent: TernAgent, _ roster: TernRoster) -> some View {
        ScrollView(.horizontal) {
            HStack(spacing: 5) {
                ForEach(Array(agent.subagents.enumerated()), id: \.offset) { _, subagent in
                    HStack(spacing: 4) {
                        AgentAvatar(hue: roster.hue(TernEndpoint(pane: agent.pane, subagent: subagent.name)),
                                    mood: subagent.running ? .working : subagent.done ? .idle : .closed, size: 13)
                        Text(subagent.name).lineLimit(1)
                        Text(subagent.running ? "running" : subagent.done ? "done" : "idle")
                            .foregroundStyle(Color.white.opacity(0.4))
                    }
                    .font(.pixel())
                    .padding(.horizontal, 7).padding(.vertical, 3)
                    .background(PixelBox(1).fill(Color.white.opacity(0.06)))
                }
            }
        }
        .scrollIndicators(.never)
    }

    private func messageDetail(_ entry: TrafficEntry, _ roster: TernRoster) -> some View {
        let item = entry.item
        return VStack(alignment: .leading, spacing: 9) {
            HStack(spacing: 6) {
                backButton
                nodeChip(item.from, roster)
                Text(item.reply ? "replied to" : "to")
                    .font(.pixel())
                    .foregroundStyle(Color.white.opacity(0.45))
                nodeChip(item.to, roster)
                Spacer(minLength: 6)
                Button { copy(item.text, token: "message") } label: {
                    Label {
                        Text(copied == "message" ? "Copied" : "Copy")
                    } icon: {
                        PixelIcon(glyph: copied == "message" ? .check : .copy)
                    }
                    .font(.pixel())
                }
                .buttonStyle(PixelButtonStyle())
                .help("Copy full message")
            }
            HStack(spacing: 8) {
                Text(entry.time)
                Text(item.kind)
                if item.reply { Text("reply") }
                if item.replied { Text("replied") }
                Text(delivery(item)).foregroundStyle(deliveryColor(item))
                Spacer(minLength: 4)
                Text("pane \(item.from.pane) → \(item.to.pane)")
                    .foregroundStyle(Color.white.opacity(0.32))
                    .textSelection(.enabled)
            }
            .font(.pixel())
            .foregroundStyle(Color.white.opacity(0.5))
            .lineLimit(1)
            ScrollView {
                Text(item.text.isEmpty ? "(Empty message)" : item.text)
                    .font(.pixel())
                    .foregroundStyle(Color.white.opacity(0.9))
                    .lineSpacing(2)
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(12)
            }
            .frame(maxHeight: .infinity)
            .background(PixelBox(2).fill(Color.white.opacity(0.05)))
            HStack(spacing: 6) {
                jumpButton(item.from.pane, roster, role: "sender")
                if item.to.pane != item.from.pane { jumpButton(item.to.pane, roster, role: "recipient") }
                Spacer(minLength: 0)
            }
        }
    }

    @ViewBuilder
    private func jumpButton(_ pane: UInt64, _ roster: TernRoster, role: String) -> some View {
        if let agent = roster.agent(pane) {
            Button { openAgent(pane) } label: {
                Text(agentName(agent)).font(.pixel())
            }
            .buttonStyle(PixelButtonStyle(tint: agentHue(pane)))
            .help("Open \(role) \(agentName(agent))")
        }
    }

    // MARK: Exchanges

    private func exchangeList(_ groups: [TrafficGroup], _ roster: TernRoster, empty: String) -> some View {
        let recent = groups.suffix(80).reversed()
        return ScrollView {
            LazyVStack(alignment: .leading, spacing: 5) {
                if recent.isEmpty {
                    Text(empty)
                        .font(.pixel())
                        .foregroundStyle(Color.white.opacity(0.45))
                        .padding(.vertical, 10)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                ForEach(recent) { group in
                    exchangeGroup(group, roster)
                        .transition(.asymmetric(insertion: .move(edge: .top).combined(with: .opacity), removal: .opacity))
                }
            }
            // Only fresh arrivals animate; loaded history and delivery updates do not change `arrivals`.
            .animation(reduceMotion ? nil : .spring(response: 0.45, dampingFraction: 0.78), value: traffic.arrivals)
        }
        .scrollIndicators(.never)
        .frame(maxHeight: .infinity)
    }

    private func exchangeGroup(_ group: TrafficGroup, _ roster: TernRoster) -> some View {
        let expanded = state.expandedReceipts.contains(group.id)
        return LazyVStack(alignment: .leading, spacing: 5) {
            if group.receipts.count > 1 { repeatDisclosure(group) }
            if expanded && group.receipts.count > 1 {
                ForEach(group.receipts.reversed()) { entry in exchangeRow(entry, roster) }
            } else {
                exchangeRow(group.root, roster)
            }
        }
    }

    /// One exchange in an agent's detail: the sender's face, "sender to recipient", and a speech bubble preview.
    private func exchangeRow(_ entry: TrafficEntry, _ roster: TernRoster) -> some View {
        let item = entry.item
        let fresh = !reduceMotion && traffic.isFresh(entry.id)
        let color = item.failed ? Color.red : roster.hue(TernEndpoint(item.from))
        let selected = state.selectedMessage == entry.id
        let bubble = PixelBox(topLeading: 0, topTrailing: 2, bottomLeading: 2, bottomTrailing: 2)
        return Button { state.selectedMessage = entry.id } label: {
            HStack(alignment: .top, spacing: 8) {
                nodeFace(item.from, roster, size: 20, speaking: fresh)
                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: 4) {
                        speakerLabel(item.from, roster)
                        Text(item.reply ? "replied to" : "to")
                            .font(.pixel())
                            .foregroundStyle(Color.white.opacity(0.4))
                            .fixedSize()
                        speakerLabel(item.to, roster)
                        Spacer(minLength: 6)
                        deliveryGlyph(item)
                        Text(entry.time)
                            .font(.pixel())
                            .foregroundStyle(Color.white.opacity(0.36))
                            .fixedSize()
                    }
                    Text(entry.preview)
                        .font(.pixel())
                        .foregroundStyle(Color.white.opacity(0.84))
                        .lineLimit(2)
                        .multilineTextAlignment(.leading)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 9).padding(.vertical, 6)
                        .background(bubble.fill(fresh ? color.opacity(0.17) : Color.white.opacity(selected ? 0.12 : 0.05)))
                        .overlay(bubble.strokeBorder(color.opacity(fresh ? 0.75 : selected ? 0.45 : 0), lineWidth: 1))
                        .animation(reduceMotion ? nil : .easeOut(duration: 0.9), value: fresh)
                }
            }
            .padding(.horizontal, 4).padding(.vertical, 3)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help("\(route(item, roster)) · \(delivery(item))\n\(entry.preview)")
        .accessibilityLabel("\(entry.time), \(route(item, roster, spoken: true)), \(delivery(item)), \(entry.preview)")
        .accessibilityAddTraits(selected ? .isSelected : [])
    }

    private func speakerLabel(_ node: TernNode, _ roster: TernRoster) -> some View {
        let endpoint = TernEndpoint(node)
        return HStack(spacing: 3) {
            Text(roster.name(TernEndpoint(pane: node.pane)))
                .font(.pixel())
                .foregroundStyle(roster.hue(endpoint))
            if let subagent = endpoint.subagent {
                Text(subagent)
                    .font(.pixel())
                    .foregroundStyle(Color.white.opacity(0.5))
            }
        }
        .lineLimit(1)
    }

    private func nodeFace(_ node: TernNode, _ roster: TernRoster, size: CGFloat, speaking: Bool = false) -> some View {
        let endpoint = TernEndpoint(node)
        return AgentAvatar(hue: roster.hue(endpoint), mood: roster.mood(endpoint), speaking: speaking, size: size)
    }

    private func nodeChip(_ node: TernNode, _ roster: TernRoster) -> some View {
        HStack(spacing: 5) {
            nodeFace(node, roster, size: 16)
            speakerLabel(node, roster)
        }
    }

    private func deliveryGlyph(_ item: TernMessage) -> some View {
        // Pending states have no sprite: a square for observable-but-undelivered, a hollow one when
        // delivery cannot be observed.
        Group {
            if item.failed {
                PixelIcon(glyph: .warning)
            } else if item.deliveredMs != nil {
                PixelIcon(glyph: .check)
            } else if item.deliveryObservable {
                Rectangle().frame(width: 3, height: 3).frame(width: 9, height: 9)
            } else {
                Rectangle().strokeBorder(lineWidth: 1).frame(width: 5, height: 5).frame(width: 9, height: 9)
            }
        }
        .foregroundStyle(deliveryColor(item))
        .help(delivery(item))
        .accessibilityElement()
        .accessibilityLabel(delivery(item))
    }

    // MARK: Controls

    private func composer(_ agent: TernAgent, hue: Color) -> some View {
        let canSend = agent.open && !state.draft.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        return HStack(spacing: 7) {
            TextField(agent.open ? "Message \(agentName(agent))…" : "Pane is closed", text: $state.draft)
                .textFieldStyle(.plain)
                .font(.pixel())
                .padding(.horizontal, 10).padding(.vertical, 7)
                .background(PixelBox(2).fill(Color.white.opacity(0.08)))
                .disabled(!agent.open)
                .focused($composing)
                .onSubmit { send(to: agent) }
                .accessibilityLabel("Message \(agentName(agent))")
            Button { send(to: agent) } label: {
                PixelIcon(glyph: .send)
                    .foregroundStyle(canSend ? Color.black : Color.white.opacity(0.4))
                    .frame(width: 32, height: 28)
                    .background(PixelBox(2).fill(canSend ? hue : Color.white.opacity(0.1)))
            }
            .buttonStyle(.plain)
            .disabled(!canSend)
            .help("Send to pane \(agent.pane)")
            .accessibilityLabel("Send to \(agentName(agent))")
        }
    }

    private var backButton: some View {
        Button { withAnimation(reduceMotion ? nil : .easeInOut(duration: 0.18)) { _ = state.back() } } label: {
            PixelIcon(glyph: .chevronLeft)
                .frame(width: 22, height: 22)
                .background(PixelBox(2).fill(Color.white.opacity(0.08)))
        }
        .buttonStyle(.plain)
        .help("Back (Esc)")
        .accessibilityLabel("Back")
    }

    private func banner(_ title: String, _ text: String, offerRestart: Bool) -> some View {
        HStack(alignment: .top, spacing: 8) {
            PixelIcon(glyph: .warning).foregroundStyle(Color.orange)
            VStack(alignment: .leading, spacing: 2) {
                Text(title).font(.pixel()).foregroundStyle(Color.orange)
                ScrollView {
                    Text(text)
                        .font(.pixel())
                        .foregroundStyle(Color.white.opacity(0.8))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                .frame(maxHeight: 32)
            }
            Button { copy(text, token: title) } label: {
                PixelIcon(glyph: copied == title ? .check : .copy)
            }
            .buttonStyle(.plain)
            .help("Copy error")
            .accessibilityLabel("Copy \(title) error")
            if offerRestart {
                Button("Restart") { store.restart() }
                    .buttonStyle(PixelButtonStyle())
                    .help("Restart ternwatch")
            }
        }
        .padding(8)
        .background(PixelBox(2).fill(Color.orange.opacity(0.1)))
    }

    private func sectionLabel(_ text: String) -> some View {
        Text(text.uppercased())
            .font(.pixel())
            .tracking(1)
            .lineLimit(1)
            .fixedSize()
            .foregroundStyle(Color.white.opacity(0.45))
    }

    private func countBadge(_ count: Int, color: Color) -> some View {
        Text("\(count)")
            .font(.pixel())
            .lineLimit(1)
            .fixedSize()
            .foregroundStyle(color)
            .padding(.horizontal, 6).padding(.vertical, 1)
            .background(PixelBox(1).fill(color.opacity(0.14)))
    }

    private func toolTag(_ name: String, hue: Color) -> some View {
        Text(name)
            .font(.pixel())
            .foregroundStyle(hue)
            .lineLimit(1)
            .padding(.horizontal, 5).padding(.vertical, 1)
            .background(PixelBox(1).fill(hue.opacity(0.14)))
            .fixedSize()
    }

    // MARK: Helpers

    /// A fresh message from this endpoint is in flight: its face talks. Pulses expire, so this is finite.
    private func speaking(_ endpoint: TernEndpoint) -> Bool {
        !reduceMotion && traffic.speaking(endpoint)
    }

    /// A fresh message to this endpoint just arrived: its face perks up.
    private func listening(_ endpoint: TernEndpoint) -> Bool {
        !reduceMotion && traffic.listening(endpoint)
    }

    /// Pins a conversation (nil follows the latest) and returns to it from any detail.
    private func show(_ id: TernConversation.ID?) {
        withAnimation(reduceMotion ? nil : .easeInOut(duration: 0.18)) {
            state.pickingRecipient = false
            state.selectedMessage = nil
            state.selectedPane = nil
            state.pinned = id
        }
    }

    private func openAgent(_ pane: UInt64) {
        withAnimation(reduceMotion ? nil : .easeInOut(duration: 0.18)) {
            state.selectedMessage = nil
            if state.selectedPane != pane { state.draft = "" }
            state.selectedPane = pane
        }
    }

    private func send(to agent: TernAgent) {
        let text = state.draft
        guard agent.open, !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        store.send(agent.pane, message: text)
        state.draft = ""
    }

    private func copy(_ text: String, token: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
        copied = token
    }

    /// Chip names stay short so several conversations fit the strip; the full pair is in help and VoiceOver.
    private func chipName(_ name: String) -> String {
        name.count > 16 ? String(name.prefix(15)) + "…" : name
    }

    private func route(_ item: TernMessage, _ roster: TernRoster, spoken: Bool = false) -> String {
        let from = roster.fullName(TernEndpoint(item.from))
        let to = roster.fullName(TernEndpoint(item.to))
        return spoken ? "\(from) \(item.reply ? "replied to" : "to") \(to)" : "\(from) → \(to)"
    }
}

private enum DetailMode: Hashable {
    case empty
    case conversation(TernConversation.ID)
    case agent(UInt64)
    case message(TrafficEntry.ID)
}

// MARK: - Agent avatars

/// Real agent states an avatar can show. Avatars never invent feelings: each look maps to a state
/// the stream reports (working, waiting on running subagents, idle, closed pane, watcher offline).
enum AgentMood: Equatable {
    case working
    case waiting
    case idle
    case closed
    case offline
}

/// Square-ended bar: a dim track and a solid fill that grows in whole 2-point cells.
private struct ProgressTrack: View {
    let fraction: Double
    let color: Color

    var body: some View {
        GeometryReader { proxy in
            let filled = (proxy.size.width * min(1, max(0, fraction)) / 2).rounded() * 2
            ZStack(alignment: .leading) {
                Rectangle().fill(Color.white.opacity(0.1))
                Rectangle().fill(color).frame(width: max(2, filled))
            }
        }
    }
}

// MARK: - Traffic identity

struct TrafficKey: Hashable {
    let from: UInt64
    let fromSubagent: String?
    let to: UInt64
    let toSubagent: String?
    let kind: String
    let text: String
    let sent: Int64?
    let reply: Bool
    init(_ item: TernMessage) {
        from = item.from.pane; fromSubagent = item.from.subagent
        to = item.to.pane; toSubagent = item.to.subagent
        kind = item.kind; text = item.text; sent = item.sentMs; reply = item.reply
    }
}

struct TrafficEntry: Identifiable {
    enum ID: Hashable {
        case receipt(UInt64)
        case legacy(key: TrafficKey, occurrence: Int)
    }
    let id: ID
    let item: TernMessage
    let offset: Int
    let time: String
    let preview: String
    init(id: ID, item: TernMessage, offset: Int) {
        self.id = id; self.item = item; self.offset = offset
        time = (item.sentMs ?? item.deliveredMs).map { Self.clock.string(from: Date(timeIntervalSince1970: Double($0) / 1000)) } ?? "--:--:--"
        preview = item.text.isEmpty ? "(Empty message)" : String(item.text.prefix(240)).replacingOccurrences(of: "\n", with: " ")
    }
    private static let clock: DateFormatter = {
        let formatter = DateFormatter()
        formatter.dateFormat = "HH:mm:ss"
        return formatter
    }()
}

struct TrafficPulse: Identifiable {
    let id = UUID()
    let entry: TrafficEntry.ID
    let from: UInt64
    let fromSubagent: String?
    let to: UInt64
    let toSubagent: String?
    let failed: Bool
    let started: Date
}

// MARK: - Formatting

func agentName(_ agent: TernAgent) -> String {
    for candidate in [agent.label, agent.title] {
        let name = candidate.trimmingCharacters(in: .whitespacesAndNewlines)
        let identifier = name.replacingOccurrences(of: "-", with: "")
        let isOpaque = name == String(agent.pane) || (identifier.count >= 20 && identifier.allSatisfy(\.isHexDigit))
        if !name.isEmpty && !isOpaque { return name }
    }
    let repository = agent.cwd.isEmpty ? "" : URL(fileURLWithPath: agent.cwd).lastPathComponent
    return repository.isEmpty ? "Agent · tab \(agent.tab)" : "\(repository) · t\(agent.tab)"
}

private func agentStatus(_ agent: TernAgent) -> String {
    if !agent.open { return "closed" }
    if agent.working { return agent.tool.map { $0.name.isEmpty ? "working" : $0.name } ?? "working" }
    if agent.subagents.contains(where: \.running) { return "orchestrating" }
    return "idle"
}

func agentMood(_ agent: TernAgent) -> AgentMood {
    if !agent.open { return .closed }
    if agent.working { return .working }
    if agent.subagents.contains(where: \.running) { return .waiting }
    return .idle
}

/// The agent's current action in plain words: its reported tool intent first, then what it waits on.
private func intentLine(_ agent: TernAgent) -> String {
    let intent = agent.tool?.intent.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
    if !intent.isEmpty { return intent }
    let running = agent.subagents.filter(\.running).count
    if !agent.working && running > 0 { return "Waiting on \(running) subagent\(running == 1 ? "" : "s")" }
    if let name = agent.tool?.name, !name.isEmpty { return "Running \(name)" }
    if !agent.description.isEmpty { return agent.description }
    return agent.working ? "Working" : "Idle"
}

/// Tool name shown as a tag beside an intent; omitted when the intent line already names the tool.
private func toolTagName(_ agent: TernAgent) -> String? {
    guard let tool = agent.tool, !tool.name.isEmpty,
          !tool.intent.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return nil }
    return tool.name
}

private func agentHeadline(_ agent: TernAgent) -> String {
    var parts = [agentStatus(agent)]
    if agent.open && !agent.working { parts.append("quiet \(quietLabel(agent.quietMs))") }
    if let model = agent.model, !model.isEmpty { parts.append(model) } else if let cli = agent.cli, !cli.isEmpty { parts.append(cli) }
    parts.append("\(tokenLabel(agent.tokens)) tokens")
    return parts.joined(separator: " · ")
}

private func locationLine(_ agent: TernAgent) -> String {
    let home = FileManager.default.homeDirectoryForCurrentUser.path
    let cwd = agent.cwd.hasPrefix(home) ? "~" + agent.cwd.dropFirst(home.count) : agent.cwd
    return "pane \(agent.pane) · tab \(agent.tab)" + (cwd.isEmpty ? "" : " · \(cwd)")
}

private func quietLabel(_ milliseconds: UInt64) -> String {
    let seconds = milliseconds / 1000
    if seconds < 60 { return "\(seconds)s" }
    if seconds < 3600 { return "\(seconds / 60)m" }
    if seconds < 86_400 { return "\(seconds / 3600)h" }
    return "\(seconds / 86_400)d"
}

private func tokenLabel(_ tokens: UInt64) -> String {
    if tokens >= 1_000_000 { return String(format: "%.1fM", Double(tokens) / 1_000_000) }
    if tokens >= 1_000 { return String(format: "%.1fk", Double(tokens) / 1_000) }
    return String(tokens)
}

private func delivery(_ message: TernMessage) -> String {
    if message.failed { return message.outcome.flatMap { $0.isEmpty ? nil : $0 } ?? "Failed" }
    if let outcome = message.outcome, !outcome.isEmpty { return outcome }
    if message.deliveredMs != nil { return "Delivered" }
    return message.deliveryObservable ? "Awaiting delivery" : "Delivery unknown"
}

private func deliveryColor(_ message: TernMessage) -> Color {
    if message.failed { return Color.red }
    if message.deliveredMs != nil { return Color.green.opacity(0.85) }
    return Color.white.opacity(message.deliveryObservable ? 0.45 : 0.35)
}
