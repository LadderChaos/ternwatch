import AppKit
import CoreText
import SwiftUI

/// Pixel-art primitives shared by every Tern Notch surface. One art pixel is one point (two device
/// pixels on Retina), the grid of the bundled Departure Mono font at 11 points. Shapes have stepped
/// corners, colors change in hard bands, and nothing is blurred.
enum PixelArt {
    static let fontName = "DepartureMono-Regular"
    /// Corner stair step for boxes and the notch silhouette.
    static let step: CGFloat = 2

    /// Makes the bundled font available to this process. Call before the first view renders.
    static func registerFont() {
        guard let url = Bundle.main.url(forResource: fontName, withExtension: "otf") else { return }
        CTFontManagerRegisterFontsForURL(url as CFURL, .process, nil)
    }

    /// Hard-edged bands from `start` to `end`: a gradient with no smooth blending.
    static func bands(_ start: Color, _ end: Color, count: Int = 4) -> Gradient {
        let count = max(1, count)
        var stops: [Gradient.Stop] = []
        for band in 0..<count {
            let color = mix(start, end, count == 1 ? 0 : Double(band) / Double(count - 1))
            stops.append(.init(color: color, location: Double(band) / Double(count)))
            stops.append(.init(color: color, location: Double(band + 1) / Double(count)))
        }
        return Gradient(stops: stops)
    }

    static func mix(_ a: Color, _ b: Color, _ fraction: Double) -> Color {
        guard let from = NSColor(a).usingColorSpace(.sRGB), let to = NSColor(b).usingColorSpace(.sRGB),
              let mixed = from.blended(withFraction: fraction, of: to) else { return fraction < 0.5 ? a : b }
        return Color(nsColor: mixed)
    }

    /// Stair offsets for a convex quarter circle of `steps` stairs: entry `i` is how far row `i`
    /// (counted from the outer edge) is inset, in stairs. Row 0 is always fully inset, so even a
    /// one-stair corner cuts its pixel.
    static func convex(_ steps: Int) -> [Int] {
        (0..<max(0, steps)).map { row in
            let n = Double(steps), d = n - Double(row)
            return Int((n - (n * n - d * d).squareRoot() - 0.2).rounded(.up))
        }
    }
}

extension Font {
    /// Departure Mono on its 11-point pixel grid. `scale` 2 doubles every font pixel.
    static func pixel(_ scale: Int = 1) -> Font { .custom(PixelArt.fontName, fixedSize: 11 * CGFloat(scale)) }
}

/// Rectangle with stepped corners: the pixel stand-in for rounded rectangles, capsules, and circles.
/// Each corner has its own number of stairs; 0 keeps it square.
struct PixelBox: InsettableShape {
    var topLeading: Int
    var topTrailing: Int
    var bottomLeading: Int
    var bottomTrailing: Int
    var step: CGFloat = PixelArt.step
    var inset: CGFloat = 0

    init(_ steps: Int = 1, step: CGFloat = PixelArt.step) {
        self.init(topLeading: steps, topTrailing: steps, bottomLeading: steps, bottomTrailing: steps, step: step)
    }

    init(topLeading: Int, topTrailing: Int, bottomLeading: Int, bottomTrailing: Int, step: CGFloat = PixelArt.step) {
        self.topLeading = topLeading
        self.topTrailing = topTrailing
        self.bottomLeading = bottomLeading
        self.bottomTrailing = bottomTrailing
        self.step = step
    }

