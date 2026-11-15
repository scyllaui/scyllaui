// 侧栏（M1 第 3 块，docs/design.md 第 5 节实现说明）。
// SidebarFeed：在串行队列 dev.scyllaui.sidebar 上维护模型、建树、算差分；主线程忙时，后续推送在队列里合并成一批。
// SidebarController：主线程上的 NSOutlineView 数据源与代理，只执行算好的增删移并原地改可见行，不整表 reload。

import AppKit
import os

private let signposter = OSSignposter(subsystem: "dev.scyllaui.app", category: .pointsOfInterest)

/// 一条带 trace 的终端推送，用于记录「Hook → 侧栏」延迟（docs/design.md 4.5 节）。
struct HookTrace {
    let terminal: String
    let tool: String
    let state: String
    let event: String
    let t0: UInt64
    let daemonRecv: UInt64
    let received: UInt64
}

/// 交给主线程的一批变化。
struct SidebarBatch {
    let tree: SidebarTree
    let changes: SidebarChanges
    let messages: Int
    let modelUs: UInt64
    let traces: [HookTrace]
    /// 侧栏底部提示栏（M1 第 7 块）。
    let banner: HookBanner
    /// 各终端的标题栏状态行信息（M2 第 2 块）。
    let status: [String: StatusInfo]
    /// 「需确认」的终端数（不受搜索与筛选影响）。
    let pending: Int
    /// 本批体现了一次搜索框或筛选的变化：变化发生的时间（`search_latency` 打点，M2 第 4 块）。
    let searchStart: UInt64?
    /// 同上：应答到达读线程、在模型中采用、出批的时间（分段打点）。
    let searchReply: UInt64?
    let searchApplied: UInt64?
    let builtAt: UInt64
    /// 本批由延后出批的定时器发出（距上一批不足 minInterval）。用于判断约 200 ms 的 `queue` 尖峰是否来自定时器被推迟。
    let viaTimer: Bool
}

final class SidebarFeed {
    private let queue = DispatchQueue(label: "dev.scyllaui.sidebar", qos: .userInitiated)
    private let model = SidebarModel()
    private let core: CoreClient
    /// 主线程上的控制器；只在主线程访问。
    private weak var controller: SidebarController?
    // 以下只在 queue 上访问。
    private var shown = SidebarTree.empty
    private var shownBanner = HookBanner()
    private var shownStatus: [String: StatusInfo] = [:]
    private var shownPending = -1
    private var dirty = false
    private var inFlight = false
    private var messages = 0
    private var traces: [HookTrace] = []
    /// 上一批交给主线程的时间与是否已排了延后的出批。距上一批不足 minInterval 的变化合并进下一批，
    /// 推送密集时每秒最多约 125 批；空闲时第一条立即出批，不增加延迟。
    private static let minInterval: UInt64 = 8_000
    private var lastFlushUs: UInt64 = 0
    private var flushScheduled = false
    /// 延后出批用的定时器：显式给 1 ms 容差，不用 `asyncAfter` 的默认容差（系统可能把它与别的定时器合并推迟，
    /// 2026-11-14，第二轮长跑里约 200 ms 的 `app_ui` 尖峰的可能原因之一）。
    private lazy var flushTimer: DispatchSourceTimer = {
        let t = DispatchSource.makeTimerSource(queue: queue)
        t.setEventHandler { [weak self] in
            self?.flushScheduled = false
            self?.flush(viaTimer: true)
        }
        t.activate()
        return t
    }()
    /// 搜索框或筛选变化的时间，等到相应结果进了一批再交出（M2 第 4 块）。
    private var searchStart: UInt64?
    private var searchReply: UInt64?
    private var searchApplied: UInt64?
    private var searchStarts: [Int: UInt64] = [:]

    init(core: CoreClient, controller: SidebarController) {
        self.core = core
        self.controller = controller
    }

    /// CoreClient 读线程按到达顺序调用（快照、推送）。
    func ingest(_ msg: [String: Any], received: UInt64) {
        queue.async {
            self.messages += 1
            self.run(self.model.apply(msg))
            if msg["type"] as? String == "terminal.upsert", let tr = msg["trace"] as? [String: Any],
               let t = msg["terminal"] as? [String: Any],
               let t0 = (tr["hook_t0_us"] as? NSNumber)?.uint64Value,
               let recv = (tr["daemon_recv_us"] as? NSNumber)?.uint64Value {
                let agent = t["agent"] as? [String: Any] ?? [:]
                let event = ((tr["event"] as? String) ?? "-") + ((tr["notification_type"] as? String).map { "(\($0))" } ?? "")
                self.traces.append(HookTrace(
                    terminal: t["terminal_id"] as? String ?? "-", tool: agent["tool"] as? String ?? "-",
                    state: agent["state"] as? String ?? "-", event: event, t0: t0, daemonRecv: recv, received: received))
            }
            self.dirty = true
            self.flush()
        }
    }

    /// 不带推送地重建一次（窗口变为前台时调用）：最近任务的显示窗口随时间推移，过期条目在此移出，不用定时器。
    func touch() {
        queue.async {
            self.dirty = true
            self.flush()
        }
    }

