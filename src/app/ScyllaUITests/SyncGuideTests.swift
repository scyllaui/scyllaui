// 会话要重开后才同步时的说明（docs/design.md 4.7 节「会话是否真的在同步」，2026-11-20）：按适配器给的命令写出每一步，
// 命令不拆行；会话开始时读 Hook 的给两种做法（开新会话、退出后接着原对话），进程启动时读的只能退出重开。

import XCTest

final class SyncGuideTests: XCTestCase {
    private let codex = SyncGuide.Commands(command: "codex", newSession: "/new", quit: "/quit", resumeLast: "codex resume --last")
    private let kimi = SyncGuide.Commands(command: "kimi", newSession: nil, quit: "/exit", resumeLast: "kimi -c")

    func testNeedsReopenOnlyForTheTwoKinds() {
        XCTAssertTrue(SyncGuide.needsReopen("new_session") && SyncGuide.needsReopen("restart"))
        XCTAssertFalse(SyncGuide.needsReopen("off") || SyncGuide.needsReopen("old") || SyncGuide.needsReopen(nil))
    }

    func testCommandsAreKeptOnOneLine() {
        XCTAssertEqual(SyncGuide.keep("/new"), "/\u{2060}new")
        XCTAssertEqual(SyncGuide.keep("codex resume --last"), "codex\u{00A0}resume\u{00A0}--\u{2060}last")
        XCTAssertEqual(SyncGuide.keep("kimi -c"), "kimi\u{00A0}-c")
    }

    /// 两个时间都有且先后对得上才写时间（带秒）；否则只说在写入之前就已启动。
    func testReasonWritesTimesOnlyWhenTheyAreKnown() {
        let fmt = DateFormatter()
        fmt.dateStyle = .none
        fmt.timeStyle = .medium
        let started: Int64 = 1_794_914_647_796, changed: Int64 = 1_794_914_673_296
        let text = SyncGuide.reason(tool: "Kimi", startedAt: started, changedAt: changed)
        for ms in [started, changed] {
            XCTAssertTrue(text.contains("（\(fmt.string(from: Date(timeIntervalSince1970: TimeInterval(ms) / 1000)))）"), text)
        }
        XCTAssertTrue(text.contains("这个 Kimi 的启动时间") && text.contains("当前线程的状态无法同步"), text)
        for (s, c) in [(nil, changed), (started, nil), (changed, started)] as [(Int64?, Int64?)] {
            let plain = SyncGuide.reason(tool: "Kimi", startedAt: s, changedAt: c)
            XCTAssertTrue(plain.contains("这个 Kimi 在写入 Hook 之前就已启动") && !plain.contains("启动时间（"), plain)
        }
    }

    /// 会话开始时读的（Codex）：开新会话，或退出后接着原对话；进程启动时读的（Kimi）：退出重开。
    func testStepsFollowTheLoadClass() {
        let a = SyncGuide.steps(tool: "Codex", sync: "new_session", commands: codex)
        for part in ["任选一种做法", "在 Codex 里输入 /\u{2060}new 再按回车", "当前对话不会带到新会话里", "输入 /\u{2060}quit 再按回车退出", "codex\u{00A0}resume\u{00A0}--\u{2060}last"] {
            XCTAssertTrue(a.contains(part), part)
        }
        let b = SyncGuide.steps(tool: "Kimi", sync: "restart", commands: kimi)
        for part in ["在 Kimi 里输入 /\u{2060}exit 再按回车，退出 Kimi", "重新打开：输入 kimi 再按回车", "kimi\u{00A0}-c"] {
            XCTAssertTrue(b.contains(part), part)
        }
        XCTAssertFalse(b.contains("开新会话"), "进程启动时读的，开新会话没用")
        // 命令缺了就不写步骤，只给一句不带命令的说明。
        XCTAssertEqual(SyncGuide.steps(tool: "X", sync: "restart", commands: SyncGuide.Commands()), "退出 X 后重新打开，就能同步。")
        let full = SyncGuide.text(tool: "Codex", sync: "new_session", commands: codex, startedAt: nil, changedAt: nil)
        XCTAssertTrue(full.hasPrefix("这个 Codex 在写入 Hook 之前就已启动") && full.hasSuffix(a), full)
    }

    /// 修复之后列给用户的一行。
    func testLinePerTool() {
        let a = SyncGuide.line(tool: "Codex", sync: "new_session", commands: codex)
        XCTAssertTrue(a.hasPrefix("• Codex：输入 /\u{2060}new 开新会话") && a.contains("codex\u{00A0}resume\u{00A0}--\u{2060}last"), a)
        let b = SyncGuide.line(tool: "Kimi", sync: "restart", commands: kimi)
        XCTAssertTrue(b.hasPrefix("• Kimi：输入 /\u{2060}exit 退出，再输入 kimi 重新打开") && b.contains("kimi\u{00A0}-c"), b)
        XCTAssertEqual(SyncGuide.line(tool: "X", sync: "restart", commands: SyncGuide.Commands()), "• X：退出后重新打开。")
    }
}