    func path(in rect: CGRect) -> Path {
        let r = rect.insetBy(dx: inset, dy: inset)
        guard r.width > 0, r.height > 0 else { return Path() }
        let limit = Int(min(r.width, r.height) / (2 * step))
        let s = step
        var points: [CGPoint] = []
        // Top-trailing, walking down.
        let tt = PixelArt.convex(min(topTrailing, limit))
        if tt.isEmpty { points.append(CGPoint(x: r.maxX, y: r.minY)) }
        for (row, cut) in tt.enumerated() {
            points.append(CGPoint(x: r.maxX - CGFloat(cut) * s, y: r.minY + CGFloat(row) * s))
            points.append(CGPoint(x: r.maxX - CGFloat(cut) * s, y: r.minY + CGFloat(row + 1) * s))
        }
        // Bottom-trailing, walking down.
        let bt = PixelArt.convex(min(bottomTrailing, limit))
        if bt.isEmpty { points.append(CGPoint(x: r.maxX, y: r.maxY)) }
        for (row, cut) in bt.enumerated().reversed() {
            points.append(CGPoint(x: r.maxX - CGFloat(cut) * s, y: r.maxY - CGFloat(row + 1) * s))
            points.append(CGPoint(x: r.maxX - CGFloat(cut) * s, y: r.maxY - CGFloat(row) * s))
        }
        // Bottom-leading, walking up.
        let bl = PixelArt.convex(min(bottomLeading, limit))
        if bl.isEmpty { points.append(CGPoint(x: r.minX, y: r.maxY)) }
        for (row, cut) in bl.enumerated() {
            points.append(CGPoint(x: r.minX + CGFloat(cut) * s, y: r.maxY - CGFloat(row) * s))
            points.append(CGPoint(x: r.minX + CGFloat(cut) * s, y: r.maxY - CGFloat(row + 1) * s))
        }
        // Top-leading, walking up.
        let tl = PixelArt.convex(min(topLeading, limit))
        if tl.isEmpty { points.append(CGPoint(x: r.minX, y: r.minY)) }
        for (row, cut) in tl.enumerated().reversed() {
            points.append(CGPoint(x: r.minX + CGFloat(cut) * s, y: r.minY + CGFloat(row + 1) * s))
            points.append(CGPoint(x: r.minX + CGFloat(cut) * s, y: r.minY + CGFloat(row) * s))
        }
        var path = Path()
        path.addLines(points)
        path.closeSubpath()
        return path
    }

    func inset(by amount: CGFloat) -> PixelBox {
        var copy = self
        copy.inset += amount
        return copy
    }
}

/// One-point-per-pixel icons on a 9 × 9 grid, drawn in the current foreground style.
struct PixelIcon: View {
    enum Glyph: CaseIterable {
        case refresh, chevronUp, chevronLeft, power, compose, pin, send, copy, check, warning, chat, bolt
    }

    let glyph: Glyph
    /// Points per icon pixel.
    var scale: CGFloat = 1

    var body: some View {
        Canvas { context, _ in
            let path = Self.paths[glyph] ?? Path()
            context.fill(path.applying(CGAffineTransform(scaleX: scale, y: scale)), with: .foreground)
        }
        .frame(width: 9 * scale, height: 9 * scale)
        .accessibilityHidden(true)
    }

    private static let paths: [Glyph: Path] = Dictionary(uniqueKeysWithValues: Glyph.allCases.map { glyph in
        var path = Path()
        for (y, row) in rows(glyph).enumerated() {
            for (x, cell) in row.enumerated() where cell == "x" {
                path.addRect(CGRect(x: x, y: y, width: 1, height: 1))
            }
        }
        return (glyph, path)
    })

    private static func rows(_ glyph: Glyph) -> [String] {
        switch glyph {
        case .refresh:
            return ["..xxxx.x.", ".x....xx.", "x....xxx.", "x........", "x.......x", "x.......x", "x.......x", ".x.....x.", "..xxxxx.."]
        case .chevronUp:
            return [".........", ".........", "....x....", "...xxx...", "..xx.xx..", ".xx...xx.", ".x.....x.", ".........", "........."]
        case .chevronLeft:
            return [".........", ".....x...", "....xx...", "...xx....", "..xx.....", "...xx....", "....xx...", ".....x...", "........."]
        case .power:
            return ["....x....", ".x..x..x.", "x...x...x", "x...x...x", "x.......x", "x.......x", "x.......x", ".x.....x.", "..xxxxx.."]
        case .compose:
            return ["......xx.", ".....x.xx", "....x.xx.", "...x.xx..", "..x.xx...", ".x.xx....", ".xxx.....", "xxx......", "........."]
        case .pin:
            return ["..xxxxx..", "...xxx...", "...xxx...", "..xxxxx..", ".xxxxxxx.", "....x....", "....x....", "....x....", "........."]
        case .send:
            return ["x........", "xxx......", "x.xxx....", "x...xxx..", "x.....xxx", "x...xxx..", "x.xxx....", "xxx......", "x........"]
        case .copy:
            return ["...xxxxxx", "...x....x", "xxxxxx..x", "x....x..x", "x....x..x", "x....xxxx", "x....x...", "x....x...", "xxxxxx..."]
        case .check:
            return [".........", "........x", ".......xx", "......xx.", "x....xx..", "xx..xx...", ".xxxx....", "..xx.....", "........."]
        case .warning:
            return ["....x....", "...xxx...", "...x.x...", "..xx.xx..", "..xx.xx..", ".xxxxxxx.", ".xxx.xxx.", "xxxxxxxxx", "........."]
        case .chat:
            return [".xxxxxxx.", "x.......x", "x.x.x.x.x", "x.......x", ".xxxxxxx.", "..x......", ".x.......", "x........", "........."]
        case .bolt:
            return [".....xx..", "....xx...", "...xx....", "..xxxxx..", "....xx...", "...xx....", "..xx.....", ".xx......", "........."]
        }
    }
}

