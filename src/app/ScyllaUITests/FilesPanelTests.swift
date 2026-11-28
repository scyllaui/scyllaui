// 右侧栏「文件」的单测（docs/design.md 5.12 节）：只留最新、树按名字合并后展开状态不丢、筛选、查看器的文案与折行按钮。

import AppKit
import XCTest

final class FilesPanelTests: XCTestCase {
    private static let asyncRender = FileViewer.renderWork

    override func setUp() {
        super.setUp()
        FileViewer.renderWork = { work, done in done(work()) }
    }

    override func tearDown() {
        FileViewer.renderWork = Self.asyncRender
        super.tearDown()
    }

    func testLatestKeepsOneInFlightAndReplaysLast() {
        let l = Latest()
        var started: [Int] = []
        var dones: [() -> Void] = []
        for i in 1...3 {
            l.submit { done in
                started.append(i)
                dones.append(done)
            }
        }
        XCTAssertEqual(started, [1], "在途时只记最后一次")
        dones[0]()
        dones[0]()
        XCTAssertEqual(started, [1, 3], "应答回来后补发最后一次；重复的 done 不会多发")
        dones[1]()
        XCTAssertFalse(l.isBusy)
        l.submit { _ in started.append(4) }
        l.cancelQueued()
        XCTAssertEqual(started, [1, 3, 4])
    }

    func testMergeKeepsNodesSortsAndInheritsIgnored() {
        let root = FileNode(name: "p", path: "/p", kind: .dir)
        FileTree.merge(into: root, items: [["name": "b.txt", "kind": "file"], ["name": "src", "kind": "dir"],
                                           ["name": "a10", "kind": "file"], ["name": "a9", "kind": "file"],
                                           ["name": "vendor", "kind": "dir", "ignored": true]], more: 0)
        XCTAssertEqual(root.children?.map(\.name), ["src", "vendor", "a9", "a10", "b.txt"], "文件夹在前，自然排序")
        let src = root.child(named: "src")!
        FileTree.merge(into: root, items: [["name": "src", "kind": "dir"], ["name": "c.txt", "kind": "file"]], more: 3)
        XCTAssertTrue(root.child(named: "src") === src, "名字没变的保留原节点对象")
        XCTAssertEqual(root.children?.last?.kind, .note, "超过上限写一行说明")
        let vendor = FileNode(name: "vendor", path: "/p/vendor", kind: .dir, ignored: true)
        FileTree.merge(into: vendor, items: [["name": "x", "kind": "file", "ignored": false]], more: 0)
        XCTAssertEqual(vendor.children?.first?.ignored, true, "父项被忽略，子项一律算忽略")
    }

    /// 用假后台跑一遍：根读出来，展开 src，后台变化后重读根，src 仍展开；筛选显示结果。
    func testTreeLoadsLazilyAndSurvivesRefresh() {
        let fs: [String: [[String: Any]]] = [
            "/p": [["name": "src", "kind": "dir"], ["name": "README.md", "kind": "file"]],
            "/p/src": [["name": "main.swift", "kind": "file"]],
        ]
        var requests: [String] = []
        let panel = FilesPanel(frame: NSRect(x: 0, y: 0, width: 300, height: 600))
        panel.request = { req, done in
            let type = req["type"] as! String
            requests.append("\(type) \(req["dir"] ?? req["path"] ?? req["query"] ?? "")")
            switch type {
            case "fs.list": done(["ok": true, "items": fs[req["dir"] as! String] ?? [], "more": 0])
            case "fs.find": done(["ok": true, "items": [["path": "src/main.swift", "kind": "file"]], "truncated": false])
            default: done(["ok": true, "size": 3, "binary": false, "text": "abc", "truncated": false, "lossy": false])
            }
        }
        panel.setRoot("/p", gitRoot: "/p", currentDir: "/p/src")
        XCTAssertEqual(panel.visibleRows, ["src", "  main.swift", "README.md"], "终端在子目录时逐级展开到那一层")
        panel.refresh(dirs: ["/p/"])
        XCTAssertEqual(panel.visibleRows, ["src", "  main.swift", "README.md"], "重读后展开状态不丢")
        XCTAssertTrue(requests.contains("fs.list /p/src"))
        panel.openFile("/p/README.md")
        XCTAssertTrue(panel.viewerOpen)
        XCTAssertEqual(panel.viewer.renderedText?.string, "abc", "Markdown 默认渲染")
        panel.setFilterForTesting("main")
        XCTAssertEqual(panel.resultRows, ["src/main.swift"])
        panel.setRoot("/q", gitRoot: nil, currentDir: nil)
        XCTAssertFalse(panel.viewerOpen, "每个根各记各的打开文件")
        panel.setRoot("/p", gitRoot: "/p", currentDir: "/p/src")
        XCTAssertTrue(panel.viewerOpen, "切回原根恢复打开的文件")
    }

