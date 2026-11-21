// 会话要重开后才同步时给用户的说明（docs/design.md 4.7 节「会话是否真的在同步」，2026-11-20）：纯逻辑，界面在
// WorkspaceController。配置正常、但这个 Agent 进程启动时条目还没写入：会话开始时读 Hook 的几款开新会话或重开后才同步，
// 进程启动时读的要退出重开。命令都由用户自己输入，ScyllaUI 不代输。

import Foundation

enum SyncGuide {
    /// 后台给的同步情况（docs/protocol.md 第 5 节 `foreground.sync`）里要用户重开的两种。
    static func needsReopen(_ sync: String?) -> Bool { sync == "new_session" || sync == "restart" }

    /// 这款的启动命令与引导里要写的三条命令（`hooks.status` 的 `command`、`new_session`、`quit`、`resume_last`）；没有的为 nil。
    struct Commands: Equatable {
        var command: String?
        var newSession: String?
        var quit: String?
        var resumeLast: String?
    }

    /// 命令不拆行：空格换成不换行空格（U+00A0），「/」「--」后插入零宽的 U+2060。只用于说明文字。
    static func keep(_ command: String) -> String {
        command.replacingOccurrences(of: " ", with: "\u{00A0}")
            .replacingOccurrences(of: "--", with: "--\u{2060}")
            .replacingOccurrences(of: "/", with: "/\u{2060}")
    }

    /// 为什么没在同步。两个时间都有、且启动确实早于写入时写出时间（带秒，两者常在同一分钟里）；否则不写时间。
    static func reason(tool: String, startedAt: Int64?, changedAt: Int64?) -> String {
        guard let startedAt, let changedAt, startedAt < changedAt else {
            return String(localized: "这个 \(tool) 在写入 Hook 之前就已启动，因此读不到新写入的 Hook，当前线程的状态无法同步。")
        }
        let fmt = DateFormatter()
        fmt.dateStyle = .none
        fmt.timeStyle = .medium
        let time = { (ms: Int64) in fmt.string(from: Date(timeIntervalSince1970: TimeInterval(ms) / 1000)) }
        return String(localized: "这个 \(tool) 的启动时间（\(time(startedAt))）早于写入 Hook 的时间（\(time(changedAt))），因此读不到新写入的 Hook，当前线程的状态无法同步。")
    }

    /// 怎么做。`sync` 为 `new_session`（开新会话或重开）或 `restart`（只能退出重开）；缺命令时退回一句不带命令的说明。
    static func steps(tool: String, sync: String, commands c: Commands) -> String {
        guard let quit = c.quit.map(keep), let resume = c.resumeLast.map(keep) else {
            return String(localized: "退出 \(tool) 后重新打开，就能同步。")
        }
        if sync == "new_session", let new = c.newSession.map(keep) {
            return String(localized: "任选一种做法：\n\n1. 开新会话：在 \(tool) 里输入 \(new) 再按回车。当前对话不会带到新会话里。\n2. 接着当前对话：输入 \(quit) 再按回车退出，然后输入 \(resume) 再按回车。")
        }
        guard let command = c.command.map(keep) else {
            return String(localized: "退出 \(tool) 后重新打开，就能同步。")
        }
        return String(localized: "1. 在 \(tool) 里输入 \(quit) 再按回车，退出 \(tool)。\n2. 重新打开：输入 \(command) 再按回车；要接着刚才的对话，输入 \(resume) 再按回车。")
    }

    /// 点「未同步」时提示框的正文。
    static func text(tool: String, sync: String, commands: Commands, startedAt: Int64?, changedAt: Int64?) -> String {
        reason(tool: tool, startedAt: startedAt, changedAt: changedAt) + "\n\n" + steps(tool: tool, sync: sync, commands: commands)
    }

    /// 修复写入成功后，列给用户的一行（每款一行）。
    static func line(tool: String, sync: String, commands c: Commands) -> String {
        guard let quit = c.quit.map(keep), let resume = c.resumeLast.map(keep) else {
            return String(localized: "• \(tool)：退出后重新打开。")
        }
        if sync == "new_session", let new = c.newSession.map(keep) {
            return String(localized: "• \(tool)：输入 \(new) 开新会话；或输入 \(quit) 退出后，用 \(resume) 接着刚才的对话。")
        }
        guard let command = c.command.map(keep) else {
            return String(localized: "• \(tool)：退出后重新打开。")
        }
        return String(localized: "• \(tool)：输入 \(quit) 退出，再输入 \(command) 重新打开；要接着刚才的对话用 \(resume)。")
    }
}
