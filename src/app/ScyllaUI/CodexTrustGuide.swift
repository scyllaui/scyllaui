// 引导用户信任 Codex 的 Hook（docs/design.md 4.7 节「引导用户信任」，2026-11-17）：走哪条路的判断与各路的说明文字，纯逻辑，
// 界面在 WorkspaceController.openCodexHooks。ScyllaUI 从不代写 Codex 的信任记录，只告诉用户在 Codex 里怎么做。

import Foundation

enum CodexTrustGuide {
    /// `/hooks`、`/quit` 会在「/」后折行，`codex resume --last` 会在空格和「--」后折行，看不出是一条命令：插入零宽的 U+2060
    /// （WORD JOINER）、把空格换成不换行空格（U+00A0），让它整体换行。只用于说明文字。
    static func keepCommands(_ text: String) -> String {
        text.replacingOccurrences(of: "/hooks", with: "/\u{2060}hooks").replacingOccurrences(of: "/quit", with: "/\u{2060}quit")
            .replacingOccurrences(of: "codex resume --last", with: "codex\u{00A0}resume\u{00A0}--\u{2060}last")
    }

    /// 引导用户信任 Codex 的 Hook 走哪条路（docs/design.md 4.7 节「引导用户信任」，2026-11-17）。Codex 在每个会话开始时读 Hook：
    /// 启动时间早于写入 Hook 的时间，这次运行读不到新写入的条目，`/hooks` 列表里全是 0，只能重开；否则在 `/hooks` 列表里按 T。
    /// 有一个时间取不到时走后者（它的说明里带着重开的退路）。时间都是 Unix 毫秒。
    enum Route: Equatable { case restart, hooksList }

    /// `entriesAtStart`：后台查到的「这个 Codex 启动时本项目的条目在不在」（按配置状态变化的时间线，比文件修改时间准）；
    /// 查得到就以它为准，查不到再比较两个时间。
    static func route(startedAt: Int64?, hookChangedAt: Int64?, entriesAtStart: Bool? = nil) -> Route {
        if let entriesAtStart { return entriesAtStart ? .hooksList : .restart }
        guard let startedAt, let hookChangedAt else { return .hooksList }
        return startedAt < hookChangedAt ? .restart : .hooksList
    }

    /// 重开一路的说明：写出两个时间和三步，每一步写明按哪个键、选第几项（默认高亮的是第 1 项「Review hooks」）。
    /// 时间带秒：先开 Codex 马上就装，两个时间常在同一分钟里，只到分钟会写成「19:24 早于 19:24」。
    /// 两个时间缺一个、或对不上先后时，不写时间，只说「在写入 Hook 之前就已启动」。
    static func restartText(startedAt: Int64?, hookChangedAt: Int64?) -> String {
        guard let startedAt, let hookChangedAt, startedAt < hookChangedAt else {
            return keepCommands(String(localized: "这个 Codex 在写入 Hook 之前就已启动，因此读不到新写入的 Hook，在当前线程中无法信任。\n\n1. 在 Codex 里输入 /quit 再按回车，退出 Codex。\n2. 重新打开：输入 codex 再按回车；要接着刚才的对话，输入 codex resume --last 再按回车。\n3. Codex 启动时会询问是否信任新的 Hook（Hooks need review）：按下方向键选中第 2 项「Trust all and continue」，再按回车。\n\n这一步只能由您在 Codex 里确认，ScyllaUI 不代写信任记录。"))
        }
        let fmt = DateFormatter()
        fmt.dateStyle = .none
        fmt.timeStyle = .medium
        let time = { (ms: Int64) in fmt.string(from: Date(timeIntervalSince1970: TimeInterval(ms) / 1000)) }
        return keepCommands(String(localized: "这个 Codex 的启动时间（\(time(startedAt))）早于写入 Hook 的时间（\(time(hookChangedAt))），因此读不到新写入的 Hook，在当前线程中无法信任。\n\n1. 在 Codex 里输入 /quit 再按回车，退出 Codex。\n2. 重新打开：输入 codex 再按回车；要接着刚才的对话，输入 codex resume --last 再按回车。\n3. Codex 启动时会询问是否信任新的 Hook（Hooks need review）：按下方向键选中第 2 项「Trust all and continue」，再按回车。\n\n这一步只能由您在 Codex 里确认，ScyllaUI 不代写信任记录。"))
    }

    /// 填入 `/hooks` 一路的说明。
    static var hooksListText: String {
        keepCommands(String(localized: "将在这个终端的 Codex 输入行填入 /hooks：\n\n1. 您按回车，打开 Codex 的 Hook 列表。\n2. 列表底部显示「t trust all」，这时按一下键盘上的字母键 T（不用回车），全部信任。\n\n列表里没有待信任的条目时：输入 /quit 再按回车退出 Codex，然后重新打开；它启动时会询问是否信任，按下方向键选中第 2 项「Trust all and continue」，再按回车。\n\n这一步只能由您在 Codex 里确认，ScyllaUI 不代写信任记录。"))
    }

    /// 没有正在运行的 Codex 时的说明。
    static var notRunningText: String {
        String(localized: "在 ScyllaUI 的终端里打开 Codex：它启动时会询问是否信任新的 Hook（Hooks need review），按下方向键选中第 2 项「Trust all and continue」，再按回车。")
    }
}
