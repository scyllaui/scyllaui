// 历史提示词面板的单测（M4a 追加块 9，docs/design.md 10.5 节决策 10）：分段、「更早的未显示」与空状态。

import AppKit
import XCTest

final class PromptHistoryPanelTests: XCTestCase {
    private func reply(_ items: [[String: Any]], truncated: Bool = false, available: Bool = true) -> [String: Any] {
        let ids = Set(items.compactMap { $0["conversation_id"] as? Int })
        return ["ok": true, "truncated": truncated, "items": items,
                "segments": (ids.isEmpty ? [1] : ids.sorted()).map { ["conversation_id": $0, "available": available] }]
    }

    func testSegmentsAndTruncation() {
        let p = PromptHistoryPanel(frame: NSRect(x: 0, y: 0, width: PromptHistoryPanel.defaultWidth, height: 600))
        let t = NSTableView()
        p.show(reply: reply([["conversation_id": 1, "text": "第一段", "at": 1_792_856_847_912],
                             ["conversation_id": 2, "text": "修复登录", "at": NSNull()],
                             ["conversation_id": 2, "text": "再跑测试"]], truncated: true))
        XCTAssertEqual(p.numberOfRows(in: t), 5, "更早未显示 + 第一段 + 恢复后的会话 + 两条")
        p.show(reply: reply([["conversation_id": 1, "text": "只有一条"]]))
        XCTAssertEqual(p.numberOfRows(in: t), 1)
    }

    func testEmptyStates() {
        let p = PromptHistoryPanel(frame: NSRect(x: 0, y: 0, width: PromptHistoryPanel.defaultWidth, height: 600))
        let t = NSTableView()
        func shows(_ text: String) -> Bool {
            p.subviews.compactMap { $0 as? NSTextField }.contains { !$0.isHidden && $0.stringValue == text }
        }
        p.show(reply: reply([], available: false))
        XCTAssertEqual(p.numberOfRows(in: t), 0)
        XCTAssertTrue(shows("会话记录已不在，读不到提示词"))
        p.showMessage("这个终端还没有 AI 对话")
        XCTAssertTrue(shows("这个终端还没有 AI 对话"))
        XCTAssertFalse(shows("会话记录已不在，读不到提示词"))
        // 没有对话时按场景说明（2026-10-30 用户要求）。
        XCTAssertEqual(PromptHistoryPanel.noConversationMessage(tool: nil, unsynced: false, command: nil),
                       "当前是 Shell，在这个终端启动 Agent 后，这里显示它的历史提示词。")
        XCTAssertEqual(PromptHistoryPanel.noConversationMessage(tool: "opencode", unsynced: false, command: nil),
                       "还没对上 OpenCode 的这次会话，发出一条提示词后显示。", "AI 在运行、还没收到这次会话的事件")
        XCTAssertEqual(PromptHistoryPanel.noConversationMessage(tool: "claude", unsynced: true, command: nil),
                       "Claude Code 的状态同步未生效，对不上它的会话；点侧栏这一行的「未同步」修复。", "发提示词也对不上时不说「发出一条」")
        XCTAssertEqual(PromptHistoryPanel.noConversationMessage(tool: nil, unsynced: false, command: "agy"),
                       "前台运行的 agy 不是 ScyllaUI 已接入的 Agent，没有历史提示词。", "不认识的命令不说成 Shell")
        // 不支持历史提示词的工具：写明不支持，不借用「记录已不在」（docs/ui-methodology.md 1.7 节）。
        p.show(reply: ["ok": true, "truncated": false, "items": [],
                       "segments": [["conversation_id": 3, "tool": "opencode", "available": false, "supported": false]]])
        XCTAssertTrue(shows("OpenCode 暂不支持历史提示词"))
        XCTAssertFalse(shows("会话记录已不在，读不到提示词"))
        p.show(reply: ["ok": true, "truncated": false, "items": [],
                       "segments": [["conversation_id": 4, "tool": "opencode", "available": false, "supported": true, "error": true]]])
        XCTAssertTrue(shows("读取 OpenCode 的会话记录失败，可能是它升级后格式变了；详情见日志文件夹里的 scyllauid.log"))
    }

