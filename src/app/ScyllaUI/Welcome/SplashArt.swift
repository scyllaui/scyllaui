// 开屏页的 Scylla（M4a 块 4，docs/design.md 10.4 节决策 9）：读 generate-splash-art.swift 生成的分层素材（静止的身体与
// 六个头的图层、各头沿脖子的距离），按时间线的摆角画出一帧。脖子跟着弯曲的变形与块 0 原型相同。

import CoreGraphics
import Foundation
import ImageIO

final class SplashArt {
    /// `pixels`：头的图层（预乘 RGBA，行序 y 向下）；`bend`：每个像素沿脖子离切口的距离与弯曲长度之比（0—255）。
    /// 坐标都是图标母版的像素坐标（y 向下）。
    struct Head { let image: CGImage; let rect: CGRect; let pivot: CGPoint; let pixels: [UInt8]; let bend: [UInt8] }

    let size: CGSize
    let creatureBox: CGRect
    let bodyRect: CGRect
    let body: CGImage
    let heads: [Head]

    /// 按显示像素缩好的身体与静止的头（键为每个原图像素对应的设备像素数 × 10000）：每帧从原图缩太慢，第一帧会卡。
    private var scaled: [Int: (body: CGImage, heads: [CGImage])] = [:]
    /// 各头变形的结果（键为头与角度；取样步长变了即清空）。逐帧模式下一秒只换 12 次姿态：画完一帧就在后台先算好下一个
    /// 姿态，主线程不再现算（块 4 实测：六个头同一帧现算时帧间隔约 27 ms）。后台线程也会写，访问都经 `lock`。
    private var poses: [Int: [(angle: Double, image: CGImage, rect: CGRect)]] = [:]
    private var poseStep = 0.0
    private var inFlight = Set<String>()
    private let lock = NSLock()

    private static let space = CGColorSpace(name: CGColorSpace.sRGB)!

    /// 生成脚本加的 2 pt 轮廓色（`generate-splash-art.swift` 的 `outlineColor`，与深色开屏背景相同）。
    static let outlineColor: (UInt8, UInt8, UInt8) = (0x1E, 0x1F, 0x22)

    /// `outlineKeep`：轮廓只留离图形最近的这么多层像素（浅色开屏用 3，即 1 pt；2026-11-06 用户在 1pt、2pt、蓝色对比中选 1pt）；
    /// nil 为素材原样（2 pt）。
    init?(bundle: Bundle = .main, outlineKeep: Int? = nil) {
        func url(_ name: String, _ ext: String) -> URL? { bundle.url(forResource: name, withExtension: ext) }
        guard let metaURL = url("splash-art", "json"), let data = try? Data(contentsOf: metaURL),
              let meta = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
              let size = meta["size"] as? [Double], let creature = meta["creature"] as? [Double], let body = meta["body"] as? [Double],
              let heads = meta["heads"] as? [[String: Any]], heads.count == SplashTimeline.heads.count,
              let bodyURL = url("splash-body", "png"), let bodySource = Self.image(bodyURL),
              let bodyImage = outlineKeep == nil ? bodySource : Self.thinned(bodySource, outlineKeep!) else { return nil }
        func rect(_ a: [Double]) -> CGRect { CGRect(x: a[0], y: a[1], width: a[2], height: a[3]) }
        self.size = CGSize(width: size[0], height: size[1])
        creatureBox = rect(creature)
        bodyRect = rect(body)
        self.body = bodyImage
        var list: [Head] = []
        for (n, h) in heads.enumerated() {
            guard let r = h["rect"] as? [Double], let p = h["pivot"] as? [Double],
                  let imgURL = url("splash-head-\(n + 1)", "png"), let bendURL = url("splash-bend-\(n + 1)", "png"),
                  let img = Self.image(imgURL), let bendImg = Self.image(bendURL) else { return nil }
            let w = img.width, hgt = img.height
            guard bendImg.width == w, bendImg.height == hgt else { return nil }
            var pixels = [UInt8](repeating: 0, count: w * hgt * 4)
            var bend = [UInt8](repeating: 0, count: w * hgt)
            pixels.withUnsafeMutableBytes { buf in
                CGContext(data: buf.baseAddress, width: w, height: hgt, bitsPerComponent: 8, bytesPerRow: w * 4, space: Self.space,
                          bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)?.draw(img, in: CGRect(x: 0, y: 0, width: w, height: hgt))
            }
            bend.withUnsafeMutableBytes { buf in
                CGContext(data: buf.baseAddress, width: w, height: hgt, bitsPerComponent: 8, bytesPerRow: w, space: CGColorSpaceCreateDeviceGray(),
                          bitmapInfo: CGImageAlphaInfo.none.rawValue)?.draw(bendImg, in: CGRect(x: 0, y: 0, width: w, height: hgt))
            }
            var headImage = img
            if let outlineKeep {
                Self.thinOutline(&pixels, width: w, keep: outlineKeep)
                guard let made = Self.makeImage(&pixels, w, hgt) else { return nil }
                headImage = made
            }
            list.append(Head(image: headImage, rect: rect(r), pivot: CGPoint(x: p[0], y: p[1]), pixels: pixels, bend: bend))
        }
        self.heads = list
    }

