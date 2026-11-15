// 侧栏单元格的单测（第 5 块起）：项目行的悬停置顶、对话行的悬停删除、工具图标、任务行间距与最窄宽度。

import AppKit
import XCTest

final class SidebarCellsTests: XCTestCase {
    private func hover(_ cell: NSView, _ inside: Bool) throws {
        let event = try XCTUnwrap(NSEvent.enterExitEvent(with: inside ? .mouseEntered : .mouseExited, location: .zero, modifierFlags: [],
                                                         timestamp: 0, windowNumber: 0, context: nil, eventNumber: 0,
                                                         trackingNumber: 0, userData: nil))
        if inside { cell.mouseEntered(with: event) } else { cell.mouseExited(with: event) }
    }

    /// 项目行（docs/design.md 10.5 节决策 7、9）：不显示任务数，名称为 Ready 绿；唯一的按钮是右端的置顶按钮：未置顶时只在
    /// 悬停时出现，已置顶时常显，点按切换；「最近任务」的项目行（不能置顶）悬停也不出现。按钮位置始终留着，悬停时名称不动。
    func testProjectRowPinOnHover() throws {
        let cell = ProjectCell(frame: NSRect(x: 0, y: 0, width: 260, height: ScyllaUIStyle.projectRowHeight))
        var target: Bool?
        cell.configure(.project(name: "ai-workspace", branch: "main", tooltip: "/w/ai-workspace", dir: "/w/ai-workspace", pinned: false))
        cell.onPin = { target = $0 }
        cell.layoutSubtreeIfNeeded()
        let buttons = cell.subviews.compactMap { $0 as? NSButton }
        XCTAssertEqual(buttons.count, 1)
        let pin = buttons[0]
        let labels = cell.subviews.compactMap { $0 as? NSTextField }
        XCTAssertEqual(labels.map(\.stringValue), ["ai-workspace"], "没有任务数")
        XCTAssertEqual(labels[0].textColor, ScyllaUIStyle.ready)
        let nameFrame = labels[0].frame
        XCTAssertTrue(pin.isHidden, "未置顶、不悬停时不显示")
        try hover(cell, true)
        cell.layoutSubtreeIfNeeded()
        XCTAssertFalse(pin.isHidden, "悬停时出现")
        XCTAssertEqual(labels[0].frame, nameFrame, "悬停时名称不动")
        XCTAssertEqual(pin.alignmentRect(forFrame: pin.frame).maxX, cell.bounds.maxX - 10, accuracy: 0.5, "在右端")
        pin.performClick(nil)
        XCTAssertEqual(target, true, "点按置顶")
        try hover(cell, false)
        cell.configure(.project(name: "ai-workspace", branch: "main", tooltip: "/w/ai-workspace", dir: "/w/ai-workspace", pinned: true))
        XCTAssertFalse(pin.isHidden, "已置顶时常显")
        pin.performClick(nil)
        XCTAssertEqual(target, false, "点按取消置顶")
        cell.onPin = nil
        cell.configure(.project(name: "ai-workspace", branch: "main", tooltip: "/w/ai-workspace", dir: "/w/ai-workspace", pinned: nil))
        try hover(cell, true)
        XCTAssertTrue(pin.isHidden, "最近任务的项目行不能置顶")
    }

    /// 对话行的悬停删除按钮（docs/design.md 10.5 节决策 4）：只在鼠标停在对话行上时出现，点按回调；终端行没有。
    func testDeleteButtonOnlyOnHoveredConversation() throws {
        let cell = TaskCell(frame: NSRect(x: 0, y: 0, width: 260, height: ScyllaUIStyle.taskRowHeight))
        let row = TaskRow(lamp: .idle, tool: "Codex", toolKey: "codex", state: "Stopped", needsConfirm: false,
                          unseen: false, subtitle: "整理测试用例", time: "10-24")
        func trash() throws -> NSButton {
            func all(_ v: NSView) -> [NSView] { v.subviews + v.subviews.flatMap(all) }
            return try XCTUnwrap(all(cell).compactMap { $0 as? NSButton }.first { $0.toolTip == "删除记录…" })
        }
        cell.configure(.terminal(row))
        cell.onDelete = nil
        try hover(cell, true)
        XCTAssertTrue(try trash().isHidden, "终端行没有删除按钮")
        try hover(cell, false)
        var deleted = false
        cell.configure(.conversation(row))
        cell.onDelete = { deleted = true }
        XCTAssertTrue(try trash().isHidden, "不悬停时不显示")
        try hover(cell, true)
        XCTAssertFalse(try trash().isHidden, "悬停时显示")
        try trash().performClick(nil)
        XCTAssertTrue(deleted)
        try hover(cell, false)
        XCTAssertTrue(try trash().isHidden, "移开后隐藏")
    }

