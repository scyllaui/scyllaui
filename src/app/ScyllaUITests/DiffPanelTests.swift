// 右侧栏「Diff」与内容搜索的单测（docs/design.md 5.12 节）：改动列表解析与文件树标记、选中第一个、读不到的说明、
// diff 转行、`?` 搜内容按文件分组。

import AppKit
import XCTest

final class DiffPanelTests: XCTestCase {
    private static let asyncBuild = DiffViewer.buildWork

    /// 正文在后台搭（D4）；测试里同步做，应答一到就能看结果。
    override func setUp() {
        super.setUp()
        DiffViewer.buildWork = { work, done in done(work()) }
    }

    override func tearDown() {
        DiffViewer.buildWork = Self.asyncBuild
        super.tearDown()
    }

    private let reply: [String: Any] = ["ok": true, "base": "HEAD", "more": 0, "truncated": false, "items": [
        ["path": "src/a.swift", "st": "M", "add": 3, "del": 1, "binary": false, "untracked": false],
        ["path": "new.md", "st": "A", "add": 2, "del": 0, "binary": false, "untracked": true],
        ["path": "gone.txt", "st": "D", "add": 0, "del": 4, "binary": false, "untracked": false],
        ["path": "img.png", "st": "M", "add": NSNull(), "del": NSNull(), "binary": true, "untracked": false],
    ]]

    func testStoreParsesAndMarks() {
        let snap = ChangesStore.parse(reply)
        XCTAssertEqual(snap.items.count, 4)
        XCTAssertEqual(snap.totals.added, 5)
        XCTAssertEqual(snap.totals.deleted, 5)
        XCTAssertEqual(ChangesStore.parse(["ok": false, "error": ["code": "not_repo"]]).error, "not_repo")
        XCTAssertEqual(ChangesStore.parse(nil).error, "unavailable", "断线也有说明")
        let store = ChangesStore()
        store.request = { _, done in done(self.reply) }
        var updated: [String] = []
        store.onUpdate = { updated.append($0) }
        store.refresh(root: "/p")
        store.refresh(root: "/p")
        XCTAssertEqual(updated, ["/p"], "结果没变不再通知")
        XCTAssertEqual(store.marks("/p"), ["src/a.swift": "M", "new.md": "A", "img.png": "M"], "删除的不在树上")
        XCTAssertEqual(store.dirtyDirs("/p"), ["src"])
    }

    func testPanelSelectsFirstAndShowsDiff() {
        var asked: [String] = []
        let panel = DiffPanel(frame: NSRect(x: 0, y: 0, width: 900, height: 600))
        panel.request = { req, done in
            asked.append(req["path"] as? String ?? "")
            done(["ok": true, "binary": false, "truncated": false, "hunks": [
                ["head": "@@ -1,2 +1,2 @@", "lines": [[" ", 1, 1, "a"], ["-", 2, NSNull(), "b"], ["+", NSNull(), 2, "c"]]]]])
        }
        panel.setRoot("/p", inRepo: true, branch: "main")
        XCTAssertEqual(panel.subtitleText, "main → 工作树")
        panel.update(ChangesStore.parse(reply))
        XCTAssertEqual(panel.listRows, ["src/ +3 −1", "  M a.swift", "D gone.txt", "M img.png", "A new.md"], "按文件夹分组（D3）")
        XCTAssertEqual(panel.selected?.path, "src/a.swift", "打开时选中第一个")
        XCTAssertTrue(panel.viewerOpen)
        XCTAssertEqual(asked, ["src/a.swift"])
        XCTAssertEqual(panel.viewer.rows, [.hunk("@@ -1,2 +1,2 @@"), .line(sign: " ", old: 1, new: 1, text: "a"),
                                           .line(sign: "-", old: 2, new: nil, text: "b"), .line(sign: "+", old: nil, new: 2, text: "c")])
        XCTAssertTrue(panel.subtitleText.hasSuffix(" · +5 −5"), "栏头带合计")
        panel.update(ChangesStore.parse(["ok": true, "items": [], "more": 0]))
        XCTAssertEqual(panel.noteText, "工作树与 HEAD 一致，没有更改")
        XCTAssertFalse(panel.viewerOpen)
        panel.update(ChangesStore.parse(["ok": false, "error": ["code": "unsafe_filter"]]))
        XCTAssertEqual(panel.noteText, "这个仓库配置了 ScyllaUI 不执行的过滤器，不读更改")
        panel.setRoot("/tmp/x", inRepo: false, branch: nil)
        XCTAssertTrue(panel.noteText?.hasPrefix("这个目录不在 Git 仓库里") == true)
    }

    func testDiffViewerNotes() {
        let v = DiffViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        v.begin(item: ChangesStore.Item(path: "img.png", status: "M", added: nil, deleted: nil, binary: true, untracked: false), root: "/p")
        v.show(reply: ["ok": true, "binary": true, "truncated": false, "hunks": []], keepScroll: false)
        XCTAssertEqual(v.noteText, "二进制文件，不显示更改")
        v.show(reply: ["ok": false, "error": ["code": "timeout"]], keepScroll: false)
        XCTAssertEqual(v.noteText, "读更改超时（超过 3 秒），文件再有变化时重试")
    }

    /// D1：栏头的关闭先关改动、回到只有列表，再关栏；关掉的改动不会被自动刷新再打开。放大只在打开着改动时有。
    func testHeaderCloseClosesChangeFirst() {
        let panel = DiffPanel(frame: NSRect(x: 0, y: 0, width: 900, height: 600))
        panel.request = { _, done in done(["ok": true, "binary": false, "truncated": false, "hunks": []]) }
        var closed = 0
        panel.onClose = { closed += 1 }
        panel.setRoot("/p", inRepo: true, branch: "main")
        panel.update(ChangesStore.parse(reply))
        XCTAssertEqual(panel.subtitleText, "main → 工作树 · +5 −5")
        XCTAssertTrue(panel.viewerOpen)
        XCTAssertEqual(panel.closeName, "关闭这个文件的更改", "第 6 版 R2：两段不同名")
        XCTAssertTrue(panel.zoomShown)
        panel.closeFromHeaderForTesting()
        XCTAssertFalse(panel.viewerOpen, "先关改动")
        XCTAssertEqual(closed, 0, "栏还开着")
        XCTAssertEqual(panel.closeName, "关闭更改")
        XCTAssertFalse(panel.zoomShown)
        panel.update(ChangesStore.parse(reply))
        XCTAssertFalse(panel.viewerOpen, "自动刷新不把关掉的改动再打开")
        panel.closeFromHeaderForTesting()
        XCTAssertEqual(closed, 1, "没有打开的改动时关栏")
    }

