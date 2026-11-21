// 引导用户信任 Codex 的 Hook（docs/design.md 4.7 节「引导用户信任」，2026-11-17）：先判断 Codex 的启动时间是否早于写入 Hook 的
// 时间，再给对应的引导；说明里每一步写明按哪个键、选第几项。

import XCTest

final class CodexTrustGuideTests: XCTestCase {
    /// 启动时间早于写入时间才提示重开；相等、晚于，或有一个时间取不到，都走 `/hooks` 列表（它的说明里带着重开的退路）。
    func testCodexTrustRouteComparesStartTimeWithHookWriteTime() {
        XCTAssertEqual(CodexTrustGuide.route(startedAt: 1_000, hookChangedAt: 1_001), .restart)
        XCTAssertEqual(CodexTrustGuide.route(startedAt: 1_000, hookChangedAt: 1_000), .hooksList)
        XCTAssertEqual(CodexTrustGuide.route(startedAt: 1_001, hookChangedAt: 1_000), .hooksList)
        XCTAssertEqual(CodexTrustGuide.route(startedAt: nil, hookChangedAt: 1_000), .hooksList)
        XCTAssertEqual(CodexTrustGuide.route(startedAt: 1_000, hookChangedAt: nil), .hooksList)
        // 后台查到「启动时条目在不在」（2026-11-20）就以它为准，不看两个时间：文件修改时间会被别的改动带着变。
        XCTAssertEqual(CodexTrustGuide.route(startedAt: 1_001, hookChangedAt: 1_000, entriesAtStart: false), .restart)
        XCTAssertEqual(CodexTrustGuide.route(startedAt: 1_000, hookChangedAt: 1_001, entriesAtStart: true), .hooksList)
        XCTAssertEqual(CodexTrustGuide.route(startedAt: nil, hookChangedAt: nil, entriesAtStart: false), .restart)
    }

    /// 重开一路缺时间、或时间对不上先后时：不写时间，只说在写入 Hook 之前就已启动，三步照写。
    func testCodexRestartTextWithoutTimes() {
        for (started, changed) in [(nil, 2_000), (1_000, nil), (3_000, 2_000)] as [(Int64?, Int64?)] {
            let text = CodexTrustGuide.restartText(startedAt: started, hookChangedAt: changed)
            for part in ["在写入 Hook 之前就已启动", "在当前线程中无法信任", "/\u{2060}quit", "codex\u{00A0}resume\u{00A0}--\u{2060}last", "第 2 项「Trust all and continue」"] {
                XCTAssertTrue(text.contains(part), part)
            }
            XCTAssertFalse(text.contains("启动时间（"), "不写时间")
        }
    }

    /// 重开一路：写出两个时间、退出与重开的命令、询问里选第 2 项；不让 `/quit` 在「/」后折行，不让 `codex resume --last` 中途折行。
    func testCodexRestartTextSpellsOutEachStep() {
        let fmt = DateFormatter()
        fmt.dateStyle = .none
        fmt.timeStyle = .medium
        // 同一分钟里的两个时间（相差 25 秒）：带秒才分得出先后。
        let started: Int64 = 1_794_914_647_796, changed: Int64 = 1_794_914_673_296
        let text = CodexTrustGuide.restartText(startedAt: started, hookChangedAt: changed)
        XCTAssertNotEqual(fmt.string(from: Date(timeIntervalSince1970: TimeInterval(started) / 1000)), fmt.string(from: Date(timeIntervalSince1970: TimeInterval(changed) / 1000)))
        for ms in [started, changed] {
            XCTAssertTrue(text.contains("（\(fmt.string(from: Date(timeIntervalSince1970: TimeInterval(ms) / 1000)))）"), text)
        }
        for part in ["启动时间", "早于写入 Hook 的时间", "在当前线程中无法信任", "/\u{2060}quit", "codex\u{00A0}resume\u{00A0}--\u{2060}last", "Hooks need review", "下方向键", "第 2 项「Trust all and continue」", "ScyllaUI 不代写信任记录"] {
            XCTAssertTrue(text.contains(part), part)
        }
        XCTAssertFalse(text.contains("/hooks") || text.contains("/\u{2060}hooks"), "重开一路不让用户去 /hooks 列表")
    }

    /// `/hooks` 一路：写明回车后按字母键 T，并带着列表里没有时重开的退路；没有运行中的 Codex 时写明选第 2 项。
    func testCodexHooksListAndNotRunningTexts() {
        let list = CodexTrustGuide.hooksListText
        for part in ["/\u{2060}hooks：", "t trust all", "字母键 T（不用回车）", "/\u{2060}quit", "第 2 项「Trust all and continue」", "ScyllaUI 不代写信任记录"] {
            XCTAssertTrue(list.contains(part), part)
        }
        XCTAssertFalse(list.contains("/hooks"), "命令不在「/」后折行")
        let none = CodexTrustGuide.notRunningText
        XCTAssertTrue(none.contains("第 2 项「Trust all and continue」") && none.contains("下方向键"), none)
    }
}
