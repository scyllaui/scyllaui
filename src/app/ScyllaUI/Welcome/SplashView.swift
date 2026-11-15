// 开屏页（M4a 块 4，docs/design.md 10.4 节决策 9）：上方 Scylla 六个头逐帧扭动，下方像素字标「逐格拼合 + 扫描线」，
// 像素字副标题、像素按钮「开始使用 →」与页脚（版本号与版权），共 5 s。由 displayLink 驱动，每帧按纯函数时间线画出；点击、Esc、空格、回车跳到终帧；
// 不跟随系统的「减少动态效果」（2026-10-24 用户决定）。整幅画面按 1100 × 720 的设计尺寸等比缩放、居中。

import AppKit
import QuartzCore

final class SplashView: NSView {
    /// 副标题（2026-10-30 用户改定）：品牌字标的一部分，中英文界面都用英文原句，不进本地化；侧栏品牌区也用它。
    static let subtitle = "Keep agents in sight"
    /// 页脚（2026-10-30 用户定稿，取代署名）：「▪ v版本 · 版权」居中一行。版权取 Info.plist 的 NSHumanReadableCopyright，
    /// 与「关于」面板同一句。
    private static let footer = "v" + (Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "")
        + " · " + (Bundle.main.object(forInfoDictionaryKey: "NSHumanReadableCopyright") as? String ?? "")
    /// 设计尺寸与其中的排版（块 0 原型第二版的定稿）：图标边长 340 pt、距顶 70 pt；字号与间距按 1.5 倍，字标 2.4 倍。
    private static let design = CGSize(width: 1100, height: 720)
    private static let ui: CGFloat = 1.5, wordScale: CGFloat = 2.4
    /// 刚弹出的这段时间不响应按键（块 0 中用户正在打字的空格被当成「跳过」）。
    private static let keyGrace: CFTimeInterval = 0.6

    private struct Palette {
        let background, ink, shadow, line: NSColor
        let shadowAlpha, tailAlpha: CGFloat
        init(background: String, _ c: SplashWordmark.Colors) {
            self.background = Self.hex(background)
            ink = Self.hex(c.ink)
            shadow = Self.hex(c.shadow)
            line = ink.blended(withFraction: 0.45, of: .white) ?? ink
            shadowAlpha = CGFloat(c.shadowAlpha)
            tailAlpha = CGFloat(c.tailAlpha)
        }
        static func hex(_ s: String) -> NSColor {
            let n = UInt32(s, radix: 16) ?? 0
            return NSColor(srgbRed: CGFloat((n >> 16) & 255) / 255, green: CGFloat((n >> 8) & 255) / 255, blue: CGFloat(n & 255) / 255, alpha: 1)
        }
    }

    private let art: SplashArt?
    /// 浅色开屏用的一份：2 pt 黑色轮廓减到 1 pt，减轻色重（2026-11-06 用户选定）；深色仍用原素材。
    private let lightArt: SplashArt?
    private let wordmark: SplashWordmark?
    private let glyph: CGPath?
    private(set) var t: Double = 0
    private(set) var finished = false
    private var link: CADisplayLink?
    private var playStart: CFTimeInterval?
    /// 进入窗口的时刻，按键的宽限从这里算（播放要等主窗口接回终端之后才开始）。
    private var shownAt: CFTimeInterval?
    private var frameTimes: [CFTimeInterval] = []
    let button = PixelButton(title: String(localized: "开始使用 →"), target: nil, action: nil)
    var onStart: (() -> Void)?

    override init(frame: NSRect) {
        art = SplashArt()
        lightArt = SplashArt(outlineKeep: 3)
        wordmark = Bundle.main.url(forResource: "wordmark-cells", withExtension: "json")
            .flatMap { try? Data(contentsOf: $0) }.flatMap(SplashWordmark.init(json:))
        glyph = wordmark?.path
        super.init(frame: frame)
        if art == nil || lightArt == nil || wordmark == nil { scyllauiLog.error("开屏素材读取失败 art=\(self.art != nil) light=\(self.lightArt != nil) wordmark=\(self.wordmark != nil)") }
        button.focusRingType = .none
        button.keyEquivalent = "\r"
        button.target = self
        button.action = #selector(start)
        addSubview(button)
        setTime(0)
    }

    required init?(coder: NSCoder) { fatalError() }