    /// 第四轮长跑缺陷①（2026-11-28）：切到别的栏或关掉右侧栏期间文件被改、新增、删除，切回「文件」（文件栏回到窗口，
    /// 工作区对同一个根再调一次 `setRoot`）时树和查看器要是新内容；展开状态、选中项、树与查看器的滚动位置、源码 / 折行选择不丢。
    func testReturningToFilesRereadsTreeAndOpenFile() {
        var fs: [String: [[String: Any]]] = [
            "/p": [["name": "src", "kind": "dir"], ["name": "static.md", "kind": "file"]]
                + (0..<60).map { ["name": String(format: "f%02d.txt", $0), "kind": "file"] },
            "/p/src": [["name": "main.swift", "kind": "file"]],
        ]
        func doc(_ v: String) -> String { (0..<400).map { "\(v) line \($0)" }.joined(separator: "\n") }
        var files: [String: String] = ["/p/static.md": doc("v1")]
        let panel = FilesPanel(frame: NSRect(x: 0, y: 0, width: 700, height: 400))
        panel.request = { req, done in
            switch req["type"] as! String {
            case "fs.list": done(["ok": true, "items": fs[req["dir"] as! String] ?? [], "more": 0])
            default:
                guard let text = files[req["path"] as! String] else { return done(["ok": false, "error": ["code": "not_found"]]) }
                done(["ok": true, "size": text.utf8.count, "binary": false, "text": text, "truncated": false, "lossy": false])
            }
        }
        let host = NSView(frame: panel.frame)
        let w = NSWindow(contentRect: host.frame, styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = host
        host.addSubview(panel)
        panel.setRoot("/p", gitRoot: nil, currentDir: "/p/src")
        panel.openFile("/p/static.md")
        panel.viewer.toggleRenderForTesting()
        panel.viewer.clickWrapForTesting()
        let wrapping = panel.viewer.isWrapping
        panel.treeScrollYForTesting = 200
        panel.viewer.scrollSourceForTesting(toChar: (doc("v1") as NSString).range(of: "v1 line 200").location)
        let treeY = panel.treeScrollYForTesting
        let topLine = { (panel.viewer.shownText.map { t in t.prefix(panel.viewer.sourceTopCharForTesting ?? 0) }?.filter { $0 == "\n" }.count) }
        let line = topLine()
        XCTAssertEqual(line, 200)
        XCTAssertGreaterThan(treeY, 0)

        // 两轮：从「状态」切回，再关掉右侧栏后打开；中间都有改动。
        for (round, v) in ["v2", "v3"].enumerated() {
            panel.removeFromSuperview()
            files["/p/static.md"] = doc(v)
            fs["/p"]!.append(["name": "added-\(round).txt", "kind": "file"])
            fs["/p/src"]!.append(["name": "new-\(round).swift", "kind": "file"])
            host.addSubview(panel)
            panel.setRoot("/p", gitRoot: nil, currentDir: "/p/src")
            XCTAssertTrue(panel.visibleRows.contains("added-\(round).txt"), "第 \(round + 1) 轮：根下新增的文件出现")
            XCTAssertTrue(panel.visibleRows.contains("  new-\(round).swift"), "第 \(round + 1) 轮：展开着的子目录重读")
            XCTAssertEqual(panel.viewer.shownText?.hasPrefix("\(v) line 0"), true, "第 \(round + 1) 轮：打开的文件是新内容，仍显示源码")
            XCTAssertEqual(topLine(), line, "第 \(round + 1) 轮：查看器按顶部那一行保留位置")
            XCTAssertEqual(panel.viewer.isWrapping, wrapping, "折行选择不丢")
            XCTAssertEqual(panel.selectedTreePathForTesting, "/p/src", "选中项不丢")
            XCTAssertEqual(panel.treeScrollYForTesting, treeY, "树的滚动位置不丢")
        }

        // 离开期间打开的文件被删了：写「文件已被删除」，不留旧内容。
        panel.removeFromSuperview()
        files["/p/static.md"] = nil
        fs["/p"]!.removeAll { $0["name"] as? String == "static.md" }
        host.addSubview(panel)
        panel.setRoot("/p", gitRoot: nil, currentDir: "/p/src")
        XCTAssertEqual(panel.viewer.noteText, String(localized: "文件已被删除"))
        XCTAssertFalse(panel.visibleRows.contains("static.md"))

        // 一直显示着时，同一个根再调 `setRoot`（切终端、状态更新）不重读。
        var reads = 0
        let inner = panel.request
        panel.request = { req, done in reads += 1; inner?(req, done) }
        panel.setRoot("/p", gitRoot: nil, currentDir: "/p/src")
        XCTAssertEqual(reads, 0)
    }

    /// 切终端到另一个项目再切回（文件栏一直显示着，换根）：读过的子目录不能用缓存（2026-11-28 修缺陷①时发现：原来只重读根）。
    /// 展开着的立即重读，读过但收着的下次展开时重读；展开状态不丢。
    func testSwitchingBackToCachedRootRereadsSubdirs() {
        var fs: [String: [[String: Any]]] = [
            "/p": [["name": "lib", "kind": "dir"], ["name": "src", "kind": "dir"]],
            "/p/lib": [["name": "a.swift", "kind": "file"]],
            "/p/src": [["name": "main.swift", "kind": "file"]],
            "/q": [["name": "x.txt", "kind": "file"]],
        ]
        var reads: [String] = []
        let panel = FilesPanel(frame: NSRect(x: 0, y: 0, width: 300, height: 600))
        panel.request = { req, done in
            switch req["type"] as! String {
            case "fs.list":
                reads.append(req["dir"] as! String)
                done(["ok": true, "items": fs[req["dir"] as! String] ?? [], "more": 0])
            default: done(["ok": true, "size": 3, "binary": false, "text": "abc", "truncated": false, "lossy": false])
            }
        }
        panel.setRoot("/p", gitRoot: nil, currentDir: "/p/lib")
        panel.setRoot("/p", gitRoot: nil, currentDir: "/p/src")
        panel.toggleExpandedForTesting("/p/lib")
        XCTAssertEqual(panel.visibleRows, ["lib", "src", "  main.swift"], "lib 读过但收着，src 展开")
        panel.setRoot("/q", gitRoot: nil, currentDir: "/q")
        fs["/p/src"]!.append(["name": "new.swift", "kind": "file"])
        fs["/p/lib"]!.append(["name": "b.swift", "kind": "file"])
        reads = []
        panel.setRoot("/p", gitRoot: nil, currentDir: "/p/src")
        XCTAssertEqual(panel.visibleRows, ["lib", "src", "  main.swift", "  new.swift"], "展开着的子目录重读")
        XCTAssertFalse(reads.contains("/p/lib"), "收着的不立即读")
        panel.toggleExpandedForTesting("/p/lib")
        XCTAssertEqual(panel.visibleRows, ["lib", "  a.swift", "  b.swift", "src", "  main.swift", "  new.swift"], "收着的展开时重读")
    }

    /// 补测返工（2026-11-28，X3）：后台对右侧栏的请求同时在途超过 8 个回 `busy`。展开 22 个目录再打开一个文件，
    /// 切回「文件」、换根再回来、FSEvents 一次报全部目录，三条路都要每一层读到新内容，`busy` 不写进树和查看器。
    func testRereadManyDirsUnderBackendLimit() {
        let names = (0..<22).map { String(format: "d%02d", $0) }
        let backend = LimitedBackend()
        backend.fs["/p"] = names.map { ["name": $0, "kind": "dir"] }
        for n in names { backend.fs["/p/\(n)"] = [["name": "a.txt", "kind": "file"]] }
        backend.fs["/q"] = [["name": "x.txt", "kind": "file"]]
        backend.files["/p/d00/a.txt"] = "v0"
        let panel = FilesPanel(frame: NSRect(x: 0, y: 0, width: 700, height: 600))
        panel.request = wireForTesting(backend.send)
        let host = NSView(frame: panel.frame)
        let w = NSWindow(contentRect: host.frame, styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = host
        host.addSubview(panel)
        panel.setRoot("/p", gitRoot: nil, currentDir: nil)
        backend.flush()
        for n in names { panel.toggleExpandedForTesting("/p/\(n)") }
        backend.flush()
        panel.openFile("/p/d00/a.txt")
        backend.flush()
        XCTAssertEqual(panel.visibleRows.filter { $0 == "  a.txt" }.count, 22)

        func change(_ v: Int) {
            for n in names { backend.fs["/p/\(n)"]!.append(["name": "new\(v).txt", "kind": "file"]) }
            backend.files["/p/d00/a.txt"] = "v\(v)"
        }
        func check(_ v: Int, _ how: String) {
            let rows = panel.visibleRows
            XCTAssertEqual(rows.filter { $0 == "  new\(v).txt" }.count, 22, "\(how)：每一层都读到新内容")
            XCTAssertFalse(rows.contains { $0.contains(String(localized: "暂时读不到这个文件夹")) }, "\(how)：busy 不写进树")
            XCTAssertEqual(panel.viewer.shownText, "v\(v)", "\(how)：打开的文件读到新内容")
        }
        panel.removeFromSuperview()
        change(1)
        host.addSubview(panel)
        panel.setRoot("/p", gitRoot: nil, currentDir: nil)
        backend.flush()
        check(1, "切回")

        panel.setRoot("/q", gitRoot: nil, currentDir: nil)
        backend.flush()
        change(2)
        panel.setRoot("/p", gitRoot: nil, currentDir: nil)
        backend.flush()
        check(2, "换根再回来")

        change(3)
        panel.refresh(dirs: ["/p"] + names.map { "/p/\($0)" })
        backend.flush()
        check(3, "FSEvents 一次报全部目录")
        XCTAssertEqual(backend.busyReplies, 0, "不超过后台的在途上限")

        // 真的读不到（断线）：写「暂时读不到」，收起再展开能重读。
        backend.down = true
        panel.toggleExpandedForTesting("/p/d05")
        panel.refresh(dirs: ["/p/d05"])
        panel.toggleExpandedForTesting("/p/d05")
        backend.flush()
        XCTAssertTrue(panel.visibleRows.contains("  " + String(localized: "暂时读不到这个文件夹")))
        backend.down = false
        panel.toggleExpandedForTesting("/p/d05")
        panel.toggleExpandedForTesting("/p/d05")
        backend.flush()
        XCTAssertFalse(panel.visibleRows.contains("  " + String(localized: "暂时读不到这个文件夹")), "收起再展开重读")
    }

    /// 各栏发请求的接法，同 WorkspaceController。
    private func wireForTesting(_ send: @escaping ([String: Any], @escaping ([String: Any]?) -> Void) -> Void) -> ([String: Any], @escaping ([String: Any]?) -> Void) -> Void {
        RequestGate(send).request
    }

    /// 右侧栏请求的闸：最多 8 个在途，多出的按先后排队；后台仍回 `busy` 时暂停后从这一个起重发，`busy` 不交给调用方。
    func testRequestGateQueuesAndRetriesBusy() {
        var sent: [Int] = []
        var dones: [Int: ([String: Any]?) -> Void] = [:]
        var busyLeft = 1
        let gate = RequestGate { req, done in
            let i = req["i"] as! Int
            sent.append(i)
            if i == 9, busyLeft > 0 {
                busyLeft -= 1
                return done(["ok": false, "error": ["code": "busy"]])
            }
            dones[i] = done
        }
        gate.retryDelay = 0
        var got: [Int] = []
        for i in 0..<10 { gate.request(["i": i]) { _ in got.append(i) } }
        XCTAssertEqual(sent, Array(0..<8), "最多 8 个在途")
        dones[0]!(["ok": true])
        XCTAssertEqual(sent.last, 8, "回来一个补发一个")
        dones[1]!(["ok": true])
        XCTAssertEqual(sent.last, 9)
        XCTAssertFalse(got.contains(9), "busy 不交给调用方")
        RunLoop.main.run(until: Date().addingTimeInterval(0.05))
        XCTAssertEqual(sent.filter { $0 == 9 }.count, 2, "暂停后重发")
        dones[9]!(["ok": true])
        XCTAssertEqual(got, [0, 1, 9])
    }

    /// 补测 X2（2026-11-28）：切回「文件」时文件栏先按别的栏的宽度排一次版、查看器没有宽度，横向位置被夹到 0；
    /// 加宽后要回到离开时的位置（行首不被行号栏挡住），在最左边和往右滚过一段都一样。0.4.58 就有。
    func testViewerKeepsHorizontalPositionAfterSqueezed() {
        let text = (0..<300).map { "line \($0) " + String(repeating: "abcdefghij", count: 30) }.joined(separator: "\n")
        let panel = FilesPanel(frame: NSRect(x: 0, y: 0, width: 700, height: 400))
        panel.request = { req, done in
            switch req["type"] as! String {
            case "fs.list": done(["ok": true, "items": [["name": "a.txt", "kind": "file"]], "more": 0])
            default: done(["ok": true, "size": text.utf8.count, "binary": false, "text": text, "truncated": false, "lossy": false])
            }
        }
        let host = NSView(frame: panel.frame)
        let w = NSWindow(contentRect: host.frame, styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = host
        host.addSubview(panel)
        panel.setRoot("/p", gitRoot: nil, currentDir: nil)
        panel.openFile("/p/a.txt")
        w.layoutIfNeeded()
        let v = panel.viewer
        let left = v.sourceClipMinXForTesting
        XCTAssertEqual(v.sourceTextLeft, v.rulerRight, "打开时行首紧挨行号栏")
        panel.translatesAutoresizingMaskIntoConstraints = false
        let width = panel.widthAnchor.constraint(equalToConstant: 700)
        NSLayoutConstraint.activate([width, panel.topAnchor.constraint(equalTo: host.topAnchor),
                                     panel.bottomAnchor.constraint(equalTo: host.bottomAnchor),
                                     panel.leadingAnchor.constraint(equalTo: host.leadingAnchor)])
        for dx in [CGFloat(0), 100] {
            v.scrollSourceForTesting(toChar: 5000, x: left + dx)
            w.layoutIfNeeded()
            panel.removeFromSuperview()
            width.isActive = false
            host.addSubview(panel)
            NSLayoutConstraint.activate([width, panel.topAnchor.constraint(equalTo: host.topAnchor),
                                         panel.bottomAnchor.constraint(equalTo: host.bottomAnchor),
                                         panel.leadingAnchor.constraint(equalTo: host.leadingAnchor)])
            width.constant = 280
            w.layoutIfNeeded()
            width.constant = 700
            w.layoutIfNeeded()
            panel.setRoot("/p", gitRoot: nil, currentDir: nil)
            RunLoop.main.run(until: Date().addingTimeInterval(0.05))
            XCTAssertEqual(v.sourceClipMinXForTesting, left + dx, "往右滚过 \(dx) 时切回，横向位置不变")
        }
    }

    /// 内容搜索的结果行只占一行、放不下截断（2026-11-25 测试版：长行折成两行，第二行只露出半截字）。
    func testContentHitRowIsSingleLine() {
        let panel = FilesPanel(frame: NSRect(x: 0, y: 0, width: 300, height: 600))
        let long = String(repeating: "结项（含试用后的键盘导航、读仓库移出核心线程）", count: 8)
        panel.request = { req, done in
            switch req["type"] as! String {
            case "fs.list": done(["ok": true, "items": [["name": "a.md", "kind": "file"]], "more": 0])
            case "fs.grep": done(["ok": true, "items": [["path": "a.md", "line": 1234, "text": long]], "truncated": false])
            default: done(["ok": true, "size": 3, "binary": false, "text": "abc", "truncated": false, "lossy": false])
            }
        }
        panel.setRoot("/p", gitRoot: nil, currentDir: nil)
        panel.setFilterForTesting("?键盘")
        RunLoop.main.run(until: Date().addingTimeInterval(0.5))
        XCTAssertEqual(panel.resultRows.count, 2, panel.resultRows.joined(separator: " | "))
        let h = panel.resultTextHeightForTesting(row: 1, width: 240)
        XCTAssertGreaterThan(h, 0)
        XCTAssertLessThanOrEqual(h, 22, "结果行只占一行：排出来高 \(h)")
    }

    /// 补测问题 1（2026-11-28）：某个文件命中超过 50 条（后台 `capped`），在它的结果下面写明只列了前 50 条；别的文件不写。
    func testContentHitsNoteFileOverPerFileLimit() {
        let panel = FilesPanel(frame: NSRect(x: 0, y: 0, width: 300, height: 600))
        let many: [[String: Any]] = (1...50).map { ["path": "search/many-hits.txt", "line": $0, "text": "zebra_marker \($0)"] }
        panel.request = { req, done in
            switch req["type"] as! String {
            case "fs.list": done(["ok": true, "items": [["name": "search", "kind": "dir"]], "more": 0])
            case "fs.grep": done(["ok": true, "items": many + [["path": "b.txt", "line": 7, "text": "zebra_marker"]],
                                  "truncated": false, "capped": ["search/many-hits.txt"]])
            default: done(["ok": true, "size": 3, "binary": false, "text": "abc", "truncated": false, "lossy": false])
            }
        }
        panel.setRoot("/p", gitRoot: nil, currentDir: nil)
        panel.setFilterForTesting("?zebra_marker")
        RunLoop.main.run(until: Date().addingTimeInterval(0.5))
        let rows = panel.resultRows
        XCTAssertEqual(rows.count, 1 + 50 + 1 + 1 + 1, rows.suffix(4).joined(separator: " | "))
        XCTAssertEqual(rows[51], String(localized: "这个文件只列出了前 50 条"), "超过 50 条的文件，结果下面写明")
        XCTAssertEqual(Array(rows.suffix(2)), ["[b.txt]", "7: zebra_marker"], "别的文件不写")
    }

    /// 内容搜索结果从匹配处附近显示、标出匹配的字（2026-11-25 方案会话定：原来显示行首，分不清各条）。
    func testHitSnippetStartsNearMatch() {
        let line = "M1 第 1—3 块已于 2026-10-10 结项，…… 窗口可见时 12 个会话 Hook → 侧栏 P95 23.1 ms，记录见 docs"
        let (shown, r) = FilesPanel.hitSnippet(line, query: "p95 23.1")
        XCTAssertTrue(shown.hasPrefix("…"), shown)
        XCTAssertEqual((shown as NSString).substring(with: r!), "P95 23.1", "不分大小写，标出原文")
        XCTAssertEqual(r!.location, 11, "匹配前留 10 个字（加开头的「…」）")
        let (near, r2) = FilesPanel.hitSnippet("提交：dbd134d（版本号）", query: "DBD134D")
        XCTAssertEqual(near, "提交：dbd134d（版本号）", "匹配在开头附近不截")
        XCTAssertEqual(r2, NSRange(location: 3, length: 7))
        XCTAssertNil(FilesPanel.hitSnippet("没有", query: "xyz").1, "找不到原样显示")
        let (emoji, r3) = FilesPanel.hitSnippet(String(repeating: "😀", count: 20) + "needle", query: "needle")
        XCTAssertEqual((emoji as NSString).substring(with: r3!), "needle", "不切开代理对")
    }

    /// 从搜索结果打开文件后清空查找框：树里选中当前打开的文件，所在目录没展开就逐级展开（2026-11-25 方案会话复验：树里高亮的还是之前那个文件）。
    func testTreeSelectsOpenFileAfterSearch() {
        let fs: [String: [[String: Any]]] = [
            "/p": [["name": "docs", "kind": "dir"], ["name": "a.md", "kind": "file"]],
            "/p/docs": [["name": "b.md", "kind": "file"]],
        ]
        let panel = FilesPanel(frame: NSRect(x: 0, y: 0, width: 900, height: 600))
        panel.request = { req, done in
            switch req["type"] as! String {
            case "fs.list": done(["ok": true, "items": fs[req["dir"] as! String] ?? [], "more": 0])
            case "fs.grep": done(["ok": true, "items": [["path": "docs/b.md", "line": 2, "text": "needle"]], "truncated": false])
            default: done(["ok": true, "size": 3, "binary": false, "text": "x\nneedle\n", "truncated": false, "lossy": false])
            }
        }
        panel.setRoot("/p", gitRoot: nil, currentDir: nil)
        panel.openFile("/p/a.md")
        panel.setFilterForTesting("?needle")
        RunLoop.main.run(until: Date().addingTimeInterval(0.5))
        XCTAssertEqual(panel.resultRows, ["[docs/b.md]", "2: needle"])
        panel.clickResultForTesting(1)
        XCTAssertEqual(panel.viewer.path, "/p/docs/b.md")
        panel.clearFilterForTesting()
        XCTAssertEqual(panel.visibleRows, ["docs", "  b.md", "a.md"], "所在目录没展开就展开")
        XCTAssertEqual(panel.selectedTreePathForTesting, "/p/docs/b.md", "回到树时选中当前打开的文件")
    }

    func testViewerTextsAndWrapDecision() {
        XCTAssertEqual(FileViewer.relative("/p/src/a.swift", root: "/p"), "src/a.swift")
        XCTAssertEqual(FileViewer.relative("/other/a", root: "/p"), "/other/a", "项目外的写绝对路径")
        XCTAssertTrue(FileViewer.usesQuickLook("/x/a.PNG"))
        XCTAssertTrue(FileViewer.usesQuickLook("/x/a.pdf"))
        XCTAssertFalse(FileViewer.usesQuickLook("/x/a.swift"))
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        v.begin(path: "/p/a.bin", root: "/p")
        v.show(reply: ["ok": true, "size": 2048, "binary": true], keepScroll: false)
        XCTAssertEqual(v.noteText, "二进制文件，不显示内容")
        v.show(reply: ["ok": false, "error": ["code": "not_found"]], keepScroll: true)
        XCTAssertEqual(v.noteText, "文件已被删除")
        v.begin(path: "/p/big.txt", root: "/p")
        v.show(reply: ["ok": true, "size": 3_000_000, "binary": false, "text": "a", "truncated": true, "lossy": true], keepScroll: false)
        let total = FileViewer.sizeText(3_000_000)
        XCTAssertEqual(v.metaText, "只显示前 1 MB · 共 \(total) · 有无法解码的字符")
        XCTAssertEqual(v.metaToolTipForTesting, "只显示前 1 MB · 共 \(total) · 有无法解码的字符：文件不是 UTF-8 编码，解不了的地方在正文里用颜色标出。",
                       "悬停提示是各段的完整写法")
    }

    /// 第 7 版（验收返工）：元信息。被截断的 Markdown「不渲染」在最前面；不是 Markdown 的照旧；没截断时只有大小；
    /// 无法解码在标题行只写短的，提示里写完整说明，其余各段提示与标题相同。
    func testViewerMetaText() {
        let total = FileViewer.sizeText(1_400_000)
        let md = FileViewer.meta(size: 1_400_000, truncated: true, markdown: true, lossy: false)
        XCTAssertEqual(md.text, "超过 1 MB，不渲染 · 只显示前 1 MB · 共 \(total)")
        XCTAssertEqual(md.tip, md.text)
        XCTAssertEqual(FileViewer.meta(size: 1_400_000, truncated: true, markdown: false, lossy: false).text, "只显示前 1 MB · 共 \(total)")
        XCTAssertEqual(FileViewer.meta(size: 2048, truncated: false, markdown: true, lossy: false).text, FileViewer.sizeText(2048))
        let lossy = FileViewer.meta(size: 2048, truncated: false, markdown: false, lossy: true)
        XCTAssertEqual(lossy.text, FileViewer.sizeText(2048) + " · 有无法解码的字符")
        XCTAssertEqual(lossy.tip, FileViewer.sizeText(2048) + " · 有无法解码的字符：文件不是 UTF-8 编码，解不了的地方在正文里用颜色标出。")
    }

    /// 第 7 版：找 U+FFFD，相邻的合成一段；只在给的范围里找，位置按整篇算；没有就是空的。
    func testUndecodableRuns() {
        let ns = "a\u{FFFD}\u{FFFD}b\n\u{FFFD}" as NSString
        XCTAssertEqual(FileViewer.undecodableRuns(ns, in: NSRange(location: 0, length: ns.length)), [NSRange(location: 1, length: 2), NSRange(location: 5, length: 1)])
        XCTAssertEqual(FileViewer.undecodableRuns(ns, in: NSRange(location: 2, length: 4)), [NSRange(location: 2, length: 1), NSRange(location: 5, length: 1)])
        XCTAssertEqual(FileViewer.undecodableRuns("没有替换字符" as NSString, in: NSRange(location: 0, length: 6)), [])
    }

    /// 第 7 版：源码视图里 U+FFFD 着色（只标看得见的部分，用临时属性）只在应答说有无法解码的字节（`lossy`）时做；
    /// 换了文字之前标的不留下；渲染视图见 MarkdownRenderTests。
    func testViewerColorsUndecodable() {
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        let w = NSWindow(contentRect: v.frame, styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = v
        v.begin(path: "/p/gbk.txt", root: "/p")
        v.show(reply: ["ok": true, "size": 6, "binary": false, "text": "x\u{FFFD}\u{FFFD}y", "truncated": false, "lossy": true], keepScroll: false)
        XCTAssertEqual(v.sourceColorForTesting(at: 1), ScyllaUIStyle.waiting)
        XCTAssertNotEqual(v.sourceColorForTesting(at: 0), ScyllaUIStyle.waiting)
        v.begin(path: "/p/utf8.txt", root: "/p")
        v.show(reply: ["ok": true, "size": 6, "binary": false, "text": "x\u{FFFD}y", "truncated": false, "lossy": false], keepScroll: false)
        XCTAssertNotEqual(v.sourceColorForTesting(at: 1), ScyllaUIStyle.waiting, "文件里本来就有的替换字符不标")
        XCTAssertEqual(v.metaToolTipForTesting, v.metaText, "没有无法解码的字符时提示与元信息相同")
        // 长文件：只标看得见的部分，滚到哪里标到哪里。
        let lines = (0..<20_000).map { $0 == 15_000 ? "bad \u{FFFD} here" : "line \($0)" }
        let text = lines.joined(separator: "\n")
        let far = (text as NSString).range(of: "\u{FFFD}").location
        v.begin(path: "/p/long.txt", root: "/p")
        v.show(reply: ["ok": true, "size": 200_000, "binary": false, "text": text, "truncated": false, "lossy": true], keepScroll: false)
        XCTAssertNil(v.sourceColorForTesting(at: far), "还没滚到的不标")
        v.scrollSourceForTesting(toChar: far - 20)
        XCTAssertEqual(v.sourceColorForTesting(at: far), ScyllaUIStyle.waiting, "滚到了就标")
    }

    /// 第 7 版：滚动条续显。浮动样式时续显约 `duration` 秒后停下；「始终显示滚动条」时什么也不做。
    func testScrollerLinger() {
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 300, height: 200), styleMask: [.titled], backing: .buffered, defer: false)
        let sv = NSScrollView(frame: w.contentView!.bounds)
        sv.hasVerticalScroller = true
        sv.hasHorizontalScroller = true
        sv.documentView = NSView(frame: NSRect(x: 0, y: 0, width: 1000, height: 1000))
        w.contentView!.addSubview(sv)
        let linger = ScrollerLinger(sv)
        sv.scrollerStyle = .legacy
        linger.show()
        XCTAssertFalse(linger.isLingering, "始终显示滚动条时照系统行为")
        sv.scrollerStyle = .overlay
        let saved = ScrollerLinger.duration
        ScrollerLinger.duration = 0.4
        defer { ScrollerLinger.duration = saved }
        sv.contentView.scroll(to: NSPoint(x: 0, y: 100))
        XCTAssertTrue(linger.isLingering, "滚动后续显")
        RunLoop.main.run(until: Date().addingTimeInterval(0.2))
        XCTAssertTrue(linger.isLingering)
        RunLoop.main.run(until: Date().addingTimeInterval(0.6))
        XCTAssertFalse(linger.isLingering, "到时间停下，之后由系统淡出")
    }

    /// 图标表（第 4 版 F1）：8 种单色系统符号，按扩展名查，大小写不论。
    func testFileIconTable() {
        XCTAssertEqual(FileIcon.symbol(kind: .dir, path: "/p/src"), "folder")
        XCTAssertEqual(FileIcon.symbol(kind: .link, path: "/p/a.swift"), "arrow.turn.up.right", "符号链接不看扩展名")
        XCTAssertNil(FileIcon.symbol(kind: .note, path: ""))
        let cases: [(String, String)] = [
            ("README.md", "doc.text"), ("notes.TXT", "doc.text"), ("run.log", "doc.text"),
            ("main.swift", "chevron.left.forwardslash.chevron.right"), ("lib.rs", "chevron.left.forwardslash.chevron.right"),
            ("index.html", "chevron.left.forwardslash.chevron.right"), ("build.sh", "chevron.left.forwardslash.chevron.right"),
            ("package.json", "curlybraces"), ("Cargo.lock", "curlybraces"), ("project.yml", "curlybraces"), ("Info.plist", "curlybraces"),
            ("logo.PNG", "photo"), ("shot.heic", "photo"), ("spec.pdf", "doc.richtext"),
            ("Makefile", "doc"), ("a.zip", "doc"), ("clip.mp4", "doc"), ("data.bin", "doc"),
            // 点文件按去掉点的名字查（2026-11-24 复查）；带扩展名的点文件照旧看扩展名。
            (".env", "curlybraces"), (".gitignore", "curlybraces"), (".gitattributes", "curlybraces"), (".npmrc", "curlybraces"),
            (".editorconfig", "curlybraces"), (".zshrc", "curlybraces"), (".bashrc", "curlybraces"), ("foo.env", "curlybraces"),
            (".env.local", "doc"), (".prettierrc.json", "curlybraces"), (".DS_Store", "doc"), (".", "doc"),
        ]
        for (name, symbol) in cases {
            XCTAssertEqual(FileIcon.symbol(kind: .file, path: "/p/" + name), symbol, name)
            XCTAssertNotNil(NSImage(systemSymbolName: symbol, accessibilityDescription: nil), "系统里有这个符号：\(symbol)")
        }
    }

    /// 「打开方式」：默认的排第一并标默认，其余按名称，同一个 App 只列一次；没有默认时全按名称。
    func testOpenWithOrder() {
        let u = { (s: String) in URL(fileURLWithPath: "/Applications/\(s).app") }
        let name = { (url: URL) in url.deletingPathExtension().lastPathComponent }
        let key = { (url: URL) in name(url).lowercased() }
        let order = FileViewer.openWithOrder([u("Xcode"), u("TextEdit"), u("BBEdit"), u("textedit"), u("Zed")], default: u("TextEdit"), name: name, key: key)
        XCTAssertEqual(order.map { name($0.0) }, ["TextEdit", "BBEdit", "Xcode", "Zed"])
        XCTAssertEqual(order.map(\.1), [true, false, false, false])
        XCTAssertEqual(FileViewer.openWithOrder([u("b"), u("a")], default: nil, name: name, key: key).map { name($0.0) }, ["a", "b"])
        XCTAssertTrue(FileViewer.openWithOrder([], default: nil, name: name, key: key).isEmpty)
    }

    /// 查看器放进真实窗口后，正文占满标题行以下（2026-11-23 测试版里分隔线没定高，正文被挤到底部只剩 6 pt）。
    func testViewerBodyFillsBelowTitleInWindow() {
        let panel = FilesPanel()
        panel.request = { req, done in
            switch req["type"] as! String {
            case "fs.list": done(["ok": true, "items": [["name": "a.md", "kind": "file"]], "more": 0])
            default: done(["ok": true, "size": 12, "binary": false, "text": "hello\nworld\n", "truncated": false, "lossy": false])
            }
        }
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 900, height: 600), styleMask: [.titled, .resizable, .fullSizeContentView], backing: .buffered, defer: false)
        let c = NSView()
        w.contentView = c
        panel.translatesAutoresizingMaskIntoConstraints = false
        c.addSubview(panel)
        NSLayoutConstraint.activate([panel.topAnchor.constraint(equalTo: c.topAnchor, constant: 30), panel.bottomAnchor.constraint(equalTo: c.bottomAnchor),
                                     panel.trailingAnchor.constraint(equalTo: c.trailingAnchor), panel.widthAnchor.constraint(equalToConstant: 650)])
        panel.setRoot("/p", gitRoot: nil, currentDir: nil)
        panel.openFile("/p/a.md")
        w.layoutIfNeeded()
        for v in panel.viewer.subviews { XCTAssertFalse(v.hasAmbiguousLayout, "\(type(of: v)) 布局不确定") }
        let line = panel.viewer.subviews.first { $0 is NSBox }
        XCTAssertTrue(line?.constraints.contains { $0.firstAttribute == .height && $0.priority == .required } == true, "标题下的分隔线有必需的高度约束")
        let body = panel.viewer.subviews.last!
        XCTAssertGreaterThan(body.frame.height, panel.viewer.frame.height - 60, "正文占满标题行以下")
    }