/// Pixel push button: a stepped box with a 1-point border in `tint` that brightens while pressed.
struct PixelButtonStyle: ButtonStyle {
    var tint: Color = .white
    @Environment(\.isEnabled) private var isEnabled

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.pixel())
            .foregroundStyle(tint == .white ? Color.white.opacity(isEnabled ? 0.9 : 0.4) : tint.opacity(isEnabled ? 1 : 0.4))
            .padding(.horizontal, 8)
            .frame(height: 20)
            .background(PixelBox(1).fill(tint.opacity(configuration.isPressed ? 0.3 : 0.1)))
            .overlay(PixelBox(1).strokeBorder(tint.opacity(isEnabled ? 0.5 : 0.2), lineWidth: 1))
            .contentShape(PixelBox(1))
    }
}

/// Pixel switch: a stepped track with a square knob that jumps between ends.
struct PixelToggleStyle: ToggleStyle {
    var tint: Color

    func makeBody(configuration: Configuration) -> some View {
        Button { configuration.isOn.toggle() } label: {
            HStack(spacing: 6) {
                configuration.label
                ZStack(alignment: configuration.isOn ? .trailing : .leading) {
                    PixelBox(1).fill(configuration.isOn ? tint.opacity(0.45) : Color.white.opacity(0.12))
                    PixelBox(1).strokeBorder(configuration.isOn ? tint : Color.white.opacity(0.35), lineWidth: 1)
                    Rectangle()
                        .fill(configuration.isOn ? tint : Color.white.opacity(0.6))
                        .frame(width: 6, height: 6)
                        .padding(.horizontal, 2)
                }
                .frame(width: 20, height: 10)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityValue(configuration.isOn ? "on" : "off")
    }
}

/// Dithered glow of whole cells around a center: solid near the middle, checkered further out.
struct PixelHalo: View {
    let color: Color
    /// 0 hides the halo; 1 is the brightest.
    let level: Double
    /// Radius in cells.
    let radius: Int
    var cell: CGFloat = 2

    var body: some View {
        Canvas { context, size in
            guard level > 0 else { return }
            let cx = (size.width / cell / 2).rounded(.down), cy = (size.height / cell / 2).rounded(.down)
            let r = Double(radius)
            for dy in -radius...radius {
                for dx in -radius...radius {
                    let distance = (Double(dx * dx + dy * dy)).squareRoot() / r
                    guard distance <= 1 else { continue }
                    let ring = distance < 0.45 ? 0 : distance < 0.75 ? 1 : 2
                    if ring == 1 && (dx + dy) % 2 != 0 { continue }
                    if ring == 2 && ((dx + dy) % 2 != 0 || dy % 2 != 0) { continue }
                    let alpha = [0.5, 0.35, 0.25][ring] * level
                    let rect = CGRect(x: (cx + CGFloat(dx)) * cell, y: (cy + CGFloat(dy)) * cell, width: cell, height: cell)
                    context.fill(Path(rect), with: .color(color.opacity(alpha)))
                }
            }
        }
        .allowsHitTesting(false)
        .accessibilityHidden(true)
    }
}

/// Agent avatar: a one-color 10 × 7 pixel invader in the agent's identity color, with paired
/// antennae, hanging arms, inward feet, and cutout eyes and mouth. Working invaders blink or
/// wiggle their antennae; minimized wings add a continuous pixel bob. Waiting agents glance
/// aside, idle agents close their eyes and dim, and closed panes show × eyes. Fresh messages
/// make the sender hop and flare its antennae and the recipient widen its eyes. Reduce Motion
/// stops animation and message poses. Each sprite pixel is a whole number of points.
struct AgentAvatar: View {
    let hue: Color
    let mood: AgentMood
    var speaking = false
    var listening = false
    var size: CGFloat = 20
    var continuousWorking = false
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @State private var blinking = false
    @State private var flipped = false

    private enum WorkingAnimation: Hashable { case none, occasional, continuous }

    private var workingAnimation: WorkingAnimation {
        guard mood == .working && !reduceMotion else { return .none }
        return continuousWorking ? .continuous : .occasional
    }

    var body: some View {
        let animation = workingAnimation
        let unit = max(1, (size / 10).rounded(.down))
        Canvas { context, canvas in
            let origin = CGPoint(x: ((canvas.width - 10 * unit) / 2).rounded(.down),
                                 y: ((canvas.height - 7 * unit) / 2).rounded(.down) - (speaking && !reduceMotion ? unit : 0))
            var path = Path()
            for (x, y) in cells() {
                path.addRect(CGRect(x: origin.x + CGFloat(x) * unit, y: origin.y + CGFloat(y) * unit, width: unit, height: unit))
            }
            context.fill(path, with: .color(color))
        }
        .frame(width: size, height: size)
        // Move the whole canvas so the antennae and feet stay intact at the smallest wing size.
        // Fresh-message poses take priority over the status bob.
        .offset(y: animation == .continuous && flipped && !speaking && !listening ? unit : 0)
        .task(id: animation) {
            blinking = false
            flipped = false
            guard animation != .none else { return }
            if animation == .continuous {
                while !Task.isCancelled {
                    do { try await Task.sleep(for: .milliseconds(450)) }
                    catch { return }
                    if Task.isCancelled { return }
                    flipped.toggle()
                }
                return
            }
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(Double.random(in: 2.4...5.0)))
                if Task.isCancelled { break }
                if Bool.random() {
                    blinking = true
                    try? await Task.sleep(for: .milliseconds(120))
                    blinking = false
                } else {
                    for flip in [true, false, true, false] {
                        flipped = flip
                        try? await Task.sleep(for: .milliseconds(130))
                    }
                }
            }
            blinking = false
            flipped = false
        }
        .accessibilityHidden(true)
    }

