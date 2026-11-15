// 设置窗口（M2 第 5 块起；M4a 起改为分页，docs/design.md 10.4 节决策 1）：`NSTabViewController` 的工具栏样式，
// 即系统设置窗口的标准样式（顶部图标 + 标签，标题随分页）。七个分页共用一个尺寸（10.5 节决策 16，取代窗口高度随分页）。
// 各分页见 Settings/。
// 窗口外观跟随主窗口（由生效的 Ghostty 配置决定，与终端主题的深浅一致）。

import AppKit

final class SettingsWindowController: NSObject, NSWindowDelegate {
    private let window: NSWindow
    private let tabs = SettingsTabController()
    private let runtime: GhosttyRuntime

    /// `onFontSizeChange`：设置里改了字号、新配置已应用之后调用（外观分页）。
    init(runtime: GhosttyRuntime, core: CoreClient, onFontSizeChange: @escaping () -> Void) {
        self.runtime = runtime
        tabs.tabStyle = .toolbar
        tabs.transitionOptions = []
        let panes: [(NSViewController, String)] = [
            (AppearancePane(runtime: runtime, onFontSizeChange: onFontSizeChange), "paintpalette"),
            (StatusBarPane(nibName: nil, bundle: nil), "menubar.rectangle"),
            (LanguagePane(nibName: nil, bundle: nil), "globe"),
            (NotificationsPane(nibName: nil, bundle: nil), "bell.badge"),
            (AgentPane(core: core), "terminal"),
            (DataPane(core: core), "internaldrive"),
            (AboutPane(core: core), "info.circle"),
        ]
        for (vc, symbol) in panes {
            let item = NSTabViewItem(viewController: vc)
            item.label = vc.title ?? ""
            item.image = NSImage(systemSymbolName: symbol, accessibilityDescription: vc.title)
            tabs.addTabViewItem(item)
        }
        window = NSWindow(contentViewController: tabs)
        window.styleMask = [.titled, .closable]
        window.toolbarStyle = .preference
        window.isReleasedWhenClosed = false
        super.init()
        window.delegate = self
        applyAppearance()
        NotificationCenter.default.addObserver(forName: GhosttyRuntime.configDidChange, object: runtime, queue: .main) { [weak self] _ in
            self?.applyAppearance()
        }
        tabs.fitWindow()
        window.center()
    }

    /// 每次打开都按各分页此刻的内容重算尺寸（如 Agent 页多了提示行）。
    func show() {
        tabs.fitWindow()
        window.makeKeyAndOrderFront(nil)
    }

    private func applyAppearance() {
        window.appearance = runtime.windowAppearance
    }
}

/// 七个分页共用一个尺寸（docs/design.md 10.5 节决策 16）：取各页所需尺寸的最大值，切换分页时窗口不变，标题为分页名。
final class SettingsTabController: NSTabViewController {
    override func tabView(_ tabView: NSTabView, didSelect item: NSTabViewItem?) {
        super.tabView(tabView, didSelect: item)
        view.window?.title = item?.label ?? ""
    }

    /// 按各分页所需尺寸的最大值定窗口大小，保持左上角不动。分页内容靠上、水平居中（`AppearancePane.padded`）。
    func fitWindow() {
        guard let window = view.window else { return }
        if selectedTabViewItemIndex >= 0 { window.title = tabViewItems[selectedTabViewItemIndex].label }
        var size = NSSize.zero
        for item in tabViewItems {
            guard let content = item.viewController?.view else { continue }
            content.layoutSubtreeIfNeeded()
            let s = content.fittingSize
            size = NSSize(width: max(size.width, s.width), height: max(size.height, s.height))
        }
        // 子页的首选尺寸都设成同一个，NSTabViewController 切换时也不会改窗口大小。
        for item in tabViewItems { item.viewController?.preferredContentSize = size }
        let old = window.frame
        var frame = window.frameRect(forContentRect: NSRect(origin: .zero, size: size))
        frame.origin = NSPoint(x: old.minX, y: old.maxY - frame.height)
        window.setFrame(frame, display: true)
    }
}
