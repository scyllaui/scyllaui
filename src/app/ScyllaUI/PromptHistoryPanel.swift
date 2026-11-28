// 历史提示词（M4a 追加块 9，docs/design.md 10.5 节决策 10、19、20）：主窗口右侧一栏，列出当前终端（或侧栏选中的已结束对话）
// 里用户发过的提示词。内容由后台按需从 CLI 自己的会话记录读出（`prompts.list`），不落库，显示前已脱敏。
// 2026-10-25 按用户意见改版：栏头为绿色「历史」符号与标题，第 2 行为工具全名 · 当前会话或已结束 · 完整会话 ID；
// 最新在上，编号 1、2、3…（最早为 1），左侧细连线同侧栏；每条默认收起（最多 2 行），点箭头展开全文；右侧拷贝常显与缩略时间，
// 「填入当前终端」在悬停或选中时出现；双击填入、Cmd+C 拷贝；经「最近任务」恢复过的按会话分段。2026-11-23 起是右侧栏的一栏，
// 底色、分隔线与左边缘拖宽由 RightPanelHost 负责（docs/design.md 5.12 节）。

import AppKit

final class PromptHistoryPanel: NSView, NSTableViewDataSource, NSTableViewDelegate, NSMenuItemValidation {
    struct Item: Equatable {
        let text: String
        let at: Int64?
        let conversationID: Int64
    }

    private enum Row {
        /// `number` 为由旧到新的编号（最早为 1），`key` 标识这一条的展开状态。
        case item(Item, number: Int, key: String)
        case separator(String)
    }

    var onClose: (() -> Void)?
    var onCopy: ((String) -> Void)?
    var onInsert: ((String) -> Void)?
    /// 栏头第 2 行：工具键（`claude` 等）与是否已结束；没有对话时 `tool` 为 nil。会话 ID 取自应答。
    var subjectTool: String? {
        didSet { updateSubject() }
    }
    var subjectEnded = false {
        didSet { updateSubject() }
    }

    private let icon = NSImageView()
    private let title = NSTextField(labelWithString: String(localized: "历史提示词"))
    private let count = NSTextField(labelWithString: "")
    private let subjectLabel = NSTextField(labelWithString: "")
    private let sessionLabel = NSTextField(labelWithString: "")
    private let close = NSButton()
    private let filter = NSSearchField()
    private let note = NSTextField(wrappingLabelWithString: "")
    private let table = NSTableView()
    private let scroll = NSScrollView()
    private var items: [Item] = []
    private var segments: [Int64] = []
    private var sessionIDs: [Int64: String] = [:]
    private var truncated = false
    private var rows: [Row] = []
    private var expanded: Set<String> = []

