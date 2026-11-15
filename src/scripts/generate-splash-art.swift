// 生成开屏页的分层素材：从新版冰蓝母版 assets/scylla-pseudo-pixel.png 提取。
// 六个头按 2026-11-06 新构图重新标定；坐标顺序为左上、右上、左中、右中、左下、右下，见 assets/README.md。
// 取出 Scylla 本身（去掉方形背景），切出六个头（连同上半截脖子），加 2 pt 轮廓，并算出每个头「沿脖子的距离」。
// 规则与块 0 原型（output/m4a-prototype/splash.swift，用户定稿）相同，App 里不再做图像分割。
// 输出到 src/app/ScyllaUI/Resources/：splash-body.png、splash-head-1…6.png、splash-bend-1…6.png、splash-art.json。
// 用法：swift src/scripts/generate-splash-art.swift
import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent("../..").standardized
let source = root.appendingPathComponent("assets/scylla-pseudo-pixel.png")
let outDir = root.appendingPathComponent("src/app/ScyllaUI/Resources")

/// 每个头：在多边形内从种子点做连通填充；`cut` 是多边形的一条边，切口另一侧为静止的脖子（原图坐标，y 向下）。
struct HeadSpec {
    let seed: (Int, Int)
    let polygon: [(Int, Int)]
    let cut: ((Int, Int), (Int, Int))
}

let headSpecs: [HeadSpec] = [
    HeadSpec(seed: (340, 220), polygon: [(160, 70), (610, 70), (610, 360), (420, 360), (380, 310), (160, 310)], cut: ((420, 360), (610, 360))),
    HeadSpec(seed: (914, 220), polygon: [(644, 70), (1094, 70), (1094, 310), (874, 310), (834, 360), (644, 360)], cut: ((644, 360), (834, 360))),
    HeadSpec(seed: (300, 445), polygon: [(90, 315), (410, 315), (410, 370), (450, 430), (330, 530), (90, 530)], cut: ((450, 430), (330, 530))),
    HeadSpec(seed: (954, 445), polygon: [(844, 315), (1164, 315), (1164, 530), (924, 530), (804, 430), (844, 370)], cut: ((804, 430), (924, 530))),
    HeadSpec(seed: (270, 660), polygon: [(75, 535), (320, 535), (320, 580), (395, 615), (450, 680), (340, 790), (75, 760)], cut: ((450, 680), (340, 790))),
    HeadSpec(seed: (984, 660), polygon: [(934, 535), (1179, 535), (1179, 760), (914, 790), (804, 680), (859, 615), (934, 580)], cut: ((804, 680), (914, 790))),
]

/// 轮廓 2 pt（按 Scylla 在开屏里的显示边长 340 pt 换算成原图像素），颜色与深色模式的开屏背景相同（2026-10-23 用户定）。
let outlinePt = 2.0, shownSide = 340.0
let outlineColor: (UInt8, UInt8, UInt8) = (0x1E, 0x1F, 0x22)
/// 脖子弯曲的长度（原图像素，沿脖子量），与 App 的变形一致。
let bendLength = 220.0

func fail(_ s: String) -> Never { FileHandle.standardError.write((s + "\n").data(using: .utf8)!); exit(1) }

guard let src = CGImageSourceCreateWithURL(source as CFURL, nil), let cg = CGImageSourceCreateImageAtIndex(src, 0, nil) else {
    fail("读不到图标母版：\(source.path)")
}
let w = cg.width, h = cg.height
let cs = CGColorSpace(name: CGColorSpace.sRGB)!
let rgba = CGImageAlphaInfo.premultipliedLast.rawValue
var px = [UInt8](repeating: 0, count: w * h * 4)
px.withUnsafeMutableBytes { buf in
    CGContext(data: buf.baseAddress, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4, space: cs, bitmapInfo: rgba)!
        .draw(cg, in: CGRect(x: 0, y: 0, width: w, height: h))
}
// 位图上下文的内存第 0 行是图像的顶行：下标即 y 向下的坐标，与多边形一致。

