// 工作区（M1 第 4 块，docs/design.md 第 5 节）：一个窗口，左侧侧栏，右侧显示当前终端；持有全部终端，
// 负责新建（Shell / 四款 AI）、切换、改名与关闭。切走的终端移出视图层但进程照常运行（暂停渲染）。
// M3 起终端由 scyllauid 持有（docs/design.md 第 7 节「实现（M3）」）：退出 App 只断开、不确认；重启后接回后台仍持有的终端。

import AppKit
import GhosttyKit

final class WorkspaceController: NSObject, NSWindowDelegate, NSSplitViewDelegate {
    let window: NSWindow
    private let runtime: GhosttyRuntime
    private let app: ghostty_app_t
    private let core = CoreClient()
    /// 系统通知与 Dock 角标（M2 第 1 块）。
    private let notifier = Notifier()
    /// 标题栏状态行（M2 第 2 块，docs/design.md 第 5 节「实现（M2 第 2 块）」）。
    private let statusField = NSTextField(labelWithString: "")
    private let statusHolder = TitlebarAlignedView(frame: NSRect(x: 0, y: 0, width: 10, height: 22))
    /// 状态行各段的悬停区域与文字（2026-11-01）：区域没变就只改文字，不重挂（2026-11-02，见 `TooltipText`）。
    private var statusTips: [(rect: NSRect, owner: TooltipText)] = []
    /// 标题栏右侧的功能按钮组（2026-11-21 用户要求）：以后的功能都往这一排加；2026-11-23 起为右侧栏的四个开关（5.12 节）。
    private let toolsHolder = TitlebarAlignedView(frame: NSRect(x: 0, y: 0, width: 1, height: 22))
    private lazy var statusButton = Self.titlebarButton(symbol: "info.circle", name: String(localized: "状态"),
                                                        shortcut: "⇧⌘I", target: self, action: #selector(toggleStatusPanel(_:)))
    private lazy var filesButton = Self.titlebarButton(symbol: "folder", name: String(localized: "文件"),
                                                       shortcut: "⇧⌘E", target: self, action: #selector(toggleFilesPanel(_:)))
    private lazy var diffButton = Self.titlebarButton(symbol: "plus.forwardslash.minus", name: String(localized: "更改"),
                                                      shortcut: "⇧⌘D", target: self, action: #selector(toggleDiffPanel(_:)))
    private lazy var historyButton = Self.titlebarButton(symbol: "clock.arrow.circlepath", name: String(localized: "历史提示词"),
                                                         shortcut: "⌘Y", target: self, action: #selector(togglePromptHistory(_:)))
    private var statusInfos: [String: StatusInfo] = [:]
    /// 当前终端最近一次 `proc.stats` 的结果。
    private var stats: (terminal: String, cpu: Double?, mem: Int64?, processes: Int)?
    /// 只在窗口可见且右侧有终端时运行的 2 秒计时器（docs/design.md 4.5 节的唯一例外）。
    private var statusTimer: Timer?
    private let sidebar: SidebarController
    private let feed: SidebarFeed
    /// 终端区（右侧窄标题栏之下）：只放当前终端或空态。
    private let container = NSView()
    private let emptyState = NSStackView()
    /// 右侧栏（docs/design.md 5.12 节）：终端区右边的一栏，状态、文件、Diff、历史提示词四选一，共用一个宿主。
    private let rightHost = RightPanelHost()
    /// 右侧栏占的位置：贴右边，宽度即右侧栏的当前宽度；终端贴它的左边，宿主平时也贴它的左边。
    private let slot = NSLayoutGuide()
    private var rightPanel: RightPanel?
    /// 历史提示词（M4a 追加块 9）：右侧栏之一，菜单「终端 › 历史提示词」开关。
    private let promptPanel = PromptHistoryPanel()
    private let statusPanel = StatusPanel()
    private let filesPanel = FilesPanel()
    /// 右侧栏开着「文件」「更改」，或开着状态栏且在 Git 仓库里时，对项目根的 FSEvents 流（docs/design.md 5.12 节「自动刷新」）。
    private var folderWatch: FolderWatch?
    /// 列表宽（四栏共用）与查看宽（打开文件时），docs/design.md 5.12 节「宽度」。
    private var listWidth = WorkspaceController.savedPromptWidth
    private var viewerWidth = WorkspaceController.savedViewerWidth
    /// 查看器放大：宿主盖住终端区，终端尺寸不变。
    private var zoomed = false
    private var hostToSlot: NSLayoutConstraint?
    private var hostToRight: NSLayoutConstraint?
    /// 正在按模式重设右侧栏宽度（自动隐藏文件树会再触发一次，不重入）。
    private var applyingRightWidth = false
    private let diffPanel = DiffPanel()
    /// 改动列表（Diff 栏与文件树的 M、A 标记共用，docs/design.md 5.12 节「Diff」）。
    private let changes = ChangesStore()
    /// 当前终端这一行（右侧栏按它取路径、分支与工具）。
    private var currentRow: TaskRow?
    /// 后台崩溃后已在原终端接回的对话与时间（防止恢复即崩溃时反复重来）。
    private var respawnResumed: [Int64: Date] = [:]
    /// 四款 CLI 此刻的状态同步（快照与 `hooks.upsert`）：重建时说明没能接回的原因、「未同步」的去向据此判断（M4b 块 5）。
    private var hookStatuses: [SBHookStatus] = []
    /// 重建时没能接回 AI 的终端的工具键（M4b 块 5 P2）：攒一小会儿再合并成一个说明（后台崩溃时 `terminal.respawned` 逐个推来）。
    private var lostAgentTools: [String] = []
    private var lostAgentReport: DispatchWorkItem?
    private var containerToEdge: NSLayoutConstraint?
    private var containerToPanel: NSLayoutConstraint?
    /// 右侧栏宽度（可拖动，四栏共用，docs/design.md 10.5 节决策 20、5.12 节）。
    private lazy var promptWidth = slot.widthAnchor.constraint(equalToConstant: Self.savedPromptWidth)
    /// 历史提示词读的对话：侧栏选中的对话优先，否则当前终端绑定的；`promptKey` 变了才重读（副标题即最新提示词）。
    private var selectedConversation: Int64?
    private var currentRowConversation: Int64?
    /// 当前终端这一行没有对话时，历史提示词栏按它说明场景（`PromptHistoryPanel.noConversationMessage`）。
    private var currentRowForeground: (tool: String?, unsynced: Bool, command: String?) = (nil, false, nil)
    private var promptKey: String?
    /// 当前终端这一行上次是否在运行中、属于哪段对话：一轮结束时历史提示词补读一次（`reloadPromptsAtTurnEnd`）。
    private var promptRowRunning: (conversation: Int64?, running: Bool) = (nil, false)
    private let split = SidebarSplitView()
    private let sideView = SidebarBackgroundView()
    /// 侧栏底部的状态同步提示栏（M1 第 7 块）。
    private let hookBar = HookStatusBar()
    /// 侧栏已收起：侧栏视图移出分隔视图（见 toggleSidebarPane）。
    private var sidebarCollapsed = false
    /// 全部终端（终端 ID → 会话）；顺序以侧栏为准。
    private var sessions: [String: TerminalSession] = [:]
    private var currentID: String?
    /// 已报给后台的 `terminal.focus` 与等待停留期满的上报（见 updateFocus）。
    private var reportedFocus: String?
    private var focusSettle: DispatchWorkItem?
    /// 还没处理第一份快照：据它接回后台仍持有的终端，没有时才新开 Home Shell（M3）。
    private var restorePending = true
    /// 后台快照与推送中存活的终端。attach 意外退出时据此判断要不要重新接回（M3）。
    private var alive: Set<String> = []
    /// 各终端最近一次自动重新接回的时间：2 秒内不再自动重试，避免后台连不上时反复重建。
    private var reattachedAt: [String: Date] = [:]
    /// 在途的一键恢复请求（对话 ID）：双击、连点只发一次；收到快照（重连）时清空，应答随断线丢失也不会卡住。
    private var resuming: Set<Int64> = []
    /// 系统深浅（M2 第 5 块）：交给 libghostty，`theme = light:A,dark:B` 据此选定主题。
    private var appearanceObserver: NSKeyValueObservation?
    private var systemDark = false
    private var settings: SettingsWindowController?
    private var welcome: WelcomeWindowController?
    /// 启动时读到的上次心跳（有即上次意外退出）与据第一份快照定下、等欢迎窗口关闭后再弹的提示（M4b 块 7）。
    private var lastAlive: Date?
    private var heartbeat: Timer?
    private var exitNotice: UnexpectedExit?
    /// 「报告问题」（2026-11-02，docs/design.md 10.6 节「问题反馈」）：窗口，以及摘要要用的 Hook 状态原样、「状态可能不准」的条目
    /// 与上次意外退出的时间。
    private var reportWindow: ProblemReportWindow?
    private var hookJSON: [[String: Any]] = []
    private var banner = HookBanner()
    private var lastExitAt: Date?

    init(runtime: GhosttyRuntime, app: ghostty_app_t) {
        self.runtime = runtime
        self.app = app
        sidebar = SidebarController(selfCheck: core.isolated)
        feed = SidebarFeed(core: core, controller: sidebar)
        // 第一次打开（没有保存过窗口位置）时约 1100 × 700、居中；之后按 setFrameAutosaveName 记住用户调整的大小与位置
        // （M4a，docs/design.md 10.4 节决策 9）。
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1100, height: 700),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false)
        super.init()

        // 先读出上次的心跳再写入这次的：正常退出时 applicationWillTerminate 会删掉它（M4b 块 7）。
        lastAlive = Prefs.aliveAt
        Prefs.aliveAt = Date()
        heartbeat = Timer.scheduledTimer(withTimeInterval: 60, repeats: true) { _ in Prefs.aliveAt = Date() }
        heartbeat?.tolerance = 5

        // 在 CoreClient 读线程上回调：侧栏模型在自己的队列里处理全部消息；前台进程补读只关心推送所属的终端。
        let feed = self.feed
        let notifier = self.notifier
        core.onMessage = { [weak self] msg, received in
            let type = msg["type"] as? String
            // 通知在读线程直接提交，不经主线程（docs/design.md 4.3 节「实现（M2 第 1 块）」）。
            switch type {
            case "notify.post": return notifier.post(msg, received: received)
            case "notify.withdraw": return notifier.withdraw(msg["ids"] as? [String] ?? [])
            case "snapshot":
                notifier.clearAll()
            default: break
            }
            // 设置的 Agent 页随 Hook 状态刷新（M4a，docs/design.md 10.4 节决策 6）。
            // 撤销过的工具状态回到正常（修复或在别处装回）即恢复提示（决策 6）。
            if type == "hooks.upsert" || type == "snapshot", let hooks = msg["hooks"] as? [[String: Any]] {
                DispatchQueue.main.async {
                    self?.hookStatuses = hooks.compactMap(SBHookStatus.init(json:))
                    self?.refreshStatusPanel()
                    self?.hookJSON = hooks
                    let ok = Set(hooks.filter { $0["state"] as? String == "ok" }.compactMap { $0["tool"] as? String })
                    if !Prefs.hooksRemoved.isDisjoint(with: ok) { Prefs.hooksRemoved.subtract(ok) }
                    NotificationCenter.default.post(name: AgentPane.hooksChanged, object: nil, userInfo: ["hooks": hooks])
                }
            }
            feed.ingest(msg, received: received)
            if type == "snapshot" { DispatchQueue.main.async { self?.resuming.removeAll() } }
            guard type == "snapshot" || type == "terminal.upsert" || type == "terminal.respawned" else { return }
            let id = (msg["terminal"] as? [String: Any])?["terminal_id"] as? String
            DispatchQueue.main.async { self?.route(msg, terminalID: id) }
        }
        core.onError = { text in scyllauiLog.error("scyllauid: \(text, privacy: .public)") }
        GeometryLog.shared = GeometryLog(directory: core.logDir)
        scyllauiLog.file = AppLogFile(directory: core.logDir)
        GeometryLog.shared?.write("launch", "pid=\(getpid())")
        registerFixtureTerminals()
        core.start()

        buildWindow()
        sidebar.onSelectTerminal = { [weak self] id in self?.show(id, takeKeyboard: false) }
        sidebar.onActivate = { [weak self] in
            guard let self, let view = self.currentID.flatMap({ self.sessions[$0]?.view }) else { return }
            self.window.makeFirstResponder(view)
        }
        sidebar.onOpenFolder = { [weak self] in self?.openFolder(nil) }
        sidebar.onRename = { [weak self] id, name in self?.core.rename(id: id, name: name) }
        sidebar.onResumeConversation = { [weak self] id in self?.resume(conversation: id) }
        sidebar.onSearch = { [weak self] q in self?.feed.search(q) }
        sidebar.onPendingOnly = { [weak self] on in self?.feed.setPendingOnly(on) }
        sidebar.onDeleteConversation = { [weak self] id, title in self?.confirmDelete(id, title: title) }
        sidebar.onPinProject = { [weak self] id, pinned in self?.core.setPinned(projectID: id, pinned: pinned) }
        sidebar.onBanner = { [weak self] banner in
            self?.banner = banner
            self?.hookBar.show(banner)
        }
        sidebar.onPendingCount = { [weak self] n in self?.notifier.setBadge(n) }
        sidebar.onStatus = { [weak self] infos in
            guard let self else { return }
            self.statusInfos = infos
            self.refreshStatusPanel()
            self.updateStatusSampling()
            self.refreshStatusLine()
        }
        notifier.onOpen = { [weak self] id in self?.openFromNotification(id) }
        // 设置里改了状态栏的段或角标开关（M4a）：即时刷新状态行、调整采样，按开关显示角标。
        NotificationCenter.default.addObserver(forName: Prefs.changed, object: nil, queue: .main) { [weak self] _ in
            guard let self else { return }
            self.refreshStatusLine()
            self.updateStatusSampling()
            self.notifier.applyBadge()
        }
        hookBar.onDetails = { [weak self] items in self?.showInaccurateDetails(items) }
        // 「未同步」（M4b 块 5）：Codex 待信任时引导用户去这一行的 Codex 里信任（重开或 /hooks，见 openCodexHooks），其余走这款工具的修复确认框。
        // 2026-11-20 起徽标按这个进程真实的同步情况显示（docs/design.md 4.7 节「会话是否真的在同步」）：配置正常、只是这个进程
        // 读不到的，说明怎么开新会话或重开；配置已失效但它还在同步的（「下次失效」），修复确认框里多说一句。
        sidebar.onUnsynced = { [weak self] id, tool in
            guard let self else { return }
            let sync = self.statusInfos[id]?.fgSync
            if let sync, SyncGuide.needsReopen(sync) {
                self.guideReopen(terminal: id, tool: tool, sync: sync)
            } else if tool == "codex", self.hookStatuses.first(where: { $0.tool == tool })?.state == "untrusted" {
                self.openCodexHooks(preferred: id, afterRepair: false)
            } else if sync == nil, let h = self.hookStatuses.first(where: { $0.tool == tool }), h.state == "ok", h.bridgeLost {
                // 状态在同步、只是额度桥被改掉了（2026-11-22）：只重装桥；被改过的（broken）只说明，不自动处理。
                if h.bridge == "replaced" { HookRepairFlow.runBridge(install: true, core: self.core) } else { self.explainBridge(h) }
            } else {
                HookRepairFlow.run(.repair, tools: [tool], core: self.core, stillSynced: sync == "old")
            }
        }
        HookRepairFlow.openCodexHooks = { [weak self] id, after in self?.openCodexHooks(preferred: id, afterRepair: after) }
        HookRepairFlow.afterRepair = { [weak self] tools in self?.reportReopenNeeded(tools) }
        HookRepairFlow.runningAgents = { [weak self] in Set(self?.statusInfos.values.compactMap(\.fgAgent) ?? []) }
        // 窗口标题跟随当前终端这一行：有用户命名时为名称，否则即任务标题，与侧栏副标题一致（docs/design.md 第 5 节）。
        sidebar.onCurrentRow = { [weak self] row in
            self?.window.title = row?.name ?? row?.subtitle ?? "ScyllaUI"
            self?.currentRow = row
            self?.refreshStatusPanel()
            self?.refreshFilesPanel()
            self?.refreshDiffPanel()
            self?.currentRowConversation = row?.conversationID
            self?.currentRowForeground = (row?.toolKey, row?.unsynced ?? false, row?.fgCommand)
            // 没有对话时按前台的场景重排说明；有对话时最新提示词变了才重读。
            let key = row?.conversationID.map { "\($0):\(row?.subtitle ?? "")" }
                ?? "-1:\(row?.toolKey ?? ""):\(row?.unsynced ?? false):\(row?.fgCommand ?? "")"
            self?.refreshPromptHistory(key: key)
            self?.reloadPromptsAtTurnEnd(row)
        }
        sidebar.onSelectConversation = { [weak self] id in
            self?.selectedConversation = id
            self?.refreshPromptHistory(key: nil)
        }
        promptPanel.onClose = { [weak self] in self?.show(nil) }
        statusPanel.onClose = { [weak self] in self?.show(nil) }
        statusPanel.onOpenChanges = { [weak self] in self?.show(.diff) }
        filesPanel.onClose = { [weak self] in self?.show(nil) }
        filesPanel.request = { [weak self] req, done in
            guard let self else { return done(nil) }
            self.core.ask(req) { reply in DispatchQueue.main.async { done(reply) } }
        }
        filesPanel.treeWidth = Self.savedTreeWidth
        // 文件树宽与 Diff 的改动列表宽是同一个值（D2）：在哪一栏拖都两边一起改，来回切换时分隔线不跳。
        filesPanel.onTreeWidth = { [weak self] w in
            Prefs.filesTreeWidth = Double(w)
            self?.diffPanel.listWidth = w
        }
        filesPanel.onLayoutChange = { [weak self] in
            guard let self else { return }
            if !self.filesPanel.contentShown { self.setZoom(false) }
            self.applyRightWidth(grow: true)
        }
        filesPanel.onZoom = { [weak self] in self.map { $0.setZoom(!$0.zoomed) } }
        filesPanel.onEscape = { [weak self] in
            guard let self, self.zoomed else { return false }
            self.setZoom(false)
            return true
        }
        filesPanel.onInsert = { [weak self] path in self?.insertIntoCurrent(GhosttyInput.shellEscape(path)) }
        diffPanel.onClose = { [weak self] in self?.show(nil) }
        let ask: ([String: Any], @escaping ([String: Any]?) -> Void) -> Void = { [weak self] req, done in
            guard let self else { return done(nil) }
            self.core.ask(req) { reply in DispatchQueue.main.async { done(reply) } }
        }
        diffPanel.request = ask
        changes.request = ask
        diffPanel.listWidth = Self.savedTreeWidth
        diffPanel.onListWidth = { [weak self] w in
            Prefs.filesTreeWidth = Double(w)
            self?.filesPanel.treeWidth = w
        }
        diffPanel.onLayoutChange = { [weak self] in
            guard let self else { return }
            if !self.diffPanel.contentShown { self.setZoom(false) }
            self.applyRightWidth(grow: true)
        }
        diffPanel.onZoom = { [weak self] in self.map { $0.setZoom(!$0.zoomed) } }
        diffPanel.onEscape = { [weak self] in
            guard let self, self.zoomed else { return false }
            self.setZoom(false)
            return true
        }
        diffPanel.onOpenInFiles = { [weak self] path in
            guard let self else { return }
            self.show(.files)
            self.filesPanel.openAndReveal(path)
        }
        changes.onUpdate = { [weak self] root in
            guard let self else { return }
            if self.diffPanel.root == root { self.diffPanel.update(self.changes.snapshot(root)) }
            if self.rightPanel == .status, self.currentProjectRoot == root { self.refreshStatusPanel() }
            if self.filesPanel.tree?.root.path == root {
                self.filesPanel.setMarks(self.changes.marks(root), dirty: self.changes.dirtyDirs(root))
            }
        }
        rightHost.onResize = { [weak self] w, done in self?.resizePromptPanel(w, done: done) }
        rightHost.onResetWidth = { [weak self] in self?.resetRightWidth() }
        promptPanel.onCopy = { text in
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(text, forType: .string)
        }
        promptPanel.onInsert = { [weak self] text in self?.insertIntoCurrent(text) }
        // 第一个终端等第一份快照到了再定：接回后台仍持有的，或新开 Home Shell（route → restore）。
        window.makeKeyAndOrderFront(nil)
        // 首次运行引导与开屏页（M4a 块 4）：出现时通知授权请求推迟到它关闭之后，避免系统弹窗压在开屏上。
        welcome = WelcomeWindowController.showIfNeeded(over: window, runtime: runtime, core: core) { [weak self] in
            self?.welcome = nil
            self?.notifier.requestAuthorization()
            self?.showExitNotice()
        }
        if welcome == nil { notifier.requestAuthorization() }
    }

    // MARK: 窗口

    private func buildWindow() {
        split.isVertical = true
        split.dividerStyle = .thin
        split.delegate = self
        // 侧栏底色取终端背景色略压暗，外观按底色亮度定，文字始终可读（docs/design.md 第 5 节视觉第二版）。
        let side = sideView
        side.onMouseDown = { [weak self] in self?.sidebar.focusList() }
        let list = sidebar.scrollView
        let search = sidebar.searchField, pending = sidebar.pendingButton
        // ScyllaUI 字标资源随侧栏外观自动切换；方案 B 字形整体放大到 266.75 × 47.75 pt，字高 34.5 pt，
        // 在侧栏内水平居中（docs/design.md 1.1 节）。
        let wordmark = NSImageView()
        wordmark.image = NSImage(named: "ScyllaWordmark")
        wordmark.imageScaling = .scaleProportionallyDown
        wordmark.translatesAutoresizingMaskIntoConstraints = false
        wordmark.setAccessibilityLabel("ScyllaUI")
        side.addSubview(wordmark)
        // 字标下方居中一行像素字：版本号在左、副标题在右（开屏同一句英文，品牌字标的一部分，不本地化；
        // docs/design.md 10.5 节决策 13，2026-10-30 改为像素字、同一行）。
        let brandLine = PixelBrandLine(version: Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "",
                                       subtitle: SplashView.subtitle)
        brandLine.translatesAutoresizingMaskIntoConstraints = false
        side.addSubview(brandLine)
        side.addSubview(search)
        side.addSubview(pending)
        side.addSubview(list)
        hookBar.translatesAutoresizingMaskIntoConstraints = false
        side.addSubview(hookBar)
        NSLayoutConstraint.activate([
            // 字标框距侧栏顶 14 pt；其下版本号与副标题一行，搜索框在其下方 12 pt；搜索与筛选保持原有排列。
            wordmark.topAnchor.constraint(equalTo: side.topAnchor, constant: 14),
            wordmark.centerXAnchor.constraint(equalTo: side.centerXAnchor),
            wordmark.widthAnchor.constraint(equalToConstant: 266.75),
            wordmark.heightAnchor.constraint(equalToConstant: 47.75),
            brandLine.topAnchor.constraint(equalTo: wordmark.bottomAnchor, constant: 4),
            brandLine.centerXAnchor.constraint(equalTo: side.centerXAnchor),
            search.topAnchor.constraint(equalTo: brandLine.bottomAnchor, constant: 12),
            search.leadingAnchor.constraint(equalTo: side.leadingAnchor, constant: 10),
            search.trailingAnchor.constraint(equalTo: pending.leadingAnchor, constant: -6),
            pending.trailingAnchor.constraint(equalTo: side.trailingAnchor, constant: -10),
            pending.centerYAnchor.constraint(equalTo: search.centerYAnchor),
            pending.widthAnchor.constraint(equalToConstant: 18),
            pending.heightAnchor.constraint(equalToConstant: 18),
            list.topAnchor.constraint(equalTo: search.bottomAnchor, constant: 6),
            list.bottomAnchor.constraint(equalTo: hookBar.topAnchor),
            list.leadingAnchor.constraint(equalTo: side.leadingAnchor, constant: 6),
            list.trailingAnchor.constraint(equalTo: side.trailingAnchor),
            hookBar.leadingAnchor.constraint(equalTo: side.leadingAnchor),
            hookBar.trailingAnchor.constraint(equalTo: side.trailingAnchor),
            hookBar.bottomAnchor.constraint(equalTo: side.bottomAnchor),
        ])
        buildEmptyState()
        // 右侧：终端区（0.3.5 起去掉终端上方的窄标题栏，docs/design.md 第 5 节「实现（维护期 0.3.5）」）。
        let right = NSView()
        container.translatesAutoresizingMaskIntoConstraints = false
        right.addSubview(container)
        // 右侧栏在终端区右边，默认隐藏（M4a 追加块 9；2026-11-23 起四栏共用宿主，5.12 节）。宿主排在终端之后（上层）。
        rightHost.translatesAutoresizingMaskIntoConstraints = false
        rightHost.isHidden = true
        right.addSubview(rightHost)
        right.addLayoutGuide(slot)
        containerToEdge = container.trailingAnchor.constraint(equalTo: right.trailingAnchor)
        containerToPanel = container.trailingAnchor.constraint(equalTo: slot.leadingAnchor)
        NSLayoutConstraint.activate([
            container.topAnchor.constraint(equalTo: right.topAnchor),
            container.leadingAnchor.constraint(equalTo: right.leadingAnchor),
            containerToEdge!,
            container.bottomAnchor.constraint(equalTo: right.bottomAnchor),
            slot.topAnchor.constraint(equalTo: right.topAnchor),
            slot.bottomAnchor.constraint(equalTo: right.bottomAnchor),
            slot.trailingAnchor.constraint(equalTo: right.trailingAnchor),
            promptWidth,
            rightHost.topAnchor.constraint(equalTo: right.topAnchor),
            rightHost.bottomAnchor.constraint(equalTo: right.bottomAnchor),
            rightHost.trailingAnchor.constraint(equalTo: right.trailingAnchor),
        ])
        hostToSlot = rightHost.leadingAnchor.constraint(equalTo: slot.leadingAnchor)
        hostToRight = rightHost.leadingAnchor.constraint(equalTo: right.leadingAnchor)
        hostToSlot?.isActive = true
        // 分隔视图首次布局前子视图宽度为 0，侧栏内部的边距约束（搜索框、列表等）会在启动时报冲突；先给侧栏它该有的宽度。
        side.frame.size.width = savedSidebarWidth
        split.addArrangedSubview(side)
        split.addArrangedSubview(right)
        split.onDoubleClickDivider = { [weak self] in self?.setSidebarWidth(ScyllaUIStyle.sidebarMinWidth, save: true) }

        window.title = "ScyllaUI"
        window.contentView = split
        window.center()
        window.setFrameAutosaveName("ScyllaUIS0")
        window.delegate = self
        split.layoutSubtreeIfNeeded()
        setSidebarWidth(savedSidebarWidth, save: false)
        addSidebarButton()
        addStatusLine()
        // 窗口可见性变化：状态行的采样只在可见时进行（M2 第 2 块）。
        for name in [NSWindow.didChangeOcclusionStateNotification, NSWindow.didMiniaturizeNotification,
                     NSWindow.didDeminiaturizeNotification] {
            NotificationCenter.default.addObserver(forName: name, object: window, queue: .main) { [weak self] _ in
                self?.updateStatusSampling()
            }
        }
        applyTheme()
        // 生效的 Ghostty 配置变了（重新载入、深浅切换后换了主题）：重算窗口与侧栏颜色（M2 第 5 块）。
        NotificationCenter.default.addObserver(forName: GhosttyRuntime.configDidChange, object: runtime, queue: .main) { [weak self] _ in
            self?.applyTheme()
        }
        appearanceObserver = NSApp.observe(\.effectiveAppearance, options: [.initial, .new]) { [weak self] app, _ in
            let dark = app.effectiveAppearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
            DispatchQueue.main.async { self?.setSystemDark(dark) }
        }
        for name in [NSWindow.didBecomeKeyNotification, NSWindow.didResignKeyNotification] {
            NotificationCenter.default.addObserver(forName: name, object: window, queue: .main) { [weak self] _ in
                self?.updateFocus()
                // 回到前台时重建一次侧栏，移出已超出显示窗口的最近任务。
                if name == NSWindow.didBecomeKeyNotification { self?.feed.touch() }
            }
        }
        // 窗口尺寸、屏幕、缩放、全屏变化写尺寸日志（GeometryLog.swift）；拖动缩放只记结束时。
        let events: [(NSNotification.Name, String)] = [
            (NSWindow.didResizeNotification, "resize"), (NSWindow.didEndLiveResizeNotification, "live_resize_end"),
            (NSWindow.didChangeScreenNotification, "screen"), (NSWindow.didChangeBackingPropertiesNotification, "backing"),
            (NSWindow.didEnterFullScreenNotification, "fullscreen_enter"), (NSWindow.didExitFullScreenNotification, "fullscreen_exit"),
            (NSWindow.didDeminiaturizeNotification, "deminiaturize"),
        ]
        for (name, event) in events {
            NotificationCenter.default.addObserver(forName: name, object: window, queue: .main) { [weak self] _ in
                guard let self, !(name == NSWindow.didResizeNotification && self.window.inLiveResize) else { return }
                let size = self.container.bounds.size
                GeometryLog.shared?.write("window", "event=\(event) terminal_area=\(Int(size.width))x\(Int(size.height)) "
                    + "scale=\(self.window.backingScaleFactor) screen=\(self.window.screen?.localizedName.replacingOccurrences(of: " ", with: "_") ?? "-")")
            }
        }
    }

    /// 最后一个终端关闭后的空态：「新建终端」（主目录）与「打开文件夹…」，下方一行说明怎么启动 AI
    /// （共用规范 7.3 节：次要色、居中；docs/design.md 10.5 节决策 6）。
    private func buildEmptyState() {
        let text = NSTextField(labelWithString: String(localized: "没有打开的终端"))
        text.font = ScyllaUIStyle.toolFont
        text.textColor = .secondaryLabelColor
        let buttons = NSStackView(views: [
            NSButton(title: String(localized: "新建终端（⌘N）"), target: self, action: #selector(newHomeShell(_:))),
            NSButton(title: String(localized: "打开文件夹…（⌘O）"), target: self, action: #selector(openFolder(_:))),
        ])
        buttons.orientation = .horizontal
        buttons.spacing = 8
        let hint = NSTextField(labelWithString: String(localized: "在终端里输入 claude、codex、kimi、grok（或 agent）、opencode、copilot 或 agy 启动 Agent"))
        hint.font = ScyllaUIStyle.detailFont
        hint.textColor = .tertiaryLabelColor
        emptyState.orientation = .vertical
        emptyState.spacing = 10
        emptyState.addArrangedSubview(text)
        emptyState.addArrangedSubview(buttons)
        emptyState.addArrangedSubview(hint)
        emptyState.translatesAutoresizingMaskIntoConstraints = false
        emptyState.isHidden = true
        container.addSubview(emptyState)
        NSLayoutConstraint.activate([
            emptyState.centerXAnchor.constraint(equalTo: container.centerXAnchor),
            emptyState.centerYAnchor.constraint(equalTo: container.centerYAnchor),
        ])
    }

    // MARK: 新建、切换、关闭

    /// 新建终端（只有 Shell，AI 由用户在 Shell 里输入命令启动，docs/design.md 10.5 节决策 6）。
    /// `directory` 为 nil 时用当前终端的当前目录，没有终端时用主目录。
    func newTerminal(directory: String?) {
        let dir = directory ?? currentID.flatMap { sessions[$0]?.directory } ?? NSHomeDirectory()
        // 新建链路计时的起点（之后是 terminal open / show、后台的「起 Shell」与「启动计时」）。
        scyllauiLog.notice("terminal new tool=shell")
        open(directory: dir, input: nil)
    }

    @discardableResult
    private func open(directory: String, input: String?, resumeOf: Int64? = nil) -> String {
        let session = TerminalSession(app: app, core: core, directory: directory, initialInput: input, resumeOf: resumeOf)
        session.onSurfaceClosed = { [weak self] s in self?.surfaceClosed(s) }
        sessions[session.id] = session
        GeometryLog.shared?.write("open", "terminal=\(session.id.prefix(8)) input=\(input != nil)")
        scyllauiLog.notice("terminal open id=\(session.id, privacy: .public) dir=\(directory, privacy: .public) input=\(input != nil)")
        show(session.id)
        return session.id
    }

    /// 右侧显示指定终端：切走的终端移出视图层（暂停渲染、进程照常运行），切到的终端加回。
    /// `takeKeyboard` 为 false 时（侧栏点选、上下键）键盘留在侧栏，其余入口（新建、快捷键、关闭后改显示）交给终端。
    func show(_ id: String, takeKeyboard: Bool = true) {
        guard let session = sessions[id] else { return }
        if currentID != id, let old = currentID.flatMap({ sessions[$0] }) {
            old.view.removeFromSuperview()
        }
        if currentID != id {
            stats = nil
            setZoom(false)
        }
        currentID = id
        Prefs.lastTerminal = id
        emptyState.isHidden = true
        // 第一次显示时才建 Surface（attach 随之启动，等首次布局后连接后台，M3）。
        let created = !session.hasView
        let view = session.view
        if created, let s = view.surface { runtime.setColorScheme(dark: systemDark, surfaces: [s]) }
        if view.superview !== container {
            view.translatesAutoresizingMaskIntoConstraints = false
            container.addSubview(view)
            NSLayoutConstraint.activate([
                view.topAnchor.constraint(equalTo: container.topAnchor),
                view.bottomAnchor.constraint(equalTo: container.bottomAnchor),
                view.leadingAnchor.constraint(equalTo: container.leadingAnchor),
                view.trailingAnchor.constraint(equalTo: container.trailingAnchor),
            ])
        }
        // 新建的 Surface 立即按终端区排版：重启后恢复终端时逐个 open → show，同一轮里下一个就把它换下，
        // 不排版的话它一直是创建时的 800×600 点（99×35），attach 等不到布局便按这个尺寸起 Shell，代输启动的
        // 全屏 CLI（OpenCode、Antigravity）就按 99×35 画，之后切过去偶尔不重画（2026-11-06 实测）。
        if created { container.layoutSubtreeIfNeeded() }
        if takeKeyboard { window.makeFirstResponder(view) }
        view.onCellSize = { [weak self] _ in self?.updateMinSize() }
        updateMinSize()
        GeometryLog.shared?.write("show", "terminal=\(id.prefix(8)) terminal_area=\(Int(container.bounds.width))x\(Int(container.bounds.height))")
        scyllauiLog.notice("terminal show id=\(id, privacy: .public) order=\(self.sidebar.terminalOrder.map { String($0.prefix(8)) }.joined(separator: ","), privacy: .public)")
        sidebar.show(terminalID: id)
        updateFocus()
        updateStatusSampling(sampleNow: true)
        refreshStatusLine()
    }

    /// 点了「需确认」通知：窗口前置，切到该终端并把键盘交给它。终端已关闭时只前置窗口。
    private func openFromNotification(_ id: String) {
        NSApp.activate(ignoringOtherApps: true)
        if window.isMiniaturized { window.deminiaturize(nil) }
        window.makeKeyAndOrderFront(nil)
        show(id)
    }

    // MARK: 一键恢复（docs/design.md 第 5 节「实现（M1 第 6 块）」）

    /// 双击「最近任务」中的一条（docs/design.md 10.5 节决策 15）：后台判定后切到正在运行它的终端，
    /// 或在对话的目录开终端代输恢复命令，或提示无法恢复。
    func resume(conversation id: Int64) {
        guard resuming.insert(id).inserted else { return }
        scyllauiLog.notice("resume request conversation=\(id)")
        core.resume(conversationID: id) { [weak self] reply in
            DispatchQueue.main.async { self?.resumeReplied(id, reply) }
        }
    }

    private func resumeReplied(_ id: Int64, _ reply: [String: Any]?) {
        resuming.remove(id)
        guard let reply, reply["ok"] as? Bool == true else {
            scyllauiLog.error("resume conversation=\(id) failed reply=\(String(describing: reply), privacy: .public)")
            NSSound.beep()
            return
        }
        let action = reply["action"] as? String ?? "-"
        scyllauiLog.notice("resume conversation=\(id) action=\(action, privacy: .public) terminal=\(reply["terminal_id"] as? String ?? "-", privacy: .public) command=\(reply["command"] as? String ?? "-", privacy: .public)")
        let conv = reply["conversation"] as? [String: Any] ?? [:]
        let tool = SidebarModel.toolName(conv["tool"] as? String)
        switch action {
        case "switch":
            if let t = reply["terminal_id"] as? String { show(t) }
        case "open":
            guard let command = reply["command"] as? String, let dir = reply["dir"] as? String else { return }
            var isDir: ObjCBool = false
            guard FileManager.default.fileExists(atPath: dir, isDirectory: &isDir), isDir.boolValue else {
                return cannotResume(id, conv, reason: String(localized: "对话所在的目录已不存在：\(dir)\n\(tool) 按启动目录保存会话，换到别的目录无法恢复。"))
            }
            open(directory: dir, input: command + "\n", resumeOf: id)
        case "gone":
            let path = reply["path"] as? String ?? ""
            cannotResume(id, conv, reason: String(localized: "这条对话的会话记录已不存在，可能已被 \(tool) 清理。\n\(path)"))
        default:
            break
        }
    }

    /// 「删除记录…」：确认后立即从 ScyllaUI 的库里删除，不影响 CLI 自己的会话记录（docs/design.md 第 5 节「移除与删除」）。
    private func confirmDelete(_ id: Int64, title: String) {
        let line = title.split(whereSeparator: \.isNewline).joined(separator: " ")
        let shown = line.count > 40 ? String(line.prefix(40)) + "…" : line
        let alert = NSAlert()
        alert.useWideLayout()
        alert.alertStyle = .warning
        alert.messageText = String(localized: "删除「\(shown)」的记录？")
        alert.informativeText = String(localized: "将从 ScyllaUI 中立即删除这条任务的标题、提示词、回复摘要与事件记录，删除后无法在 ScyllaUI 中恢复。Agent 自己的会话记录不受影响，知道会话 ID 仍可在 Agent 里恢复。")
        alert.addButton(withTitle: String(localized: "删除"))
        alert.addButton(withTitle: String(localized: "取消"))
        alert.buttons.first?.hasDestructiveAction = true
        alert.beginSheetModal(for: window) { [weak self] response in
            if response == .alertFirstButtonReturn { self?.core.delete(conversationID: id) }
        }
    }

    /// 无法恢复且原因确定（会话记录或目录已不存在）：提示，可接着「删除记录…」（docs/design.md 10.5 节决策 4）。
    private func cannotResume(_ id: Int64, _ conv: [String: Any], reason: String) {
        let alert = NSAlert()
        alert.messageText = String(localized: "无法恢复「\(Self.taskTitle(conv))」")
        alert.informativeText = reason
        alert.addButton(withTitle: String(localized: "删除记录…"))
        alert.addButton(withTitle: String(localized: "保留"))
        alert.beginSheetModal(for: window) { [weak self] response in
            guard response == .alertFirstButtonReturn else { return }
            // 前一个提示框收起后再弹删除确认。
            DispatchQueue.main.async { self?.confirmDelete(id, title: Self.taskTitle(conv)) }
        }
    }

    /// 提示里的任务标题：标题 → 第一条提示词 → 会话 ID 开头，单行，过长截断。
    private static func taskTitle(_ c: [String: Any]) -> String {
        let t = (c["title"] as? String) ?? (c["last_prompt"] as? String) ?? String(((c["native_id"] as? String) ?? "").prefix(8))
        let line = t.split(whereSeparator: \.isNewline).joined(separator: " ")
        return line.count > 40 ? String(line.prefix(40)) + "…" : line
    }

    /// 用户关闭（菜单、右键、Cmd+W）：交给 libghostty，由关闭回调统一处理确认。
    /// 关闭终端（M3）：由后台判定是否要确认（光标不在 Shell 提示符时，即 Ghostty 的规则，docs/design.md 第 5 节），
    /// 需要时弹确认，确认后由后台结束 Shell；终端随后台的推送移除。后台没连上时不做任何事。
    func close(_ id: String) {
        guard sessions[id] != nil else { return }
        core.closeTerminal(id: id, confirmed: false) { [weak self] reply in
            DispatchQueue.main.async { self?.closeReplied(id, reply) }
        }
    }

    private func closeReplied(_ id: String, _ reply: [String: Any]) {
        guard let session = sessions[id] else { return }
        if reply["needs_confirm"] as? Bool == true {
            guard window.attachedSheet == nil else { return }
            let alert = NSAlert()
            alert.messageText = String(localized: "关闭这个终端？")
            alert.informativeText = String(localized: "终端里还有程序在运行（例如 Agent 会话），关闭会结束它。")
            alert.addButton(withTitle: String(localized: "关闭"))
            alert.addButton(withTitle: String(localized: "取消"))
            alert.beginSheetModal(for: window) { [weak self] response in
                guard response == .alertFirstButtonReturn else { return }
                self?.core.closeTerminal(id: id, confirmed: true) { reply in
                    DispatchQueue.main.async { self?.closeReplied(id, reply) }
                }
            }
            return
        }
        let gone = (reply["error"] as? [String: Any])?["code"] as? String == "not_found"
        if reply["closed"] as? Bool == true || gone { remove(session) }
    }

    /// Surface 关闭（attach 已退出，M3）：后台已关闭这个终端就移除；后台仍持有它（attach 意外退出）则只释放 Surface，
    /// 当前显示的稍后重新接回，其余的在下次显示时接回。
    private func surfaceClosed(_ session: TerminalSession) {
        guard sessions[session.id] === session else { return }
        session.dropView()
        guard alive.contains(session.id) else { return remove(session) }
        scyllauiLog.notice("terminal attach exited id=\(session.id, privacy: .public)")
        guard currentID == session.id else { return }
        // 稍等后台的推送：Shell 刚退出时 attach 先退出、关闭的推送随后才到。
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.3) { [weak self] in
            guard let self, self.currentID == session.id, self.sessions[session.id] === session, !session.hasView,
                  self.alive.contains(session.id) else { return }
            if let last = self.reattachedAt[session.id], Date().timeIntervalSince(last) < 2 { return }
            self.reattachedAt[session.id] = Date()
            self.show(session.id)
        }
    }

    /// 移除终端（后台已关闭它，其上的对话进入最近任务）：显示侧栏中相邻的终端，没有则显示空态。
    private func remove(_ session: TerminalSession) {
        guard sessions[session.id] != nil else { return }
        let order = sidebar.terminalOrder
        core.forgetTerminal(id: session.id)
        session.dropView()
        sessions[session.id] = nil
        reattachedAt[session.id] = nil
        GeometryLog.shared?.write("close", "terminal=\(session.id.prefix(8))")
        scyllauiLog.notice("terminal close id=\(session.id, privacy: .public)")
        guard currentID == session.id else { return }
        currentID = nil
        stats = nil
        defer {
            updateStatusSampling()
            refreshStatusLine()
        }
        // 优先显示侧栏中的下一个，其次上一个。
        let i = order.firstIndex(of: session.id) ?? order.count
        let next = order.suffix(from: min(i + 1, order.count)).first { sessions[$0] != nil }
            ?? order.prefix(i).last { sessions[$0] != nil }
            ?? sessions.keys.first
        if let next {
            show(next)
        } else {
            emptyState.isHidden = false
            window.title = "ScyllaUI"
            sidebar.show(terminalID: nil)
            updateFocus()
        }
    }

    /// 最小窗口尺寸：终端区域至少 40 列 × 10 行（侧栏另计，侧栏收起时不计）。窗口压到一两列宽时 zsh 的
    /// 提示符重画会留下残片，Ghostty.app 同样如此（2026-10-11 对照），这里不让终端被压到没有实际用途的尺寸。
    /// 字符格尺寸未知时按 8 × 17 点估计。
    private func updateMinSize() {
        window.contentMinSize = NSSize(width: sidebarExtent + terminalMinSize.width + promptPanelExtent,
                                       height: terminalMinSize.height)
    }

    private var terminalMinSize: CGSize {
        let cell = currentID.flatMap { sessions[$0]?.view.cellSize }.flatMap { $0 == .zero ? nil : $0 }
            ?? CGSize(width: 8, height: 17)
        return CGSize(width: (cell.width * 40).rounded(.up) + 8, height: (cell.height * 10).rounded(.up) + 8)
    }

    /// 右侧栏占用的宽度；关着时为 0。
    private var promptPanelExtent: CGFloat { rightPanel == nil ? 0 : promptWidth.constant }

    private static var savedPromptWidth: CGFloat {
        let w = CGFloat(Prefs.promptPanelWidth)
        return w == 0 ? RightPanelHost.defaultWidth : min(max(w, RightPanelHost.minWidth), RightPanelHost.maxWidth)
    }

    /// 查看宽（docs/design.md 5.12 节「宽度」）：默认 680，不小于 320（隐藏了文件树或改动列表的查看器最窄 320；
    /// 显示列表时查看器最窄 160，见 `ListSplit.viewerMinWidth`）。
    static let defaultViewerWidth: CGFloat = 680
    static let viewerMinWidth: CGFloat = 320
    private static var savedViewerWidth: CGFloat {
        let w = CGFloat(Prefs.rightViewerWidth)
        return w == 0 ? defaultViewerWidth : max(w, viewerMinWidth)
    }

    /// 右侧栏此刻在显示查看器（打开了文件，或选中了 Diff 的文件），用查看宽。
    private var viewerMode: Bool {
        (rightPanel == .files && filesPanel.contentShown) || (rightPanel == .diff && diffPanel.contentShown)
    }
    /// 查看器旁的文件树或改动列表是否隐藏。
    private var sideListHidden: Bool {
        get { rightPanel == .diff ? diffPanel.listHidden : filesPanel.treeHidden }
        set { if rightPanel == .diff { diffPanel.listHidden = newValue } else { filesPanel.treeHidden = newValue } }
    }
    /// 查看宽的下限：显示文件树或改动列表时为「树宽 + 160」（两栏同一个宽，D2 起 Diff 与文件栏一致），隐藏时为 320。
    private var viewerLowerBound: CGFloat {
        if sideListHidden { return Self.viewerMinWidth }
        return splitListWidth + ListSplit.viewerMinWidth
    }
    /// 有分栏的两栏（文件、Diff，`ListSplit`）：打开着内容（不论收起）、列表宽、查看器是否收起。
    private var splitOpen: Bool {
        (rightPanel == .files && filesPanel.viewerOpen) || (rightPanel == .diff && diffPanel.viewerOpen)
    }
    private var splitListWidth: CGFloat { rightPanel == .diff ? diffPanel.listWidth : filesPanel.treeWidth }
    private var splitCollapsed: Bool {
        get { (rightPanel == .files && filesPanel.contentCollapsed) || (rightPanel == .diff && diffPanel.contentCollapsed) }
        set { if rightPanel == .diff { diffPanel.contentCollapsed = newValue } else { filesPanel.contentCollapsed = newValue } }
    }
    /// 打开着内容但查看器收起（只剩列表）：右侧栏用拖到的宽度（只在内存里），没拖过时用列表宽。
    private var collapsedWidth: CGFloat?

    /// 树宽（F3）：单独记，默认 280，不小于 160。
    private static var savedTreeWidth: CGFloat {
        let w = CGFloat(Prefs.filesTreeWidth)
        return w == 0 ? FilesPanel.defaultTreeWidth : max(w, FilesPanel.treeHideWidth)
    }
    /// 按当前窗口，右侧栏最多能占的宽度（终端保留 40 列）。
    private var rightRoom: CGFloat { (rightHost.superview?.bounds.width ?? 0) - terminalMinSize.width }

    /// 拖动右侧栏左边缘（同侧栏策略）：列表限在 280—520、查看器限在下限到可用宽度，且终端区不小于最小尺寸（拖动时不加宽窗口）；
    /// 松手时记住（列表宽、查看宽各记各的）。
    private func resizePromptPanel(_ width: CGFloat, done: Bool) {
        let room = rightRoom
        // F3、D2：「文件」显示树、Diff 显示改动列表时，查看器窄到 160 以下收起，列表滑过来盖住它，栏宽仍跟着鼠标；
        // 拖回到「列表宽 + 176」以上再展开。
        if splitOpen, !sideListHidden {
            let r = ListSplit.edgeDrag(width: width, listWidth: splitListWidth, collapsed: splitCollapsed, room: room)
            // 先定好宽度再切换：切换会按新状态重算右侧栏宽度。收起时查看宽回到记住的值（拖到收起前那一下很窄，
            // 之后点「显示文件内容」不该回到它）。
            if r.collapsed {
                collapsedWidth = r.width
                if !splitCollapsed { viewerWidth = Self.savedViewerWidth }
            } else {
                viewerWidth = r.width
            }
            if r.collapsed != splitCollapsed {
                splitCollapsed = r.collapsed
                scyllauiLog.notice("\(self.rightPanel == .diff ? "diff" : "files", privacy: .public) content collapsed=\(r.collapsed) by drag width=\(Int(width))")
            }
            if r.collapsed {
                if abs(r.width - promptWidth.constant) > 0.5 {
                    promptWidth.constant = r.width
                    updateMinSize()
                }
                return
            }
        }
        let w: CGFloat
        if viewerMode {
            w = min(max(width, viewerLowerBound), max(viewerLowerBound, room))
            viewerWidth = w
        } else {
            w = min(max(width, RightPanelHost.minWidth), max(RightPanelHost.minWidth, min(RightPanelHost.maxWidth, room)))
            listWidth = w
        }
        if abs(w - promptWidth.constant) > 0.5 {
            promptWidth.constant = w
            updateMinSize()
        }
        if done {
            if viewerMode { Prefs.rightViewerWidth = Double(w) } else { Prefs.promptPanelWidth = Double(w) }
            scyllauiLog.notice("right panel width=\(Int(w)) viewer=\(self.viewerMode)")
        }
    }

    /// 双击左边缘：列表回到 280，查看器回到 680。
    private func resetRightWidth() {
        if splitCollapsed { return splitCollapsed = false }
        resizePromptPanel(viewerMode ? Self.defaultViewerWidth : RightPanelHost.defaultWidth, done: true)
    }

    /// 按当前模式定右侧栏宽度。`grow`：打开文件等主动操作时窗口放不下就先加宽（不超过屏幕可见宽度），仍放不下就隐藏文件树。
    private func applyRightWidth(grow: Bool) {
        guard rightPanel != nil, !applyingRightWidth else { return }
        applyingRightWidth = true
        defer { applyingRightWidth = false }
        var target = viewerMode ? max(viewerWidth, viewerLowerBound) : splitCollapsed ? collapsedWidth ?? splitListWidth : listWidth
        if grow, target > rightRoom, let screen = window.screen?.visibleFrame {
            var f = window.frame
            f.size.width = min(f.width + target - rightRoom, screen.width)
            if f.maxX > screen.maxX { f.origin.x = max(screen.minX, screen.maxX - f.width) }
            window.setFrame(f, display: true, animate: false)
            rightHost.superview?.superview?.layoutSubtreeIfNeeded()
        }
        if grow, viewerMode, !sideListHidden, rightRoom < viewerLowerBound {
            sideListHidden = true
            scyllauiLog.notice("right panel tree auto-hidden room=\(Int(self.rightRoom))")
        }
        if viewerMode { target = min(max(viewerWidth, viewerLowerBound), max(rightRoom, Self.viewerMinWidth)) }
        promptWidth.constant = target
        updateMinSize()
        let min = window.frameRect(forContentRect: NSRect(origin: .zero, size: window.contentMinSize)).width
        if window.frame.width < min {
            var f = window.frame
            f.size.width = min
            window.setFrame(f, display: true, animate: false)
        }
    }

    /// 放大查看器：宿主改贴右侧区域的左边，盖住终端区；终端仍贴 `slot`，尺寸不变。放大时键盘给查看器，缩回时还给终端。
    private func setZoom(_ on: Bool) {
        let on = on && viewerMode
        guard on != zoomed else { return }
        zoomed = on
        hostToSlot?.isActive = !on
        hostToRight?.isActive = on
        filesPanel.zoomed = on && rightPanel == .files
        diffPanel.zoomed = on && rightPanel == .diff
        let viewerHasFocus = filesPanel.viewer.holdsFocus(window.firstResponder) || diffPanel.viewer.holdsFocus(window.firstResponder)
        if on {
            rightPanel == .diff ? diffPanel.viewer.takeFocus() : filesPanel.viewer.takeFocus()
        } else if let view = currentID.flatMap({ sessions[$0]?.view }), viewerHasFocus {
            window.makeFirstResponder(view)
        }
        scyllauiLog.notice("right panel zoom=\(on)")
    }

    /// 往当前终端的输入行放一段文字：与粘贴相同（libghostty 的 surface text 走粘贴，支持 bracketed paste 的程序不会把换行当回车），
    /// 不追加回车；键盘随之交给终端。历史提示词的「填入」与查看器的「插入到终端」共用。
    private func insertIntoCurrent(_ text: String) {
        guard let view = currentID.flatMap({ sessions[$0]?.view }) else { return NSSound.beep() }
        view.sendText(text)
        window.makeFirstResponder(view)
    }

    /// 侧栏占用的宽度（含分隔线）；收起时为 0。
    private var sidebarExtent: CGFloat {
        sidebarCollapsed ? 0 : max(sideView.frame.width, ScyllaUIStyle.sidebarMinWidth) + split.dividerThickness
    }

    // MARK: 铺满屏幕时的三栏比例（2026-11-06 用户要求）

    /// 照 Claude 桌面版铺满时的三栏（2026-11-06 用户在 1512 pt 与 1280 pt 两种屏幕上对照截图）：左栏基本固定在 300 左右。
    /// 历史提示词栏原按「窗口宽度减去左栏」的 34.2% 放宽，2026-11-21 用户嫌占比大，改为铺满时也取默认宽度（即最小宽度）。
    static let filledSidebarWidth: CGFloat = 300

    /// 上一次缩放时窗口是否铺满；nil 为还没记过（启动时恢复的窗口大小不算用户的操作，只记下状态）。
    private var wasFilled: Bool?

    private var windowFilled: Bool {
        guard let visible = window.screen?.visibleFrame else { return false }
        return window.styleMask.contains(.fullScreen)
            || (abs(window.frame.width - visible.width) <= 2 && abs(window.frame.height - visible.height) <= 2)
    }

    /// 系统的「填充」、平铺与拖边框都以实时调整大小完成，结束时再判断（中途的尺寸不算）。
    func windowDidEndLiveResize(_ notification: Notification) {
        noteWindowFrame()
    }

    /// 窗口变成铺满屏幕（双击标题栏、绿色按钮或「窗口」菜单的缩放、填充与全屏）时，按比例重设侧栏与历史提示词栏的宽度并记住。
    func windowDidResize(_ notification: Notification) {
        guard !window.inLiveResize else { return }
        noteWindowFrame()
    }

    private func noteWindowFrame() {
        guard window.screen != nil else { return }
        let filled = windowFilled
        let before = wasFilled
        wasFilled = filled
        guard filled, before == false else { return }
        let width = window.frame.width
        // 等分隔视图按新尺寸排好再设，否则侧栏的位置会被这一轮布局改回去。
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            let side = self.sidebarCollapsed ? 0 : min(max(Self.filledSidebarWidth, ScyllaUIStyle.sidebarMinWidth), ScyllaUIStyle.sidebarMaxWidth)
            // 列表宽与查看宽都回到默认（docs/design.md 5.12 节「宽度」）。
            self.listWidth = RightPanelHost.defaultWidth
            self.filesPanel.treeWidth = FilesPanel.defaultTreeWidth
            Prefs.filesTreeWidth = 0
            self.diffPanel.listWidth = FilesPanel.defaultTreeWidth
            self.viewerWidth = Self.defaultViewerWidth
            Prefs.promptPanelWidth = Double(self.listWidth)
            Prefs.rightViewerWidth = 0
            self.applyRightWidth(grow: false)
            if !self.sidebarCollapsed { self.setSidebarWidth(side, save: true) }
            scyllauiLog.notice("filled layout width=\(Int(width)) sidebar=\(Int(self.sideView.frame.width)) prompt=\(self.promptWidth.constant)")
        }
    }

    // MARK: 侧栏宽度与收起（docs/design.md 第 5 节「宽度与连线」）

    private static let sidebarWidthKey = "sidebarWidth"

    /// 上次拖到的宽度（UserDefaults，本机便利设置），限制在 300—420。
    private var savedSidebarWidth: CGFloat {
        let w = CGFloat(UserDefaults.standard.double(forKey: Self.sidebarWidthKey))
        return w == 0 ? ScyllaUIStyle.sidebarMinWidth : min(max(w, ScyllaUIStyle.sidebarMinWidth), ScyllaUIStyle.sidebarMaxWidth)
    }

    private func setSidebarWidth(_ width: CGFloat, save: Bool) {
        guard !sidebarCollapsed else { return }
        split.setPosition(width, ofDividerAt: 0)
        if save { UserDefaults.standard.set(Double(width), forKey: Self.sidebarWidthKey) }
        updateMinSize()
        scyllauiLog.notice("sidebar width=\(self.sideView.frame.width) saved=\(save)")
    }

    /// 侧栏开关按钮：紧挨红黄绿三个按钮（标题栏附属视图），与「终端」菜单的 Cmd+\ 等效。
    private func addSidebarButton() {
        let tip = String(localized: "隐藏 / 显示侧栏（⌘\\）")
        let button = NSButton(image: NSImage(systemSymbolName: "sidebar.left", accessibilityDescription: tip) ?? NSImage(),
                              target: self, action: #selector(toggleSidebarPane(_:)))
        button.isBordered = false
        button.bezelStyle = .accessoryBarAction
        button.contentTintColor = .secondaryLabelColor
        button.toolTip = tip
        button.frame = NSRect(x: 4, y: 0, width: 28, height: 22)
        let holder = TitlebarAlignedView(frame: NSRect(x: 0, y: 0, width: 32, height: 22))
        holder.addSubview(button)
        let accessory = NSTitlebarAccessoryViewController()
        accessory.view = holder
        accessory.layoutAttribute = .leading
        window.addTitlebarAccessoryViewController(accessory)
    }

    // MARK: 标题栏状态行（M2 第 2 块；2026-10-18 改版，docs/design.md 第 5 节「实现（M2 第 2 块）」位置与样式）

    /// 与侧栏、终端同一字体（2026-11-21）。
    private static let statusFont = ScyllaUIStyle.mono(13)
    /// 红黄绿三个按钮加侧栏开关约占的宽度，以及右侧留白：状态行最多用窗口宽度减去这两项。
    private static let statusReservedWidth: CGFloat = 140
    /// 文字框里文字左边的留白（标签的单元格自带），各段悬停区域从这里起算。
    private static let statusTextInset: CGFloat = 2

    /// 紧跟侧栏开关、靠左的状态行，与靠右的 CPU / Mem（0.3.5）；居中的窗口标题隐藏（标题照常写入，供「窗口」菜单与调度中心使用）。
    private func addStatusLine() {
        window.titleVisibility = .hidden
        statusField.font = Self.statusFont
        statusField.lineBreakMode = .byTruncatingTail
        statusField.cell?.usesSingleLineMode = true
        statusField.frame = NSRect(x: 8, y: 0, width: 10, height: ceil(statusField.fittingSize.height))
        statusHolder.addSubview(statusField)
        // 右侧按钮组：按钮从右往左排，右边留 8 点。
        var x: CGFloat = 0
        for b in [statusButton, filesButton, diffButton, historyButton] {
            b.frame.origin.x = x
            toolsHolder.addSubview(b)
            x += b.frame.width + 4
        }
        toolsHolder.frame.size.width = x - 4 + 8
        for (holder, side) in [(statusHolder, NSLayoutConstraint.Attribute.leading), (toolsHolder, .trailing)] {
            let accessory = NSTitlebarAccessoryViewController()
            accessory.view = holder
            accessory.layoutAttribute = side
            window.addTitlebarAccessoryViewController(accessory)
        }
        toolsHolder.align()
        NotificationCenter.default.addObserver(forName: NSWindow.didResizeNotification, object: window, queue: .main) { [weak self] _ in
            self?.refreshStatusLine()
        }
    }

    /// 标题栏右侧的一个功能按钮（2026-11-21）：只有图标，鼠标移上去才显示底框；打开对应功能时保持按下（系统的灰底白图标，
    /// 这种按钮样式不吃 contentTintColor，不另画蓝色）。
    /// 悬停写功能名与快捷键（图标上看不到名字，docs/ui-methodology.md 1.8 节）。
    private static func titlebarButton(symbol: String, name: String, shortcut: String, target: AnyObject, action: Selector) -> NSButton {
        let image = NSImage(systemSymbolName: symbol, accessibilityDescription: name)?
            .withSymbolConfiguration(NSImage.SymbolConfiguration(pointSize: 14, weight: .regular))
        let b = NSButton(image: image ?? NSImage(), target: target, action: action)
        b.bezelStyle = .accessoryBarAction
        b.setButtonType(.pushOnPushOff)
        b.showsBorderOnlyWhileMouseInside = true
        b.toolTip = "\(name)  \(shortcut)"
        b.setAccessibilityLabel(name)
        b.frame = NSRect(x: 0, y: 0, width: 28, height: 24)
        return b
    }

    /// 按段的用途取色：颜色都是随深浅主题变化的动态色，切换主题时不必重建。
    private static func statusText(_ spans: [StatusInfo.Span]) -> NSAttributedString {
        let paragraph = NSMutableParagraphStyle()
        paragraph.lineBreakMode = .byTruncatingTail
        let out = NSMutableAttributedString()
        for span in spans {
            let color: NSColor
            switch span.role {
            case .muted: color = .secondaryLabelColor
            case .separator: color = .tertiaryLabelColor
            // Total、CPU 与内存的值和侧栏项目名同用 Ready 绿（2026-11-05、2026-11-21 用户要求），不新增颜色。
            case .value, .quotaLow: color = ScyllaUIStyle.ready
            case .quotaMid: color = ScyllaUIStyle.waiting
            case .quotaHigh: color = ScyllaUIStyle.error
            }
            out.append(NSAttributedString(string: span.text, attributes: [
                .font: statusFont,
                .foregroundColor: color, .paragraphStyle: paragraph,
            ]))
        }
        return out
    }

    private var windowVisible: Bool { window.occlusionState.contains(.visible) && !window.isMiniaturized }

    /// 按当前终端更新状态行；宽度随内容。右侧 CPU / Mem 完整显示，左侧最多用到窗口宽度减去左侧按钮、右侧一组与留白：
    /// 放不下时按 `StatusInfo.fitted` 收缩（项目名、分支截在中间，再依次去掉次要的部分，2026-11-01），仍超出的从末尾截断。
    /// 每段各自挂悬停（docs/ui-methodology.md 1.8 节）。
    private func refreshStatusLine() {
        let info = currentID.flatMap { statusInfos[$0] }
        let s = stats.flatMap { $0.terminal == currentID ? $0 : nil }
        let hidden = Prefs.statusHidden
        let st = StatusInfo.Stats(cpu: s?.cpu, mem: s?.mem, processes: s?.processes ?? 0)
        let available = max(120, window.frame.width - Self.statusReservedWidth - toolsHolder.frame.width)
        let segments = (info ?? StatusInfo()).fitted(width: available - 16 - Self.statusTextInset * 2, hidden: hidden, stats: st) {
            Self.statusText($0).size().width
        }
        statusField.attributedStringValue = Self.statusText(segments.flatMap(\.spans))
        let width = min(ceil(statusField.fittingSize.width) + 16, available)
        statusHolder.frame.size.width = segments.isEmpty ? 1 : width
        statusField.frame.size.width = max(0, width - 16)
        statusHolder.align()
        var regions: [(NSRect, String)] = []
        var x = Self.statusTextInset
        for s in segments {
            let w = Self.statusText(s.spans).size().width
            if let tip = s.tooltip { regions.append((NSRect(x: x, y: 0, width: w, height: statusField.bounds.height), tip)) }
            x += w
        }
        // 这里每 2 秒走一次（CPU / Mem 采样与额度倒计时）：区域没变就只改文字，变了才重挂。
        if regions.map(\.0) == statusTips.map(\.rect) {
            for (t, r) in zip(statusTips, regions) { t.owner.text = r.1 }
        } else {
            statusField.removeAllToolTips()
            statusTips = regions.map { (rect: $0.0, owner: TooltipText($0.1)) }
            for t in statusTips { statusField.addToolTip(t.rect, owner: t.owner, userData: nil) }
        }
    }

    /// 窗口可见且右侧有终端时运行 2 秒计时器，否则停掉（后台没有定时器）。设置里关掉的段不采样（M4a）：CPU 与内存都关掉时
    /// 计时器只刷新额度的倒计时，不发 `proc.stats`；5 小时、7 天与月额度也都关掉时不开计时器。2026-11-21 状态行去掉分支与改动后
    /// 不再向后台报 `status.watch`。
    private func updateStatusSampling(sampleNow: Bool = false) {
        let hidden = Prefs.statusHidden
        let ticking = !hidden.contains(.cpu) || !hidden.contains(.mem) || !hidden.contains(.quota) || !hidden.contains(.quota5h)
            || !hidden.contains(.quotaMonth)
        let active = windowVisible && currentID != nil
        if active && ticking {
            if statusTimer == nil {
                let t = Timer(timeInterval: 2, repeats: true) { [weak self] _ in self?.sampleStats() }
                t.tolerance = 0.2
                RunLoop.main.add(t, forMode: .common)
                statusTimer = t
                scyllauiLog.notice("status_timer start")
                sampleStats()
            } else if sampleNow {
                sampleStats()
            }
        } else if let t = statusTimer {
            t.invalidate()
            statusTimer = nil
            scyllauiLog.notice("status_timer stop visible=\(self.windowVisible)")
        }
    }

    private func sampleStats() {
        let hidden = Prefs.statusHidden
        guard !(hidden.contains(.cpu) && hidden.contains(.mem)) else {
            refreshStatusLine()   // 只为周额度的倒计时刷新
            return
        }
        guard let id = currentID else { return }
        core.request(["type": "proc.stats", "terminal_id": id]) { [weak self] reply in
            let cpu = (reply["cpu_percent"] as? NSNumber)?.doubleValue
            let mem = (reply["mem_bytes"] as? NSNumber)?.int64Value
            let n = (reply["processes"] as? NSNumber)?.intValue ?? 0
            // 每 2 秒一条：只进统一日志的 debug 级，不进文件（docs/design.md 10.5 节决策 23）。
            scyllauiLog.debug("status_tick terminal=\(id.prefix(8), privacy: .public) cpu=\(cpu.map { String($0) } ?? "-", privacy: .public) mem=\(mem.map { String($0) } ?? "-", privacy: .public) processes=\(n)")
            DispatchQueue.main.async {
                guard let self, self.currentID == id else { return }
                self.stats = (id, cpu, mem, n)
                self.refreshStatusLine()
            }
        }
    }

    /// 收起 / 展开侧栏：收起后终端占满窗口（沉浸模式）；收起状态不记忆。展开时窗口不够宽就加宽。
    /// 收起时把侧栏移出分隔视图，而不是只隐藏它：分隔视图把分隔线画在单独的图层上（zPosition 5000，高于子视图），
    /// 只隐藏侧栏时该图层留在原位置，终端与窄标题栏上会留下一条竖线（2026-10-11 用户试用发现，最小脚本复现）。
    // MARK: 主题（M2 第 5 块，docs/design.md 第 5 节「实现（M2 第 5 块）」）

    /// 按生效的 Ghostty 配置定窗口外观、侧栏底色与外观（与 Ghostty.app 相同的规则，见 GhosttyRuntime）。
    private func applyTheme() {
        if let bg = runtime.backgroundColor {
            let light = GhosttyRuntime.isLight(bg)
            sideView.color = bg.blended(withFraction: light ? 0.035 : 0.18, of: .black) ?? bg
            sideView.appearance = NSAppearance(named: light ? .aqua : .darkAqua)
            rightHost.color = sideView.color
            filesPanel.fillColor = sideView.color
            diffPanel.fillColor = sideView.color
            rightHost.appearance = sideView.appearance
        }
        // 窗口深浅与 Ghostty.app 一致（默认按终端背景色），侧栏不再单独跟随系统而与终端不协调。
        window.appearance = runtime.windowAppearance
        sendAppearance()
        scyllauiLog.notice("window appearance=\(self.window.appearance?.name.rawValue ?? "system", privacy: .public) background=\(self.runtime.backgroundColor?.description ?? "-", privacy: .public)")
    }

    private func setSystemDark(_ dark: Bool) {
        systemDark = dark
        runtime.setColorScheme(dark: dark, surfaces: sessions.values.filter(\.hasView).compactMap { $0.view.surface })
        scyllauiLog.notice("color scheme dark=\(dark)")
        sendAppearance()
    }

    /// 断开期间后台代答终端查询所用的外观（`appearance.set`，M3）：与 Surface 的应答一致。
    private func sendAppearance() {
        var a: [String: Any] = ["color_scheme": systemDark ? "dark" : "light"]
        if let fg = runtime.foregroundColor.flatMap(Self.hex) { a["foreground"] = fg }
        if let bg = runtime.backgroundColor.flatMap(Self.hex) { a["background"] = bg }
        core.setAppearance(a)
    }

    private static func hex(_ c: NSColor) -> String? {
        guard let c = c.usingColorSpace(.sRGB) else { return nil }
        let v = { (x: CGFloat) in Int((x * 255).rounded()) }
        return String(format: "#%02x%02x%02x", v(c.redComponent), v(c.greenComponent), v(c.blueComponent))
    }

    /// 「终端」菜单「重新载入配置」（Cmd+Shift+,）。
    @objc func reloadConfiguration(_ sender: Any?) {
        runtime.reloadConfig()
    }

    /// App 菜单「关于 ScyllaUI」：系统的关于面板，显示版本号、构建号与构建标识（docs/design.md 10.1 节「版本号」）。
    @objc func showAbout(_ sender: Any?) {
        let number = Bundle.main.infoDictionary?["CFBundleVersion"] as? String ?? "-"
        NSApp.activate(ignoringOtherApps: true)
        NSApp.orderFrontStandardAboutPanel(options: [.version: core.appBuildID.map { "\(number) · \($0)" } ?? number])
    }

    /// App 菜单「设置…」（Cmd+,）：分页的设置窗口（M4a，docs/design.md 10.4 节）。
    /// 「打开日志文件夹」（1.0.0 之前，docs/design.md 10.5 节决策 23）：在 Finder 中打开 App 与后台日志所在的目录。
    @objc func openLogFolder(_ sender: Any?) {
        try? FileManager.default.createDirectory(atPath: core.logDir, withIntermediateDirectories: true)
        NSWorkspace.shared.open(URL(fileURLWithPath: core.logDir, isDirectory: true))
    }

    /// 「帮助 › 报告问题…」（2026-11-02，docs/design.md 10.6 节「问题反馈」）；两个提示框里的同名按钮带上提示的标题。
    @objc func reportProblem(_ sender: Any?) { showReport(prefill: "") }

    /// 「macOS 27.0.1 (26A434)」：`operatingSystemVersionString` 随系统语言变，摘要要固定写法。
    private static var systemVersion: String {
        let v = ProcessInfo.processInfo.operatingSystemVersion
        var size = 0
        sysctlbyname("kern.osversion", nil, &size, nil, 0)
        var build = [CChar](repeating: 0, count: max(size, 1))
        sysctlbyname("kern.osversion", &build, &size, nil, 0)
        return "macOS \(v.majorVersion).\(v.minorVersion).\(v.patchVersion) (\(String(cString: build)))"
    }

    private func showReport(prefill: String) {
        if let w = reportWindow?.window {
            w.makeKeyAndOrderFront(nil)
            return
        }
        let info = Bundle.main.infoDictionary
        let engine: String? = {
            let i = ghostty_info()
            guard let p = i.version else { return nil }
            return String(decoding: UnsafeRawBufferPointer(start: p, count: Int(i.version_len)), as: UTF8.self)
        }()
        let env = ProblemReport.Environment(
            appVersion: info?["CFBundleShortVersionString"] as? String ?? "-",
            buildNumber: info?["CFBundleVersion"] as? String ?? "-",
            buildID: core.appBuildID, daemonBuildID: core.daemonBuildID,
            system: Self.systemVersion,
            language: Bundle.main.preferredLocalizations.first ?? "-",
            engine: engine, hooks: hookJSON, inaccurate: banner.details, lastUnexpectedExit: lastExitAt)
        do {
            let w = try ProblemReportWindow(env: env, prefill: prefill, logDir: core.logDir)
            w.onClose = { [weak self] in self?.reportWindow = nil }
            reportWindow = w
            NSApp.activate(ignoringOtherApps: true)
            window.beginSheet(w.window!)
            scyllauiLog.notice("problem_report opened")
        } catch {
            scyllauiLog.error("problem_report folder_failed error=\(error.localizedDescription, privacy: .public)")
            let alert = NSAlert()
            alert.messageText = String(localized: "诊断包生成失败")
            alert.informativeText = String(localized: "可以点「打开日志文件夹」，把日志直接发给开发者。") + "\n\n" + error.localizedDescription
            alert.addButton(withTitle: String(localized: "好"))
            alert.addButton(withTitle: String(localized: "打开日志文件夹"))
            alert.beginSheetModal(for: window) { [weak self] response in
                if response == .alertSecondButtonReturn { self?.openLogFolder(nil) }
            }
        }
    }

    /// 「状态可能不准」的「详情…」（M4b 块 6，docs/design.md 10.6 节）：每款的版本与原因、影响、根源与最新的已验证版本，
    /// 附「打开日志文件夹」便于反馈。ScyllaUI 不生成也不执行降级命令（四款安装方式不同，属于全局安装）。
    private func showInaccurateDetails(_ items: [InaccurateItem]) {
        guard !items.isEmpty, window.attachedSheet == nil else { return }
        let names = items.map { SidebarModel.toolName($0.tool) }.joined(separator: String(localized: "、"))
        var lines = items.map { i in
            ([SidebarModel.toolName(i.tool)] + i.versions).joined(separator: " ") + String(localized: "：") + i.reasons.joined(separator: String(localized: "；"))
        }
        lines.append("")
        lines.append(String(localized: "只影响侧栏显示状态、通知与「待查看」，Agent 本身和终端不影响，可能 Agent 新版本更改了 Hook 数据格式。"))
        let latest = items.compactMap { i in i.verifiedLatest.map { "\(SidebarModel.toolName(i.tool)) \($0)" } }
        if !latest.isEmpty {
            let list = latest.joined(separator: String(localized: "、"))
            lines.append(String(localized: "待 ScyllaUI 版本升级，如在意，可先安装回状态提示前的 Agent 版本：\(list)（做法见使用说明「状态同步」一节）。"))
        }
        lines.append("")
        lines.append(String(localized: "反馈问题时点「报告问题…」，诊断包会带上日志和各 Agent 的版本。"))
        let alert = NSAlert()
        alert.useWideLayout()
        alert.messageText = String(localized: "状态可能不准：\(names)")
        alert.informativeText = lines.joined(separator: "\n")
        alert.addButton(withTitle: String(localized: "好"))
        alert.addButton(withTitle: String(localized: "报告问题…"))
        scyllauiLog.notice("inaccurate_details tools=\(items.map(\.tool).joined(separator: ","), privacy: .public)")
        alert.beginSheetModal(for: window) { [weak self] response in
            if response == .alertSecondButtonReturn { self?.showReport(prefill: alert.messageText) }
        }
    }

    @objc func showSettings(_ sender: Any?) {
        let s = settings ?? SettingsWindowController(runtime: runtime, core: core) { [weak self] in
            // 设置里改字号：所有已建界面的终端换成新字号，包括用 Cmd+= / Cmd+- 临时调过的（docs/design.md 10.4 节决策 2）。
            guard let self else { return }
            let surfaces = self.sessions.values.filter(\.hasView).compactMap { $0.view.surface }
            surfaces.forEach(GhosttyRuntime.resetFontSize)
            scyllauiLog.notice("font size reset terminals=\(surfaces.count)")
        }
        settings = s
        s.show()
    }

    /// Cmd+F：键盘交给侧栏的搜索框；侧栏收起时先展开（M2 第 4 块）。
    @objc func focusSearch(_ sender: Any?) {
        // 键盘焦点在查看器里时是查看器自己的查找条（docs/design.md 5.12 节「查看器」）。
        if rightPanel == .files, filesPanel.viewerOpen, filesPanel.viewer.holdsFocus(window.firstResponder) {
            return filesPanel.viewer.showFindBar()
        }
        if rightPanel == .diff, diffPanel.contentShown, diffPanel.viewer.holdsFocus(window.firstResponder) {
            return diffPanel.viewer.showFindBar()
        }
        if sidebarCollapsed { toggleSidebarPane(nil) }
        sidebar.focusSearch()
    }

    @objc func toggleSidebarPane(_ sender: Any?) {
        let hide = !sidebarCollapsed
        if hide, let r = window.firstResponder as? NSView, r.isDescendant(of: sideView),
           let view = currentID.flatMap({ sessions[$0]?.view }) {
            window.makeFirstResponder(view)
        }
        sidebarCollapsed = hide
        if hide {
            split.removeArrangedSubview(sideView)
            sideView.removeFromSuperview()
        } else {
            split.insertArrangedSubview(sideView, at: 0)
            let need = savedSidebarWidth + split.dividerThickness + terminalMinSize.width
            let have = window.contentLayoutRect.width
            if have < need {
                var f = window.frame
                f.size.width += need - have
                window.setFrame(f, display: true)
            }
        }
        split.adjustSubviews()
        if !hide { setSidebarWidth(savedSidebarWidth, save: false) }
        updateMinSize()
        scyllauiLog.notice("sidebar collapsed=\(hide)")
        GeometryLog.shared?.write("window", "event=sidebar_\(hide ? "collapse" : "expand") terminal_area=\(Int(container.bounds.width))x\(Int(container.bounds.height))")
    }

    /// 窗口缩放（拖边、放大、全屏）时侧栏保持宽度，只改终端宽度（docs/design.md 第 5 节「宽度与连线」）。两侧的 frame 由
    /// 分隔视图直接设置（autoresizing 约束），Auto Layout 的保持优先级不起作用（2026-10-23 实测：侧栏被按比例压到 248 pt、
    /// 放大时拉到约 640 pt），所以在这里明确不让侧栏参与缩放；分隔线拖动不受影响。
    func splitView(_ splitView: NSSplitView, shouldAdjustSizeOfSubview view: NSView) -> Bool {
        view !== sideView
    }

    func splitView(_ splitView: NSSplitView, constrainMinCoordinate proposedMinimumPosition: CGFloat,
                   ofSubviewAt dividerIndex: Int) -> CGFloat {
        ScyllaUIStyle.sidebarMinWidth
    }

    func splitView(_ splitView: NSSplitView, constrainMaxCoordinate proposedMaximumPosition: CGFloat,
                   ofSubviewAt dividerIndex: Int) -> CGFloat {
        let room = splitView.bounds.width - splitView.dividerThickness - terminalMinSize.width - promptPanelExtent
        return max(ScyllaUIStyle.sidebarMinWidth, min(ScyllaUIStyle.sidebarMaxWidth, room))
    }

    /// 用户拖动分隔线时记住宽度。只认鼠标拖动：首次布局时分隔视图平分宽度，通知同样带分隔线序号，
    /// 不能当成用户的选择（2026-10-11 测试中曾因此存下 750、被限成 420）。
    func splitViewDidResizeSubviews(_ notification: Notification) {
        guard notification.userInfo?["NSSplitViewDividerIndex"] != nil, !sidebarCollapsed,
              NSApp.currentEvent?.type == .leftMouseDragged else { return }
        UserDefaults.standard.set(Double(sideView.frame.width), forKey: Self.sidebarWidthKey)
        updateMinSize()
    }

    /// 用户正在看的终端：窗口是 key 时为当前终端，否则为空（docs/protocol.md 第 4 节 `terminal.focus`）。
    /// 换到另一个终端时先报空，停留 0.5 秒再报它：用上下键扫过的终端不算看过，「待查看」不会被一扫而空。
    private func updateFocus() {
        focusSettle?.cancel()
        let id = window.isKeyWindow ? currentID : nil
        guard let id, id != reportedFocus else {
            if id == nil { reportedFocus = nil; core.focus(id: nil) }
            return
        }
        reportedFocus = nil
        core.focus(id: nil)
        let item = DispatchWorkItem { [weak self] in
            self?.reportedFocus = id
            self?.core.focus(id: id)
        }
        focusSettle = item
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5, execute: item)
    }

    /// 后台的快照与终端推送（M3）：记下存活的终端；第一份快照到时接回后台仍持有的终端；后台已关闭的终端移除
    /// （Shell 退出、被关闭，或后台崩溃重启后按 `pty_lost` 关闭）。
    private func route(_ msg: [String: Any], terminalID: String?) {
        switch msg["type"] as? String {
        case "snapshot":
            let list = msg["terminals"] as? [[String: Any]] ?? []
            let before = alive
            alive = Set(list.compactMap { $0["terminal_id"] as? String })
            // 之前在后台存活、这份快照里没有了的（后台崩溃重启后按 pty_lost 关闭等）：移除。刚新建、后台还没推送过的不动。
            for s in sessions.values where before.contains(s.id) && !alive.contains(s.id) { remove(s) }
            // 后台被结束或重启电脑后丢掉的终端（App 启动时，或 App 开着时后台崩溃重连）：按设置恢复（M4a 追加块 10），
            // 这时不另开 Home Shell。
            let lost = msg["restorable"] as? [[String: Any]] ?? []
            let first = restorePending
            if restorePending {
                restore(list, projects: msg["projects"] as? [[String: Any]] ?? [], lastFocus: msg["last_focus"] as? String,
                        openHome: lost.isEmpty || !Prefs.restoreTerminals)
            }
            if !lost.isEmpty { restoreLost(lost) }
            if first { noteUnexpectedExit(held: list.filter { $0["held"] as? Bool == true }.count, lost: lost.count) }
            for r in msg["respawned"] as? [[String: Any]] ?? [] { resumeRespawned(r) }
        case "terminal.respawned":
            resumeRespawned(msg)
        case "terminal.upsert":
            guard let t = msg["terminal"] as? [String: Any], let id = terminalID else { return }
            if t["status"] as? String == "closed" {
                alive.remove(id)
                if let s = sessions[id] { remove(s) }
            } else {
                alive.insert(id)
            }
        default:
            break
        }
    }

    /// App 启动后的第一份快照（M3）：为后台仍持有的终端建会话对象（Surface 等第一次显示时再建），先显示 App 断开前
    /// 用户在看的那个（`last_focus`），没有时显示最近建的；一个都没有才新开 Home Shell。
    private func restore(_ list: [[String: Any]], projects: [[String: Any]], lastFocus: String?, openHome: Bool) {
        restorePending = false
        // 开屏动画等接回之后再播（M4a 块 4）。
        defer { welcome?.mainWindowRestored() }
        var dirs: [Int64: String] = [:]
        for p in projects {
            if let id = (p["project_id"] as? NSNumber)?.int64Value, let dir = p["dir"] as? String { dirs[id] = dir }
        }
        let held = list
            .filter { $0["held"] as? Bool == true && sessions[$0["terminal_id"] as? String ?? ""] == nil }
            .sorted { (($0["created_at"] as? NSNumber)?.int64Value ?? 0) < (($1["created_at"] as? NSNumber)?.int64Value ?? 0) }
        for t in held {
            guard let id = t["terminal_id"] as? String, let cwd = t["cwd"] as? String else { continue }
            let project = (t["project_id"] as? NSNumber).flatMap { dirs[$0.int64Value] } ?? cwd
            let session = TerminalSession(app: app, core: core, restoring: id, projectDir: project,
                                          directory: t["current_dir"] as? String ?? cwd)
            session.onSurfaceClosed = { [weak self] s in self?.surfaceClosed(s) }
            sessions[id] = session
        }
        scyllauiLog.notice("restore held=\(held.count) last_focus=\(lastFocus ?? "-", privacy: .public)")
        GeometryLog.shared?.write("restore", "held=\(held.count)")
        if currentID != nil { return }
        let last = held.last?["terminal_id"] as? String
        if let id = lastFocus.flatMap({ sessions[$0] != nil ? $0 : nil }) ?? last {
            show(id)
        } else if sessions.isEmpty && openHome {
            newTerminal(directory: NSHomeDirectory())
        }
    }

    // MARK: 重启后恢复终端（M4a 追加块 10，docs/design.md 10.5 节决策 12）

    /// 逐个重建：原来跑着 AI 对话的走一键恢复（在对话的目录代输恢复命令）；恢复不了或纯 Shell 的在断电前的目录开 Shell
    /// （目录不在退回启动目录，再退回主目录）。连续两次恢复中途退出即暂停，改为询问。不结束任何进程。
    private func restoreLost(_ lost: [[String: Any]]) {
        guard Prefs.restoreTerminals else {
            scyllauiLog.notice("restore lost=\(lost.count) skipped=pref_off")
            return
        }
        if Prefs.restoreAttempts >= 2 {
            scyllauiLog.notice("restore lost=\(lost.count) paused attempts=\(Prefs.restoreAttempts)")
            let alert = NSAlert()
            alert.messageText = String(localized: "上次恢复终端时中途退出了两次")
            alert.informativeText = String(localized: "这次没有自动恢复 \(lost.count) 个终端。它们的 Agent 对话都在「最近任务」里，也可以双击逐个恢复。")
            alert.addButton(withTitle: String(localized: "现在恢复"))
            alert.addButton(withTitle: String(localized: "不恢复"))
            alert.beginSheetModal(for: window) { [weak self] response in
                Prefs.restoreAttempts = 0
                if response == .alertFirstButtonReturn { self?.restoreLost(lost) }
            }
            return
        }
        Prefs.restoreAttempts += 1
        scyllauiLog.notice("restore lost=\(lost.count) attempt=\(Prefs.restoreAttempts)")
        let home = NSHomeDirectory()
        func usable(_ dir: Any?) -> String? {
            guard let dir = dir as? String else { return nil }
            var isDir: ObjCBool = false
            return FileManager.default.fileExists(atPath: dir, isDirectory: &isDir) && isDir.boolValue ? dir : nil
        }
        let group = DispatchGroup()
        // 断电前在看的终端（重建时每开一个都会切过去，先记下），恢复完把焦点交给它的替身。
        let focus = Prefs.lastTerminal
        // 先向后台要齐各段对话的恢复命令，全部回来后再按原顺序开终端：边要边开时纯 Shell 先开、带 AI 的等应答后才开，
        // 带 AI 的终端会排到组尾，顺序与断电前不一致（2026-10-25 块 11 复测发现）。
        var replies = [[String: Any]?](repeating: nil, count: lost.count)
        for (i, t) in lost.enumerated() {
            guard let cid = (t["conversation_id"] as? NSNumber)?.int64Value else { continue }
            group.enter()
            core.resume(conversationID: cid) { reply in
                DispatchQueue.main.async {
                    replies[i] = reply
                    group.leave()
                }
            }
        }
        group.notify(queue: .main) { [weak self] in
            guard let self else { return }
            var focusNew: String?
            for (i, t) in lost.enumerated() {
                // 两个目录都不在：把原启动目录照常交给后台，由它退回主目录并在侧栏标「目录不存在」（2026-11-14，
                // 第二轮长跑缺陷 K；原来在这里直接换成主目录，没有任何提示）。
                let dir = usable(t["dir"]) ?? usable(t["cwd"]) ?? (t["cwd"] as? String) ?? home
                let name = (t["name"] as? String).flatMap { $0.isEmpty ? nil : $0 }
                var id: String?
                if let cid = (t["conversation_id"] as? NSNumber)?.int64Value {
                    let reply = replies[i]
                    if reply?["ok"] as? Bool == true, reply?["action"] as? String == "open",
                       let command = reply?["command"] as? String, let cdir = usable(reply?["dir"]) {
                        id = self.open(directory: cdir, input: command + "\n", resumeOf: cid)
                        if let name, let id { self.core.rename(id: id, name: name) }
                    } else if reply?["ok"] as? Bool == true, reply?["action"] as? String == "open", let cdir = reply?["dir"] as? String {
                        // 对话的启动目录不在了：不代输恢复命令，同样由后台退回主目录并标「目录不存在」。
                        scyllauiLog.notice("restore conversation=\(cid) skipped=dir_missing")
                        id = self.restoreShell(dir: cdir, name: name)
                    } else if reply?["action"] as? String != "switch" {
                        id = self.restoreShell(dir: dir, name: name)
                    }
                } else {
                    id = self.restoreShell(dir: dir, name: name)
                    // 原来前台是某款 AI、却没有可恢复的对话（M4b 块 5 P2）：说明没能接回的原因。
                    if let tool = t["fg_tool"] as? String { self.noteLostAgent(tool) }
                }
                if focus != nil, t["terminal_id"] as? String == focus { focusNew = id }
            }
            Prefs.restoreAttempts = 0
            if let id = focusNew, self.sessions[id] != nil { self.show(id) }
            scyllauiLog.notice("restore done lost=\(lost.count) focus=\(focusNew.map { String($0.prefix(8)) } ?? "-", privacy: .public)")
        }
    }

    /// 后台崩溃重启后在原终端重起了 Shell，原来的 AI 已随旧后台结束（`terminal.respawned`，docs/design.md 10.5 节决策 21）：
    /// 按「启动时恢复上次的终端」开关，在同一终端恢复原对话（先 `cd` 到对话目录）。命令由后台当键盘输入写进这个终端的 PTY
    /// （`type_into` → `typed`）：App 用 `sendText` 代输按粘贴处理，zsh 开着括号粘贴时连同回车留在输入行上不执行（2026-11-13
    /// 长跑缺陷 C）。同一对话 2 分钟内只接一次，避免恢复即崩溃时反复重来。
    private func resumeRespawned(_ m: [String: Any]) {
        guard let id = m["terminal_id"] as? String else { return }
        guard let cid = (m["conversation_id"] as? NSNumber)?.int64Value else {
            // 没有可恢复的对话、原来前台是某款 AI（M4b 块 5 P2）：说明没能接回的原因。
            if let tool = m["fg_tool"] as? String, Prefs.restoreTerminals { noteLostAgent(tool) }
            return
        }
        guard Prefs.restoreTerminals else {
            return scyllauiLog.notice("respawn resume skipped=pref_off terminal=\(id.prefix(8), privacy: .public) conversation=\(cid)")
        }
        let now = Date()
        if let last = respawnResumed[cid], now.timeIntervalSince(last) < 120 {
            return scyllauiLog.notice("respawn resume paused=recent terminal=\(id.prefix(8), privacy: .public) conversation=\(cid)")
        }
        respawnResumed[cid] = now
        core.resume(conversationID: cid, typeInto: id) { reply in
            // 只记日志，终端里不另做什么：`typed` 已由后台输入；`gone`（会话记录已不存在）按无法恢复的规则留在这个目录的 Shell；
            // `busy`（前台已是 AI 或其他命令）不打扰；`not_found`（终端没了或没有 PTY）、`switch`（对话已在别处运行）、未连接同样不动。
            let action = reply?["action"] as? String
                ?? ((reply?["error"] as? [String: Any])?["code"] as? String).map { "error:\($0)" } ?? "no_reply"
            scyllauiLog.notice("respawn resume terminal=\(id.prefix(8), privacy: .public) conversation=\(cid) result=\(action, privacy: .public)")
        }
    }

    private func noteLostAgent(_ tool: String) {
        lostAgentTools.append(tool)
        lostAgentReport?.cancel()
        let work = DispatchWorkItem { [weak self] in self?.reportLostAgents() }
        lostAgentReport = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 1, execute: work)
    }

    /// 没能接回的 AI（M4b 块 5 P2，docs/design.md 10.6 节）：有状态同步未生效的才提示，附「修复…」（只修这些工具）；
    /// 状态同步正常的（多半还没开始对话）只在同一提示里附一行，单独出现时不提示、只记日志。
    private func reportLostAgents() {
        let tools = lostAgentTools
        lostAgentTools = []
        let lost = LostAgents(tools: tools, hooks: hookStatuses, removed: Prefs.hooksRemoved)
        scyllauiLog.notice("restore lost_agents tools=\(tools.joined(separator: ","), privacy: .public) broken=\(lost.broken.map(\.tool).joined(separator: ","), privacy: .public) idle=\(lost.idle.map(\.tool).joined(separator: ","), privacy: .public)")
        guard !lost.broken.isEmpty else { return }
        var lines = lost.broken.map { item -> String in
            let reason = hookStatuses.first { $0.tool == item.tool }?.reason ?? ""
            return String(localized: "\(SidebarModel.toolName(item.tool))：状态同步未生效（\(reason)），\(item.count) 个终端已恢复为 Shell")
        }
        lines += lost.idle.map { String(localized: "\(SidebarModel.toolName($0.tool))：还没开始对话，\($0.count) 个终端已恢复为 Shell") }
        let alert = NSAlert()
        alert.useWideLayout()
        alert.messageText = String(localized: "有终端没能接回 Agent")
        alert.informativeText = lines.joined(separator: "\n")
        alert.addButton(withTitle: String(localized: "修复…"))
        alert.addButton(withTitle: String(localized: "好"))
        let broken = lost.broken.map(\.tool)
        alert.beginSheetModal(for: window) { [weak self] response in
            guard response == .alertFirstButtonReturn, let self else { return }
            HookRepairFlow.run(.repair, tools: broken, core: self.core)
        }
    }

    /// 第一份快照处理完（接回或按设置恢复之后）：上次意外退出时记下提示，等欢迎窗口关闭后再弹（M4b 块 7，docs/design.md 10.6 节）。
    private func noteUnexpectedExit(held: Int, lost: Int) {
        guard let notice = UnexpectedExit(lastAlive: lastAlive, held: held, lost: lost) else { return }
        lastAlive = nil
        lastExitAt = notice.lastAlive
        scyllauiLog.notice("unexpected_exit last_alive=\(ISO8601DateFormatter().string(from: notice.lastAlive), privacy: .public) held=\(held) lost=\(lost)")
        exitNotice = notice
        showExitNotice()
    }

    /// 窗口上已有别的提示框时，系统把这个排在它后面，关掉前一个再出现。
    private func showExitNotice() {
        guard welcome == nil, let notice = exitNotice else { return }
        exitNotice = nil
        let alert = NSAlert()
        alert.useWideLayout()
        alert.messageText = notice.title()
        alert.informativeText = notice.message
        alert.addButton(withTitle: String(localized: "好"))
        alert.addButton(withTitle: String(localized: "报告问题…"))
        alert.beginSheetModal(for: window) { [weak self] response in
            if response == .alertSecondButtonReturn { self?.showReport(prefill: alert.messageText) }
        }
    }

    /// 引导用户去 Codex 里信任 ScyllaUI 的 Hook（M4b 块 5 P1 方案 B；2026-11-17 重做，docs/design.md 4.7 节「引导用户信任」）：
    /// ScyllaUI 不代写信任记录，由用户在 Codex 自己的界面里确认。先判断 Codex 的启动时间是否早于写入 Hook 的时间：早于，
    /// 这个 Codex 读不到新写入的条目，提示重开；否则把 `/hooks` 像粘贴一样填进它的输入行（不回车）。终端依次取 `preferred`
    /// （侧栏「未同步」那一行）、当前显示的、侧栏里第一个前台为 Codex 的；都没有时说明怎么做，不自动启动 Codex。
    /// `afterRepair`：修复刚写入、接着提示这一步。写入时间向后台现取，不用推送来的：紧接修复之后，推送可能还没到。
    private func openCodexHooks(preferred: String?, afterRepair: Bool) {
        window.makeKeyAndOrderFront(nil)
        guard window.attachedSheet == nil else { return NSSound.beep() }
        core.request(["type": "hooks.status"]) { [weak self] reply in
            let codex = ((reply["hooks"] as? [[String: Any]]) ?? []).first { $0["tool"] as? String == "codex" }
            let changed = (codex?["changed_at"] as? NSNumber)?.int64Value
            let stateChanged = (codex?["state_changed_at"] as? NSNumber)?.int64Value
            DispatchQueue.main.async {
                self?.guideCodexTrust(preferred: preferred, afterRepair: afterRepair, hookChangedAt: changed, stateChangedAt: stateChanged)
            }
        }
    }

    /// `hookChangedAt` 是 `hooks.json` 的修改时间，`stateChangedAt` 是后台记下的配置状态最近一次变化的时间（待信任时即条目写入的
    /// 时间）。后台查得到「这个 Codex 启动时条目在不在」就以它分路、写后一个时间；查不到再比较启动时间与文件修改时间。
    private func guideCodexTrust(preferred: String?, afterRepair: Bool, hookChangedAt: Int64?, stateChangedAt: Int64?) {
        guard window.attachedSheet == nil else { return NSSound.beep() }
        let running = Set(statusInfos.filter { $0.value.fgAgent == "codex" }.map(\.key))
        let id = [preferred, currentID].compactMap { $0 }.first(where: running.contains)
            ?? sidebar.terminalOrder.first(where: running.contains) ?? running.first
        let alert = NSAlert()
        alert.useWideLayout()
        alert.messageText = afterRepair ? String(localized: "还差一步：在 Codex 里信任 ScyllaUI 的 Hook") : String(localized: "在 Codex 里信任 ScyllaUI 的 Hook")
        guard let id, sessions[id] != nil else {
            if !afterRepair { alert.messageText = String(localized: "没有正在运行的 Codex") }
            alert.informativeText = CodexTrustGuide.notRunningText
            scyllauiLog.notice("codex_trust_guide route=no_codex after_repair=\(afterRepair)")
            alert.beginSheetModal(for: window)
            return
        }
        show(id)
        let startedAt = statusInfos[id]?.fgStartedAt
        let entriesAtStart = statusInfos[id]?.fgEntriesAtStart
        let route = CodexTrustGuide.route(startedAt: startedAt, hookChangedAt: hookChangedAt, entriesAtStart: entriesAtStart)
        let writtenAt = entriesAtStart == false ? (stateChangedAt ?? hookChangedAt) : hookChangedAt
        scyllauiLog.notice("codex_trust_guide route=\(route == .restart ? "restart" : "hooks_list", privacy: .public) terminal=\(id.prefix(8), privacy: .public) after_repair=\(afterRepair) started_at=\(startedAt ?? 0) changed_at=\(writtenAt ?? 0) entries_at_start=\(entriesAtStart.map { $0 ? "yes" : "no" } ?? "unknown", privacy: .public)")
        if route == .restart {
            if !afterRepair { alert.messageText = String(localized: "这个 Codex 需要重开一次") }
            alert.informativeText = CodexTrustGuide.restartText(startedAt: startedAt, hookChangedAt: writtenAt)
            alert.addButton(withTitle: String(localized: "好"))
            alert.beginSheetModal(for: window) { [weak self] _ in
                guard let self, let s = self.sessions[id], s.hasView else { return }
                self.window.makeFirstResponder(s.view)
            }
            return
        }
        alert.informativeText = CodexTrustGuide.hooksListText
        alert.addButton(withTitle: String(localized: "填入 /hooks"))
        alert.addButton(withTitle: String(localized: "取消"))
        alert.beginSheetModal(for: window) { [weak self] response in
            guard response == .alertFirstButtonReturn, let self, let s = self.sessions[id], s.hasView else { return }
            s.view.sendText("/hooks")
            self.window.makeFirstResponder(s.view)
            scyllauiLog.notice("codex_hooks_fill terminal=\(id.prefix(8), privacy: .public) after_repair=\(afterRepair)")
        }
    }

    /// 配置正常、但这个 Agent 进程启动时条目还没写入（`foreground.sync` 为 `new_session` 或 `restart`，docs/design.md 4.7 节
    /// 「会话是否真的在同步」）：只说明怎么开新会话或重开，不往终端里填内容。
    private func guideReopen(terminal id: String, tool: String, sync: String) {
        window.makeKeyAndOrderFront(nil)
        guard window.attachedSheet == nil else { return NSSound.beep() }
        let name = SidebarModel.toolName(tool)
        let commands = hookStatuses.first { $0.tool == tool }?.commands ?? SyncGuide.Commands()
        let startedAt = statusInfos[id]?.fgStartedAt
        let changedAt = (hookJSON.first { $0["tool"] as? String == tool }?["state_changed_at"] as? NSNumber)?.int64Value
        if sessions[id] != nil { show(id) }
        scyllauiLog.notice("sync_guide kind=\(sync, privacy: .public) tool=\(tool, privacy: .public) terminal=\(id.prefix(8), privacy: .public) started_at=\(startedAt ?? 0) changed_at=\(changedAt ?? 0)")
        let alert = NSAlert()
        alert.useWideLayout()
        alert.messageText = String(localized: "这个 \(name) 要重开后才同步")
        alert.informativeText = SyncGuide.text(tool: name, sync: sync, commands: commands, startedAt: startedAt, changedAt: changedAt)
        alert.addButton(withTitle: String(localized: "好"))
        alert.beginSheetModal(for: window) { [weak self] _ in
            guard let self, let s = self.sessions[id], s.hasView else { return }
            self.window.makeFirstResponder(s.view)
        }
    }

    /// 修复写入成功后：有正在运行的会话要重开才同步时，接着说明一次，每款一行（同上一节）。终端的新情况随推送到，
    /// 所以稍等一下再看；Codex 还要信任时不走这里，以信任引导为准。
    private func reportReopenNeeded(_ tools: [String]) {
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.6) { [weak self] in
            guard let self, self.window.attachedSheet == nil else { return }
            var kinds: [String: String] = [:]
            for info in self.statusInfos.values {
                if let tool = info.fgAgent, tools.contains(tool), let sync = info.fgSync, SyncGuide.needsReopen(sync) { kinds[tool] = sync }
            }
            let lines = tools.compactMap { tool -> String? in
                guard let sync = kinds[tool] else { return nil }
                let commands = self.hookStatuses.first { $0.tool == tool }?.commands ?? SyncGuide.Commands()
                return SyncGuide.line(tool: SidebarModel.toolName(tool), sync: sync, commands: commands)
            }
            scyllauiLog.notice("sync_after_repair tools=\(tools.joined(separator: ","), privacy: .public) reopen=\(kinds.keys.sorted().joined(separator: ","), privacy: .public)")
            guard !lines.isEmpty else { return }
            self.window.makeKeyAndOrderFront(nil)
            let alert = NSAlert()
            alert.useWideLayout()
            alert.messageText = String(localized: "修复完成，正在运行的会话要重开后才同步")
            alert.informativeText = String(localized: "这些会话在写入 Hook 之前就已启动，读不到新写入的 Hook：") + "\n\n" + lines.joined(separator: "\n")
                + "\n\n" + String(localized: "这些终端的行上显示「未同步」，点它可以再看到做法。")
            alert.addButton(withTitle: String(localized: "好"))
            alert.beginSheetModal(for: self.window)
        }
    }

