// 右侧栏（docs/design.md 5.12 节）：终端区右边的一栏，状态、文件、Diff、历史提示词四选一。
// 宿主负责四栏共用的底色、左侧分隔线、左边缘拖宽与蓝线、当前显示哪一栏；各栏只管自己的内容。

import AppKit

enum RightPanel: CaseIterable {
    case status, files, diff, history

    /// 再按一次同一个为关，按别的直接换过去（四栏互斥）。
    static func toggled(_ current: RightPanel?, _ requested: RightPanel) -> RightPanel? {
        current == requested ? nil : requested
    }
}

final class RightPanelHost: NSView {
    /// 四栏共用的列表宽度：默认取最小宽度（2026-11-21 用户要求，原为 360），可拖到 520。
    static let defaultWidth: CGFloat = minWidth
    static let minWidth: CGFloat = 280
    static let maxWidth: CGFloat = 520

    /// 底色同侧栏（工作区按主题设置），左侧画一条分隔线。
    var color: NSColor = .windowBackgroundColor {
        didSet { needsDisplay = true }
    }
    /// 拖动左边缘：新宽度与是否松手；双击左边缘：回到默认宽度。宽度的取值范围由控制器决定。
    var onResize: ((CGFloat, Bool) -> Void)?
    var onResetWidth: (() -> Void)?

    private(set) var content: NSView?
    private let edge = EdgeHandle()
    private var dragWidth: CGFloat?
    /// 左边缘悬停或拖动时的蓝线（2026-11-06，DividerHighlight）。
    private let edgeHighlight = DividerHighlight()

