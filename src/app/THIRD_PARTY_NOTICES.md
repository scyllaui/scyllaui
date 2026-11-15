# 第三方代码声明

> 随 App 分发的完整清单是 `ScyllaUI/Resources/ThirdPartyNotices.txt`，由 `src/scripts/gen-third-party.py` 生成（M4b 块 4）。本文件只记开发侧的来源与改动细节。

## Ghostty

- 来源：https://github.com/ghostty-org/ghostty ，固定 commit `34f39002c6e3974b54e6a6d400bd83777c5ea558`
- 用途：链接 libghostty（GhosttyKit.xcframework）；打包其 terminfo 与 shell 集成脚本；`ScyllaUI/Ghostty/` 下的 Swift 文件移植自其 macOS 版源码，各文件头注明来源文件。

```text
MIT License

Copyright (c) 2024 Mitchell Hashimoto, Ghostty contributors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## lobe-icons（工具图标）

- 来源：npm 包 `@lobehub/icons-static-svg@1.95.1`（https://github.com/lobehub/lobe-icons ），压缩包 SHA-1 `7c6281bf457230b5cfc3b7c2cdd15d3b8d8ab479` 与 npm 仓库一致；`tool-opencode` 取自同一包的 1.101.0（2026-10-29，SHA-1 `58ee9c600e3ca19c82da8364d07e0b9145dfb953`，与 npm 仓库登记的一致）；许可证全文取自该仓库 `LICENSE`（包内未附）。
- 用途：`ScyllaUI/ToolIcons.xcassets` 中的 `tool-grok`（取自 `grok.svg`）、`tool-opencode`（取自 `opencode.svg`，OpenCode 的方块标志，`docs/design.md` 10.6 节 D13）、`tool-copilot`（取自 1.101.0 的 `github.svg`，GitHub 的 Invertocat，D13）三个单色图标。`tool-claude` 曾取自 `claudecode.svg`（Claude Code 的像素小机器人），2026-10-30 起改为本项目自绘的不对称墨迹（`docs/design.md` 10.6 节 D12，由 `src/scripts/icons/claude-splash.py` 生成），不再取自 lobe-icons，也不含 Anthropic 的标志。为放入 Xcode 资源目录，宽高由 `1em` 改为 `24`、填充色由 `currentColor` 改为 `#000`、去掉 `style` 属性；路径数据未改。`tool-codex` 2026-10-11 曾改用 `openai.svg`（当时 `codex.svg` 显示成色块），2026-10-27 用户要求换回 `codex.svg`：查明原因是其圆弧命令的标志位紧挨后面的数字（如 `0 013.046`），系统 SVG 渲染器（CoreSVG）读错参数个数、把这些圆弧画成直线；路径数据只改为显式分隔的写法，命令与数值不变；2026-11-02 起 `tool-codex` 改为按用户提供的参考图描出的紫色圆润色块加 `>_`（`docs/design.md` 10.6 节 D12，由 `src/scripts/icons/codex-blob.py` 生成），不再取自 lobe-icons，也不含 OpenAI 的标志。`tool-kimi` 曾取自 `moonshot.svg`（月之暗面标志），2026-10-25 改为按 Kimi.app 图标描出的「K + 蓝点」，2026-10-27 起改为本项目自绘的新月加三颗四角星（`docs/design.md` 10.6 节 D12），不再取自 lobe-icons，也不含任何公司的标志。
- `tool-antigravity` 不取自 lobe-icons：按用户提供的参考图（彩色像素拱形「A」）逐格取色画成 12 × 10 格像素图（D13），形状与配色与 Google Antigravity 的标志相近。
- 商标：图标所示标志的商标归各自所有者（xAI、OpenCode 项目、GitHub、Google）。ScyllaUI 只用它们标识对应的命令行工具，不表示与这些公司有关联或获得认可。

```text
MIT License

Copyright (c) 2023 LobeHub

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```