/// 前景：头与脖子是亮青 / 蓝色，底是深蓝灰。
func fg(_ i: Int) -> Bool { max(px[i * 4], px[i * 4 + 1], px[i * 4 + 2]) > 80 && px[i * 4 + 3] > 200 }
func neighbors(_ i: Int) -> [Int] {
    let x = i % w, y = i / w
    return [(x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)].filter { $0.0 >= 0 && $0.1 >= 0 && $0.0 < w && $0.1 < h }.map { $0.1 * w + $0.0 }
}
func distToSegment(_ x: Int, _ y: Int, _ a: (Int, Int), _ b: (Int, Int)) -> Double {
    let (ax, ay, bx, by) = (Double(a.0), Double(a.1), Double(b.0), Double(b.1))
    let (dx, dy) = (bx - ax, by - ay)
    let t = max(0, min(1, ((Double(x) - ax) * dx + (Double(y) - ay) * dy) / (dx * dx + dy * dy)))
    return hypot(Double(x) - (ax + t * dx), Double(y) - (ay + t * dy))
}

/// 掩码按 (2r+1)×(2r+1) 的方形外扩（像素画的轮廓是方的），只算 box（含两端）内；先横向后纵向，各用滑动计数。
func dilate(_ m: [Bool], r: Int, box: (Int, Int, Int, Int)) -> [Bool] {
    let (bx0, by0, bx1, by1) = box
    var horiz = [Bool](repeating: false, count: w * h)
    for y in by0...by1 {
        var count = 0
        for x in max(0, bx0 - r)...min(w - 1, bx0 + r) where m[y * w + x] { count += 1 }
        for x in bx0...bx1 {
            horiz[y * w + x] = count > 0
            let out = x - r, inn = x + r + 1
            if out >= 0, m[y * w + out] { count -= 1 }
            if inn < w, m[y * w + inn] { count += 1 }
        }
    }
    var out = [Bool](repeating: false, count: w * h)
    for x in bx0...bx1 {
        var count = 0
        for y in max(0, by0 - r)...min(h - 1, by0 + r) where horiz[y * w + x] { count += 1 }
        for y in by0...by1 {
            out[y * w + x] = count > 0
            let o = y - r, n = y + r + 1
            if o >= 0, horiz[o * w + x] { count -= 1 }
            if n < h, horiz[n * w + x] { count += 1 }
        }
    }
    return out
}

/// 最小堆（距离, 下标），供沿脖子的最短路径用。
struct MinHeap {
    private var a: [(Float, Int)] = []
    mutating func push(_ d: Float, _ i: Int) {
        a.append((d, i))
        var c = a.count - 1
        while c > 0 { let p = (c - 1) / 2; if a[p].0 <= a[c].0 { break }; a.swapAt(p, c); c = p }
    }
    mutating func pop() -> (Float, Int)? {
        guard !a.isEmpty else { return nil }
        let top = a[0], last = a.removeLast()
        if !a.isEmpty {
            a[0] = last
            var p = 0
            while true {
                let l = 2 * p + 1, r = l + 1
                var m = p
                if l < a.count, a[l].0 < a[m].0 { m = l }
                if r < a.count, a[r].0 < a[m].0 { m = r }
                if m == p { break }
                a.swapAt(p, m); p = m
            }
        }
        return top
    }
}