    override init(frame: NSRect) {
        super.init(frame: frame)
        // 只画自己的范围：macOS 14 起视图默认不裁剪绘制，脏区可以超出边界，底色会涂满整个终端区（2026-10-25 实测）。
        clipsToBounds = true
        // 同一次拖动里按鼠标位置累计宽度：拖过头被夹住后再往回拖，边缘跟着鼠标回来（F3：查看器收起后要能拖回来展开）。
        edge.onDrag = { [weak self] dx, done in
            guard let self else { return }
            let w = (self.dragWidth ?? self.bounds.width) - dx
            self.dragWidth = done ? nil : w
            self.onResize?(w, done)
        }
        edge.onDoubleClick = { [weak self] in self?.onResetWidth?() }
        edge.onHover = { [weak self] on in self?.edgeHighlight.setHovering(on) }
        edge.onDragging = { [weak self] on in self?.edgeHighlight.setDragging(on) }
        for v in [edge, edgeHighlight] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            addSubview(v)
        }
        NSLayoutConstraint.activate([
            edge.leadingAnchor.constraint(equalTo: leadingAnchor),
            edge.topAnchor.constraint(equalTo: topAnchor),
            edge.bottomAnchor.constraint(equalTo: bottomAnchor),
            edge.widthAnchor.constraint(equalToConstant: 5),
            edgeHighlight.leadingAnchor.constraint(equalTo: leadingAnchor),
            edgeHighlight.topAnchor.constraint(equalTo: topAnchor),
            edgeHighlight.bottomAnchor.constraint(equalTo: bottomAnchor),
            edgeHighlight.widthAnchor.constraint(equalToConstant: DividerHighlight.width),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    /// 换成另一栏的内容：铺满宿主，压在左边缘手柄之下（手柄始终可拖）。
    func setContent(_ view: NSView?) {
        guard view !== content else { return }
        content?.removeFromSuperview()
        content = view
        guard let view else { return }
        view.translatesAutoresizingMaskIntoConstraints = false
        addSubview(view, positioned: .below, relativeTo: edge)
        NSLayoutConstraint.activate([
            view.topAnchor.constraint(equalTo: topAnchor),
            view.bottomAnchor.constraint(equalTo: bottomAnchor),
            view.leadingAnchor.constraint(equalTo: leadingAnchor),
            view.trailingAnchor.constraint(equalTo: trailingAnchor),
        ])
    }

    override func draw(_ dirtyRect: NSRect) {
        color.setFill()
        dirtyRect.intersection(bounds).fill()
        let scale = window?.backingScaleFactor ?? 2
        NSColor.separatorColor.setFill()
        NSRect(x: 0, y: 0, width: 1 / scale, height: bounds.height).fill(using: .sourceOver)
    }
}

/// 状态、文件、Diff 三栏共用的栏头（同历史提示词的样式）：第 1 行图标、标题、计数、附加按钮与关闭；第 2 行说明。
final class RightPanelHeader: NSView {
    let title = NSTextField(labelWithString: "")
    let count = NSTextField(labelWithString: "")
    let subtitle = NSTextField(labelWithString: "")
    var onClose: (() -> Void)?

    private let icon = NSImageView()
    private let close = NSButton()
    private let tools = NSStackView()

    init(symbol: String, title text: String, closeName: String) {
        super.init(frame: .zero)
        icon.image = NSImage(systemSymbolName: symbol, accessibilityDescription: text)
        icon.symbolConfiguration = .init(pointSize: 13, weight: .semibold)
        icon.contentTintColor = ScyllaUIStyle.ready
        title.stringValue = text
        title.font = ScyllaUIStyle.sectionFont
        title.textColor = .labelColor
        count.font = ScyllaUIStyle.sectionFont
        count.textColor = .tertiaryLabelColor
        subtitle.font = ScyllaUIStyle.detailFont
        subtitle.textColor = .secondaryLabelColor
        subtitle.lineBreakMode = .byTruncatingMiddle
        subtitle.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        close.image = NSImage(systemSymbolName: "xmark", accessibilityDescription: closeName)
        close.symbolConfiguration = .init(pointSize: 11, weight: .semibold)
        close.bezelStyle = .accessoryBarAction
        close.isBordered = false
        close.contentTintColor = .secondaryLabelColor
        close.toolTip = closeName
        close.target = self
        close.action = #selector(closeClicked(_:))
        tools.orientation = .horizontal
        tools.spacing = 2
        for v in [icon, title, count, tools, close, subtitle] as [NSView] {
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
            tools.centerYAnchor.constraint(equalTo: title.centerYAnchor),
            tools.leadingAnchor.constraint(greaterThanOrEqualTo: count.trailingAnchor, constant: 6),
            tools.trailingAnchor.constraint(equalTo: close.leadingAnchor, constant: -2),
            close.centerYAnchor.constraint(equalTo: title.centerYAnchor),
            close.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -10),
            close.widthAnchor.constraint(equalToConstant: 20),
            subtitle.topAnchor.constraint(equalTo: title.bottomAnchor, constant: 5),
            subtitle.leadingAnchor.constraint(equalTo: icon.leadingAnchor),
            subtitle.trailingAnchor.constraint(lessThanOrEqualTo: trailingAnchor, constant: -12),
            subtitle.bottomAnchor.constraint(equalTo: bottomAnchor),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    /// 关闭按钮的功能名（悬停与读屏）：文件栏打开着文件时是「关闭文件」，否则是「关闭文件栏」。
    var closeName: String {
        get { close.toolTip ?? "" }
        set {
            close.toolTip = newValue
            close.setAccessibilityLabel(newValue)
        }
    }

    /// 栏头的附加按钮（只有图标，悬停写功能名），排在关闭按钮左边。
    @discardableResult
    func addTool(symbol: String, name: String, target: AnyObject, action: Selector) -> NSButton {
        let b = Self.iconButton(symbol: symbol, name: name, target: target, action: action)
        tools.addArrangedSubview(b)
        return b
    }

    /// 折行按钮：文件查看器与「更改」查看器共用同一个图标和文案，放在标题行右边那排图标的最左边（实现方案第 6 版 R1）。
    static let wrapSymbol = "arrow.turn.down.left"

    static func wrapButton(target: AnyObject, action: Selector) -> NSButton {
        let b = iconButton(symbol: wrapSymbol, name: String(localized: "折行"), target: target, action: action)
        styleWrap(b, on: false)
        return b
    }

    /// 开着用强调色、提示「取消折行」；关着用次要色、提示「折行」（同「隐藏文件树」按钮的做法）。
    static func styleWrap(_ b: NSButton, on: Bool) {
        b.contentTintColor = on ? .controlAccentColor : .secondaryLabelColor
        b.toolTip = on ? String(localized: "取消折行") : String(localized: "折行")
        b.setAccessibilityLabel(b.toolTip)
    }

    /// 只有图标的小按钮（栏头、行尾共用）：系统符号 12 pt，次要文字色，悬停写功能名。
    static func iconButton(symbol: String, name: String, target: AnyObject?, action: Selector?) -> NSButton {
        let b = NSButton()
        b.image = NSImage(systemSymbolName: symbol, accessibilityDescription: name)
        b.symbolConfiguration = .init(pointSize: 12, weight: .regular)
        b.bezelStyle = .accessoryBarAction
        b.isBordered = false
        b.contentTintColor = .secondaryLabelColor
        b.toolTip = name
        b.setAccessibilityLabel(name)
        b.target = target
        b.action = action
        b.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([b.widthAnchor.constraint(equalToConstant: 22), b.heightAnchor.constraint(equalToConstant: 22)])
        return b
    }

    @objc private func closeClicked(_ sender: Any?) { onClose?() }
}

/// 左边缘拖宽的热区（原在 PromptHistoryPanel，2026-11-23 移到宿主，四栏共用；F3 起「文件」的分隔线也用它）。
final class EdgeHandle: NSView {
    var onDrag: ((CGFloat, Bool) -> Void)?
    var onDoubleClick: (() -> Void)?
    var onHover: ((Bool) -> Void)?
    var onDragging: ((Bool) -> Void)?
    private var startX: CGFloat?
    private var hoverArea: NSTrackingArea?

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .resizeLeftRight)
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let a = hoverArea { removeTrackingArea(a) }
        // 拖动改宽度时会重建热区；鼠标正在里面时要声明 assumeInside，否则移开时收不到离开事件，蓝线留着。
        var options: NSTrackingArea.Options = [.mouseEnteredAndExited, .mouseMoved, .cursorUpdate, .activeInKeyWindow, .inVisibleRect]
        if let w = window, bounds.contains(convert(w.mouseLocationOutsideOfEventStream, from: nil)) { options.insert(.assumeInside) }
        let a = NSTrackingArea(rect: .zero, options: options, owner: self)
        addTrackingArea(a)
        hoverArea = a
    }

    // 光标（docs/design.md 5.12 节「共用的宿主」）：边缘压在查看器的 NSTextView 上，光标区域会被它的文字光标盖掉，
    // 所以 cursorUpdate、进入、移动、拖动时都自己设箭头（2026-11-23 用户录屏：蓝线亮了光标不变）。
    override func cursorUpdate(with event: NSEvent) { NSCursor.resizeLeftRight.set() }

    override func mouseMoved(with event: NSEvent) {
        if mouseInside { NSCursor.resizeLeftRight.set() }
    }

    private var mouseInside: Bool {
        guard let w = window else { return false }
        return bounds.contains(convert(w.mouseLocationOutsideOfEventStream, from: nil))
    }

    /// 只认鼠标真的在边缘上（同侧栏分隔线：窗口切到前台时会按过时的位置补发进入事件）。
    override func mouseEntered(with event: NSEvent) {
        let inside = mouseInside
        onHover?(inside)
        guard inside else { return }
        NSCursor.resizeLeftRight.set()
        // NSTextView 的跟踪区在同一个进入事件里也会设文字光标，先后不定；下一轮主循环再设一次。
        DispatchQueue.main.async { [weak self] in
            if self?.mouseInside == true { NSCursor.resizeLeftRight.set() }
        }
    }

    override func mouseExited(with event: NSEvent) { onHover?(false) }

    override func mouseDown(with event: NSEvent) {
        if event.clickCount == 2 {
            startX = nil
            onDoubleClick?()
            return
        }
        startX = event.locationInWindow.x
        NSCursor.resizeLeftRight.set()
        onDragging?(true)
    }

    override func mouseDragged(with event: NSEvent) {
        guard let s = startX else { return }
        NSCursor.resizeLeftRight.set()
        onDrag?(event.locationInWindow.x - s, false)
        startX = event.locationInWindow.x
    }

    /// 拖动中收不到进出事件（往右拖到最窄时鼠标会越过边缘）；松手时按实际位置补一次，否则蓝线一直亮着（2026-11-22 用户实测）。
    /// 先让窗口按新宽度排好再判断：双击复位刚改了约束、还没重新布局时，热区仍在鼠标底下，算成悬停；布局后热区移走、
    /// 收不到离开事件，蓝线一直亮着（2026-11-25 方案会话验收 D2 时发现，单测复现）。
    override func mouseUp(with event: NSEvent) {
        window?.layoutIfNeeded()
        onHover?(bounds.contains(convert(event.locationInWindow, from: nil)))
        onDragging?(false)
        guard startX != nil else { return }
        startX = nil
        onDrag?(0, true)
    }
}

// MARK: - 查看器与列表的分栏（文件栏 F3、Diff 栏 D2 共用）

/// 查看器在左、列表（文件树或改动列表）在右：两者之间可拖的分隔线（列表拖到 160 以下隐藏，同一次拖回来又出现，双击回到 280），
/// 以及查看器收起时列表从右边滑过来盖住它的过渡（docs/design.md 5.12 节「分隔线与自动隐藏」）。
/// 视图仍由所在栏建好、加进去；这里只管它们之间会切换的约束、分隔线的热区与蓝线、拖动和过渡。打开没有、列表隐藏、查看器收起由所在栏给出。
final class ListSplit {
    /// 列表宽（打开内容后列表的宽度），文件树与改动列表共用一个值（`Prefs.filesTreeWidth`）。
    static let defaultWidth: CGFloat = 280
    static let hideWidth: CGFloat = 160
    /// 列表显示时查看器最窄 160：再窄就收起、由列表盖住（2026-11-23 用户看 demo 后由 320 改为 160）。
    static let viewerMinWidth: CGFloat = 160
    /// 收起后拖回到「列表宽 + 160」再多这么多才展开，免得在临界处来回抖时反复收起、展开。
    static let expandHysteresis: CGFloat = 16
    /// 收起、展开的过渡（0.2 秒）：查看器定宽不重排，列表的宽度在列表宽与整栏宽之间滑动，盖在查看器上面。
    static let slideDuration: CFTimeInterval = 0.2
    /// 系统设了「减弱动态效果」时不播（测试里可换掉）。
    static var reduceMotion: () -> Bool = { NSWorkspace.shared.accessibilityDisplayShouldReduceMotion }
    /// 过渡用的时钟（测试里可换掉）。
    static var now: () -> CFTimeInterval = CACurrentMediaTime

