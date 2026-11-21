// 正式 ScyllaUI 的入口（M1 第 4 块起）：创建工作区（多终端 + 侧栏）与菜单。

import AppKit

final class AppDelegate: NSObject, NSApplicationDelegate {
    private var runtime: GhosttyRuntime?
    private var workspace: WorkspaceController?

    func applicationDidFinishLaunching(_ notification: Notification) {
        // 被系统挪位或在 DMG 里直接运行（M4b 块 2）：先于连接后台提示，否则后台从临时路径启动、授权也记不住。
        if InstallLocation.needsMove(bundlePath: Bundle.main.bundlePath) {
            scyllauiLog.notice("install_location needs_move path=\(Bundle.main.bundlePath, privacy: .public)")
            let alert = NSAlert()
            alert.useWideLayout()
            alert.messageText = String(localized: "请先把 ScyllaUI 拖到「应用程序」")
            alert.informativeText = String(localized: "ScyllaUI 现在是从下载的磁盘映像或临时位置打开的。这样运行时，终端后台会从临时位置启动，系统也记不住你给的授权。请退出后，把 ScyllaUI 拖到「应用程序」文件夹，再从那里打开。")
            alert.addButton(withTitle: String(localized: "退出"))
            alert.addButton(withTitle: String(localized: "仍然打开"))
            NSApp.activate(ignoringOtherApps: true)
            if alert.runModal() == .alertFirstButtonReturn {
                NSApp.terminate(nil)
                return
            }
        }
        guard let runtime = GhosttyRuntime(), let app = runtime.app else {
            let alert = NSAlert()
            alert.messageText = String(localized: "终端引擎初始化失败")
            alert.runModal()
            NSApp.terminate(nil)
            return
        }
        self.runtime = runtime
        // 没有标签页，不要系统自动加到「窗口」菜单的标签页项；窗口菜单要在建窗口之前设好，主窗口才进窗口列表。
        NSWindow.allowsAutomaticWindowTabbing = false
        NSApp.windowsMenu = windowMenu
        let workspace = WorkspaceController(runtime: runtime, app: app)
        self.workspace = workspace
        // 自动更新（M4b 块 3）：在 DMG 或临时位置运行时不启动，那里替换不了。
        if !InstallLocation.needsMove(bundlePath: Bundle.main.bundlePath) { AppUpdater.shared.start() }
        buildMenu(workspace)
        NSApp.activate(ignoringOtherApps: true)
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        true
    }

    /// 正常退出（菜单、关最后一个窗口、关机与注销）删掉心跳；下次启动时还在即上次意外退出（M4b 块 7，UnexpectedExit）。
    func applicationWillTerminate(_ notification: Notification) {
        Prefs.aliveAt = nil
    }