// Scylla 本身：从身体上一点在前景里连通填充（六个头经脖子与身体相连）。
var creature = [Bool](repeating: false, count: w * h)
var seedBody = -1
find: for r in 0...40 { for dy in -r...r { for dx in -r...r {
    let i = (960 + dy) * w + (680 + dx)
    if fg(i) { seedBody = i; break find }
} } }
guard seedBody >= 0 else { fail("身体上的种子点附近没有前景") }
var stack = [seedBody]
creature[seedBody] = true
while let i = stack.popLast() { for j in neighbors(i) where !creature[j] && fg(j) { creature[j] = true; stack.append(j) } }
// 身体上的 >_：颜色与脖子之间的缝隙几乎相同，只能按位置认：从这两处出发，在深色像素里连通填充。
for (gx, gy) in [(540, 980), (695, 1090)] {
    var gs = [gy * w + gx]
    while let i = gs.popLast() {
        guard !creature[i], max(px[i * 4], px[i * 4 + 1], px[i * 4 + 2]) < 70 else { continue }
        creature[i] = true
        gs.append(contentsOf: neighbors(i))
    }
}
var cb = (w, h, 0, 0)
for i in 0..<(w * h) where creature[i] { let x = i % w, y = i / w; cb = (min(cb.0, x), min(cb.1, y), max(cb.2, x), max(cb.3, y)) }
let creatureBox = (x: cb.0, y: cb.1, w: cb.2 - cb.0 + 1, h: cb.3 - cb.1 + 1)
let ring = max(1, Int((outlinePt / (shownSide / Double(max(creatureBox.w, creatureBox.h)))).rounded()))

