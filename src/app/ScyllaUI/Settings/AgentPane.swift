// 设置「Agent」（M4a，docs/design.md 10.4 节决策 6）：四款 CLI 的状态同步（Hook）各一行：图标与名称（下一行为版本与是否
// 已验证，M4b 块 6）、状态（4.7 节七种，下一行为配置文件路径，中间截断，悬停看全；2026-10-28 起铺满窗口宽度）与按钮：缺失或停用「修复…」、正常「撤销…」、未信任「去信任…」（在正在运行的 Codex 里
// 打开 /hooks，M4b 块 5 P1）、策略锁定等「说明…」、没装的不给按钮；未信任时另给「撤销…」（Codex 修复后要在 Codex 里信任才算
// 正常，期间也要能撤销）。
// 修复与撤销走 HookRepairFlow；随快照与 `hooks.upsert` 刷新。首次运行引导的 Hook 页用不带按钮的同一份列表（块 4）。

import AppKit

final class AgentPane: NSViewController {
    /// Hook 状态有更新（快照或 `hooks.upsert`），`userInfo["hooks"]` 为状态数组（WorkspaceController 发出）。
    static let hooksChanged = Notification.Name("ScyllaUIHooksChanged")
    private static let tools = ["claude", "codex", "kimi", "grok", "opencode", "copilot", "antigravity"]

    private let core: CoreClient
    private let showsButtons: Bool
    private var states: [String: NSTextField] = [:]
    private var paths: [String: NSTextField] = [:]
    private var buttons: [String: NSButton] = [:]
    private var undos: [String: NSButton] = [:]
    private var versions: [String: NSTextField] = [:]
    private var statuses: [String: SBHookStatus] = [:]
    private var grid: NSGridView?
    private var note: NSTextField?

    /// Hook 状态（`SBHookStatus`）有更新时调用，供引导页按状态改按钮。
    var onUpdate: (([SBHookStatus]) -> Void)?

    init(core: CoreClient, showsButtons: Bool = true) {
        self.core = core
        self.showsButtons = showsButtons
        super.init(nibName: nil, bundle: nil)
        title = "Agent"
        NotificationCenter.default.addObserver(forName: Self.hooksChanged, object: nil, queue: .main) { [weak self] n in
            self?.update(n.userInfo?["hooks"] as? [[String: Any]] ?? [])
        }
    }

    required init?(coder: NSCoder) { fatalError() }