    /// 拖右侧栏左边缘到 `width` 时（打开着内容、列表显示着）：收起与否，以及右侧栏该用的宽度。
    /// 查看器窄到 160 以下收起，栏宽仍跟着鼠标（只剩列表，最窄 280）；收起后拖回到「列表宽 + 160 + 16」以上、且放得下才展开。
    static func edgeDrag(width: CGFloat, listWidth: CGFloat, collapsed: Bool, room: CGFloat) -> (collapsed: Bool, width: CGFloat) {
        let collapseAt = listWidth + viewerMinWidth
        let expandAt = collapseAt + expandHysteresis
        let collapse = collapsed ? !(width >= expandAt && room >= expandAt) : width < collapseAt
        if collapse {
            let minW = RightPanelHost.minWidth
            return (true, min(max(width, minW), max(minW, room)))
        }
        return (false, min(max(width, collapseAt), max(collapseAt, room)))
    }

    /// 打开着内容（不论收起没有）。
    var isOpen: () -> Bool = { false }
    /// 列表是否隐藏（只在打开着内容时有意义），与拖动时直接改它（不经所在栏的重排）。
    var listHidden: () -> Bool = { false }
    var setListHidden: (Bool) -> Void = { _ in }
    /// 查看器收起、只剩列表。
    var collapsed: () -> Bool = { false }
    /// 拖动中列表隐藏或出现、松手：所在栏重排并通知工作区。
    var onLayout: () -> Void = {}
    /// 松手或双击后的列表宽：工作区记下来。
    var onWidth: ((CGFloat) -> Void)?
    /// 日志里的名字（「files tree」「diff list」）。
    var logName = ""