    @discardableResult
    private func restoreShell(dir: String, name: String?) -> String {
        let id = open(directory: dir, input: nil)
        if let name { core.rename(id: id, name: name) }
        return id
    }

    /// 「打开文件夹…」：在侧栏里还没有的目录新建 Shell（docs/design.md 10.5 节决策 6）；选中目录的归组由后台按
    /// Git 仓库根判定，规则与 `cd` 相同。
    @objc func openFolder(_ sender: Any?) {
        guard window.attachedSheet == nil else { return }
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.canCreateDirectories = true
        panel.prompt = String(localized: "打开")
        panel.message = String(localized: "选择要在其中新建终端的文件夹")
        panel.beginSheetModal(for: window) { [weak self] response in
            guard response == .OK, let url = panel.url else { return }
            // 符号链接路径（如 /var → /private/var）先解析成真实路径，与后台判定项目的规则一致（docs/design.md 第 5 节
            // 「实现（维护期 0.3.5）」）。Foundation 的 resolvingSymlinksInPath 反而会去掉 /private，所以用 realpath。
            let dir = realpath(url.path, nil).map { p in defer { free(p) }; return String(cString: p) } ?? url.path
            scyllauiLog.notice("open folder dir=\(dir, privacy: .public)")
            self?.newTerminal(directory: dir)
        }
    }