    /// D1：改动标题一行里的元信息与「…」菜单；已删除的文件只能拷贝相对路径。
    func testViewerTitleAndMenu() {
        let items = ChangesStore.parse(reply).items
        func meta(_ path: String, truncated: Bool = false) -> String {
            DiffViewer.metaText(items.first { $0.path == path }!, truncated: truncated)
        }
        XCTAssertEqual(meta("src/a.swift"), "src · +3 −1")
        XCTAssertEqual(meta("new.md"), "+2 −0 · 未跟踪的新文件")
        XCTAssertEqual(meta("gone.txt"), "+0 −4 · 文件已删除")
        XCTAssertEqual(meta("img.png"), "二进制")
        XCTAssertEqual(meta("src/a.swift", truncated: true), "src · +3 −1 · 更改太大，只显示前 2 MB")
        let v = DiffViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        v.begin(item: items.first { $0.path == "src/a.swift" }!, root: "/p")
        v.show(reply: ["ok": true, "binary": false, "truncated": true, "hunks": [["head": "@@ -1 +1 @@", "lines": [["+", NSNull(), 1, "x"]]]]], keepScroll: false)
        XCTAssertTrue(v.metaTextForTesting.hasSuffix("更改太大，只显示前 2 MB"), "截断写在标题里")
        XCTAssertEqual(v.moreMenu().items.map(\.title), ["拷贝相对路径", "打开方式", "在访达中显示"], "第 6 版 R1 起折行在标题行的按钮上")
        XCTAssertTrue(v.moreMenu().items.allSatisfy(\.isEnabled))
        XCTAssertTrue(v.openInFilesEnabled)
        v.begin(item: items.first { $0.path == "gone.txt" }!, root: "/p")
        XCTAssertEqual(v.metaTextForTesting, "+0 −4 · 文件已删除", "换文件后不留上一个的截断说明")
        XCTAssertEqual(v.moreMenu().items.map(\.isEnabled).suffix(3), [true, false, false], "已删除：打开方式、在访达中显示置灰")
        XCTAssertFalse(v.openInFilesEnabled, "已删除：在「文件」里打开置灰")
    }

    /// D2：改动与列表之间的分隔线同文件栏 F3（同一份 `ListSplit`）：列表拖到 160 以下隐藏，同一次拖回来出现；双击回到 280；
    /// 松手、双击都把列表宽交给工作区（与文件树宽同一个值）。
    func testSplitDragHidesListAndResets() {
        let panel = DiffPanel(frame: NSRect(x: 0, y: 0, width: 900, height: 600))
        panel.request = { _, done in done(["ok": true, "binary": false, "truncated": false, "hunks": []]) }
        var saved: [CGFloat] = []
        panel.onListWidth = { saved.append($0) }
        panel.setRoot("/p", inRepo: true, branch: "main")
        panel.update(ChangesStore.parse(reply))
        panel.layoutSubtreeIfNeeded()
        XCTAssertEqual(panel.listColumnWidthForTesting, 280, accuracy: 0.5, "默认列表宽同文件树")
        panel.dragSplitForTesting(130, done: false)
        XCTAssertTrue(panel.listHidden, "窄到 160 以下列表隐藏")
        panel.dragSplitForTesting(-80, done: false)
        XCTAssertFalse(panel.listHidden, "同一次拖动里拖回来列表出现")
        XCTAssertEqual(panel.listWidth, 230, accuracy: 0.5)
        panel.dragSplitForTesting(0, done: true)
        XCTAssertEqual(saved, [230])
        panel.dragSplitForTesting(-2000, done: true)
        XCTAssertEqual(panel.listWidth, 900 - 160, accuracy: 0.5, "最宽到改动剩 160")
        panel.listWidth = 200
        panel.layoutSubtreeIfNeeded()
        XCTAssertEqual(panel.listColumnWidthForTesting, 200, accuracy: 0.5, "工作区设的宽（文件栏拖过）直接生效")
    }

    /// D2：改动区收起后只剩列表、改动仍打开着；自动刷新不展开；点列表里的文件或「显示改动内容」展开；关掉改动时收起一并清掉。
    func testContentCollapseKeepsChangeOpen() {
        let panel = DiffPanel(frame: NSRect(x: 0, y: 0, width: 900, height: 600))
        panel.request = { _, done in done(["ok": true, "binary": false, "truncated": false, "hunks": []]) }
        panel.setRoot("/p", inRepo: true, branch: "main")
        panel.update(ChangesStore.parse(reply))
        XCTAssertFalse(panel.expandShown)
        panel.contentCollapsed = true
        XCTAssertTrue(panel.viewerOpen, "改动仍打开着")
        XCTAssertFalse(panel.contentShown)
        XCTAssertTrue(panel.expandShown, "栏头出现「显示更改内容」")
        XCTAssertFalse(panel.zoomShown, "收起时没有放大")
        XCTAssertTrue(panel.viewer.isHidden)
        panel.update(ChangesStore.parse(reply))
        XCTAssertTrue(panel.contentCollapsed, "自动刷新不展开")
        panel.clickRowForTesting(0)
        XCTAssertFalse(panel.contentCollapsed, "点列表里（已选中）的文件展开")
        XCTAssertFalse(panel.viewer.isHidden)
        panel.contentCollapsed = true
        panel.clickRowForTesting(1)
        XCTAssertEqual(panel.selected?.path, "gone.txt", "按分组后的列表顺序第 2 个文件（D3）")
        XCTAssertFalse(panel.contentCollapsed, "点别的文件也展开")
        panel.contentCollapsed = true
        panel.closeFromHeaderForTesting()
        XCTAssertFalse(panel.viewerOpen)
        panel.select(ChangesStore.parse(reply).items[0])
        XCTAssertTrue(panel.contentShown, "关掉改动时收起状态一并清掉")
    }