    override var acceptsFirstResponder: Bool { true }
    override var isFlipped: Bool { false }

    private var endTime: Double { SplashTimeline.total }

    /// 设计尺寸到视图的缩放与偏移。
    private var placement: (scale: CGFloat, offset: CGPoint) {
        let s = min(1.4, max(0.6, min(bounds.width / Self.design.width, bounds.height / Self.design.height)))
        return (s, CGPoint(x: (bounds.width - Self.design.width * s) / 2, y: (bounds.height - Self.design.height * s) / 2))
    }

    /// 像素字每格的边长（设计单位）：字标一格的 1/2（副标题）与 1/3（按钮、页脚）。
    private var subPx: CGFloat { (wordmark?.cell ?? 1.625 * 1.18) * Self.wordScale / 2 }
    private var metaPx: CGFloat { (wordmark?.cell ?? 1.625 * 1.18) * Self.wordScale / 3 }

    /// 副标题顶边：字标下方 26（设计单位）。
    private func subtitleTop(_ w: SplashWordmark?) -> CGFloat { (w.map { glyphBottom($0) } ?? tileRect.minY) - 26 }

    private var tileRect: CGRect {
        CGRect(x: Self.design.width / 2 - 170, y: Self.design.height - 70 - 340, width: 340, height: 340)
    }

    /// 字标空间原点在设计尺寸里的位置：可见范围水平居中，顶边在图标下方 18 × 1.5 pt。
    private func glyphOrigin(_ w: SplashWordmark) -> CGPoint {
        CGPoint(x: Self.design.width / 2 - w.visualBox.midX * Self.wordScale,
                y: tileRect.minY - 18 * Self.ui - w.visualBox.maxY * Self.wordScale)
    }

    private func glyphBottom(_ w: SplashWordmark) -> CGFloat { glyphOrigin(w).y + w.visualBox.minY * Self.wordScale }

    override func layout() {
        super.layout()
        setTime(t)
    }

    /// 设定时间：按钮的状态与位置在绘制之前定好（在 draw 里改子视图会落后一帧）。
    private func setTime(_ time: Double) {
        t = time
        let a = SplashTimeline.buttonAlpha(SplashTimeline.base(time) - SplashTimeline.wordOffset)
        button.alphaValue = a
        button.isEnabled = a > 0.5
        let (s, off) = placement
        let (dark, w) = (effectiveAppearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua, wordmark)
        if let w {
            let p = palette(dark: dark, w)
            button.fill = p.ink
            button.shadowColor = p.shadow.withAlphaComponent(p.shadowAlpha)
            // 按钮字：深色为白色按 60% 叠在橙底上；浅色为纯白，蓝底上降灰度会看不清（2026-10-30 用户定）。
            button.titleColor = dark ? p.ink.blended(withFraction: PixelText.inkFraction, of: .white) ?? .white : .white
        }
        // 副标题一行之下 26（设计单位）是按钮面的顶边；frame 另含右下的阴影。
        button.px = metaPx * s
        let face = button.faceSize(minWidth: 150 * Self.ui * s)
        let shadow = PixelButton.shadowCells * button.px
        let faceTop = subtitleTop(w) - (PixelText.shared?.lineHeight ?? 16) * subPx - 26
        button.frame = NSRect(x: bounds.midX - face.width / 2, y: off.y + faceTop * s - face.height - shadow,
                              width: face.width + shadow, height: face.height + shadow)
        needsDisplay = true
    }

    private func palette(dark: Bool, _ w: SplashWordmark) -> Palette {
        dark ? Palette(background: "1E1F22", w.dark) : Palette(background: "ECECEE", w.light)
    }

