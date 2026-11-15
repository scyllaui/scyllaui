// 开屏的像素字与像素按钮（docs/design.md 10.4 节决策 9，2026-10-30 用户定稿）：副标题、「开始使用 →」与页脚
// 用 Fusion Pixel Font 12px 的点阵逐格画矩形，任何缩放下边缘都是整格，与字标同一风格。
// 点阵来自 `pixel-text.json`（由 src/scripts/generate-pixel-text.swift 生成），App 不带字体文件、也不注册字体。

import AppKit

struct PixelText {
    /// 像素字的灰度：白 / 黑按 60% 叠在底色上（2026-10-30 用户定稿），开屏副标题、深色模式的按钮字与侧栏副标题共用。
    static let inkFraction: CGFloat = 0.6

    /// 基线以上、以下的字体像素数。
    let ascent: CGFloat, descent: CGFloat
    private let glyphs: [Character: (advance: CGFloat, cells: [CGPoint])]

    static let shared: PixelText? = Bundle.main.url(forResource: "pixel-text", withExtension: "json")
        .flatMap { try? Data(contentsOf: $0) }.flatMap(PixelText.init(json:))

    init?(json data: Data) {
        guard let d = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
              let ascent = d["ascent"] as? Double, let descent = d["descent"] as? Double,
              let glyphs = d["glyphs"] as? [String: [Any]] else { return nil }
        self.ascent = CGFloat(ascent)
        self.descent = CGFloat(descent)
        var out: [Character: (CGFloat, [CGPoint])] = [:]
        for (k, g) in glyphs {
            guard let ch = k.first, g.count == 2, let advance = g[0] as? Double, let flat = g[1] as? [Double] else { return nil }
            out[ch] = (CGFloat(advance), stride(from: 0, to: flat.count - 1, by: 2).map { CGPoint(x: flat[$0], y: flat[$0 + 1]) })
        }
        self.glyphs = out
    }

    var lineHeight: CGFloat { ascent + descent }

    /// 一行字的格子（字体像素，x 自左缘、y 自基线向上）与总宽。kern 只加在字与字之间；bold 按点阵字的老做法
    /// 再向右错开一格画一遍，宽度多一格。点阵里没有的字按空格处理。
    func layout(_ s: String, kern: CGFloat = 0, bold: Bool = false) -> (cells: [CGPoint], width: CGFloat) {
        var pen: CGFloat = 0, cells: [CGPoint] = []
        for (i, ch) in s.enumerated() {
            if i > 0 { pen += kern }
            guard let g = glyphs[ch] ?? glyphs[" "] else { continue }
            for c in g.cells {
                cells.append(CGPoint(x: pen + c.x, y: c.y))
                if bold { cells.append(CGPoint(x: pen + c.x + 1, y: c.y)) }
            }
            pen += g.advance
        }
        return (cells, pen + (bold ? 1 : 0))
    }

    /// 以 origin 为左缘与基线、每格边长 px 画出。所有格子一次填充，加粗重叠处在半透明时也不会加深。
    func draw(_ s: String, kern: CGFloat = 0, bold: Bool = false, at origin: CGPoint, px: CGFloat, color: NSColor, in ctx: CGContext) {
        ctx.setFillColor(color.cgColor)
        ctx.fill(layout(s, kern: kern, bold: bold).cells.map {
            CGRect(x: origin.x + $0.x * px, y: origin.y + $0.y * px, width: px, height: px)
        })
    }
}

/// 实心像素按钮：字标色底、加粗像素字（颜色由使用方定），硬边阴影向右下错开 4 格；按下时按钮面落进阴影 2 格。
/// frame 右边和下边多出 4 格放阴影。保留 NSButton 的标题、回车键与辅助功能，只换掉绘制。
final class PixelButton: NSButton {
    static let shadowCells: CGFloat = 4
    /// 每格边长（pt）。
    var px: CGFloat = 1 { didSet { needsDisplay = true } }
    var fill = NSColor.controlAccentColor { didSet { needsDisplay = true } }
    var shadowColor = NSColor.clear { didSet { needsDisplay = true } }
    var titleColor = NSColor.white { didSet { needsDisplay = true } }