    /// 搜索框内容变化（主线程调用）：`query` 为 nil 即清除。每次都发一次 `search`，只采用最新一次的应答。
    func search(_ query: String?) {
        let start = CoreClient.nowUs()
        queue.async {
            let seq = self.model.setSearch(query)
            self.searchStarts = [:]
            guard let query else {
                self.searchStart = start
                self.dirty = true
                self.flush()
                return
            }
            self.searchStarts[seq] = start
            self.core.request(["type": "search", "query": query, "limit": SidebarModel.searchLimit + 1]) { [weak self] reply in
                guard let self else { return }
                let received = CoreClient.nowUs()
                self.queue.async {
                    guard self.model.applySearch(reply, seq: seq) else { return }
                    self.searchStart = self.searchStarts.removeValue(forKey: seq)
                    self.searchReply = received
                    self.searchApplied = CoreClient.nowUs()
                    self.dirty = true
                    self.flush()
                }
            }
        }
    }

    /// 「只看待处理」开关（主线程调用）。
    func setPendingOnly(_ on: Bool) {
        let start = CoreClient.nowUs()
        queue.async {
            self.model.setPendingOnly(on)
            self.searchStart = start
            self.dirty = true
            self.flush()
        }
    }

    private func run(_ effects: [SidebarModel.Effect]) {
        for case let .loadRecent(since) in effects {
            let req: [String: Any] = ["type": "recent.query", "since": since, "limit": SidebarModel.recentLimit]
            // 应答在读线程回调，与推送同序进入本队列。
            core.request(req) { [weak self] reply in
                guard let self else { return }
                self.queue.async {
                    self.model.applyRecent(reply)
                    self.dirty = true
                    self.flush()
                }
            }
            dirty = true   // 「正在加载」状态
        }
        if !effects.isEmpty { flush() }
    }

    /// 主线程空闲时出一批；有一批在主线程应用时什么也不做，应用完会回来再出下一批。
    private func flush(viaTimer: Bool = false) {
        guard dirty, !inFlight else { return }
        let now = CoreClient.nowUs()
        if now < lastFlushUs + Self.minInterval {
            if !flushScheduled {
                flushScheduled = true
                let wait = Int(lastFlushUs + Self.minInterval - now)
                flushTimer.schedule(deadline: .now() + .microseconds(wait), repeating: .never, leeway: .milliseconds(1))
            }
            return
        }
        dirty = false
        let start = CoreClient.nowUs()
        let removed = Prefs.hooksRemoved
        model.setHooksRemoved(removed)
        let tree = model.buildTree()
        let changes = SidebarChanges.diff(from: shown, to: tree)
        let banner = model.banner()
        let status = model.statusInfos(tree)
        let batch = SidebarBatch(tree: tree, changes: changes, messages: messages,
                                 modelUs: CoreClient.nowUs() - start, traces: traces, banner: banner, status: status,
                                 pending: model.pendingTotal, searchStart: searchStart, searchReply: searchReply,
                                 searchApplied: searchApplied, builtAt: CoreClient.nowUs(), viaTimer: viaTimer)
        messages = 0
        traces = []
        searchStart = nil
        searchReply = nil
        searchApplied = nil
        // 被筛掉的终端行不在差分里，它变了也要出批：右侧正在显示它时，窗口标题与足迹要跟随（SidebarTree.terminalRows）。
        guard !changes.isEmpty || !batch.traces.isEmpty || banner != shownBanner || status != shownStatus
            || batch.pending != shownPending || tree.terminalRows != shown.terminalRows else { return }
        // 只在真正出批时记时间：没有变化、不出批的那次不该让下一批多等一个间隔。
        lastFlushUs = now
        shownPending = batch.pending
        shownStatus = status
        if banner != shownBanner {
            scyllauiLog.notice("hooks_banner inaccurate=\(banner.inaccurate.joined(separator: ","), privacy: .public)")
        }
        shown = tree
        shownBanner = banner
        inFlight = true
        DispatchQueue.main.async { [weak self] in
            self?.controller?.apply(batch)
            self?.queue.async {
                self?.inFlight = false
                self?.flush()
            }
        }
    }
}

/// NSOutlineView 的 item：按 key 缓存，保证身份稳定（展开状态、选中随之保留）。NSOutlineView 不持有 item。
final class SidebarItem: NSObject {
    let key: String
    init(_ key: String) { self.key = key }
}

/// 点侧栏后键盘归侧栏：上下键在终端行间移动（右侧同步显示），回车把键盘交回终端；点终端区域同样交回
/// （docs/design.md 第 5 节）。其余按键沿用 NSOutlineView 的默认处理（左右键折叠展开）。
private final class SidebarOutlineView: NSOutlineView {
    var onStep: ((Int) -> Void)?
    var onActivate: (() -> Void)?

    /// 点侧栏任意位置（项目行、最近任务行、空白处，不只终端行）都先把键盘交给侧栏：NSOutlineView 自己只在点中可选中的行
    /// 时才接过键盘，点别处后上下键仍进终端（2026-10-12 用户试用发现：必须先点中某个终端行，上下键才生效）。
    override func mouseDown(with event: NSEvent) {
        window?.makeFirstResponder(self)
        super.mouseDown(with: event)
    }