    /// 额度桥的命令被改过、不能自动处理时的说明（2026-11-22）。
    private func explainBridge(_ h: SBHookStatus) {
        let alert = NSAlert()
        alert.messageText = String(localized: "Claude 的额度读不到")
        alert.informativeText = h.bridgeReason ?? ""
        alert.beginSheetModal(for: window)
    }

    // MARK: 历史提示词（M4a 追加块 9，docs/design.md 10.5 节决策 10）

    /// 右侧栏的四个开关（docs/design.md 5.12 节）：再按一次同一个为关，按别的直接换过去。
    @objc func toggleStatusPanel(_ sender: Any?) { show(RightPanel.toggled(rightPanel, .status)) }
    @objc func toggleFilesPanel(_ sender: Any?) { show(RightPanel.toggled(rightPanel, .files)) }
    @objc func toggleDiffPanel(_ sender: Any?) { show(RightPanel.toggled(rightPanel, .diff)) }
    @objc func togglePromptHistory(_ sender: Any?) { show(RightPanel.toggled(rightPanel, .history)) }

    /// 打开某一栏或收起（nil）。打开时窗口不够宽先加宽，终端区保持至少 40 列。
    private func show(_ panel: RightPanel?) {
        guard panel != rightPanel else { return }
        let wasOpen = rightPanel != nil
        setZoom(false)
        rightPanel = panel
        for (b, p) in [(statusButton, RightPanel.status), (filesButton, .files), (diffButton, .diff), (historyButton, .history)] {
            b.state = panel == p ? .on : .off
        }
        rightHost.setContent(panel.map { p -> NSView in
            switch p {
            case .status: return statusPanel
            case .files: return filesPanel
            case .diff: return diffPanel
            case .history: return promptPanel
            }
        })
        rightHost.isHidden = panel == nil
        if wasOpen != (panel != nil) {
            containerToEdge?.isActive = panel == nil
            containerToPanel?.isActive = panel != nil
        }
        if panel == .files { refreshFilesPanel() }
        if panel == .diff { refreshDiffPanel() }
        // 状态栏的「更改」一段与「更改」栏同一份数据，状态栏打开时由 `refreshStatusPanel` 取（第 6 版 R3）。
        if panel == .files || panel == .diff, let root = currentProjectRoot, currentRow?.branch != nil { changes.refresh(root: root) }
        updateFolderWatch()
        applyRightWidth(grow: true)
        if panel == nil { updateMinSize() }
        scyllauiLog.notice("right panel=\(String(describing: panel), privacy: .public)")
        switch panel {
        case .history:
            promptKey = nil
            refreshPromptHistory(key: nil)
        case .status:
            refreshStatusPanel()
        default:
            break
        }
    }

