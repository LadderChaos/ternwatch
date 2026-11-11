import AppKit
import SwiftUI

@main
struct TernNotchApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate

    var body: some Scene {
        Settings { EmptyView() }
    }
}

/// Geometry of the notch surface on the current screen.
struct NotchMetrics: Equatable {
    /// Camera housing width; 0 on displays without a notch (a faux notch is drawn over the menu bar).
    var cameraWidth: CGFloat = 0
    /// Camera housing (or menu bar) height. The collapsed silhouette is exactly this tall.
    var headerHeight: CGFloat = 32
    var collapsedWidth: CGFloat = 2 * NotchStyle.wing + NotchStyle.fauxCameraGap
    var expandedSize = CGSize(width: NotchStyle.expandedWidth, height: NotchStyle.expandedHeight)
}

final class NotchModel: ObservableObject {
    @Published var expanded = false
    @Published var metrics = NotchMetrics()
    /// Horizontal shift of the silhouette from the window center, nonzero only when the window is clamped to a screen edge.
    @Published var centerOffset: CGFloat = 0
}

enum NotchStyle {
    /// Width of each black wing beside the camera housing while collapsed: the silhouette's top flare,
    /// clear room at the outer edge, an avatar, and `wingInset` next to the camera.
    static let wing: CGFloat = 36
    /// Space between the camera housing and each collapsed avatar.
    static let wingInset: CGFloat = 2
    /// Stand-in camera gap on displays without a notch.
    static let fauxCameraGap: CGFloat = 12
    /// Transparent window margin around the expanded silhouette for its drop shadow.
    static let margin: CGFloat = 30
    static let expandedWidth: CGFloat = 820
    static let expandedHeight: CGFloat = 520
    static let collapsedRadii = (top: CGFloat(6), bottom: CGFloat(10))
    static let expandedRadii = (top: CGFloat(14), bottom: CGFloat(26))
    static let contentInset: CGFloat = 24
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private let store = TernStreamStore()
    private let model = NotchModel()
    private let viewState = TernViewState()
    private lazy var traffic = TernTraffic(store: store)
    private var panel: NotchPanel?
    private var pendingShrink: DispatchWorkItem?
    private var screenFrame = NSRect.zero
    private var notchCenterX: CGFloat = 0
    /// Owns the requested open state; `model.expanded` follows it one run-loop turn later when opening.
    private let hover = NotchHoverPolicy()

