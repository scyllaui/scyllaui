// 侧栏单元格与行视图（视觉第二版，docs/design.md 第 5 节）。每种单元格的子视图和约束只在创建时建一次，
// 之后经 makeView(withIdentifier:) 复用，configure 只改文字、颜色与显隐。颜色与字号见 ScyllaUIStyle.swift。

import AppKit

/// 所有侧栏单元格：按行数据刷新内容。
protocol SidebarCell: NSView {
    func configure(_ row: SidebarRow)
}

private func label(_ font: NSFont, _ color: NSColor) -> NSTextField {
    let l = NSTextField(labelWithString: "")
    l.font = font
    l.textColor = color
    l.lineBreakMode = .byTruncatingTail
    l.translatesAutoresizingMaskIntoConstraints = false
    return l
}

/// 跟踪鼠标是否在单元格上（窗口为主窗口时），供行上的悬停按钮显隐（docs/design.md 10.5 节决策 4、7）。
/// 单元格被回收时清零。
class HoverCellView: NSTableCellView {
    private(set) var hovered = false {
        didSet { if hovered != oldValue { hoverChanged() } }
    }
    private var hoverArea: NSTrackingArea?

    /// 悬停状态变了；子类据此显隐按钮。
    func hoverChanged() {}

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let a = hoverArea { removeTrackingArea(a) }
        let a = NSTrackingArea(rect: .zero, options: [.mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect], owner: self)
        addTrackingArea(a)
        hoverArea = a
    }

    override func mouseEntered(with event: NSEvent) { hovered = true }
    override func mouseExited(with event: NSEvent) { hovered = false }

    override func prepareForReuse() {
        super.prepareForReuse()
        hovered = false
    }
}

/// 行上的小图标按钮（无边框、次要色），初始隐藏。
private func rowButton(_ symbol: String, _ tip: String) -> NSButton {
    let b = NSButton()
    b.image = NSImage(systemSymbolName: symbol, accessibilityDescription: tip)
    b.symbolConfiguration = .init(pointSize: 12, weight: .regular)
    b.bezelStyle = .accessoryBarAction
    b.isBordered = false
    b.contentTintColor = .secondaryLabelColor
    b.toolTip = tip
    b.isHidden = true
    b.translatesAutoresizingMaskIntoConstraints = false
    return b
}

/// 分区标题：「当前任务」「最近任务」+ 数量；「当前任务」右侧的「+」在侧栏里还没有的目录新建（选文件夹）。
final class SectionCell: NSTableCellView, SidebarCell {
    static let id = NSUserInterfaceItemIdentifier("section")
    private let title = label(ScyllaUIStyle.sectionFont, .secondaryLabelColor)
    private let count = label(ScyllaUIStyle.sectionFont, .tertiaryLabelColor)
    private let add = addButton(String(localized: "打开文件夹，新建终端…"))
    /// 点「+」时调用（参数为按钮）；为 nil 时不显示按钮。
    var onAdd: ((NSView) -> Void)? {
        didSet {
            add.isHidden = onAdd == nil
            needsDisplay = true
        }
    }

    override init(frame: NSRect) {
        super.init(frame: frame)
        identifier = Self.id
        add.target = self
        add.action = #selector(addClicked(_:))
        addSubview(title)
        addSubview(count)
        addSubview(add)
        NSLayoutConstraint.activate([
            add.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
            add.centerYAnchor.constraint(equalTo: title.centerYAnchor),
            add.widthAnchor.constraint(equalToConstant: 20),
            title.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 2),
            title.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -4),
            count.leadingAnchor.constraint(equalTo: title.trailingAnchor, constant: 6),
            count.firstBaselineAnchor.constraint(equalTo: title.firstBaselineAnchor),
            count.trailingAnchor.constraint(lessThanOrEqualTo: trailingAnchor, constant: -8),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    func configure(_ row: SidebarRow) {
        guard case let .section(t, n, more) = row else { return }
        title.stringValue = t
        count.stringValue = more ? "\(n)+" : "\(n)"
        needsDisplay = true
    }

    /// 分区细线的起止（本单元格坐标）：从数字文字右缘后 8 pt 起，有「+」时止于「+」图形左缘前 8 pt，否则止于右缘前 8 pt
    /// （docs/design.md 第 5 节「实现（维护期 0.3.5）」）。
    var ruleSpan: (from: CGFloat, to: CGFloat) {
        // 文字框左右各有一段内边距，按文字本身的右缘算。
        let from = count.frame.maxX - (count.frame.width - count.attributedStringValue.size().width) / 2 + 8
        let end = add.isHidden ? bounds.maxX
            : add.frame.minX + (add.cell?.imageRect(forBounds: add.bounds).minX ?? 0) + Self.plusInkInset
        return (from, end - 8)
    }

    /// 「+」符号图片左侧的留白（pt）：细线按看得见的「+」算。渲染一次测出，不写死。
    private static let plusInkInset: CGFloat = {
        guard let image = NSImage(systemSymbolName: "plus", accessibilityDescription: nil)?
                .withSymbolConfiguration(addSymbol) else { return 0 }
        let scale: CGFloat = 4
        let w = Int(image.size.width * scale), h = Int(image.size.height * scale)
        guard w > 0, h > 0,
              let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: w, pixelsHigh: h, bitsPerSample: 8,
                                         samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                                         bytesPerRow: 0, bitsPerPixel: 0) else { return 0 }
        rep.size = image.size
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
        image.draw(in: NSRect(origin: .zero, size: image.size))
        NSGraphicsContext.restoreGraphicsState()
        let column = (0..<w).first { x in (0..<h).contains { y in (rep.colorAt(x: x, y: y)?.alphaComponent ?? 0) > 0.3 } }
        return column.map { CGFloat($0) / scale } ?? 0
    }()

