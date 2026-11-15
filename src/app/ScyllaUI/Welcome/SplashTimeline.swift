// 开屏动画的时间线与字标数据（M4a 块 4，docs/design.md 10.4 节决策 9）：纯函数，给定时刻算出一帧的各项参数，
// 实时播放与单测共用。参数取自块 0 原型的定稿（output/m4a-prototype/splash.swift）。只依赖 Foundation 与 CoreGraphics。

import CoreGraphics
import Foundation

/// 字标数据，来自 `wordmark-cells.json`（由 src/scripts/generate-wordmark.swift cells 生成）。字标空间 W × H，y 向上。
struct SplashWordmark {
    struct Cell {
        let px: Int, py: Int
        let rect: CGRect
        /// 落位前的纵向偏移（整格，-1、0、1）与开始时间的抖动，都由格子坐标固定算出。
        let dy: Int
        let jitter: Double
    }

    struct Colors {
        let ink, shadow: String
        let shadowAlpha, tailAlpha: Double
    }

    let size: CGSize
    /// 像素格边长。
    let cell: CGFloat
    let contours: [[CGPoint]]
    let cells: [Cell]
    let shadowRects: [CGRect]
    let tailRects: [CGRect]
    let light: Colors, dark: Colors
    /// 字形、阴影与拖尾的可见范围，用于排版。
    let visualBox: CGRect

    init?(json data: Data) {
        guard let d = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
              let size = d["size"] as? [Double], size.count == 2, let cell = d["cell"] as? Double,
              let contours = d["contours"] as? [[[Double]]], let cells = d["cells"] as? [[Double]],
              let offset = d["shadow_offset"] as? [Double], offset.count == 2, let tails = d["tails"] as? [[Double]],
              let colors = d["colors"] as? [String: [String: Any]] else { return nil }
        func palette(_ k: String) -> Colors? {
            guard let c = colors[k], let ink = c["ink"] as? String, let shadow = c["shadow"] as? String,
                  let sa = c["shadow_alpha"] as? Double, let ta = c["tail_alpha"] as? Double else { return nil }
            return Colors(ink: ink, shadow: shadow, shadowAlpha: sa, tailAlpha: ta)
        }
        guard let light = palette("light"), let dark = palette("dark") else { return nil }
        self.size = CGSize(width: size[0], height: size[1])
        self.cell = CGFloat(cell)
        self.contours = contours.map { $0.compactMap { $0.count == 2 ? CGPoint(x: $0[0], y: $0[1]) : nil } }
        self.cells = cells.compactMap { c in
            guard c.count == 4 else { return nil }
            let px = Int(c[0]), py = Int(c[1])
            return Cell(px: px, py: py, rect: CGRect(x: c[2], y: c[3], width: cell, height: cell),
                        dy: Int((Self.hash01(px, py, 7) * 3).rounded(.down)) - 1, jitter: -0.02 + 0.07 * Self.hash01(px, py, 11))
        }
        shadowRects = self.cells.map { $0.rect.offsetBy(dx: offset[0], dy: offset[1]) }
        tailRects = tails.compactMap { $0.count == 4 ? CGRect(x: $0[0], y: $0[1], width: $0[2], height: $0[3]) : nil }
        self.light = light
        self.dark = dark
        visualBox = (self.cells.map(\.rect) + shadowRects + tailRects).reduce(CGRect.null) { $0.union($1) }
    }

    /// 固定种子的伪随机（0—1），与原型相同。
    static func hash01(_ a: Int, _ b: Int, _ salt: UInt64) -> Double {
        var x = UInt64(bitPattern: Int64(a &* 73_856_093 ^ b &* 19_349_663)) ^ salt
        x ^= x >> 33; x &*= 0xff51afd7ed558ccd; x ^= x >> 33; x &*= 0xc4ceb9fe1a85ec53; x ^= x >> 33
        return Double(x % 10_000) / 10_000
    }

    /// 阶梯轮廓（even-odd 挖出 a 的内孔）。
    var path: CGPath {
        let p = CGMutablePath()
        for c in contours where !c.isEmpty {
            p.addLines(between: c)
            p.closeSubpath()
        }
        return p
    }
}

/// 一个头的摆动：主摆（f1）与小抖（f2）两个频率叠加（基准时间里的每秒周数），先往哪边摆（逆时针为正）、起动时间、
/// 小抖的相位与幅度倍数各不相同（2026-10-23 用户：碰撞问题不大，主要是要活起来）。
struct SplashHeadMotion {
    let sign: CGFloat
    let delay: Double
    let f1, f2, phase, amp: Double
}