    func applicationDidFinishLaunching(_ notification: Notification) {
        PixelArt.registerFont()
        let panel = NotchPanel(
            contentRect: .zero,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        panel.isFloatingPanel = true
        panel.hidesOnDeactivate = false
        panel.isMovable = false
        panel.isOpaque = false
        panel.backgroundColor = .clear
        // The silhouette draws its own shadow; a window shadow would outline the transparent frame.
        panel.hasShadow = false
        panel.level = .mainMenu + 3
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .stationary, .ignoresCycle]
        panel.isReleasedWhenClosed = false
        panel.onCancel = { [weak self] in self?.back() }
        panel.onClick = { [weak self] in self?.hover.clicked() }
        hover.apply = { [weak self] effect in self?.apply(effect) }
        hover.pointerOverCollapsed = { [weak self] in
            guard let self = self else { return false }
            return self.collapsedRect().contains(NSEvent.mouseLocation)
        }

        let root = NotchRootView(
            store: store,
            traffic: traffic,
            model: model,
            viewState: viewState,
            toggle: { [weak self] in self?.hover.toggle() },
            collapse: { [weak self] in self?.hover.setExpanded(false) },
            hover: { [weak self] inside in self?.hover.pointerChanged(inside: inside) }
        )
        let hostingView = NotchHostingView(rootView: root)
        // The panel frame drives layout; SwiftUI must never resize the window. As a window's content
        // view, NSHostingView resizes its window from `windowDidLayout`, and at the top of a notched
        // screen each resize changes the safe-area insets, which re-invalidates the hosting view until
        // AppKit aborts with "more Update Constraints in Window passes than there are views". Hosting
        // inside a plain container with autoresizing, ignoring safe areas, keeps geometry window-owned.
        hostingView.sizingOptions = []
        hostingView.safeAreaRegions = []
        hostingView.translatesAutoresizingMaskIntoConstraints = true
        hostingView.autoresizingMask = []
        let container = NotchContainerView()
        container.addSubview(hostingView)
        hostingView.frame = container.bounds
        panel.contentView = container
        self.panel = panel

        NotificationCenter.default.addObserver(self, selector: #selector(screenParametersChanged), name: NSApplication.didChangeScreenParametersNotification, object: nil)
        store.start()
        positionPanel()
        panel.orderFrontRegardless()
    }

    func applicationWillTerminate(_ notification: Notification) {
        store.stop()
    }

    /// Clicking anywhere outside the notch folds it back into the camera housing. A hover-opened
    /// notch never activated the app, so this only fires after a click or keyboard open.
    func applicationDidResignActive(_ notification: Notification) {
        hover.setExpanded(false)
    }

    @objc private func screenParametersChanged() {
        positionPanel()
        panel?.orderFrontRegardless()
    }

    /// Esc steps back from message to agent to pinned conversation, then collapses.
    private func back() {
        guard hover.expanded else { return }
        let reduce = NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
        var handled = false
        withAnimation(reduce ? nil : .easeInOut(duration: 0.18)) { handled = viewState.back() }
        if !handled { hover.setExpanded(false) }
    }

    private func collapsedRect() -> NSRect {
        let metrics = model.metrics
        return NSRect(x: notchCenterX - metrics.collapsedWidth / 2, y: screenFrame.maxY - metrics.headerHeight,
                      width: metrics.collapsedWidth, height: metrics.headerHeight)
    }

    // MARK: Expansion

    /// Carries out the hover policy's decisions. An unpinned open (from hover) only orders the
    /// panel front: it never activates the app or takes key focus, so typing stays in the frontmost app.
    private func apply(_ effect: NotchHoverPolicy.Effect) {
        let reduce = NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
        switch effect {
        case .pin:
            NSApplication.shared.activate(ignoringOtherApps: true)
            panel?.makeKeyAndOrderFront(nil)
        case .expand(let pinned):
            pendingShrink?.cancel()
            pendingShrink = nil
            // Reveal the fixed-size hosting canvas before unfolding the silhouette.
            applyFrame(large: true)
            if pinned {
                NSApplication.shared.activate(ignoringOtherApps: true)
                panel?.makeKeyAndOrderFront(nil)
            } else {
                panel?.orderFrontRegardless()
            }
            DispatchQueue.main.async { [weak self] in
                guard let self = self, self.hover.expanded else { return }
                withAnimation(reduce ? .easeOut(duration: 0.12) : .spring(response: 0.42, dampingFraction: 0.84)) {
                    self.model.expanded = true
                }
            }
        case .collapse:
            pendingShrink?.cancel()
            withAnimation(reduce ? .easeOut(duration: 0.12) : .spring(response: 0.34, dampingFraction: 0.92)) {
                model.expanded = false
            }
            // Shrink the window back to the collapsed silhouette once the fold finishes, so the
            // transparent margin never blocks clicks on the menu bar.
            let task = DispatchWorkItem { [weak self] in
                guard let self = self, !self.hover.expanded else { return }
                self.pendingShrink = nil
                self.applyFrame(large: false)
                self.panel?.orderFrontRegardless()
            }
            pendingShrink = task
            DispatchQueue.main.asyncAfter(deadline: .now() + (reduce ? 0.15 : 0.5), execute: task)
        }
    }

    private func notchScreen() -> NSScreen? {
        NSScreen.screens.first {
            $0.safeAreaInsets.top > 0 && $0.auxiliaryTopLeftArea != nil && $0.auxiliaryTopRightArea != nil
        } ?? NSScreen.main ?? NSScreen.screens.first
    }

    private func positionPanel() {
        guard panel != nil, let screen = notchScreen() else { return }
        let frame = screen.frame
        var cameraWidth: CGFloat = 0
        var centerX = frame.midX
        if screen.safeAreaInsets.top > 0,
           let leftArea = screen.auxiliaryTopLeftArea,
           let rightArea = screen.auxiliaryTopRightArea {
            let left = frame.minX + leftArea.width
            let right = frame.maxX - rightArea.width
            if right > left {
                cameraWidth = right - left
                centerX = (left + right) / 2
            }
        }
        let menuBarHeight = frame.maxY - screen.visibleFrame.maxY
        let headerHeight = cameraWidth > 0 ? screen.safeAreaInsets.top : min(max(menuBarHeight, 24), 32)
        let collapsedWidth = (cameraWidth > 0 ? cameraWidth : NotchStyle.fauxCameraGap) + 2 * NotchStyle.wing
        let width = max(collapsedWidth, min(NotchStyle.expandedWidth, frame.width - 2 * NotchStyle.margin))
        let available = frame.maxY - screen.visibleFrame.minY - NotchStyle.margin
        let height = max(headerHeight + 120, min(NotchStyle.expandedHeight, available))
        let metrics = NotchMetrics(
            cameraWidth: cameraWidth,
            headerHeight: headerHeight,
            collapsedWidth: collapsedWidth,
            expandedSize: CGSize(width: width, height: height)
        )
        if model.metrics != metrics { model.metrics = metrics }
        screenFrame = frame
        notchCenterX = centerX
        applyFrame(large: hover.expanded || pendingShrink != nil)
    }

    /// The window's top edge is always the physical top of the screen, centered on the camera.
    private func applyFrame(large: Bool) {
        guard let panel else { return }
        let metrics = model.metrics
        let size = large
            ? CGSize(width: metrics.expandedSize.width + 2 * NotchStyle.margin, height: metrics.expandedSize.height + NotchStyle.margin)
            : CGSize(width: metrics.collapsedWidth, height: metrics.headerHeight)
        let x = min(max(notchCenterX - size.width / 2, screenFrame.minX), screenFrame.maxX - size.width)
        let offset = notchCenterX - (x + size.width / 2)
        if model.centerOffset != offset { model.centerOffset = offset }
        panel.disableScreenUpdatesUntilFlush()
        if let container = panel.contentView as? NotchContainerView {
            container.canvasSize = CGSize(width: metrics.expandedSize.width + 2 * NotchStyle.margin,
                                          height: metrics.expandedSize.height + NotchStyle.margin)
        }
        panel.setFrame(NSRect(x: x, y: screenFrame.maxY - size.height, width: size.width, height: size.height), display: false, animate: false)
        panel.contentView?.layoutSubtreeIfNeeded()
        panel.displayIfNeeded()
    }
}

private final class NotchPanel: NSPanel {
    var onCancel: (() -> Void)?
    /// Fires before a click is dispatched, so a hover-opened notch can pin itself.
    var onClick: (() -> Void)?
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }
    override func cancelOperation(_ sender: Any?) { onCancel?() }
    /// Cmd-Q quits while the notch is key; the app has no menu bar item or visible app menu.
    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        if event.modifierFlags.intersection(.deviceIndependentFlagsMask) == .command,
           event.charactersIgnoringModifiers?.lowercased() == "q" {
            NSApplication.shared.terminate(nil)
            return true
        }
        return super.performKeyEquivalent(with: event)
    }
    override func sendEvent(_ event: NSEvent) {
        switch event.type {
        case .leftMouseDown, .rightMouseDown, .otherMouseDown: onClick?()
        default: break
        }
        super.sendEvent(event)
    }
}

