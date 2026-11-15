// 状态同步的修复与撤销（M1 第 7 块起在侧栏提示栏；M4a 抽出共用，docs/design.md 10.4 节决策 6）：先向后台要改动
// （不写文件），把问题、要改的文件、备份与 diff 给用户看，确认后再写入；确认之后文件又被改过（指纹不符）即放弃。
// 侧栏提示栏、设置的 Agent 页与首次运行引导共用。

import AppKit

enum HookRepairFlow {
    enum Action: String {
        case repair, uninstall
    }

    /// 「在 Codex 里打开 /hooks」（M4b 块 5 P1 方案 B）：由工作区设置；参数为优先的终端（nil 时自动找）与是否紧接修复之后。
    static var openCodexHooks: ((_ terminalID: String?, _ afterRepair: Bool) -> Void)?

    /// 提示框较窄时 `/hooks` 会在「/」后折行，看不出是一条命令：插入零宽的 U+2060（WORD JOINER）让它整体换行。只用于说明文字。
    static func keepHooksCommand(_ text: String) -> String {
        text.replacingOccurrences(of: "/hooks", with: "/\u{2060}hooks")
    }

    /// `tools` 为 nil 时：修复为可修复或未信任、且用户没在设置里撤销过的工具（侧栏提示栏列出的那些，决策 6）；
    /// 撤销由后台按缺省选四款（docs/protocol.md 第 4 节 `hooks.plan`）。
    static func run(_ action: Action, tools: [String]? = nil, core: CoreClient) {
        core.request(["type": "hooks.status"]) { status in
            var chosen = tools
            if chosen == nil && action == .repair {
                let removed = Prefs.hooksRemoved
                chosen = ((status["hooks"] as? [[String: Any]]) ?? []).compactMap(SBHookStatus.init(json:))
                    .filter { ($0.fixable || $0.state == "untrusted") && !removed.contains($0.tool) }.map(\.tool)
            }
            var req: [String: Any] = ["type": "hooks.plan", "action": action.rawValue]
            if let chosen { req["tools"] = chosen }
            core.request(req) { plan in
                DispatchQueue.main.async { confirm(action, tools: chosen, status: status, plan: plan, core: core) }
            }
        }
    }

    /// 修复确认框里一条改动的说明（`hooks.plan` 的结构化 `steps`，docs/protocol.md 第 7 节；M2 第 6 块）。
    static func stepText(_ step: [String: Any]) -> String {
        let n = (step["count"] as? NSNumber)?.intValue ?? 0
        let names = ((step["names"] as? [String]) ?? []).joined(separator: String(localized: "、"))
        let path = step["path"] as? String ?? ""
        let restores = (step["restores"] as? NSNumber)?.intValue ?? 0
        let also = restores > 0 ? String(localized: "，同时恢复您自己的 \(restores) 条 Hook") : ""
        switch step["kind"] as? String {
        case "kimi_block_write": return String(localized: "在文件末尾写入本项目的标记块（\(n) 条 [[hooks]]）")
        case "kimi_block_remove": return String(localized: "删除本项目的标记块")
        case "grok_write": return String(localized: "写入 \(path)（\(n) 条）")
        case "grok_reenable": return String(localized: "重新启用 \(n) 条（从 disabled-hooks 删除本项目的行）")
        case "grok_delete": return String(localized: "删除 \(path)")
        case "opencode_write": return String(localized: "写入插件 \(path)")
        case "opencode_delete": return String(localized: "删除插件 \(path)")
        case "copilot_write": return String(localized: "写入 \(path)（\(n) 条）")
        case "copilot_enable_all": return String(localized: "打开全局开关（settings.json 的 disableAllHooks 改为 false）")
        case "copilot_delete": return String(localized: "删除 \(path)")
        case "antigravity_write": return String(localized: "在 \(path) 写入「scyllaui」一项（\(n) 条）；这份文件 Antigravity 的 App、IDE 也读，这几条只在 ScyllaUI 的终端里起作用")
        case "antigravity_enable": return String(localized: "重新启用「scyllaui」一项（去掉 \"enabled\": false）")
        case "antigravity_remove": return String(localized: "从 \(path) 删除「scyllaui」一项，您自己的 Hook 不动")
        case "append": return String(localized: "追加 \(n) 条：\(names)")
        case "rewrite": return String(localized: "\(n) 条改为当前写法")
        case "uninstall": return String(localized: "删除本项目的 \(n) 条")
        case "claude_enable_all": return String(localized: "打开全局开关（disableAllHooks 改为 false）") + also
        case "codex_feature":
            let keys = ((step["keys"] as? [String]) ?? []).joined(separator: String(localized: "、"))
            return String(localized: "打开 Hook 功能（config.toml 的 \(keys) 改为 true）") + also
        case "codex_reenable": return String(localized: "重新启用 \(n) 条：\(names)（config.toml 中该条的 enabled 改为 true）")
        case "codex_trust":
            return keepHooksCommand(step["after_repair"] as? Bool == true
                ? String(localized: "修复后需要在 Codex 里输入 /hooks，按提示信任 ScyllaUI 的 \(n) 条 Hook")
                : String(localized: "需要在 Codex 里输入 /hooks，按提示信任 ScyllaUI 的 \(n) 条 Hook"))
        default: return step["kind"] as? String ?? ""
        }
    }

