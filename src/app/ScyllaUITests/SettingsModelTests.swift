// 设置的纯逻辑单测（M4a，docs/design.md 10.4 节）：主题清单的读写往返、外观覆盖文件的解析与生成。

import XCTest

final class ThemeCatalogTests: XCTestCase {
    func testTenFamiliesWithKnownVariants() {
        XCTAssertEqual(ThemeCatalog.families.count, 10)
        XCTAssertEqual(ThemeCatalog.families.map(\.name).first, "Dracula")
        // 只有深色版的家族：Dracula、Nord。
        XCTAssertEqual(ThemeCatalog.families.filter { $0.light == nil }.map(\.name), ["Dracula", "Nord"])
    }

    func testValueForChoice() {
        let catppuccin = ThemeCatalog.families.firstIndex { $0.name == "Catppuccin" }!
        let nord = ThemeCatalog.families.firstIndex { $0.name == "Nord" }!
        XCTAssertNil(ThemeCatalog.value(for: .follow))
        XCTAssertEqual(ThemeCatalog.value(for: .family(catppuccin, .system)), "light:Catppuccin Latte,dark:Catppuccin Mocha")
        XCTAssertEqual(ThemeCatalog.value(for: .family(catppuccin, .dark)), "Catppuccin Mocha")
        XCTAssertEqual(ThemeCatalog.value(for: .family(catppuccin, .light)), "Catppuccin Latte")
        XCTAssertEqual(ThemeCatalog.value(for: .family(nord, .system)), "Nord", "只有深色版时不论深浅都写深色版")
        XCTAssertEqual(ThemeCatalog.value(for: .other("Belafonte Day")), "Belafonte Day")
    }

    func testChoiceRoundTrip() {
        for i in ThemeCatalog.families.indices {
            let modes: [ThemeMode] = ThemeCatalog.families[i].light == nil ? [.dark] : [.system, .dark, .light]
            for m in modes {
                let v = ThemeCatalog.value(for: .family(i, m))
                XCTAssertEqual(ThemeCatalog.choice(for: v), .family(i, m), v ?? "nil")
            }
        }
    }

    func testChoiceParsing() {
        XCTAssertEqual(ThemeCatalog.choice(for: nil), .follow)
        XCTAssertEqual(ThemeCatalog.choice(for: "  "), .follow)
        // Ghostty 允许逗号与冒号两侧有空白，顺序也可颠倒。
        XCTAssertEqual(ThemeCatalog.choice(for: " dark : Gruvbox Dark , light:Gruvbox Light "), .family(3, .system))
        // 表外的主题、表外的组合原样保留。
        XCTAssertEqual(ThemeCatalog.choice(for: "Belafonte Day"), .other("Belafonte Day"))
        XCTAssertEqual(ThemeCatalog.choice(for: "light:Dayfox,dark:Catppuccin Mocha"), .other("light:Dayfox,dark:Catppuccin Mocha"))
    }
}

final class AppearanceOverridesTests: XCTestCase {
    func testParseM2ThemeFile() {
        // M2 写的 theme.conf：只有注释与 theme。
        let m2 = "# ScyllaUI 设置中选择的主题，在用户的 Ghostty 配置之后加载；选「跟随 Ghostty 配置」时不含设置。\ntheme = \"Belafonte Day\"\n"
        XCTAssertEqual(AppearanceOverrides.parse(m2), AppearanceOverrides(theme: "Belafonte Day"))
        XCTAssertEqual(AppearanceOverrides.parse("# 只有注释\n"), AppearanceOverrides())
    }

    func testRenderAndParseRoundTrip() {
        let o = AppearanceOverrides(theme: "light:Catppuccin Latte,dark:Catppuccin Mocha", fontFamily: "SF Mono", fontSize: 14)
        let text = o.render()
        XCTAssertTrue(text.contains("font-family = \"\"\nfont-family = \"SF Mono\"\n"), "先写空值清掉用户配置里的字体")
        XCTAssertTrue(text.contains("font-size = 14\n"))
        XCTAssertEqual(AppearanceOverrides.parse(text), o)
        XCTAssertEqual(AppearanceOverrides.parse(AppearanceOverrides(fontSize: 13.5).render()).fontSize, 13.5)
    }

    func testFollowWritesNoSettings() {
        let lines = AppearanceOverrides().render().split(separator: "\n")
        XCTAssertTrue(lines.allSatisfy { $0.hasPrefix("#") })
    }
}

/// 状态栏的段开关（M4a，docs/design.md 10.4 节决策 3）：关掉的段连同分隔整段省略，整组都关掉时整组省略。
final class StatusSegmentTests: XCTestCase {
    private let now: Int64 = 1_800_000_000_000

    private let stats = StatusInfo.Stats(cpu: 12, mem: 840 * 1_048_576, processes: 3)

    private func info() -> StatusInfo {
        StatusInfo(tool: "codex", usage: SBUsage(total: 1_234_567),
                   quota: SBQuota(usedPercent: 34, resetsAt: now + 3_600_000 * 53, asOf: now, source: "log"))
    }

    private func text(_ hidden: Set<StatusSegment>) -> String {
        info().spans(now: now, hidden: hidden, stats: stats).map(\.text).joined()
    }

    /// 2026-11-21：项目、分支与改动、模型与推理强度不再显示；CPU 与内存跟在额度后面，同一行。
    func testAllShownMatchesDefault() {
        XCTAssertEqual(text([]), info().text(now: now, stats: stats))
        XCTAssertEqual(text([]), "Total: 1.2M • 7d: 34% (2d5h)   CPU: 12% • Mem: 840 MB")
    }

    func testHiddenSegmentsAreOmitted() {
        XCTAssertEqual(text([.total]), "7d: 34% (2d5h)   CPU: 12% • Mem: 840 MB")
        XCTAssertEqual(text([.cpu]), "Total: 1.2M • 7d: 34% (2d5h)   Mem: 840 MB")
    }

    func testWholeGroupsDisappearWithoutStraySeparators() {
        XCTAssertEqual(text([.total, .quota]), "CPU: 12% • Mem: 840 MB", "额度组全关时不留开头的组间空白")
        XCTAssertEqual(text([.cpu, .mem]), "Total: 1.2M • 7d: 34% (2d5h)", "CPU、内存都关时不留结尾的组间空白")
        XCTAssertEqual(text(Set(StatusSegment.allCases)), "")
    }

    func testStatsSegmentsUseValueColorAndTooltip() {
        let segs = StatusInfo.statsSegments(stats)
        XCTAssertEqual(segs.map(\.kind), [.cpu, .mem])
        XCTAssertEqual(segs.flatMap(\.spans).filter { $0.role == .value }.map(\.text), ["12%", "840 MB"], "数值与 Total 同为绿色")
        XCTAssertTrue(segs.allSatisfy { $0.tooltip?.contains("Shell 及其所有子进程（3 个）") == true })
        XCTAssertEqual(StatusInfo.statsSegments(stats, hidden: [.cpu, .mem]).count, 0)
        XCTAssertEqual(StatusInfo.statsSegments(StatusInfo.Stats()).count, 0, "还没采样时不显示")
    }
}