    /// 切换深浅外观时按钮颜色跟着换。
    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        setTime(t)
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        if window != nil { shownAt = CACurrentMediaTime() }
    }

    /// 播放前先离屏画一帧：缩小后的图层在第一次绘制时才生成（解码与缩小），放到第一帧里会卡。
    func warmUp() {
        guard let rep = bitmapImageRepForCachingDisplay(in: bounds) else { return }
        cacheDisplay(in: bounds, to: rep)
    }

    /// 开始播放；等待期间已被跳过（到了终帧）的不再播放。
    func play() {
        guard !finished else { return }
        playStart = nil
        frameTimes = []
        setTime(0)
        link?.invalidate()
        let l = displayLink(target: self, selector: #selector(tick(_:)))
        l.preferredFrameRateRange = CAFrameRateRange(minimum: 60, maximum: 120, preferred: 120)
        l.add(to: .main, forMode: .common)
        link = l
    }

    @objc private func tick(_ l: CADisplayLink) {
        if playStart == nil { playStart = l.targetTimestamp }
        frameTimes.append(l.targetTimestamp)
        let now = l.targetTimestamp - (playStart ?? l.targetTimestamp)
        if now >= endTime { finish(skipped: false) } else { setTime(now) }
    }

    /// 直接到终帧（点击、Esc、空格、回车）。
    func skip(_ reason: String) {
        guard !finished else { return }
        scyllauiLog.notice("splash skip reason=\(reason, privacy: .public) t=\(String(format: "%.2f", self.t), privacy: .public)")
        finish(skipped: true)
    }

    private func finish(skipped: Bool) {
        finished = true
        link?.invalidate()
        link = nil
        setTime(endTime)
        let gaps = zip(frameTimes.dropFirst(), frameTimes).map { $0 - $1 }
        let total = (frameTimes.last ?? 0) - (frameTimes.first ?? 0)
        // 最大的三个帧间隔与它们开始时的播放时间，供核对卡顿出在哪一段。
        let worst = gaps.indices.sorted { gaps[$0] > gaps[$1] }.prefix(3)
            .map { String(format: "%.1f@%.2f", gaps[$0] * 1000, frameTimes[$0] - frameTimes[0]) }.joined(separator: ",")
        scyllauiLog.notice("splash done total_ms=\(Int(total * 1000)) max_gap_ms=\(String(format: "%.1f", (gaps.max() ?? 0) * 1000), privacy: .public) worst=\(worst, privacy: .public) frames=\(self.frameTimes.count) skipped=\(skipped)")
    }

    @objc private func start() { onStart?() }

    override func mouseDown(with event: NSEvent) {
        if !finished { skip("click") }
    }

    override func keyDown(with event: NSEvent) {
        if let s = shownAt, CACurrentMediaTime() - s < Self.keyGrace {
            scyllauiLog.notice("splash key ignored keyCode=\(event.keyCode)")
            return
        }
        switch event.charactersIgnoringModifiers {
        case "\u{1b}", " ", "\r":
            // 播完后回车 =「开始使用 →」：按键先到这里，不会再走到默认按钮。
            if !finished { skip("key \(event.keyCode)") } else if event.charactersIgnoringModifiers == "\r" { button.performClick(nil) }
        default: super.keyDown(with: event)
        }
    }

    override func cancelOperation(_ sender: Any?) {
        if !finished { skip("cancel") }
    }

    override func draw(_ dirtyRect: NSRect) {
        guard let ctx = NSGraphicsContext.current?.cgContext else { return }
        let dark = effectiveAppearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
        guard let w = wordmark else {
            NSColor.windowBackgroundColor.setFill()
            bounds.fill()
            return
        }
        let p = palette(dark: dark, w)
        p.background.setFill()
        bounds.fill()
        let tb = SplashTimeline.base(t)
        let (s, off) = placement
        ctx.saveGState()
        ctx.translateBy(x: off.x, y: off.y)
        ctx.scaleBy(x: s, y: s)
        if let art = dark ? art : lightArt ?? art {
            ctx.saveGState()
            ctx.setAlpha(SplashTimeline.tileAlpha(tb))
            art.draw(in: tileRect, tb: tb, ctx: ctx)
            ctx.restoreGState()
        }
        let tw = tb - SplashTimeline.wordOffset
        let origin = glyphOrigin(w)
        ctx.saveGState()
        ctx.translateBy(x: origin.x, y: origin.y)
        ctx.scaleBy(x: Self.wordScale, y: Self.wordScale)
        let shadowA = SplashTimeline.shadowAlpha(tw)
        if shadowA > 0 {
            ctx.setFillColor(p.shadow.withAlphaComponent(p.shadowAlpha * shadowA).cgColor)
            ctx.fill(w.shadowRects)
            ctx.setFillColor(p.shadow.withAlphaComponent(p.tailAlpha * shadowA).cgColor)
            ctx.fill(w.tailRects)
        }
        // 格子与矢量字标放在同一个透明层里交叉淡化：矢量用 plusLighter 叠加，重合处始终是满色，
        // 只在格子或只在矢量里的部分各自淡出、淡入（普通叠加在中途会整体变淡）。
        ctx.beginTransparencyLayer(auxiliaryInfo: nil)
        let cellsA = SplashTimeline.cellsAlpha(tw)
        if cellsA > 0 {
            // 同一步的格子并成一组再填充，相邻格之间不留缝。
            var levels: [[CGRect]] = [[], [], [], []]
            for c in w.cells {
                let k = SplashTimeline.cellStep(c, tw, w)
                guard k > 0 else { continue }
                let remain = CGFloat(3 - k) / 3
                let dx = (CGFloat(SplashTimeline.startOffsetCells) * remain).rounded() * w.cell
                let dy = -(CGFloat(c.dy) * remain).rounded() * w.cell
                levels[k].append(c.rect.offsetBy(dx: dx, dy: dy))
            }
            for k in 1...3 where !levels[k].isEmpty {
                ctx.setFillColor(p.ink.withAlphaComponent(SplashTimeline.stepOpacity[k] * cellsA).cgColor)
                ctx.fill(levels[k])
            }
        }
        let vectorA = SplashTimeline.vectorAlpha(tw)
        if vectorA > 0, let glyph {
            ctx.setBlendMode(.plusLighter)
            ctx.setFillColor(p.ink.withAlphaComponent(vectorA).cgColor)
            ctx.addPath(glyph)
            ctx.fillPath(using: .evenOdd)
            ctx.setBlendMode(.normal)
        }
        ctx.endTransparencyLayer()
        let lineA = SplashTimeline.lineAlpha(tw)
        if lineA > 0 {
            let x = SplashTimeline.lineX(tw, w)
            let top = w.visualBox.maxY + 3 * w.cell, bottom = w.visualBox.minY - 3 * w.cell
            // 尾迹：扫描线左侧 16 格内由淡到无。
            let trail = NSRect(x: x - 16 * w.cell, y: bottom, width: 16 * w.cell, height: top - bottom)
            NSGradient(starting: p.line.withAlphaComponent(0), ending: p.line.withAlphaComponent(0.22 * CGFloat(lineA)))?.draw(in: trail, angle: 0)
            ctx.setFillColor(p.line.withAlphaComponent(0.95 * CGFloat(lineA)).cgColor)
            ctx.fill(CGRect(x: x - w.cell, y: bottom, width: 2 * w.cell, height: top - bottom))
        }
        ctx.restoreGState()
        let (subA, rise) = SplashTimeline.subtitle(tw)
        if subA > 0, let text = PixelText.shared {
            // 副标题：白（深色）/ 黑（浅色）按 60% 叠在底色上，比纯白纯黑柔和（2026-10-30 用户选定）；字距 1.5 格，加粗。
            let subInk = p.background.blended(withFraction: PixelText.inkFraction, of: Palette.hex(dark ? "FFFFFF" : "000000")) ?? p.ink
            let subW = text.layout(Self.subtitle, kern: 1.5, bold: true).width * subPx
            text.draw(Self.subtitle, kern: 1.5, bold: true,
                      at: CGPoint(x: Self.design.width / 2 - subW / 2, y: subtitleTop(w) - text.ascent * subPx - rise),
                      px: subPx, color: subInk.withAlphaComponent(subA), in: ctx)
            // 页脚：距画面底 12（设计单位）居中一行，灰色；版本号前一个 4 × 4 格的字标色方块，与文字隔 4 格。
            let dot = 4 * metaPx, gap = 4 * metaPx, bottom: CGFloat = 12
            let footW = dot + gap + text.layout(Self.footer, kern: 0.5).width * metaPx
            let x0 = Self.design.width / 2 - footW / 2
            ctx.setFillColor(p.ink.withAlphaComponent(subA).cgColor)
            ctx.fill(CGRect(x: x0, y: bottom + (text.lineHeight * metaPx - dot) / 2, width: dot, height: dot))
            text.draw(Self.footer, kern: 0.5, at: CGPoint(x: x0 + dot + gap, y: bottom + text.descent * metaPx), px: metaPx,
                      color: Palette.hex(dark ? "8A8B90" : "7A7B80").withAlphaComponent(subA), in: ctx)
        }
        ctx.restoreGState()
    }
}
