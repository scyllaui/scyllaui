// 上次意外退出的判定与正文、App 菜单的日志入口（M4b 块 7，docs/design.md 10.6 节「块 7 实现」）。

import AppKit
import XCTest

final class UnexpectedExitTests: XCTestCase {
    private let alive = Date(timeIntervalSince1970: 1_793_000_000)

    func testNoHeartbeatMeansCleanExit() {
        XCTAssertNil(UnexpectedExit(lastAlive: nil, held: 3, lost: 0))
        XCTAssertNil(UnexpectedExit(lastAlive: nil, held: 0, lost: 2))
    }

    func testTerminalsFollowFirstSnapshot() {
        XCTAssertEqual(UnexpectedExit(lastAlive: alive, held: 2, lost: 0)?.terminals, .held)
        XCTAssertEqual(UnexpectedExit(lastAlive: alive, held: 0, lost: 3)?.terminals, .lost)
        XCTAssertEqual(UnexpectedExit(lastAlive: alive, held: 0, lost: 0)?.terminals, UnexpectedExit.Terminals.none)
        // 后台还保管着一部分时按「已经接回」说。
        XCTAssertEqual(UnexpectedExit(lastAlive: alive, held: 1, lost: 1)?.terminals, .held)
    }

    func testMessages() throws {
        let report = "反馈时点「报告问题…」"
        let held = try XCTUnwrap(UnexpectedExit(lastAlive: alive, held: 1, lost: 0)).message
        XCTAssertTrue(held.hasPrefix("终端和任务都在后台保管着，已经接回。"))
        XCTAssertTrue(held.contains(report))
        let lost = try XCTUnwrap(UnexpectedExit(lastAlive: alive, held: 0, lost: 1)).message
        XCTAssertTrue(lost.hasPrefix("后台也已结束，终端按设置恢复。"))
        XCTAssertTrue(lost.contains(report))
        let none = try XCTUnwrap(UnexpectedExit(lastAlive: alive, held: 0, lost: 0)).message
        XCTAssertTrue(none.hasPrefix(report))
        XCTAssertTrue(none.contains("崩溃报告和这个时间"))
    }

    func testTitleCarriesHeartbeatTime() throws {
        let notice = try XCTUnwrap(UnexpectedExit(lastAlive: alive, held: 0, lost: 0))
        XCTAssertEqual(notice.title { $0 == self.alive ? "10月28日 15:30" : "?" }, "ScyllaUI 上次意外退出（约 10月28日 15:30）")
    }

    func testLogsItemIsOptionAlternateOfAbout() {
        let items = AppMenu.aboutItems(target: self, about: Selector(("showAbout:")), logs: Selector(("openLogFolder:")))
        XCTAssertEqual(items.map(\.title), ["关于 ScyllaUI", "打开日志文件夹"])
        XCTAssertFalse(items[0].isAlternate)
        XCTAssertTrue(items[1].isAlternate)
        XCTAssertEqual(items[1].keyEquivalentModifierMask, .option)
        // 替代项与前一项的快捷键必须相同、修饰键不同，系统才把它们当成一对。
        XCTAssertEqual(items[0].keyEquivalent, items[1].keyEquivalent)
        XCTAssertNotEqual(items[0].keyEquivalentModifierMask, items[1].keyEquivalentModifierMask)
        XCTAssertTrue(items.allSatisfy { $0.target === self })
    }
}
