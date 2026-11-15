// App 日志文件的单测（docs/design.md 10.5 节决策 23）：隐私规则、按天分文件、保留 14 天。

import XCTest

final class AppLogTests: XCTestCase {
    func testPrivacyMatchesUnifiedLogDefaults() {
        let secret = "OPENAI_API_KEY=sk-test"
        let id = "abc"
        let m: LogMessage = "id=\(id, privacy: .public) prompt=\(secret) n=\(3) ok=\(true) ratio=\(0.5)"
        XCTAssertEqual(m.text, "id=abc prompt=<private> n=3 ok=true ratio=0.5", "没标 public 的字符串不落文件")
    }

    func testDailyFilesAndRetention() throws {
        let dir = (NSTemporaryDirectory() as NSString).appendingPathComponent("scyllaui-applog-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(atPath: dir) }
        var clock = DateComponents(calendar: .current, year: 2026, month: 10, day: 25, hour: 23, minute: 59).date!
        let log = AppLogFile(directory: dir, now: { clock })
        try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        // 15 天前的旧文件应在新的一天第一次写入时删掉，14 天内的保留。
        for name in ["app-2026-10-10.log", "app-2026-10-12.log", "scyllauid.log"] {
            FileManager.default.createFile(atPath: (dir as NSString).appendingPathComponent(name), contents: Data())
        }
        log.write("NOTICE", "睡前一条\n第二行")
        clock = clock.addingTimeInterval(120)
        log.write("ERROR", "跨天后一条")
        log.flush()
        let first = try String(contentsOfFile: (dir as NSString).appendingPathComponent("app-2026-10-25.log"), encoding: .utf8)
        let second = try String(contentsOfFile: (dir as NSString).appendingPathComponent("app-2026-10-26.log"), encoding: .utf8)
        XCTAssertTrue(first.hasPrefix("2026-10-25 23:59:00.000 NOTICE 睡前一条 ⏎ 第二行\n"), first)
        XCTAssertTrue(second.contains(" ERROR 跨天后一条"), "按写入时刻分到第二天的文件，不依赖跨天通知")
        let names = Set(try FileManager.default.contentsOfDirectory(atPath: dir))
        XCTAssertFalse(names.contains("app-2026-10-10.log"), "超过 14 天的删除")
        XCTAssertTrue(names.contains("app-2026-10-12.log"), "14 天内的保留")
        XCTAssertTrue(names.contains("scyllauid.log"), "不碰其他日志")
    }
}