    override func layout() {
        super.layout()
        needsDisplay = true
    }

    /// 1 个物理像素高、对齐像素网格，与分区标题文字垂直居中，系统分隔线色。
    override func draw(_ dirtyRect: NSRect) {
        super.draw(dirtyRect)
        let (from, to) = ruleSpan
        guard to > from else { return }
        let scale = window?.backingScaleFactor ?? 2
        let y = (title.frame.midY * scale).rounded(.down) / scale
        NSColor.separatorColor.setFill()
        NSRect(x: from, y: y, width: to - from, height: 1 / scale).fill(using: .sourceOver)
    }

    @objc private func addClicked(_ sender: NSButton) { onAdd?(sender) }
}

/// 分支徽标：分支图标 + 分支名，Claude 橙文字与同色浅底（docs/design.md 第 5 节），过长时截断。
final class BranchView: NSView {
    private let icon = NSImageView()
    private let text = label(ScyllaUIStyle.badgeFont, ScyllaUIStyle.branch)

    var branch: String {
        get { text.stringValue }
        set { text.stringValue = newValue }
    }

    override init(frame: NSRect) {
        super.init(frame: frame)
        icon.image = NSImage(systemSymbolName: "arrow.triangle.branch", accessibilityDescription: String(localized: "分支"))
        icon.symbolConfiguration = .init(pointSize: 11, weight: .medium)
        icon.contentTintColor = ScyllaUIStyle.branch
        icon.translatesAutoresizingMaskIntoConstraints = false
        // 高于项目名（.defaultLow）：项目名过长时先截断项目名，徽标保留分支名（2026-10-11 试用发现徽标被压成只剩图标）。
        text.setContentCompressionResistancePriority(.defaultLow + 2, for: .horizontal)
        addSubview(icon)
        addSubview(text)
        let i = ScyllaUIStyle.badgeInsets
        NSLayoutConstraint.activate([
            icon.leadingAnchor.constraint(equalTo: leadingAnchor, constant: i.left - 2),
            icon.centerYAnchor.constraint(equalTo: centerYAnchor),
            icon.widthAnchor.constraint(equalToConstant: 12),
            text.leadingAnchor.constraint(equalTo: icon.trailingAnchor, constant: 3),
            text.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -i.right),
            text.topAnchor.constraint(equalTo: topAnchor, constant: i.top),
            text.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -i.bottom),
            widthAnchor.constraint(lessThanOrEqualToConstant: 130),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    override func draw(_ dirtyRect: NSRect) {
        ScyllaUIStyle.branchBg.setFill()
        NSBezierPath(roundedRect: bounds, xRadius: ScyllaUIStyle.radius, yRadius: ScyllaUIStyle.radius).fill()
    }
}

/// 工具图标（docs/design.md 第 5 节「布局」）：适配器资源 `tool-<工具键>`（ToolIcons.xcassets），取不到或 Shell
/// 用系统符号 `terminal`。按模板图着色，不写针对某款工具的分支。
func toolIcon(_ key: String?) -> NSImage? {
    key.flatMap { NSImage(named: "tool-\($0)") }
        ?? NSImage(systemSymbolName: "terminal", accessibilityDescription: "Shell")
}

/// 分区标题的「+」按钮，初始隐藏。
private let addSymbol = NSImage.SymbolConfiguration(pointSize: 13, weight: .semibold)

private func addButton(_ tip: String) -> NSButton {
    let b = NSButton()
    b.image = NSImage(systemSymbolName: "plus", accessibilityDescription: tip)
    b.symbolConfiguration = addSymbol
    b.bezelStyle = .accessoryBarAction
    b.isBordered = false
    b.contentTintColor = .secondaryLabelColor
    b.toolTip = tip
    b.isHidden = true
    b.translatesAutoresizingMaskIntoConstraints = false
    return b
}

/// 项目行：文件夹图标 + 名称（Ready 绿）+ 分支徽标；悬停显示目录与分支。不显示任务数（docs/design.md 10.5 节决策 7、9）。
/// 「当前任务」的项目行在右端有置顶按钮：未置顶时只在鼠标停在这一行时出现，已置顶时常显（实心书签），点按切换。
/// 按钮位置始终留着，悬停时名称与分支不移动。
final class ProjectCell: HoverCellView, SidebarCell {
    static let id = NSUserInterfaceItemIdentifier("project")
    /// 图标宽度；连线的竖线从图标中心向下（控制器据此定位）。
    static let iconWidth: CGFloat = 19
    private let icon = NSImageView()
    private let name = label(ScyllaUIStyle.projectFont, ScyllaUIStyle.ready)
    private let branch = BranchView()
    private let pin = rowButton("bookmark", String(localized: "置顶项目"))
    /// 置顶状态：nil 为不能置顶（「最近任务」区）。
    private var pinned: Bool?
    /// 点置顶按钮时调用，参数为目标状态；控制器只给「当前任务」的项目行设置。
    var onPin: ((Bool) -> Void)? {
        didSet { hoverChanged() }
    }

