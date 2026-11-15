# -*- coding: utf-8 -*-
"""生成 Codex 工具图标 tool-codex.svg / tool-codex-dark.svg（docs/design.md 10.6 节 D12）：不规则圆润色块加 >_，替换原样引用的 OpenAI Codex 标志。
2026-11-02 用户给定两张参考图（同一紫色色块，>_ 一白一黑），深色外观用白 >_、浅色外观用黑 >_。开发用，不进入产品运行时。
色块轮廓由参考图描出（沿 24 个方向取边缘点、闭合 Catmull-Rom 转三次贝塞尔，与参考图最大偏差约 0.07 格），宽度占满 24 格画布、上下居中。
用法：python3 src/scripts/icons/codex-blob.py（直接写入 ToolIcons.xcassets/tool-codex.imageset）"""
from pathlib import Path

OUT = Path(__file__).resolve().parents[2] / "app/ScyllaUI/ToolIcons.xcassets/tool-codex.imageset"
BLOB = "M23.986 12.000C23.945 13.047 23.856 14.101 23.604 15.109C23.353 16.117 22.988 17.144 22.475 18.048C21.963 18.952 21.297 19.841 20.532 20.532C19.766 21.222 18.814 21.777 17.883 22.190C16.953 22.604 15.932 22.826 14.951 23.014C13.970 23.201 13.001 23.250 12.000 23.314C10.999 23.378 9.979 23.494 8.946 23.398C7.913 23.302 6.745 23.185 5.800 22.739C4.855 22.293 3.949 21.529 3.276 20.724C2.603 19.919 2.183 18.863 1.762 17.911C1.342 16.959 1.031 15.999 0.753 15.014C0.475 14.028 0.172 13.026 0.093 12.000C0.014 10.974 0.041 9.868 0.279 8.859C0.517 7.851 0.972 6.831 1.522 5.951C2.072 5.071 2.789 4.228 3.579 3.579C4.368 2.930 5.333 2.434 6.259 2.057C7.186 1.679 8.180 1.498 9.137 1.315C10.094 1.132 11.020 1.056 12.000 0.960C12.980 0.864 13.936 0.772 15.017 0.740C16.098 0.708 17.384 0.495 18.485 0.767C19.587 1.039 20.830 1.585 21.628 2.372C22.426 3.160 22.903 4.416 23.274 5.491C23.644 6.567 23.732 7.740 23.851 8.825C23.970 9.909 24.027 10.953 23.986 12.000Z"
# >_：圆头圆角笔画的中心线与笔画宽度，取自参考图。
PROMPT = "M5.554 9.024L7.502 12.507L5.554 15.991M11.973 16.032H17.074"
STROKE = 1.865
FILL = "#b492fd"
GLYPH = {"tool-codex.svg": "#1f1f1f", "tool-codex-dark.svg": "#ffffff"}

for name, glyph in GLYPH.items():
    (OUT / name).write_text('<svg height="24" viewBox="0 0 24 24" width="24" xmlns="http://www.w3.org/2000/svg"><title>Codex</title>'
                            f'<path fill="{FILL}" d="{BLOB}"/>'
                            f'<path fill="none" stroke="{glyph}" stroke-width="{STROKE}" stroke-linecap="round" stroke-linejoin="round" d="{PROMPT}"/></svg>\n')
    print("wrote", OUT / name)