    override func keyDown(with event: NSEvent) {
        let plain = event.modifierFlags.intersection([.command, .control, .option, .shift]).isEmpty
        switch event.keyCode {
        case 126 where plain: onStep?(-1)
        case 125 where plain: onStep?(1)
        case 36, 76: onActivate?()
        default: super.keyDown(with: event)
        }
    }
}

final class SidebarController: NSObject, NSOutlineViewDataSource, NSOutlineViewDelegate, NSSearchFieldDelegate {
    let scrollView = NSScrollView()
    /// 侧栏顶部的搜索框与「只看待处理」开关（M2 第 4 块），由工作区摆放。
    let searchField = NSSearchField()
    let pendingButton = NSButton()
    /// 搜索框内容变化：去掉首尾空白后的查询，空为 nil。
    var onSearch: ((String?) -> Void)?
    var onPendingOnly: ((Bool) -> Void)?
    private var sentQuery: String?
    /// 用户点了某个终端行或用上下键选中：参数为终端 ID（工作区据此切换，键盘留在侧栏，再经 `show(terminalID:)` 回到这里）。
    var onSelectTerminal: ((String) -> Void)?
    /// 侧栏有键盘焦点时按回车：把键盘交回当前终端。
    var onActivate: (() -> Void)?
    /// 「当前任务」标题的「+」：在侧栏里还没有的目录新建（工作区弹出选文件夹窗口）。
    var onOpenFolder: (() -> Void)?
    var onRename: ((_ id: String, _ name: String) -> Void)?
    /// 双击「最近任务」中的一条：一键恢复该对话（参数为对话 ID，M1 第 6 块；单击只选中，docs/design.md 10.5 节决策 15）。
    var onResumeConversation: ((Int64) -> Void)?
    /// 用户单击选中了某条对话（最近任务、搜索结果）或选中回到终端（nil）：提示词足迹跟随（M4a 追加块 9）。
    var onSelectConversation: ((Int64?) -> Void)?
    /// 「最近任务」与搜索结果里对话行的悬停删除按钮（docs/design.md 10.5 节决策 4）：对话 ID 与确认框里显示的标题。
    var onDeleteConversation: ((_ id: Int64, _ title: String) -> Void)?
    /// 置顶 / 取消置顶「当前任务」里的项目（维护期 0.3.5）：项目 ID 与目标状态。
    var onPinProject: ((_ id: Int64, _ pinned: Bool) -> Void)?
    /// 当前终端这一行变了（任务标题、工具）：窗口标题与终端上方的窄标题栏跟随（docs/design.md 第 5 节）。
    /// 该行尚未出现（新建后推送未到）或没有当前终端时为 nil。
    var onCurrentRow: ((TaskRow?) -> Void)?
    /// 底部提示栏的内容（M1 第 7 块），每批都回调。
    var onBanner: ((HookBanner) -> Void)?
    /// 点了终端行的「未同步」（M4b 块 5 P2）：终端 ID 与前台那款 AI 的工具键。
    var onUnsynced: ((_ terminalID: String, _ tool: String) -> Void)?
    /// 各终端的标题栏状态行信息（M2 第 2 块），每批都回调。
    var onStatus: (([String: StatusInfo]) -> Void)?
    /// 「需确认」的终端数有变化（Dock 角标，M2 第 1 块）；与侧栏同口径。
    var onPendingCount: ((Int) -> Void)?
    private var pendingCount = -1
    /// 右侧正在显示的终端（工作区为准）；侧栏选中跟随它。
    private(set) var selectedTerminalID: String?
    /// 用户单击选中的对话行（键）：选中留在这一行，直到点了终端行或工作区切换终端（docs/design.md 10.5 节决策 15）。
    private var selectedConversation: String? {
        didSet {
            if selectedConversation != oldValue { onSelectConversation?(selectedConversation.flatMap(SidebarKey.conversationID)) }
        }
    }
    private var reportedRow: TaskRow??

    private let outline = SidebarOutlineView()
    private var tree = SidebarTree.empty
    private var items: [String: SidebarItem] = [:]
    /// 每批应用后核对 NSOutlineView 的行与模型树（只在测试数据目录下开启）。
    private let selfCheck: Bool
    private var applying = false
    /// 用户亲手收起的分组（点击、键盘）；应用推送时程序做的展开收起不算。分组暂时不在树里（如搜索中）也记着，
    /// 回到树里时保持收起（M2 第 4 块）。
    private var userCollapsed: Set<String> = []