    var width: CGFloat = ListSplit.defaultWidth {
        didSet { if slide == nil { widthConstraint.constant = width } }
    }
    /// 列表的底色（同右侧栏宿主）：收起、展开的过渡里列表要盖住查看器，得有底色。
    var fillColor: NSColor = .windowBackgroundColor {
        didSet { applyFill() }
    }
    /// 查看器与列表之间的分隔线：热区与悬停蓝线同右侧栏左边缘。
    let handle = EdgeHandle()
    let highlight = DividerHighlight()

    private unowned let host: NSView
    private unowned let viewer: NSView
    private unowned let column: NSView
    private unowned let divider: NSView
    private let widthConstraint: NSLayoutConstraint
    private let columnToEdge: NSLayoutConstraint
    private let viewerToColumn: NSLayoutConstraint
    private let viewerToEdge: NSLayoutConstraint
    private let viewerFixedWidth: NSLayoutConstraint
    /// 同一次拖动里按鼠标位置累计的列表宽（可以小于 0），松手清掉。
    private var drag: CGFloat?
    private var slide: (start: CFTimeInterval, from: CGFloat, to: CGFloat)?
    private var timer: Timer?

    /// `column` 已加进 `host`、贴好上下与右边；`viewer` 已贴好上下与左边。热区与蓝线加在最上面，从 `column` 的顶端到 `host` 的底。
    init(host: NSView, viewer: NSView, column: NSView, divider: NSView) {
        self.host = host
        self.viewer = viewer
        self.column = column
        self.divider = divider
        widthConstraint = column.widthAnchor.constraint(equalToConstant: ListSplit.defaultWidth)
        columnToEdge = column.leadingAnchor.constraint(equalTo: host.leadingAnchor)
        viewerToColumn = viewer.trailingAnchor.constraint(equalTo: column.leadingAnchor)
        viewerToEdge = viewer.trailingAnchor.constraint(equalTo: host.trailingAnchor)
        viewerFixedWidth = viewer.widthAnchor.constraint(equalToConstant: 0)
        column.wantsLayer = true
        for v in [handle, highlight] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            host.addSubview(v)
        }
        handle.onDrag = { [weak self] dx, done in self?.drag(dx, done: done) }
        handle.onDoubleClick = { [weak self] in self?.resetWidth() }
        handle.onHover = { [weak self] on in self?.highlight.setHovering(on) }
        handle.onDragging = { [weak self] on in self?.highlight.setDragging(on) }
        NSLayoutConstraint.activate([
            // 分隔线的热区贴着查看器的右边（列表隐藏时在栏的右边缘，只在拖动中用得到）。
            handle.centerXAnchor.constraint(equalTo: viewer.trailingAnchor),
            handle.topAnchor.constraint(equalTo: column.topAnchor),
            handle.bottomAnchor.constraint(equalTo: host.bottomAnchor),
            handle.widthAnchor.constraint(equalToConstant: 5),
            highlight.centerXAnchor.constraint(equalTo: viewer.trailingAnchor),
            highlight.topAnchor.constraint(equalTo: column.topAnchor),
            highlight.bottomAnchor.constraint(equalTo: host.bottomAnchor),
            highlight.widthAnchor.constraint(equalToConstant: DividerHighlight.width),
        ])
        applyFill()
    }