    /// D2 顺手修：换文件时正文先留空，不留上一个文件的说明（先点二进制文件再点别的，读完之前仍是「二进制文件，不显示改动」）。
    func testBeginClearsPreviousNote() {
        let items = ChangesStore.parse(reply).items
        let v = DiffViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        v.begin(item: items.first { $0.path == "img.png" }!, root: "/p")
        v.show(reply: ["ok": true, "binary": true, "truncated": false, "hunks": []], keepScroll: false)
        XCTAssertEqual(v.noteText, "二进制文件，不显示更改")
        v.begin(item: items.first { $0.path == "src/a.swift" }!, root: "/p")
        XCTAssertNil(v.noteText, "换文件后说明收起")
        XCTAssertTrue(v.rows.isEmpty)
        v.show(reply: ["ok": true, "binary": false, "truncated": false, "hunks": [["head": "@@ -1 +1 @@", "lines": [["+", NSNull(), 1, "x"]]]]], keepScroll: false)
        XCTAssertEqual(v.text, "@@ -1 +1 @@\n+ x")
        v.begin(item: items.first { $0.path == "new.md" }!, root: "/p")
        XCTAssertEqual(v.text, "", "文本视图里换文件后正文也先留空（D4）")
    }

    /// D2 顺手修：「在『文件』里打开」后文件树逐级展开并选中这个文件。
    func testOpenAndRevealSelectsInTree() {
        let fs: [String: [[String: Any]]] = [
            "/p": [["name": "src", "kind": "dir"], ["name": "a.md", "kind": "file"]],
            "/p/src": [["name": "core", "kind": "dir"]],
            "/p/src/core": [["name": "one.txt", "kind": "file"]],
        ]
        let panel = FilesPanel(frame: NSRect(x: 0, y: 0, width: 900, height: 600))
        panel.request = { req, done in
            switch req["type"] as! String {
            case "fs.list": done(["ok": true, "items": fs[req["dir"] as! String] ?? [], "more": 0])
            default: done(["ok": true, "size": 2, "binary": false, "text": "x\n", "truncated": false, "lossy": false])
            }
        }
        panel.setRoot("/p", gitRoot: "/p", currentDir: nil)
        panel.openAndReveal("/p/src/core/one.txt")
        XCTAssertEqual(panel.viewer.path, "/p/src/core/one.txt")
        XCTAssertEqual(panel.visibleRows, ["src", "  core", "    one.txt", "a.md"], "逐级展开")
        XCTAssertEqual(panel.selectedTreePathForTesting, "/p/src/core/one.txt", "选中这个文件")
    }

    /// D3：由路径搭树——文件夹在前、按 `localizedStandardCompare` 排；只套着一个子文件夹的一串合并；文件夹合计不计二进制与没数行数的。
    func testChangeTreeBuild() {
        func item(_ p: String, _ a: Int?, _ d: Int?, binary: Bool = false) -> ChangesStore.Item {
            ChangesStore.Item(path: p, status: "M", added: a, deleted: d, binary: binary, untracked: false)
        }
        let nodes = ChangeNode.build([
            item("src/core/one.txt", 2, 2), item("src/core/two.txt", 0, 3), item("src/core/deep/er/leaf.txt", 1, 0),
            item("a/b/c/only.txt", 1, 1), item("long.txt", 1, 0), item("bin.dat", nil, nil, binary: true),
            item("src/core/new.txt", nil, nil), item("file10.txt", 1, 0), item("file2.txt", 1, 0), item("pics/x.png", nil, nil, binary: true),
        ], more: 3)
        func dump(_ ns: [ChangeNode], _ pad: String = "") -> [String] {
            ns.flatMap { n -> [String] in
                switch n.kind {
                case .dir: return [pad + n.name + "/" + (n.counted ? " +\(n.added) −\(n.deleted)" : "")] + dump(n.children, pad + "  ")
                case .file: return [pad + n.name]
                case .more: return [pad + "…\(n.more)"]
                }
            }
        }
        XCTAssertEqual(dump(nodes), [
            "a/b/c/ +1 −1", "  only.txt",
            "pics/", "  x.png",
            "src/core/ +3 −5", "  deep/er/ +1 −0", "    leaf.txt", "  new.txt", "  one.txt", "  two.txt",
            "bin.dat", "file2.txt", "file10.txt", "long.txt",
            "…3",
        ])
        XCTAssertEqual(nodes[0].chain, ["a", "a/b", "a/b/c"], "合并的一串记下各级")
        XCTAssertEqual(nodes[2].path, "src/core")
        XCTAssertEqual(ChangeNode.initialFile(nodes, cwd: "src/core")?.path, "src/core/deep/er/leaf.txt", "终端目录里（含子目录）按列表顺序的第一个")
        XCTAssertEqual(ChangeNode.initialFile(nodes, cwd: "src/core/deep")?.path, "src/core/deep/er/leaf.txt")
        XCTAssertEqual(ChangeNode.initialFile(nodes, cwd: "docs")?.path, "a/b/c/only.txt", "终端目录里没有改动就选第一个")
        XCTAssertEqual(ChangeNode.initialFile(nodes, cwd: "")?.path, "a/b/c/only.txt")
        XCTAssertEqual(ChangeNode.initialFile(nodes, cwd: nil)?.path, "a/b/c/only.txt")
    }

    /// D3：收起状态、选中、终端标记与筛选。点文件夹只展开收起；自动刷新不丢收起与选中；筛过全部展开，清空后恢复。
    func testGroupedListKeepsStateAcrossRefreshAndFilter() {
        let items: [[String: Any]] = [
            ["path": "src/core/one.txt", "st": "M", "add": 2, "del": 2, "binary": false, "untracked": false],
            ["path": "src/core/two.txt", "st": "M", "add": 0, "del": 3, "binary": false, "untracked": false],
            ["path": "docs/a.md", "st": "A", "add": 1, "del": 0, "binary": false, "untracked": false],
            ["path": "top.txt", "st": "M", "add": 1, "del": 1, "binary": false, "untracked": false],
        ]
        let snap = ChangesStore.parse(["ok": true, "more": 0, "items": items])
        let panel = DiffPanel(frame: NSRect(x: 0, y: 0, width: 900, height: 600))
        var asked: [String] = []
        panel.request = { req, done in
            asked.append(req["path"] as? String ?? "")
            done(["ok": true, "binary": false, "truncated": false, "hunks": []])
        }
        panel.setRoot("/p", inRepo: true, branch: "main", currentDir: "/p/src/core")
        panel.update(snap)
        XCTAssertEqual(panel.selected?.path, "src/core/one.txt", "打开时选终端目录里的第一个改动")
        XCTAssertEqual(panel.listRows, ["docs/ +1 −0", "  A a.md", "src/core/ +2 −5 [终端]", "  M one.txt", "  M two.txt", "M top.txt"])
        panel.clickListRowForTesting("docs")
        XCTAssertEqual(panel.listRows.first, "docs/ +1 −0")
        XCTAssertFalse(panel.listRows.contains("  A a.md"), "点文件夹收起")
        XCTAssertEqual(panel.selected?.path, "src/core/one.txt", "点文件夹不打开改动")
        XCTAssertEqual(asked, ["src/core/one.txt"])
        panel.update(snap)
        XCTAssertFalse(panel.listRows.contains("  A a.md"), "自动刷新后收起状态不丢")
        XCTAssertEqual(panel.selectedListPath, "src/core/one.txt", "选中不丢")
        panel.clickEmptyListForTesting()
        XCTAssertEqual(panel.selectedListPath, "src/core/one.txt", "点空白处不清掉列表里的选中（改动仍打开着）")
        XCTAssertEqual(panel.selected?.path, "src/core/one.txt")
        panel.setFilterForTesting("a")
        XCTAssertTrue(panel.listRows.contains("  A a.md"), "筛过全部展开")
        panel.setFilterForTesting("")
        XCTAssertFalse(panel.listRows.contains("  A a.md"), "清空后恢复收起")
        panel.setRoot("/p", inRepo: true, branch: "main", currentDir: "/p")
        XCTAssertFalse(panel.listRows.contains { $0.contains("[终端]") }, "终端换目录，标记跟着换")
    }