    private var awake: Bool { mood == .working || mood == .waiting || (!reduceMotion && (speaking || listening)) }

    private var color: Color {
        let base = mood == .offline ? Color.orange : hue
        return base.opacity(mood == .closed ? 0.35 : awake || mood == .offline ? 1 : 0.5)
    }

    private typealias Cell = (Int, Int)

    /// Body rows 1–6; eyes are cut out below, and row 0 holds the movable antenna tips.
    private static let bodyCells: [Cell] = {
        let rows = ["...x..x...", "..xxxxxx..", ".xxxxxxxx.", "xxxxxxxxxx", "x.x....x.x", "...x..x..."]
        return rows.enumerated().flatMap { y, row in
            row.enumerated().compactMap { x, c in c == "x" ? (x, y + 1) : nil }
        }
    }()

    private func cells() -> [Cell] {
        let holes = eyes()
        var filled = Self.bodyCells.filter { cell in !holes.contains { $0 == cell } }
        let reacting = speaking && !reduceMotion
        let spread = mood != .closed && ((mood == .waiting && !reacting) != (flipped && !reduceMotion))
        filled += spread ? [(1, 0), (8, 0)] : [(2, 0), (7, 0)]
        if reacting { filled += [(1, 0), (8, 0)] }
        return filled
    }

    /// Square eyes match the reference; listening widens them, idle closes them, closed uses ×.
    private func eyes() -> [Cell] {
        if mood == .closed {
            return [(2, 2), (4, 2), (3, 3), (2, 4), (4, 4), (5, 2), (7, 2), (6, 3), (5, 4), (7, 4)]
        }
        let reacting = !reduceMotion && (speaking || listening)
        let shift = mood == .waiting && !reacting ? 1 : 0
        if listening && !reduceMotion { return [(3, 2), (3, 3), (6, 2), (6, 3)] }
        if (!awake && mood != .offline) || (blinking && !reduceMotion) {
            return [(2, 3), (3, 3), (6, 3), (7, 3)]
        }
        return [(3 + shift, 3), (6 + shift, 3)]
    }
}