    override var isFlipped: Bool { false }

    /// 按钮面：字宽左右各留 14 格、上下各 4 格，宽度不小于 minWidth（pt），不含阴影。
    func faceSize(minWidth: CGFloat) -> NSSize {
        guard let text = PixelText.shared else { return NSSize(width: minWidth, height: 24 * px) }
        return NSSize(width: max((text.layout(title, kern: 1, bold: true).width + 28) * px, minWidth), height: (text.lineHeight + 8) * px)
    }

    override func draw(_ dirtyRect: NSRect) {
        guard let ctx = NSGraphicsContext.current?.cgContext else { return }
        let off = Self.shadowCells * px
        let face = CGRect(x: 0, y: off, width: bounds.width - off, height: bounds.height - off)
        ctx.setFillColor(shadowColor.cgColor)
        ctx.fill(face.offsetBy(dx: off, dy: -off))
        let press: CGFloat = isHighlighted ? 2 * px : 0
        let f = face.offsetBy(dx: press, dy: -press)
        ctx.setFillColor(fill.cgColor)
        ctx.fill(f)
        guard let text = PixelText.shared else { return }
        let w = text.layout(title, kern: 1, bold: true).width * px
        text.draw(title, kern: 1, bold: true, at: CGPoint(x: f.midX - w / 2, y: f.midY - text.lineHeight * px / 2 + text.descent * px),
                  px: px, color: titleColor, in: ctx)
    }
}

/// 侧栏品牌区字标下的一行像素字：「▪ v版本 · 副标题」（2026-10-30 用户要求版本号放在副标题左侧、不另起一行）。
/// 写法与开屏一致：方块为字标色，版本号为次要色、字距 0.5 格，副标题加粗、字距 1.5 格、白 / 黑 60%。
/// 每格 1 pt，2 倍屏上正好 2 个像素，边缘清晰；颜色随侧栏外观。
final class PixelBrandLine: NSView {
    private let version: String, subtitle: String

    init(version: String, subtitle: String) {
        self.version = "v" + version + " · "
        self.subtitle = subtitle
        super.init(frame: .zero)
        setAccessibilityElement(true)
        setAccessibilityRole(.staticText)
        setAccessibilityLabel("v" + version + " · " + subtitle)
    }

    required init?(coder: NSCoder) { fatalError() }

    override var isFlipped: Bool { false }

    private var widths: (version: CGFloat, subtitle: CGFloat) {
        guard let text = PixelText.shared else { return (0, 0) }
        return (text.layout(version, kern: 0.5).width + 0.5, text.layout(subtitle, kern: 1.5, bold: true).width)
    }

    override var intrinsicContentSize: NSSize {
        let w = widths
        return NSSize(width: 8 + w.version + w.subtitle, height: PixelText.shared?.lineHeight ?? 16)
    }

    override func draw(_ dirtyRect: NSRect) {
        guard let text = PixelText.shared, let ctx = NSGraphicsContext.current?.cgContext else { return }
        let dark = effectiveAppearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
        // 与侧栏字标资源同色：浅色蓝、深色橙。
        let ink = dark ? NSColor(srgbRed: 0xD9 / 255.0, green: 0x77 / 255.0, blue: 0x57 / 255.0, alpha: 1)
                       : NSColor(srgbRed: 0x23 / 255.0, green: 0x7B / 255.0, blue: 0xC3 / 255.0, alpha: 1)
        let x0 = (bounds.width - intrinsicContentSize.width) / 2
        ctx.setFillColor(ink.cgColor)
        ctx.fill(CGRect(x: x0, y: (text.lineHeight - 4) / 2, width: 4, height: 4))
        effectiveAppearance.performAsCurrentDrawingAppearance {
            text.draw(version, kern: 0.5, at: CGPoint(x: x0 + 8, y: text.descent), px: 1, color: .secondaryLabelColor, in: ctx)
            text.draw(subtitle, kern: 1.5, bold: true, at: CGPoint(x: x0 + 8 + widths.version, y: text.descent), px: 1,
                      color: NSColor(white: dark ? 1 : 0, alpha: PixelText.inkFraction), in: ctx)
        }
    }
}
