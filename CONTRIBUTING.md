# Contributing to ScyllaUI

[中文](#为-scyllaui-做贡献)

Thanks for your interest in ScyllaUI. This guide explains how to report problems, propose changes, and sign the Contributor License Agreement (CLA).

## Reporting problems

- **Bugs and feature requests**: open an issue at <https://github.com/scyllaui/scyllaui/issues>. In the app, **Help › Report a Problem…** prepares a diagnostic bundle on your Mac; review it before you attach it.
- **Security vulnerabilities**: do not open a public issue. Follow [SECURITY.md](SECURITY.md).

## Proposing changes

1. For anything larger than a small fix, open an issue first so we can agree on the approach.
2. Read [README.md](README.md) and [docs/usage.md](docs/usage.md) (user-facing behavior). Design notes are kept by the maintainer and are not published here; ask in an issue before larger changes.
3. Keep each pull request focused on one change. Match the existing style; do not reformat unrelated code.
4. Run the checks that apply to your change:
   - `src/scripts/with-rust.sh cargo test --workspace --manifest-path src/core/Cargo.toml` for the Rust core;
   - `src/scripts/test-app.sh` and `python3 src/scripts/l10n-check.py` for the app;
   - terminal or agent compatibility changes must also be checked in a real ScyllaUI window, side by side with Ghostty.app.
5. Every user-facing string needs both Chinese (Simplified) and English.
6. Commit messages start with a prefix such as `feat:`, `fix:`, `docs:`, `refactor:` or `chore:` and describe what changed and why.

ScyllaUI supports Apple silicon Macs only.

## Contributor License Agreement

Before we can merge your first pull request, you need to accept the [ScyllaUI Individual Contributor License Agreement](CLA.md). In short:

- you keep the copyright in your contribution;
- you grant the maintainer a broad, irrevocable license (including a patent license) to use and relicense it, so the project can, for example, offer the app through app stores or change its license in the future;
- in return, the maintainer commits that **any software that includes your contribution, whether distributed by the maintainer or by anyone the maintainer licenses, will have its complete source code published under GPL-3.0-or-later or another open-source license no later than its first distribution** (CLA Section 4).

**How to sign**: when you open your first pull request, the CLA Assistant bot comments on it with a link. Open the link, sign in with GitHub, and click to accept. You only need to do this once; it covers that pull request and all your later contributions under the same CLA version.

If you contribute on behalf of an employer, make sure you have its permission (CLA Section 5).

## License

By contributing, you agree that your contributions are distributed under the project's license, the [GNU General Public License v3.0 or later](LICENSE), in addition to the rights granted in the CLA. The name "ScyllaUI", its logos, and original artwork such as the app icon and splash artwork are not licensed under the GPL; see [ARTWORK.md](ARTWORK.md).

---

# 为 ScyllaUI 做贡献

感谢您关注 ScyllaUI。本文说明如何报告问题、提交改动，以及如何签署贡献者许可协议（CLA）。

## 报告问题

- **缺陷与功能建议**：在 <https://github.com/scyllaui/scyllaui/issues> 提交。App 里的「帮助 › 报告问题…」会在您的 Mac 上生成诊断包，附上前请先看过内容。
- **安全漏洞**：不要公开提交，按 [SECURITY.md](SECURITY.md) 私下报告。

## 提交改动

1. 小修以外的改动，请先开一个问题讨论做法。
2. 先读 [README.md](README.md) 与 [docs/usage.md](docs/usage.md)（用户看得到的行为）。设计文档由维护者保存，不随本仓库公开；较大的改动请先开 Issue 讨论。
3. 每个拉取请求只做一件事，沿用现有风格，不顺手格式化无关代码。
4. 按改动范围运行检查：
   - Rust 内核：`src/scripts/with-rust.sh cargo test --workspace --manifest-path src/core/Cargo.toml`；
   - App：`src/scripts/test-app.sh` 与 `python3 src/scripts/l10n-check.py`；
   - 涉及终端或 Agent 兼容的改动，还要在 ScyllaUI 的真实窗口里与 Ghostty.app 同条件对照验证。
5. 面向用户的文字都要有简体中文与英文。
6. 提交说明以 `feat:`、`fix:`、`docs:`、`refactor:`、`chore:` 等开头，写清改了什么、为什么改。

ScyllaUI 只支持 Apple 芯片的 Mac。

## 贡献者许可协议

合并您的第一个拉取请求前，需要您接受《[ScyllaUI 个人贡献者许可协议](CLA.md)》（以英文版为准，附中文译本）。简单说：

- 贡献的著作权仍归您；
- 您授予维护者广泛、不可撤销的许可（含专利许可），可以使用并再许可您的贡献，例如通过应用商店提供 App，或将来调整项目的许可；
- 作为回报，维护者承诺：**凡包含您贡献的软件，无论由维护者还是由维护者授权的任何人分发，最迟在首次分发时都会以 GPL-3.0-or-later 或其他开源许可公开完整源码**（CLA 第 4 条）。

**签署方式**：提交第一个拉取请求后，CLA Assistant 机器人会在下面留言并附链接。打开链接，用 GitHub 登录，点击同意即可。只需签一次，这个拉取请求以及同一版本 CLA 下以后的贡献都适用。

代表雇主做贡献的，请先取得雇主同意（CLA 第 5 条）。

## 许可

您的贡献除按 CLA 授予的权利外，也随本项目以 [GNU 通用公共许可证第 3 版或更高版本](LICENSE) 分发。「ScyllaUI」名称与标志、App 图标与开屏图等原创美术作品不随 GPL 授权，见 [ARTWORK.md](ARTWORK.md) 。