    override init(frame: NSRect) {
        super.init(frame: frame)
        // 只画自己的范围：macOS 14 起视图默认不裁剪绘制，`draw` 收到的脏区可以超出边界，原来 `dirtyRect.fill()` 把底色涂满了
        // 整个终端区，栏一打开终端整块看不见（2026-10-25 实测，图层树里这一栏的内容层宽 1103 而栏宽 320）。
        clipsToBounds = true
        icon.image = NSImage(systemSymbolName: "clock.arrow.circlepath", accessibilityDescription: String(localized: "历史提示词"))
        icon.symbolConfiguration = .init(pointSize: 13, weight: .semibold)
        icon.contentTintColor = ScyllaUIStyle.ready
        title.font = ScyllaUIStyle.sectionFont
        title.textColor = .labelColor
        count.font = ScyllaUIStyle.sectionFont
        count.textColor = .tertiaryLabelColor
        subjectLabel.font = ScyllaUIStyle.detailFont
        subjectLabel.textColor = .secondaryLabelColor
        subjectLabel.setContentHuggingPriority(.required, for: .horizontal)
        sessionLabel.font = ScyllaUIStyle.mono(11)
        sessionLabel.textColor = .tertiaryLabelColor
        sessionLabel.lineBreakMode = .byTruncatingMiddle
        sessionLabel.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        close.image = NSImage(systemSymbolName: "xmark", accessibilityDescription: String(localized: "关闭历史提示词"))
        close.symbolConfiguration = .init(pointSize: 11, weight: .semibold)
        close.bezelStyle = .accessoryBarAction
        close.isBordered = false
        close.contentTintColor = .secondaryLabelColor
        close.toolTip = String(localized: "关闭历史提示词")
        close.target = self
        close.action = #selector(closeClicked(_:))
        filter.placeholderString = String(localized: "筛选")
        filter.font = ScyllaUIStyle.detailFont
        filter.sendsSearchStringImmediately = true
        filter.target = self
        filter.action = #selector(filterChanged(_:))
        note.font = ScyllaUIStyle.detailFont
        note.textColor = .secondaryLabelColor
        note.alignment = .center
        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("prompt"))
        column.resizingMask = .autoresizingMask
        table.addTableColumn(column)
        table.headerView = nil
        table.style = .plain
        table.backgroundColor = .clear
        table.usesAutomaticRowHeights = true
        // 高亮由单元格自己画（连线右侧的圆角底色），行视图不画系统高亮。不能设成 `.none`：那样列表不再接受选中，
        // 选中后的「填入」与 Cmd+C 都用不了（2026-10-25 实测）。
        table.selectionHighlightStyle = .regular
        table.intercellSpacing = NSSize(width: 0, height: 0)
        table.columnAutoresizingStyle = .uniformColumnAutoresizingStyle
        table.dataSource = self
        table.delegate = self
        table.target = self
        table.doubleAction = #selector(rowDoubleClicked(_:))
        scroll.documentView = table
        scroll.drawsBackground = false
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        for v in [icon, title, count, close, subjectLabel, sessionLabel, filter, note, scroll] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            addSubview(v)
        }
        NSLayoutConstraint.activate([
            icon.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 12),
            icon.centerYAnchor.constraint(equalTo: title.centerYAnchor),
            title.topAnchor.constraint(equalTo: topAnchor, constant: 12),
            title.leadingAnchor.constraint(equalTo: icon.trailingAnchor, constant: 6),
            count.firstBaselineAnchor.constraint(equalTo: title.firstBaselineAnchor),
            count.leadingAnchor.constraint(equalTo: title.trailingAnchor, constant: 6),
            close.centerYAnchor.constraint(equalTo: title.centerYAnchor),
            close.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -10),
            close.widthAnchor.constraint(equalToConstant: 20),
            subjectLabel.topAnchor.constraint(equalTo: title.bottomAnchor, constant: 5),
            subjectLabel.leadingAnchor.constraint(equalTo: icon.leadingAnchor),
            // 会话 ID 单独一行：默认宽度下完整显示（用户要求信息要全），栏窄时中间省略、悬停看全文。
            sessionLabel.topAnchor.constraint(equalTo: subjectLabel.bottomAnchor, constant: 2),
            sessionLabel.leadingAnchor.constraint(equalTo: icon.leadingAnchor),
            sessionLabel.trailingAnchor.constraint(lessThanOrEqualTo: trailingAnchor, constant: -12),
            filter.topAnchor.constraint(equalTo: sessionLabel.bottomAnchor, constant: 10),
            filter.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 10),
            filter.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -10),
            scroll.topAnchor.constraint(equalTo: filter.bottomAnchor, constant: 8),
            scroll.leadingAnchor.constraint(equalTo: leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: trailingAnchor),
            scroll.bottomAnchor.constraint(equalTo: bottomAnchor),
            note.topAnchor.constraint(equalTo: filter.bottomAnchor, constant: 24),
            note.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 16),
            note.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -16),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    /// 宽度变了：正文折行宽度随之变化，重算行高。
    override func setFrameSize(_ newSize: NSSize) {
        let changed = abs(newSize.width - frame.width) > 0.5
        super.setFrameSize(newSize)
        if changed && !rows.isEmpty { table.reloadData() }
    }

    static func toolName(_ key: String) -> String {
        switch key {
        case "claude": return "Claude Code"
        case "codex": return "Codex"
        case "kimi": return "Kimi Code"
        case "grok": return "Grok Build"
        case "opencode": return "OpenCode"
        case "copilot": return "GitHub Copilot CLI"
        case "antigravity": return "Antigravity CLI"
        default: return key
        }
    }

    private func updateSubject() {
        guard let tool = subjectTool else {
            subjectLabel.stringValue = ""
            sessionLabel.stringValue = ""
            sessionLabel.toolTip = nil
            return
        }
        subjectLabel.stringValue = "\(Self.toolName(tool)) · " + (subjectEnded ? String(localized: "已结束") : String(localized: "当前会话"))
        let session = segments.last.flatMap { sessionIDs[$0] } ?? ""
        sessionLabel.stringValue = session
        sessionLabel.toolTip = session.isEmpty ? nil : session
    }

    /// 后台的 `prompts.list` 应答。列表变了才重排，滚到最新一条（最上面）。
    func show(reply: [String: Any]) {
        let list = (reply["items"] as? [[String: Any]] ?? []).compactMap { d -> Item? in
            guard let text = d["text"] as? String, let cid = (d["conversation_id"] as? NSNumber)?.int64Value else { return nil }
            return Item(text: text, at: (d["at"] as? NSNumber)?.int64Value, conversationID: cid)
        }
        let segInfo = reply["segments"] as? [[String: Any]] ?? []
        let segs = segInfo.compactMap { ($0["conversation_id"] as? NSNumber)?.int64Value }
        let ids = Dictionary(segInfo.compactMap { s -> (Int64, String)? in
            guard let c = (s["conversation_id"] as? NSNumber)?.int64Value, let n = s["native_id"] as? String else { return nil }
            return (c, n)
        }, uniquingKeysWith: { a, _ in a })
        let missing = segInfo.allSatisfy { ($0["available"] as? Bool) == false }
        // 这款工具不支持历史提示词（后台标 `supported: false`）：写明不支持，不说成记录已不在（docs/ui-methodology.md 1.7 节）。
        let unsupported = segInfo.last.flatMap { s in (s["supported"] as? Bool) == false ? s["tool"] as? String : nil }
        // 读取出错（`error`，如工具升级后记录格式变了）：说读取失败，同样不说成记录已不在。
        let failed = segInfo.last.flatMap { s in (s["error"] as? Bool) == true ? s["tool"] as? String : nil }
        let cut = reply["truncated"] as? Bool ?? false
        guard list != items || segs != segments || cut != truncated || ids != sessionIDs else { return }
        if segs != segments { expanded = [] }
        (items, segments, truncated, sessionIDs) = (list, segs, cut, ids)
        updateSubject()
        let empty = unsupported.map { String(localized: "\(Self.toolName($0)) 暂不支持历史提示词") }
            ?? failed.map { String(localized: "读取 \(Self.toolName($0)) 的会话记录失败，可能是它升级后格式变了；详情见日志文件夹里的 scyllauid.log") }
            ?? (missing ? String(localized: "会话记录已不在，读不到提示词") : String(localized: "这段对话还没有提示词"))
        rebuild(message: list.isEmpty ? empty : nil)
    }

    /// 这个终端没有可读的对话时的说明，按场景分（2026-10-30 用户要求）：前台是 ScyllaUI 认识的 AI（`tool`）但
    /// 还没对上会话——状态同步未生效（`unsynced`）时对不上，否则发出一条提示词后就对上；前台是不认识的命令（`command`）；Shell。
    /// 根本没有终端时与另三栏一样写「没有终端」（2026-11-28 补测 D14）。
    static func noConversationMessage(terminal: Bool, tool: String?, unsynced: Bool, command: String?) -> String {
        if !terminal { return StatusPanel.noTerminal }
        if let tool, unsynced { return String(localized: "\(toolName(tool)) 的状态没在同步，对不上它的会话；点侧栏这一行的「未同步」处理。") }
        if let tool { return String(localized: "还没对上 \(toolName(tool)) 的这次会话，发出一条提示词后显示。") }
        if let command { return String(localized: "前台运行的 \(command) 不是 ScyllaUI 已接入的 Agent，没有历史提示词。") }
        return String(localized: "当前是 Shell，在这个终端启动 Agent 后，这里显示它的历史提示词。")
    }

    /// 没有可读的对话或读取失败时的说明。
    func showMessage(_ text: String) {
        (items, segments, truncated, sessionIDs) = ([], [], false, [:])
        updateSubject()
        rebuild(message: text)
    }

    private func rebuild(message: String?) {
        let q = filter.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        rows = []
        var newer: Int64?
        for (i, item) in items.enumerated().reversed() {
            if !q.isEmpty && !item.text.localizedCaseInsensitiveContains(q) { continue }
            // 由新到旧：换到更早的一段会话时插一行说明，带那段的会话 ID。
            if let n = newer, n != item.conversationID {
                let id = sessionIDs[item.conversationID].map { " · \($0)" } ?? ""
                rows.append(.separator(String(localized: "以下为恢复前的会话") + id))
            }
            newer = item.conversationID
            rows.append(.item(item, number: i + 1, key: "\(item.conversationID):\(i)"))
        }
        let shown = rows.filter { if case .item = $0 { return true } else { return false } }.count
        count.stringValue = items.isEmpty ? "" : (q.isEmpty ? "\(items.count)" : "\(shown) / \(items.count)")
        if truncated && !items.isEmpty && q.isEmpty { rows.append(.separator(String(localized: "更早的提示词未显示"))) }
        note.stringValue = message ?? (rows.isEmpty && !items.isEmpty ? String(localized: "没有符合筛选的提示词") : "")
        note.isHidden = note.stringValue.isEmpty
        table.reloadData()
        if !rows.isEmpty { table.scrollRowToVisible(0) }
    }

    @objc private func filterChanged(_ sender: Any?) { rebuild(message: nil) }
    @objc private func closeClicked(_ sender: Any?) { onClose?() }

    private func item(at row: Int) -> Item? {
        guard row >= 0, row < rows.count, case .item(let item, _, _) = rows[row] else { return nil }
        return item
    }

    @objc private func rowDoubleClicked(_ sender: Any?) {
        if let item = item(at: table.clickedRow) { onInsert?(item.text) }
    }

    /// 「编辑 › 拷贝」（Cmd+C）：列表有焦点时拷贝选中的一条。
    @objc func copy(_ sender: Any?) {
        if let item = item(at: table.selectedRow) { onCopy?(item.text) }
    }

    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        menuItem.action == #selector(copy(_:)) ? item(at: table.selectedRow) != nil : true
    }

    func tableView(_ tableView: NSTableView, shouldSelectRow row: Int) -> Bool { item(at: row) != nil }

    func tableViewSelectionDidChange(_ notification: Notification) {
        table.enumerateAvailableRowViews { _, row in
            (table.view(atColumn: 0, row: row, makeIfNecessary: false) as? PromptCell)?.selected = table.isRowSelected(row)
        }
    }

    private func toggle(_ key: String) {
        if expanded.contains(key) { expanded.remove(key) } else { expanded.insert(key) }
        guard let row = rows.firstIndex(where: { if case .item(_, _, let k) = $0 { return k == key } else { return false } }) else { return }
        table.reloadData(forRowIndexes: IndexSet(integer: row), columnIndexes: IndexSet(integer: 0))
        table.noteHeightOfRows(withIndexesChanged: IndexSet(integer: row))
    }

    func numberOfRows(in tableView: NSTableView) -> Int { rows.count }

    func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? { PlainRowView() }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        let last = row == rows.count - 1
        switch rows[row] {
        case .separator(let text):
            let cell = (tableView.makeView(withIdentifier: PromptSeparatorCell.id, owner: nil) as? PromptSeparatorCell) ?? PromptSeparatorCell()
            cell.text.stringValue = text
            cell.isLast = last
            return cell
        case .item(let item, let number, let key):
            let cell = (tableView.makeView(withIdentifier: PromptCell.id, owner: nil) as? PromptCell) ?? PromptCell()
            cell.configure(text: item.text, number: number, time: item.at.map { PromptTime.short($0) } ?? "",
                           expanded: expanded.contains(key), width: bounds.width)
            cell.isLast = last
            cell.onToggle = { [weak self] in self?.toggle(key) }
            cell.onCopy = { [weak self] in self?.onCopy?(item.text) }
            cell.onInsert = { [weak self] in self?.onInsert?(item.text) }
            cell.selected = tableView.isRowSelected(row)
            return cell
        }
    }
}