    /// 把轮廓减到离图形最近的 `keep` 层像素，外侧的轮廓变透明（浅色开屏用 1 pt：原图 6 px 为 2 pt，留 3 px）。
    /// 从图形（不透明、不是轮廓色）向外逐层走，走到的轮廓保留；预乘 RGBA，不透明像素即原色。
    static func thinOutline(_ px: inout [UInt8], width w: Int, keep: Int) {
        let o = outlineColor, n = px.count / 4
        func isRing(_ i: Int) -> Bool { px[i * 4 + 3] == 255 && (px[i * 4], px[i * 4 + 1], px[i * 4 + 2]) == o }
        var kept = [Bool](repeating: false, count: n), seen = [Bool](repeating: false, count: n)
        var frontier = (0..<n).filter { px[$0 * 4 + 3] > 0 && !isRing($0) }
        for i in frontier { seen[i] = true }
        for _ in 0..<keep {
            var next: [Int] = []
            for i in frontier {
                let x = i % w
                for j in [x > 0 ? i - 1 : -1, x < w - 1 ? i + 1 : -1, i - w, i + w] where j >= 0 && j < n && !seen[j] && isRing(j) {
                    seen[j] = true; kept[j] = true; next.append(j)
                }
            }
            frontier = next
        }
        for i in 0..<n where isRing(i) && !kept[i] { (px[i * 4], px[i * 4 + 1], px[i * 4 + 2], px[i * 4 + 3]) = (0, 0, 0, 0) }
    }

    private static func makeImage(_ px: inout [UInt8], _ w: Int, _ h: Int) -> CGImage? {
        px.withUnsafeMutableBytes { buf in
            CGContext(data: buf.baseAddress, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4, space: space,
                      bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)?.makeImage()
        }
    }

    private static func thinned(_ img: CGImage, _ keep: Int) -> CGImage? {
        let (w, h) = (img.width, img.height)
        var px = [UInt8](repeating: 0, count: w * h * 4)
        px.withUnsafeMutableBytes { buf in
            CGContext(data: buf.baseAddress, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4, space: space,
                      bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)?.draw(img, in: CGRect(x: 0, y: 0, width: w, height: h))
        }
        thinOutline(&px, width: w, keep: keep)
        return makeImage(&px, w, h)
    }

    private static func image(_ url: URL) -> CGImage? {
        guard let src = CGImageSourceCreateWithURL(url as CFURL, nil) else { return nil }
        return CGImageSourceCreateImageAtIndex(src, 0, nil)
    }

    private func scaledImages(_ q: Double) -> (body: CGImage, heads: [CGImage]) {
        let key = Int((q * 10_000).rounded())
        if let c = scaled[key] { return c }
        func shrink(_ img: CGImage, _ rc: CGRect) -> CGImage {
            let w = max(1, Int((rc.width * q).rounded())), h = max(1, Int((rc.height * q).rounded()))
            guard let c = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0, space: Self.space,
                                    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { return img }
            c.interpolationQuality = .high
            c.draw(img, in: CGRect(x: 0, y: 0, width: w, height: h))
            return c.makeImage() ?? img
        }
        let made = (body: shrink(body, bodyRect), heads: heads.map { shrink($0.image, $0.rect) })
        scaled[key] = made
        return made
    }