    /// 当前终端所在项目的根：项目目录（Git 工作区根；不在仓库里为终端目录归属的项目目录），没有时为终端目录。
    private var currentProjectRoot: String? {
        currentRow.map { $0.projectDir ?? $0.currentDir ?? "" }.flatMap { $0.isEmpty ? nil : $0 }
    }

    /// 文件栏跟着当前终端所在的项目（开着时才换），在仓库里才判忽略。
    private func refreshFilesPanel() {
        guard rightPanel == .files else { return }
        let root = currentProjectRoot
        let changed = root != filesPanel.tree?.root.path
        filesPanel.setRoot(root, gitRoot: currentRow?.branch != nil ? root : nil, currentDir: currentRow?.currentDir)
        if changed, let root {
            filesPanel.setMarks(changes.marks(root), dirty: changes.dirtyDirs(root))
            if currentRow?.branch != nil { changes.refresh(root: root) }
        }
        updateFolderWatch()
    }

    /// Diff 栏跟着当前终端所在的项目（开着时才换）；换根时先用存着的结果，再重取。
    private func refreshDiffPanel() {
        guard rightPanel == .diff else { return }
        let root = currentProjectRoot
        let inRepo = currentRow?.branch != nil
        let changed = root != diffPanel.root
        diffPanel.setRoot(root, inRepo: inRepo, branch: currentRow?.branch, currentDir: currentRow?.currentDir)
        if changed, let root, inRepo {
            if let snap = changes.snapshot(root) { diffPanel.update(snap) }
            changes.refresh(root: root)
        }
        updateFolderWatch()
    }