    /// 图标取不到（未知工具或 Shell）时用系统符号 terminal，不写针对某款工具的分支。
    func testToolIconFallsBackToTerminalSymbol() {
        let shell = toolIcon(nil)
        XCTAssertNotNil(shell)
        XCTAssertEqual(toolIcon("unknown-tool")?.tiffRepresentation, shell?.tiffRepresentation)
    }

    /// 子视图墨迹的左右边界（单元格坐标，pt）：按 2 倍像素渲染，取不透明度超过 0.3 的列。
    private func inkX(_ v: NSView) -> (lo: CGFloat, hi: CGFloat)? {
        let scale: CGFloat = 2
        let w = Int(v.bounds.width * scale), h = Int(v.bounds.height * scale)
        guard w > 0, h > 0,
              let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: w, pixelsHigh: h, bitsPerSample: 8,
                                         samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                                         bytesPerRow: 0, bitsPerPixel: 0) else { return nil }
        rep.size = v.bounds.size
        v.cacheDisplay(in: v.bounds, to: rep)
        // 阈值取这个视图最深墨迹的 30%：跟随文字色的单色图标（labelColor 本身不是满不透明）与着色图标按同一标准量边缘。
        let alpha = { (x: Int, y: Int) in rep.colorAt(x: x, y: y)?.alphaComponent ?? 0 }
        let peak = (0..<w).flatMap { x in (0..<h).map { y in alpha(x, y) } }.max() ?? 0
        let cols = (0..<w).filter { x in (0..<h).contains { y in alpha(x, y) > 0.3 * peak } }
        guard let lo = cols.first, let hi = cols.last else { return nil }
        return (v.frame.minX + CGFloat(lo) / scale, v.frame.minX + CGFloat(hi + 1) / scale)
    }

    /// 任务行第 1 行：「图标 → 工具名」与「工具名 → 状态词」的视觉间距（墨迹之间）一致，误差 1 pt 内
    /// （2026-10-12 用户试用：图标贴着名字，要求与名字和状态词的间距一致）。用真实图标，四款都测。
    func testToolIconGapMatchesStateGap() throws {
        let bundle = Bundle(for: Self.self)
        for (key, tool, state) in [("claude", "Claude", "Ready"), ("codex", "Codex", "未知"),
                                   ("kimi", "Kimi", "未知"), ("grok", "Grok", "Ready")] {
            let image = try XCTUnwrap(bundle.image(forResource: "tool-\(key)"), key)
            image.setName("tool-\(key)")
            let cell = TaskCell(frame: NSRect(x: 0, y: 0, width: ScyllaUIStyle.sidebarMinWidth, height: ScyllaUIStyle.taskRowHeight))
            cell.configure(.terminal(TaskRow(lamp: .ready, tool: tool, toolKey: key, state: state, needsConfirm: false,
                                             unseen: false, subtitle: "~", time: "17:37")))
            cell.layoutSubtreeIfNeeded()
            let icon = try XCTUnwrap(cell.subviews.first { $0 is NSImageView })
            let labels = cell.subviews.compactMap { $0 as? NSTextField }
            let name = try XCTUnwrap(labels.first { $0.stringValue == tool })
            let word = try XCTUnwrap(labels.first { $0.stringValue == state })
            let i = try XCTUnwrap(inkX(icon)), n = try XCTUnwrap(inkX(name)), s = try XCTUnwrap(inkX(word))
            let iconGap = n.lo - i.hi, stateGap = s.lo - n.hi
            print("\(tool)：图标→名字 \(iconGap) pt，名字→状态 \(stateGap) pt")
            XCTAssertEqual(iconGap, stateGap, accuracy: 1, tool)
        }
    }

    /// 分区细线（0.3.5）：起点在数字墨迹右缘后 8 pt；「当前任务」止于「+」墨迹左缘前 8 pt，没有「+」时止于右缘前 8 pt。误差 1 pt 内。
    func testSectionRuleSpan() throws {
        let width = ScyllaUIStyle.sidebarMinWidth - 6
        let cell = SectionCell(frame: NSRect(x: 0, y: 0, width: width, height: ScyllaUIStyle.sectionRowHeight))
        cell.configure(.section(title: "当前任务", count: 12))
        cell.onAdd = { _ in }
        cell.layoutSubtreeIfNeeded()
        let count = try XCTUnwrap(cell.subviews.compactMap { $0 as? NSTextField }.first { $0.stringValue == "12" })
        let plus = try XCTUnwrap(cell.subviews.first { $0 is NSButton })
        let n = try XCTUnwrap(inkX(count)), p = try XCTUnwrap(inkX(plus))
        XCTAssertEqual(cell.ruleSpan.from, n.hi + 8, accuracy: 1)
        XCTAssertEqual(cell.ruleSpan.to, p.lo - 8, accuracy: 0.5)
        cell.configure(.section(title: "最近任务", count: 3))
        cell.onAdd = nil
        cell.layoutSubtreeIfNeeded()
        XCTAssertEqual(cell.ruleSpan.to, width - 8)
    }

    /// 字号改大后（2026-10-12，参照 Codex 侧栏），侧栏最小宽度下任务行的状态词不被压缩。单元格宽度按真实侧栏估算：
    /// 最小宽度 − 列表左边距 6 − 两级缩进 14 × 2 − 展开箭头 16。
    func testTaskRowFitsAtMinimumWidth() throws {
        let image = try XCTUnwrap(Bundle(for: Self.self).image(forResource: "tool-claude"))
        image.setName("tool-claude")
        let width = ScyllaUIStyle.sidebarMinWidth - 6 - 28 - 16
        let cell = TaskCell(frame: NSRect(x: 0, y: 0, width: width, height: ScyllaUIStyle.taskRowHeight))
        cell.configure(.terminal(TaskRow(lamp: .running, tool: "Claude", toolKey: "claude", state: "Running", needsConfirm: false,
                                         unseen: true, subtitle: "排查登录失败的原因", time: "19:59")))
        cell.layoutSubtreeIfNeeded()
        let word = try XCTUnwrap(cell.subviews.compactMap { $0 as? NSTextField }.first { $0.stringValue == "Running" })
        XCTAssertGreaterThanOrEqual(word.frame.width, word.intrinsicContentSize.width - 0.5)
    }
    /// 「冲突」徽标（M1 第 6 块）：只在冲突时显示，在第 2 行时间左侧；与「需确认」同时出现时，最小宽度下状态词仍不被压缩，
    /// 时间完整显示。
    func testConflictBadge() throws {
        let width = ScyllaUIStyle.sidebarMinWidth - 6 - 28 - 16
        let cell = TaskCell(frame: NSRect(x: 0, y: 0, width: width, height: ScyllaUIStyle.taskRowHeight))
        func badge() -> NSView? { cell.descendants.first { ($0 as? BadgeView)?.text == "冲突" } }
        var row = TaskRow(lamp: .waiting, tool: "Codex", toolKey: "codex", state: "Waiting", needsConfirm: true,
                          unseen: false, subtitle: "修导航焦点样式，顺便检查键盘焦点的顺序", time: "19:59")
        cell.configure(.terminal(row))
        XCTAssertEqual(badge()?.isHidden, true)
        row.conflictTerminals = 2
        cell.configure(.terminal(row))
        cell.layoutSubtreeIfNeeded()
        XCTAssertEqual(badge()?.isHidden, false)
        XCTAssertEqual(badge()?.toolTip, "同一对话同时在 2 个终端打开", "原在整行的悬停提示里（2026-10-30 移到徽标上）")
        let labels = cell.descendants.compactMap { $0 as? NSTextField }
        for text in ["Waiting", "19:59"] {
            let l = try XCTUnwrap(labels.first { $0.stringValue == text })
            XCTAssertGreaterThanOrEqual(l.frame.width, l.intrinsicContentSize.width - 0.5, text)
        }
        let b = try XCTUnwrap(badge()), time = try XCTUnwrap(labels.first { $0.stringValue == "19:59" })
        XCTAssertLessThanOrEqual(b.convert(b.bounds, to: cell).maxX, time.convert(time.bounds, to: cell).minX, "在时间左侧")
    }

    /// 「目录无权限」（0.3.5 起从窄标题栏移到终端行）：只在后台进不了启动目录时显示，在第 2 行时间左侧，悬停写明处理方法；
    /// 与「冲突」「需确认」同时出现时，最小宽度下状态词与时间仍完整。
    func testCwdDeniedBadge() throws {
        let width = ScyllaUIStyle.sidebarMinWidth - 6 - 28 - 16
        let cell = TaskCell(frame: NSRect(x: 0, y: 0, width: width, height: ScyllaUIStyle.taskRowHeight))
        func badge() -> BadgeView? { cell.descendants.first { ($0 as? BadgeView)?.text == "目录无权限" } as? BadgeView }
        var row = TaskRow(lamp: .waiting, tool: "Codex", toolKey: "codex", state: "Waiting", needsConfirm: true,
                          unseen: false, subtitle: "修导航焦点样式，顺便检查键盘焦点的顺序", time: "19:59")
        cell.configure(.terminal(row))
        XCTAssertEqual(badge()?.isHidden, true)
        row.cwdDenied = true
        row.conflictTerminals = 2
        cell.configure(.terminal(row))
        cell.layoutSubtreeIfNeeded()
        let b = try XCTUnwrap(badge())
        XCTAssertFalse(b.isHidden)
        XCTAssertTrue(b.toolTip?.contains("文件与文件夹") == true)
        let labels = cell.descendants.compactMap { $0 as? NSTextField }
        for text in ["Waiting", "19:59"] {
            let l = try XCTUnwrap(labels.first { $0.stringValue == text })
            XCTAssertGreaterThanOrEqual(l.frame.width, l.intrinsicContentSize.width - 0.5, text)
        }
        let time = try XCTUnwrap(labels.first { $0.stringValue == "19:59" })
        XCTAssertLessThanOrEqual(b.convert(b.bounds, to: cell).maxX, time.convert(time.bounds, to: cell).minX, "在时间左侧")
        // 启动目录不存在（`cwd_missing`，2026-11-13）：同一位置换成「目录不存在」，悬停说已在主目录打开，不提授权。
        row.cwdDenied = false
        row.cwdMissing = true
        cell.configure(.terminal(row))
        XCTAssertNil(badge(), "不再显示「目录无权限」")
        let missing = try XCTUnwrap(cell.descendants.first { ($0 as? BadgeView)?.text == "目录不存在" } as? BadgeView)
        XCTAssertFalse(missing.isHidden)
        XCTAssertTrue(missing.toolTip?.contains("主目录") == true)
        XCTAssertFalse(missing.toolTip?.contains("文件与文件夹") == true)
        row.cwdMissing = false
        cell.configure(.terminal(row))
        XCTAssertTrue(missing.isHidden, "提示清除后隐藏")
    }
}

