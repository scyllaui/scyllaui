// 设置「通知」（M4a，docs/design.md 10.4 节决策 5）：需确认时通知、完成一轮时通知、Dock 角标三个开关，即时生效；
// 显示系统通知授权状态，被拒时提供「打开系统通知设置…」（只打开系统设置页面，不改设置）。

import AppKit
import UserNotifications

final class NotificationsPane: NSViewController {
    private let waiting = NSButton(checkboxWithTitle: String(localized: "需确认时发通知"), target: nil, action: nil)
    private let done = NSButton(checkboxWithTitle: String(localized: "完成一轮时发通知（您没在看那个终端时）"), target: nil, action: nil)
    private let badge = NSButton(checkboxWithTitle: String(localized: "Dock 角标显示需确认的终端数"), target: nil, action: nil)
    private let auth = NSTextField(wrappingLabelWithString: "")
    private let openSettings = NSButton(title: String(localized: "打开系统通知设置…"), target: nil, action: nil)
    /// 「打开系统通知设置…」所在行；隐藏整行，免得空出按钮的高度。
    private var openSettingsRow: NSGridRow?

    override init(nibName: NSNib.Name?, bundle: Bundle?) {
        super.init(nibName: nil, bundle: nil)
        title = String(localized: "通知")
    }

    required init?(coder: NSCoder) { fatalError() }

    override func loadView() {
        for b in [waiting, done, badge] {
            b.target = self
            b.action = #selector(toggled(_:))
        }
        auth.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        auth.textColor = .secondaryLabelColor
        auth.preferredMaxLayoutWidth = 380
        openSettings.bezelStyle = .rounded
        openSettings.target = self
        openSettings.action = #selector(openSystemSettings)
        let note = NSTextField(wrappingLabelWithString: String(localized: "通知只写项目与任务名，不含命令内容。点通知会把 ScyllaUI 调到前台。"))
        note.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        note.textColor = .secondaryLabelColor
        note.preferredMaxLayoutWidth = 380
        let grid = NSGridView(views: [
            [AppearancePane.label(String(localized: "系统通知：")), waiting],
            [NSGridCell.emptyContentView, done],
            [NSGridCell.emptyContentView, note],
            [AppearancePane.label(String(localized: "Dock：")), badge],
            [NSGridCell.emptyContentView, auth],
            [NSGridCell.emptyContentView, openSettings],
        ])
        grid.column(at: 0).xPlacement = .trailing
        grid.rowAlignment = .firstBaseline
        grid.rowSpacing = 8
        grid.columnSpacing = 8
        grid.row(at: 3).topPadding = 10
        grid.row(at: 4).topPadding = 10
        openSettingsRow = grid.row(at: 5)
        openSettingsRow?.isHidden = true
        view = AppearancePane.padded(grid)
        waiting.state = Prefs.notifyWaiting ? .on : .off
        done.state = Prefs.notifyDone ? .on : .off
        badge.state = Prefs.dockBadge ? .on : .off
    }

    override func viewWillAppear() {
        super.viewWillAppear()
        UNUserNotificationCenter.current().getNotificationSettings { settings in
            let status = settings.authorizationStatus
            DispatchQueue.main.async {
                switch status {
                case .denied:
                    self.auth.stringValue = String(localized: "系统设置中关闭了 ScyllaUI 的通知，上面两个通知开关不会生效。")
                case .notDetermined:
                    self.auth.stringValue = String(localized: "还没有向系统申请通知授权。")
                default:
                    self.auth.stringValue = String(localized: "系统已允许 ScyllaUI 发送通知。")
                }
                let hide = status != .denied && status != .notDetermined
                if self.openSettingsRow?.isHidden != hide {
                    self.openSettingsRow?.isHidden = hide
                    (self.parent as? SettingsTabController)?.fitWindow()
                }
            }
        }
    }

    @objc private func toggled(_ sender: NSButton) {
        let on = sender.state == .on
        if sender === waiting { Prefs.notifyWaiting = on }
        if sender === done { Prefs.notifyDone = on }
        if sender === badge { Prefs.dockBadge = on }
        scyllauiLog.notice("notify prefs waiting=\(Prefs.notifyWaiting) done=\(Prefs.notifyDone) badge=\(Prefs.dockBadge)")
    }

    @objc private func openSystemSettings() {
        let id = Bundle.main.bundleIdentifier ?? ""
        if let url = URL(string: "x-apple.systempreferences:com.apple.Notifications-Settings.extension?id=\(id)") {
            NSWorkspace.shared.open(url)
        }
    }
}