    /// FSEvents 流只在开着「文件」「更改」或（在仓库里时）状态栏时对当前根开，换根就换（docs/design.md 5.12 节「自动刷新」）：
    /// 文件树重读变了的目录，在仓库里时重取更改列表。
    private func updateFolderWatch() {
        let root: String?
        switch rightPanel {
        case .files: root = filesPanel.tree?.root.path
        case .diff: root = diffPanel.root
        case .status: root = currentRow?.branch != nil ? currentProjectRoot : nil
        default: root = nil
        }
        guard root != folderWatch?.root else { return }
        folderWatch?.stop()
        folderWatch = root.flatMap { r in
            FolderWatch(root: r) { [weak self] dirs in
                guard let self else { return }
                if self.rightPanel == .files { self.filesPanel.refresh(dirs: dirs) }
                if self.rightPanel != nil, self.rightPanel != .history, self.currentRow?.branch != nil { self.changes.refresh(root: r) }
            }
        }
    }

    /// 状态栏上一次取「更改」的根：换了（切终端、终端换目录到别的项目）才重取并换 FSEvents 流。
    private var statusChangesRoot: String?

    /// 状态栏跟着当前终端这一行（开着时才排）。
    private func refreshStatusPanel() {
        guard rightPanel == .status else {
            statusChangesRoot = nil
            return
        }
        let info = currentID.flatMap { statusInfos[$0] }
        let changesRoot = currentRow?.branch != nil ? currentProjectRoot : nil
        if changesRoot != statusChangesRoot {
            statusChangesRoot = changesRoot
            if let changesRoot { changes.refresh(root: changesRoot) }
            updateFolderWatch()
        }
        statusPanel.subject = currentRow.map { row in
            // 上下文只取绑定的对话就是这款工具时的用量（与标题栏同一份，`statusInfos`）。
            let usage = info?.tool == row.toolKey ? info?.usage : nil
            // 经额度桥读上下文的工具，桥没接上时写明原因（docs/design.md 5.12 节「上下文」）。
            let bridge = hookStatuses.first { $0.tool == row.toolKey }?.bridge
            return StatusPanel.Subject(tool: row.toolKey, path: row.currentDir ?? "", branch: row.branch,
                                       contextUsed: usage?.contextUsed, contextLimit: usage?.contextLimit,
                                       contextSupported: info?.contextSupported ?? false,
                                       contextNote: usage?.contextLimit == nil ? row.toolKey.flatMap { StatusPanel.bridgeNote(bridge: bridge, tool: $0) } : nil,
                                       changes: changesRoot.map { StatusPanel.Changes(changes.snapshot($0)) })
        }
    }