    init(selfCheck: Bool) {
        self.selfCheck = selfCheck
        super.init()
        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("main"))
        column.resizingMask = .autoresizingMask
        outline.addTableColumn(column)
        outline.outlineTableColumn = column
        outline.headerView = nil
        // plain：选中态与行背景由 SidebarRowView 自己画（sourceList 会用系统的整块高亮，绕过 drawSelection）。
        outline.style = .plain
        outline.intercellSpacing = NSSize(width: 0, height: 0)
        outline.rowSizeStyle = .custom
        outline.usesAutomaticRowHeights = false
        outline.floatsGroupRows = false
        outline.indentationPerLevel = 14
        outline.selectionHighlightStyle = .regular
        outline.backgroundColor = .clear
        outline.allowsEmptySelection = true
        outline.allowsMultipleSelection = false
        outline.columnAutoresizingStyle = .uniformColumnAutoresizingStyle
        // 展开时不按缩进加宽大纲列，列宽始终贴合侧栏（否则右侧的徽标与时间被裁掉）。
        outline.autoresizesOutlineColumn = false
        outline.dataSource = self
        outline.delegate = self
        outline.target = self
        outline.action = #selector(clicked(_:))
        outline.doubleAction = #selector(doubleClicked(_:))
        outline.onStep = { [weak self] d in self?.step(d) }
        outline.onActivate = { [weak self] in self?.onActivate?() }
        scrollView.documentView = outline
        scrollView.drawsBackground = false
        scrollView.hasVerticalScroller = true
        scrollView.hasHorizontalScroller = false
        scrollView.autohidesScrollers = true
        scrollView.translatesAutoresizingMaskIntoConstraints = false

