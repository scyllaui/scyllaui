# -*- coding: utf-8 -*-
"""生成 Kimi 工具图标 tool-kimi.svg / tool-kimi-dark.svg（docs/design.md 10.6 节 D12）：自绘新月，开口朝左上，凹处三颗四角星。
2026-11-01 用户改定：三颗星放大，大星移向左上、与两颗小星不成直线（成三角形），黄色调亮。开发用，不进入产品运行时。
用法：python3 src/scripts/icons/kimi-moon.py（直接写入 ToolIcons.xcassets/tool-kimi.imageset）"""
from pathlib import Path

OUT = Path(__file__).resolve().parents[2] / "app/ScyllaUI/ToolIcons.xcassets/tool-kimi.imageset"
# 月牙：外圆半径 10.4、内圆半径 9.7，两圆交于左下与右上两个尖。
MOON = "M3.035 16.683A10.4 10.4 0 1 0 16.683 3.035A9.7 9.7 0 0 1 3.035 16.683Z"
# (中心 x, 中心 y, 尖角半径)：大星、左下小星、右上小星；三颗都离月牙边留出间隙。
STARS = [(8.0, 8.2, 3.9), (5.6, 15.2, 2.1), (13.3, 5.3, 2.1)]
# 腰点到中心的距离与尖角半径之比：17 pt 下仍看得出是四角星。
WAIST = 0.34
# 浅色外观的黄比深色略深，免得在白底上发虚。
COLORS = {"tool-kimi.svg": "#f0b000", "tool-kimi-dark.svg": "#ffc933"}


def star(cx, cy, r):
    w = r * WAIST / 2 ** 0.5  # 腰点在对角线上
    pts = [(cx, cy - r), (cx + w, cy - w), (cx + r, cy), (cx + w, cy + w),
           (cx, cy + r), (cx - w, cy + w), (cx - r, cy), (cx - w, cy - w)]
    return "M" + "L".join(f"{x:.3f} {y:.3f}" for x, y in pts) + "Z"


d = MOON + "".join(star(*s) for s in STARS)
for name, color in COLORS.items():
    (OUT / name).write_text('<svg height="24" viewBox="0 0 24 24" width="24" xmlns="http://www.w3.org/2000/svg">'
                            f'<title>Kimi</title><path fill="{color}" d="{d}"/></svg>\n')
    print("wrote", OUT / name)
