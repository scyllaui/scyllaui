// 欢迎窗口（M4a 块 4，docs/design.md 10.4 节决策 9）：与主窗口同大、盖在主窗口上（子窗口），透明标题栏、不可调整大小；
// 主窗口照常接回终端。五页：① 开屏；② 「文稿」等文件夹的访问授权说明；③ 为什么要同步状态（Hook）；④ 安装状态同步
// （0.4.1 起③④由原来的一页拆成两页）；⑤ 遇到问题怎么反馈（2026-11-02）。②—④ 只在首次启动与主或次版本号变化时出现，
// ⑤ 每天第一次打开时出现（WelcomePolicy）。用户在那一页点了往下走的按钮才记为看过；停在某一页不操作、窗口被关掉
// （关闭按钮、退出 App、改语言后重启）都不算，下次还会出现（2026-11-16 用户决定）。
// 没看过当前版本的《使用须知》时，开屏后先是须知页，只有「继续」，关窗不退出（2026-11-07 开源后取代原先的许可协议同意页）。
// ②—④ 的内容列宽随窗口（不少于窗口宽的 60%），③④ 可「← 上一步」回看（2026-10-28 用户要求，docs/design.md 10.4 节决策 9）。

import AppKit

/// 欢迎窗口自己不可移动（盖在主窗口上的子窗口，单独移动会和主窗口错开）；在标题栏区域按下鼠标（不在按钮上）时改为拖动主窗口，
/// 欢迎窗口作为子窗口跟着走（2026-10-28 用户要求，docs/design.md 10.4 节决策 9「拖动」）。
private final class WelcomeWindow: NSWindow {
    private static let dragBarHeight: CGFloat = 28

    override func sendEvent(_ event: NSEvent) {
        if event.type == .leftMouseDown, let parent,
           event.locationInWindow.y >= frame.height - Self.dragBarHeight,
           !(contentView?.superview?.hitTest(event.locationInWindow) is NSControl) {
            parent.performDrag(with: event)
            return
        }
        super.sendEvent(event)
    }
}

final class WelcomeWindowController: NSObject, NSWindowDelegate {
    private let window: NSWindow
    private let core: CoreClient
    private let guide: Bool
    private let feedback: Bool
    private let notice: Bool
    private let record: Bool
    private let version: String
    private let onClose: () -> Void
    private var agents: AgentPane?
    /// 「安装状态同步」页里的「同时接入 Claude 的额度」（2026-11-22）：默认勾选；Claude 没装、已接入或撤销过状态同步时隐藏。
    private let bridgeBox = NSButton(checkboxWithTitle: String(localized: "同时接入 Claude 的 5h、7d 额度（不在 Claude 底部显示任何内容）"), target: nil, action: nil)
    /// 最近一次的 Hook 状态：勾选或取消「同时接入额度」时按它重算按钮。
    private var hookStatuses: [SBHookStatus] = []
    private var splash: SplashView?
    private var playScheduled = false

    /// 按显示策略决定是否出现；出现时返回控制器（调用方持有到关闭）。`onClose` 在窗口关闭后调用。
    static func showIfNeeded(over main: NSWindow, runtime: GhosttyRuntime, core: CoreClient,
                             onClose: @escaping () -> Void) -> WelcomeWindowController? {
        let version = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "0"
        // 测试用：隔离数据目录下 SCYLLAUI_WELCOME=force 时开屏与引导都显示，且不写记录（不影响共用的偏好）。
        let forced = core.isolated && ProcessInfo.processInfo.environment["SCYLLAUI_WELCOME"] == "force"
        let d = forced
            ? WelcomePolicy.Decision(splash: true, guide: true, feedback: true, notice: true)
            : WelcomePolicy.decide(now: Date(), version: version, lastShown: Prefs.splashShownAt,
                                   lastVersion: Prefs.splashVersion, guideSeen: Prefs.guideSeen,
                                   lastFeedback: Prefs.feedbackShownAt, noticeSeen: Prefs.noticeSeen)
        scyllauiLog.notice("welcome decide splash=\(d.splash) guide=\(d.guide) feedback=\(d.feedback) notice=\(d.notice) forced=\(forced) version=\(version, privacy: .public)")
        guard d.splash else { return nil }
        let c = WelcomeWindowController(over: main, runtime: runtime, core: core, guide: d.guide, feedback: d.feedback, notice: d.notice,
                                        record: !forced, version: version, onClose: onClose)
        c.showSplash()
        return c
    }