    /// 第 6 版 R2：中文界面这一栏叫「更改」，英文界面仍叫 Diff；右侧栏里的「改动」统一成「更改」，英文不动，旧键清掉。
    func testPanelNamedChangesInChinese() throws {
        XCTAssertEqual(DiffPanel().titleText, "更改")
        let url = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent().appendingPathComponent("ScyllaUI/Localizable.xcstrings")
        let catalog = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let strings = try XCTUnwrap(catalog["strings"] as? [String: Any])
        func en(_ k: String) -> String? {
            (((strings[k] as? [String: Any])?["localizations"] as? [String: Any])?["en"] as? [String: Any])
                .flatMap { ($0["stringUnit"] as? [String: Any])?["value"] as? String }
        }
        XCTAssertEqual(en("更改"), "Diff")
        XCTAssertEqual(en("关闭更改"), "Close Diff Panel")
        XCTAssertEqual(en("关闭这个文件的更改"), "Close File Diff")
        XCTAssertEqual(en("上一处更改"), "Previous Change")
        XCTAssertEqual(en("正在读取更改…"), "Reading changes…")
        for old in ["Diff", "关闭 Diff", "关闭改动", "上一处改动", "正在读取改动…", "里面有改动"] {
            XCTAssertNil(strings[old], "旧键清掉：\(old)")
        }
    }

    // MARK: D4 正文改成文本视图

    private func hunks(_ lines: [[Any]], head: String = "@@ -1 +1 @@") -> [String: Any] {
        ["ok": true, "binary": false, "truncated": false, "hunks": [["head": head, "lines": lines]]]
    }