    /// 2026-10-25 改版（docs/design.md 10.5 节决策 19）：栏头为工具全名 · 已结束 · 最新一段的会话 ID；最新在上、编号最早为 1；
    /// 分段说明带更早那段的会话 ID；筛选计数「符合 / 全部」；长提示词收起时最多两行，点箭头展开全文；分段说明不能选中。
    func testHeaderOrderCountAndExpand() throws {
        let p = PromptHistoryPanel(frame: NSRect(x: 0, y: 0, width: PromptHistoryPanel.defaultWidth, height: 600))
        let t = NSTableView()
        let labels = { p.subviews.compactMap { $0 as? NSTextField }.filter { !$0.isHidden }.map(\.stringValue) }
        func texts(_ row: Int) -> [String] {
            (p.tableView(t, viewFor: nil, row: row)?.subviews ?? []).compactMap { ($0 as? NSTextField)?.stringValue }
        }
        p.subjectTool = "codex"
        p.subjectEnded = true
        let long = String(repeating: "把侧栏的副标题改成相对项目根的路径，", count: 8)
        p.show(reply: ["ok": true, "truncated": false,
                       "items": [["conversation_id": 1, "text": "第一问：什么是 PTY"], ["conversation_id": 1, "text": long],
                                 ["conversation_id": 2, "text": "恢复后：说明 PTY"]],
                       "segments": [["conversation_id": 1, "available": true, "native_id": "019a-old"],
                                    ["conversation_id": 2, "available": true, "native_id": "019a-new-session"]]])
        XCTAssertTrue(labels().contains("Codex · 已结束"))
        XCTAssertTrue(labels().contains("019a-new-session"), "最新一段的完整会话 ID")
        XCTAssertEqual(p.numberOfRows(in: t), 4, "三条 + 分段说明")
        XCTAssertTrue(texts(0).contains("3") && texts(0).contains("恢复后：说明 PTY"), "最新在上，编号最早为 1")
        XCTAssertTrue(texts(1).contains { $0.contains("019a-old") }, "分段说明带更早那段的会话 ID")
        XCTAssertTrue(texts(3).contains("1"))
        XCTAssertFalse(p.tableView(t, shouldSelectRow: 1), "分段说明不能选中")
        let filter = try XCTUnwrap(p.subviews.compactMap { $0 as? NSSearchField }.first)
        filter.stringValue = "PTY"
        filter.sendAction(filter.action, to: filter.target)
        XCTAssertTrue(labels().contains("2 / 3"), "筛选时显示「符合 / 全部」")
        filter.stringValue = ""
        filter.sendAction(filter.action, to: filter.target)
        XCTAssertTrue(labels().contains("3"))
        let line = NSLayoutManager().defaultLineHeight(for: ScyllaUIStyle.detailFont)
        let short = try XCTUnwrap(p.tableView(t, viewFor: nil, row: 3)).fittingSize.height
        let cell = try XCTUnwrap(p.tableView(t, viewFor: nil, row: 2))
        let collapsed = cell.fittingSize.height
        XCTAssertGreaterThan(collapsed, short + line / 2, "收起时两行")
        XCTAssertLessThan(collapsed, short + line * 1.5, "收起时最多两行")
        let chevron = try XCTUnwrap(cell.subviews.compactMap { $0 as? NSButton }.first { !$0.isHidden && $0.toolTip == "展开" })
        chevron.performClick(nil)
        let expanded = try XCTUnwrap(p.tableView(t, viewFor: nil, row: 2)).fittingSize.height
        XCTAssertGreaterThan(expanded, collapsed + line, "展开显示全文")
        XCTAssertTrue(try XCTUnwrap(p.tableView(t, viewFor: nil, row: 3)).subviews.compactMap { $0 as? NSButton }
            .allSatisfy { $0.toolTip != "展开" || $0.isHidden }, "不超过两行的没有箭头")
    }

    func testShortTime() {
        var cal = Calendar(identifier: .gregorian)
        cal.timeZone = TimeZone(identifier: "Asia/Shanghai")!
        let now = DateComponents(calendar: cal, year: 2026, month: 10, day: 25, hour: 13).date!
        let ms = { (d: Int, h: Int) in Int64(DateComponents(calendar: cal, year: 2026, month: 10, day: d, hour: h).date!.timeIntervalSince1970 * 1000) }
        XCTAssertTrue(PromptTime.short(ms(25, 9), now: now, calendar: cal).hasPrefix("今天 "))
        XCTAssertTrue(PromptTime.short(ms(24, 23), now: now, calendar: cal).hasPrefix("昨天 "))
        XCTAssertFalse(PromptTime.short(ms(20, 9), now: now, calendar: cal).hasPrefix("今天"))
    }
}