    private init(over main: NSWindow, runtime: GhosttyRuntime, core: CoreClient, guide: Bool, feedback: Bool, notice: Bool, record: Bool,
                 version: String, onClose: @escaping () -> Void) {
        self.core = core
        self.guide = guide
        self.feedback = feedback
        self.notice = notice
        self.record = record
        self.version = version
        self.onClose = onClose
        window = WelcomeWindow(contentRect: main.contentRect(forFrameRect: main.frame),
                               styleMask: [.titled, .closable, .fullSizeContentView], backing: .buffered, defer: false)
        super.init()
        window.titlebarAppearsTransparent = true
        window.titleVisibility = .hidden
        window.isMovable = false
        window.isReleasedWhenClosed = false
        window.standardWindowButton(.miniaturizeButton)?.isHidden = true
        window.standardWindowButton(.zoomButton)?.isHidden = true
        window.appearance = runtime.windowAppearance
        window.setFrame(main.frame, display: false)
        window.delegate = self
        main.addChildWindow(window, ordered: .above)
    }

    private func showSplash() {
        let splash = SplashView(frame: window.contentView?.bounds ?? .zero)
        splash.onStart = { [weak self] in self?.next() }
        self.splash = splash
        window.contentView = splash
        window.makeKeyAndOrderFront(nil)
        window.makeFirstResponder(splash)
        splash.warmUp()
        // 主窗口接回终端（建 Surface、刷新侧栏）要占主线程几十毫秒，与动画开头撞在一起会掉帧（块 4 实测最大帧间隔
        // 29—92 ms，都在开头 0.1—0.2 s）。先只显示底色（动画第 0 帧本来就是空的），等接回之后再播；最多等 1.5 s。
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { [weak self] in self?.startSplash(after: 0, reason: "timeout") }
    }

    /// 主窗口接回了终端（WorkspaceController 的第一份快照处理完）。
    func mainWindowRestored() {
        startSplash(after: 0.25, reason: "restored")
    }

