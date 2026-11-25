// 状态栏（docs/design.md 5.12 节）：右侧栏的一栏，跟着当前终端显示环境（完整路径、分支）、上下文占比与「更改」一段（第 6 版 R3）。
// 读不到的写明「ScyllaUI 暂不读取」，不留空白（docs/ui-methodology.md 1.7 节）。

import AppKit

final class StatusPanel: NSView {
    /// 当前终端这一行的状态栏所需（主线程，取自 TaskRow）。
    struct Subject: Equatable {
        /// 工具键（`claude` 等）；Shell 为 nil。
        var tool: String?
        /// 终端此刻所在目录（没有上报时为启动目录）。
        var path: String
        /// 终端所在项目的分支；不在 Git 仓库里为 nil。
        var branch: String?
        /// 上下文占用（已用与上限都有才有）与这款工具 ScyllaUI 读不读（M4，2026-11-23）。
        var contextUsed: Int64? = nil
        var contextLimit: Int64? = nil
        var contextSupported = false
        /// 这款的上下文经额度桥（statusline）读、桥没接上时的说明（`bridgeNote`）；有它就不写「还没读到」。
        var contextNote: String? = nil
        /// 「更改」一段（第 6 版 R3）；nil 为不在 Git 仓库里，这一段不显示。
        var changes: Changes? = nil
    }

    /// 「更改」一段的内容，与「更改」栏同一份 `ChangesStore` 结果（同一个根、同一个时刻）。
    enum Changes: Equatable {
        /// 还没取到。
        case loading
        /// 读不到：后台的错误码，文字与「更改」栏相同（`DiffPanel.failure`）。
        case failed(String)
        /// 合计与「更改」栏头相同（列出的各项之和）；`files` 是列出的文件数（同栏头的数量），`more` 为没列出的项数。
        case counts(added: Int, deleted: Int, files: Int, more: Int)

        init(_ snap: ChangesStore.Snapshot?) {
            guard let snap else { self = .loading; return }
            if let code = snap.error { self = .failed(code); return }
            let t = snap.totals
            self = .counts(added: t.added, deleted: t.deleted, files: snap.items.count, more: snap.more)
        }
    }

    /// 点「更改」一行（或行尾按钮）：打开「更改」栏。
    var onOpenChanges: (() -> Void)?

    var onClose: (() -> Void)? {
        didSet { header.onClose = onClose }
    }
    /// nil 为没有终端。
    var subject: Subject? {
        didSet { if subject != oldValue { rebuild() } }
    }

    private let header = RightPanelHeader(symbol: "info.circle", title: String(localized: "状态"), closeName: String(localized: "关闭状态"))
    private let scroll = NSScrollView()
    private let stack = NSStackView()
    private let note = NSTextField(wrappingLabelWithString: "")

