// 报告问题的诊断包（docs/design.md 10.6 节「问题反馈」）：挑文件、截尾、脱敏、摘要与 GitHub 网址。

import XCTest

final class ProblemReportTests: XCTestCase {
    private var root: URL!
    private let fm = FileManager.default
    private let now = Date(timeIntervalSince1970: 1_793_000_000)

    override func setUpWithError() throws {
        root = fm.temporaryDirectory.appendingPathComponent("problem-report-\(UUID().uuidString)", isDirectory: true)
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        try? fm.removeItem(at: root)
    }

    private func file(_ path: String, _ text: String, age days: Double = 0) throws {
        let url = root.appendingPathComponent(path)
        try fm.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try Data(text.utf8).write(to: url)
        try fm.setAttributes([.modificationDate: now.addingTimeInterval(-days * 86_400)], ofItemAtPath: url.path)
    }

    private func day(_ back: Int) -> String {
        let f = DateFormatter()
        f.locale = Locale(identifier: "en_US_POSIX")
        f.timeZone = .current
        f.dateFormat = "yyyy-MM-dd"
        return f.string(from: Calendar.current.date(byAdding: .day, value: -back, to: now)!)
    }

    func testCollectPicksOwnLogsAndCrashReports() throws {
        let logs = "Library/Logs/ScyllaUI"
        try file("\(logs)/scyllauid.log", "daemon\n")
        try file("\(logs)/app-\(day(0)).log", "today\n")
        try file("\(logs)/app-\(day(2)).log", "two days ago\n")
        try file("\(logs)/app-\(day(3)).log", "too old\n")
        try file("\(logs)/terminal-geometry.log", "geometry\n")
        let reports = "Library/Logs/DiagnosticReports"
        try file("\(reports)/ScyllaUI-2026-11-02-100000.ips", "{}", age: 0.5)
        try file("\(reports)/ScyllaUI-2026-10-28-195600.ips", "{}", age: 1)
        try file("\(reports)/scyllauid-2026-10-20-100000.ips", "{}", age: 9)
        try file("\(reports)/ScyllaUI-2026-09-01-100000.ips", "{}", age: 40)
        try file("\(reports)/codex-2026-10-28-100000.ips", "{}", age: 1)
        let crash = ".local/state/ghostty/crash"
        for (i, age) in [1.0, 2, 3, 4].enumerated() {
            try file("\(crash)/own-\(i).ghosttycrash", #"{"event_id":"x","release":"1.3.2-HEAD+34f3900"}"#, age: age)
        }
        try file("\(crash)/ghostty-app.ghosttycrash", #"{"release":"1.3.1"}"#, age: 1)

        let items = ProblemReport.collect(logDir: root.appendingPathComponent(logs).path, home: root.path,
                                          engine: "1.3.2-HEAD+34f3900", now: now)
        XCTAssertEqual(items.filter { !$0.crash }.map(\.name), ["scyllauid.log", "app-\(day(0)).log", "app-\(day(2)).log"])
        XCTAssertEqual(items.filter { $0.crash }.map(\.name), [
            "ScyllaUI-2026-11-02-100000.ips", "ScyllaUI-2026-10-28-195600.ips", "scyllauid-2026-10-20-100000.ips",
            "own-0.ghosttycrash", "own-1.ghosttycrash", "own-2.ghosttycrash",
        ], "30 天内的 .ips；终端引擎的只取版本一致的最新 3 份")

        let none = ProblemReport.collect(logDir: root.appendingPathComponent(logs).path, home: root.path, engine: nil, now: now)
        XCTAssertFalse(none.contains { $0.name.hasSuffix(".ghosttycrash") }, "认不出引擎版本时不附终端引擎的崩溃报告")
    }

    func testDiagnosticReportNames() {
        XCTAssertTrue(ProblemReport.isOwnDiagnosticReport("ScyllaUI-2026-11-02-195600.ips"))
        XCTAssertTrue(ProblemReport.isOwnDiagnosticReport("ScyllaUIBeta-2026-11-02-195600.ips"))
        XCTAssertTrue(ProblemReport.isOwnDiagnosticReport("scyllauid-2026-10-28-195600.ips"))
        XCTAssertFalse(ProblemReport.isOwnDiagnosticReport("codex-2026-10-28-195600.ips"))
        XCTAssertFalse(ProblemReport.isOwnDiagnosticReport("ScyllaUI-2026-10-28.diag"))
    }

    func testGhosttyRelease() {
        XCTAssertEqual(ProblemReport.ghosttyRelease(in: Data(#"{"dsn":null,"release":"1.3.2-HEAD+34f3900","x":1}"#.utf8)), "1.3.2-HEAD+34f3900")
        XCTAssertNil(ProblemReport.ghosttyRelease(in: Data("binary".utf8)))
    }

    func testTailKeepsWholeLines() {
        let data = Data("first line\nsecond line\nthird\n".utf8)
        XCTAssertEqual(ProblemReport.tail(data, limit: 100), data)
        XCTAssertEqual(String(decoding: ProblemReport.tail(data, limit: 14), as: UTF8.self), "third\n")
    }

    func testRedactHome() {
        let text = "cwd=/Users/example/Documents/x db=/Users/example other=/Users/example2/y \"/Users/example\""
        XCTAssertEqual(ProblemReport.redact(text, home: "/Users/example"), "cwd=~/Documents/x db=~ other=/Users/example2/y \"~\"")
    }

    func testSummary() {
        let env = ProblemReport.Environment(
            appVersion: "0.4.22", buildNumber: "512", buildID: "3481def", daemonBuildID: "3481def",
            system: "macOS 27.0.1", language: "zh-Hans", engine: "1.3.2-HEAD+34f3900",
            hooks: [["tool": "claude", "state": "ok", "problems": [String](), "version": "2.1.3", "version_running": true, "version_verified": true],
                    ["tool": "codex", "state": "untrusted", "problems": ["untrusted:Stop"]]],
            inaccurate: [InaccurateItem(tool: "kimi", versions: ["9.9.9"], reasons: ["unknown_event:Begin"], verifiedLatest: nil)],
            lastUnexpectedExit: now)
        let s = ProblemReport.summary(env, description: "侧栏卡在 Running", now: now)
        XCTAssertTrue(s.hasPrefix("ScyllaUI problem report\n"))
        XCTAssertTrue(s.contains("App: 0.4.22 (build 512, id 3481def)"))
        XCTAssertTrue(s.contains("scyllauid: connected, id 3481def (same as app)"))
        XCTAssertTrue(s.contains("  claude: ok, 2.1.3 (running, verified)"))
        XCTAssertTrue(s.contains("  codex: untrusted [untrusted:Stop]"))
        XCTAssertTrue(s.contains("  kimi 9.9.9: unknown_event:Begin"))
        XCTAssertTrue(s.contains("Last unexpected exit: "))
        XCTAssertTrue(s.hasSuffix("Description:\n侧栏卡在 Running\n"))
    }

    func testIssueURL() throws {
        let url = ProblemReport.issueURL(title: "ScyllaUI 0.4.22: a+b", body: "C++ 很慢")
        let s = url.absoluteString
        XCTAssertTrue(s.hasPrefix("https://github.com/scyllaui/scyllaui/issues/new?"))
        XCTAssertTrue(s.contains("a%2Bb") && s.contains("C%2B%2B"), "「+」要转义，否则 GitHub 读成空格")
        let items = try XCTUnwrap(URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems)
        XCTAssertEqual(items.first { $0.name == "body" }?.value, "C++ 很慢")

        let long = ProblemReport.issueURL(title: "t", body: String(repeating: "长", count: 5000), maxBody: 100)
        let body = try XCTUnwrap(URLComponents(url: long, resolvingAgainstBaseURL: false)?.queryItems?.first { $0.name == "body" }?.value)
        XCTAssertEqual(body, String(repeating: "长", count: 100) + "\n…")
    }

    func testWriteAndZip() throws {
        try file("logs/scyllauid.log", "old\n" + String(repeating: "x", count: 10) + "\ncwd=\(root.path)/proj\n")
        try file("crash/own.ghosttycrash", "binary \(root.path)")
        let items = [
            ProblemReport.Item(source: root.appendingPathComponent("logs/scyllauid.log"), name: "scyllauid.log", size: 1, crash: false),
            ProblemReport.Item(source: root.appendingPathComponent("crash/own.ghosttycrash"), name: "own.ghosttycrash", size: 1, crash: true),
        ]
        let dir = root.appendingPathComponent("out/scyllaui-report-test", isDirectory: true)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        ProblemReport.write(items, to: dir, home: root.path)
        XCTAssertEqual(try String(contentsOf: dir.appendingPathComponent("logs/scyllauid.log"), encoding: .utf8).components(separatedBy: "\n").last { !$0.isEmpty },
                       "cwd=~/proj")
        XCTAssertEqual(try String(contentsOf: dir.appendingPathComponent("crash/own.ghosttycrash"), encoding: .utf8), "binary \(root.path)",
                       "二进制的崩溃报告原样复制")
        let zip = try ProblemReport.zip(dir, summary: "summary\n")
        XCTAssertEqual(zip.lastPathComponent, "scyllaui-report-test.zip")
        XCTAssertGreaterThan((try fm.attributesOfItem(atPath: zip.path)[.size] as? Int) ?? 0, 0)
        XCTAssertEqual(try String(contentsOf: dir.appendingPathComponent("summary.txt"), encoding: .utf8), "summary\n")
    }
}
