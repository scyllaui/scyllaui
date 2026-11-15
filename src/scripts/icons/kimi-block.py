# -*- coding: utf-8 -*-
"""生成 Kimi 工具图标 tool-kimi.svg / tool-kimi-dark.svg（docs/design.md 10.6 节 D12）：蓝色横长方块，两只相对的 > < 眼睛。
2026-11-15 用户改定：方块宽占满 24 格、宽高比约 1.41；眼睛为左 >、右 <，颜色与外观反差（浅色外观深色眼、深色外观白眼）。
开发用，不进入产品运行时。用法：python3 src/scripts/icons/kimi-block.py（直接写入 ToolIcons.xcassets/tool-kimi.imageset）"""
from pathlib import Path

OUT = Path(__file__).resolve().parents[2] / "app/ScyllaUI/ToolIcons.xcassets/tool-kimi.imageset"
# 方块：24 × 17.04，上下居中。
W, H = 24.0, 17.04
Y0 = (24 - H) / 2
# 眼睛中心：左右各距中线 4 格，高度在方块上部（方块顶边下 6.24 格）。
EYES = [(8.0, False), (16.0, True)]
EYE_Y = Y0 + 6.24
# > 的高、单臂水平宽、笔画水平粗：17 pt 下仍看得清。
EYE_H, ARM, STROKE = 5.28, 3.12, 2.352
# 浅色外观的蓝略深，免得在白底上发虚；眼睛与外观反差。
COLORS = {"tool-kimi.svg": ("#3a98f5", "#1d1f24"), "tool-kimi-dark.svg": ("#4ea8ff", "#ffffff")}


def chevron(cx, cy, flip):
    """以 (cx, cy) 为中心的 >；flip 为 True 时左右镜像成 <。"""
    a = cx - (ARM + STROKE) / 2
    b = a + ARM
    y0, y1 = cy - EYE_H / 2, cy + EYE_H / 2
    pts = [(a, y0), (b, cy), (a, y1), (a + STROKE, y1), (b + STROKE, cy), (a + STROKE, y0)]
    if flip:
        pts = [(2 * cx - x, y) for x, y in pts]
    return "M" + "L".join(f"{x:.3f} {y:.3f}" for x, y in pts) + "Z"


rect = f"M0 {Y0:.3f}L{W:.0f} {Y0:.3f}L{W:.0f} {Y0 + H:.3f}L0 {Y0 + H:.3f}Z"
eyes = "".join(chevron(cx, EYE_Y, flip) for cx, flip in EYES)
for name, (block, eye) in COLORS.items():
    (OUT / name).write_text('<svg height="24" viewBox="0 0 24 24" width="24" xmlns="http://www.w3.org/2000/svg">'
                            f'<title>Kimi</title><path fill="{block}" d="{rect}"/><path fill="{eye}" d="{eyes}"/></svg>\n')
    print("wrote", OUT / name)