private final class NotchHostingView<Content: View>: NSHostingView<Content> {
    /// The first click on the notch acts immediately, even while another app is active.
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
}

/// Plain content view that owns the hosting view's geometry; it also takes the first click.
private final class NotchContainerView: NSView {
    // Keep SwiftUI's coordinate space unchanged across window resizes. The smaller window
    // clips this canvas while collapsed; both stay centered and anchored at the screen top.
    var canvasSize = CGSize.zero {
        didSet { positionCanvas() }
    }

    override func resizeSubviews(withOldSize oldSize: NSSize) {
        positionCanvas()
    }

    private func positionCanvas() {
        subviews.first?.frame = NSRect(x: (bounds.width - canvasSize.width) / 2,
                                      y: bounds.height - canvasSize.height,
                                      width: canvasSize.width, height: canvasSize.height)
    }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
}

/// Hardware-notch silhouette: flush with the screen top, flaring outward at the top corners and
/// rounded at the bottom, so the black surface reads as an extension of the camera housing. The
/// outline stays smooth to match the physical notch; only the content inside is pixel art.
struct NotchSilhouette: Shape {
    var topRadius: CGFloat
    var bottomRadius: CGFloat

    var animatableData: AnimatablePair<CGFloat, CGFloat> {
        get { AnimatablePair(topRadius, bottomRadius) }
        set {
            topRadius = newValue.first
            bottomRadius = newValue.second
        }
    }