    /// 画在 `r`（视图坐标，y 向上）里：Scylla 按比例放满 `r` 并居中；先画静止的身体，再按各自的角度画六个头。
    /// 要重算的变形用 `concurrentPerform` 并行算（逐帧换姿态时一次要算六个，串行会掉帧）。`tb` 为基准时间。
    func draw(in r: CGRect, tb: Double, ctx: CGContext) {
        let cb = creatureBox
        let k = min(r.width / cb.width, r.height / cb.height)
        let ox = r.midX - cb.width * k / 2, oy = r.midY - cb.height * k / 2
        func view(_ rc: CGRect) -> CGRect {
            CGRect(x: ox + (rc.minX - cb.minX) * k, y: oy + (cb.maxY - rc.maxY) * k, width: rc.width * k, height: rc.height * k)
        }
        ctx.saveGState()
        ctx.interpolationQuality = .high
        // 一个原图像素对应多少设备像素（含 Retina 与外层缩放）；变形按输出像素取样。
        let device = Double(k) * max(0.01, Double(abs(ctx.userSpaceToDeviceSpaceTransform.a)))
        let step = 1 / device
        let cached = scaledImages(device)
        ctx.draw(cached.body, in: view(bodyRect))
        let angles = heads.indices.map { Double(SplashTimeline.headAngle($0, tb)) }
        // 后台没算好的（第一个姿态、或后台来不及）当场并行算。
        let missing = heads.indices.filter { abs(angles[$0]) >= 0.0005 && pose($0, angles[$0], step) == nil }
        if !missing.isEmpty {
            var made = [(CGImage, CGRect)?](repeating: nil, count: missing.count)
            made.withUnsafeMutableBufferPointer { buf in
                DispatchQueue.concurrentPerform(iterations: missing.count) { j in
                    buf[j] = warp(heads[missing[j]], angle: angles[missing[j]], step: step)
                }
            }
            for (j, i) in missing.enumerated() { if let m = made[j] { store(i, angles[i], step, m) } }
        }
        for (i, hd) in heads.enumerated() {
            if abs(angles[i]) < 0.0005 {
                ctx.draw(cached.heads[i], in: view(hd.rect))
            } else if let (img, rc) = pose(i, angles[i], step) {
                ctx.draw(img, in: view(rc))
            }
        }
        ctx.restoreGState()
        prefetch(tb: tb + 1 / SplashTimeline.headFPS, step: step)
    }

    private func pose(_ i: Int, _ angle: Double, _ step: Double) -> (CGImage, CGRect)? {
        lock.lock(); defer { lock.unlock() }
        guard step == poseStep else { return nil }
        return poses[i]?.first { $0.angle == angle }.map { ($0.image, $0.rect) }
    }

    /// 每个头只留最近的 3 个姿态（当前、下一个与上一个）。
    private func store(_ i: Int, _ angle: Double, _ step: Double, _ made: (CGImage, CGRect)) {
        lock.lock(); defer { lock.unlock() }
        if step != poseStep { poses = [:]; poseStep = step }
        var list = poses[i] ?? []
        list.removeAll { $0.angle == angle }
        list.append((angle, made.0, made.1))
        poses[i] = Array(list.suffix(3))
    }

    /// 在后台算好 `tb` 时各头的姿态。
    private func prefetch(tb: Double, step: Double) {
        let todo: [(Int, Double)] = heads.indices.compactMap { i in
            let a = Double(SplashTimeline.headAngle(i, tb))
            return abs(a) >= 0.0005 && pose(i, a, step) == nil ? (i, a) : nil
        }
        let keys = todo.map { "\($0.0):\($0.1):\(step)" }
        lock.lock()
        let fresh = zip(todo, keys).filter { !inFlight.contains($0.1) }
        fresh.forEach { inFlight.insert($0.1) }
        lock.unlock()
        guard !fresh.isEmpty else { return }
        DispatchQueue.global(qos: .userInteractive).async { [self] in
            DispatchQueue.concurrentPerform(iterations: fresh.count) { j in
                let ((i, a), key) = fresh[j]
                if let m = warp(heads[i], angle: a, step: step) { store(i, a, step, m) }
                lock.lock(); inFlight.remove(key); lock.unlock()
            }
        }
    }