    /// 栏头：关闭在打开着文件时关文件、否则关栏；放大只在打开文件后有。树为空写「空文件夹」。「…」菜单的四项。
    func testHeaderCloseZoomAndEmptyFolder() {
        let panel = FilesPanel(frame: NSRect(x: 0, y: 0, width: 300, height: 600))
        panel.request = { req, done in
            switch req["type"] as! String {
            case "fs.list": done(["ok": true, "items": [], "more": 0])
            default: done(["ok": true, "size": 3, "binary": false, "text": "abc", "truncated": false, "lossy": false])
            }
        }
        var closedPanel = 0
        panel.onClose = { closedPanel += 1 }
        panel.setRoot("/p", gitRoot: nil, currentDir: nil)
        XCTAssertEqual(panel.visibleRows, ["空文件夹"], "空文件夹写明，不留空白")
        XCTAssertEqual(panel.closeName, "关闭文件栏")
        XCTAssertFalse(panel.zoomShown)
        panel.openFile("/p/a.txt")
        XCTAssertEqual(panel.closeName, "关闭文件")
        XCTAssertTrue(panel.zoomShown)
        let menu = panel.viewer.moreMenu()
        XCTAssertEqual(menu.items.map(\.title), ["拷贝相对路径", "打开方式", "在访达中显示"], "第 6 版 R1 起折行不在「…」里")
        XCTAssertEqual(menu.items[1].submenu?.items.last?.title, "其他…")
        if #available(macOS 27.0, *) {
            for app in menu.items[1].submenu?.items.filter({ $0.representedObject != nil }) ?? [] {
                XCTAssertNotNil(app.image)
                XCTAssertEqual(app.preferredImageVisibility, .visible, "macOS 27 默认隐藏菜单图片，App 图标要显式可见")
            }
        }
        panel.closeFileFromHeaderForTesting()
        XCTAssertFalse(panel.viewerOpen, "打开着文件时先关文件")
        XCTAssertEqual(closedPanel, 0)
        panel.closeFileFromHeaderForTesting()
        XCTAssertEqual(closedPanel, 1, "没有文件时关栏")
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        v.begin(path: "/p/a.bin", root: "/p")
        v.show(reply: ["ok": true, "size": 2048, "binary": true], keepScroll: false)
        XCTAssertFalse(v.toolNamesForTesting.contains("折行"), "不是文本时没有折行按钮")
    }

    /// 第 6 版 R1：折行按钮的图标在当前 SDK 上取得到（取不到会是空按钮）。
    func testWrapSymbolAvailable() {
        XCTAssertEqual(RightPanelHeader.wrapSymbol, "arrow.turn.down.left")
        XCTAssertNotNil(NSImage(systemSymbolName: RightPanelHeader.wrapSymbol, accessibilityDescription: nil))
    }

    /// 第 6 版 R1：新打开一律不折行（超长单行也不自动折行）；标题行最左边的折行按钮开关、强调色与提示；换文件沿用；
    /// 不是文本时隐藏、别的按钮不挪位置；开关前后顶端仍是同一行。
    func testViewerWrapButton() {
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 400), styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = v
        func open(_ path: String, _ text: String) {
            v.begin(path: path, root: "/p")
            v.show(reply: ["ok": true, "size": text.utf8.count, "binary": false, "text": text, "truncated": false, "lossy": false], keepScroll: false)
            w.layoutIfNeeded()
        }
        let long = (1...400).map { $0 == 3 ? String(repeating: "x", count: 30_000) : "第 \($0) 行 " + String(repeating: "字", count: 60) }.joined(separator: "\n")
        open("/p/long.txt", long)
        XCTAssertFalse(v.isWrapping, "超长单行也不自动折行")
        XCTAssertEqual(v.toolNamesForTesting, ["折行", "插入到终端", "隐藏文件树", "更多"], "折行在最左边")
        XCTAssertEqual(v.wrapTintForTesting, .secondaryLabelColor)
        v.scrollSourceForTesting(toChar: (long as NSString).range(of: "第 200 行").location)
        let top = v.sourceTopCharForTesting
        v.clickWrapForTesting()
        XCTAssertTrue(v.isWrapping)
        XCTAssertEqual(v.toolNamesForTesting.first, "取消折行")
        XCTAssertEqual(v.wrapTintForTesting, .controlAccentColor)
        w.layoutIfNeeded()
        XCTAssertEqual(v.sourceTopCharForTesting, top, "打开折行后顶端仍是同一行")
        v.clickWrapForTesting()
        w.layoutIfNeeded()
        XCTAssertEqual(v.sourceTopCharForTesting, top, "关掉折行后顶端仍是同一行")
        v.clickWrapForTesting()
        open("/p/b.txt", "short")
        XCTAssertTrue(v.isWrapping, "换文件沿用这次的选择")
        let md = "# 标题\n\n正文\n"
        v.begin(path: "/p/a.md", root: "/p")
        v.show(reply: ["ok": true, "size": md.utf8.count, "binary": false, "text": md, "truncated": false, "lossy": false], keepScroll: false)
        XCTAssertNotNil(v.renderedText)
        XCTAssertEqual(v.toolNamesForTesting, ["显示源码", "插入到终端", "隐藏文件树", "更多"], "渲染视图里没有折行，别的按钮照旧")
        v.toggleRenderForTesting()
        XCTAssertEqual(v.toolNamesForTesting.first, "取消折行", "切回源码沿用选择")
        v.begin(path: "/p/a.bin", root: "/p")
        v.show(reply: ["ok": true, "size": 2048, "binary": true], keepScroll: false)
        XCTAssertEqual(v.toolNamesForTesting, ["插入到终端", "隐藏文件树", "更多"])
        open("/p/c.txt", "short")
        v.clickWrapForTesting()
        XCTAssertFalse(v.isWrapping)
        open("/p/d.txt", "short")
        XCTAssertFalse(v.isWrapping, "关掉后换文件也沿用")
    }

    /// 看着一个文本文件时从内容搜索跳到另一个文件的某行：要等新文件的文字到了再滚，不能滚在旧文件上丢掉这一跳（2026-11-23 F2 单测发现）。
    func testRevealWaitsForNewFileText() {
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        v.begin(path: "/p/a.txt", root: "/p")
        v.show(reply: ["ok": true, "size": 6, "binary": false, "text": "l1\nl2\n", "truncated": false, "lossy": false], keepScroll: false)
        v.begin(path: "/p/b.txt", root: "/p")
        v.reveal(line: 3)
        XCTAssertNil(v.revealedLine, "旧文件的文字不滚")
        v.show(reply: ["ok": true, "size": 6, "binary": false, "text": "x\ny\nz\n", "truncated": false, "lossy": false], keepScroll: false)
        XCTAssertEqual(v.revealedLine, 3)
    }

    /// 补测问题 2（2026-11-28）：打开开头是中文的文件后，再打开别的文本文件仍用等宽字体，不跟着上一个文件的首字变成中文字体。
    func testSourceFontStaysMonoAfterFileStartingWithChinese() {
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        let w = NSWindow(contentRect: v.frame, styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = v
        func show(_ path: String, _ text: String) {
            v.begin(path: path, root: "/p")
            v.show(reply: ["ok": true, "size": text.utf8.count, "binary": false, "text": text, "truncated": false, "lossy": false], keepScroll: false)
            w.layoutIfNeeded()
        }
        let mono = ScyllaUIStyle.mono(12)
        show("/p/a.txt", "abc def\nline 2\n")
        XCTAssertEqual(v.sourceFontForTesting(at: 0)?.fontName, mono.fontName, "第一次打开是等宽")
        show("/p/cn.txt", "中文笔记\nwidget abc\n")
        XCTAssertEqual(v.sourceFontForTesting(at: 5)?.fontName, mono.fontName, "中文文件里的英文也是等宽")
        show("/p/a.txt", "abc def\nline 2\n")
        XCTAssertEqual(v.sourceFontForTesting(at: 0)?.fontName, mono.fontName, "再打开英文文件仍是等宽")
        XCTAssertEqual(v.sourceFontForTesting(at: 8)?.fontName, mono.fontName)
    }

    /// 补测问题 3（2026-11-28）：先打开超过 1 MB 的文件（跳的行在前 1 MB 之外、没跳），再从内容搜索跳到 Markdown 第 289 行，
    /// 要停在第 289 行，不能落到文件末尾。
    func testRevealLineAfterHugeFileDoesNotFallToEnd() {
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 500))
        let w = NSWindow(contentRect: v.frame, styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = v
        w.layoutIfNeeded()
        let huge = (1...19000).map { String(format: "%07d", $0) + " filler filler filler filler filler filler filler" }.joined(separator: "\n")
        v.begin(path: "/p/huge.txt", root: "/p")
        v.reveal(line: 21000)
        v.show(reply: ["ok": true, "size": 1254026, "binary": false, "text": huge, "truncated": true, "lossy": false], keepScroll: false)
        RunLoop.main.run(until: Date().addingTimeInterval(0.1))
        var lines = ["# Long Markdown Guide", "", "Intro paragraph.", ""]
        let para = String(repeating: "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ", count: 4)
        for i in 1...30 {
            lines += ["## Section \(i)", "", "Paragraph \(i). " + para, "", "- item \(i).1", "- item \(i).2", "  - nested \(i).2.1",
                      "1. first", "", "| Col A | Col B | Col C |", "|:---|:---:|---:|", "| a\(i) | b\(i) | c\(i) |", "", "> quote in section \(i)", ""]
            if i < 30 { lines += ["---", ""] }
        }
        let md = lines.joined(separator: "\n")
        let line = lines.firstIndex(of: "## Section 22")! + 1
        v.begin(path: "/p/guide.md", root: "/p")
        v.reveal(line: line)
        v.show(reply: ["ok": true, "size": md.utf8.count, "binary": false, "text": md, "truncated": false, "lossy": false], keepScroll: false)
        RunLoop.main.run(until: Date().addingTimeInterval(0.2))
        XCTAssertEqual(v.revealedLine, line)
        let target = (md as NSString).range(of: "## Section 22").location
        XCTAssertTrue(NSLocationInRange(target, v.sourceVisibleRangeForTesting), "停在第 \(line) 行，看得见的是 \(v.sourceVisibleRangeForTesting)，共 \((md as NSString).length)")
    }

    private func viewerPanel(width: CGFloat = 700) -> FilesPanel {
        let panel = FilesPanel(frame: NSRect(x: 0, y: 0, width: width, height: 600))
        panel.request = { req, done in
            switch req["type"] as! String {
            case "fs.list": done(["ok": true, "items": [["name": "a.txt", "kind": "file"], ["name": "b.txt", "kind": "file"]], "more": 0])
            default: done(["ok": true, "size": 3, "binary": false, "text": "abc", "truncated": false, "lossy": false])
            }
        }
        panel.setRoot("/p", gitRoot: nil, currentDir: nil)
        panel.openFile("/p/a.txt")
        return panel
    }

    /// F3 分隔线：按鼠标位置累计；树窄到 160 以下隐藏，同一次拖回来出现；最宽到查看器剩 160；松手记下树宽；双击回到 280。
    func testSplitDragHidesAndRestoresTree() {
        let panel = viewerPanel()
        var saved: [CGFloat] = []
        panel.onTreeWidth = { saved.append($0) }
        XCTAssertEqual(panel.treeWidth, 280)
        panel.dragSplitForTesting(100, done: false)
        XCTAssertEqual(panel.treeWidth, 180)
        XCTAssertFalse(panel.treeHidden)
        panel.dragSplitForTesting(30, done: false)
        XCTAssertTrue(panel.treeHidden, "窄到 160 以下树隐藏")
        XCTAssertEqual(panel.treeWidth, 180, "隐藏时树宽保留隐藏前的")
        panel.dragSplitForTesting(-60, done: false)
        XCTAssertFalse(panel.treeHidden, "同一次拖动里拖回来，树出现")
        XCTAssertEqual(panel.treeWidth, 210)
        panel.dragSplitForTesting(0, done: true)
        XCTAssertEqual(saved, [210])
        panel.dragSplitForTesting(-1000, done: true)
        XCTAssertEqual(panel.treeWidth, 540, "最宽到查看器剩 160（700 − 160）")
        panel.dragSplitForTesting(400, done: true)
        XCTAssertTrue(panel.treeHidden)
        panel.treeHidden = false
        XCTAssertEqual(panel.treeWidth, 540, "用按钮恢复时沿用隐藏前的树宽")
    }

    /// 左边缘拖动（F3 补改）：查看器窄到 160 以下收起，栏宽仍跟着鼠标（只剩树，最窄 280）；收起后要拖回到「树宽 + 176」以上、且放得下才展开。
    func testEdgeDragCollapsesAtViewerMinAndFollowsMouse() {
        func drag(_ w: CGFloat, collapsed: Bool, room: CGFloat = 1000) -> (Bool, CGFloat) {
            let r = FilesPanel.edgeDrag(width: w, treeWidth: 280, collapsed: collapsed, room: room)
            return (r.collapsed, r.width)
        }
        XCTAssertTrue(drag(441, collapsed: false) == (false, 441), "查看器还剩 161，不收起")
        XCTAssertTrue(drag(440, collapsed: false) == (false, 440), "正好 160，不收起")
        XCTAssertTrue(drag(439, collapsed: false) == (true, 439), "窄到 160 以下收起，栏宽不跳")
        XCTAssertTrue(drag(350, collapsed: true) == (true, 350), "收起后继续往右拖，栏宽跟着变窄")
        XCTAssertTrue(drag(200, collapsed: true) == (true, 280), "只剩树时最窄 280")
        XCTAssertTrue(drag(450, collapsed: true) == (true, 450), "拖回到树宽 + 160 还不够，免得临界处反复")
        XCTAssertTrue(drag(456, collapsed: true) == (false, 456), "拖回到树宽 + 176 展开")
        XCTAssertTrue(drag(600, collapsed: true, room: 450) == (true, 450), "放不下展开后的宽度时保持收起，宽度到可用为止")
        XCTAssertTrue(drag(900, collapsed: false, room: 700) == (false, 700), "展开时最宽到可用宽度")
    }

    /// 收起、展开的过渡：窗口可见且没设减弱动态效果时，树从树宽滑到整栏宽盖住查看器（查看器过渡中仍显示），0.2 秒后到位；
    /// 设了减弱动态效果时直接切换。
    func testCollapseSlidesTreeOverViewer() {
        let panel = viewerPanel()
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 700, height: 600), styleMask: [.titled], backing: .buffered, defer: false)
        window.contentView = panel
        window.orderFront(nil)
        var clock: CFTimeInterval = 1000
        FilesPanel.reduceMotion = { false }
        FilesPanel.now = { clock }
        defer {
            window.orderOut(nil)
            FilesPanel.reduceMotion = { NSWorkspace.shared.accessibilityDisplayShouldReduceMotion }
            FilesPanel.now = CACurrentMediaTime
        }
        func at(_ t: CFTimeInterval) -> CGFloat {
            clock = 1000 + t
            panel.stepSlideForTesting()
            panel.layoutSubtreeIfNeeded()
            return panel.treeColumnWidthForTesting
        }
        panel.layoutSubtreeIfNeeded()
        XCTAssertEqual(panel.treeColumnWidthForTesting, 280, accuracy: 0.5)
        panel.contentCollapsed = true
        XCTAssertTrue(panel.slidingForTesting)
        XCTAssertFalse(panel.viewer.isHidden, "过渡中查看器仍在下面")
        let mid = at(0.08)
        XCTAssertTrue(mid > 281 && mid < 699, "过渡中途树宽在树宽与整栏之间：\(mid)")
        XCTAssertEqual(at(0.25), 700, accuracy: 0.5, "收起后树占满整栏")
        XCTAssertFalse(panel.slidingForTesting)
        XCTAssertTrue(panel.viewer.isHidden)
        clock = 2000
        panel.contentCollapsed = false
        XCTAssertTrue(panel.slidingForTesting, "展开也有过渡")
        clock = 2000
        let back = at(1000.08)
        XCTAssertTrue(back > 281 && back < 699, "展开中途：\(back)")
        XCTAssertEqual(at(1000.25), 280, accuracy: 0.5, "展开后树退回原宽")
        XCTAssertFalse(panel.viewer.isHidden)
        FilesPanel.reduceMotion = { true }
        panel.contentCollapsed = true
        XCTAssertFalse(panel.slidingForTesting, "减弱动态效果时直接切换")
        XCTAssertTrue(panel.viewer.isHidden)
    }

    /// F3 查看器收起：只剩树，文件仍打开；栏头出现「显示文件内容」，没有放大；点文件或按钮展开；关文件后清掉；每个根各记一份。
    func testContentCollapseAndExpand() {
        let panel = viewerPanel()
        XCTAssertFalse(panel.expandShown)
        panel.contentCollapsed = true
        XCTAssertTrue(panel.viewerOpen)
        XCTAssertFalse(panel.contentShown)
        XCTAssertTrue(panel.viewer.isHidden)
        XCTAssertTrue(panel.expandShown)
        XCTAssertFalse(panel.zoomShown)
        XCTAssertEqual(panel.closeName, "关闭文件", "文件仍打开，关闭先关文件")
        panel.setRoot("/q", gitRoot: nil, currentDir: nil)
        XCTAssertFalse(panel.contentCollapsed)
        panel.setRoot("/p", gitRoot: nil, currentDir: nil)
        XCTAssertTrue(panel.contentCollapsed, "切回原根保持收起")
        panel.openFile("/p/b.txt")
        XCTAssertTrue(panel.contentShown, "点文件展开")
        XCTAssertFalse(panel.viewer.isHidden)
        panel.contentCollapsed = true
        panel.closeFileFromHeaderForTesting()
        XCTAssertFalse(panel.viewerOpen)
        panel.openFile("/p/a.txt")
        XCTAssertTrue(panel.contentShown, "关文件后收起状态清掉")
        panel.treeHidden = true
        panel.contentCollapsed = true
        panel.treeHidden = false
        XCTAssertTrue(panel.contentCollapsed)
    }
}

