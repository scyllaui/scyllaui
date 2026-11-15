# -*- coding: utf-8 -*-
"""生成 Claude 工具图标 tool-claude.svg（docs/design.md 10.6 节 D12，2026-10-30 用户选定方案 D）：自绘的不对称墨迹，
与 Anthropic 的标志无关、不描摹任何现有图形。开发用，不进入产品运行时。
做法：距离场——中心一个圆，每个墨滴是「根部粗、中段细、末端圆滴」的两段锥形颈加一个圆，平滑并集相连，另有脱离的小墨点；
取零等值线（marching squares），RDP 化简后用 Catmull-Rom 转三次贝塞尔，输出 24×24 的单一路径 SVG。
用法：claude-splash.py <输出目录>（写 <目录>/splash-D.svg）"""
import math, sys

G = 288          # 采样网格边长
S = 24.0         # viewBox
FIT = 24.0       # 输出时外接框的长边（2026-10-30 用户：第二版 22 格再放大 10%，以画布 24 格为上限）


def sd_circle(x, y, cx, cy, r):
    return math.hypot(x - cx, y - cy) - r


def sd_cone(x, y, ax, ay, ra, bx, by, rb):
    """两端半径不同的胶囊（round cone）的近似距离：沿线段按投影插值半径。"""
    dx, dy = bx - ax, by - ay
    L2 = dx * dx + dy * dy
    t = max(0.0, min(1.0, ((x - ax) * dx + (y - ay) * dy) / L2))
    px, py = ax + t * dx, ay + t * dy
    return math.hypot(x - px, y - py) - (ra + (rb - ra) * t)


def smin(a, b, k):
    h = max(k - abs(a - b), 0.0) / k
    return min(a, b) - h * h * k * 0.25


def build(center, core_r, drops, k=0.55, dots=(), reach=1.0):
    """drops：(角度°, 距离, 墨滴半径, 颈根部半径, 颈最细半径)。返回 f(x, y) = -sdf。"""
    cx, cy = center
    parts = []
    for ang, dist, r, w0, w1 in drops:
        dist *= reach
        a = math.radians(ang)
        ex, ey = cx + dist * math.cos(a), cy + dist * math.sin(a)
        mx, my = cx + (dist * 0.62) * math.cos(a), cy + (dist * 0.62) * math.sin(a)
        parts.append(("cone", (cx, cy, w0, mx, my, w1)))
        parts.append(("cone", (mx, my, w1, ex, ey, r * 0.8)))
        parts.append(("circle", (ex, ey, r)))
    def f(x, y):
        d = sd_circle(x, y, cx, cy, core_r)
        for dx_, dy_, dr_ in dots:
            d = min(d, sd_circle(x, y, dx_, dy_, dr_))
        for kind, args in parts:
            e = sd_cone(x, y, *args) if kind == "cone" else sd_circle(x, y, *args)
            d = smin(d, e, k)
        return -d
    return f


def contour(f, thr=0.0):
    h = S / G
    val = [[f(i * h, j * h) for i in range(G + 1)] for j in range(G + 1)]
    segs = []
    def interp(p1, p2, v1, v2):
        t = (thr - v1) / (v2 - v1) if v2 != v1 else 0.5
        return (p1[0] + t * (p2[0] - p1[0]), p1[1] + t * (p2[1] - p1[1]))
    for j in range(G):
        for i in range(G):
            c = [(i * h, j * h), ((i + 1) * h, j * h), ((i + 1) * h, (j + 1) * h), (i * h, (j + 1) * h)]
            v = [val[j][i], val[j][i + 1], val[j + 1][i + 1], val[j + 1][i]]
            idx = sum(1 << k for k in range(4) if v[k] >= thr)
            if idx in (0, 15):
                continue
            e = {}
            for k, (a, b) in enumerate([(0, 1), (1, 2), (2, 3), (3, 0)]):
                if (v[a] >= thr) != (v[b] >= thr):
                    e[k] = interp(c[a], c[b], v[a], v[b])
            ks = sorted(e)
            if len(ks) == 2:
                segs.append((e[ks[0]], e[ks[1]]))
            elif len(ks) == 4:
                segs.append((e[0], e[1])); segs.append((e[2], e[3]))
    # 连成环
    key = lambda p: (round(p[0], 5), round(p[1], 5))
    adj = {}
    for a, b in segs:
        adj.setdefault(key(a), []).append(b); adj.setdefault(key(b), []).append(a)
    seen, loops = set(), []
    for a, b in segs:
        if key(a) in seen:
            continue
        loop, cur, prev = [a], a, None
        while True:
            seen.add(key(cur))
            nxt = [p for p in adj[key(cur)] if prev is None or key(p) != key(prev)]
            nxt = [p for p in nxt if key(p) not in seen] or []
            if not nxt:
                break
            prev, cur = cur, nxt[0]
            loop.append(cur)
        if len(loop) > 8:
            loops.append(loop)
    return loops


