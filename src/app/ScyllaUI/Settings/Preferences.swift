// App 自己的显示偏好（M4a，docs/design.md 10.4 节决策 3、5、6、9）：存在 App 的 UserDefaults 里。读可在任意线程
// （通知在 CoreClient 的读线程里按开关决定是否提交）；写在主线程，写完发 `Prefs.changed`，工作区据此刷新。

import Foundation

enum Prefs {
    static let changed = Notification.Name("ScyllaUIPrefsChanged")
    private static var d: UserDefaults { .standard }
    private enum Key {
        static let statusHidden = "ScyllaUIStatusHidden"
        static let notifyWaiting = "ScyllaUINotifyWaiting"
        static let notifyDone = "ScyllaUINotifyDone"
        static let dockBadge = "ScyllaUIDockBadge"
        static let hooksRemoved = "ScyllaUIHooksRemoved"
        static let restoreTerminals = "ScyllaUIRestoreTerminals"
        static let restoreAttempts = "ScyllaUIRestoreAttempts"
        static let lastTerminal = "ScyllaUILastTerminal"
        static let promptPanelWidth = "ScyllaUIPromptPanelWidth2"
        static let legacyPromptPanelWidth = "ScyllaUIPromptPanelWidth"
        static let rightViewerWidth = "ScyllaUIRightViewerWidth"
        static let filesTreeWidth = "ScyllaUIFilesTreeWidth"
        static let aliveAt = "ScyllaUIAliveAt"
    }

    /// 状态栏里关掉的段；默认全开。
    static var statusHidden: Set<StatusSegment> {
        get { Set((d.stringArray(forKey: Key.statusHidden) ?? []).compactMap(StatusSegment.init(rawValue:))) }
        set { d.set(newValue.map(\.rawValue).sorted(), forKey: Key.statusHidden); post() }
    }

    /// 「需确认时通知」，默认开。
    static var notifyWaiting: Bool {
        get { d.object(forKey: Key.notifyWaiting) as? Bool ?? true }
        set { d.set(newValue, forKey: Key.notifyWaiting); post() }
    }

    /// 「完成一轮时通知」，默认关（M4a 新功能，保持原有行为）。
    static var notifyDone: Bool {
        get { d.object(forKey: Key.notifyDone) as? Bool ?? false }
        set { d.set(newValue, forKey: Key.notifyDone); post() }
    }

    /// 「Dock 角标显示需确认的终端数」，默认开。
    static var dockBadge: Bool {
        get { d.object(forKey: Key.dockBadge) as? Bool ?? true }
        set { d.set(newValue, forKey: Key.dockBadge); post() }
    }

    /// 用户在设置里撤销过状态同步的工具（`claude` 等）：侧栏提示栏不再为它们提示，状态回到正常即移除。
    static var hooksRemoved: Set<String> {
        get { Set(d.stringArray(forKey: Key.hooksRemoved) ?? []) }
        set { d.set(newValue.sorted(), forKey: Key.hooksRemoved); post() }
    }

    /// 「启动时恢复上次的终端」，默认开（M4a 追加块 10，docs/design.md 10.5 节决策 12）。
    static var restoreTerminals: Bool {
        get { d.object(forKey: Key.restoreTerminals) as? Bool ?? true }
        set { d.set(newValue, forKey: Key.restoreTerminals); post() }
    }

    /// 已开始、还没做完的自动恢复次数：恢复做完清零；连续两次中途退出（App 崩溃等）即暂停自动恢复。
    static var restoreAttempts: Int {
        get { d.integer(forKey: Key.restoreAttempts) }
        set { d.set(newValue, forKey: Key.restoreAttempts) }
    }

    /// 最后显示的终端：后台被结束或重启电脑后，恢复完把焦点交给它的替身（后台的 `last_focus` 只在内存里，随后台丢失）。
    static var lastTerminal: String? {
        get { d.string(forKey: Key.lastTerminal) }
        set { d.set(newValue, forKey: Key.lastTerminal) }
    }