    var sliding: Bool { slide != nil }
    /// 查看器此刻显示着（打开着且没收起）。
    var contentShown: Bool { isOpen() && !collapsed() }

    func applyFill() {
        host.effectiveAppearance.performAsCurrentDrawingAppearance { column.layer?.backgroundColor = fillColor.cgColor }
    }

    /// 按此刻的状态切换约束与显隐。
    func applyConstraints() {
        let open = contentShown
        let hidden = listHidden()
        let sliding = slide != nil
        viewer.isHidden = !open && !sliding
        column.isHidden = open && hidden
        divider.isHidden = (!open && !sliding) || hidden
        // 列表隐藏后热区收起；拖动中留着，好在同一次拖动里拖回来。过渡中不能拖。
        handle.isHidden = !open || sliding || (hidden && drag == nil)
        highlight.isHidden = handle.isHidden
        columnToEdge.isActive = !open && !sliding
        widthConstraint.isActive = (open && !hidden) || sliding
        viewerToColumn.isActive = open && !hidden && !sliding
        viewerToEdge.isActive = open && hidden && !sliding
        viewerFixedWidth.isActive = sliding
    }

    // MARK: 收起、展开的过渡（F3 补改，2026-11-23）

    /// 收起：查看器定在原宽，列表从列表宽滑到整栏宽盖住它；展开反过来，查看器按展开后的宽度先在下面摆好。
    /// 窗口不可见（测试）或系统设了减弱动态效果时不播。过渡中途反向时从当前位置接着滑。调用前状态已经切换。
    func startSlide(collapsing: Bool, viewerWidth: CGFloat) {
        guard host.window?.isVisible == true, !Self.reduceMotion() else { return finishSlide() }
        // 起点：没在过渡时取切换前的位置（此时状态已经切换），过渡中途反向时取当前位置。
        let from = slide == nil ? (collapsing ? 0 : 1) : fraction
        if collapsing { viewerFixedWidth.constant = slide == nil ? viewerWidth : viewer.frame.width }
        slide = (Self.now(), from, collapsing ? 1 : 0)
        applyConstraints()
        step()
        if timer == nil {
            let t = Timer(timeInterval: 1.0 / 120, repeats: true) { [weak self] _ in self?.step() }
            RunLoop.main.add(t, forMode: .common)
            timer = t
        }
    }