    /// 重读历史提示词：`key` 为当前终端这一行的（对话、最新提示词），没变就不读；nil 表示目标可能变了，总是读。
    private func refreshPromptHistory(key: String?) {
        guard rightPanel == .history else { return }
        let target = selectedConversation ?? currentRowConversation
        // 栏头写明读的是哪段对话：工具与当前会话或已结束（会话 ID 由面板从应答里取）。
        let row = target.flatMap { sidebar.row(forConversation: $0) }
        promptPanel.subjectTool = row?.toolKey
        promptPanel.subjectEnded = row?.state == "Stopped"
        let full = "\(target ?? -1)|\(key ?? "")"
        if key != nil, full == promptKey { return }
        promptKey = full
        guard let target else {
            let fg = currentRowForeground
            return promptPanel.showMessage(PromptHistoryPanel.noConversationMessage(tool: fg.tool, unsynced: fg.unsynced, command: fg.command))
        }
        loadPrompts(target)
        // 最新提示词变了：CLI 可能在 Hook 之后才写会话记录（Copilot 晚约 1 ms，2026-10-30 实测），过一会儿补读一次；
        // 内容没变时面板不重排。期间目标或提示词又变了就不补。
        if key != nil {
            DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { [weak self] in
                guard let self, self.rightPanel == .history, self.promptKey == full else { return }
                self.loadPrompts(target)
            }
        }
    }