        searchField.placeholderString = String(localized: "搜索")
        searchField.sendsSearchStringImmediately = true
        searchField.sendsWholeSearchString = false
        searchField.target = self
        searchField.action = #selector(searchChanged(_:))
        searchField.delegate = self
        searchField.translatesAutoresizingMaskIntoConstraints = false
        pendingButton.setButtonType(.pushOnPushOff)
        pendingButton.isBordered = false
        pendingButton.image = NSImage(systemSymbolName: "line.3.horizontal.decrease.circle", accessibilityDescription: nil)
        pendingButton.alternateImage = NSImage(systemSymbolName: "line.3.horizontal.decrease.circle.fill", accessibilityDescription: nil)
        pendingButton.imageScaling = .scaleProportionallyUpOrDown
        pendingButton.toolTip = String(localized: "只看待处理（需确认、待查看）")
        pendingButton.setAccessibilityLabel(String(localized: "只看待处理"))
        pendingButton.target = self
        pendingButton.action = #selector(pendingToggled(_:))
        pendingButton.translatesAutoresizingMaskIntoConstraints = false
    }

    // MARK: 搜索与只看待处理（M2 第 4 块，docs/design.md 第 5 节「实现（M2 第 4 块）」）

    /// Cmd+F：键盘交给搜索框并全选已有文字。
    func focusSearch() {
        searchField.window?.makeFirstResponder(searchField)
        searchField.currentEditor()?.selectAll(nil)
    }

    @objc private func searchChanged(_ sender: Any?) {
        let q = searchField.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        let query = q.isEmpty ? nil : String(q.prefix(200))
        guard query != sentQuery else { return }
        sentQuery = query
        onSearch?(query)
    }

    private func clearSearch() {
        searchField.stringValue = ""
        searchChanged(nil)
    }

    @objc private func pendingToggled(_ sender: NSButton) {
        sender.contentTintColor = sender.state == .on ? .controlAccentColor : nil
        onPendingOnly?(sender.state == .on)
    }

    /// 回车打开第一条（当前任务里第一个终端，没有时恢复搜索结果的第一条）并清空搜索；Esc 清空并把键盘交回终端。
    func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
        guard control === searchField else { return false }
        switch selector {
        case #selector(NSResponder.insertNewline(_:)):
            openFirst()
            return true
        case #selector(NSResponder.cancelOperation(_:)):
            clearSearch()
            onActivate?()
            return true
        default:
            return false
        }
    }

    private func openFirst() {
        func first(_ section: String, _ id: (String) -> Bool) -> String? {
            tree.kids(section).lazy.flatMap { self.tree.kids($0) }.first(where: id)
        }
        if let key = first(SidebarKey.current, { SidebarKey.terminalID($0) != nil }), let id = SidebarKey.terminalID(key) {
            clearSearch()
            onSelectTerminal?(id)
            onActivate?()
        } else if sentQuery != nil, let key = first(SidebarKey.recent, { SidebarKey.conversationID($0) != nil }),
                  let id = SidebarKey.conversationID(key) {
            clearSearch()
            onResumeConversation?(id)
        }
    }

    private func item(_ key: String) -> SidebarItem {
        if let i = items[key] { return i }
        let i = SidebarItem(key)
        items[key] = i
        return i
    }

    private func key(_ item: Any?) -> String { (item as? SidebarItem)?.key ?? SidebarKey.root }

    // MARK: 应用一批变化

    func apply(_ batch: SidebarBatch) {
        let state = signposter.beginInterval("sidebar_apply")
        let start = CoreClient.nowUs()
        let first = tree.kids(SidebarKey.root).isEmpty
        let old = tree
        applying = true
        tree = batch.tree
        if first {
            outline.reloadData()
            outline.expandItem(nil, expandChildren: true)
            outline.sizeLastColumnToFit()
        }
        var opsUs: UInt64 = 0, expandUs: UInt64 = 0, refreshUs: UInt64 = 0
        if !first && !batch.changes.isEmpty {
            let t0 = CoreClient.nowUs()
            outline.beginUpdates()
            for op in batch.changes.ops {
                switch op {
                case let .remove(p, i, _):
                    outline.removeItems(at: [i], inParent: parentItem(p), withAnimation: [])
                case let .insert(p, i, _):
                    outline.insertItems(at: [i], inParent: parentItem(p), withAnimation: [])
                case let .move(p, from, to, _):
                    outline.moveItem(at: from, inParent: parentItem(p), to: to, inParent: parentItem(p))
                }
            }
            let t1 = CoreClient.nowUs()
            // 展开放在同一次更新里：排版与行高推迟到 endUpdates 只算一次（逐个展开时每次都重算，M2 验收后优化）。
            for k in batch.changes.inserted where !userCollapsed.contains(k) { outline.expandItem(item(k), expandChildren: true) }
            if !batch.changes.inserted.isEmpty {
                // 连带展开的子分组里有用户收起过的：收回去。
                for k in userCollapsed where tree.rows[k] != nil {
                    if let i = items[k], outline.isItemExpanded(i) { outline.collapseItem(i) }
                }
            }
            outline.endUpdates()
            let t2 = CoreClient.nowUs()
            for k in batch.changes.updated { refresh(k) }
            (opsUs, expandUs, refreshUs) = (t1 - t0, t2 - t1, CoreClient.nowUs() - t2)
            for k in Set(old.rows.keys).subtracting(tree.rows.keys) { items[k] = nil }
            // 兄弟节点增删会改变「最后一个」，连线随之重画（只有可见行有行视图）。
            if !batch.changes.ops.isEmpty { redrawGuides() }
        }
        applying = false
        keepSelection()
        reportCurrentRow()
        onBanner?(batch.banner)
        onStatus?(batch.status)
        if batch.pending != pendingCount {
            pendingCount = batch.pending
            onPendingCount?(batch.pending)
        }
        let applyUs = CoreClient.nowUs() - start
        var displayUs: UInt64 = 0
        if !batch.traces.isEmpty || batch.searchStart != nil {
            // 与 S0 的计时口径一致：界面绘制完成才算「侧栏已更新」。
            let d = CoreClient.nowUs()
            scrollView.displayIfNeeded()
            displayUs = CoreClient.nowUs() - d
        }
        signposter.endInterval("sidebar_apply", state)
        scyllauiLog.notice("sidebar_apply msgs=\(batch.messages) ops=\(batch.changes.ops.count) updated=\(batch.changes.updated.count) inserted=\(batch.changes.inserted.count) rows=\(self.outline.numberOfRows) model_us=\(batch.modelUs) apply_us=\(applyUs) ops_us=\(opsUs) expand_us=\(expandUs) refresh_us=\(refreshUs) display_us=\(displayUs)")
        let shown = CoreClient.nowUs()
        if let s0 = batch.searchStart, s0 <= shown {
            // 分段：发出请求 → 应答到读线程 → 模型采用 → 出批 → 主线程开始应用（main_wait）→ 应用与绘制完成。
            let reply = batch.searchReply.map { $0 >= s0 ? $0 - s0 : 0 }
            let queued = batch.searchReply.flatMap { r in batch.searchApplied.map { $0 >= r ? $0 - r : 0 } }
            let wait = start >= batch.builtAt ? start - batch.builtAt : 0
            scyllauiLog.notice("search_latency total_us=\(shown - s0) rows=\(self.outline.numberOfRows) reply_us=\(reply.map(String.init) ?? "-", privacy: .public) to_model_us=\(queued.map(String.init) ?? "-", privacy: .public) model_us=\(batch.modelUs) main_wait_us=\(wait) apply_us=\(applyUs) display_us=\(displayUs)")
        }
        // 分段（2026-11-14）：读线程收到 → 出批（queue）→ 主线程开始应用（main_wait）→ 应用 → 绘制；窗口不可见时
        // 系统会节流（App Nap），统计延迟时按 visible 区分。2026-11-15 加 active（App 是否在前台）与 via_timer（本批是否由
        // 延后出批的定时器发出），验证「App 不在前台时定时器被系统合并推迟」的推测（第三轮长跑的观察项）。
        let mainWait = start >= batch.builtAt ? start - batch.builtAt : 0
        let visible = scrollView.window.map { $0.occlusionState.contains(.visible) && !$0.isMiniaturized } ?? false
        let active = NSApp.isActive
        for t in batch.traces where t.t0 <= t.daemonRecv && t.daemonRecv <= shown {
            signposter.emitEvent("hook_to_sidebar", "\(t.tool, privacy: .public) \(t.event, privacy: .public) \(shown - t.t0) us")
            scyllauiLog.notice("hook_latency terminal=\(t.terminal, privacy: .public) tool=\(t.tool, privacy: .public) event=\(t.event, privacy: .public) state=\(t.state, privacy: .public) total_us=\(shown - t.t0) hook_to_daemon_us=\(t.daemonRecv - t.t0) daemon_to_app_us=\(t.received >= t.daemonRecv ? t.received - t.daemonRecv : 0) app_ui_us=\(shown >= t.received ? shown - t.received : 0) queue_us=\(batch.builtAt >= t.received ? batch.builtAt - t.received : 0) main_wait_us=\(mainWait) apply_us=\(applyUs) display_us=\(displayUs) visible=\(visible) active=\(active) via_timer=\(batch.viaTimer)")
        }
        if selfCheck { check() }
    }

    private func parentItem(_ key: String) -> Any? { key == SidebarKey.root ? nil : item(key) }

    /// 内容变化：只改可见行的字段；不可见的行在显示时按新数据配置。
    private func refresh(_ key: String) {
        guard let i = items[key], let row = tree.rows[key] else { return }
        let r = outline.row(forItem: i)
        guard r >= 0 else { return }
        if let cell = outline.view(atColumn: 0, row: r, makeIfNecessary: false) as? SidebarCell {
            cell.configure(row)
        } else {
            outline.reloadData(forRowIndexes: [r], columnIndexes: [0])
        }
    }

    /// 工作区切换了右侧终端（新建、`Cmd+1…9`、关闭后改显示另一个）：侧栏选中跟随。
    func show(terminalID: String?) {
        selectedTerminalID = terminalID
        selectedConversation = nil
        keepSelection()
        reportCurrentRow()
    }

    private func reportCurrentRow() {
        var row: TaskRow?
        // 按右侧显示的终端取行，不看它此刻是否在列表里（搜索、「只看待处理」会把它筛掉）。
        if let id = selectedTerminalID { row = tree.terminalRows[id] }
        guard reportedRow != .some(row) else { return }
        reportedRow = .some(row)
        onCurrentRow?(row)
    }

    /// 侧栏选中与右侧显示的终端保持一致；该行尚未出现（推送未到）时等下一批再选。
    /// 选中位置变了（新建、改归项目后该行挪动）时把它滚到可见处。用户单击选中的对话行还在时，选中留在那一行。
    private func keepSelection() {
        if let key = selectedConversation {
            if tree.rows[key] != nil {
                let r = outline.row(forItem: item(key))
                if r >= 0 && outline.selectedRow != r { programmatic { outline.selectRowIndexes([r], byExtendingSelection: false) } }
                return
            }
            selectedConversation = nil
        }
        guard let id = selectedTerminalID, tree.rows[SidebarKey.terminal(id)] != nil else {
            if outline.selectedRow >= 0 { programmatic { outline.deselectAll(nil) } }
            return
        }
        let r = outline.row(forItem: item(SidebarKey.terminal(id)))
        if r >= 0 && outline.selectedRow != r {
            programmatic { outline.selectRowIndexes([r], byExtendingSelection: false) }
            outline.scrollRowToVisible(r)
        }
    }

    private var selectingByCode = false
    private func programmatic(_ body: () -> Void) {
        selectingByCode = true
        body()
        selectingByCode = false
    }

    /// 上下键：在「当前任务」可见的终端行之间移动一格（跳过折叠项目里的，到头循环，与 Cmd+Shift+[ / ] 一致），
    /// 等同用户点选。
    private func step(_ delta: Int) {
        let rows = terminalOrder.map { outline.row(forItem: item(SidebarKey.terminal($0))) }.filter { $0 >= 0 }
        guard !rows.isEmpty else { return }
        // 点空白处会清掉选中、单击会选中对话行：都从右侧正在显示的终端接着移动，而不是回到第一个。
        let current = outline.selectedRow >= 0 && selectedConversation == nil ? outline.selectedRow
            : selectedTerminalID.map { outline.row(forItem: item(SidebarKey.terminal($0))) } ?? -1
        let i = rows.firstIndex(of: current)
        let r = rows[i.map { ($0 + delta + rows.count) % rows.count } ?? 0]
        guard r != outline.selectedRow else { return }
        outline.selectRowIndexes([r], byExtendingSelection: false)
        outline.scrollRowToVisible(r)
    }

    /// 把键盘交给侧栏（点到侧栏列表外侧的边距时，工作区调用）。
    func focusList() {
        outline.window?.makeFirstResponder(outline)
    }

    /// 「当前任务」里终端的显示顺序（`Cmd+1…9` 按它切换）。
    var terminalOrder: [String] {
        tree.kids(SidebarKey.current).flatMap { tree.kids($0) }.compactMap(SidebarKey.terminalID)
    }

    /// 用 NSOutlineView 自己的行缓存（item(atRow:)、level(forRow:)）与模型树按展开状态展平的结果逐行比对。
    private func check() {
        let start = CoreClient.nowUs()
        var expect: [(String, Int)] = []
        func walk(_ k: String, _ level: Int) {
            for c in tree.kids(k) {
                expect.append((c, level))
                if let i = items[c], outline.isItemExpanded(i) { walk(c, level + 1) }
            }
        }
        walk(SidebarKey.root, 0)
        var bad: String?
        if expect.count != outline.numberOfRows {
            bad = "行数 \(outline.numberOfRows) ≠ \(expect.count)"
        } else {
            for (r, (k, level)) in expect.enumerated() {
                let got = (outline.item(atRow: r) as? SidebarItem)?.key
                if got != k || outline.level(forRow: r) != level {
                    bad = "第 \(r) 行为 \(got ?? "nil")（层级 \(outline.level(forRow: r))），应为 \(k)（层级 \(level)）"
                    break
                }
            }
        }
        if let bad {
            scyllauiLog.error("sidebar_check mismatch \(bad, privacy: .public)")
        } else {
            scyllauiLog.notice("sidebar_check ok rows=\(expect.count) us=\(CoreClient.nowUs() - start)")
        }
    }

    // MARK: 连线

    private func redrawGuides() {
        outline.enumerateAvailableRowViews { rowView, _ in rowView.needsDisplay = true }
    }

    /// 按当前树和单元格位置算出一行的连线（docs/design.md 第 5 节：两级直角细线）。
    private func guide(_ rowView: SidebarRowView) -> RowGuide? {
        let r = outline.row(for: rowView)
        guard r >= 0, let item = outline.item(atRow: r) as? SidebarItem, let row = tree.rows[item.key] else { return nil }
        var g = RowGuide()
        switch row {
        case .project:
            guard outline.isItemExpanded(item), !tree.kids(item.key).isEmpty,
                  let cell = rowView.view(atColumn: 0) as? ProjectCell else { return nil }
            let icon = rowView.convert(cell.iconFrame, from: cell)
            g.stem = (icon.midX, icon.maxY + 2)
        case .terminal, .conversation:
            guard let cell = rowView.view(atColumn: 0) as? TaskCell,
                  let parent = outline.parent(forItem: item) as? SidebarItem else { return nil }
            // 状态灯是线上的节点：横线连到灯边（留 2 pt），竖线从灯下方拐到副标题前。
            let lamp = rowView.convert(cell.lampCenter, from: cell)
            let sub = rowView.convert(cell.subtitleFirstLine, from: cell)
            let r = LampView.dot / 2 + 2
            // 项目文件夹图标中心：项目行单元格左边 + 图标半宽。
            let trunk = outline.frameOfCell(atColumn: 0, row: outline.row(forItem: parent)).minX + ProjectCell.iconWidth / 2
            g.branch = (trunk, lamp.y, lamp.x - r, tree.kids(parent.key).last == item.key)
            g.inner = (lamp.x, lamp.y + r, sub.midY, sub.minX - 4)
        default:
            return nil
        }
        return g
    }

    // MARK: 点击

    /// 单击：项目行与分区折叠展开；终端行切换、对话行只选中（选中由 NSOutlineView 处理）。
    @objc private func clicked(_ sender: Any?) {
        let r = outline.clickedRow
        guard r >= 0, let i = outline.item(atRow: r) as? SidebarItem, tree.rows[i.key]?.isExpandable == true else { return }
        if outline.isItemExpanded(i) { outline.collapseItem(i) } else { outline.expandItem(i) }
    }

    /// 双击：最近任务行一键恢复（docs/design.md 10.5 节决策 15，取代单击即恢复）；终端行第 1 行的名称（没有命名时为
    /// 工具名）就地改名（docs/design.md 第 5 节，2026-10-25 起名称在第 1 行），双击状态、副标题或空白处只选中。
    @objc private func doubleClicked(_ sender: Any?) {
        let r = outline.clickedRow
        guard r >= 0, let i = outline.item(atRow: r) as? SidebarItem else { return }
        if let id = SidebarKey.conversationID(i.key) {
            // 与回车打开相同：先清空搜索，否则恢复出的新终端被搜索筛掉、侧栏里看不到（2026-10-29 长跑发现）。
            if sentQuery != nil { clearSearch() }
            onResumeConversation?(id)
            return
        }
        guard let id = SidebarKey.terminalID(i.key),
              let cell = outline.view(atColumn: 0, row: r, makeIfNecessary: false) as? TaskCell,
              let event = NSApp.currentEvent else { return }
        let p = cell.convert(event.locationInWindow, from: nil)
        guard cell.titleFrame.insetBy(dx: -4, dy: -3).contains(p) else { return }
        beginRename(id)
    }

    /// 某段对话所在的行：最近任务（对话行）优先，其次是绑定它的终端行；都没有为 nil。提示词足迹的栏头用。
    func row(forConversation id: Int64) -> TaskRow? {
        if case .conversation(let r)? = tree.rows[SidebarKey.conversation(id)] { return r }
        // 终端行含被筛掉的（SidebarTree.terminalRows）。
        for r in tree.terminalRows.values where r.conversationID == id { return r }
        return nil
    }

    func beginRename(_ id: String) {
        guard case .terminal(let row)? = tree.rows[SidebarKey.terminal(id)], let it = items[SidebarKey.terminal(id)] else {
            return
        }
        let r = outline.row(forItem: it)
        guard r >= 0, let cell = outline.view(atColumn: 0, row: r, makeIfNecessary: true) as? TaskCell else { return }
        // 初值为第 1 行此刻显示的名称（没改过名时为任务标题），在它上面改；留空恢复自动。
        cell.beginRename(initial: row.name ?? row.heading ?? "") { [weak self] name in
            if let name { self?.onRename?(id, name) }
        }
    }

    // MARK: NSOutlineViewDataSource

    func outlineView(_ outlineView: NSOutlineView, numberOfChildrenOfItem item: Any?) -> Int {
        tree.kids(key(item)).count
    }

    func outlineView(_ outlineView: NSOutlineView, child index: Int, ofItem item: Any?) -> Any {
        self.item(tree.kids(key(item))[index])
    }

    func outlineView(_ outlineView: NSOutlineView, isItemExpandable item: Any) -> Bool {
        tree.rows[key(item)]?.isExpandable ?? false
    }

    // MARK: NSOutlineViewDelegate

    func outlineView(_ outlineView: NSOutlineView, viewFor tableColumn: NSTableColumn?, item: Any) -> NSView? {
        guard let row = tree.rows[key(item)] else { return nil }
        let id: NSUserInterfaceItemIdentifier
        let make: () -> SidebarCell
        switch row {
        case .section: (id, make) = (SectionCell.id, { SectionCell() })
        case .project: (id, make) = (ProjectCell.id, { ProjectCell() })
        case .terminal, .conversation: (id, make) = (TaskCell.id, { TaskCell() })
        case .empty: (id, make) = (NoteCell.id, { NoteCell() })
        }
        let cell = (outlineView.makeView(withIdentifier: id, owner: nil) as? SidebarCell) ?? make()
        cell.configure(row)
        if let section = cell as? SectionCell {
            section.onAdd = key(item) == SidebarKey.current ? { [weak self] _ in self?.onOpenFolder?() } : nil
        }
        if let project = cell as? ProjectCell {
            project.onPin = SidebarKey.currentProjectID(key(item)).map { id in { [weak self] pinned in self?.onPinProject?(id, pinned) } }
        }
        if let task = cell as? TaskCell {
            if case .conversation(let c) = row, let id = SidebarKey.conversationID(key(item)) {
                task.onDelete = { [weak self] in self?.onDeleteConversation?(id, c.taskTitle ?? c.subtitle) }
            } else {
                task.onDelete = nil
            }
            // 点按时按这一行此刻的数据取工具（行内容会原地刷新，不重设闭包）。
            if let id = SidebarKey.terminalID(key(item)) {
                task.onUnsynced = { [weak self] in
                    guard let self, case .terminal(let r)? = self.tree.rows[SidebarKey.terminal(id)], let tool = r.fgTool else { return }
                    self.onUnsynced?(id, tool)
                }
            } else {
                task.onUnsynced = nil
            }
        }
        return cell
    }

    func outlineView(_ outlineView: NSOutlineView, rowViewForItem item: Any) -> NSTableRowView? {
        let rv = (outlineView.makeView(withIdentifier: SidebarRowView.id, owner: nil) as? SidebarRowView) ?? {
            let v = SidebarRowView()
            v.identifier = SidebarRowView.id
            return v
        }()
        rv.guide = { [weak self] in self?.guide($0) }
        return rv
    }

    func outlineViewItemDidExpand(_ notification: Notification) {
        if !applying, let k = (notification.userInfo?["NSObject"] as? SidebarItem)?.key { userCollapsed.remove(k) }
        redrawGuides()
    }

    func outlineViewItemDidCollapse(_ notification: Notification) {
        if !applying, let k = (notification.userInfo?["NSObject"] as? SidebarItem)?.key { userCollapsed.insert(k) }
        redrawGuides()
    }

    func outlineView(_ outlineView: NSOutlineView, heightOfRowByItem item: Any) -> CGFloat {
        switch tree.rows[key(item)] {
        case .section?: return ScyllaUIStyle.sectionRowHeight
        case .project?: return ScyllaUIStyle.projectRowHeight
        case .terminal?, .conversation?: return ScyllaUIStyle.taskRowHeight
        default: return ScyllaUIStyle.noteRowHeight
        }
    }

    func outlineView(_ outlineView: NSOutlineView, isGroupItem item: Any) -> Bool {
        if case .section? = tree.rows[key(item)] { return true }
        return false
    }

    /// 终端行与对话行可选中：对话行单击只选中、双击才恢复（docs/design.md 10.5 节决策 15）。
    func outlineView(_ outlineView: NSOutlineView, shouldSelectItem item: Any) -> Bool {
        switch tree.rows[key(item)] {
        case .terminal?, .conversation?: return true
        default: return false
        }
    }

    /// 只有用户点击才通知工作区；程序选中（跟随工作区）不回调，避免来回。选中对话行只记下，不通知。
    func outlineViewSelectionDidChange(_ notification: Notification) {
        guard !applying, !selectingByCode else { return }
        guard outline.selectedRow >= 0, let i = outline.item(atRow: outline.selectedRow) as? SidebarItem else {
            selectedConversation = nil
            return
        }
        if SidebarKey.conversationID(i.key) != nil {
            selectedConversation = i.key
            return
        }
        guard let id = SidebarKey.terminalID(i.key) else { return }
        selectedConversation = nil
        selectedTerminalID = id
        onSelectTerminal?(id)
    }
}