struct Head { let rect: (Int, Int, Int, Int); let pivot: (Double, Double); let pixels: [UInt8]; let bend: [UInt8] }
var heads: [Head] = []
var eraseAll = [Bool](repeating: false, count: w * h)
for (n, spec) in headSpecs.enumerated() {
    let path = CGMutablePath()
    path.addLines(between: spec.polygon.map { CGPoint(x: $0.0, y: $0.1) })
    path.closeSubpath()
    let pb = path.boundingBox
    let x0 = max(0, Int(pb.minX) - 24), x1 = min(w - 1, Int(pb.maxX) + 24)
    let y0 = max(0, Int(pb.minY) - 24), y1 = min(h - 1, Int(pb.maxY) + 24)
    var inPoly = [Bool](repeating: false, count: w * h)
    for y in y0...y1 { for x in x0...x1 where path.contains(CGPoint(x: Double(x) + 0.5, y: Double(y) + 0.5)) { inPoly[y * w + x] = true } }
    var seed = -1
    search: for r in 0...30 { for dy in -r...r { for dx in -r...r {
        let x = spec.seed.0 + dx, y = spec.seed.1 + dy, i = y * w + x
        if x >= 0, y >= 0, x < w, y < h, inPoly[i], fg(i) { seed = i; break search }
    } } }
    guard seed >= 0 else { fail("头 \(n + 1)：种子点附近没有前景") }
    // 连通填充（4 邻接），只在多边形内。
    var region = [Bool](repeating: false, count: w * h)
    stack = [seed]
    region[seed] = true
    while let i = stack.popLast() {
        let x = i % w, y = i / w
        for (nx, ny) in [(x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)] where nx >= x0 && nx <= x1 && ny >= y0 && ny <= y1 {
            let j = ny * w + nx
            if !region[j] && inPoly[j] && fg(j) { region[j] = true; stack.append(j) }
        }
    }
    // 补内部空洞（眼睛等）：从外框向内填非区域像素，填不到的就是洞。
    var outside = [Bool](repeating: false, count: w * h)
    stack = []
    for x in x0...x1 { for y in [y0, y1] { let i = y * w + x; if !region[i] { outside[i] = true; stack.append(i) } } }
    for y in y0...y1 { for x in [x0, x1] { let i = y * w + x; if !region[i] && !outside[i] { outside[i] = true; stack.append(i) } } }
    while let i = stack.popLast() {
        let x = i % w, y = i / w
        for (nx, ny) in [(x + 1, y), (x - 1, y), (x, y + 1), (x, y - 1)] where nx >= x0 && nx <= x1 && ny >= y0 && ny <= y1 {
            let j = ny * w + nx
            if !region[j] && !outside[j] { outside[j] = true; stack.append(j) }
        }
    }
    for y in y0...y1 { for x in x0...x1 where !outside[y * w + x] { region[y * w + x] = true } }
    // 只有标定切口可以穿过颈部。其他多边形边必须走背景，否则会把细条轮廓留在静止身体上。
    var strayCuts = [(Int, Int)]()
    for y in y0...y1 { for x in x0...x1 where region[y * w + x] && fg(y * w + x) {
        if distToSegment(x, y, spec.cut.0, spec.cut.1) > 2,
           neighbors(y * w + x).contains(where: { !inPoly[$0] && fg($0) }) {
            strayCuts.append((x, y))
        }
    } }
    guard strayCuts.isEmpty else { fail("头 \(n + 1)：非切口边穿过前景（\(strayCuts.count) px，首点 \(strayCuts[0])），请重标多边形") }
    // 头四周 24 像素（原画的投影范围）：底图里挖掉。
    var nearHead = region
    for _ in 0..<24 {
        var next = nearHead
        for y in max(1, y0)...min(h - 2, y1) { for x in max(1, x0)...min(w - 2, x1) where !nearHead[y * w + x] {
            let i = y * w + x
            if nearHead[i - 1] || nearHead[i + 1] || nearHead[i - w] || nearHead[i + w] { next[i] = true }
        } }
        nearHead = next
    }
    // 擦除区：区域外扩 2 像素（含抗锯齿边），只在多边形内；重叠带：切口另一侧 18 像素内的前景。
    var erase = [Bool](repeating: false, count: w * h)
    var sprite = [Bool](repeating: false, count: w * h)
    var pivotSum = (0.0, 0.0, 0)
    for y in y0...y1 { for x in x0...x1 {
        let i = y * w + x
        if inPoly[i] {
            var near = region[i]
            if !near { outer: for dy in -2...2 { for dx in -2...2 {
                let nx = x + dx, ny = y + dy
                if nx >= 0, ny >= 0, nx < w, ny < h, region[ny * w + nx] { near = true; break outer }
            } } }
            if near { erase[i] = true; sprite[i] = true }
            if nearHead[i] && !region[i] && !fg(i) { erase[i] = true }
            if region[i] && distToSegment(x, y, spec.cut.0, spec.cut.1) <= 4 { pivotSum.0 += Double(x); pivotSum.1 += Double(y); pivotSum.2 += 1 }
        } else if fg(i) && distToSegment(x, y, spec.cut.0, spec.cut.1) <= 18 {
            sprite[i] = true
        }
    } }
    // 轮廓连同 18 px 静止重叠带一起延伸，避免浅色主题把各图层轮廓减薄后在切口外缘留下缺口。
    var isOutline = [Bool](repeating: false, count: w * h)
    let ox0 = max(0, x0 - ring), ox1 = min(w - 1, x1 + ring), oy0 = max(0, y0 - ring), oy1 = min(h - 1, y1 + ring)
    let outlineBase = (0..<(w * h)).map { region[$0] || (sprite[$0] && fg($0)) }
    let grown = dilate(outlineBase, r: ring, box: (ox0, oy0, ox1, oy1))
    for y in oy0...oy1 { for x in ox0...ox1 {
        let i = y * w + x
        if sprite[i] && !region[i] && !fg(i) { sprite[i] = false }
        if grown[i] && !region[i] && !creature[i] { isOutline[i] = true; sprite[i] = true; erase[i] = true }
    } }
    guard pivotSum.2 > 0 else { fail("头 \(n + 1)：切口未穿过颈部") }
    let pivot = (pivotSum.0 / Double(pivotSum.2), pivotSum.1 / Double(pivotSum.2))
    var sx0 = w, sy0 = h, sx1 = 0, sy1 = 0
    for y in oy0...oy1 { for x in ox0...ox1 where sprite[y * w + x] { sx0 = min(sx0, x); sx1 = max(sx1, x); sy0 = min(sy0, y); sy1 = max(sy1, y) } }
    let sw = sx1 - sx0 + 1, sh = sy1 - sy0 + 1
    var spx = [UInt8](repeating: 0, count: sw * sh * 4)
    for y in sy0...sy1 { for x in sx0...sx1 where sprite[y * w + x] {
        let i = (y * w + x) * 4, o = ((y - sy0) * sw + (x - sx0)) * 4
        if isOutline[y * w + x] {
            (spx[o], spx[o + 1], spx[o + 2], spx[o + 3]) = (outlineColor.0, outlineColor.1, outlineColor.2, 255)
        } else {
            for k in 0..<4 { spx[o + k] = px[i + k] }
        }
    } }
    // 切口法线，取指向种子点（头）的一侧。
    let (ca, cb2) = spec.cut
    var nx = -Double(cb2.1 - ca.1), ny = Double(cb2.0 - ca.0)
    let nl = hypot(nx, ny); nx /= nl; ny /= nl
    if (Double(spec.seed.0 - ca.0) * nx + Double(spec.seed.1 - ca.1) * ny) < 0 { nx = -nx; ny = -ny }
    // 沿脖子的距离：在头的图层（本体与轮廓，不含原画的投影）上做 8 邻接最短路径（Dijkstra），切口另一侧（含重叠带）
    // 为 0；图层外的像素取最近图层像素的值（与原型相同）。
    func body(_ x: Int, _ y: Int) -> Bool { sprite[y * w + x] }
    var dist = [Float](repeating: .infinity, count: sw * sh)
    var heap = MinHeap()
    for y in sy0...sy1 { for x in sx0...sx1 where body(x, y) {
        let side = Double(x - ca.0) * nx + Double(y - ca.1) * ny
        if side <= 0 { let j = (y - sy0) * sw + (x - sx0); dist[j] = 0; heap.push(0, j) }
    } }
    let steps: [(Int, Int, Float)] = [(1, 0, 1), (-1, 0, 1), (0, 1, 1), (0, -1, 1), (1, 1, 1.4142), (1, -1, 1.4142), (-1, 1, 1.4142), (-1, -1, 1.4142)]
    while let (d, j) = heap.pop() {
        if d > dist[j] { continue }
        let x = j % sw, y = j / sw
        for (dx, dy, c) in steps {
            let xx = x + dx, yy = y + dy
            guard xx >= 0, yy >= 0, xx < sw, yy < sh, body(xx + sx0, yy + sy0) else { continue }
            let k = yy * sw + xx, nd = d + c
            if nd < dist[k] { dist[k] = nd; heap.push(nd, k) }
        }
    }
    var queue: [Int] = []
    for j in 0..<(sw * sh) where dist[j].isFinite { queue.append(j) }
    var qi = 0
    while qi < queue.count {
        let j = queue[qi]; qi += 1
        let x = j % sw, y = j / sw
        for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let xx = x + dx, yy = y + dy
            guard xx >= 0, yy >= 0, xx < sw, yy < sh else { continue }
            let k = yy * sw + xx
            if !dist[k].isFinite { dist[k] = dist[j]; queue.append(k) }
        }
    }
    let bend = dist.map { UInt8((min(1, Double($0) / bendLength) * 255).rounded()) }
    heads.append(Head(rect: (sx0, sy0, sw, sh), pivot: pivot, pixels: spx, bend: bend))
    for i in 0..<(w * h) where erase[i] || isOutline[i] { eraseAll[i] = true }
    print(String(format: "头 %d：图层 %d×%d，支点 (%.1f, %.1f)", n + 1, sw, sh, pivot.0, pivot.1))
}