extension SidebarCellsTests {
    /// 副标题最多两行，超出在末尾加「…」；行高按两行固定（M1 第 6 块）。
    func testSubtitleWrapsToTwoLines() {
        let width = ScyllaUIStyle.sidebarMinWidth - 6 - 28 - 16
        let cell = TaskCell(frame: NSRect(x: 0, y: 0, width: width, height: ScyllaUIStyle.taskRowHeight))
        let long = String(repeating: "主人，查证完成，结论如下：确有疑似事故，但不是外界传言的那样。", count: 4)
        cell.configure(.conversation(TaskRow(lamp: .idle, tool: "Kimi", toolKey: "kimi", state: "Stopped", needsConfirm: false,
                                             unseen: false, subtitle: long, time: "10月12日")))
        cell.layoutSubtreeIfNeeded()
        let sub = cell.subviews.compactMap { $0 as? NSTextField }.first { $0.stringValue == long }!
        XCTAssertEqual(sub.maximumNumberOfLines, 2)
        XCTAssertEqual(sub.frame.height, ScyllaUIStyle.subtitleLineHeight * 2, accuracy: 0.5)
        XCTAssertGreaterThanOrEqual(sub.frame.minY, ScyllaUIStyle.taskRowInset - 0.5, "两行都在行内")
        XCTAssertLessThanOrEqual(sub.frame.maxX, width - 10, "不压到时间")
        let time = cell.descendants.compactMap { $0 as? NSTextField }.first { $0.stringValue == "10月12日" }!
        XCTAssertGreaterThanOrEqual(time.frame.width, time.intrinsicContentSize.width - 0.5, "时间完整显示")
    }