    override func loadView() {
        var rows: [[NSView]] = []
        for tool in Self.tools {
            let icon = NSImageView(image: toolIcon(tool) ?? NSImage())
            icon.contentTintColor = .labelColor
            icon.translatesAutoresizingMaskIntoConstraints = false
            NSLayoutConstraint.activate([
                icon.widthAnchor.constraint(equalToConstant: ScyllaUIStyle.toolIconSize),
                icon.heightAnchor.constraint(equalToConstant: ScyllaUIStyle.toolIconSize),
            ])
            // 工具名下一行小字：版本与是否已验证（M4b 块 6），只在设置里显示，首次运行引导不显示。
            let version = NSTextField(labelWithString: "")
            version.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
            version.textColor = .secondaryLabelColor
            version.isHidden = true
            let label = NSStackView(views: [NSTextField(labelWithString: SidebarModel.toolName(tool))] + (showsButtons ? [version] : []))
            label.orientation = .vertical
            label.alignment = .leading
            label.spacing = 1
            let head = NSStackView(views: [icon, label])
            head.spacing = 6
            let state = NSTextField(labelWithString: "")
            let path = NSTextField(labelWithString: "")
            path.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
            path.textColor = .secondaryLabelColor
            path.lineBreakMode = .byTruncatingMiddle
            // 设置里（2026-10-28 用户要求，原先内容缩在中间、路径常被截断）：三列——名称与版本、状态与路径（两行）、按钮——铺满
            // 窗口宽度，多出的宽度都给中间一列；路径的压缩阻力低于窗口尺寸计算用的优先级，再长也不撑大窗口，放不下时中间省略。
            // 引导页（同日）：一行三列——名称、状态、路径——铺满内容列，多出的宽度都给路径一列，路径尽量显示全。
            for s in [head, label] { s.setHuggingPriority(.defaultHigh, for: .horizontal) }
            var detail: NSStackView?
            if showsButtons {
                let d = NSStackView(views: [state, path])
                d.orientation = .vertical
                d.alignment = .leading
                d.spacing = 1
                // 栈视图没有固有尺寸，内容拉伸优先级不起作用，要用它自己的 setHuggingPriority。
                d.setHuggingPriority(NSLayoutConstraint.Priority(1), for: .horizontal)
                path.setContentCompressionResistancePriority(
                    NSLayoutConstraint.Priority(NSLayoutConstraint.Priority.fittingSizeCompression.rawValue - 1), for: .horizontal)
                detail = d
            } else {
                state.setContentHuggingPriority(.defaultHigh, for: .horizontal)
                path.setContentHuggingPriority(NSLayoutConstraint.Priority(1), for: .horizontal)
                path.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
            }
            let button = NSButton(title: "", target: self, action: #selector(act(_:)))
            button.bezelStyle = .rounded
            button.identifier = NSUserInterfaceItemIdentifier(tool)
            button.isHidden = true
            let undo = NSButton(title: String(localized: "撤销…"), target: self, action: #selector(undo(_:)))
            undo.bezelStyle = .rounded
            undo.identifier = NSUserInterfaceItemIdentifier(tool)
            undo.isHidden = true
            states[tool] = state
            paths[tool] = path
            buttons[tool] = button
            undos[tool] = undo
            versions[tool] = version
            let actions = NSStackView(views: [button, undo])
            actions.setHuggingPriority(.defaultHigh, for: .horizontal)
            rows.append(detail.map { [head, $0, actions] } ?? [head, state, path])
        }
        let grid = NSGridView(views: rows)
        grid.rowSpacing = 14
        grid.columnSpacing = 14
        for i in 0..<grid.numberOfRows { grid.row(at: i).yPlacement = .center }
        // 按钮列右对齐（2026-10-28）：只有一个按钮的行与 Codex 待信任时的两个按钮右边缘对齐。
        if showsButtons { grid.column(at: 2).xPlacement = .trailing }
        let note = NSTextField(wrappingLabelWithString: String(localized: "ScyllaUI 靠 Hook 得知各款 Agent 的状态：在它们的配置文件里写入几条命令，Agent 运行时把状态告诉侧栏。修复与撤销前都会先列出改动，写入前备份原文件。"))
        note.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        note.textColor = .secondaryLabelColor
        note.preferredMaxLayoutWidth = 520
        self.grid = grid
        if showsButtons { self.note = note }
        let stack = NSStackView(views: showsButtons ? [grid, note] : [grid])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 16
        grid.widthAnchor.constraint(equalTo: stack.widthAnchor).isActive = true
        if showsButtons { note.widthAnchor.constraint(equalTo: stack.widthAnchor).isActive = true }
        // 引导页里列表与上方说明左对齐，宽度由引导页给定，不加设置页的左右边距；上下各留 12 pt，与说明、Codex 提醒分开。
        if !showsButtons { stack.edgeInsets = NSEdgeInsets(top: 12, left: 0, bottom: 12, right: 0) }
        view = showsButtons ? AppearancePane.padded(stack, fill: true) : stack
    }

    /// 说明按网格的实际宽度折行（设置里网格铺满窗口，宽度由窗口决定）。只在本页显示在窗口里时更新：设置窗口按各页的
    /// 首选尺寸定大小时，不在显示的分页宽度很窄，按它折行会算出过高的高度、把窗口撑高。
    override func viewDidLayout() {
        super.viewDidLayout()
        guard view.window != nil, let grid, let note, grid.frame.width >= 300 else { return }
        if note.preferredMaxLayoutWidth != grid.frame.width { note.preferredMaxLayoutWidth = grid.frame.width }
    }

    override func viewWillAppear() {
        super.viewWillAppear()
        core.request(["type": "hooks.status"]) { [weak self] reply in
            let hooks = reply["hooks"] as? [[String: Any]] ?? []
            DispatchQueue.main.async { self?.update(hooks) }
        }
    }

    static func stateText(_ s: SBHookStatus) -> String {
        switch s.state {
        case "ok": return String(localized: "正常")
        case "missing": return String(localized: "缺少条目")
        case "disabled": return String(localized: "已停用")
        case "untrusted": return String(localized: "需在 Codex 中信任")
        case "policy": return String(localized: "被组织策略锁定")
        case "unreadable": return String(localized: "读不了配置")
        case "absent": return String(localized: "未安装此 Agent")
        default: return s.state
        }
    }

    private func update(_ hooks: [[String: Any]]) {
        guard isViewLoaded else { return }
        for s in hooks.compactMap(SBHookStatus.init(json:)) {
            guard let state = states[s.tool], let path = paths[s.tool], let button = buttons[s.tool], let undo = undos[s.tool] else { continue }
            statuses[s.tool] = s
            state.stringValue = Self.stateText(s)
            state.textColor = s.broken ? .systemOrange : .secondaryLabelColor
            state.toolTip = s.broken ? s.reason : nil
            let d = hooks.first { $0["tool"] as? String == s.tool } ?? [:]
            let p = (d["path"] as? String) ?? ""
            path.stringValue = (p as NSString).abbreviatingWithTildeInPath
            path.toolTip = p
            // 正在运行的版本优先，没在运行时为最近一次对话的版本；都没有不显示（docs/protocol.md 第 5 节「Hook 状态」）。
            if let version = versions[s.tool] {
                let v = d["version"] as? String
                let verified = d["version_verified"] as? Bool == true
                version.stringValue = v.map { "\($0) · " + (verified ? String(localized: "已验证") : String(localized: "未验证")) } ?? ""
                version.toolTip = d["version_running"] as? Bool == true
                    ? String(localized: "正在终端里运行的版本。「已验证」表示 ScyllaUI 测过这个版本的状态同步；未验证的版本通常也能正常使用。")
                    : String(localized: "最近一次对话时的版本。「已验证」表示 ScyllaUI 测过这个版本的状态同步；未验证的版本通常也能正常使用。")
                version.isHidden = v == nil
            }
            switch s.state {
            case "ok": button.title = String(localized: "撤销…")
            case "missing" where s.fixable, "disabled" where s.fixable: button.title = String(localized: "修复…")
            case "untrusted": button.title = String(localized: "去信任…")
            case "absent": button.title = ""
            default: button.title = String(localized: "说明…")
            }
            button.isHidden = button.title.isEmpty
            undo.isHidden = s.state != "untrusted"
        }
        onUpdate?(Array(statuses.values))
        (parent as? SettingsTabController)?.fitWindow()
    }

    @objc private func act(_ sender: NSButton) {
        guard let tool = sender.identifier?.rawValue, let s = statuses[tool] else { return }
        switch s.state {
        case "ok": HookRepairFlow.run(.uninstall, tools: [tool], core: core)
        case "missing" where s.fixable, "disabled" where s.fixable: HookRepairFlow.run(.repair, tools: [tool], core: core)
        // 未信任：去正在运行的 Codex 里打开 /hooks；工作区没设置时退回修复流程的说明（没有要写的文件）。
        case "untrusted":
            if let open = HookRepairFlow.openCodexHooks { open(nil, false) } else { HookRepairFlow.run(.repair, tools: [tool], core: core) }
        default:
            let alert = NSAlert()
            alert.messageText = String(localized: "\(SidebarModel.toolName(tool)) 的状态同步未生效")
            alert.informativeText = s.reason
            guard let window = view.window else { return }
            alert.beginSheetModal(for: window)
        }
    }

    @objc private func undo(_ sender: NSButton) {
        guard let tool = sender.identifier?.rawValue else { return }
        HookRepairFlow.run(.uninstall, tools: [tool], core: core)
    }
}
