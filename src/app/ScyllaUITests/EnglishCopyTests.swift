import AppKit
import XCTest

/// 2026-11-28 补测记录的英文截断（D3、D6、侧栏最近任务）：英文文案在默认宽度下放得下。测试运行在中文环境，
/// 英文取自字符串目录源文件，按控件实际用的字体量宽度。
final class EnglishCopyTests: XCTestCase {
    private static let catalog: [String: Any] = {
        let url = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent("ScyllaUI/Localizable.xcstrings")
        let json = try! JSONSerialization.jsonObject(with: Data(contentsOf: url)) as! [String: Any]
        return json["strings"] as! [String: Any]
    }()

    private func en(_ key: String) -> String {
        let entry = Self.catalog[key] as? [String: Any]
        let loc = (entry?["localizations"] as? [String: Any])?["en"] as? [String: Any]
        return ((loc?["stringUnit"] as? [String: Any])?["value"] as? String) ?? ""
    }

    private func width(_ s: String, _ font: NSFont) -> CGFloat { (s as NSString).size(withAttributes: [.font: font]).width }

    /// 「文件」栏筛选框的占位文字：默认栏宽（280）下中英文都放得下（D3：原英文截成「…to search co」）。
    func testFilterPlaceholderFitsDefaultWidth() {
        let panel = FilesPanel(frame: NSRect(x: 0, y: 0, width: RightPanelHost.defaultWidth, height: 600))
        let w = NSWindow(contentRect: panel.frame, styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = panel
        w.layoutIfNeeded()
        let room = panel.filterTextWidthForTesting
        let font = panel.filterFontForTesting!
        let key = "筛选，或输入 ? 搜索内容"
        XCTAssertGreaterThan(room, 100)
        XCTAssertLessThanOrEqual(width(key, font), room, "中文")
        XCTAssertLessThanOrEqual(width(en(key), font), room, "英文「\(en(key))」\(width(en(key), font)) pt，框里只有 \(room) pt")
    }

    /// 侧栏「最近任务」为空时那一行：侧栏最小宽度下放得下。单元格宽度同 SidebarCellsTests 的估法（最小宽度 − 列表左边距 6
    /// − 两级缩进 14 × 2 − 展开箭头 16），文字再让出 NoteCell 右边的 10。
    func testRecentEmptyFitsSidebarMinWidth() {
        let days = SidebarModel.recentDays
        let room = ScyllaUIStyle.sidebarMinWidth - 6 - 28 - 16 - 10
        let zh = "近 \(days) 天没有已结束的任务"
        let english = en("近 %lld 天没有已结束的任务").replacingOccurrences(of: "%lld", with: "\(days)")
        XCTAssertLessThanOrEqual(width(zh, ScyllaUIStyle.detailFont), room, "中文")
        XCTAssertLessThanOrEqual(width(english, ScyllaUIStyle.detailFont), room, "英文「\(english)」\(width(english, ScyllaUIStyle.detailFont)) pt，只有 \(room) pt")
    }

    /// 查看器标题行的「无法解码」一段：英文不比中文宽（标题行放不下时两种语言都截断、悬停看全）。
    func testUndecodableNotWiderThanChinese() {
        let bad = "有无法解码的字符"
        XCTAssertLessThanOrEqual(width(en(bad), ScyllaUIStyle.mono(11)), width(bad, ScyllaUIStyle.mono(11)), en(bad))
    }
}