    private func inWindow(_ v: DiffViewer, width: CGFloat = 600, height: CGFloat = 400) -> NSWindow {
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: width, height: height), styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = v
        w.layoutIfNeeded()
        return w
    }

    /// 正文的文字就是看到的（符号、空格、内容；段头原样），不含行号；行号栏两列按位数补齐，段头一行不写；每处改动记第一行。
    func testBodyTextAndLineLabels() {
        let rows = DiffViewer.rows(from: [["head": "@@ -9,3 +9,3 @@ f", "lines": [[" ", 9, 9, "a"], ["-", 10, NSNull(), "b"], ["+", NSNull(), 10, "c"], [" ", 11, 11, "d"]]],
                                          ["head": "@@ -1200 +1200 @@", "lines": [["-", 1200, NSNull(), "e"]]]])
        let b = DiffViewer.build(rows)
        XCTAssertEqual(b.text.string, "@@ -9,3 +9,3 @@ f\n  a\n- b\n+ c\n  d\n@@ -1200 +1200 @@\n- e")
        XCTAssertEqual(b.changes, [2, 6], "连续的增删算一处")
        XCTAssertEqual(b.digits, 4)
        let v = DiffViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        v.begin(item: ChangesStore.Item(path: "a.txt", status: "M", added: 1, deleted: 2, binary: false, untracked: false), root: "/p")
        v.show(reply: ["ok": true, "binary": false, "truncated": false, "hunks": [["head": "@@ -9,3 +9,3 @@ f", "lines": [[" ", 9, 9, "a"], ["-", 10, NSNull(), "b"], ["+", NSNull(), 10, "c"], [" ", 11, 11, "d"]]],
                                                                                 ["head": "@@ -1200 +1200 @@", "lines": [["-", 1200, NSNull(), "e"]]]]], keepScroll: false)
        XCTAssertEqual(v.text, b.text.string, "拷贝出来的就是看到的文字，不含行号")
        XCTAssertEqual(v.labelForTesting(0), "", "段头不写行号")
        XCTAssertEqual(v.labelForTesting(1), "   9    9")
        XCTAssertEqual(v.labelForTesting(2), "  10     ")
        XCTAssertEqual(v.labelForTesting(3), "       10")
        XCTAssertEqual(v.labelForTesting(6), "1200     ")
    }

    /// 第 7 版：非 UTF-8 的文件，后台换成的 U+FFFD 在正文里着提醒色（与文件查看器同一个函数）。
    func testBodyColorsUndecodable() {
        let v = DiffViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        let w = NSWindow(contentRect: v.frame, styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = v
        v.begin(item: ChangesStore.Item(path: "gbk.txt", status: "M", added: 1, deleted: 0, binary: false, untracked: false), root: "/p")
        v.show(reply: ["ok": true, "binary": false, "truncated": false, "hunks": [["head": "@@ -1 +1 @@", "lines": [["+", NSNull(), 1, "x\u{FFFD}\u{FFFD}y"]]]]], keepScroll: false)
        let i = (v.text as NSString).range(of: "\u{FFFD}").location
        XCTAssertEqual(v.colorForTesting(at: i), ScyllaUIStyle.waiting)
        XCTAssertEqual(v.colorForTesting(at: i + 1), ScyllaUIStyle.waiting)
        XCTAssertNil(v.colorForTesting(at: i - 1), "别的字不标")
    }

    /// 第 6 版 R1：新打开一律不折行（超长单行也不自动折行）、可横向滚动，正文不窄于可见区（整行底色铺满）；标题行最左边的
    /// 折行按钮开关、强调色与提示；换文件沿用；读取中、说明文字时隐藏；开关前后顶端仍是同一行。
    func testWrapDefaultAndToggle() {
        let v = DiffViewer()
        let w = inWindow(v)
        _ = w
        let long = ChangesStore.Item(path: "long.txt", status: "M", added: 1, deleted: 0, binary: false, untracked: false)
        v.begin(item: long, root: "/p")
        XCTAssertFalse(v.toolNamesForTesting.contains("折行"), "读取中没有折行按钮")
        var lines: [[Any]] = [["+", NSNull(), 1, String(repeating: "L", count: 30_000)]]
        for i in 2...400 { lines.append([" ", i, i, "line \(i) " + String(repeating: "z", count: 120)]) }
        v.show(reply: hunks(lines), keepScroll: false)
        XCTAssertFalse(v.wrapOn, "超长单行也不自动折行")
        XCTAssertTrue(v.bodyCoversVisibleWidth, "不折行时正文至少和可见区一样宽")
        XCTAssertEqual(v.toolNamesForTesting, ["折行", "上一处更改", "下一处更改", "在「文件」里打开", "隐藏更改列表", "更多"], "折行在最左边")
        XCTAssertEqual(v.wrapTintForTesting, .secondaryLabelColor)
        v.scrollToLineForTesting(200)
        let top = v.topLineForTesting
        v.toggleWrapForTesting()
        XCTAssertTrue(v.wrapOn)
        XCTAssertEqual(v.toolNamesForTesting.first, "取消折行")
        XCTAssertEqual(v.wrapTintForTesting, .controlAccentColor)
        w.layoutIfNeeded()
        XCTAssertEqual(v.topLineForTesting, top, "打开折行后顶端仍是同一行")
        v.toggleWrapForTesting()
        w.layoutIfNeeded()
        XCTAssertEqual(v.topLineForTesting, top, "关掉折行后顶端仍是同一行")
        v.toggleWrapForTesting()
        let item = ChangesStore.Item(path: "a.txt", status: "M", added: 1, deleted: 0, binary: false, untracked: false)
        v.begin(item: item, root: "/p")
        v.show(reply: hunks([["+", NSNull(), 1, "short"]]), keepScroll: false)
        XCTAssertTrue(v.wrapOn, "换文件沿用这次的选择")
        v.show(reply: ["ok": true, "binary": true, "truncated": false, "hunks": []], keepScroll: false)
        XCTAssertFalse(v.toolNamesForTesting.contains("取消折行"), "说明文字时没有折行按钮")
        XCTAssertEqual(v.toolNamesForTesting.first, "上一处更改", "别的按钮照旧")
        v.show(reply: hunks([["+", NSNull(), 1, "short"]]), keepScroll: false)
        v.toggleWrapForTesting()
        v.begin(item: long, root: "/p")
        v.show(reply: hunks(lines), keepScroll: false)
        XCTAssertFalse(v.wrapOn, "关掉后换文件也沿用")
    }

    /// 上一处、下一处改动：跳过去后第一行离顶端 3 行；那个方向没有了就置灰。
    func testJumpBetweenChanges() {
        var lines: [[Any]] = []
        for i in 1...600 {
            let changed = [100, 101, 300, 550].contains(i)
            lines.append(changed ? ["+", NSNull(), i, "changed \(i)"] : [" ", i, i, "line \(i)"])
        }
        let v = DiffViewer()
        let w = inWindow(v)
        _ = w
        v.begin(item: ChangesStore.Item(path: "a.txt", status: "M", added: 4, deleted: 0, binary: false, untracked: false), root: "/p")
        v.show(reply: hunks(lines, head: "@@ -1,596 +1,600 @@"), keepScroll: false)
        XCTAssertFalse(v.prevEnabled, "在顶端时上面没有")
        XCTAssertTrue(v.nextEnabled)
        v.nextChangeForTesting()
        XCTAssertEqual(v.topLineForTesting, 100 - 3, "第一处（第 100 行，正文第 100 行含段头）离顶端 3 行")
        v.nextChangeForTesting()
        XCTAssertEqual(v.topLineForTesting, 300 - 3, "连续的两行算一处，下一处是 300")
        XCTAssertTrue(v.prevEnabled)
        v.prevChangeForTesting()
        XCTAssertEqual(v.topLineForTesting, 100 - 3)
        v.scrollToLineForTesting(560)
        XCTAssertFalse(v.nextEnabled, "最后一处之后没有了")
        XCTAssertTrue(v.prevEnabled)
        // 开头就是改动（测试版 huge.txt）：在顶端时上面没有，下一处是后面那一段。
        var head: [[Any]] = (1...5).map { ["-", $0, NSNull(), "old \($0)"] }
        head += (6...300).map { [" ", $0, $0 - 5, "line \($0)"] }
        head.append(["+", NSNull(), 296, "tail"])
        v.begin(item: ChangesStore.Item(path: "b.txt", status: "M", added: 1, deleted: 5, binary: false, untracked: false), root: "/p")
        v.show(reply: hunks(head, head: "@@ -1,300 +1,296 @@"), keepScroll: false)
        XCTAssertFalse(v.prevEnabled, "开头就是改动时，在顶端上面没有")
        XCTAssertTrue(v.nextEnabled)
        v.nextChangeForTesting()
        // 末尾那一处到不了顶端：估算的高度停在估算的底部，随后（下一轮主循环）补到真正的末尾（`EndFollower`，同文件查看器）。
        RunLoop.main.run(until: Date().addingTimeInterval(0.1))
        XCTAssertGreaterThan(v.topLineForTesting ?? 0, 100, "跳过开头那段（顶端已经在它那里），到后面那一处")
        XCTAssertTrue(v.endVisibleForTesting, "末尾那一处看得见")
        XCTAssertFalse(v.nextEnabled, "滚到底了，下面没有")
        XCTAssertTrue(v.prevEnabled)
    }

    /// 自动刷新按顶端那一行（第几行）保留位置，不按像素；换了文件回到顶端。
    func testRefreshKeepsTopLine() {
        let lines: [[Any]] = (1...2000).map { [" ", $0, $0, "line \($0) " + String(repeating: "内容", count: $0 % 30)] }
        let v = DiffViewer()
        let w = inWindow(v)
        _ = w
        let item = ChangesStore.Item(path: "a.txt", status: "M", added: 1, deleted: 0, binary: false, untracked: false)
        v.begin(item: item, root: "/p")
        v.show(reply: hunks(lines, head: "@@ -1,2000 +1,2000 @@"), keepScroll: false)
        v.scrollToLineForTesting(1200)
        XCTAssertEqual(v.topLineForTesting, 1200)
        var changed = lines
        changed[1500] = ["+", NSNull(), 1501, "new"]
        v.begin(item: item, root: "/p")
        v.show(reply: hunks(changed, head: "@@ -1,2000 +1,2000 @@"), keepScroll: true)
        XCTAssertEqual(v.topLineForTesting, 1200, "刷新后仍在第 1200 行")
        v.begin(item: ChangesStore.Item(path: "b.txt", status: "M", added: 1, deleted: 0, binary: false, untracked: false), root: "/p")
        v.show(reply: hunks(lines, head: "@@ -1,2000 +1,2000 @@"), keepScroll: false)
        XCTAssertEqual(v.topLineForTesting, 0, "换了文件回到顶端")
    }

    /// 同文件查看器（`testLeaveBottomAfterReachingEnd`）：按真实顺序（先滚可见区、滚动条的值还是旧的，再让滚动条跟上），
    /// 设到最底能到最后一行；到底后往上一小步、一大步、到顶都不被拉回底部。
    func testLeaveBottomAfterReachingEnd() {
        let v = DiffViewer()
        let w = inWindow(v)
        let lines: [[Any]] = (1...3000).map { i in
            let t = "第 \(i) 行 " + String(repeating: "长短不一的内容", count: (i * 7) % 40)
            return i % 50 == 0 ? ["+", NSNull(), i, t] : [" ", i, i, t]
        }
        v.begin(item: ChangesStore.Item(path: "a.txt", status: "M", added: 60, deleted: 0, binary: false, untracked: false), root: "/p")
        v.show(reply: hunks(lines, head: "@@ -1,2940 +1,3000 @@"), keepScroll: false)
        w.layoutIfNeeded()
        v.scrollToBottomForTesting()
        RunLoop.main.run(until: Date().addingTimeInterval(0.1))
        w.layoutIfNeeded()
        w.displayIfNeeded()
        XCTAssertTrue(v.endVisibleForTesting, "设到最底一次到最后一行")
        let end = v.clipMinYForTesting
        for dy in [-40.0, -3000.0] {
            let before = v.clipMinYForTesting
            v.scrollByForTesting(dy: dy)
            RunLoop.main.run(until: Date().addingTimeInterval(0.1))
            w.layoutIfNeeded()
            w.displayIfNeeded()
            let got = v.clipMinYForTesting
            if dy > -100 {
                XCTAssertEqual(got, before + dy, accuracy: 1, "往上 \(-dy) 停在那里（底部 \(end)）")
            } else {
                XCTAssertLessThan(got, before + dy / 2, "往上 \(-dy) 没被拉回底部：\(got)，滚之前 \(before)")
            }
        }
        v.scrollByForTesting(dy: -1e9)
        RunLoop.main.run(until: Date().addingTimeInterval(0.1))
        w.layoutIfNeeded()
        w.displayIfNeeded()
        XCTAssertLessThan(v.clipMinYForTesting, 1, "滚到顶")
    }

    // MARK: D4 返工（2026-11-25 方案会话验收）

    /// 多处分散改动的一段 diff（同测试仓库的 multi.txt）：`changed` 里的行号各改一处，其余是上下文。
    private func multiHunks(_ changed: [Int], total: Int = 400) -> [String: Any] {
        var lines: [[Any]] = []
        for i in 1...total {
            if changed.contains(i) {
                lines.append(["-", i, NSNull(), "multi line \(i)"])
                lines.append(["+", NSNull(), i, "multi line \(i) CHANGED"])
            } else {
                lines.append([" ", i, i, "multi line \(i)"])
            }
        }
        return hunks(lines, head: "@@ -1,\(total) +1,\(total) @@")
    }

    /// B：自动刷新按顶端那一行在文件里的行号保留位置。上面多了、少了一处改动时，正文的行数变了，按「正文第几行」会窜。
    func testRefreshKeepsTopFileLineWhenChangeAddedAbove() {
        let v = DiffViewer()
        let w = inWindow(v)
        _ = w
        let item = ChangesStore.Item(path: "multi.txt", status: "M", added: 7, deleted: 7, binary: false, untracked: false)
        let base = [5, 30, 60, 90, 120, 150, 180]
        v.begin(item: item, root: "/p")
        v.show(reply: multiHunks(base), keepScroll: false)
        v.nextChangeForTesting()
        v.nextChangeForTesting()
        v.nextChangeForTesting()
        XCTAssertEqual(v.topNewLineForTesting, 57, "第三处（第 60 行）离顶 3 行，顶端是第 57 行")
        // 刷新后等下一轮主循环：换上新正文后按行号滚过去，不能被「滚到末尾的补救」当成往下滚到了底、又补到末尾
        // （测试版：顶端从 57 跳到了 123 之后）。
        v.begin(item: item, root: "/p")
        v.show(reply: multiHunks(base + [15]), keepScroll: true)
        RunLoop.main.run(until: Date().addingTimeInterval(0.1))
        XCTAssertEqual(v.topNewLineForTesting, 57, "上面多了一处改动，刷新后顶端仍是第 57 行")
        v.begin(item: item, root: "/p")
        v.show(reply: multiHunks(base), keepScroll: true)
        RunLoop.main.run(until: Date().addingTimeInterval(0.1))
        XCTAssertEqual(v.topNewLineForTesting, 57, "改回去再刷新，顶端仍是第 57 行")
    }

    /// B 一并核对文件查看器：文件被改了重读（保留位置）后等下一轮主循环，不被补到末尾（改前同样跳到第 379 行，改版前就有）。
    func testFileViewerRefreshKeepsTopAfterRunLoop() {
        let f = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 400), styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = f
        func text(_ edited: Bool) -> String { (1...400).map { $0 == 15 && edited ? "line 15 EDIT" : "line \($0)" }.joined(separator: "\n") }
        func reply(_ t: String) -> [String: Any] { ["ok": true, "size": t.utf8.count, "binary": false, "text": t, "truncated": false, "lossy": false] }
        f.begin(path: "/p/a.txt", root: "/p")
        f.show(reply: reply(text(false)), keepScroll: false)
        w.layoutIfNeeded()
        let c57 = (text(false) as NSString).range(of: "line 57\n").location
        f.scrollSourceForTesting(toChar: c57)
        RunLoop.main.run(until: Date().addingTimeInterval(0.1))
        let before = f.sourceTopCharForTesting
        f.begin(path: "/p/a.txt", root: "/p")
        f.show(reply: reply(text(true)), keepScroll: true)
        RunLoop.main.run(until: Date().addingTimeInterval(0.1))
        let after = f.sourceTopCharForTesting.map { LineNumberRuler.lineIndex(of: $0, in: LineNumberRuler.lineStarts(of: text(true) as NSString)) + 1 }
        XCTAssertNotNil(before)
        // 文件查看器按顶端字符保留（docs/design.md 5.12 节），上面那行多了 5 个字时会往前错一行；这里只管不被补到末尾（改前到了第 379 行）。
        XCTAssertEqual(Double(after ?? 0), 57, accuracy: 1, "刷新后顶端仍在第 57 行附近，没被补到末尾")
    }

    /// A：`Cmd+↓` 只动纵向，横向保持（最后一行比可见区宽、不折行时）。文件查看器共用同一种文本视图，一并核对。
    func testMoveToEndKeepsHorizontalPosition() {
        let v = DiffViewer()
        let w = inWindow(v)
        let lines: [[Any]] = (1...300).map { ["+", NSNull(), $0, "huge \($0) " + String(repeating: "y", count: 300)] }
        v.begin(item: ChangesStore.Item(path: "huge.txt", status: "M", added: 300, deleted: 0, binary: false, untracked: false), root: "/p")
        v.show(reply: hunks(lines, head: "@@ -0,0 +1,300 @@"), keepScroll: false)
        XCTAssertFalse(v.wrapOn)
        // 窗口要真的在前台、正文有键盘焦点：插入点移到文末后横向滚到行尾只在这时发生（与测试版按键相同）。
        w.makeKeyAndOrderFront(nil)
        defer { w.orderOut(nil) }
        let x = v.clipMinXForTesting
        v.takeFocus()
        w.sendEvent(Self.cmdDown(w))
        RunLoop.main.run(until: Date().addingTimeInterval(0.1))
        w.layoutIfNeeded()
        XCTAssertTrue(v.endVisibleForTesting, "到了最后一行")
        XCTAssertEqual(v.clipMinXForTesting, x, accuracy: 0.5, "横向没被带到行尾")
        // 单测环境里系统不会把可见区带到行尾（测试版里会），上面这条改前也通过；所以再直接检查：`Cmd+↓`、`End` 走「保持横向」，
        // 其间可见区被挪到右边，出来后回到原处。
        let sel = #selector(NSTextView.moveToEndOfDocument(_:))
        XCTAssertNotEqual(class_getMethodImplementation(ViewerTextView.self, sel), class_getMethodImplementation(NSTextView.self, sel), "Cmd+↓ 经过查看器自己的处理")
        let end = #selector(NSTextView.scrollToEndOfDocument(_:))
        XCTAssertNotEqual(class_getMethodImplementation(ViewerTextView.self, end), class_getMethodImplementation(NSTextView.self, end), "End 经过查看器自己的处理")
        v.shiftRightKeepingXForTesting(500)
        XCTAssertEqual(v.clipMinXForTesting, x, accuracy: 0.5, "被挪到右边后回到原处")
        let f = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        let fw = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 400), styleMask: [.titled], backing: .buffered, defer: false)
        fw.contentView = f
        let text = (1...300).map { "line \($0) " + String(repeating: "z", count: 300) }.joined(separator: "\n")
        f.begin(path: "/p/a.txt", root: "/p")
        f.show(reply: ["ok": true, "size": text.utf8.count, "binary": false, "text": text, "truncated": false, "lossy": false], keepScroll: false)
        fw.layoutIfNeeded()
        fw.makeKeyAndOrderFront(nil)
        defer { fw.orderOut(nil) }
        let x0 = f.sourceClipMinXForTesting
        f.takeFocus()
        fw.sendEvent(Self.cmdDown(fw))
        RunLoop.main.run(until: Date().addingTimeInterval(0.1))
        XCTAssertEqual(f.sourceClipMinXForTesting, x0, accuracy: 0.5, "文件查看器同样不被带到行尾")
    }

    /// 真实的 `Cmd+↓` 按键（键码 125）。
    static func cmdDown(_ w: NSWindow) -> NSEvent {
        NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: [.command, .function, .numericPad], timestamp: ProcessInfo.processInfo.systemUptime,
                         windowNumber: w.windowNumber, context: nil, characters: String(Character(UnicodeScalar(NSDownArrowFunctionKey)!)),
                         charactersIgnoringModifiers: String(Character(UnicodeScalar(NSDownArrowFunctionKey)!)), isARepeat: false, keyCode: 125)!
    }

    /// C：行号栏只画在自己范围内（macOS 14 起视图默认不裁剪，系统给的重画范围超出边界，行号栏照着填底色、画边线，伸进了标题行与栏头）。
    /// 离屏绘制复现不出越界（单测里重画范围不超出），所以检查行号栏会裁剪；真实窗口的效果在测试版里看。
    func testRulersClipToBounds() {
        XCTAssertTrue(DiffViewer().rulerClipsForTesting)
        XCTAssertTrue(FileViewer().rulerClipsForTesting)
    }

    /// D：行号栏跟着整行着色：新增、删除、段头三种行与正文同色，上下文不着色。
    func testRulerRowColors() {
        let v = DiffViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        v.begin(item: ChangesStore.Item(path: "a.txt", status: "M", added: 1, deleted: 1, binary: false, untracked: false), root: "/p")
        v.show(reply: hunks([[" ", 1, 1, "a"], ["-", 2, NSNull(), "b"], ["+", NSNull(), 2, "c"]], head: "@@ -1,2 +1,2 @@"), keepScroll: false)
        XCTAssertEqual(v.rulerColorForTesting(0), DiffTextView.color(UInt8(ascii: "@")), "段头")
        XCTAssertNil(v.rulerColorForTesting(1), "上下文不着色")
        XCTAssertEqual(v.rulerColorForTesting(2), DiffTextView.color(UInt8(ascii: "-")))
        XCTAssertEqual(v.rulerColorForTesting(3), DiffTextView.color(UInt8(ascii: "+")))
    }

    /// 与后台连接上按行读：大的一行（Diff 的 2.7 MB 应答）只在新读进来的部分里找换行。原来每读进 64 KB 都从头找，测试版里 1.5—2.8 秒。
    func testLineReaderSplitsLargeLinesQuickly() throws {
        let big = String(repeating: "x", count: 3_000_000)
        let url = FileManager.default.temporaryDirectory.appendingPathComponent("linereader-\(UUID().uuidString)")
        try Data(("a\n" + big + "\n\nb\nc").utf8).write(to: url)
        defer { try? FileManager.default.removeItem(at: url) }
        let fd = open(url.path, O_RDONLY)
        XCTAssertGreaterThanOrEqual(fd, 0)
        defer { close(fd) }
        let r = LineReader(fd: fd)
        let start = Date()
        XCTAssertEqual(r.next().map { String(decoding: $0, as: UTF8.self) }, "a")
        XCTAssertEqual(r.next()?.count, big.utf8.count)
        XCTAssertLessThan(Date().timeIntervalSince(start), 0.2, "3 MB 的一行不该慢（改前 0.55 秒，改后 0.05 秒）")
        XCTAssertEqual(r.next()?.count, 0, "空行")
        XCTAssertEqual(r.next().map { String(decoding: $0, as: UTF8.self) }, "b")
        XCTAssertNil(r.next(), "最后没有换行的半行不算")
    }

    /// `?` 开头搜内容：结果按文件分组，没有结果、没装命令行工具都有说明。
    func testContentSearchGroupsByFile() {
        var grepReply: [String: Any] = ["ok": true, "truncated": false, "items": [
            ["path": "a.txt", "line": 1, "text": "Hello"], ["path": "a.txt", "line": 4, "text": "hello again"],
            ["path": "sub/b.md", "line": 2, "text": "HELLO"]]]
        let panel = FilesPanel(frame: NSRect(x: 0, y: 0, width: 300, height: 600))
        panel.request = { req, done in
            switch req["type"] as! String {
            case "fs.grep":
                XCTAssertEqual(req["query"] as? String, "hello", "去掉开头的 ?")
                done(grepReply)
            case "fs.list": done(["ok": true, "items": [], "more": 0])
            default: done(["ok": true, "size": 1, "binary": false, "text": "x\ny\n", "truncated": false, "lossy": false])
            }
        }
        panel.setRoot("/p", gitRoot: "/p", currentDir: nil)
        panel.setFilterForTesting("?hello")
        XCTAssertEqual(panel.resultRows, ["[a.txt]", "1: Hello", "4: hello again", "[sub/b.md]", "2: HELLO"])
        grepReply = ["ok": false, "error": ["code": "no_git"]]
        panel.setFilterForTesting("? hello")
        XCTAssertEqual(panel.resultRows, ["搜内容需要 Xcode 命令行工具"])
        panel.openFile("/p/a.txt", line: 2)
        XCTAssertEqual(panel.viewer.revealedLine, 2, "打开后滚到那一行")
    }

    /// 放进真实尺寸的窗口：改动正文占满标题区以下，分隔线都有必需的高度或宽度约束（2026-11-24 测试版里标题下的分隔线没定高，
    /// 正文被挤得看不见，逐行改动整块空白）。
    func testViewerBodyFillsBelowHeaderInWindow() {
        let panel = DiffPanel()
        panel.request = { _, done in
            done(["ok": true, "binary": false, "truncated": false, "hunks": [
                ["head": "@@ -1 +1 @@", "lines": [["-", 1, NSNull(), "a"], ["+", NSNull(), 1, "b"]]]]])
        }
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 900, height: 600), styleMask: [.titled, .resizable, .fullSizeContentView], backing: .buffered, defer: false)
        let c = NSView()
        w.contentView = c
        panel.translatesAutoresizingMaskIntoConstraints = false
        c.addSubview(panel)
        NSLayoutConstraint.activate([panel.topAnchor.constraint(equalTo: c.topAnchor, constant: 30), panel.bottomAnchor.constraint(equalTo: c.bottomAnchor),
                                     panel.trailingAnchor.constraint(equalTo: c.trailingAnchor), panel.widthAnchor.constraint(equalToConstant: 700)])
        panel.setRoot("/p", inRepo: true, branch: "main")
        panel.update(ChangesStore.parse(reply))
        w.layoutIfNeeded()
        XCTAssertTrue(panel.viewerOpen)
        for v in panel.viewer.subviews { XCTAssertFalse(v.hasAmbiguousLayout, "\(type(of: v)) 布局不确定") }
        for v in panel.subviews { XCTAssertFalse(v.hasAmbiguousLayout, "\(type(of: v)) 布局不确定") }
        let body = panel.viewer.subviews.first { $0 is NSScrollView }!
        XCTAssertGreaterThan(body.frame.height, panel.viewer.frame.height - 80, "正文占满标题区以下")
        for box in (panel.viewer.subviews + panel.subviews).compactMap({ $0 as? NSBox }) {
            XCTAssertTrue(Self.hasFixedThickness(box), "分隔线显式定了高度或宽度")
        }
    }

    /// 右侧栏里与正文上下或左右相连的 `NSBox` 分隔线都显式定了粗细（Diff 标题下、Diff 列表分隔、文件树分隔、查看器标题下）。
    /// 状态栏的分节线不算：它只按段标题居中、不与别的视图相连，固有高度挤不到谁；定成 1 pt 反而落在半像素上画不出来
    /// （2026-11-24 测试版对照）。
    func testAllRightPanelSeparatorsHaveFixedThickness() {
        func boxes(_ v: NSView) -> [NSBox] { (v as? NSBox).map { [$0] } ?? [] + v.subviews.flatMap(boxes) }
        let panels: [(String, NSView)] = [("Diff", DiffPanel()), ("文件", FilesPanel()), ("查看器", FileViewer())]
        for (name, v) in panels {
            let found = boxes(v)
            XCTAssertFalse(found.isEmpty, "\(name) 里有分隔线")
            for b in found { XCTAssertTrue(Self.hasFixedThickness(b), "\(name) 的分隔线显式定了粗细") }
        }
    }

    /// 显式的必需高度或宽度约束（系统按固有尺寸加的不算：分隔线的固有高度是 5，优先级与滚动区一样，谁被拉伸由求解决定，
    /// 测试窗口里恰好分给了滚动区，测试版里分给了分隔线，正文高 0）。
    static func hasFixedThickness(_ box: NSBox) -> Bool {
        box.constraints.contains {
            ($0.firstAttribute == .height || $0.firstAttribute == .width) && $0.secondItem == nil && $0.priority == .required
                && type(of: $0) == NSLayoutConstraint.self
        }
    }
}
