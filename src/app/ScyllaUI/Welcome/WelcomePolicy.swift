// 欢迎窗口何时出现（M4a 块 4，docs/design.md 5.9 节）。2026-11-15 用户决定：开屏每次启动都显示；「文稿」授权说明与
// Hook 两页引导只在首次启动与主或次版本号变化时接在开屏之后；最后一页「遇到问题，告诉我们」每天第一次打开时出现，不分新老用户；
// 使用须知没看过当前版本（新用户、须知改版）时显示一次。不再按「版本号低于 1.0.0」算试用期（主版本保持 0，对外发布也一样）。
// 判定是纯函数，可单测；记录存在 App 的 UserDefaults。

import Foundation

enum WelcomePolicy {
    /// 当前使用须知的版本（`Resources/Legal/Notice-*.txt` 的「版本」行）；改内容时升级，所有用户会再看到一次。
    static let noticeVersion = "2026-11-15"

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

