// 欢迎窗口何时出现（M4a 块 4，docs/design.md 5.9 节）。2026-11-15 用户决定：开屏每次启动都显示；「文稿」授权说明与
// Hook 两页引导只在首次启动与主或次版本号变化时接在开屏之后；最后一页「遇到问题，告诉我们」每天第一次打开时出现，不分新老用户；
// 使用须知没看过当前版本（新用户、须知改版）时显示一次。不再按「版本号低于 1.0.0」算试用期（主版本保持 0，对外发布也一样）。
// 「看过」的定义（2026-11-16 用户决定）：用户在那一页点了往下走的按钮才算；停在某一页没有操作、窗口被关掉都不算，下次还会出现。
// 判定是纯函数，可单测；记录存在 App 的 UserDefaults。

import Foundation

enum WelcomePolicy {
    /// 当前使用须知的版本（`Resources/Legal/Notice-*.txt` 的「版本」行）；改内容时升级，所有用户会再看到一次。
    static let noticeVersion = "2026-11-22"

    struct Decision: Equatable {
        var splash = false
        var guide = false
        /// 最后一页「遇到问题，告诉我们」。
        var feedback = false
        /// 开屏后先显示使用须知。
        var notice = false
    }

    /// `lastShown` / `lastVersion`：上次显示开屏的时间与当时的版本号；`guideSeen`：看过引导的「主.次」；
    /// `lastFeedback`：上次安排「遇到问题」一页的时间；`noticeSeen`：看过的使用须知版本。
    static func decide(now: Date, version: String, lastShown: Date?, lastVersion: String?, guideSeen: String?,
                       lastFeedback: Date?, noticeSeen: String?, calendar: Calendar = .current) -> Decision {
        var d = Decision()
        d.splash = true
        d.guide = guideSeen != majorMinor(version)
        d.feedback = lastFeedback.map { !calendar.isDate($0, inSameDayAs: now) } ?? true
        d.notice = noticeSeen != noticeVersion
        return d
    }

    /// 欢迎窗口里开屏之后的各页。
    enum Page: Equatable { case notice, files, why, hooks, feedback }

    /// 一项「看过」的记录。
    enum Mark: Equatable { case notice, guide, feedback }

    /// 用户在 `page` 上点了「继续」「完成」「以后再说」时该记下什么。只有这一个入口会写「看过」：窗口被关掉（关闭按钮、
    /// 退出 App、改语言后自动重启）不写。此前关窗一律记为看过，新用户在开屏页改语言、App 重启后就再也看不到引导
    /// （2026-11-16 用户全新安装 0.4.50 时遇到）。引导三页是一组，在最后一页点了才算看完。
    static func mark(leaving page: Page) -> Mark? {
        switch page {
        case .notice: return .notice
        case .files, .why: return nil
        case .hooks: return .guide
        case .feedback: return .feedback
        }
    }

    /// 测试版（Bundle ID 以 `.debug` 结尾）：App 菜单里另留一项常驻的「打开日志文件夹」，正式版只有按住 Option 的替代项。
    static func isTestBuild(bundleID: String?) -> Bool {
        bundleID?.hasSuffix(".debug") ?? false
    }

    /// 「主.次」，如 0.4.0 → 0.4；不足两段时原样返回。
    static func majorMinor(_ version: String) -> String {
        version.split(separator: ".").prefix(2).joined(separator: ".")
    }

    /// 「安装状态同步」页的 Codex 提醒（0.4.1）：装了 Codex 且状态同步还没正常（缺少条目、已停用、待信任）时显示，
    /// 因为 Codex 装好后还要用户在 Codex 里信任才生效。没装、已正常、被策略锁定或读不了配置时不显示。
    static func showsCodexTrustNote(codexState: String?) -> Bool {
        ["missing", "disabled", "untrusted"].contains(codexState ?? "")
    }
}

