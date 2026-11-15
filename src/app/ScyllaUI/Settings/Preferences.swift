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
        static let promptPanelWidth = "ScyllaUIPromptPanelWidth"
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

    /// 提示词足迹栏的宽度，0 为默认（docs/design.md 10.5 节决策 20）。
    static var promptPanelWidth: Double {
        get { d.double(forKey: Key.promptPanelWidth) }
        set { d.set(newValue, forKey: Key.promptPanelWidth) }
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

    /// 显示使用须知时记下它的版本与时间。
    static func recordNotice(version: String, at date: Date) {
        UserDefaults.standard.set(version, forKey: WelcomeKey.noticeSeen)
        UserDefaults.standard.set(date, forKey: WelcomeKey.noticeSeenAt)
    }

    /// 关闭欢迎窗口（任何方式）即记为看过：开屏记时间与版本号，带引导时另记「主.次」，安排了「遇到问题」一页时另记时间。
    static func recordWelcome(at date: Date, version: String, guide: Bool, feedback: Bool) {
        let d = UserDefaults.standard
        d.set(date, forKey: WelcomeKey.shownAt)
        d.set(version, forKey: WelcomeKey.version)
        if guide { d.set(WelcomePolicy.majorMinor(version), forKey: WelcomeKey.guideSeen) }
        if feedback { d.set(date, forKey: WelcomeKey.feedbackAt) }
    }
}
