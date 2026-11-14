import Foundation

/// Decides when the notch opens, pins, and folds. Free of AppKit so it can be driven directly:
/// the owner reports pointer, click, and toggle input and carries out the returned effects.
///
/// - Hover opens after a short dwell, unpinned: the app is not activated and keyboard focus stays
///   in the frontmost app. Leaving folds an unpinned notch after a delay; coming back cancels it.
/// - A click inside an unpinned notch pins it (`.pin`: activate and take focus). Composing always
///   pins first, because the composer can only take focus through a click. A click outside the
///   silhouette (its transparent shadow margin) folds it.
/// - Click and keyboard toggles open pinned. A toggle right after opening pins instead of
///   folding, so the click that follows a hover-open does not bounce the notch shut.
/// - After a fold with the pointer still on the notch, hover opens again only after the pointer leaves.
final class NotchHoverPolicy {
    enum Effect: Equatable {
        case expand(pinned: Bool)
        case collapse
        case pin
    }

    struct Timing {
        /// Dwell before hover opens, so a pointer crossing the menu bar does not unfold the notch.
        var openDelay: TimeInterval = 0.2
        /// Delay before an unpinned notch folds, forgiving a pointer that briefly overshoots the edge.
        var closeDelay: TimeInterval = 0.6
        /// A toggle this soon after opening pins instead of folding.
        var toggleGrace: TimeInterval = 0.6
    }

    typealias Cancel = () -> Void
    typealias Scheduler = (TimeInterval, @escaping () -> Void) -> Cancel

    static let mainQueue: Scheduler = { delay, work in
        let item = DispatchWorkItem(block: work)
        DispatchQueue.main.asyncAfter(deadline: .now() + delay, execute: item)
        return item.cancel
    }

    /// Carries out an effect. Called synchronously; must not call back into the policy.
    var apply: (Effect) -> Void = { _ in }
    /// Whether the pointer is over the collapsed silhouette. A resize can report a spurious exit
    /// while the pointer is still there, which must not re-arm hover after a fold.
    var pointerOverCollapsed: () -> Bool = { false }

    private(set) var expanded = false
    /// Opened from hover and not clicked since.
    private(set) var unpinned = false
    private(set) var hovering = false
    private(set) var armed = true

    private let timing: Timing
    private let now: () -> Date
    private let schedule: Scheduler
    private var expandedAt = Date.distantPast
    private var cancelOpen: Cancel?
    private var cancelClose: Cancel?

    init(timing: Timing = Timing(), now: @escaping () -> Date = Date.init, schedule: @escaping Scheduler = NotchHoverPolicy.mainQueue) {
        self.timing = timing
        self.now = now
        self.schedule = schedule
    }

    /// Pointer entered or left the notch silhouette.
    func pointerChanged(inside: Bool) {
        hovering = inside
        if inside {
            stopClose()
            guard !expanded, armed, cancelOpen == nil else { return }
            cancelOpen = schedule(timing.openDelay) { [weak self] in
                guard let self = self else { return }
                self.cancelOpen = nil
                guard self.hovering, self.armed, !self.expanded else { return }
                self.setExpanded(true, pinned: false)
            }
        } else {
            stopOpen()
            if !pointerOverCollapsed() { armed = true }
            guard expanded, unpinned else { return }
            cancelClose = schedule(timing.closeDelay) { [weak self] in
                guard let self = self else { return }
                self.cancelClose = nil
                guard !self.hovering, self.expanded, self.unpinned else { return }
                self.setExpanded(false)
            }
        }
    }

    /// A mouse button went down anywhere in the notch window.
    func clicked() {
        guard expanded, unpinned else { return }
        guard hovering else { setExpanded(false); return }
        unpinned = false
        stopClose()
        apply(.pin)
    }

    /// Click on the notch or keyboard activation.
    func toggle() {
        if expanded && now().timeIntervalSince(expandedAt) < timing.toggleGrace {
            clicked()
        } else {
            setExpanded(!expanded)
        }
    }

    /// Explicit open (pinned) or fold: Esc, chevron, app deactivation, or a timer above.
    func setExpanded(_ expanded: Bool, pinned: Bool = true) {
        stopOpen()
        stopClose()
        guard self.expanded != expanded else { return }
        self.expanded = expanded
        unpinned = expanded && !pinned
        if expanded {
            expandedAt = now()
        } else {
            armed = !hovering
        }
        apply(expanded ? .expand(pinned: pinned) : .collapse)
    }

    private func stopOpen() {
        cancelOpen?()
        cancelOpen = nil
    }

    private func stopClose() {
        cancelClose?()
        cancelClose = nil
    }
}