    private static func confirm(_ action: Action, tools: [String]?, status: [String: Any], plan reply: [String: Any], core: CoreClient) {
        let uninstall = action == .uninstall
        let statuses = ((status["hooks"] as? [[String: Any]]) ?? []).compactMap(SBHookStatus.init(json:))
            .filter { $0.broken && (tools?.contains($0.tool) ?? true) }
        let plan = reply["plan"] as? [String: Any] ?? [:]
        let changes = plan["changes"] as? [[String: Any]] ?? []
        let home = NSHomeDirectory()
        func short(_ p: String) -> String { p.hasPrefix(home + "/") ? "~" + p.dropFirst(home.count) : p }
        var lines: [String] = []
        let writes = changes.filter { !(($0["files"] as? [[String: Any]]) ?? []).isEmpty }
        let names = writes.compactMap { $0["tool"] as? String }.map(SidebarModel.toolName).joined(separator: String(localized: "、"))
        if uninstall && !writes.isEmpty {
            // 写明撤销的是哪款；信任记录与 Hook 功能开关只有 Codex 有，只在撤销 Codex 时说（2026-11-01 用户指出各款都写了这句）。
            lines.append(String(localized: "只删除 ScyllaUI 写入的条目，您自己的 Hook 与其他设置不动。撤销后侧栏收不到 \(names) 的状态，只能显示运行或退出。"))
            if writes.contains(where: { $0["tool"] as? String == "codex" }) {
                lines.append(String(localized: "Codex 的信任记录与 Hook 功能开关保留。"))
            }
        } else if !uninstall {
            for s in statuses {
                let name = SidebarModel.toolName(s.tool)
                lines.append(String(localized: "\(name)：\(s.reason)"))
            }
        }
        if !changes.isEmpty && !lines.isEmpty { lines.append("") }
        for c in changes {
            let tool = SidebarModel.toolName(c["tool"] as? String)
            let steps = (c["steps"] as? [[String: Any]]) ?? []
            let texts = steps.isEmpty ? (c["actions"] as? [String]) ?? [] : steps.map(stepText)
            for a in texts { lines.append(String(localized: "• \(tool)：\(a)")) }
            for f in (c["files"] as? [[String: Any]]) ?? [] {
                guard let path = f["path"] as? String else { continue }
                let what = f["delete"] as? Bool == true ? String(localized: "删除") : f["create"] as? Bool == true ? String(localized: "新建") : String(localized: "改写")
                let name = (path as NSString).lastPathComponent
                let bak = f["backup"] as? Bool == true ? String(localized: "，改写前备份为 \(name).scyllaui-bak-<时间>") : ""
                lines.append("  \(what) \(short(path))\(bak)")
            }
        }
        let alert = NSAlert()
        if uninstall {
            alert.messageText = writes.isEmpty ? String(localized: "没有要撤销的条目") : String(localized: "撤销 \(names) 的状态同步？")
        } else {
            alert.messageText = writes.isEmpty ? String(localized: "状态同步未生效") : String(localized: "修复状态同步？")
        }
        alert.informativeText = lines.joined(separator: "\n")
        let diff = changes.compactMap { $0["diff"] as? String }.filter { !$0.isEmpty }.joined(separator: "\n")
        if !diff.isEmpty {
            let scroll = NSScrollView(frame: NSRect(x: 0, y: 0, width: 560, height: 220))
            scroll.hasVerticalScroller = true
            scroll.hasHorizontalScroller = true
            scroll.borderType = .bezelBorder
            let text = NSTextView(frame: scroll.bounds)
            text.isEditable = false
            text.font = NSFont.monospacedSystemFont(ofSize: 11, weight: .regular)
            text.string = diff
            text.isHorizontallyResizable = true
            text.textContainer?.widthTracksTextView = false
            text.textContainer?.containerSize = NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
            scroll.documentView = text
            alert.accessoryView = scroll
        }
        if writes.isEmpty {
            // 只剩 Codex 待信任（M4b 块 5 P1）：多一个按钮，去正在运行的 Codex 里打开 /hooks。
            let trust = !uninstall && openCodexHooks != nil && statuses.contains { $0.tool == "codex" && $0.state == "untrusted" }
            if trust { alert.addButton(withTitle: String(localized: "在 Codex 里打开 /hooks")) }
            alert.addButton(withTitle: String(localized: "好"))
            if alert.runModal() == .alertFirstButtonReturn, trust { openCodexHooks?(nil, false) }
            return
        }
        alert.addButton(withTitle: uninstall ? String(localized: "撤销") : String(localized: "修复"))
        alert.addButton(withTitle: String(localized: "取消"))
        alert.buttons.first?.hasDestructiveAction = uninstall
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        let chosen = writes.compactMap { $0["tool"] as? String }
        // 修复后 Codex 还要信任：写入成功后接着提示这一步（M4b 块 5 P1）。
        let trustAfter = !uninstall && changes.contains { (($0["steps"] as? [[String: Any]]) ?? []).contains { $0["kind"] as? String == "codex_trust" } }
        // 撤销的工具在写入之前记下，推送来的新状态（缺少条目）就不会在侧栏提示；失败时取消（docs/design.md 10.4 节决策 6）。
        let before = Prefs.hooksRemoved
        Prefs.hooksRemoved = uninstall ? before.union(chosen) : before.subtracting(chosen)
        let req: [String: Any] = ["type": "hooks.repair", "tools": chosen, "fingerprint": plan["fingerprint"] ?? "", "action": action.rawValue]
        core.request(req) { reply in
            let ok = reply["ok"] as? Bool == true
            let err = reply["error"] as? [String: Any]
            scyllauiLog.notice("hooks_\(action.rawValue, privacy: .public) tools=\(chosen.joined(separator: ","), privacy: .public) ok=\(ok) error=\((err?["code"] as? String) ?? "-", privacy: .public) backups=\(((reply["backups"] as? [String]) ?? []).joined(separator: ","), privacy: .public)")
            guard !ok else {
                if trustAfter { DispatchQueue.main.async { openCodexHooks?(nil, true) } }
                return
            }
            DispatchQueue.main.async {
                Prefs.hooksRemoved = before
                let a = NSAlert()
                if err?["code"] as? String == "conflict" {
                    a.messageText = String(localized: "配置文件刚被改动")
                    a.informativeText = uninstall
                        ? String(localized: "确认之后配置文件又有变化，没有写入。请重新点「撤销…」查看最新的改动。")
                        : String(localized: "确认之后配置文件又有变化，没有写入。请重新点「修复…」查看最新的改动。")
                } else {
                    a.messageText = uninstall ? String(localized: "撤销失败") : String(localized: "修复失败")
                    a.informativeText = (err?["message"] as? String) ?? String(localized: "后台没有给出原因。")
                }
                a.runModal()
            }
        }
    }
}