    func path(in rect: CGRect) -> Path {
        let top = max(0, min(topRadius, rect.width / 4, rect.height / 2))
        let bottom = max(0, min(bottomRadius, (rect.width - 2 * top) / 2, rect.height - top))
        var path = Path()
        path.move(to: CGPoint(x: rect.minX, y: rect.minY))
        path.addQuadCurve(to: CGPoint(x: rect.minX + top, y: rect.minY + top),
                          control: CGPoint(x: rect.minX + top, y: rect.minY))
        path.addArc(tangent1End: CGPoint(x: rect.minX + top, y: rect.maxY),
                    tangent2End: CGPoint(x: rect.maxX - top, y: rect.maxY), radius: bottom)
        path.addArc(tangent1End: CGPoint(x: rect.maxX - top, y: rect.maxY),
                    tangent2End: CGPoint(x: rect.maxX - top, y: rect.minY), radius: bottom)
        path.addLine(to: CGPoint(x: rect.maxX - top, y: rect.minY + top))
        path.addQuadCurve(to: CGPoint(x: rect.maxX, y: rect.minY),
                          control: CGPoint(x: rect.maxX - top, y: rect.minY))
        path.closeSubpath()
        return path
    }
}

private struct NotchRootView: View {
    @ObservedObject var store: TernStreamStore
    let traffic: TernTraffic
    @ObservedObject var model: NotchModel
    let viewState: TernViewState
    let toggle: () -> Void
    let collapse: () -> Void
    let hover: (Bool) -> Void
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @AppStorage(TernPreferences.showSubagents) private var showSubagents = false