    override init(frame: NSRect) {
        super.init(frame: frame)
        identifier = Self.id
        // 次要文字色：蓝色在共用视觉规范里表示运行中 / 待查看（docs/design.md 第 5 节「实现（维护期 0.3.5）」）。
        pin.target = self
        pin.action = #selector(pinClicked(_:))
        icon.image = NSImage(systemSymbolName: "folder", accessibilityDescription: String(localized: "项目"))
        icon.contentTintColor = .secondaryLabelColor
        icon.symbolConfiguration = .init(pointSize: 15, weight: .regular)
        icon.translatesAutoresizingMaskIntoConstraints = false
        name.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        branch.setContentCompressionResistancePriority(.defaultLow + 1, for: .horizontal)
        branch.setContentHuggingPriority(.required, for: .horizontal)
        branch.translatesAutoresizingMaskIntoConstraints = false
        for v in [icon, name, branch, pin] as [NSView] { addSubview(v) }
        NSLayoutConstraint.activate([
            icon.leadingAnchor.constraint(equalTo: leadingAnchor),
            icon.centerYAnchor.constraint(equalTo: centerYAnchor),
            icon.widthAnchor.constraint(equalToConstant: Self.iconWidth),
            name.leadingAnchor.constraint(equalTo: icon.trailingAnchor, constant: 6),
            name.centerYAnchor.constraint(equalTo: centerYAnchor),
            branch.leadingAnchor.constraint(equalTo: name.trailingAnchor, constant: 6),
            branch.centerYAnchor.constraint(equalTo: name.centerYAnchor),
            pin.leadingAnchor.constraint(greaterThanOrEqualTo: branch.trailingAnchor, constant: 6),
            pin.centerYAnchor.constraint(equalTo: name.centerYAnchor),
            pin.widthAnchor.constraint(equalToConstant: 16),
            pin.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -10),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    /// 文件夹图标的位置（本单元格坐标），连线的竖线从它下方出发。
    var iconFrame: NSRect { icon.frame }

    func configure(_ row: SidebarRow) {
        guard case let .project(n, b, tip, _, pinned) = row else { return }
        name.stringValue = n
        branch.branch = b ?? ""
        branch.isHidden = b == nil
        toolTip = tip.isEmpty ? nil : tip
        self.pinned = pinned
        let on = pinned == true
        pin.image = NSImage(systemSymbolName: on ? "bookmark.fill" : "bookmark",
                            accessibilityDescription: on ? String(localized: "取消置顶") : String(localized: "置顶项目"))
        pin.toolTip = on ? String(localized: "已置顶，点按取消置顶") : String(localized: "置顶项目")
        hoverChanged()
    }

    override func hoverChanged() {
        pin.isHidden = !(pinned == true || (pinned == false && hovered && onPin != nil))
    }

    @objc private func pinClicked(_ sender: NSButton) {
        guard let pinned else { return }
        onPin?(!pinned)
    }
}

/// 空态（共用规范 7.3 节：次要色）。
final class NoteCell: NSTableCellView, SidebarCell {
    static let id = NSUserInterfaceItemIdentifier("note")
    private let text = label(ScyllaUIStyle.detailFont, .secondaryLabelColor)

    override init(frame: NSRect) {
        super.init(frame: frame)
        identifier = Self.id
        text.alignment = .center
        addSubview(text)
        NSLayoutConstraint.activate([
            text.leadingAnchor.constraint(equalTo: leadingAnchor),
            text.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -10),
            text.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    func configure(_ row: SidebarRow) {
        if case .empty(let s) = row { text.stringValue = s }
    }
}

/// 状态灯：连线上的节点。圆形（共用规范 1.6 节允许的例外）；「需确认」时外加一圈同色光晕。
final class LampView: NSView {
    /// 灯的直径；视图本身留出光晕的位置。
    static let dot: CGFloat = 8
    static let size: CGFloat = 14

    var lamp: Lamp = .idle {
        didSet { if lamp != oldValue { needsDisplay = true } }
    }

    override var intrinsicContentSize: NSSize { NSSize(width: Self.size, height: Self.size) }

    override func draw(_ dirtyRect: NSRect) {
        let color: NSColor
        switch lamp {
        case .idle: color = .tertiaryLabelColor
        case .ready: color = ScyllaUIStyle.ready
        case .running: color = ScyllaUIStyle.accent
        case .waiting: color = ScyllaUIStyle.waiting
        case .error: color = ScyllaUIStyle.error
        }
        let c = NSPoint(x: bounds.midX, y: bounds.midY)
        if lamp == .waiting {
            color.withAlphaComponent(0.3).setFill()
            NSBezierPath(ovalIn: NSRect(x: c.x - Self.size / 2, y: c.y - Self.size / 2, width: Self.size, height: Self.size)).fill()
        }
        color.setFill()
        NSBezierPath(ovalIn: NSRect(x: c.x - Self.dot / 2, y: c.y - Self.dot / 2, width: Self.dot, height: Self.dot)).fill()
    }
}

/// 徽标（共用规范 7.2 节）：语义底色 + 前景色，圆角 2，内边距 3 / 8。
/// 设了 `onClick` 的可点（「未同步」，M4b 块 5 P2）：点按不选中所在行，光标为手形。
final class BadgeView: NSView {
    private(set) var text = ""
    private var fg = NSColor.secondaryLabelColor
    private var bg = NSColor.quaternaryLabelColor
    var onClick: (() -> Void)? {
        didSet { window?.invalidateCursorRects(for: self) }
    }

    override func mouseDown(with event: NSEvent) {
        if let onClick { onClick() } else { super.mouseDown(with: event) }
    }

    override func resetCursorRects() {
        if onClick != nil { addCursorRect(bounds, cursor: .pointingHand) }
    }

    // 可点的徽标对辅助功能是一个按钮：读屏能读出文字并按下（2026-11-20；此前只有鼠标点得到）。不可点的仍不是独立元素。
    override func isAccessibilityElement() -> Bool { onClick != nil }
    override func accessibilityRole() -> NSAccessibility.Role? { .button }
    override func accessibilityLabel() -> String? { text }
    override func accessibilityPerformPress() -> Bool {
        guard let onClick else { return false }
        onClick()
        return true
    }

    func set(_ text: String, fg: NSColor, bg: NSColor) {
        self.text = text
        self.fg = fg
        self.bg = bg
        invalidateIntrinsicContentSize()
        needsDisplay = true
    }

    private var attributed: NSAttributedString {
        NSAttributedString(string: text, attributes: [.font: ScyllaUIStyle.badgeFont, .foregroundColor: fg])
    }

    override var intrinsicContentSize: NSSize {
        let s = attributed.size()
        let i = ScyllaUIStyle.badgeInsets
        return NSSize(width: ceil(s.width) + i.left + i.right, height: ceil(s.height) + i.top + i.bottom)
    }

    override func draw(_ dirtyRect: NSRect) {
        bg.setFill()
        NSBezierPath(roundedRect: bounds, xRadius: ScyllaUIStyle.radius, yRadius: ScyllaUIStyle.radius).fill()
        let i = ScyllaUIStyle.badgeInsets
        attributed.draw(at: NSPoint(x: i.left, y: i.bottom))
    }
}

/// 任务行（终端与最近任务共用）。第 1 行：状态灯 / 转圈 · 工具图标 · 工具名（终端有用户命名时为名称）· 状态词，右侧徽标；
/// 第 2 行：标题，右侧时间。
/// 「冲突」（M1 第 6 块）说的是这条对话，放第 2 行时间左侧：放第 1 行与「需确认」同时出现时，最小宽度下会挤压状态词。
/// 「目录无权限」（M3 的启动目录提示，0.3.5 起从终端上方的窄标题栏移来）同样放第 2 行时间左侧，悬停写明怎么处理；
/// 启动目录不存在时同一位置换成「目录不存在」（2026-11-13）。
/// 终端行可在第 1 行就地改名（M1 第 4 块；2026-10-25 名称从第 2 行移到第 1 行）：回车提交、Esc 取消。
/// 对话行（最近任务、搜索结果中已结束的）悬停时在第 1 行右端显示删除按钮（docs/design.md 10.5 节决策 4）。
/// 终端行前台 AI 的状态同步未生效时，第 1 行右侧显示可点的「未同步」（M4b 块 5 P2，docs/design.md 10.6 节）。
final class TaskCell: HoverCellView, SidebarCell, NSTextFieldDelegate {
    static let id = NSUserInterfaceItemIdentifier("task")
    private let lamp = LampView()
    private let spinner = NSProgressIndicator()
    private let icon = NSImageView()
    private let tool = label(ScyllaUIStyle.toolFont, .labelColor)
    private let state = label(ScyllaUIStyle.stateFont, .secondaryLabelColor)
    private let confirm = BadgeView()
    private let unseen = BadgeView()
    private let conflict = BadgeView()
    private let denied = BadgeView()
    private let unsynced = BadgeView()
    private let time = label(ScyllaUIStyle.detailFont, .tertiaryLabelColor)
    private let subtitle = label(ScyllaUIStyle.detailFont, .secondaryLabelColor)
    private let editor = NSTextField()
    private let remove = rowButton("trash", String(localized: "删除记录…"))
    private var renameDone: ((String?) -> Void)?
    private var running = false
    /// 副标题的全文与总要附上的一句（`TaskRow.fullText`、`note`），排版后按是否截断设悬停提示（`updateToolTips`）。
    private var fullText = ""
    private var note: String?
    /// 点删除按钮时调用；控制器只给对话行设置，终端行为 nil（不显示按钮）。
    var onDelete: (() -> Void)? {
        didSet { hoverChanged() }
    }
    /// 点「未同步」时调用；控制器只给终端行设置。
    var onUnsynced: (() -> Void)?
    private var subtitleHeight: NSLayoutConstraint!
    /// 副标题占几行高：侧栏固定两行（行距不跳）；欢迎页的样例副标题都只有一行，设为 1 使样例更紧凑，行高相应减一行。
    var subtitleLines = 2 {
        didSet {
            subtitle.maximumNumberOfLines = subtitleLines
            subtitleHeight.constant = ScyllaUIStyle.subtitleLineHeight * CGFloat(subtitleLines)
        }
    }

    override init(frame: NSRect) {
        super.init(frame: frame)
        identifier = Self.id
        remove.target = self
        remove.action = #selector(removeClicked(_:))
        spinner.style = .spinning
        spinner.controlSize = .small
        spinner.isDisplayedWhenStopped = false
        confirm.set(String(localized: "需确认"), fg: ScyllaUIStyle.waiting, bg: ScyllaUIStyle.waitingBg)
        unseen.set(String(localized: "待查看"), fg: ScyllaUIStyle.accent, bg: ScyllaUIStyle.accentBg)
        conflict.set(String(localized: "冲突"), fg: ScyllaUIStyle.error, bg: ScyllaUIStyle.errorBg)
        unsynced.onClick = { [weak self] in self?.onUnsynced?() }
        let badges = NSStackView(views: [unsynced, confirm, unseen, remove])
        badges.orientation = .horizontal
        badges.spacing = 4
        badges.setHuggingPriority(.required, for: .horizontal)
        badges.setContentCompressionResistancePriority(.required, for: .horizontal)
        // 第 1 行的名称可能是很长的任务标题（2026-11-01）：只占一行，放不下时直接裁掉、不加省略号（用户要求，同 GUI 版
        // Agent 工具，侧栏拉宽就多显示），状态词总是完整显示；被裁掉时悬停看全文（updateToolTips）。
        tool.lineBreakMode = .byClipping
        tool.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        state.setContentCompressionResistancePriority(.defaultLow + 1, for: .horizontal)
        time.setContentCompressionResistancePriority(.required, for: .horizontal)
        subtitle.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        time.alignment = .right
        // 副标题最多两行，超出在末尾加「…」（M1 第 6 块）。
        subtitle.usesSingleLineMode = false
        subtitle.maximumNumberOfLines = 2
        subtitle.lineBreakMode = .byWordWrapping
        subtitle.cell?.wraps = true
        subtitle.cell?.truncatesLastVisibleLine = true
        // 第 2 行右侧：「目录无权限」「冲突」与时间（隐藏的徽标不占位）。
        let tail = NSStackView(views: [denied, conflict, time])
        tail.orientation = .horizontal
        tail.alignment = .centerY
        tail.spacing = 6
        tail.setHuggingPriority(.required, for: .horizontal)
        tail.setContentCompressionResistancePriority(.required, for: .horizontal)
        editor.font = ScyllaUIStyle.toolFont
        editor.placeholderString = String(localized: "终端名称（留空恢复自动）")
        editor.focusRingType = .none
        editor.isHidden = true
        editor.delegate = self
        icon.contentTintColor = .labelColor
        icon.imageScaling = .scaleProportionallyUpOrDown
        for v in [lamp, spinner, icon, tool, state, badges, tail, subtitle, editor] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            addSubview(v)
        }
        subtitleHeight = subtitle.heightAnchor.constraint(equalToConstant: ScyllaUIStyle.subtitleLineHeight * 2)
        NSLayoutConstraint.activate([
            lamp.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 6),
            lamp.centerYAnchor.constraint(equalTo: tool.centerYAnchor),
            lamp.widthAnchor.constraint(equalToConstant: LampView.size),
            lamp.heightAnchor.constraint(equalToConstant: LampView.size),
            spinner.centerXAnchor.constraint(equalTo: lamp.centerXAnchor),
            spinner.centerYAnchor.constraint(equalTo: lamp.centerYAnchor),
            spinner.widthAnchor.constraint(equalToConstant: 12),
            spinner.heightAnchor.constraint(equalToConstant: 12),
            icon.leadingAnchor.constraint(equalTo: lamp.trailingAnchor, constant: 6),
            icon.centerYAnchor.constraint(equalTo: tool.centerYAnchor),
            icon.widthAnchor.constraint(equalToConstant: ScyllaUIStyle.toolIconSize),
            icon.heightAnchor.constraint(equalToConstant: ScyllaUIStyle.toolIconSize),
            tool.leadingAnchor.constraint(equalTo: icon.trailingAnchor, constant: ScyllaUIStyle.toolIconGap),
            tool.topAnchor.constraint(equalTo: topAnchor, constant: ScyllaUIStyle.taskRowInset),
            state.leadingAnchor.constraint(equalTo: tool.trailingAnchor, constant: 6),
            state.firstBaselineAnchor.constraint(equalTo: tool.firstBaselineAnchor),
            badges.leadingAnchor.constraint(greaterThanOrEqualTo: state.trailingAnchor, constant: 6),
            badges.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -10),
            badges.centerYAnchor.constraint(equalTo: tool.centerYAnchor),
            subtitle.leadingAnchor.constraint(equalTo: icon.leadingAnchor),
            subtitle.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -ScyllaUIStyle.taskRowInset),
            subtitleHeight,
            tail.leadingAnchor.constraint(greaterThanOrEqualTo: subtitle.trailingAnchor, constant: 8),
            tail.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -10),
            time.firstBaselineAnchor.constraint(equalTo: subtitle.firstBaselineAnchor),
            editor.leadingAnchor.constraint(equalTo: tool.leadingAnchor, constant: -2),
            editor.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -10),
            editor.firstBaselineAnchor.constraint(equalTo: tool.firstBaselineAnchor),
        ])
    }

    /// 就地改名：在第 1 行（名称所在处）显示输入框并取得键盘焦点；`done` 收到新名称（回车）或 nil（Esc / 失去焦点）。
    func beginRename(initial: String, done: @escaping (String?) -> Void) {
        renameDone = done
        editor.stringValue = initial
        editor.isHidden = false
        tool.isHidden = true
        state.alphaValue = 0
        window?.makeFirstResponder(editor)
        editor.currentEditor()?.selectAll(nil)
    }

    private func endRename(_ result: String?) {
        guard let done = renameDone else { return }
        renameDone = nil
        editor.isHidden = true
        tool.isHidden = false
        state.alphaValue = 1
        done(result)
    }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
        switch selector {
        case #selector(NSResponder.insertNewline(_:)):
            endRename(editor.stringValue.trimmingCharacters(in: .whitespacesAndNewlines))
            return true
        case #selector(NSResponder.cancelOperation(_:)):
            endRename(nil)
            return true
        default:
            return false
        }
    }

    func controlTextDidEndEditing(_ obj: Notification) { endRename(nil) }

    /// 改名期间该行的内容推送照常刷新其他字段；单元格被回收时才放弃改名。
    override func prepareForReuse() {
        super.prepareForReuse()
        endRename(nil)
    }

    override func hoverChanged() {
        remove.isHidden = !(hovered && onDelete != nil)
    }

    @objc private func removeClicked(_ sender: NSButton) { onDelete?() }

    required init?(coder: NSCoder) { fatalError() }

    /// 状态灯中心与副标题的位置（本单元格坐标），连线据此拐弯。
    var lampCenter: NSPoint { NSPoint(x: lamp.frame.midX, y: lamp.frame.midY) }
    var subtitleFrame: NSRect { subtitle.frame }
    /// 第 1 行的名称（用户命名、任务标题或工具名）：双击这里改名。
    var titleFrame: NSRect { tool.frame }
    /// 副标题第一行（连线拐到这一行的中间）。
    var subtitleFirstLine: NSRect {
        NSRect(x: subtitle.frame.minX, y: subtitle.frame.maxY - ScyllaUIStyle.subtitleLineHeight,
               width: subtitle.frame.width, height: ScyllaUIStyle.subtitleLineHeight)
    }

    func configure(_ row: SidebarRow) {
        let r: TaskRow
        switch row {
        case .terminal(let t), .conversation(let t): r = t
        default: return
        }
        tool.stringValue = r.name ?? r.heading ?? r.tool
        icon.image = toolIcon(r.toolKey)
        icon.contentTintColor = r.toolKey.flatMap { ScyllaUIStyle.toolColors[$0] } ?? .labelColor
        if case .conversation = row { icon.alphaValue = ScyllaUIStyle.stoppedIconAlpha } else { icon.alphaValue = 1 }
        state.stringValue = r.state ?? ""
        state.isHidden = r.state == nil
        state.textColor = r.lamp == .error ? ScyllaUIStyle.error : .secondaryLabelColor
        confirm.isHidden = !r.needsConfirm
        unseen.isHidden = !r.unseen
        conflict.isHidden = r.conflictTerminals == 0
        conflict.toolTip = r.conflictTerminals == 0 ? nil : String(localized: "同一对话同时在 \(r.conflictTerminals) 个终端打开")
        denied.isHidden = !(r.cwdDenied || r.cwdMissing)
        // 两种启动目录提示共用一个徽标（文字与悬停在第一次配置时设上），只在种类变了时改（每次重挂悬停会闪）。
        let dirNotice = r.cwdMissing ? String(localized: "目录不存在") : String(localized: "目录无权限")
        if denied.text != dirNotice {
            denied.set(dirNotice, fg: ScyllaUIStyle.waiting, bg: ScyllaUIStyle.waitingBg)
            denied.toolTip = r.cwdMissing
                ? String(localized: "这个终端的启动目录已不存在（可能已被删除、重命名或移走），Shell 已在主目录打开，没有代输启动命令。目录还在别处时，可在那里新开终端。")
                : String(localized: "后台进不了这个终端的启动目录，Shell 已在其他目录启动，没有代输启动命令。可在「系统设置 › 隐私与安全性 › 文件与文件夹」中允许 ScyllaUI 访问，再新开终端。")
        }
        // 「未同步」与「下次失效」共用一个徽标：前者是这个会话此刻没在同步，后者是它还在同步、退出后就不同步了。
        // 额度桥被改掉（2026-11-22）也用「未同步」，悬停写明只是额度读不到；状态没在同步的优先说。
        let syncText = r.unsynced || r.quotaLost ? String(localized: "未同步") : String(localized: "下次失效")
        let syncTip = r.unsynced
            ? String(localized: "这个 Agent 的状态没在同步，侧栏看不到它在做什么。点按查看怎么处理。")
            : r.quotaLost
            ? String(localized: "Claude 的状态在同步，但额度桥（statusline）被别的工具改掉了，标题栏读不到 5h、7d 额度。点按查看并修复。")
            : String(localized: "这款 Agent 的状态同步配置已失效。这个会话还在同步，退出后就不再同步。点按查看并修复。")
        if unsynced.text != syncText || unsynced.toolTip != syncTip {
            unsynced.set(syncText, fg: ScyllaUIStyle.waiting, bg: ScyllaUIStyle.waitingBg)
            unsynced.toolTip = syncTip
        }
        unsynced.isHidden = !(r.unsynced || r.syncNext || r.quotaLost)
        time.stringValue = r.time
        // 目录单行、截掉开头（保留末段）；其余最多两行、末尾截断。只在变化时设置：重设会让这一行重新排版，
        // 一批对话同时结束时侧栏会明显变慢（2026-10-25 块 11 并行回归实测）。
        if subtitle.usesSingleLineMode != r.subtitleIsPath {
            subtitle.maximumNumberOfLines = r.subtitleIsPath ? 1 : 2
            subtitle.usesSingleLineMode = r.subtitleIsPath
            subtitle.cell?.wraps = !r.subtitleIsPath
            subtitle.lineBreakMode = r.subtitleIsPath ? .byTruncatingHead : .byWordWrapping
        }
        subtitle.stringValue = r.subtitle
        (fullText, note) = (r.fullText ?? r.subtitle, r.note)
        needsLayout = true
        lamp.lamp = r.lamp
        running = r.lamp == .running
        lamp.isHidden = running
        updateSpinner()
    }

    override func layout() {
        super.layout()
        updateToolTips()
    }

    /// 悬停只给被截断的文字的全文（2026-10-30 用户要求）：第 1 行的名称、第 2 行的副标题各自看；完整显示的不弹。
    /// 副标题另有 `note` 时总是弹（如「ScyllaUI 暂不读取 OpenCode 的回复」）。宽度变了会重新排版，随之更新。每次布局都走到
    /// 这里，相同就不重赋，免得正在显示的提示被重挂、闪一下（2026-11-02，同标题栏的闪烁）。
    private func updateToolTips() {
        let toolTip = Self.isTruncated(tool) ? tool.stringValue : nil
        if tool.toolTip != toolTip { tool.toolTip = toolTip }
        let tip = [Self.isTruncated(subtitle) ? fullText : nil, note].compactMap { $0 }.joined(separator: "\n")
        let subtitleTip = tip.isEmpty ? nil : tip
        if subtitle.toolTip != subtitleTip { subtitle.toolTip = subtitleTip }
    }

    /// 文字在当前大小下放不下、显示时被截断。单行比控件自己要的宽度；折行的按标签左右各 2 pt 的内边距算可用宽度，比高度。
    static func isTruncated(_ f: NSTextField) -> Bool {
        guard f.bounds.width > 4, !f.stringValue.isEmpty else { return false }
        if f.usesSingleLineMode || f.cell?.wraps != true { return f.intrinsicContentSize.width > f.bounds.width + 0.5 }
        let height = f.attributedStringValue.boundingRect(with: NSSize(width: f.bounds.width - 4, height: .greatestFiniteMagnitude),
                                                          options: [.usesLineFragmentOrigin, .usesFontLeading]).height
        return ceil(height) > f.bounds.height
    }

    /// 只有在窗口中、状态为 Running 的行转圈；移出窗口（滚出可见区被回收）即停。
    private func updateSpinner() {
        if running && window != nil { spinner.startAnimation(nil) } else { spinner.stopAnimation(nil) }
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        updateSpinner()
    }
}