/// 时间线。播放时间 `t`（秒，0—`total`）除以 `timeScale` 得到「基准时间」，下面的时刻都是基准时间；
/// 字标、副标题与按钮再推后 `wordOffset`。
enum SplashTimeline {
    /// 共 5 s（2026-10-23 用户：2 s → 4 s → 5 s）。
    static let total = 5.0
    static let timeScale = 2.5
    static let wordOffset = 0.5
    static let tileIn = 0.2
    static let headsStart = 0.2, wiggleDuration = 1.5
    /// 最大摆角 45°（用户：10° → 20° → 30° → 45°）；各头的幅度倍数不超过 1。
    static let headAmplitude: CGFloat = 45 * .pi / 180
    /// 头按 12 帧 / 秒逐帧切换姿态（用户并排对比后选定）。
    static let headFPS = 12.0
    static let heads: [SplashHeadMotion] = [
        SplashHeadMotion(sign: 1, delay: 0, f1: 1.9, f2: 3.7, phase: 0.3, amp: 1.0),
        SplashHeadMotion(sign: -1, delay: 0.08, f1: 2.2, f2: 3.1, phase: 1.1, amp: 0.9),
        SplashHeadMotion(sign: -1, delay: 0.15, f1: 1.7, f2: 4.0, phase: 2.0, amp: 1.0),
        SplashHeadMotion(sign: 1, delay: 0.04, f1: 2.0, f2: 3.4, phase: 0.7, amp: 1.0),
        SplashHeadMotion(sign: 1, delay: 0.2, f1: 2.4, f2: 3.9, phase: 1.6, amp: 0.85),
        SplashHeadMotion(sign: -1, delay: 0.12, f1: 1.8, f2: 3.2, phase: 2.4, amp: 0.95),
    ]
    static let lineStart = 0.08, lineEnd = 0.88
    /// 每格 3 步落位，每步 0.04 s。
    static let cellDuration = 0.12
    /// 格子 → 矢量字标。
    static let crossStart = 1.05, crossEnd = 1.15
    static let shadowSteps = (1.05, 1.13)
    static let subtitleStart = 1.15, subtitleEnd = 1.40
    static let buttonStart = 1.30, buttonEnd = 1.50
    /// 从右侧 3 格外落位。
    static let startOffsetCells = 3
    static let stepOpacity: [CGFloat] = [0, 0.35, 0.7, 1]

    static func base(_ t: Double) -> Double { t / timeScale }
    static func clamp(_ x: Double) -> Double { min(1, max(0, x)) }
    static func easeOut(_ x: Double) -> Double { 1 - pow(1 - clamp(x), 3) }
    static func lerp(_ t: Double, _ a: Double, _ b: Double) -> Double { clamp((t - a) / (b - a)) }

    /// 图标淡入（只变透明度）。
    static func tileAlpha(_ tb: Double) -> CGFloat { CGFloat(easeOut(lerp(tb, 0, tileIn))) }

    /// 头 `i` 在基准时间 `tb` 的摆角（弧度，逆时针为正）：两个正弦叠加，乘以包络（开头 0.12 s 渐强、最后 30% 渐弱到 0，
    /// 最终画面回到静止的图标）；时间按 1/12 s 取整。
    static func headAngle(_ i: Int, _ tb: Double) -> CGFloat {
        let h = heads[i]
        let u = ((tb - headsStart - h.delay) * headFPS).rounded(.down) / headFPS
        guard u > 0, u < wiggleDuration else { return 0 }
        func smooth(_ x: Double) -> Double { let c = clamp(x); return c * c * (3 - 2 * c) }
        let env = smooth(u / 0.12) * (1 - smooth((u - 0.7 * wiggleDuration) / (0.3 * wiggleDuration)))
        let wave = 0.75 * sin(2 * .pi * h.f1 * u) + 0.25 * sin(2 * .pi * h.f2 * u + h.phase)
        return headAmplitude * h.sign * CGFloat(h.amp * env * wave)
    }

    // 以下的 `tw` 为字标时间（基准时间减去 `wordOffset`）。

    /// 扫描线中心（字标空间 x），从左侧 3 格外扫到右侧 3 格外。
    static func lineX(_ tw: Double, _ w: SplashWordmark) -> CGFloat {
        -3 * w.cell + (w.size.width + 6 * w.cell) * CGFloat(lerp(tw, lineStart, lineEnd))
    }
    static func lineAlpha(_ tw: Double) -> Double {
        guard tw >= lineStart else { return 0 }
        return min(lerp(tw, lineStart, lineStart + 0.06), 1 - lerp(tw, lineEnd - 0.04, lineEnd + 0.06))
    }
    /// 扫描线经过该格中心的时刻，加固定抖动。
    static func cellStart(_ c: SplashWordmark.Cell, _ w: SplashWordmark) -> Double {
        lineStart + (lineEnd - lineStart) * Double((c.rect.midX + 3 * w.cell) / (w.size.width + 6 * w.cell)) + c.jitter
    }
    /// 0 = 还没出现，1…3 = 逐格落位中，3 = 到位。
    static func cellStep(_ c: SplashWordmark.Cell, _ tw: Double, _ w: SplashWordmark) -> Int {
        let s = cellStart(c, w)
        guard tw >= s else { return 0 }
        return min(3, 1 + Int((tw - s) / (cellDuration / 3)))
    }
    static func cellsAlpha(_ tw: Double) -> CGFloat { CGFloat(1 - lerp(tw, crossStart, crossEnd)) }
    static func vectorAlpha(_ tw: Double) -> CGFloat { CGFloat(lerp(tw, crossStart, crossEnd)) }
    static func shadowAlpha(_ tw: Double) -> CGFloat { tw >= shadowSteps.1 ? 1 : tw >= shadowSteps.0 ? 0.5 : 0 }
    static func subtitle(_ tw: Double) -> (alpha: CGFloat, rise: CGFloat) {
        let k = easeOut(lerp(tw, subtitleStart, subtitleEnd))
        return (CGFloat(k), CGFloat(6 * (1 - k)))
    }
    static func buttonAlpha(_ tw: Double) -> CGFloat { CGFloat(easeOut(lerp(tw, buttonStart, buttonEnd))) }
}