/// 假后台：应答先攒着、`flush` 时才回（模拟在途）；同时在途超过 8 个回 `busy`（同 `files.rs` 的 `MAX_INFLIGHT`）；`down` 时回 nil（断线）。
private final class LimitedBackend {
    var fs: [String: [[String: Any]]] = [:]
    var files: [String: String] = [:]
    var down = false
    private(set) var busyReplies = 0
    private var inflight = 0
    private var pending: [() -> Void] = []

    func send(_ req: [String: Any], _ done: @escaping ([String: Any]?) -> Void) {
        if down { return done(nil) }
        guard inflight < 8 else {
            busyReplies += 1
            return done(["ok": false, "error": ["code": "busy"]])
        }
        inflight += 1
        let reply: [String: Any]
        switch req["type"] as! String {
        case "fs.list": reply = ["ok": true, "items": fs[req["dir"] as! String] ?? [], "more": 0]
        default:
            let text = files[req["path"] as! String] ?? ""
            reply = ["ok": true, "size": text.utf8.count, "binary": false, "text": text, "truncated": false, "lossy": false]
        }
        pending.append { [weak self] in
            self?.inflight -= 1
            done(reply)
        }
    }

    /// 把攒着的应答（以及应答回来后补发的请求的应答）都回完。
    func flush() {
        while !pending.isEmpty {
            let p = pending.removeFirst()
            p()
        }
    }
}