    /// 历史提示词栏的宽度，0 为默认（docs/design.md 10.5 节决策 20）。0.4.58 起换了键：0.4.56 及以前铺满窗口时按旧比例
    /// 自动算出的宽度（如 335）也存在旧键里，升级后被当成用户拖过的宽度，栏不按新的默认宽度打开（2026-11-22 用户实测），
    /// 所以旧键不再读、读到即删。
    static var promptPanelWidth: Double {
        get {
            d.removeObject(forKey: Key.legacyPromptPanelWidth)
            return d.double(forKey: Key.promptPanelWidth)
        }
        set { d.set(newValue, forKey: Key.promptPanelWidth) }
    }

    /// 右侧栏查看文件时的宽度，0 为默认（docs/design.md 5.12 节「宽度」）。
    static var rightViewerWidth: Double {
        get { d.double(forKey: Key.rightViewerWidth) }
        set { d.set(newValue, forKey: Key.rightViewerWidth) }
    }

    /// 「文件」打开文件后树栏的宽度，0 为默认（docs/design.md 5.12 节「宽度」，F3）。
    static var filesTreeWidth: Double {
        get { d.double(forKey: Key.filesTreeWidth) }
        set { d.set(newValue, forKey: Key.filesTreeWidth) }
    }

    /// 运行中的心跳（M4b 块 7）：启动时写入并每 60 秒更新，正常退出时删除；启动时还在即上次意外退出。
    static var aliveAt: Date? {
        get { d.object(forKey: Key.aliveAt) as? Date }
        set { d.set(newValue, forKey: Key.aliveAt) }
    }

    private static func post() {
        NotificationCenter.default.post(name: changed, object: nil)
    }
}

extension Prefs {
    private enum WelcomeKey {
        static let shownAt = "ScyllaUISplashShownAt"
        static let version = "ScyllaUISplashVersion"
        static let guideSeen = "ScyllaUIGuideSeen"
        static let feedbackAt = "ScyllaUIFeedbackShownAt"
        // 0.4.36 及之前的许可协议同意记录（ScyllaUIEulaAccepted、ScyllaUIEulaAcceptedAt）开源后不再使用，旧值留着无害。
        static let noticeSeen = "ScyllaUINoticeSeen"
        static let noticeSeenAt = "ScyllaUINoticeSeenAt"
    }

    static var splashShownAt: Date? { UserDefaults.standard.object(forKey: WelcomeKey.shownAt) as? Date }
    static var splashVersion: String? { UserDefaults.standard.string(forKey: WelcomeKey.version) }
    static var guideSeen: String? { UserDefaults.standard.string(forKey: WelcomeKey.guideSeen) }
    static var feedbackShownAt: Date? { UserDefaults.standard.object(forKey: WelcomeKey.feedbackAt) as? Date }
    static var noticeSeen: String? { UserDefaults.standard.string(forKey: WelcomeKey.noticeSeen) }

    // 下面三项「看过」的记录只在用户点了那一页的按钮时写（WelcomePolicy.mark），关窗不写。

    /// 用户在使用须知页点了「继续」：记下须知的版本与时间。
    static func recordNotice(version: String, at date: Date) {
        UserDefaults.standard.set(version, forKey: WelcomeKey.noticeSeen)
        UserDefaults.standard.set(date, forKey: WelcomeKey.noticeSeenAt)
    }

    /// 用户在引导最后一页点了按钮：记下看过引导的「主.次」。
    static func recordGuide(version: String) {
        UserDefaults.standard.set(WelcomePolicy.majorMinor(version), forKey: WelcomeKey.guideSeen)
    }

    /// 用户在「遇到问题」页点了「完成」：记下时间。
    static func recordFeedback(at date: Date) {
        UserDefaults.standard.set(date, forKey: WelcomeKey.feedbackAt)
    }

    /// 欢迎窗口关闭时记开屏的时间与版本号（只是记录，不影响哪些页出现）。
    static func recordSplash(at date: Date, version: String) {
        UserDefaults.standard.set(date, forKey: WelcomeKey.shownAt)
        UserDefaults.standard.set(version, forKey: WelcomeKey.version)
    }
}
