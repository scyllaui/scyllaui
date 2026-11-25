// 右侧栏的单测（docs/design.md 5.12 节）：四栏互斥、状态栏文案、终端行带出的路径与分支。

import AppKit
import XCTest

final class RightPanelTests: XCTestCase {
    func testPanelsAreMutuallyExclusive() {
        XCTAssertEqual(RightPanel.toggled(nil, .status), .status, "关着时按哪个开哪个")
        XCTAssertNil(RightPanel.toggled(.status, .status), "再按一次同一个为关")
        XCTAssertEqual(RightPanel.toggled(.history, .diff), .diff, "按别的直接换过去，不先关")
        for a in RightPanel.allCases {
            for b in RightPanel.allCases {
                XCTAssertEqual(RightPanel.toggled(a, b), a == b ? nil : b)
            }
        }
    }

    func testStatusPanelTexts() {
        let p = StatusPanel(frame: NSRect(x: 0, y: 0, width: RightPanelHost.defaultWidth, height: 600))
        XCTAssertEqual(p.visibleTexts, [StatusPanel.noTerminal], "没有终端时整栏只写一句")

        p.subject = .init(tool: nil, path: "/tmp/demo", branch: nil)
        XCTAssertEqual(p.visibleTexts, ["环境", "/tmp/demo", StatusPanel.notInRepo, "上下文", StatusPanel.shellNote],
                       "Shell：路径、不在仓库、上下文写一句")

        p.subject = .init(tool: "codex", path: "/w/p", branch: "main")
        XCTAssertEqual(p.visibleTexts, ["环境", "/w/p", "main", "上下文", "ScyllaUI 暂不读取 Codex 的上下文用量"],
                       "只有环境与上下文两段；读不到的写明暂不读取（ui-methodology 1.7 节）")
    }

    /// 第 6 版 R3：「上下文」下面的「更改」一段：各状态的文字与顺序、Shell 也显示、不在仓库不显示、点行打开「更改」栏。
    func testChangesSection() throws {
        let p = StatusPanel(frame: NSRect(x: 0, y: 0, width: RightPanelHost.defaultWidth, height: 600))
        let head = ["环境", "/w/p", "main", "上下文", "ScyllaUI 暂不读取 Codex 的上下文用量", "更改"]
        p.subject = .init(tool: "codex", path: "/w/p", branch: "main", changes: .loading)
        XCTAssertEqual(p.visibleTexts, head + ["正在读取更改…"])
        p.subject = .init(tool: "codex", path: "/w/p", branch: "main", changes: .counts(added: 491, deleted: 70, files: 12, more: 0))
        XCTAssertEqual(p.visibleTexts, head + ["+491 −70 · 12 个文件"])
        p.subject = .init(tool: "codex", path: "/w/p", branch: "main", changes: .counts(added: 0, deleted: 0, files: 0, more: 0))
        XCTAssertEqual(p.visibleTexts, head + ["没有更改"])
        p.subject = .init(tool: "codex", path: "/w/p", branch: "main", changes: .failed("no_git"))
        XCTAssertEqual(p.visibleTexts, head + ["看更改需要 Xcode 命令行工具"], "读不到用「更改」栏同一套原因")
        XCTAssertEqual(StatusPanel.changesText(.failed("timeout")).text, DiffPanel.failure("timeout"))
        XCTAssertEqual(StatusPanel.changesText(.failed("unsafe_filter")).text, DiffPanel.failure("unsafe_filter"))
        XCTAssertEqual(StatusPanel.changesText(.counts(added: 3, deleted: 1, files: 500, more: 20)).text, " · 500 个文件 · 还有 20 项未列出")
        p.subject = .init(tool: nil, path: "/w/p", branch: "main", changes: .counts(added: 1, deleted: 0, files: 1, more: 0))
        XCTAssertEqual(p.visibleTexts, ["环境", "/w/p", "main", "上下文", StatusPanel.shellNote, "更改", "+1 −0 · 1 个文件"], "Shell 终端也显示")
        p.subject = .init(tool: nil, path: "/tmp", branch: nil)
        XCTAssertFalse(p.visibleTexts.contains("更改"), "不在 Git 仓库里不显示这一段")
        var opened = 0
        p.onOpenChanges = { opened += 1 }
        p.subject = .init(tool: "codex", path: "/w/p", branch: "main", changes: .loading)
        p.clickChangesForTesting()
        XCTAssertEqual(opened, 1, "点行打开「更改」栏")

        // 与「更改」栏同一份数据：合计取列出的各项之和（同栏头），读不到取错误码。
        let snap = ChangesStore.parse(["ok": true, "items": [["path": "a", "st": "M", "add": 3, "del": 1], ["path": "b", "st": "A", "add": 2, "del": 0],
                                                            ["path": "c.png", "st": "M", "binary": true]], "more": 0])
        XCTAssertEqual(StatusPanel.Changes(snap), .counts(added: 5, deleted: 1, files: 3, more: 0))
        XCTAssertEqual(StatusPanel.Changes(nil), .loading)
        XCTAssertEqual(StatusPanel.Changes(ChangesStore.parse(["ok": false, "error": ["code": "timeout"]])), .failed("timeout"))

        // 英文：1 file / 2 files。
        let url = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().appendingPathComponent("ScyllaUI/Localizable.xcstrings")
        let catalog = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let plural = try XCTUnwrap(((((catalog["strings"] as? [String: Any])?["%lld 个文件"] as? [String: Any])?["localizations"] as? [String: Any])?["en"] as? [String: Any])
            .flatMap { ($0["variations"] as? [String: Any])?["plural"] as? [String: Any] })
        let one = ((plural["one"] as? [String: Any])?["stringUnit"] as? [String: Any])?["value"] as? String
        let other = ((plural["other"] as? [String: Any])?["stringUnit"] as? [String: Any])?["value"] as? String
        XCTAssertEqual(one.map { String(format: $0, 1) }, "1 file")
        XCTAssertEqual(other.map { String(format: $0, 2) }, "2 files")
    }

