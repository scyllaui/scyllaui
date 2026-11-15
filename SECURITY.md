# Security Policy

[中文](#安全策略)

## Supported versions

Security fixes are made for the latest release of ScyllaUI only. ScyllaUI runs on Apple silicon Macs; update through **ScyllaUI › Check for Updates…** or download the latest release from <https://github.com/scyllaui/scyllaui/releases>.

## Reporting a vulnerability

Please report vulnerabilities privately. Do not open a public issue, pull request or discussion.

- **Preferred**: use GitHub's private vulnerability reporting at <https://github.com/scyllaui/scyllaui/security/advisories/new>.
- **Email**: security@thinkfarlabs.com, with "ScyllaUI security" in the subject.

Include the ScyllaUI version (Settings › About), your macOS version, steps to reproduce, and the impact you observed. A diagnostic bundle from **Help › Report a Problem…** can help, but review its contents first.

## What to expect

- We acknowledge your report within 7 days.
- We keep you informed while we investigate, and agree with you on a disclosure date. We aim to release a fix within 90 days.
- With your permission, we credit you in the release notes.

## Scope

Areas of particular interest:

- the hook bridge (`scyllaui`) and the local socket between the app and the background service (`scyllauid`);
- the entries ScyllaUI writes into the configuration of supported AI CLIs, and their backups;
- the update mechanism (signature checks of downloaded updates);
- anything that lets another local user or a web page read terminal content or run commands.

Vulnerabilities in the AI CLIs themselves (Claude Code, Codex and others) should be reported to their vendors.

---

# 安全策略

## 支持的版本

安全修复只针对 ScyllaUI 的最新正式版。ScyllaUI 运行在 Apple 芯片的 Mac 上；请通过「ScyllaUI › 检查更新…」更新，或从 <https://github.com/scyllaui/scyllaui/releases> 下载最新版本。

## 报告漏洞

请私下报告，不要公开提交问题、拉取请求或讨论。

- **首选**：GitHub 私密漏洞报告 <https://github.com/scyllaui/scyllaui/security/advisories/new>。
- **邮件**：security@thinkfarlabs.com，标题写「ScyllaUI security」。

请写明 ScyllaUI 版本（「设置 › 关于」）、macOS 版本、复现步骤与观察到的影响。「帮助 › 报告问题…」生成的诊断包可能有帮助，附上前请先看过内容。

## 处理流程

- 7 天内确认收到。
- 调查期间告知进展，与您商定公开日期；力争 90 天内发布修复。
- 经您同意，在版本说明中致谢。

## 重点范围

- Hook 桥（`scyllaui`），以及 App 与后台（`scyllauid`）之间的本机 socket；
- ScyllaUI 写进各款 AI CLI 配置的条目及其备份；
- 更新机制（下载的更新包的签名校验）；
- 任何能让本机其他用户或网页读取终端内容、执行命令的问题。

AI CLI 本身（Claude Code、Codex 等）的漏洞，请报告给相应厂商。