    var body: some View {
        let metrics = model.metrics
        let expanded = model.expanded
        let radii = expanded ? NotchStyle.expandedRadii : NotchStyle.collapsedRadii
        let shape = NotchSilhouette(topRadius: radii.top, bottomRadius: radii.bottom)
        let size = expanded ? metrics.expandedSize : CGSize(width: metrics.collapsedWidth, height: metrics.headerHeight)
        VStack(spacing: 0) {
            header(metrics, expanded: expanded)
            if expanded {
                // Fixed final size: the silhouette unfolds over already-laid-out content instead of reflowing it.
                TernView(store: store, traffic: traffic, state: viewState)
                    .frame(width: metrics.expandedSize.width - 2 * NotchStyle.contentInset,
                           height: max(0, metrics.expandedSize.height - metrics.headerHeight - 18),
                           alignment: .top)
                    .padding(.top, 4)
                    .transition(reduceMotion ? .opacity : .opacity.combined(with: .offset(y: -12)))
            }
        }
        .frame(width: size.width, height: size.height, alignment: .top)
        .clipShape(shape)
        .background(
            shape.fill(Color.black)
                .shadow(color: Color.black.opacity(expanded ? 0.55 : 0), radius: 18, y: 10)
        )
        .contentShape(shape)
        // Hover tracks the silhouette only, never the transparent shadow margin around it.
        .onHover(perform: hover)
        .offset(x: model.centerOffset)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .font(.pixel())
        .foregroundStyle(Color.white)
        .preferredColorScheme(.dark)
        // A pin whose conversation left the index (filter change, reconnect) returns both states to live.
        .onReceive(traffic.$conversations) { conversations in
            if let pinned = viewState.pinned, !conversations.contains(where: { $0.id == pinned }) { viewState.pinned = nil }
        }
    }

    @ViewBuilder
    private func header(_ metrics: NotchMetrics, expanded: Bool) -> some View {
        let gap = metrics.cameraWidth > 0 ? metrics.cameraWidth : NotchStyle.fauxCameraGap
        if expanded {
            let roster = TernRoster(store.agents)
            HStack(spacing: 0) {
                HStack(spacing: 8) {
                    Text("TERN").font(.pixel()).foregroundStyle(ternAccent).fixedSize()
                    liveBadge
                    Text("\(roster.active.count) working · \(roster.idle.count) idle")
                        .font(.pixel())
                        .foregroundStyle(Color.white.opacity(0.62))
                        .lineLimit(1)
                    Spacer(minLength: 0)
                }
                .frame(maxWidth: .infinity)
                Color.clear.frame(width: gap)
                HStack(spacing: 8) {
                    Spacer(minLength: 0)
                    subagentsToggle
                    iconButton(.refresh, label: "Restart watcher") { store.restart() }
                    iconButton(.chevronUp, label: "Collapse Tern Notch", action: collapse)
                    iconButton(.power, label: "Quit Tern Notch") { NSApplication.shared.terminate(nil) }
                }
                .frame(maxWidth: .infinity)
            }
            .padding(.horizontal, NotchStyle.contentInset - 4)
            .frame(height: metrics.headerHeight)
            .contentShape(Rectangle())
            .onTapGesture(perform: toggle)
            .transition(.opacity)
        } else {
            CollapsedHeader(store: store, traffic: traffic, state: viewState, gap: gap, height: metrics.headerHeight, action: toggle)
                .transition(.opacity)
        }
    }

    /// Off: conversations are between top-level panes only. On: each subagent takes part on its own.
    private var subagentsToggle: some View {
        Toggle("Subagents", isOn: $showSubagents)
            .toggleStyle(PixelToggleStyle(tint: ternAccent))
            .font(.pixel())
            .foregroundStyle(Color.white.opacity(0.62))
            .fixedSize()
            .help(showSubagents ? "Subagents take part in conversations on their own" : "Show subagents as their own conversation participants")
            .accessibilityHint("Shows subagents as their own conversation participants")
    }

    private var liveBadge: some View {
        HStack(spacing: 4) {
            Rectangle().fill(store.isRunning ? Color.green : Color.orange).frame(width: 4, height: 4)
            Text(store.isRunning ? "live" : "offline")
                .font(.pixel())
                .foregroundStyle(store.isRunning ? Color.green.opacity(0.85) : Color.orange)
        }
        .fixedSize()
        .accessibilityElement(children: .combine)
    }

    private func iconButton(_ glyph: PixelIcon.Glyph, label: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            PixelIcon(glyph: glyph)
                .frame(width: 22, height: 22)
                .background(PixelBox(2).fill(Color.white.opacity(0.08)))
                .contentShape(PixelBox(2))
        }
        .buttonStyle(.plain)
        .help(label)
        .accessibilityLabel(label)
    }
}