    override init(frame: NSRect) {
        super.init(frame: frame)
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 0
        stack.translatesAutoresizingMaskIntoConstraints = false
        let doc = FlippedView()
        doc.translatesAutoresizingMaskIntoConstraints = false
        doc.addSubview(stack)
        scroll.documentView = doc
        scroll.drawsBackground = false
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        note.font = ScyllaUIStyle.detailFont
        note.textColor = .secondaryLabelColor
        note.alignment = .center
        for v in [header, scroll, note] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            addSubview(v)
        }
        NSLayoutConstraint.activate([
            header.topAnchor.constraint(equalTo: topAnchor),
            header.leadingAnchor.constraint(equalTo: leadingAnchor),
            header.trailingAnchor.constraint(equalTo: trailingAnchor),
            scroll.topAnchor.constraint(equalTo: header.bottomAnchor, constant: 6),
            scroll.leadingAnchor.constraint(equalTo: leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: trailingAnchor),
            scroll.bottomAnchor.constraint(equalTo: bottomAnchor),
            doc.leadingAnchor.constraint(equalTo: scroll.contentView.leadingAnchor),
            doc.trailingAnchor.constraint(equalTo: scroll.contentView.trailingAnchor),
            doc.topAnchor.constraint(equalTo: scroll.contentView.topAnchor),
            stack.topAnchor.constraint(equalTo: doc.topAnchor),
            stack.leadingAnchor.constraint(equalTo: doc.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: doc.trailingAnchor),
            stack.bottomAnchor.constraint(equalTo: doc.bottomAnchor, constant: -12),
            note.topAnchor.constraint(equalTo: header.bottomAnchor, constant: 24),
            note.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 16),
            note.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -16),
        ])
        rebuild()
    }

    required init?(coder: NSCoder) { fatalError() }

    // MARK: 文案（纯函数，单测覆盖）

    static let noTerminal = String(localized: "没有终端")
    static let shellNote = String(localized: "当前是 Shell，在这个终端启动 Agent 后显示上下文用量。")
    static let notInRepo = String(localized: "不在 Git 仓库里")

    /// 上下文占用的百分比（四舍五入，限在 0—100）。
    static func percent(used: Int64, limit: Int64) -> Int {
        guard limit > 0 else { return 0 }
        return min(100, max(0, Int((Double(used) / Double(limit) * 100).rounded())))
    }

    /// 读得到这款的上下文、但还没读到时的说明。
    static func contextPending(tool: String) -> String {
        String(localized: "还没读到 \(PromptHistoryPanel.toolName(tool)) 的上下文用量，Agent 回复一轮后显示。")
    }

    /// 读不到这款上下文用量时的说明。
    static func contextUnread(tool: String) -> String {
        String(localized: "ScyllaUI 暂不读取 \(PromptHistoryPanel.toolName(tool)) 的上下文用量")
    }

    /// 上下文要经额度桥读、桥没接上时的说明（后台 Hook 状态的 `bridge`，只有经桥读的工具才有）：没接入写怎么接，被改掉写去哪里看；
    /// 接上了为 nil。
    static func bridgeNote(bridge: String?, tool: String) -> String? {
        let name = PromptHistoryPanel.toolName(tool)
        switch bridge {
        case nil, "on": return nil
        case "replaced", "broken":
            return String(localized: "\(name) 的上下文用量经额度桥（statusline）读，额度桥被改掉了，现在读不到。在「设置 › Agent」里查看并修复。")
        default:
            return String(localized: "\(name) 的上下文用量和额度一起经它的 statusline 交给 ScyllaUI，现在未接入。在「设置 › Agent」里点「接入额度…」，新开的 \(name) 会话生效。")
        }
    }

    /// 「更改」一行除 `+N −M` 外的文字：「· K 个文件」，没列全时再写「· 还有 N 项未列出」；没有数字时为说明文字。
    static func changesText(_ c: Changes) -> (counts: (added: Int, deleted: Int)?, text: String) {
        switch c {
        case .loading: return (nil, String(localized: "正在读取更改…"))
        case .failed(let code): return (nil, DiffPanel.failure(code))
        case .counts(let a, let d, let files, let more):
            if files == 0 && more == 0 { return (nil, String(localized: "没有更改")) }
            var s = " · " + String(localized: "\(files) 个文件")
            if more > 0 { s += " · " + String(localized: "还有 \(more) 项未列出") }
            return ((a, d), s)
        }
    }

    // MARK: 排版

    private func rebuild() {
        stack.arrangedSubviews.forEach { $0.removeFromSuperview() }
        guard let s = subject else {
            header.subtitle.stringValue = ""
            note.stringValue = Self.noTerminal
            note.isHidden = false
            scroll.isHidden = true
            return
        }
        note.isHidden = true
        scroll.isHidden = false
        header.subtitle.stringValue = s.tool.map(PromptHistoryPanel.toolName) ?? "Shell"
        add(section(String(localized: "环境")))
        let copy = RightPanelHeader.iconButton(symbol: "doc.on.doc", name: String(localized: "拷贝路径"), target: self, action: #selector(copyPath(_:)))
        let reveal = RightPanelHeader.iconButton(symbol: "folder", name: String(localized: "在访达中显示"), target: self, action: #selector(revealPath(_:)))
        add(row(symbol: "folder", text: s.path, selectable: true, buttons: [copy, reveal]))
        add(row(symbol: "arrow.triangle.branch", text: s.branch ?? Self.notInRepo, muted: s.branch == nil))
        add(section(String(localized: "上下文")))
        if let tool = s.tool {
            if let used = s.contextUsed, let limit = s.contextLimit {
                add(contextRow(used: used, limit: limit))
            } else {
                add(noteRow(s.contextNote ?? (s.contextSupported ? Self.contextPending(tool: tool) : Self.contextUnread(tool: tool))))
            }
        } else {
            add(noteRow(Self.shellNote))
        }
        // 「更改」跟项目走，Shell 终端也显示；不在仓库里不显示（上面分支一行已写「不在 Git 仓库里」）。
        if let c = s.changes {
            add(section(String(localized: "更改")))
            add(changesRow(c))
        }
    }

    /// 「更改」一行：图标、`+N −M`（绿、红）与次要色的「· K 个文件」或说明，行尾「在『更改』里查看」；整行可点。
    private func changesRow(_ c: Changes) -> NSView {
        let (counts, text) = Self.changesText(c)
        let s = NSMutableAttributedString()
        if let counts { s.append(DiffViewer.countsText(added: counts.added, deleted: counts.deleted)) }
        s.append(NSAttributedString(string: text, attributes: [.font: counts == nil ? ScyllaUIStyle.detailFont : ScyllaUIStyle.mono(11),
                                                              .foregroundColor: NSColor.secondaryLabelColor]))
        let open = RightPanelHeader.iconButton(symbol: "chevron.right", name: String(localized: "在「更改」里查看"), target: self, action: #selector(openChanges(_:)))
        let v = row(symbol: "plus.forwardslash.minus", text: "", buttons: [open])
        if let label = v.subviews.first(where: { $0 is NSTextField }) as? NSTextField { label.attributedStringValue = s }
        v.addGestureRecognizer(NSClickGestureRecognizer(target: self, action: #selector(openChanges(_:))))
        return v
    }

    @objc private func openChanges(_ sender: Any?) { onOpenChanges?() }

    /// 测试用：点「更改」一行。
    func clickChangesForTesting() { openChanges(nil) }

    private func add(_ v: NSView) {
        stack.addArrangedSubview(v)
        v.widthAnchor.constraint(equalTo: stack.widthAnchor).isActive = true
    }

    /// 段标题：灰色小标题，后接一条细分隔线（同侧栏的分区标题）。
    private func section(_ title: String) -> NSView {
        let v = NSView()
        let label = NSTextField(labelWithString: title)
        label.font = ScyllaUIStyle.sectionFont
        label.textColor = .secondaryLabelColor
        let rule = NSBox()
        rule.boxType = .separator
        for s in [label, rule] as [NSView] {
            s.translatesAutoresizingMaskIntoConstraints = false
            v.addSubview(s)
        }
        NSLayoutConstraint.activate([
            label.topAnchor.constraint(equalTo: v.topAnchor, constant: 12),
            label.bottomAnchor.constraint(equalTo: v.bottomAnchor, constant: -4),
            label.leadingAnchor.constraint(equalTo: v.leadingAnchor, constant: 12),
            rule.leadingAnchor.constraint(equalTo: label.trailingAnchor, constant: 8),
            rule.trailingAnchor.constraint(equalTo: v.trailingAnchor, constant: -12),
            rule.centerYAnchor.constraint(equalTo: label.centerYAnchor),
        ])
        return v
    }

    /// 一行：图标、可换行的文字、行尾按钮。
    private func row(symbol: String, text: String, muted: Bool = false, selectable: Bool = false, buttons: [NSButton] = []) -> NSView {
        let v = NSView()
        let icon = NSImageView(image: NSImage(systemSymbolName: symbol, accessibilityDescription: nil) ?? NSImage())
        icon.symbolConfiguration = .init(pointSize: 12, weight: .regular)
        icon.contentTintColor = .secondaryLabelColor
        let label = NSTextField(wrappingLabelWithString: text)
        label.font = ScyllaUIStyle.detailFont
        label.textColor = muted ? .tertiaryLabelColor : .labelColor
        label.isSelectable = selectable
        label.lineBreakMode = .byCharWrapping
        label.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        let tools = NSStackView(views: buttons)
        tools.spacing = 2
        for s in [icon, label, tools] as [NSView] {
            s.translatesAutoresizingMaskIntoConstraints = false
            v.addSubview(s)
        }
        NSLayoutConstraint.activate([
            icon.leadingAnchor.constraint(equalTo: v.leadingAnchor, constant: 12),
            icon.firstBaselineAnchor.constraint(equalTo: label.firstBaselineAnchor),
            icon.widthAnchor.constraint(equalToConstant: 16),
            label.topAnchor.constraint(equalTo: v.topAnchor, constant: 5),
            label.bottomAnchor.constraint(equalTo: v.bottomAnchor, constant: -5),
            label.leadingAnchor.constraint(equalTo: icon.trailingAnchor, constant: 6),
            tools.leadingAnchor.constraint(equalTo: label.trailingAnchor, constant: 4),
            tools.trailingAnchor.constraint(equalTo: v.trailingAnchor, constant: -8),
            tools.topAnchor.constraint(equalTo: v.topAnchor, constant: 2),
        ])
        return v
    }

    /// 上下文一行：百分比、进度条、「已用 X / 上限 Y tokens」。
    private func contextRow(used: Int64, limit: Int64) -> NSView {
        let pct = Self.percent(used: used, limit: limit)
        let v = NSView()
        let value = NSTextField(labelWithString: "\(pct)%")
        value.font = ScyllaUIStyle.mono(13, semibold: true)
        let detail = NSTextField(labelWithString: String(localized: "已用 \(StatusInfo.tokens(used)) / \(StatusInfo.tokens(limit)) tokens"))
        detail.font = ScyllaUIStyle.mono(11)
        detail.textColor = .tertiaryLabelColor
        let bar = NSProgressIndicator()
        bar.style = .bar
        bar.isIndeterminate = false
        bar.minValue = 0
        bar.maxValue = 100
        bar.doubleValue = Double(pct)
        bar.controlSize = .small
        bar.setAccessibilityLabel(String(localized: "上下文已用 \(pct)%"))
        for s in [value, detail, bar] as [NSView] {
            s.translatesAutoresizingMaskIntoConstraints = false
            v.addSubview(s)
        }
        NSLayoutConstraint.activate([
            value.topAnchor.constraint(equalTo: v.topAnchor, constant: 5),
            value.leadingAnchor.constraint(equalTo: v.leadingAnchor, constant: 12),
            bar.centerYAnchor.constraint(equalTo: value.centerYAnchor),
            bar.leadingAnchor.constraint(equalTo: value.trailingAnchor, constant: 10),
            bar.trailingAnchor.constraint(equalTo: v.trailingAnchor, constant: -12),
            detail.topAnchor.constraint(equalTo: value.bottomAnchor, constant: 3),
            detail.leadingAnchor.constraint(equalTo: value.leadingAnchor),
            detail.trailingAnchor.constraint(lessThanOrEqualTo: v.trailingAnchor, constant: -12),
            detail.bottomAnchor.constraint(equalTo: v.bottomAnchor, constant: -5),
        ])
        return v
    }

    /// 说明文字：次要色，与行文字左对齐。
    private func noteRow(_ text: String) -> NSView {
        let v = NSView()
        let label = NSTextField(wrappingLabelWithString: text)
        label.font = ScyllaUIStyle.detailFont
        label.textColor = .secondaryLabelColor
        label.translatesAutoresizingMaskIntoConstraints = false
        v.addSubview(label)
        NSLayoutConstraint.activate([
            label.topAnchor.constraint(equalTo: v.topAnchor, constant: 4),
            label.bottomAnchor.constraint(equalTo: v.bottomAnchor, constant: -4),
            label.leadingAnchor.constraint(equalTo: v.leadingAnchor, constant: 12),
            label.trailingAnchor.constraint(equalTo: v.trailingAnchor, constant: -12),
        ])
        return v
    }

    @objc private func copyPath(_ sender: Any?) {
        guard let path = subject?.path else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(path, forType: .string)
    }

    @objc private func revealPath(_ sender: Any?) {
        guard let path = subject?.path else { return }
        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
    }

    /// 读屏与测试用：栏里此刻显示的全部文字，按从上到下的顺序。
    var visibleTexts: [String] {
        if !note.isHidden { return [note.stringValue] }
        func texts(_ v: NSView) -> [String] {
            (v as? NSTextField).map { [$0.stringValue] } ?? v.subviews.flatMap(texts)
        }
        return stack.arrangedSubviews.flatMap(texts)
    }
}

/// 自上而下排的文档视图（滚动视图默认从下往上）。
private final class FlippedView: NSView {
    override var isFlipped: Bool { true }
}