/// 缩略时间：今天 / 昨天 `HH:mm`，今年 `M月d日 HH:mm`，更早 `yyyy/M/d`（随系统语言）。每次显示时按当时的日期算。
enum PromptTime {
    private static let clock = formatter("HHmm")
    private static let monthDay = formatter("MMMdHHmm")
    private static let full = formatter("yMd")

    private static func formatter(_ template: String) -> DateFormatter {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate(template)
        return f
    }

    static func short(_ ms: Int64, now: Date = Date(), calendar: Calendar = .current) -> String {
        let d = Date(timeIntervalSince1970: TimeInterval(ms) / 1000)
        if calendar.isDate(d, inSameDayAs: now) { return String(localized: "今天 \(clock.string(from: d))") }
        if let y = calendar.date(byAdding: .day, value: -1, to: now), calendar.isDate(d, inSameDayAs: y) {
            return String(localized: "昨天 \(clock.string(from: d))")
        }
        if calendar.isDate(d, equalTo: now, toGranularity: .year) { return monthDay.string(from: d) }
        return full.string(from: d)
    }
}

/// 历史提示词的一行：左侧细连线（竖线贯穿，最后一行只画到短横线处），与侧栏连线同色同粗。
private class PromptRowCell: HoverCellView {
    static let lineX: CGFloat = 14
    var isLast = false {
        didSet { needsDisplay = true }
    }
    /// 短横线的高度（本单元格坐标，自下而上）；nil 表示只画竖线。
    var tickY: CGFloat? { nil }