    /// 悬停只给被截断的文字的全文（2026-10-30 用户要求）：副标题、名称各自看，完整显示的不弹；整行不再有悬停提示；
    /// `note`（ScyllaUI 读不到的内容）总是附上。目录副标题单行，截断时给完整路径。
    func testToolTipOnlyWhenTruncated() throws {
        let width = ScyllaUIStyle.sidebarMinWidth - 6 - 28 - 16
        let cell = TaskCell(frame: NSRect(x: 0, y: 0, width: width, height: ScyllaUIStyle.taskRowHeight))
        func field(_ text: String) throws -> NSTextField { try XCTUnwrap(cell.descendants.compactMap { $0 as? NSTextField }.first { $0.stringValue == text }) }
        let long = String(repeating: "把侧栏的悬停提示改成只显示被截断的全文，", count: 4)
        var row = TaskRow(lamp: .ready, tool: "Kimi", toolKey: "kimi", state: "Ready", needsConfirm: false, unseen: false,
                          subtitle: long, time: "21:55", fullText: long.replacingOccurrences(of: "，把", with: "，\n把"))
        cell.configure(.terminal(row))
        cell.layoutSubtreeIfNeeded()
        XCTAssertNil(cell.toolTip, "整行不再有悬停提示")
        XCTAssertEqual(try field(long).toolTip, row.fullText, "截断时给全文，保留换行")
        XCTAssertNil(try field("Kimi").toolTip, "名称完整显示：不弹")
        row.subtitle = "短提示词"
        row.fullText = nil
        cell.configure(.terminal(row))
        cell.layoutSubtreeIfNeeded()
        XCTAssertNil(try field("短提示词").toolTip, "完整显示：不弹")
        row.note = "ScyllaUI 暂不读取 OpenCode 的回复"
        cell.configure(.conversation(row))
        cell.layoutSubtreeIfNeeded()
        XCTAssertEqual(try field("短提示词").toolTip, "ScyllaUI 暂不读取 OpenCode 的回复", "没截断也写明读不到的内容")
        row.note = nil
        row.name = String(repeating: "很长的终端名称", count: 6)
        cell.configure(.terminal(row))
        cell.layoutSubtreeIfNeeded()
        XCTAssertEqual(try field(row.name!).toolTip, row.name, "名称被截断：给全名")
        let dir = "/Users/example/Documents/ai-workspace/projects/06-project-agent-terminal/src/app/ScyllaUI/Sidebar"
        row = TaskRow(lamp: .idle, tool: "Shell", state: nil, needsConfirm: false, unseen: false, subtitle: dir, time: "22:00", fullText: dir)
        row.subtitleIsPath = true
        cell.configure(.terminal(row))
        cell.layoutSubtreeIfNeeded()
        XCTAssertEqual(try field(dir).toolTip, dir, "目录单行截掉开头：给完整路径")
    }
}

private extension NSView {
    var descendants: [NSView] { subviews + subviews.flatMap(\.descendants) }
}