/// 一行的连线（行视图坐标，翻转坐标系：y 向下）。
struct RowGuide {
    /// 项目行：从文件夹图标下方到行底的竖线。
    var stem: (x: CGFloat, fromY: CGFloat)?
    /// 任务行：项目竖线（最后一个任务只画到拐点）及拐向状态灯的横线。
    var branch: (x: CGFloat, elbowY: CGFloat, toX: CGFloat, last: Bool)?
    /// 任务行：从状态灯向下拐到副标题前。
    var inner: (x: CGFloat, fromY: CGFloat, toY: CGFloat, toX: CGFloat)?
}

/// 侧栏行视图：在行内空白处画连线与选中态，不增加子视图（docs/design.md 第 5 节视觉第二版）。
final class SidebarRowView: NSTableRowView {
    static let id = NSUserInterfaceItemIdentifier("row")
    var guide: ((SidebarRowView) -> RowGuide?)?

    /// 不画行背景（包括分组行的灰底）：底色来自侧栏的 SidebarBackgroundView。
    override func drawBackground(in dirtyRect: NSRect) {}

    /// 选中：浅底 + 左侧 2 pt 强调色竖条，圆角 2 pt；不用系统整块高亮。
    /// 侧栏有键盘焦点（isEmphasized）时底色换成强调色浅底，提示上下键作用在侧栏。
    override func drawSelection(in dirtyRect: NSRect) {
        let r = bounds.insetBy(dx: 6, dy: 1)
        (isEmphasized ? ScyllaUIStyle.accent.withAlphaComponent(0.22) : NSColor.labelColor.withAlphaComponent(0.08)).setFill()
        NSBezierPath(roundedRect: r, xRadius: ScyllaUIStyle.radius, yRadius: ScyllaUIStyle.radius).fill()
        ScyllaUIStyle.accent.setFill()
        NSBezierPath(roundedRect: NSRect(x: r.minX, y: r.minY + 4, width: 2, height: r.height - 8),
                     xRadius: 1, yRadius: 1).fill()
    }