    /// 列表盖住查看器的程度：0 为展开（列表宽），1 为收起（整栏宽）。
    private var fraction: CGFloat {
        guard let s = slide else { return contentShown ? 0 : 1 }
        let p = min(1, (Self.now() - s.start) / Self.slideDuration)
        return s.from + (s.to - s.from) * (1 - pow(1 - p, 3))
    }

    func step() {
        guard let s = slide else { return }
        let f = fraction
        let full = max(host.bounds.width, width)
        // 展开时栏宽还在跟着鼠标变，查看器按当前栏宽摆。
        if s.to == 0 { viewerFixedWidth.constant = max(host.bounds.width - width, Self.viewerMinWidth) }
        widthConstraint.constant = width + (full - width) * f
        if Self.now() - s.start >= Self.slideDuration { finishSlide() }
    }

    private func finishSlide() {
        timer?.invalidate()
        timer = nil
        guard slide != nil else { return }
        slide = nil
        widthConstraint.constant = width
        applyConstraints()
    }

    // MARK: 分隔线（F3）

    /// 拖分隔线：按鼠标位置累计列表宽；窄到 160 以下列表隐藏，拖回来列表出现；最宽到查看器剩 160。右侧栏总宽不变。
    func drag(_ dx: CGFloat, done: Bool) {
        guard contentShown else { return }
        let t = (drag ?? (listHidden() ? 0 : width)) - dx
        drag = done ? nil : t
        if t < Self.hideWidth {
            if !listHidden() { setListHidden(true); onLayout() }
        } else {
            width = min(t, max(Self.hideWidth, host.bounds.width - Self.viewerMinWidth))
            if listHidden() { setListHidden(false); onLayout() }
        }
        if done {
            onLayout()
            onWidth?(width)
            scyllauiLog.notice("\(self.logName, privacy: .public) width=\(Int(self.width)) hidden=\(self.listHidden())")
        }
    }