// 静止的部分：本体原样，外侧 ring 像素为轮廓，其余（方形背景与边框、六个头的位置）透明。
var still = [Bool](repeating: false, count: w * h)
for i in 0..<(w * h) where creature[i] && !eraseAll[i] { still[i] = true }
let grownStill = dilate(still, r: ring, box: (0, 0, w - 1, h - 1))
var bodyPixels = [UInt8](repeating: 0, count: w * h * 4)
for i in 0..<(w * h) where !eraseAll[i] {
    if creature[i] {
        for k in 0..<4 { bodyPixels[i * 4 + k] = px[i * 4 + k] }
    } else if grownStill[i] {
        (bodyPixels[i * 4], bodyPixels[i * 4 + 1], bodyPixels[i * 4 + 2], bodyPixels[i * 4 + 3]) = (outlineColor.0, outlineColor.1, outlineColor.2, 255)
    }
}
// 裁到有内容的范围。
var bx = (w, h, 0, 0)
for i in 0..<(w * h) where bodyPixels[i * 4 + 3] > 0 { let x = i % w, y = i / w; bx = (min(bx.0, x), min(bx.1, y), max(bx.2, x), max(bx.3, y)) }
let bodyRect = (bx.0, bx.1, bx.2 - bx.0 + 1, bx.3 - bx.1 + 1)
var cropped = [UInt8](repeating: 0, count: bodyRect.2 * bodyRect.3 * 4)
for y in 0..<bodyRect.3 {
    let from = ((bodyRect.1 + y) * w + bodyRect.0) * 4
    cropped.replaceSubrange((y * bodyRect.2 * 4)..<((y + 1) * bodyRect.2 * 4), with: bodyPixels[from..<(from + bodyRect.2 * 4)])
}