    override func draw(_ dirtyRect: NSRect) {
        super.draw(dirtyRect)
        guard let g = guide?(self) else { return }
        let path = NSBezierPath()
        path.lineWidth = 1
        // 落在像素中心，保持笔直清晰。
        func px(_ v: CGFloat) -> CGFloat { floor(v) + 0.5 }
        if let s = g.stem {
            path.move(to: NSPoint(x: px(s.x), y: s.fromY))
            path.line(to: NSPoint(x: px(s.x), y: bounds.maxY))
        }
        if let b = g.branch {
            path.move(to: NSPoint(x: px(b.x), y: bounds.minY))
            path.line(to: NSPoint(x: px(b.x), y: b.last ? px(b.elbowY) : bounds.maxY))
            path.move(to: NSPoint(x: px(b.x), y: px(b.elbowY)))
            path.line(to: NSPoint(x: b.toX, y: px(b.elbowY)))
        }
        if let i = g.inner {
            path.move(to: NSPoint(x: px(i.x), y: i.fromY))
            path.line(to: NSPoint(x: px(i.x), y: px(i.toY)))
            path.line(to: NSPoint(x: i.toX, y: px(i.toY)))
        }
        NSColor.separatorColor.setStroke()
        path.stroke()
    }
}

/// 侧栏底色：取终端背景色略微压暗，不用毛玻璃，与终端连成一体（docs/design.md 第 5 节视觉第二版）。
final class SidebarBackgroundView: NSView {
    var color: NSColor = .windowBackgroundColor {
        didSet { needsDisplay = true }
    }
    /// 点到列表外侧的边距：同样把键盘交给侧栏（docs/design.md 第 5 节「键盘在侧栏与终端之间」）。
    var onMouseDown: (() -> Void)?

    override func mouseDown(with event: NSEvent) {
        onMouseDown?()
    }

    override func draw(_ dirtyRect: NSRect) {
        color.setFill()
        dirtyRect.fill()
    }
}
