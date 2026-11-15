// 系统通知与 Dock 角标（M2 第 1 块，docs/design.md 4.3 节「实现（M2 第 1 块）」）。发不发、何时撤回由 scyllauid 决定
// （`notify.post` / `notify.withdraw`，docs/protocol.md 第 5、7 节），这里只负责显示：通知在 CoreClient 的读线程里直接
// 提交，不经主线程（窗口不可见时主线程被 App Nap 节流）；角标为侧栏上「需确认」的终端数，在主线程设置。

import AppKit
import UserNotifications

final class Notifier: NSObject, UNUserNotificationCenterDelegate {
    private let center = UNUserNotificationCenter.current()
    /// 用户点了某条通知：在主线程回调其终端 ID。
    var onOpen: ((String) -> Void)?
    /// 侧栏上「需确认」的终端数（始终维护）与 Dock 上此刻显示的数字（设置里关掉角标时为 0，M4a）。
    private var badge = 0
    private var shownBadge: Int?

    override init() {
        super.init()
        center.delegate = self
    }

    /// 启动时请求一次提醒与角标授权（macOS 12 起角标也要先取得授权）。拒绝时不报错，侧栏照常。
    func requestAuthorization() {
        center.requestAuthorization(options: [.alert, .badge, .sound]) { granted, error in
            scyllauiLog.notice("notify_auth granted=\(granted) error=\(error?.localizedDescription ?? "-", privacy: .public)")
        }
    }

    /// `notify.post`：读线程调用。`received` 为读到该行时的墙钟微秒。`kind`（M4a）：`waiting`「需确认」（旧后台不带）或
    /// `done`「完成一轮」，按设置里的开关决定是否提交（docs/design.md 10.4 节决策 5）。
    func post(_ msg: [String: Any], received: UInt64) {
        guard let id = msg["id"] as? String, let terminal = msg["terminal_id"] as? String else { return }
        let done = msg["kind"] as? String == "done"
        guard done ? Prefs.notifyDone : Prefs.notifyWaiting else {
            scyllauiLog.notice("notify_skip id=\(id, privacy: .public) kind=\(done ? "done" : "waiting", privacy: .public)")
            return
        }
        let tool = SidebarModel.toolName(msg["tool"] as? String)
        let content = UNMutableNotificationContent()
        if done {
            content.title = msg["state"] as? String == "error" ? String(localized: "\(tool) 出错") : String(localized: "\(tool) 已完成")
        } else {
            content.title = String(localized: "\(tool) 需确认")
        }
        // 不含待审批的命令内容：锁屏时通知可能被他人看到。
        content.body = [msg["project"] as? String, msg["title"] as? String]
            .compactMap { $0?.isEmpty == false ? $0 : nil }
            .joined(separator: " · ")
        content.sound = .default
        content.threadIdentifier = terminal
        content.userInfo = ["terminal_id": terminal]
        let trace = msg["trace"] as? [String: Any]
        let t0 = (trace?["hook_t0_us"] as? NSNumber)?.uint64Value
        let daemonSend = (trace?["daemon_send_us"] as? NSNumber)?.uint64Value
        center.add(UNNotificationRequest(identifier: id, content: content, trigger: nil)) { error in
            let done = CoreClient.nowUs()
            let total = t0.map { done >= $0 ? done - $0 : 0 } ?? 0
            let toApp = daemonSend.map { received >= $0 ? received - $0 : 0 } ?? 0
            scyllauiLog.notice("notify_latency id=\(id, privacy: .public) tool=\(tool, privacy: .public) total_us=\(total) daemon_to_app_us=\(toApp) app_us=\(done - received) error=\(error?.localizedDescription ?? "-", privacy: .public)")
        }
    }

    /// `notify.withdraw`：从通知中心移除。
    func withdraw(_ ids: [String]) {
        center.removeDeliveredNotifications(withIdentifiers: ids)
        center.removePendingNotificationRequests(withIdentifiers: ids)
        scyllauiLog.notice("notify_withdraw ids=\(ids.joined(separator: ","), privacy: .public)")
    }

    /// 连上后台（收到快照）时清空：之前的等待记录在后台已不存在，留下的通知不会再被撤回。
    func clearAll() {
        center.removeAllDeliveredNotifications()
        center.removeAllPendingNotificationRequests()
    }

    /// 主线程调用。
    func setBadge(_ n: Int) {
        badge = n
        applyBadge()
    }

    /// 按设置里的角标开关显示（主线程）：关掉时清掉角标，计数照常维护，打开即恢复。
    func applyBadge() {
        let n = Prefs.dockBadge ? badge : 0
        guard n != shownBadge else { return }
        shownBadge = n
        NSApp.dockTile.badgeLabel = n > 0 ? "\(n)" : nil
        scyllauiLog.notice("dock_badge count=\(n) pending=\(self.badge)")
    }

    // MARK: UNUserNotificationCenterDelegate

    /// ScyllaUI 在前台（用户看着别的终端）时也显示横幅，不出声。
    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification,
                                withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void) {
        completionHandler([.banner, .list])
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
                                withCompletionHandler completionHandler: @escaping () -> Void) {
        if response.actionIdentifier == UNNotificationDefaultActionIdentifier,
           let id = response.notification.request.content.userInfo["terminal_id"] as? String {
            scyllauiLog.notice("notify_open terminal=\(id, privacy: .public)")
            DispatchQueue.main.async { self.onOpen?(id) }
        }
        completionHandler()
    }
}