    private func startSplash(after delay: Double, reason: String) {
        guard !playScheduled else { return }
        playScheduled = true
        DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak self] in
            scyllauiLog.notice("splash start reason=\(reason, privacy: .public)")
            self?.splash?.play()
        }
    }

    private var page = 0

    private typealias Page = WelcomePolicy.Page

    /// 开屏之后的页序：使用须知、引导页 ②—④ 与「遇到问题」⑤ 各按自己的规则出现。
    private lazy var pages: [Page] = (notice ? [.notice] : []) + (guide ? [.files, .why, .hooks] : []) + (feedback ? [.feedback] : [])

    /// 用户在当前页点了往下走的按钮（「继续」「完成」「以后再说」）：这是写「看过」记录的唯一入口（WelcomePolicy.mark）。
    private func acted() {
        guard page >= 1, page <= pages.count, let mark = WelcomePolicy.mark(leaving: pages[page - 1]) else { return }
        if record {
            switch mark {
            case .notice: Prefs.recordNotice(version: WelcomePolicy.noticeVersion, at: Date())
            case .guide: Prefs.recordGuide(version: version)
            case .feedback: Prefs.recordFeedback(at: Date())
            }
        }
        scyllauiLog.notice("welcome read mark=\(String(describing: mark), privacy: .public) page=\(self.page) recorded=\(self.record)")
    }

    private func next() {
        acted()
        page += 1
        showPage()
    }

    private func showPage() {
        guard page >= 1, page <= pages.count else { return window.close() }
        switch pages[page - 1] {
        case .notice: window.contentView = scaled(noticePage())
        case .files: window.contentView = scaled(filesPage())
        case .why: window.contentView = scaled(whyPage())
        case .hooks: window.contentView = scaled(hooksPage())
        case .feedback: window.contentView = scaled(feedbackPage())
        }
    }

    func windowWillClose(_ notification: Notification) {
        // 关窗只记开屏的时间与版本；各页是否看过由 acted() 在用户点按钮时记，停在某一页被关掉的不算。
        if record { Prefs.recordSplash(at: Date(), version: version) }
        scyllauiLog.notice("welcome closed page=\(self.page) of=\(self.pages.count) recorded=\(self.record)")
        window.parent?.removeChildWindow(window)
        onClose()
    }

    // MARK: 引导页

    /// 引导页内容列的宽度（2026-10-28 用户要求至少占窗口宽的 60%）：取 62%，不少于 `minimum`（原来的固定宽度），
    /// 两侧各留至少 48 pt。
    private func columnWidth(minimum: CGFloat = 560, fraction: CGFloat = 0.62) -> CGFloat {
        let w = window.frame.width / pageScale
        return max(minimum, min((w * fraction).rounded(), w - 96))
    }

    /// 引导页整页放大的倍数（2026-10-28 用户要求纵向也撑开、文字放大）：按 1100 × 720 的设计尺寸随窗口等比放大，
    /// 限制在 1—1.5 倍（与开屏页同一思路）。内容列宽按放大前的尺寸算，放大后仍约占窗口宽的 62%。
    private lazy var pageScale: CGFloat = {
        let f = window.frame
        return min(1.5, max(1, min(f.width / 1100, f.height / 720)))
    }()

    /// 页面放进窗口：内容（含上下各 24 pt 的边距）比窗口高时整页再缩小，最低 0.7 倍。③ 的对比卡片 2026-11-01 列到七款后
    /// 变高，1280 × 705 的窗口按原倍数放不下「继续」按钮。
    private func scaled(_ page: NSView) -> NSView {
        let need = page.fittingSize.height
        let fit = need > 0 ? window.frame.height / need : pageScale
        return ScaledPage(page, scale: max(0.7, min(pageScale, fit)))
    }

    /// 页面骨架：居中一列（图标、标题、正文、附加内容），右下角按钮；`back` 时左下角加「← 上一步」，或放 `leading`。
    private func pageView(symbol: String, title: String, body: String?, extra: [NSView], buttons: [NSButton],
                          width: CGFloat, back: Bool = false, leading: NSButton? = nil) -> NSView {
        let icon = NSImageView(image: NSImage(systemSymbolName: symbol, accessibilityDescription: nil) ?? NSImage())
        icon.symbolConfiguration = NSImage.SymbolConfiguration(pointSize: 44, weight: .regular)
        icon.contentTintColor = .controlAccentColor
        let titleLabel = NSTextField(labelWithString: title)
        titleLabel.font = .systemFont(ofSize: 26, weight: .semibold)
        var head: [NSView] = [icon, titleLabel]
        if let body {
            let bodyLabel = NSTextField(wrappingLabelWithString: body)
            bodyLabel.font = .systemFont(ofSize: 15)
            bodyLabel.preferredMaxLayoutWidth = width
            head.append(bodyLabel)
        }
        let column = NSStackView(views: head + extra)
        column.orientation = .vertical
        column.alignment = .leading
        column.spacing = 22
        column.setCustomSpacing(12, after: icon)
        let row = NSStackView(views: buttons)
        row.spacing = 12
        let v = NSView()
        for sub in [column, row] {
            sub.translatesAutoresizingMaskIntoConstraints = false
            v.addSubview(sub)
        }
        // 上下边距只用来算这一页要多高（`scaled`）；窗口实在放不下时让位，不与固定的窗口尺寸冲突。
        let top = column.topAnchor.constraint(greaterThanOrEqualTo: v.topAnchor, constant: 24)
        let bottom = row.bottomAnchor.constraint(lessThanOrEqualTo: v.bottomAnchor, constant: -24)
        top.priority = .defaultHigh
        bottom.priority = .defaultHigh
        NSLayoutConstraint.activate([
            column.centerXAnchor.constraint(equalTo: v.centerXAnchor),
            column.centerYAnchor.constraint(equalTo: v.centerYAnchor, constant: -20),
            column.widthAnchor.constraint(equalToConstant: width),
            row.trailingAnchor.constraint(equalTo: column.trailingAnchor),
            row.topAnchor.constraint(equalTo: column.bottomAnchor, constant: 40),
            top, bottom,
        ])
        if back || leading != nil {
            let b = leading ?? button(String(localized: "← 上一步"), #selector(previousPage))
            b.translatesAutoresizingMaskIntoConstraints = false
            v.addSubview(b)
            NSLayoutConstraint.activate([
                b.leadingAnchor.constraint(equalTo: column.leadingAnchor),
                b.centerYAnchor.constraint(equalTo: row.centerYAnchor),
            ])
        }
        buttons.last?.keyEquivalent = "\r"
        return v
    }

    private func button(_ title: String, _ action: Selector) -> NSButton {
        let b = NSButton(title: title, target: self, action: action)
        b.bezelStyle = .push
        b.controlSize = .large
        return b
    }

    /// 使用须知（2026-11-07，开源后取代许可协议同意页）：全文可滚动，按界面语言显示中文或英文；只有「继续」，
    /// 点了「继续」才记下版本（2026-11-16 起，此前显示时就记）。GPL 不要求接收者同意任何条款才能运行，所以不设「同意」，
    /// 也不因关窗退出；没点「继续」就关掉的，下次打开还会出现。
    private func noticePage() -> NSView {
        scyllauiLog.notice("notice shown version=\(WelcomePolicy.noticeVersion, privacy: .public)")
        // 全文长，这一页内容列放宽到窗口宽的约八成（2026-11-02 用户看过协议页后要求横向拉宽）。
        let width = columnWidth(fraction: 0.8)
        // 全文按固定宽度排好放进滚动框，文档视图不用 Auto Layout：NSTextView 放在整页缩放（ScaledPage）里会随缩放反复
        // 改尺寸，窗口布局陷入死循环、AppKit 抛异常退出（2026-11-02 测试版实测）。
        let inner = width - 28
        let text = NSTextField(wrappingLabelWithString: LegalDocs.text(.notice) ?? "")
        text.font = .systemFont(ofSize: 11)
        text.preferredMaxLayoutWidth = inner
        let height = ceil(text.fittingSize.height)
        text.frame = NSRect(x: 10, y: 10, width: inner, height: height)
        let doc = FlippedView(frame: NSRect(x: 0, y: 0, width: inner + 20, height: height + 20))
        doc.addSubview(text)
        let scroll = NSScrollView()
        scroll.documentView = doc
        scroll.hasVerticalScroller = true
        scroll.borderType = .bezelBorder
        scroll.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            scroll.widthAnchor.constraint(equalToConstant: width),
            scroll.heightAnchor.constraint(equalToConstant: 370),
        ])
        let note = NSTextField(wrappingLabelWithString: String(localized: "之后可在「设置 › 关于」里查看使用须知与开源许可全文。"))
        note.font = .systemFont(ofSize: 13)
        note.textColor = .secondaryLabelColor
        note.preferredMaxLayoutWidth = width
        return pageView(symbol: "doc.text",
                        title: String(localized: "使用须知"),
                        body: String(localized: "使用前请了解以下几点。"),  // 开源与许可写在正文里，这里不单独提示（2026-11-21 用户要求）
                        extra: [scroll, note],
                        buttons: [button(String(localized: "继续 →"), #selector(nextPage))],
                        width: width)
    }

    /// ② 「文稿」「桌面」「下载」等文件夹的访问授权：只说明、打开系统设置页面，不主动触发授权弹窗。
    private func filesPage() -> NSView {
        let width = columnWidth()
        let note = NSTextField(wrappingLabelWithString: String(localized: "之后可以在「系统设置 › 隐私与安全性 › 文件与文件夹」里修改。"))
        note.font = .systemFont(ofSize: 13)
        note.textColor = .secondaryLabelColor
        note.preferredMaxLayoutWidth = width
        let open = NSButton(title: String(localized: "打开系统设置…"), target: self, action: #selector(openPrivacySettings))
        open.bezelStyle = .push
        return pageView(symbol: "folder.badge.questionmark",
                        title: String(localized: "允许访问「文稿」等文件夹"),
                        body: String(localized: "终端在「文稿」「桌面」「下载」等文件夹里工作时，macOS 会询问是否允许 ScyllaUI 访问。请选「允许」，否则在这些文件夹里运行的终端和 Agent 会卡住。"),
                        extra: [note, open],
                        buttons: [button(String(localized: "继续 →"), #selector(nextPage))],
                        width: width)
    }

    /// ③ 为什么要同步状态（0.4.1）：左右两张卡片对比装与不装 Hook 时侧栏的样子（直接用侧栏的终端行画），下方列出装上能得到的。
    private func whyPage() -> NSView {
        // 这一页比其他页宽：英文下「Waiting」与「Needs approval」徽标并排，卡片窄了会把状态词截断。
        let pageWidth = columnWidth(minimum: 640), column = (pageWidth - 12) / 2
        let cards = NSStackView(views: [
            sampleCard(String(localized: "装上之后"), symbol: "checkmark.circle.fill", tint: .systemGreen,
                       rows: Self.samples(hooked: true), footer: String(localized: "通知「Codex 需确认」，程序坞角标 1"), footerSymbol: "bell.badge",
                       width: column),
            sampleCard(String(localized: "不装"), symbol: "circle.dashed", tint: .secondaryLabelColor,
                       rows: Self.samples(hooked: false), footer: String(localized: "没有通知，「最近任务」为空"), footerSymbol: "bell.slash",
                       width: column),
        ])
        cards.distribution = .fillEqually
        cards.alignment = .top
        cards.spacing = 12
        cards.widthAnchor.constraint(equalToConstant: pageWidth).isActive = true
        // 两张卡片等高（英文下底部说明可能一张一行、一张两行）。
        cards.arrangedSubviews[0].heightAnchor.constraint(equalTo: cards.arrangedSubviews[1].heightAnchor).isActive = true
        // 两列与上面两张卡片对齐。
        let gains = NSGridView(views: [
            [item("waveform.path.ecg", String(localized: "实时状态：运行中、等您确认、已答完、出错"), width: column),
             item("bell.badge", String(localized: "等您确认或答完时发通知，并显示角标"), width: column)],
            [item("clock.arrow.circlepath", String(localized: "「最近任务」一键恢复已结束的对话"), width: column),
             item("text.bubble", String(localized: "侧栏显示每个任务的提示词"), width: column)],
        ])
        gains.rowSpacing = 14
        gains.columnSpacing = 12
        return pageView(symbol: "sidebar.left",
                        title: String(localized: "让侧栏知道每个 Agent 在干什么"),
                        body: String(localized: "ScyllaUI 靠 Hook 得知各款 Agent 的状态。同样七个终端，装与不装的区别："),
                        extra: [cards, gains],
                        buttons: [button(String(localized: "继续 →"), #selector(nextPage))],
                        width: pageWidth, back: true)
    }

    /// 对比卡片的样例，取值按 SidebarModel.terminalRow 的规则：装上之后有状态、徽标与提示词；不装只认得出是哪款工具，
    /// 状态「未知」、标「未同步」（Hook 没生效时侧栏的样子），副标题是目录。七款都列出（2026-11-01 用户要求），覆盖各种
    /// 状态（2026-10-30、11-01 用户要求）：运行中、需确认、答完待查看、已结束（「最近任务」里的对话行：Stopped、图标变淡、
    /// 副标题为最后一条回复）、出错、答完已看过。
    private static func samples(hooked: Bool) -> [SidebarRow] {
        let items = [("claude", String(localized: "修复登录页的报错"), "~/projects/web"),
                     ("codex", String(localized: "给接口补上单元测试"), "~/projects/api"),
                     ("kimi", String(localized: "整理本周的发布说明"), "~/projects/docs"),
                     ("grok", String(localized: "重构支付模块的回调"), "~/projects/pay"),
                     ("opencode", String(localized: "把依赖升级到最新版本"), "~/projects/app"),
                     ("copilot", String(localized: "解释这段正则表达式"), "~/projects/tools"),
                     ("antigravity", String(localized: "给首页换一套配色"), "~/projects/site")]
        return items.enumerated().map { i, s in
            var r = TaskRow(lamp: .idle, tool: SidebarModel.toolName(s.0), toolKey: s.0, state: String(localized: "未知"),
                            needsConfirm: false, unseen: false, subtitle: s.2, time: "")
            guard hooked else {
                r.unsynced = true
                return .terminal(r)
            }
            r.subtitle = s.1
            switch s.0 {
            case "claude": r.lamp = .running; r.state = "Running"
            case "codex": r.lamp = .waiting; r.state = "Waiting"; r.needsConfirm = true
            case "kimi", "antigravity": r.lamp = .ready; r.state = "Ready"; r.unseen = true
            case "grok":
                r.state = "Stopped"
                r.subtitle = String(localized: "支付回调已改好，测试全部通过")
                return .conversation(r)
            case "opencode": r.lamp = .error; r.state = "Error"
            default: r.lamp = .ready; r.state = "Ready"
            }
            return .terminal(r)
        }
    }

    /// 一张对比卡片：标题、七行样例终端（侧栏的 TaskCell；样例副标题只有一行，行高比侧栏少一行）、一行说明。
    private func sampleCard(_ title: String, symbol: String, tint: NSColor, rows: [SidebarRow],
                            footer: String, footerSymbol: String, width: CGFloat) -> NSView {
        let card = CardView()
        let head = item(symbol, title, tint: tint, font: .systemFont(ofSize: 13, weight: .semibold), width: width - 20)
        let foot = item(footerSymbol, footer, tint: .secondaryLabelColor, font: .systemFont(ofSize: 12),
                        color: .secondaryLabelColor, width: width - 20)
        let cells: [NSView] = rows.map { r in
            let cell = TaskCell(frame: .zero)
            cell.subtitleLines = 1
            cell.configure(r)
            return cell
        }
        for v in [head] + cells + [foot] {
            v.translatesAutoresizingMaskIntoConstraints = false
            card.addSubview(v)
        }
        // 卡片高度取内容所需（低优先级的「尽量矮」），与另一张等高时底部留白。
        let shrink = card.heightAnchor.constraint(equalToConstant: 0)
        shrink.priority = .defaultLow
        var constraints = [
            head.topAnchor.constraint(equalTo: card.topAnchor, constant: 10),
            head.leadingAnchor.constraint(equalTo: card.leadingAnchor, constant: 10),
            foot.leadingAnchor.constraint(equalTo: head.leadingAnchor),
            foot.bottomAnchor.constraint(lessThanOrEqualTo: card.bottomAnchor, constant: -10),
            shrink,
        ]
        var above = head.bottomAnchor
        for cell in cells {
            constraints += [
                cell.topAnchor.constraint(equalTo: above, constant: 2),
                cell.leadingAnchor.constraint(equalTo: card.leadingAnchor, constant: 4),
                cell.trailingAnchor.constraint(equalTo: card.trailingAnchor, constant: -4),
                cell.heightAnchor.constraint(equalToConstant: ScyllaUIStyle.taskRowHeight - ScyllaUIStyle.subtitleLineHeight),
            ]
            above = cell.bottomAnchor
        }
        constraints.append(foot.topAnchor.constraint(equalTo: above, constant: 2))
        NSLayoutConstraint.activate(constraints)
        return card
    }

    /// ④ 安装状态同步：三条说明、四款的列表（即设置的 Agent 页，不带每行按钮）与 Codex 提醒；「查看改动并安装…」走 HookRepairFlow。
    private func hooksPage() -> NSView {
        let width = columnWidth()
        let list = AgentPane(core: core, showsButtons: false)
        agents = list
        // 列表铺满内容列（2026-10-28），多出的宽度给路径。
        list.view.widthAnchor.constraint(equalToConstant: width).isActive = true
        let notes = NSStackView(views: [
            item("terminal", String(localized: "只在各款 Agent 的配置文件里加几条命令，只在 ScyllaUI 的终端里起作用，您在别的终端里照常使用"), width: width),
            item("doc.text.magnifyingglass", String(localized: "写入前先列出改动，确认后才写，并备份原文件"), width: width),
            item("arrow.uturn.backward.circle", String(localized: "随时可以在「设置 › Agent」里撤销"), width: width),
        ])
        notes.orientation = .vertical
        notes.alignment = .leading
        notes.spacing = 12
        // 行首的橙色警示符号（用户要求）：说明文字多时，这一行不被看漏。
        let codexNote = item("exclamationmark.triangle.fill",
                             String(localized: "装好后打开 Codex，它会询问是否信任新的 Hook：按下方向键选中第 2 项「Trust all and continue」再按回车，Codex 的状态同步才会生效。"),
                             tint: .systemOrange, font: .systemFont(ofSize: 13), width: width)
        codexNote.isHidden = true
        // 后面接着「遇到问题」一页时，这里的按钮翻到下一页而不是关掉欢迎窗口。
        let last = pages.last == .hooks
        let done = button(String(localized: "以后再说"), last ? #selector(finish) : #selector(nextPage))
        let install = button(String(localized: "查看改动并安装…"), #selector(install))
        install.isHidden = true
        bridgeBox.state = .on
        bridgeBox.isHidden = true
        bridgeBox.font = .systemFont(ofSize: 13)
        bridgeBox.target = self
        bridgeBox.action = #selector(bridgeToggled)
        let box = bridgeBox
        list.onUpdate = { [weak self] statuses in
            self?.hookStatuses = statuses
            let claude = statuses.first { $0.tool == "claude" }
            box.isHidden = claude == nil || claude?.state == "absent" || claude?.bridge == nil || claude?.bridge == "on"
                || Prefs.hooksRemoved.contains("claude")
            // 有可修复的（或 Codex 待信任、或要接入 Claude 的额度）时「查看改动并安装…」是默认按钮，否则只剩「完成」。
            let fixable = statuses.contains { $0.fixable || $0.state == "untrusted" } || (!box.isHidden && box.state == .on)
            install.isHidden = !fixable
            install.keyEquivalent = fixable ? "\r" : ""
            done.keyEquivalent = fixable ? "" : "\r"
            done.title = statuses.contains { $0.broken } ? String(localized: "以后再说")
                : last ? String(localized: "完成") : String(localized: "继续 →")
            codexNote.isHidden = !WelcomePolicy.showsCodexTrustNote(codexState: statuses.first { $0.tool == "codex" }?.state)
        }
        let v = pageView(symbol: "dot.radiowaves.left.and.right",
                         title: String(localized: "安装状态同步"),
                         body: nil,
                         extra: [notes, list.view, bridgeBox, codexNote],
                         buttons: [done, install],
                         width: width, back: true)
        install.keyEquivalent = ""
        done.keyEquivalent = "\r"
        list.viewWillAppear()
        return v
    }

    /// ⑤ 遇到问题怎么反馈（2026-11-02 用户要求，文案经用户修改定稿）：哪些情况欢迎报告、怎么报告（「帮助 › 报告问题…」）、
    /// 诊断包不含什么，以及不经 App 的直接渠道。1.0.0 之前每次启动、之后每天第一次打开时出现。
    private func feedbackPage() -> NSView {
        let width = columnWidth()
        func section(_ title: String, _ rows: [NSView]) -> NSView {
            let header = NSTextField(labelWithString: title)
            header.font = .systemFont(ofSize: 15, weight: .semibold)
            let stack = NSStackView(views: [header] + rows)
            stack.orientation = .vertical
            stack.alignment = .leading
            stack.spacing = 10
            return stack
        }
        let cases = section(String(localized: "这些情况都欢迎报告"), [
            item("sidebar.left", String(localized: "侧栏的状态和 Agent 实际不一致，比如已经答完还显示 Running"), width: width),
            item("terminal", String(localized: "终端显示错乱、卡住，或者输入没反应"), width: width),
            item("xmark.octagon", String(localized: "App 闪退，或者下次打开时提示「上次意外退出」"), width: width),
            item("lightbulb", String(localized: "用着别扭、看不懂的地方，或者您希望有的功能"), width: width),
        ])
        let steps = section(String(localized: "怎么报告"), [
            item("1.circle", String(localized: "点菜单栏「帮助 › 报告问题…」。出问题时弹出的提示框里也有同名按钮。"), width: width),
            item("2.circle", String(localized: "写下发生了什么、大约几点、怎么复现。时间最重要，日志靠时间对上。"), width: width),
            item("3.circle", String(localized: "先看一眼诊断包：里面有版本、各 Agent 的状态同步、最近 3 天的日志和崩溃报告，可以在 Finder 里逐个打开查看。"), width: width),
            item("4.circle", String(localized: "选用邮件或 GitHub 发出。用的是您自己的账号；GitHub 上的问题是公开的。"), width: width),
        ])
        let note = NSTextField(wrappingLabelWithString: String(localized: "诊断包在您的 Mac 上生成，不含提示词、回复和密钥，ScyllaUI 不会自动上传任何内容。也可以直接发邮件到 \(FeedbackChannel.email)，或在 \(FeedbackChannel.issuesPage) 提交。"))
        note.font = .systemFont(ofSize: 13)
        note.textColor = .secondaryLabelColor
        note.preferredMaxLayoutWidth = width
        note.isSelectable = true
        return pageView(symbol: "exclamationmark.bubble",
                        title: String(localized: "遇到问题，告诉我们"),
                        body: String(localized: "您的每一条反馈对我们都非常宝贵和重要。"),
                        extra: [cases, steps, note],
                        buttons: [button(String(localized: "完成"), #selector(finish))],
                        width: width, back: page > 1)
    }

    /// 一条带系统符号的说明：符号与第一行文字对齐，文字折行时第二行与第一行左对齐。
    private func item(_ symbol: String, _ text: String, tint: NSColor = .controlAccentColor,
                      font: NSFont = .systemFont(ofSize: 14), color: NSColor = .labelColor, width: CGFloat) -> NSView {
        let icon = NSImageView(image: NSImage(systemSymbolName: symbol, accessibilityDescription: nil) ?? NSImage())
        icon.symbolConfiguration = NSImage.SymbolConfiguration(pointSize: font.pointSize, weight: .regular)
        icon.contentTintColor = tint
        let label = NSTextField(wrappingLabelWithString: text)
        label.font = font
        label.textColor = color
        let iconWidth = font.pointSize + 6
        label.preferredMaxLayoutWidth = width - iconWidth - 6
        let v = NSView()
        for sub in [icon, label] {
            sub.translatesAutoresizingMaskIntoConstraints = false
            v.addSubview(sub)
        }
        NSLayoutConstraint.activate([
            icon.leadingAnchor.constraint(equalTo: v.leadingAnchor),
            icon.widthAnchor.constraint(equalToConstant: iconWidth),
            icon.centerYAnchor.constraint(equalTo: label.firstBaselineAnchor, constant: -font.capHeight / 2),
            label.leadingAnchor.constraint(equalTo: icon.trailingAnchor, constant: 6),
            label.trailingAnchor.constraint(equalTo: v.trailingAnchor),
            label.topAnchor.constraint(equalTo: v.topAnchor),
            label.bottomAnchor.constraint(equalTo: v.bottomAnchor),
            v.widthAnchor.constraint(equalToConstant: width),
        ])
        return v
    }

    @objc private func nextPage() { next() }

    @objc private func previousPage() {
        page -= 1
        showPage()
    }

    @objc private func finish() {
        acted()
        window.close()
    }

    @objc private func bridgeToggled() { agents?.onUpdate?(hookStatuses) }

    @objc private func install() {
        HookRepairFlow.run(.repair, core: core, bridge: !bridgeBox.isHidden && bridgeBox.state == .on ? true : nil)
    }

    @objc private func openPrivacySettings() {
        if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_FilesAndFolders") {
            NSWorkspace.shared.open(url)
        }
    }
}

/// 原点在左上的视图：协议全文从顶部排起。
private final class FlippedView: NSView {
    override var isFlipped: Bool { true }
}

/// 引导页整页等比放大：页面按原生字号排版，这一层的 bounds 设为 frame 的 1/scale，子视图整体放大绘制，文字仍是矢量。
/// 内容的 frame 直接设为缩放后的 bounds（Auto Layout 贴边按 frame 算、不认缩放后的 bounds），内容内部照常用 Auto Layout。
private final class ScaledPage: NSView {
    private let scale: CGFloat
    private let content: NSView

    init(_ content: NSView, scale: CGFloat) {
        self.scale = scale
        self.content = content
        super.init(frame: .zero)
        content.translatesAutoresizingMaskIntoConstraints = true
        content.autoresizingMask = []
        addSubview(content)
    }

    required init?(coder: NSCoder) { fatalError() }

    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        setBoundsSize(NSSize(width: newSize.width / scale, height: newSize.height / scale))
        content.frame = bounds
    }
}

/// 对比卡片的底：圆角、浅底与细边，颜色随深浅外观（在 draw 里取动态颜色）。
private final class CardView: NSView {
    override func draw(_ dirtyRect: NSRect) {
        let path = NSBezierPath(roundedRect: bounds.insetBy(dx: 0.5, dy: 0.5), xRadius: 8, yRadius: 8)
        NSColor.labelColor.withAlphaComponent(0.04).setFill()
        path.fill()
        NSColor.separatorColor.setStroke()
        path.stroke()
    }
}