    /// 双击分隔线：列表宽回到 280（查看器不够 160 时取能放下的最宽）。
    func resetWidth() {
        width = min(Self.defaultWidth, max(Self.hideWidth, host.bounds.width - Self.viewerMinWidth))
        onWidth?(width)
    }
}

/// 查看器正文（文件查看器的源码、渲染视图与「更改」正文）的浮动滚动条：停止滚动后续显约 3 秒再淡出，系统默认约半秒，
/// 来不及去拖（2026-11-26 第 7 版，docs/design.md 5.12 节「滚动条」）。续显只是每隔一小段调一次 `flashScrollers`
/// （两个方向一起显示），不改滚动条样式、正文区尺寸与滚动逻辑；系统设为「始终显示滚动条」（`.legacy`）时什么也不做。
final class ScrollerLinger {
    static var duration: TimeInterval = 3
    static let interval: TimeInterval = 0.25
    private weak var scroll: NSScrollView?
    private var until: CFTimeInterval = 0
    private var timer: Timer?

    /// 可滚区域每次滚动（含程序定位）都续显。
    init(_ scroll: NSScrollView) {
        self.scroll = scroll
        scroll.contentView.postsBoundsChangedNotifications = true
        NotificationCenter.default.addObserver(forName: NSView.boundsDidChangeNotification, object: scroll.contentView, queue: nil) { [weak self] _ in
            self?.show()
        }
    }

    /// 从现在起显示约 `duration` 秒（新打开文件时也调，让人知道能滚、有多长多宽）。内容没超出的方向系统本来就不显示。
    func show() {
        guard let sv = scroll, sv.window != nil, !sv.isHiddenOrHasHiddenAncestor, sv.scrollerStyle == .overlay else { return }
        until = CACurrentMediaTime() + Self.duration
        guard timer == nil else { return }
        // 放进 common 模式：拖滚动条、滚轮惯性期间（事件跟踪模式）也照常走。
        let t = Timer(timeInterval: Self.interval, repeats: true) { [weak self] _ in self?.tick() }
        RunLoop.main.add(t, forMode: .common)
        timer = t
    }

    private func tick() {
        guard let sv = scroll, sv.window != nil, sv.scrollerStyle == .overlay else { return stop() }
        // 鼠标停在滚动条上：一直显示，移开后再显示约 `duration` 秒。系统自己不管这一条：续显结束时即使鼠标停在
        // 滚动条上也照常淡出（测试版实测）。
        if Self.pointerOnScroller(sv) { until = max(until, CACurrentMediaTime() + Self.duration) }
        guard CACurrentMediaTime() < until else { return stop() }
        sv.flashScrollers()
    }

    private func stop() {
        timer?.invalidate()
        timer = nil
    }

    /// 鼠标此刻是否在看得见的竖向或横向滚动条上（滚动条在滚动视图里的位置，四周放宽 2 点）。
    static func pointerOnScroller(_ sv: NSScrollView) -> Bool {
        guard let w = sv.window else { return false }
        let p = sv.convert(w.mouseLocationOutsideOfEventStream, from: nil)
        return [sv.hasVerticalScroller ? sv.verticalScroller : nil, sv.hasHorizontalScroller ? sv.horizontalScroller : nil]
            .contains { s in
                guard let s, !s.isHidden else { return false }
                return s.frame.insetBy(dx: -2, dy: -2).contains(p)
            }
    }

    /// 测试用：是否正在续显。
    var isLingering: Bool { timer != nil }
}