    override var isFlipped: Bool { true }

    override func draw(_ dirtyRect: NSRect) {
        let scale = window?.backingScaleFactor ?? 2
        let w = 1 / scale
        NSColor.separatorColor.setFill()
        let x = Self.lineX
        let bottom = isLast ? (tickY ?? bounds.midY) : bounds.maxY
        NSRect(x: x, y: 0, width: w, height: bottom).fill()
        if let y = tickY { NSRect(x: x, y: y, width: 8, height: w).fill() }
    }
}

/// 一条提示词：编号 · 展开箭头 · 正文（收起最多 2 行），右侧拷贝与缩略时间；悬停或选中时「填入」出现在时间的位置。
private final class PromptCell: PromptRowCell {
    static let id = NSUserInterfaceItemIdentifier("prompt")
    private static let bodyLeading: CGFloat = 64
    private let number = NSTextField(labelWithString: "")
    private let chevron = NSButton()
    private let body = NSTextField(wrappingLabelWithString: "")
    private let time = NSTextField(labelWithString: "")
    private let copy = NSButton()
    private let insert = NSButton()
    var onToggle: (() -> Void)?
    var onCopy: (() -> Void)?
    var onInsert: (() -> Void)?
    var selected = false {
        didSet { if selected != oldValue { hoverChanged() } }
    }