    /// 一轮结束（运行中 → Ready / Error）时补读一次：到这时各款 CLI 都已把本轮提示词写进会话记录，比按时间补读可靠；
    /// 每轮只多读一次，不随「需确认」等中间状态反复读。侧栏选中了别的对话时不读。
    private func reloadPromptsAtTurnEnd(_ row: TaskRow?) {
        let running = row?.state == "Running"
        let ended = promptRowRunning.running && promptRowRunning.conversation == row?.conversationID
            && (row?.state == "Ready" || row?.state == "Error")
        if row?.state != "Waiting" { promptRowRunning = (row?.conversationID, running) }
        guard ended, rightPanel == .history, selectedConversation == nil, let target = row?.conversationID else { return }
        scyllauiLog.notice("prompt history reload at turn end conversation=\(target)")
        loadPrompts(target)
    }

    private func loadPrompts(_ target: Int64) {
        core.prompts(conversationID: target) { [weak self] reply in
            DispatchQueue.main.async {
                guard let self, self.rightPanel == .history, (self.selectedConversation ?? self.currentRowConversation) == target else { return }
                if let reply, reply["ok"] as? Bool == true {
                    self.promptPanel.show(reply: reply)
                } else {
                    self.promptPanel.showMessage(String(localized: "暂时读不到这段对话的提示词"))
                }
            }
        }
    }