    /// 上下文占用（M4）：有数据显示百分比与「已用 / 上限」；读得到但还没读到、读不到各写一句。
    func testContextSection() {
        let p = StatusPanel(frame: NSRect(x: 0, y: 0, width: RightPanelHost.defaultWidth, height: 600))
        p.subject = .init(tool: "claude", path: "/w", branch: "main", contextUsed: 40_000, contextLimit: 200_000, contextSupported: true)
        XCTAssertTrue(p.visibleTexts.contains("20%"))
        XCTAssertTrue(p.visibleTexts.contains("已用 40.0K / 200.0K tokens"))
        p.subject = .init(tool: "codex", path: "/w", branch: "main", contextSupported: true)
        XCTAssertTrue(p.visibleTexts.contains(StatusPanel.contextPending(tool: "codex")))
        p.subject = .init(tool: "kimi", path: "/w", branch: "main", contextSupported: false)
        XCTAssertTrue(p.visibleTexts.contains("ScyllaUI 暂不读取 Kimi Code 的上下文用量"))
        // 经额度桥读的工具，桥没接上时写原因与怎么接，不写「还没读到」；接上了、或这款没有桥时为 nil。
        let note = StatusPanel.bridgeNote(bridge: "off", tool: "claude")
        XCTAssertEqual(note?.contains("接入额度…"), true)
        p.subject = .init(tool: "claude", path: "/w", branch: "main", contextSupported: true, contextNote: note)
        XCTAssertTrue(p.visibleTexts.contains(note!))
        XCTAssertFalse(p.visibleTexts.contains(StatusPanel.contextPending(tool: "claude")))
        XCTAssertEqual(StatusPanel.bridgeNote(bridge: "replaced", tool: "claude")?.contains("修复"), true)
        XCTAssertNil(StatusPanel.bridgeNote(bridge: "on", tool: "claude"))
        XCTAssertNil(StatusPanel.bridgeNote(bridge: nil, tool: "codex"))
        XCTAssertEqual(StatusPanel.percent(used: 300, limit: 200), 100, "超过上限按 100%")
        let u = SBUsage(json: ["context": ["used": 5, "limit": 10]])
        XCTAssertEqual([u.contextUsed, u.contextLimit], [5, 10])
        XCTAssertNil(SBUsage(json: ["context": ["used": 5, "limit": 0]]).contextLimit, "上限为 0 不算")
    }

