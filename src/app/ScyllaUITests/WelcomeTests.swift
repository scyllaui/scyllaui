// 首次运行引导与开屏页的纯逻辑单测（M4a 块 4，docs/design.md 10.4 节决策 9）：显示策略、开屏时间线与字标数据。

import XCTest

final class WelcomePolicyTests: XCTestCase {
    private var calendar: Calendar = {
        var c = Calendar(identifier: .gregorian)
        c.timeZone = TimeZone(identifier: "Asia/Shanghai")!
        return c
    }()

    private func date(_ s: String) -> Date {
        let f = DateFormatter()
        f.calendar = calendar
        f.timeZone = calendar.timeZone
        f.dateFormat = "yyyy-MM-dd HH:mm"
        return f.date(from: s)!
    }

    /// `feedback`：上次安排「遇到问题」一页的时间，缺省与上次开屏相同。
    private func decide(_ now: String, _ version: String, shown: String?, shownVersion: String?, guide: String?,
                        feedback: String?? = .none, notice: String? = WelcomePolicy.noticeVersion) -> WelcomePolicy.Decision {
        WelcomePolicy.decide(now: date(now), version: version, lastShown: shown.map(date), lastVersion: shownVersion,
                             guideSeen: guide, lastFeedback: (feedback ?? shown).map(date), noticeSeen: notice, calendar: calendar)
    }

    func testFirstLaunchShowsSplashAndGuide() {
        XCTAssertEqual(decide("2026-10-24 09:00", "0.4.0", shown: nil, shownVersion: nil, guide: nil),
                       .init(splash: true, guide: true, feedback: true))
        // 从 0.3.x 升上来（以前没有这些记录）同样算首次。
        XCTAssertEqual(decide("2026-10-24 09:00", "0.4.0", shown: nil, shownVersion: nil, guide: nil).guide, true)
    }

    func testSplashOnEveryLaunchGuideOnlyWhenNeeded() {
        // 2026-11-15 用户决定：开屏每次启动都显示；看过引导、当天已出现过「遇到问题」页时，只有开屏（不再按 1.0.0 前算试用期）。
        XCTAssertEqual(decide("2026-10-24 23:59", "0.4.47", shown: "2026-10-24 23:58", shownVersion: "0.4.47", guide: "0.4"),
                       .init(splash: true, guide: false, feedback: false))
        // 修订号变化：只有开屏。
        XCTAssertEqual(decide("2026-10-24 10:00", "0.4.48", shown: "2026-10-24 09:00", shownVersion: "0.4.47", guide: "0.4"),
                       .init(splash: true, guide: false, feedback: false))
        // 次版本、主版本变化：开屏与引导。
        XCTAssertEqual(decide("2026-10-24 10:00", "0.5.0", shown: "2026-10-24 09:00", shownVersion: "0.4.48", guide: "0.4"),
                       .init(splash: true, guide: true, feedback: false))
        XCTAssertEqual(decide("2026-10-24 10:00", "1.0.0", shown: "2026-10-24 09:00", shownVersion: "0.9.3", guide: "0.9"),
                       .init(splash: true, guide: true, feedback: false))
    }

    func testTestBuildKeepsLogsMenuItem() {
        XCTAssertTrue(WelcomePolicy.isTestBuild(bundleID: "dev.scyllaui.app.debug"))
        XCTAssertFalse(WelcomePolicy.isTestBuild(bundleID: "dev.scyllaui.app"))
        XCTAssertFalse(WelcomePolicy.isTestBuild(bundleID: nil))
    }

    func testNoticeUntilSeen() {
        // 没看过（新用户，或须知改版）：出现，开屏也出现。
        let d = decide("2026-11-07 10:00", "1.0.0", shown: "2026-11-07 09:00", shownVersion: "1.0.0", guide: "1.0", notice: nil)
        XCTAssertTrue(d.notice)
        XCTAssertTrue(d.splash, "须知待显示时开屏一定出现")
        // 看过当前版本：不再出现。
        XCTAssertFalse(decide("2026-11-07 10:00", "1.0.0", shown: "2026-11-07 09:00", shownVersion: "1.0.0", guide: "1.0").notice)
        // 须知改版：看过的是旧版本，再出现。
        XCTAssertTrue(decide("2026-11-07 10:00", "1.0.0", shown: "2026-11-07 09:00", shownVersion: "1.0.0", guide: "1.0",
                             notice: "2026-01-01").notice)
    }

    func testFeedbackPageOncePerDay() {
        // 每天第一次打开出现，不分新老用户（2026-11-02 用户要求）。
        XCTAssertEqual(decide("2026-10-25 10:00", "1.0.0", shown: "2026-10-25 09:00", shownVersion: "1.0.0", guide: "1.0",
                              feedback: "2026-10-24 20:00"),
                       .init(splash: true, guide: false, feedback: true), "开屏今天已显示过，第 5 页今天还没有")
        // 当天再打开（中间退出过后台也一样）不出现。
        XCTAssertEqual(decide("2026-10-25 23:00", "1.0.0", shown: "2026-10-25 09:00", shownVersion: "1.0.0", guide: "1.0",
                              feedback: "2026-10-25 09:00").feedback, false)
        // 第二天又出现；从没安排过（新用户或老用户刚升级）也出现。
        XCTAssertEqual(decide("2026-10-26 00:01", "1.0.0", shown: "2026-10-25 09:00", shownVersion: "1.0.0", guide: "1.0",
                              feedback: "2026-10-25 23:59").feedback, true)
        XCTAssertEqual(decide("2026-10-25 10:00", "1.0.0", shown: "2026-10-25 09:00", shownVersion: "1.0.0", guide: "1.0",
                              feedback: .some(nil)), .init(splash: true, guide: false, feedback: true))
    }