    // MARK: 菜单动作（ScyllaUIAppDelegate 的「终端」菜单）

    @objc func newShell(_ sender: Any?) { newTerminal(directory: nil) }

    /// 空态的「新建终端」：在主目录新建。
    @objc private func newHomeShell(_ sender: Any?) { newTerminal(directory: NSHomeDirectory()) }

    @objc func closeCurrent(_ sender: Any?) {
        // 最前面的是别的窗口（如设置窗口，M2 第 5 块）时，Cmd+W 关那个窗口，而不是当前终端。
        if let key = NSApp.keyWindow, key !== window {
            key.performClose(sender)
            return
        }
        if let id = currentID { close(id) }
    }

    @objc func renameCurrent(_ sender: Any?) {
        if let id = currentID { sidebar.beginRename(id) }
    }

    /// 「记录终端尺寸」：把每个终端此刻的行列与回滚写进尺寸日志，用户看到异常时点一下，便于按时间定位。
    @objc func dumpGeometry(_ sender: Any?) {
        GeometryLog.shared?.write("mark", "terminals=\(sessions.count)")
        for id in sidebar.terminalOrder {
            sessions[id]?.geometry.dump(current: id == currentID)
        }
    }

    /// Cmd+Shift+[ / ]：按侧栏顺序切到上一个 / 下一个终端（循环），补足 Cmd+1…9 只到第 9 个。
    @objc func selectAdjacent(_ sender: NSMenuItem) {
        let order = sidebar.terminalOrder
        guard !order.isEmpty else { return }
        let i = currentID.flatMap { order.firstIndex(of: $0) } ?? 0
        show(order[(i + sender.tag + order.count) % order.count])
    }

    /// Cmd+1…9：按侧栏「当前任务」的显示顺序切换。
    @objc func selectNumbered(_ sender: NSMenuItem) {
        let order = sidebar.terminalOrder
        let n = sender.tag
        if n >= 1 && n <= order.count { show(order[n - 1]) }
    }

    // MARK: 测试夹具

    /// 测试数据目录下（`SCYLLAUI_DATA_DIR`）登记没有界面的夹具终端，供侧栏的多项目、多条目验证
    /// （src/scripts/sidebar-fixture.py 生成）。真实数据目录下不生效。
    private func registerFixtureTerminals() {
        guard core.isolated, let path = ProcessInfo.processInfo.environment["SCYLLAUI_FIXTURE_TERMINALS"],
              let data = FileManager.default.contents(atPath: path),
              let list = (try? JSONSerialization.jsonObject(with: data)) as? [[String: Any]] else { return }
        for t in list {
            guard let id = t["terminal_id"] as? String, let dir = t["project_dir"] as? String else { continue }
            core.registerTerminal(id: id, projectDir: dir, cwd: t["cwd"] as? String ?? dir)
        }
        scyllauiLog.notice("fixture terminals=\(list.count) file=\(path, privacy: .public)")
    }
}

/// 侧栏与右侧之间的分隔视图：双击分隔线把侧栏恢复为默认宽度（docs/design.md 第 5 节「宽度与连线」）；
/// 悬停或拖动时在分隔线上显示蓝线（DividerHighlight，2026-11-06）。
final class SidebarSplitView: NSSplitView {
    var onDoubleClickDivider: (() -> Void)?
    private let highlight = DividerHighlight()
    private var hoverArea: NSTrackingArea?

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        // 蓝线是浮在上面的普通子视图，不是分栏：只有 addArrangedSubview 加进来的才分栏。
        arrangesAllSubviews = false
        addSubview(highlight)
    }

    required init?(coder: NSCoder) { fatalError() }

    /// 分隔线的拖动热区：细分隔线只有 1 pt，按 AppKit 的拖动热区放宽到左右各 3 pt；侧栏收起时（只剩右侧一个分栏）没有。
    private var dividerZone: NSRect? {
        guard arrangedSubviews.count == 2, let first = arrangedSubviews.first else { return nil }
        return NSRect(x: first.frame.maxX - 3, y: 0, width: dividerThickness + 6, height: bounds.height)
    }

    override func layout() {
        super.layout()
        placeHighlight()
    }

    override func resizeSubviews(withOldSize oldSize: NSSize) {
        super.resizeSubviews(withOldSize: oldSize)
        placeHighlight()
    }

    private func placeHighlight() {
        guard let first = arrangedSubviews.first, dividerZone != nil else {
            highlight.isHidden = true
            return
        }
        highlight.isHidden = false
        let x = first.frame.maxX + dividerThickness / 2 - DividerHighlight.width / 2
        highlight.frame = NSRect(x: x, y: 0, width: DividerHighlight.width, height: bounds.height)
        updateTrackingAreas()
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let a = hoverArea { removeTrackingArea(a) }
        hoverArea = nil
        guard let zone = dividerZone else { return }
        let a = NSTrackingArea(rect: zone, options: [.mouseEnteredAndExited, .activeInKeyWindow], owner: self)
        addTrackingArea(a)
        hoverArea = a
    }

    /// 只认鼠标真的在热区里：窗口切到前台等时候 AppKit 会按过时的位置补发进入事件（2026-11-06 测试版实测，鼠标在 x≈893 时侧栏分隔线亮起）。
    private var mouseInZone: Bool {
        guard let w = window, let zone = dividerZone else { return false }
        return zone.contains(convert(w.mouseLocationOutsideOfEventStream, from: nil))
    }

    override func mouseEntered(with event: NSEvent) {
        highlight.setHovering(mouseInZone)
    }

    override func mouseExited(with event: NSEvent) { highlight.setHovering(false) }

    override func mouseDown(with event: NSEvent) {
        let p = convert(event.locationInWindow, from: nil)
        guard let zone = dividerZone, zone.contains(p) else {
            super.mouseDown(with: event)
            return
        }
        if event.clickCount == 2 {
            onDoubleClickDivider?()
            return
        }
        // NSSplitView 在 mouseDown 里自己跑拖动循环，返回即松手；期间收不到进出事件，按松手时的位置补一次。
        highlight.setDragging(true)
        super.mouseDown(with: event)
        highlight.setHovering(mouseInZone)
        highlight.setDragging(false)
    }
}

/// 悬停文字的提供者（`addToolTip` 的 owner，2026-11-02）：文字可以随时改，悬停区域不必重挂。状态行每 2 秒刷新一次，原来每次都
/// 重挂（`removeAllToolTips` 后再挂、给 `toolTip` 赋新值），正在显示的提示就消失一下再出现，即用户看到的规律性闪烁。
final class TooltipText: NSObject, NSViewToolTipOwner {
    var text: String

    init(_ text: String) {
        self.text = text
    }

    func view(_ view: NSView, stringForToolTip tag: NSView.ToolTipTag, point: NSPoint, userData data: UnsafeMutableRawPointer?) -> String {
        text
    }
}

/// 标题栏附件的容器：子视图的垂直中心对齐到红黄绿按钮的中线（docs/design.md 第 5 节「垂直对齐」）。
/// AppKit 决定附件视图的高度与位置，所以在尺寸变化、进入窗口时按关闭按钮的位置重新对齐，不写死纵向坐标。
final class TitlebarAlignedView: NSView {
    func align() {
        guard let close = window?.standardWindowButton(.closeButton), let parent = close.superview else { return }
        let mid = convert(NSPoint(x: 0, y: close.frame.midY), from: parent).y
        for v in subviews { v.frame.origin.y = (mid - v.frame.height / 2).rounded() }
    }

    override func resizeSubviews(withOldSize oldSize: NSSize) {
        super.resizeSubviews(withOldSize: oldSize)
        align()
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        align()
    }
}