/// Collapsed notch content, symmetric around the camera: one agent in each wing. The pair is the shown
/// conversation (pinned, or the latest), on the same sides as in the open transcript; without any
/// conversation it is the first two working agents, then idle ones. An empty wing holds a crew avatar.
/// Working robots continuously bob and flip their antenna; idle robots stay still. A fresh message
/// takes priority: the sender hops and flares its antenna, the recipient's eyes widen, and both glows brighten.
private struct CollapsedHeader: View {
    @ObservedObject var store: TernStreamStore
    @ObservedObject var traffic: TernTraffic
    @ObservedObject var state: TernViewState
    let gap: CGFloat
    let height: CGFloat
    let action: () -> Void
    @Environment(\.accessibilityReduceMotion) private var reduceMotion

    var body: some View {
        let roster = TernRoster(store.agents)
        let conversation = traffic.conversation(pinned: state.pinned)
        let pair = endpoints(conversation, roster)
        Button(action: action) {
            HStack(spacing: 0) {
                wing(pair.left, roster)
                    .padding(.trailing, NotchStyle.wingInset)
                    .frame(maxWidth: .infinity, alignment: .trailing)
                Color.clear.frame(width: gap)
                wing(pair.right, roster)
                    .padding(.leading, NotchStyle.wingInset)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            .padding(.horizontal, NotchStyle.collapsedRadii.top)
            .frame(height: height)
            .contentShape(Rectangle())
            .animation(reduceMotion ? nil : .easeOut(duration: 0.2), value: [pair.left, pair.right])
        }
        .buttonStyle(.plain)
        .accessibilityLabel("Open Tern Notch")
        .accessibilityValue(status(roster, pair))
        .help("Tern · \(status(roster, pair))")
    }

    private var offline: Bool { !store.isRunning || store.error != nil }
    private var crewHue: Color { offline ? Color.orange : ternAccent }
    private var avatarSize: CGFloat { min(20, max(12, height - 11)) }

    private func endpoints(_ conversation: TernConversation?, _ roster: TernRoster) -> (left: TernEndpoint?, right: TernEndpoint?) {
        if let conversation = conversation { return (conversation.left, conversation.right) }
        let candidates = (roster.active + roster.idle.filter(\.open)).prefix(2).map { TernEndpoint(pane: $0.pane, subagent: nil) }
        return (candidates.first, candidates.dropFirst().first)
    }

    /// An avatar over a dithered glow in its color; the glow brightens while it sends or receives.
    @ViewBuilder
    private func wing(_ endpoint: TernEndpoint?, _ roster: TernRoster) -> some View {
        let hue = endpoint.map(roster.hue) ?? crewHue
        let mood = endpoint.map(roster.mood) ?? (offline ? .offline : .idle)
        let speaking = endpoint.map { !reduceMotion && traffic.speaking($0) } ?? false
        let listening = endpoint.map { !reduceMotion && traffic.listening($0) } ?? false
        let lit = speaking || listening
        let awake = mood == .working || mood == .waiting
        ZStack {
            PixelHalo(color: hue, level: lit ? 1 : awake ? 0.55 : 0.2, radius: lit ? 9 : 7)
                .frame(width: avatarSize * 2, height: avatarSize * 2)
            AgentAvatar(hue: hue, mood: mood, speaking: speaking, listening: listening, size: avatarSize,
                        continuousWorking: true)
        }
        .frame(width: avatarSize, height: avatarSize)
        .id(endpoint)
        .transition(.opacity)
    }

    private func status(_ roster: TernRoster, _ pair: (left: TernEndpoint?, right: TernEndpoint?)) -> String {
        if !store.isRunning { return "Watcher unavailable" }
        let working = "\(roster.active.count) working"
        guard let left = pair.left, let right = pair.right else { return working }
        return "\(roster.fullName(left)) and \(roster.fullName(right)), \(working)"
    }
}