    /// 管理操作全部用 Cmd 键（docs/design.md 第 5 节）；终端获得焦点时，被 libghostty 绑定的 Cmd+N / T / W / 1…9
    /// 先走这里的菜单项（TerminalSurfaceView.performKeyEquivalent，与 Ghostty.app 相同）。Cmd+O 没有被 libghostty
    /// 默认绑定（固定 commit 的 Config.zig 只有 Ctrl+Shift+O），直接走菜单。
    private func buildMenu(_ workspace: WorkspaceController) {
        let main = NSMenu()
        let appItem = NSMenuItem()
        let appMenu = NSMenu()
        AppMenu.aboutItems(target: workspace, about: #selector(WorkspaceController.showAbout(_:)),
                           logs: #selector(WorkspaceController.openLogFolder(_:))).forEach(appMenu.addItem)
        if let update = AppUpdater.shared.menuItem() { appMenu.addItem(update) }
        appMenu.addItem(.separator())
        // Cmd+, 走菜单，优先于 libghostty 默认的 open_config 绑定（M2 第 5 块）。
        let settingsItem = NSMenuItem(title: String(localized: "设置…"), action: #selector(WorkspaceController.showSettings(_:)), keyEquivalent: ",")
        settingsItem.target = workspace
        appMenu.addItem(settingsItem)
        // 测试版另留一项可见的，报问题时按时间排查（docs/design.md 5.10 节）；正式版只有按住 Option 的替代项（2026-11-15 用户决定）。
        if WelcomePolicy.isTestBuild(bundleID: Bundle.main.bundleIdentifier) {
            let logsItem = NSMenuItem(title: String(localized: "打开日志文件夹"), action: #selector(WorkspaceController.openLogFolder(_:)), keyEquivalent: "")
            logsItem.target = workspace
            appMenu.addItem(logsItem)
        }
        appMenu.addItem(.separator())
        appMenu.addItem(withTitle: String(localized: "退出 ScyllaUI"), action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        appItem.submenu = appMenu
        main.addItem(appItem)

        let editItem = NSMenuItem()
        let editMenu = NSMenu(title: String(localized: "编辑"))
        editMenu.addItem(withTitle: String(localized: "拷贝"), action: #selector(TerminalSurfaceView.copy(_:)), keyEquivalent: "c")
        editMenu.addItem(withTitle: String(localized: "粘贴"), action: #selector(TerminalSurfaceView.paste(_:)), keyEquivalent: "v")
        editMenu.addItem(withTitle: String(localized: "全选"), action: #selector(NSResponder.selectAll(_:)), keyEquivalent: "a")
        editItem.submenu = editMenu
        main.addItem(editItem)

        let termItem = NSMenuItem()
        let term = NSMenu(title: String(localized: "终端"))
        func add(_ title: String, _ action: Selector, _ key: String, tag: Int = 0) {
            let item = NSMenuItem(title: title, action: action, keyEquivalent: key)
            item.target = workspace
            item.tag = tag
            term.addItem(item)
        }
        // 新建只有终端一种，AI 由用户在 Shell 里输入命令启动（docs/design.md 10.5 节决策 1、6）。
        add(String(localized: "新建终端"), #selector(WorkspaceController.newShell(_:)), "n")
        // Cmd+T：同一操作的备用键（照顾 Ghostty 习惯），不在菜单里显示。
        add(String(localized: "新建终端"), #selector(WorkspaceController.newShell(_:)), "t")
        term.items.last?.isHidden = true
        term.items.last?.allowsKeyEquivalentWhenHidden = true
        add(String(localized: "打开文件夹…"), #selector(WorkspaceController.openFolder(_:)), "o")
        term.addItem(.separator())
        add(String(localized: "重命名…"), #selector(WorkspaceController.renameCurrent(_:)), "")
        add(String(localized: "关闭终端"), #selector(WorkspaceController.closeCurrent(_:)), "w")
        term.addItem(.separator())
        // Cmd+\ 没有被 libghostty 默认绑定（固定 commit 的 Config.zig 中没有反斜杠键），直接走菜单。
        add(String(localized: "隐藏 / 显示侧栏"), #selector(WorkspaceController.toggleSidebarPane(_:)), "\\")
        // Cmd+F：只留快捷键，不在菜单里显示；libghostty 默认的 super+f（start_search）已由包内 scyllaui.conf 解除，按键回到菜单
        // （docs/design.md 10.5 节决策 25）。
        add(String(localized: "搜索"), #selector(WorkspaceController.focusSearch(_:)), "f")
        term.items.last?.isHidden = true
        term.items.last?.allowsKeyEquivalentWhenHidden = true
        // Cmd+Y：固定 commit 的 libghostty 没有默认绑定，直接走菜单（M4a 追加块 9）。
        add(String(localized: "历史提示词"), #selector(WorkspaceController.togglePromptHistory(_:)), "y")
        // Cmd+Shift+,：与 Ghostty 的默认绑定相同（M2 第 5 块）；只留快捷键，不在菜单里显示（docs/design.md 10.5 节决策 25）。
        add(String(localized: "重新载入配置"), #selector(WorkspaceController.reloadConfiguration(_:)), ",")
        term.items.last?.keyEquivalentModifierMask = [.command, .shift]
        term.items.last?.isHidden = true
        term.items.last?.allowsKeyEquivalentWhenHidden = true
        term.addItem(.separator())
        // Cmd+Shift+[ / ]：只留快捷键，不在菜单里显示（决策 25）。
        add(String(localized: "上一个终端"), #selector(WorkspaceController.selectAdjacent(_:)), "{", tag: -1)
        term.items.last?.isHidden = true
        term.items.last?.allowsKeyEquivalentWhenHidden = true
        add(String(localized: "下一个终端"), #selector(WorkspaceController.selectAdjacent(_:)), "}", tag: 1)
        term.items.last?.isHidden = true
        term.items.last?.allowsKeyEquivalentWhenHidden = true
        // Cmd+1…9 只留快捷键，不在菜单里显示（docs/design.md 10.5 节决策 24）。
        for n in 1...9 {
            add(String(localized: "切换到第 \(n) 个终端"), #selector(WorkspaceController.selectNumbered(_:)), "\(n)", tag: n)
            term.items.last?.isHidden = true
            term.items.last?.allowsKeyEquivalentWhenHidden = true
        }
        term.addItem(.separator())
        add(String(localized: "记录终端尺寸（排查用）"), #selector(WorkspaceController.dumpGeometry(_:)), "")
        termItem.submenu = term
        main.addItem(termItem)

        let windowItem = NSMenuItem()
        windowItem.submenu = windowMenu
        main.addItem(windowItem)

        // 「帮助」只有「报告问题…」（2026-11-02，docs/design.md 10.6 节「问题反馈」）；设为 helpMenu 后系统在里面加菜单搜索框。
        let helpMenu = NSMenu(title: String(localized: "帮助"))
        let reportItem = NSMenuItem(title: String(localized: "报告问题…"), action: #selector(WorkspaceController.reportProblem(_:)), keyEquivalent: "")
        reportItem.target = workspace
        helpMenu.addItem(reportItem)
        let helpItem = NSMenuItem()
        helpItem.submenu = helpMenu
        main.addItem(helpItem)
        NSApp.helpMenu = helpMenu
        NSApp.mainMenu = main
    }

    /// 「窗口」菜单只放系统的基本项（2026-10-29 长跑后用户要求）；设为 windowsMenu 后 AppKit 自动附上窗口列表，
    /// 全屏项的标题随状态切换。⌃⌘F 被 libghostty 默认绑到 toggle_fullscreen（ScyllaUI 不处理该动作），终端有焦点时
    /// 经 performKeyEquivalent 先走这里。
    private let windowMenu: NSMenu = {
        let menu = NSMenu(title: String(localized: "窗口"))
        menu.addItem(withTitle: String(localized: "最小化"), action: #selector(NSWindow.performMiniaturize(_:)), keyEquivalent: "m")
        menu.addItem(withTitle: String(localized: "缩放"), action: #selector(NSWindow.performZoom(_:)), keyEquivalent: "")
        let fullScreenItem = menu.addItem(withTitle: String(localized: "进入全屏幕"), action: #selector(NSWindow.toggleFullScreen(_:)), keyEquivalent: "f")
        fullScreenItem.keyEquivalentModifierMask = [.control, .command]
        menu.addItem(.separator())
        menu.addItem(withTitle: String(localized: "前置全部窗口"), action: #selector(NSApplication.arrangeInFront(_:)), keyEquivalent: "")
        return menu
    }()
}