def rdp(pts, eps):
    if len(pts) < 3:
        return pts
    a, b = pts[0], pts[-1]
    dx, dy = b[0] - a[0], b[1] - a[1]
    L = math.hypot(dx, dy) or 1e-9
    dmax, idx = 0, 0
    for i in range(1, len(pts) - 1):
        d = abs(dy * pts[i][0] - dx * pts[i][1] + b[0] * a[1] - b[1] * a[0]) / L
        if d > dmax:
            dmax, idx = d, i
    if dmax > eps:
        return rdp(pts[:idx + 1], eps)[:-1] + rdp(pts[idx:], eps)
    return [a, b]


def to_path(loop):
    h = len(loop) // 2
    pts = rdp(loop[:h + 1], 0.035)[:-1] + rdp(loop[h:] + [loop[0]], 0.035)[:-1]
    n = len(pts)
    out = [f"M{pts[0][0]:.2f} {pts[0][1]:.2f}"]
    for i in range(n):
        p0, p1, p2, p3 = pts[(i - 1) % n], pts[i], pts[(i + 1) % n], pts[(i + 2) % n]
        c1 = (p1[0] + (p2[0] - p0[0]) / 6, p1[1] + (p2[1] - p0[1]) / 6)
        c2 = (p2[0] - (p3[0] - p1[0]) / 6, p2[1] - (p3[1] - p1[1]) / 6)
        out.append(f"C{c1[0]:.2f} {c1[1]:.2f} {c2[0]:.2f} {c2[1]:.2f} {p2[0]:.2f} {p2[1]:.2f}")
    return "".join(out) + "Z", n


VARIANTS = {
    # (角度°, 距离, 墨滴半径, 颈根部半径, 颈最细半径)；y 轴向下，-90° 为正上方。
    # 7 滴偏向右上甩出（长而大），左下短而小，右上外侧两颗脱离的小墨点：有方向感，不是居中对称的星芒。
    # 2026-10-30 第二版：左下三滴拉长到与右上相当（用户：太短、不细看不明显），方向感留在右上更大更远的墨滴与外侧小墨点上。
    "D": dict(center=(11.0, 12.6), core_r=3.6, k=1.3, drops=[
        (-78, 8.0, 2.1, 1.85, 0.95), (-38, 9.0, 2.35, 1.95, 1.0), (4, 8.0, 1.95, 1.8, 0.92),
        (52, 6.9, 1.55, 1.55, 0.82), (128, 7.6, 1.75, 1.65, 0.88), (182, 7.2, 1.7, 1.6, 0.85), (232, 6.6, 1.45, 1.5, 0.8)],
        # 第四版：墨滴伸出距离都乘 1.1（用户：第三版仍偏小，伸出的墨迹视觉上较轻）。
        dots=[(22.6, 2.5, 0.85)], reach=1.1),
}

if __name__ == "__main__":
    outdir = sys.argv[1]
    for name, v in VARIANTS.items():
        loops = contour(build(**v))
        loops.sort(key=len, reverse=True)
        # 等比缩放并居中到 FIT × FIT 的外接框：与其他工具图标的外接框同一量级（Grok 23.5 × 22.4、Kimi 19.9），视觉大小一致。
        xs = [p[0] for l in loops for p in l]; ys = [p[1] for l in loops for p in l]
        k_ = FIT / max(max(xs) - min(xs), max(ys) - min(ys))
        cx_, cy_ = (max(xs) + min(xs)) / 2, (max(ys) + min(ys)) / 2
        loops = [[(12 + (x - cx_) * k_, 12 + (y - cy_) * k_) for x, y in l] for l in loops]
        paths = [to_path(l) for l in loops]
        d, n = "".join(p for p, _ in paths), sum(c for _, c in paths)
        svg = (f'<svg height="24" viewBox="0 0 24 24" width="24" xmlns="http://www.w3.org/2000/svg">'
               f'<title>Claude</title><path fill="#000" d="{d}"/></svg>\n')
        open(f"{outdir}/splash-{name}.svg", "w").write(svg)
        xs = [p[0] for p in loops[0]]; ys = [p[1] for p in loops[0]]
        print(name, "环数", len(loops), "节点", n, "范围 x %.1f—%.1f y %.1f—%.1f" % (min(xs), max(xs), min(ys), max(ys)), "字节", len(svg))