    func testTerminalRowCarriesDirAndBranch() {
        let m = SidebarModel()
        m.apply(["type": "snapshot", "rev": 1, "recent_total": 0,
                 "projects": [["project_id": 1, "dir": "/w/p", "name": "p", "sort_order": 0, "branch": "main"],
                              ["project_id": 2, "dir": "/tmp/x", "name": "x", "sort_order": 1]],
                 "terminals": [
                    ["terminal_id": "a", "project_id": 1, "cwd": "/w/p", "current_dir": "/w/p/src", "status": "alive",
                     "created_at": 0, "unseen": false, "agent": ["state": "unknown"], "foreground": ["kind": "shell"]],
                    ["terminal_id": "b", "project_id": 2, "cwd": "/tmp/x", "status": "alive",
                     "created_at": 0, "unseen": false, "agent": ["state": "unknown"], "foreground": ["kind": "shell"]]]])
        let tree = m.buildTree(now: Date(), calendar: Calendar(identifier: .gregorian))
        func row(_ key: String) -> TaskRow? {
            if case .terminal(let r)? = tree.rows[key] { return r }
            return nil
        }
        XCTAssertEqual(row("t:a")?.currentDir, "/w/p/src", "当前目录优先于启动目录")
        XCTAssertEqual(row("t:a")?.projectDir, "/w/p")
        XCTAssertEqual(row("t:a")?.branch, "main")
        XCTAssertEqual(row("t:b")?.currentDir, "/tmp/x", "没上报当前目录时为启动目录")
        XCTAssertNil(row("t:b")?.branch, "不是仓库没有分支")
    }
    /// 双击分隔线复位后蓝线不灭（2026-11-25 方案会话验收 D2 时发现）：双击改了宽度、还没重新布局时松手，按旧位置算成「鼠标在热区上」，
    /// 布局后热区移走，再也收不到离开事件。松手时要按布局后的位置判断。右侧栏左边缘、文件栏与 Diff 栏的分隔线都是这个 `EdgeHandle`。
    func testDoubleClickResetDoesNotLeaveHover() {
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 400, height: 200), styleMask: [.titled], backing: .buffered, defer: false)
        let c = NSView()
        w.contentView = c
        let h = EdgeHandle()
        h.translatesAutoresizingMaskIntoConstraints = false
        c.addSubview(h)
        let x = h.leadingAnchor.constraint(equalTo: c.leadingAnchor, constant: 100)
        NSLayoutConstraint.activate([x, h.topAnchor.constraint(equalTo: c.topAnchor), h.bottomAnchor.constraint(equalTo: c.bottomAnchor),
                                     h.widthAnchor.constraint(equalToConstant: 5)])
        w.layoutIfNeeded()
        var hover: [Bool] = []
        h.onHover = { hover.append($0) }
        h.onDoubleClick = { x.constant = 300 }
        func event(_ type: NSEvent.EventType, clicks: Int) -> NSEvent {
            NSEvent.mouseEvent(with: type, location: NSPoint(x: 102, y: 100), modifierFlags: [], timestamp: 0, windowNumber: w.windowNumber,
                               context: nil, eventNumber: 0, clickCount: clicks, pressure: 1)!
        }
        h.mouseDown(with: event(.leftMouseDown, clicks: 2))
        h.mouseUp(with: event(.leftMouseUp, clicks: 2))
        XCTAssertEqual(hover.last, false, "复位后热区已离开鼠标，不能算悬停")
        x.constant = 100
        w.layoutIfNeeded()
        h.mouseDown(with: event(.leftMouseDown, clicks: 1))
        h.mouseUp(with: event(.leftMouseUp, clicks: 1))
        XCTAssertEqual(hover.last, true, "单击松手时鼠标仍在热区上")
    }
}