    override var tickY: CGFloat? { body.frame.minY + ScyllaUIStyle.subtitleLineHeight / 2 }

    override init(frame: NSRect) {
        super.init(frame: frame)
        identifier = Self.id
        number.font = .monospacedDigitSystemFont(ofSize: 12, weight: .regular)
        number.textColor = .secondaryLabelColor
        number.alignment = .right
        chevron.bezelStyle = .accessoryBarAction
        chevron.isBordered = false
        chevron.contentTintColor = .secondaryLabelColor
        chevron.target = self
        chevron.action = #selector(chevronClicked(_:))
        body.font = ScyllaUIStyle.detailFont
        body.textColor = .labelColor
        // 折行用 `.byWordWrapping`，末行超出由 `truncatesLastVisibleLine` 加「…」；设成 `.byTruncatingTail` 会退回单行。
        body.lineBreakMode = .byWordWrapping
        body.cell?.truncatesLastVisibleLine = true
        body.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        time.font = ScyllaUIStyle.mono(11)
        time.textColor = .tertiaryLabelColor
        time.setContentCompressionResistancePriority(.required, for: .horizontal)
        time.setContentHuggingPriority(.required, for: .horizontal)
        for (b, symbol, tip, action) in [(copy, "doc.on.doc", String(localized: "拷贝"), #selector(copyClicked(_:))),
                                         (insert, "text.insert", String(localized: "填入当前终端"), #selector(insertClicked(_:)))] {
            b.image = NSImage(systemSymbolName: symbol, accessibilityDescription: tip)
            b.symbolConfiguration = .init(pointSize: 12, weight: .regular)
            b.bezelStyle = .accessoryBarAction
            b.isBordered = false
            b.contentTintColor = .secondaryLabelColor
            b.toolTip = tip
            b.target = self
            b.action = action
        }
        insert.isHidden = true
        for v in [number, chevron, body, time, copy, insert] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            addSubview(v)
        }
        NSLayoutConstraint.activate([
            number.leadingAnchor.constraint(equalTo: leadingAnchor, constant: Self.lineX + 10),
            number.widthAnchor.constraint(equalToConstant: 22),
            number.firstBaselineAnchor.constraint(equalTo: body.firstBaselineAnchor),
            chevron.leadingAnchor.constraint(equalTo: number.trailingAnchor, constant: 2),
            chevron.widthAnchor.constraint(equalToConstant: 14),
            chevron.centerYAnchor.constraint(equalTo: number.centerYAnchor),
            body.topAnchor.constraint(equalTo: topAnchor, constant: 7),
            body.leadingAnchor.constraint(equalTo: leadingAnchor, constant: Self.bodyLeading),
            body.trailingAnchor.constraint(equalTo: copy.leadingAnchor, constant: -6),
            body.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -7),
            insert.leadingAnchor.constraint(equalTo: time.leadingAnchor),
            insert.centerYAnchor.constraint(equalTo: number.centerYAnchor),
            insert.widthAnchor.constraint(equalToConstant: 18),
            copy.trailingAnchor.constraint(equalTo: time.leadingAnchor, constant: -6),
            copy.centerYAnchor.constraint(equalTo: number.centerYAnchor),
            copy.widthAnchor.constraint(equalToConstant: 18),
            time.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -12),
            time.firstBaselineAnchor.constraint(equalTo: body.firstBaselineAnchor),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    /// `width` 为栏宽：自动行高按单元格的最适尺寸算，折行文本要给出实际可用宽度，否则只算出一行。
    func configure(text: String, number n: Int, time t: String, expanded: Bool, width: CGFloat) {
        number.stringValue = "\(n)"
        time.stringValue = t
        body.stringValue = text
        toolTip = text.count > 300 ? String(text.prefix(300)) + "…" : text
        let available = width - Self.bodyLeading - 6 - 18 - 6 - time.intrinsicContentSize.width - 12
        body.preferredMaxLayoutWidth = max(available, 60)
        // 超过两行才有箭头；收起最多两行。
        let measured = (text as NSString).boundingRect(with: NSSize(width: max(available - 4, 56), height: .greatestFiniteMagnitude),
                                                       options: [.usesLineFragmentOrigin], attributes: [.font: body.font as Any]).height
        let long = measured > ScyllaUIStyle.subtitleLineHeight * 2 + 1
        chevron.isHidden = !long
        chevron.image = NSImage(systemSymbolName: expanded ? "chevron.down" : "chevron.right",
                                accessibilityDescription: expanded ? String(localized: "收起") : String(localized: "展开"))
        chevron.symbolConfiguration = .init(pointSize: 10, weight: .semibold)
        chevron.toolTip = expanded ? String(localized: "收起") : String(localized: "展开")
        body.maximumNumberOfLines = long && expanded ? 0 : 2
    }

    override func draw(_ dirtyRect: NSRect) {
        // 悬停或选中：连线右侧的圆角底色。
        if hovered || selected {
            let r = NSRect(x: Self.lineX + 6, y: 2, width: bounds.width - Self.lineX - 12, height: bounds.height - 4)
            NSColor.labelColor.withAlphaComponent(selected ? 0.10 : 0.05).setFill()
            NSBezierPath(roundedRect: r, xRadius: 8, yRadius: 8).fill()
        }
        super.draw(dirtyRect)
    }

    override func hoverChanged() {
        insert.isHidden = !(hovered || selected)
        time.alphaValue = insert.isHidden ? 1 : 0
        needsDisplay = true
    }

    override func prepareForReuse() {
        super.prepareForReuse()
        selected = false
    }

    @objc private func chevronClicked(_ sender: NSButton) { onToggle?() }
    @objc private func copyClicked(_ sender: NSButton) { onCopy?() }
    @objc private func insertClicked(_ sender: NSButton) { onInsert?() }
}

/// 分段说明（恢复前的会话、更早的未显示）：次要文字，连线照常穿过。
private final class PromptSeparatorCell: PromptRowCell {
    static let id = NSUserInterfaceItemIdentifier("prompt-separator")
    let text = NSTextField(labelWithString: "")

    override init(frame: NSRect) {
        super.init(frame: frame)
        identifier = Self.id
        text.font = ScyllaUIStyle.mono(11)
        text.textColor = .tertiaryLabelColor
        text.lineBreakMode = .byTruncatingMiddle
        text.translatesAutoresizingMaskIntoConstraints = false
        addSubview(text)
        NSLayoutConstraint.activate([
            text.topAnchor.constraint(equalTo: topAnchor, constant: 6),
            text.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -6),
            text.leadingAnchor.constraint(equalTo: leadingAnchor, constant: Self.lineX + 10),
            text.trailingAnchor.constraint(lessThanOrEqualTo: trailingAnchor, constant: -12),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }
}

/// 不画系统选中高亮的行视图：选中照常记在列表里，样式由单元格画。
private final class PlainRowView: NSTableRowView {
    override func drawSelection(in dirtyRect: NSRect) {}
}