    /// 「看过」只在用户点了那一页的按钮时记：须知点「继续」、引导在最后一页点了按钮、「遇到问题」点「完成」。
    /// 引导前两页不单独记，没走到最后一页就关掉的，下次整组再出现。关窗不走这里，所以不会记。
    func testReadMarksComeOnlyFromActingOnAPage() {
        XCTAssertEqual(WelcomePolicy.mark(leaving: .notice), .notice)
        XCTAssertNil(WelcomePolicy.mark(leaving: .files))
        XCTAssertNil(WelcomePolicy.mark(leaving: .why))
        XCTAssertEqual(WelcomePolicy.mark(leaving: .hooks), .guide)
        XCTAssertEqual(WelcomePolicy.mark(leaving: .feedback), .feedback)
    }

    func testMajorMinor() {
        XCTAssertEqual(WelcomePolicy.majorMinor("0.4.0"), "0.4")
        XCTAssertEqual(WelcomePolicy.majorMinor("1.0"), "1.0")
        XCTAssertEqual(WelcomePolicy.majorMinor("2"), "2")
    }
}

final class CodexTrustNoteTests: XCTestCase {
    func testShownOnlyWhileCodexIsInstalledButNotWorking() {
        for state in ["missing", "disabled", "untrusted"] {
            XCTAssertTrue(WelcomePolicy.showsCodexTrustNote(codexState: state), state)
        }
        for state in ["ok", "absent", "policy", "unreadable"] {
            XCTAssertFalse(WelcomePolicy.showsCodexTrustNote(codexState: state), state)
        }
        // 状态还没取到（页面刚打开）时不显示。
        XCTAssertFalse(WelcomePolicy.showsCodexTrustNote(codexState: nil))
    }
}

final class SplashTimelineTests: XCTestCase {
    private func wordmark() throws -> SplashWordmark {
        let url = try XCTUnwrap(Bundle(for: Self.self).url(forResource: "wordmark-cells", withExtension: "json"))
        return try XCTUnwrap(SplashWordmark(json: Data(contentsOf: url)))
    }

    func testWordmarkData() throws {
        let w = try wordmark()
        XCTAssertEqual(w.size, CGSize(width: 266.75, height: 47.75))
        XCTAssertEqual(w.contours.count, 11)
        XCTAssertEqual(w.cells.count, 780)
        XCTAssertEqual(w.shadowRects.count, w.cells.count)
        XCTAssertEqual(w.tailRects.count, 6)
        XCTAssertTrue(w.cells.allSatisfy { (-1...1).contains($0.dy) && (-0.02...0.05).contains($0.jitter) })
        XCTAssertFalse(w.visualBox.isEmpty)
        XCTAssertEqual(w.light.ink, "237BC3")
        XCTAssertEqual(w.dark.ink, "D97757")
    }

    func testHeadsWiggleWithinLimitsAndRestAtEnd() {
        let end = SplashTimeline.base(SplashTimeline.total)
        for i in SplashTimeline.heads.indices {
            XCTAssertEqual(SplashTimeline.headAngle(i, 0), 0)
            XCTAssertEqual(SplashTimeline.headAngle(i, end), 0, "终帧回到静止的图标")
            var moved = false
            for n in 0...400 {
                let tb = end * Double(n) / 400
                let a = SplashTimeline.headAngle(i, tb)
                XCTAssertLessThanOrEqual(abs(a), SplashTimeline.headAmplitude + 1e-9)
                if abs(a) > 0.1 { moved = true }
            }
            XCTAssertTrue(moved, "头 \(i + 1) 要动起来")
        }
        // 逐帧：同一个 1/12 s 内姿态不变。
        let t0 = SplashTimeline.headsStart + 0.5 + 0.001
        XCTAssertEqual(SplashTimeline.headAngle(0, t0), SplashTimeline.headAngle(0, t0 + 0.08))
    }

    func testFinalFrame() throws {
        let w = try wordmark()
        let tw = SplashTimeline.base(SplashTimeline.total) - SplashTimeline.wordOffset
        XCTAssertTrue(w.cells.allSatisfy { SplashTimeline.cellStep($0, tw, w) == 3 })
        XCTAssertEqual(SplashTimeline.cellsAlpha(tw), 0)
        XCTAssertEqual(SplashTimeline.vectorAlpha(tw), 1)
        XCTAssertEqual(SplashTimeline.shadowAlpha(tw), 1)
        XCTAssertEqual(SplashTimeline.lineAlpha(tw), 0)
        XCTAssertEqual(SplashTimeline.subtitle(tw).alpha, 1)
        XCTAssertEqual(SplashTimeline.subtitle(tw).rise, 0)
        XCTAssertEqual(SplashTimeline.buttonAlpha(tw), 1)
        XCTAssertEqual(SplashTimeline.tileAlpha(SplashTimeline.base(SplashTimeline.total)), 1)
        // 开头什么都没出现。
        XCTAssertTrue(w.cells.allSatisfy { SplashTimeline.cellStep($0, -SplashTimeline.wordOffset, w) == 0 })
        XCTAssertEqual(SplashTimeline.tileAlpha(0), 0)
    }

    func testScanLineMovesRight() throws {
        let w = try wordmark()
        var last = -CGFloat.infinity
        for n in 0...100 {
            let x = SplashTimeline.lineX(Double(n) / 100, w)
            XCTAssertGreaterThanOrEqual(x, last)
            last = x
        }
        XCTAssertEqual(SplashTimeline.lineX(1, w), w.size.width + 3 * w.cell, accuracy: 1e-9)
    }
}