    /// 「脖子跟着弯」：沿脖子离切口 d 处的转角为 angle × smoothstep(d / 弯曲长度)，切口处为 0，与静止的脖子始终连续；
    /// 头部在弯曲长度之外整块转动。正向投射：每个原像素按自己的转角算出新位置，双线性分到输出像素上，按覆盖度归一化
    /// （覆盖达到预期密度的 60% 即不透明，折叠处取平均）。反向迭代在 45° 下不收敛（块 0 实测）。屏幕上逆时针为正，
    /// 换到 y 向下的坐标里是 R(−angle)。
    private func warp(_ hd: Head, angle: Double, step: Double) -> (CGImage, CGRect)? {
        let pv = hd.pivot, rc = hd.rect
        // 输出范围：原范围四角绕支点转 0、1/3、2/3、全角后的包围盒，再留边。
        var box = rc
        for f in [1.0 / 3, 2.0 / 3, 1.0] {
            let c = cos(angle * f), s = sin(angle * f)
            for corner in [CGPoint(x: rc.minX, y: rc.minY), CGPoint(x: rc.maxX, y: rc.minY), CGPoint(x: rc.minX, y: rc.maxY), CGPoint(x: rc.maxX, y: rc.maxY)] {
                let dx = Double(corner.x - pv.x), dy = Double(corner.y - pv.y)
                box = box.union(CGRect(x: Double(pv.x) + dx * c + dy * s, y: Double(pv.y) - dx * s + dy * c, width: 1, height: 1))
            }
        }
        box = box.insetBy(dx: -8, dy: -8)
        let ow = Int((box.width / step).rounded(.up)), oh = Int((box.height / step).rounded(.up))
        guard ow > 0, oh > 0 else { return nil }
        let sw = Int(rc.width), sh = Int(rc.height)
        // 转角只取决于比例 u（素材里量化到 0—255），按 256 级查表。
        let table: [(Double, Double)] = (0...255).map { n in
            let u = Double(n) / 255
            let a = angle * u * u * (3 - 2 * u)
            return (cos(a), sin(a))
        }
        // 原像素比输出像素密时 1 个样本，稀时每个原像素拆成 2×2 个子样本，保证输出不漏点。
        let sub = step < 1.6 ? 2 : 1
        let subW = 1.0 / Double(sub * sub)
        let expected = 1.0 / (step * step)
        var acc = [Float](repeating: 0, count: ow * oh * 4)
        var wsum = [Float](repeating: 0, count: ow * oh)
        let bx = Double(box.minX), by = Double(box.minY), px0 = Double(pv.x), py0 = Double(pv.y)
        hd.pixels.withUnsafeBufferPointer { src in
            for y in 0..<sh { for x in 0..<sw {
                let si = (y * sw + x) * 4
                let alpha = src[si + 3]
                guard alpha > 0 else { continue }
                let (c, s) = table[Int(hd.bend[y * sw + x])]
                for sy in 0..<sub { for sx in 0..<sub {
                    let qx = Double(rc.minX) + Double(x) + (Double(sx) + 0.5) / Double(sub)
                    let qy = Double(rc.minY) + Double(y) + (Double(sy) + 0.5) / Double(sub)
                    let dx = qx - px0, dy = qy - py0
                    let nx = px0 + dx * c + dy * s, ny = py0 - dx * s + dy * c
                    let fx = (nx - bx) / step - 0.5, fy = (ny - by) / step - 0.5
                    let x0 = Int(fx.rounded(.down)), y0 = Int(fy.rounded(.down))
                    let tx = fx - Double(x0), ty = fy - Double(y0)
                    for (ox, oy, wgt) in [(x0, y0, (1 - tx) * (1 - ty)), (x0 + 1, y0, tx * (1 - ty)), (x0, y0 + 1, (1 - tx) * ty), (x0 + 1, y0 + 1, tx * ty)] {
                        guard ox >= 0, oy >= 0, ox < ow, oy < oh else { continue }
                        let wv = Float(wgt * subW)
                        let o = oy * ow + ox
                        acc[o * 4] += Float(src[si]) * wv; acc[o * 4 + 1] += Float(src[si + 1]) * wv
                        acc[o * 4 + 2] += Float(src[si + 2]) * wv; acc[o * 4 + 3] += Float(alpha) * wv
                        wsum[o] += wv
                    }
                } }
            } }
        }
        var out = [UInt8](repeating: 0, count: ow * oh * 4)
        let full = Float(expected * 0.6)
        for o in 0..<(ow * oh) where wsum[o] > 0 {
            let cover = min(1, wsum[o] / full)
            let k = cover / wsum[o]
            for ch in 0..<4 { out[o * 4 + ch] = UInt8(min(255, acc[o * 4 + ch] * k)) }
        }
        let img = out.withUnsafeMutableBytes { buf in
            CGContext(data: buf.baseAddress, width: ow, height: oh, bitsPerComponent: 8, bytesPerRow: ow * 4, space: Self.space,
                      bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)?.makeImage()
        }
        guard let img else { return nil }
        return (img, CGRect(x: box.minX, y: box.minY, width: CGFloat(ow) * step, height: CGFloat(oh) * step))
    }
}