/// 写 PNG：RGBA（预乘，y 向下的行序即图像的行序）或 8 位灰度。
func writePNG(_ name: String, _ bytes: [UInt8], _ width: Int, _ height: Int, gray: Bool = false) {
    var b = bytes
    let img = b.withUnsafeMutableBytes { buf -> CGImage in
        gray
            ? CGContext(data: buf.baseAddress, width: width, height: height, bitsPerComponent: 8, bytesPerRow: width,
                        space: CGColorSpaceCreateDeviceGray(), bitmapInfo: CGImageAlphaInfo.none.rawValue)!.makeImage()!
            : CGContext(data: buf.baseAddress, width: width, height: height, bitsPerComponent: 8, bytesPerRow: width * 4,
                        space: cs, bitmapInfo: rgba)!.makeImage()!
    }
    let url = outDir.appendingPathComponent(name)
    guard let dest = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil) else { fail("写不了 \(url.path)") }
    CGImageDestinationAddImage(dest, img, nil)
    guard CGImageDestinationFinalize(dest) else { fail("写不了 \(url.path)") }
}

writePNG("splash-body.png", cropped, bodyRect.2, bodyRect.3)
for (n, hd) in heads.enumerated() {
    writePNG("splash-head-\(n + 1).png", hd.pixels, hd.rect.2, hd.rect.3)
    writePNG("splash-bend-\(n + 1).png", hd.bend, hd.rect.2, hd.rect.3, gray: true)
}
let meta: [String: Any] = [
    "comment": "由 src/scripts/generate-splash-art.swift 生成，勿手改。坐标为图标母版的像素坐标（y 向下）。",
    "size": [w, h],
    "source": "assets/scylla-pseudo-pixel.png",
    "creature": [creatureBox.x, creatureBox.y, creatureBox.w, creatureBox.h],
    "body": [bodyRect.0, bodyRect.1, bodyRect.2, bodyRect.3],
    "bend_length": bendLength,
    "heads": zip(heads, headSpecs).map { hd, spec -> [String: Any] in
        ["rect": [hd.rect.0, hd.rect.1, hd.rect.2, hd.rect.3], "pivot": [hd.pivot.0, hd.pivot.1],
         "seed": [spec.seed.0, spec.seed.1], "cut": [[spec.cut.0.0, spec.cut.0.1], [spec.cut.1.0, spec.cut.1.1]]]
    },
]
let json = try JSONSerialization.data(withJSONObject: meta, options: [.prettyPrinted, .sortedKeys])
try json.write(to: outDir.appendingPathComponent("splash-art.json"))
print("splash：母版 \(w)×\(h)，Scylla \(creatureBox.w)×\(creatureBox.h)，轮廓 \(ring) 像素，身体图层 \(bodyRect.2)×\(bodyRect.3)")
