// Markdown 渲染的单测（docs/design.md 5.12 节「Markdown」）：各类块与行内样式、表格画成网格、图片内嵌与上限、链接点击规则，
// 以及查看器里渲染 / 源码的切换与记忆。

import AppKit
import XCTest

final class MarkdownRenderTests: XCTestCase {
    private static let asyncRender = FileViewer.renderWork

    override func setUp() {
        super.setUp()
        // 查看器的渲染默认在后台队列；这里同步做，断言不用等。
        FileViewer.renderWork = { work, done in done(work()) }
    }

    override func tearDown() {
        MarkdownRender.loadImage = MarkdownRender.defaultLoadImage
        FileViewer.renderWork = Self.asyncRender
        super.tearDown()
    }

    private func attrs(_ r: NSAttributedString, at needle: String) -> [NSAttributedString.Key: Any] {
        let range = (r.string as NSString).range(of: needle)
        XCTAssertNotEqual(range.location, NSNotFound, "找不到「\(needle)」于：\(r.string)")
        return range.location == NSNotFound ? [:] : r.attributes(at: range.location, effectiveRange: nil)
    }

    private func font(_ r: NSAttributedString, _ needle: String) -> NSFont? { attrs(r, at: needle)[.font] as? NSFont }
    private func bold(_ f: NSFont?) -> Bool { f.map { NSFontManager.shared.traits(of: $0).contains(.boldFontMask) } ?? false }

    func testBlocks() {
        let r = MarkdownRender.render("# 标题一\n\n## 标题二\n\n正文段落。\n\n> 引用的话\n\n---\n\n```swift\nlet a = 1\n```\n")
        XCTAssertEqual(font(r, "标题一")?.pointSize, 20)
        XCTAssertTrue(bold(font(r, "标题一")))
        XCTAssertEqual(font(r, "标题二")?.pointSize, 17)
        XCTAssertEqual(font(r, "正文段落")?.pointSize, MarkdownRender.bodySize)
        XCTAssertFalse(bold(font(r, "正文段落")))
        XCTAssertEqual(attrs(r, at: "引用的话")[.foregroundColor] as? NSColor, .secondaryLabelColor)
        XCTAssertTrue(r.string.contains("▎ 引用的话"), "引用左边一道竖线")
        XCTAssertTrue(r.string.contains("─\n"), "分隔线单独一行")
        XCTAssertEqual(font(r, "let a = 1")?.fontName, MarkdownRender.mono.fontName, "代码块等宽")
        let code = MarkdownRender.render("```\naa\nbb\n```\n\n后文")
        XCTAssertTrue(code.string.contains("aa\nbb\n后文"), "代码换行照常，拷出去不变：\(code.string.debugDescription)")
        XCTAssertEqual((attrs(code, at: "aa")[.paragraphStyle] as? NSParagraphStyle)?.paragraphSpacing, 0, "代码行间不拉开")
        XCTAssertEqual((attrs(code, at: "bb")[.paragraphStyle] as? NSParagraphStyle)?.paragraphSpacing, 8, "代码块后留段间距")
        XCTAssertFalse(r.string.hasSuffix("\n"), "结尾不留空行")
        XCTAssertFalse(r.string.contains("```"), "不显示源码标记")
    }

    func testListsKeepOrdinalsAndNest() {
        let r = MarkdownRender.render("- 苹果\n- 香蕉\n  - 小香蕉\n\n3. 第三\n4. 第四\n")
        let s = r.string
        XCTAssertTrue(s.contains("• 苹果"), s)
        XCTAssertTrue(s.contains("• 香蕉"), s)
        XCTAssertTrue(s.contains("3. 第三"), "有序列表用原序号：\(s)")
        XCTAssertTrue(s.contains("4. 第四"), s)
        let outer = attrs(r, at: "香蕉")[.paragraphStyle] as? NSParagraphStyle
        let inner = attrs(r, at: "小香蕉")[.paragraphStyle] as? NSParagraphStyle
        XCTAssertGreaterThan(inner?.headIndent ?? 0, outer?.headIndent ?? 0, "嵌套一层多缩进一层")
    }

    func testInlineStyles() {
        let r = MarkdownRender.render("有 **粗体**、*斜体*、`code` 和 ~~删掉~~。")
        XCTAssertTrue(bold(font(r, "粗体")))
        XCTAssertTrue(font(r, "斜体").map { NSFontManager.shared.traits(of: $0).contains(.italicFontMask) } ?? false)
        XCTAssertEqual(font(r, "code")?.fontName, MarkdownRender.mono.fontName)
        XCTAssertNotNil(attrs(r, at: "删掉")[.strikethroughStyle])
        XCTAssertFalse(r.string.contains("**"))
    }

    func testLinksResolveAndOnlyWebOpens() {
        let base = URL(fileURLWithPath: "/p/docs", isDirectory: true)
        let r = MarkdownRender.render("[官网](https://example.com) 与 [说明](usage.md)", baseURL: base)
        XCTAssertEqual(attrs(r, at: "官网")[.link] as? URL, URL(string: "https://example.com"))
        XCTAssertNil(attrs(r, at: "官网")[.toolTip], "网址不加提示")
        let local = attrs(r, at: "说明")[.link] as? URL
        XCTAssertEqual(local?.path, "/p/docs/usage.md", "相对链接按文件所在目录补全")
        XCTAssertEqual(attrs(r, at: "说明")[.toolTip] as? String, "暂不支持打开这类链接：/p/docs/usage.md")
        XCTAssertTrue(MarkdownRender.opensOnClick(URL(string: "HTTPS://a.b")!))
        XCTAssertTrue(MarkdownRender.opensOnClick(URL(string: "mailto:a@b.c")!))
        XCTAssertFalse(MarkdownRender.opensOnClick(URL(fileURLWithPath: "/tmp/run.command")))
        XCTAssertFalse(MarkdownRender.opensOnClick(URL(string: "javascript:alert(1)")!))
    }

    /// 表格画成网格：每格一个段落，挂同一张 `NSTextTable`；表头加粗；列对齐照 Markdown；单元格里的行内格式保留。
    func testTableRendersAsGrid() {
        let t = MarkdownRender.render("| 名 | 值 |\n|:--|--:|\n| **a** | 1 |\n| b | 2 |\n\n后面的段落")
        let s = t.string
        XCTAssertFalse(s.contains("|"), "不再按源码列出：\(s.debugDescription)")
        XCTAssertFalse(s.contains("---"))
        func block(_ needle: String) -> NSTextTableBlock? {
            (attrs(t, at: needle)[.paragraphStyle] as? NSParagraphStyle)?.textBlocks.first as? NSTextTableBlock
        }
        let header = block("名"), cellA = block("a"), cell2 = block("2")
        XCTAssertNotNil(header)
        XCTAssertTrue(header?.table === cell2?.table, "同一张表")
        XCTAssertEqual(header?.table.numberOfColumns, 2)
        XCTAssertEqual(header?.startingRow, 0)
        XCTAssertEqual(cellA?.startingRow, 1)
        XCTAssertEqual(cell2?.startingRow, 2)
        XCTAssertEqual(cell2?.startingColumn, 1)
        XCTAssertTrue(bold(font(t, "名")), "表头加粗")
        XCTAssertFalse(bold(font(t, "2")))
        XCTAssertTrue(bold(font(t, "a")), "单元格里的粗体保留")
        XCTAssertEqual((attrs(t, at: "2")[.paragraphStyle] as? NSParagraphStyle)?.alignment, .right, "列对齐")
        XCTAssertEqual((attrs(t, at: "名")[.paragraphStyle] as? NSParagraphStyle)?.alignment, .left)
        XCTAssertNil((attrs(t, at: "后面的段落")[.paragraphStyle] as? NSParagraphStyle)?.textBlocks.first, "表格后的段落不在表里")
    }

    /// 图片：能显示的放进正文（附件，按栏宽缩放）；不能显示的写替代文字和原因，没有替代文字时写「图片」。
    func testImagesInlineOrExplained() {
        var asked: [URL] = []
        MarkdownRender.loadImage = { url in
            asked.append(url)
            return url.lastPathComponent == "ok.png" ? .image(NSImage(size: NSSize(width: 40, height: 20))) : .unavailable("找不到这张图片")
        }
        let r = MarkdownRender.render("图：![架构](ok.png) ![缺](gone.png) ![](x.png)", baseURL: URL(fileURLWithPath: "/p", isDirectory: true))
        XCTAssertEqual(asked.map(\.path), ["/p/ok.png", "/p/gone.png", "/p/x.png"], "相对路径按文件所在目录补全")
        let i = (r.string as NSString).range(of: "\u{FFFC}").location
        XCTAssertNotEqual(i, NSNotFound)
        let att = r.attribute(.attachment, at: i, effectiveRange: nil) as? NSTextAttachment
        XCTAssertTrue(att?.attachmentCell is ScaledImageCell)
        XCTAssertEqual(r.attribute(.toolTip, at: i, effectiveRange: nil) as? String, "架构")
        XCTAssertTrue(r.string.contains("「缺」（找不到这张图片）"), r.string)
        XCTAssertTrue(r.string.contains("「图片」（找不到这张图片）"), r.string)
    }

    /// 读图片的规则：网络图片、找不到、超过 20 MB、超过 5000 万像素、不是图片都不显示；正常的按原尺寸。
    func testDefaultImageLoaderLimits() throws {
        func reason(_ l: MarkdownRender.ImageLoad) -> String? { if case .unavailable(let r) = l { return r }; return nil }
        XCTAssertEqual(reason(MarkdownRender.defaultLoadImage(URL(string: "https://a.b/c.png")!)), "网络图片不加载")
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("md-img-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        XCTAssertEqual(reason(MarkdownRender.defaultLoadImage(dir.appendingPathComponent("none.png"))), "找不到这张图片")
        let text = dir.appendingPathComponent("a.png")
        try Data("not an image".utf8).write(to: text)
        XCTAssertEqual(reason(MarkdownRender.defaultLoadImage(text)), "不是能显示的图片")
        let big = dir.appendingPathComponent("big.png")
        FileManager.default.createFile(atPath: big.path, contents: nil)
        let h = try FileHandle(forWritingTo: big)
        try h.truncate(atOffset: UInt64(MarkdownRender.imageMaxBytes) + 1)
        try h.close()
        XCTAssertEqual(reason(MarkdownRender.defaultLoadImage(big)), "图片超过 20 MB，不显示")
        let ok = dir.appendingPathComponent("ok.png")
        try Self.png(width: 30, height: 10).write(to: ok)
        if case .image(let img) = MarkdownRender.defaultLoadImage(ok) {
            XCTAssertEqual(img.size, NSSize(width: 30, height: 10), "原尺寸，1 像素 = 1 点")
        } else { XCTFail("小图应能显示") }
        XCTAssertTrue(MarkdownRender.exceedsPixels(width: 8000, height: 8000))
        XCTAssertFalse(MarkdownRender.exceedsPixels(width: 5000, height: 10000), "正好 5000 万像素不算超")
    }

    /// 查看器的预览上限（F4）：图片 20 MB、5000 万像素（与 Markdown 里的图片同一组），PDF 50 MB；正好等于上限不算超；
    /// 读不到尺寸时交给 Quick Look。像素只读文件头。
    func testPreviewRefusalLimits() throws {
        XCTAssertNil(FileViewer.previewRefusal(path: "/a/b.pdf", size: 50_000_000, pixels: nil))
        XCTAssertEqual(FileViewer.previewRefusal(path: "/a/b.pdf", size: 50_000_001, pixels: nil), "PDF 超过 50 MB，不预览")
        XCTAssertNil(FileViewer.previewRefusal(path: "/a/b.PNG", size: 20_000_000, pixels: (5000, 10000)))
        XCTAssertEqual(FileViewer.previewRefusal(path: "/a/b.png", size: 20_000_001, pixels: nil), "图片超过 20 MB，不预览")
        XCTAssertEqual(FileViewer.previewRefusal(path: "/a/b.jpg", size: 1_000, pixels: (8000, 8000)), "图片超过 5000 万像素，不预览")
        XCTAssertNil(FileViewer.previewRefusal(path: "/a/b.jpg", size: 1_000, pixels: nil), "读不到尺寸时交给 Quick Look")
        XCTAssertNil(FileViewer.previewRefusal(path: "/a/b.pdf", size: 1_000, pixels: (9000, 9000)), "PDF 不看像素")
        let url = FileManager.default.temporaryDirectory.appendingPathComponent("px-\(UUID().uuidString).png")
        try Self.png(width: 30, height: 10).write(to: url)
        defer { try? FileManager.default.removeItem(at: url) }
        XCTAssertTrue(MarkdownRender.pixelSize(url).map { $0 == (30, 10) } ?? false)
        XCTAssertNil(MarkdownRender.pixelSize(url.deletingLastPathComponent().appendingPathComponent("none.png")))
    }

    /// 图片按栏宽等比缩小，不放大。
    func testScaledImageCellFitsWidth() {
        let container = NSTextContainer(size: NSSize(width: 300, height: 1000))
        container.lineFragmentPadding = 5
        let cell = ScaledImageCell(imageCell: NSImage(size: NSSize(width: 1000, height: 500)))
        let f = cell.cellFrame(for: container, proposedLineFragment: NSRect(x: 0, y: 0, width: 300, height: 20), glyphPosition: .zero, characterIndex: 0)
        XCTAssertEqual(f.size, NSSize(width: 288, height: 144))
        let small = ScaledImageCell(imageCell: NSImage(size: NSSize(width: 40, height: 20)))
        XCTAssertEqual(small.cellFrame(for: container, proposedLineFragment: NSRect(x: 0, y: 0, width: 300, height: 20), glyphPosition: .zero, characterIndex: 0).size,
                       NSSize(width: 40, height: 20), "小图不放大")
    }

    /// 真的 PNG（`CGImageDestination` 写）。
    static func png(width: Int, height: Int) -> Data {
        let ctx = CGContext(data: nil, width: width, height: height, bitsPerComponent: 8, bytesPerRow: 0,
                            space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
        ctx.setFillColor(CGColor(red: 1, green: 0, blue: 0, alpha: 1))
        ctx.fill(CGRect(x: 0, y: 0, width: width, height: height))
        let data = NSMutableData()
        let dest = CGImageDestinationCreateWithData(data, "public.png" as CFString, 1, nil)!
        CGImageDestinationAddImage(dest, ctx.makeImage()!, nil)
        CGImageDestinationFinalize(dest)
        return data as Data
    }


    /// 放进真实窗口：渲染切回源码后，文字不压在行号栏下面（2026-11-23 测试版里行首被切掉）。
    func testSourceAfterRenderIsNotUnderRuler() {
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 400), styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = v
        v.begin(path: "/p/a.txt", root: "/p")
        v.show(reply: ["ok": true, "size": 3, "binary": false, "text": "abc", "truncated": false, "lossy": false], keepScroll: false)
        w.layoutIfNeeded()
        v.begin(path: "/p/a.md", root: "/p")
        v.show(reply: ["ok": true, "size": 9, "binary": false, "text": "# 标题\n\n正文", "truncated": false, "lossy": false], keepScroll: false)
        w.layoutIfNeeded()
        v.toggleRenderForTesting()
        w.layoutIfNeeded()
        XCTAssertNotNil(v.shownText)
        XCTAssertGreaterThanOrEqual(v.sourceTextLeft, v.rulerRight - 0.5, "文字从行号栏右边开始")
    }

    /// 第 7 版：有无法解码的字节时，渲染视图里的 U+FFFD 同样着提醒色（标题那句话才成立）。
    func testRenderedColorsUndecodable() throws {
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        let w = NSWindow(contentRect: v.frame, styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = v
        v.begin(path: "/p/gbk.md", root: "/p")
        v.show(reply: ["ok": true, "size": 12, "binary": false, "text": "# 标题\n\nx\u{FFFD}\u{FFFD}y", "truncated": false, "lossy": true], keepScroll: false)
        let r = try XCTUnwrap(v.renderedText)
        let i = (r.string as NSString).range(of: "\u{FFFD}").location
        XCTAssertNotEqual(i, NSNotFound)
        XCTAssertEqual(v.renderColorForTesting(at: i), ScyllaUIStyle.waiting)
        XCTAssertEqual(v.renderColorForTesting(at: i + 1), ScyllaUIStyle.waiting)
        XCTAssertNil(v.renderColorForTesting(at: i - 1))
    }

    func testIsMarkdown() {
        XCTAssertTrue(MarkdownRender.isMarkdown("/p/README.md"))
        XCTAssertTrue(MarkdownRender.isMarkdown("/p/a.MarkDown"))
        XCTAssertFalse(MarkdownRender.isMarkdown("/p/a.mdx"))
        XCTAssertFalse(MarkdownRender.isMarkdown("/p/a.txt"))
    }

    /// 查看器：Markdown 默认渲染，可切回源码并记住；超过 1 MB 只显示源码；内容搜索跳行时改看源码；不是 Markdown 没有切换。
    func testViewerRenderToggleAndMemory() {
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        let md: [String: Any] = ["ok": true, "size": 20, "binary": false, "text": "# 标题\n\n| a | b |\n|---|---|\n| 1 | 2 |\n", "truncated": false, "lossy": false]
        v.begin(path: "/p/a.md", root: "/p")
        v.show(reply: md, keepScroll: false)
        XCTAssertTrue(v.renderedText?.string.hasPrefix("标题") == true)
        XCTAssertTrue(v.renderToggleShown)
        XCTAssertFalse(v.toolNamesForTesting.contains("折行"), "渲染时没有折行按钮")
        v.toggleRenderForTesting()
        XCTAssertEqual(v.shownText, md["text"] as? String)
        XCTAssertNil(v.renderedText)
        XCTAssertTrue(v.renderToggleShown)
        v.begin(path: "/p/b.md", root: "/p")
        v.show(reply: ["ok": true, "size": 5, "binary": false, "text": "正文", "truncated": false, "lossy": false], keepScroll: false)
        XCTAssertEqual(v.renderedText?.string, "正文", "别的文件仍默认渲染")
        v.begin(path: "/p/a.md", root: "/p")
        v.show(reply: md, keepScroll: false)
        XCTAssertNotNil(v.shownText, "a.md 记住了源码")
        v.begin(path: "/p/big.md", root: "/p")
        v.show(reply: ["ok": true, "size": 3_000_000, "binary": false, "text": "# x", "truncated": true, "lossy": false], keepScroll: false)
        XCTAssertEqual(v.shownText, "# x")
        XCTAssertFalse(v.renderToggleShown)
        XCTAssertTrue(v.metaText.hasPrefix("超过 1 MB，不渲染 · 只显示前 1 MB · 共 "), "原因放在最前面，标题行窄时不被截掉：\(v.metaText)")
        v.begin(path: "/p/c.md", root: "/p")
        v.reveal(line: 1)
        v.show(reply: ["ok": true, "size": 3, "binary": false, "text": "# c", "truncated": false, "lossy": false], keepScroll: false)
        XCTAssertEqual(v.shownText, "# c", "内容搜索跳行时看源码")
        v.begin(path: "/p/d.txt", root: "/p")
        v.show(reply: ["ok": true, "size": 3, "binary": false, "text": "abc", "truncated": false, "lossy": false], keepScroll: false)
        XCTAssertFalse(v.renderToggleShown, "不是 Markdown 没有切换")
    }

    /// 渲染在后台做（2026-11-24 复查）：期间换了文件，先回来的旧结果丢掉；正在显示渲染时重读到同样的文字不重渲染；
    /// 被截断的只显示源码并写明原因，没被截断的都渲染。
    func testRenderIsAsyncDropsStaleAndSkipsSameText() {
        var pending: [() -> Void] = []
        var renders = 0
        FileViewer.renderWork = { work, done in
            renders += 1
            pending.append { done(work()) }
        }
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        func md(_ text: String) -> [String: Any] {
            ["ok": true, "size": text.utf8.count, "binary": false, "text": text, "truncated": false, "lossy": false]
        }
        v.begin(path: "/p/a.md", root: "/p")
        v.show(reply: md("# 甲"), keepScroll: false)
        XCTAssertTrue(v.renderPendingForTesting)
        XCTAssertNil(v.renderedText, "渲染没回来之前不显示渲染结果")
        v.begin(path: "/p/b.md", root: "/p")
        v.show(reply: md("# 乙"), keepScroll: false)
        pending[1]()
        XCTAssertEqual(v.renderedText?.string, "乙")
        pending[0]()
        XCTAssertEqual(v.renderedText?.string, "乙", "过期的结果被丢掉")
        XCTAssertFalse(v.renderPendingForTesting)
        // 文件被重写但文字没变：不重渲染。
        v.show(reply: md("# 乙"), keepScroll: true)
        XCTAssertEqual(renders, 2, "文字没变不重渲染")
        XCTAssertFalse(v.renderPendingForTesting)
        v.show(reply: md("# 乙改"), keepScroll: true)
        XCTAssertEqual(renders, 3)
        XCTAssertEqual(v.renderedText?.string, "乙", "新结果回来之前保留原内容")
        // 渲染在途时切到源码：结果回来也不再换成渲染。
        v.toggleRenderForTesting()
        XCTAssertEqual(v.shownText, "# 乙改")
        pending[2]()
        XCTAssertNil(v.renderedText, "切到源码后，在途的渲染结果丢掉")
        XCTAssertEqual(v.shownText, "# 乙改")
        // 被截断的（超过 1 MB）：直接显示源码，写明原因，没有切换按钮；没被截断的再大也渲染。
        v.begin(path: "/p/big.md", root: "/p")
        v.show(reply: ["ok": true, "size": 3_000_000, "binary": false, "text": "# 大", "truncated": true, "lossy": false], keepScroll: false)
        XCTAssertEqual(renders, 3, "被截断的不渲染")
        XCTAssertEqual(v.shownText, "# 大")
        XCTAssertFalse(v.renderToggleShown)
        XCTAssertTrue(v.metaText.hasPrefix("超过 1 MB，不渲染 · 只显示前 1 MB · 共 "), "原因放在最前面，标题行窄时不被截掉：\(v.metaText)")
        let long = "# 长\n\n" + String(repeating: "正文正文\n", count: 70_000)
        v.begin(path: "/p/long.md", root: "/p")
        v.show(reply: md(long), keepScroll: false)
        XCTAssertEqual(renders, 4, "没被截断的 900 多 KB 也渲染")
        pending[3]()
        XCTAssertTrue(v.renderedText?.string.hasPrefix("长") == true)
    }

    /// 大文件等渲染时先显示的源码只排看得见的部分（2026-11-24 测试版：679 KB 整篇排版在主线程上 0.6 秒，渲染结果排在它后面）；
    /// 切到源码也不整篇排（2026-11-25）。
    func testSourceFallbackLaysOutOnlyVisiblePart() {
        var pending: [() -> Void] = []
        FileViewer.renderWork = { work, done in pending.append { done(work()) } }
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        let long = "# 长\n\n" + String(repeating: "正文正文正文正文正文\n", count: 5_000)
        v.begin(path: "/p/long.md", root: "/p")
        v.show(reply: ["ok": true, "size": long.utf8.count, "binary": false, "text": long, "truncated": false, "lossy": false], keepScroll: false)
        RunLoop.main.run(until: Date().addingTimeInterval(0.3))
        XCTAssertNil(v.shownText, "0.6 秒内不先显示源码（2026-11-25 前 0.15 秒就显示，296 KB 的文件会先闪一下源码）")
        XCTAssertNil(v.renderedText)
        RunLoop.main.run(until: Date().addingTimeInterval(0.5))
        XCTAssertEqual(v.shownText, long, "超过 0.6 秒渲染还没回来，先显示源码")
        XCTAssertFalse(v.sourceFullyLaidOutForTesting, "先显示的源码不整篇排版")
        pending[0]()
        XCTAssertTrue(v.renderedText?.string.hasPrefix("长") == true)
        v.toggleRenderForTesting()
        XCTAssertEqual(v.shownText, long)
        XCTAssertFalse(v.sourceFullyLaidOutForTesting, "切到源码也不整篇排版")
    }

    /// 换文件时正文先留空：标题已换，正文不能还是上一个文件的（2026-11-25 测试版：新标题配着上一个文件的正文）。
    func testBeginClearsPreviousBody() {
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        v.begin(path: "/p/a.txt", root: "/p")
        v.show(reply: ["ok": true, "size": 3, "binary": false, "text": "abc", "truncated": false, "lossy": false], keepScroll: false)
        XCTAssertEqual(v.shownText, "abc")
        v.begin(path: "/p/a.txt", root: "/p")
        XCTAssertEqual(v.shownText, "abc", "同一个文件重读不留空")
        v.begin(path: "/p/b.txt", root: "/p")
        XCTAssertNil(v.shownText, "换文件后、新内容到之前不显示上一个文件的正文")
        XCTAssertNil(v.renderedText)
    }

    /// 切换后落在大致相同的内容处（2026-11-25 测试版：渲染滚到 40% 处切到源码停在第 1 行）：按顶端那一块对应的源码行，块内按比例。
    /// 渲染没好就切过去的（在途），好了再落位。
    func testToggleKeepsReadingPosition() {
        var pending: [() -> Void] = []
        var sync = true
        FileViewer.renderWork = { work, done in if sync { done(work()) } else { pending.append { done(work()) } } }
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 400), styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = v
        let text = (1...3000).map { "## 第 \($0) 节\n\n这一节的正文，带 **粗体** 与 `代码`。\n" }.joined(separator: "\n")
        let lines = (text as NSString).components(separatedBy: "\n")
        func lineOf(_ c: Int) -> Int { (text as NSString).substring(to: c).filter { $0 == "\n" }.count + 1 }
        let reply: [String: Any] = ["ok": true, "size": text.utf8.count, "binary": false, "text": text, "truncated": false, "lossy": false]
        v.begin(path: "/p/a.md", root: "/p")
        v.show(reply: reply, keepScroll: false)
        w.layoutIfNeeded()
        // 渲染 → 源码：渲染顶端是第 2000 节的标题，源码顶端是那一行。
        v.scrollRenderForTesting(toChar: (v.renderedText!.string as NSString).range(of: "第 2000 节").location)
        w.layoutIfNeeded(); w.displayIfNeeded()
        v.toggleRenderForTesting()
        w.layoutIfNeeded(); w.displayIfNeeded()
        let srcTop = lineOf(v.sourceVisibleRangeForTesting.location)
        XCTAssertEqual(lines[srcTop - 1], "## 第 2000 节", "源码顶端是第 \(srcTop) 行：\(lines[srcTop - 1])")
        // 源码 → 渲染（渲染视图已有这份，直接换回）：源码滚到第 1000 节，渲染顶端是那一节。
        v.scrollSourceForTesting(toChar: (text as NSString).range(of: "## 第 1000 节").location)
        w.layoutIfNeeded(); w.displayIfNeeded()
        v.toggleRenderForTesting()
        w.layoutIfNeeded(); w.displayIfNeeded()
        let rendered = v.renderedText!.string as NSString
        let want = rendered.range(of: "第 1000 节").location
        XCTAssertLessThan(abs(v.renderTopCharForTesting! - want), 12, "渲染顶端是第 1000 节：\(rendered.substring(with: NSRange(location: v.renderTopCharForTesting!, length: 8)))")
        // 渲染在途时从源码切过去：好了再落到对应处。
        v.toggleRenderForTesting()
        v.show(reply: ["ok": true, "size": 0, "binary": false, "text": text + "\n尾巴\n", "truncated": false, "lossy": false], keepScroll: true)
        v.scrollSourceForTesting(toChar: (text as NSString).range(of: "## 第 500 节").location)
        w.layoutIfNeeded(); w.displayIfNeeded()
        sync = false
        v.toggleRenderForTesting()
        XCTAssertEqual(pending.count, 1, "内容变了，重新渲染")
        pending[0]()
        w.layoutIfNeeded(); w.displayIfNeeded()
        let r2 = v.renderedText!.string as NSString
        XCTAssertLessThan(abs(v.renderTopCharForTesting! - r2.range(of: "第 500 节").location), 12, "渲染好了落到第 500 节")
    }

    /// 点内容搜索结果跳行：这一次用源码显示并跳到那一行，不改这个文件记住的选择（2026-11-25 前会记成源码，之后从文件树再点也是源码）。
    func testJumpUsesSourceOnceWithoutChangingChoice() {
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 400), styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = v
        let text = (1...300).map { "第 \($0) 行" }.joined(separator: "\n")
        func reply(_ t: String) -> [String: Any] { ["ok": true, "size": t.utf8.count, "binary": false, "text": t, "truncated": false, "lossy": false] }
        func open(_ path: String, line: Int? = nil, keepScroll: Bool = false, _ t: String = text) {
            // 与 FilesPanel.openFile / 文件变化重读同序。
            v.begin(path: path, root: "/p")
            if let line { v.reveal(line: line) }
            v.show(reply: reply(t), keepScroll: keepScroll)
            w.layoutIfNeeded()
        }
        func jumped(_ line: Int) -> Bool {
            v.shownText != nil && v.sourceSelectionForTesting.location == (text as NSString).range(of: "第 \(line) 行").location
        }
        open("/p/a.md")
        XCTAssertNotNil(v.renderedText, "没选过的默认渲染")
        open("/p/a.md", line: 120)
        XCTAssertTrue(jumped(120), "跳行时用源码并跳到那一行")
        open("/p/a.md", line: 200)
        XCTAssertTrue(jumped(200), "同一个文件连点第二条也跳")
        open("/p/a.md", keepScroll: true, text)
        XCTAssertNotNil(v.shownText, "文件被改了重读：仍是这一次的源码，不跳回渲染")
        open("/p/b.md")
        open("/p/a.md")
        XCTAssertNotNil(v.renderedText, "之后从文件树再点：按原来的选择（没选过默认渲染）")
        // 跳行后自己点「显示渲染」，再点下一条结果：仍切到源码跳行。
        open("/p/a.md", line: 50)
        XCTAssertTrue(jumped(50))
        v.toggleRenderForTesting()
        XCTAssertNotNil(v.renderedText)
        open("/p/a.md", line: 60)
        XCTAssertTrue(jumped(60), "点下一条仍切到源码跳行")
        // 自己选过源码的文件：跳行之后再打开仍是源码。
        open("/p/c.md")
        v.toggleRenderForTesting()
        XCTAssertNotNil(v.shownText)
        open("/p/c.md", line: 10)
        open("/p/b.md")
        open("/p/c.md")
        XCTAssertNotNil(v.shownText, "自己选的源码照旧")
    }

    /// 来回切换不重做（2026-11-25：每次切到渲染都重新解析、切到源码都重放文字并整篇排版，679 KB 每次约 1 秒）：
    /// 文字没变时只换显示的视图、落在对应的内容处；没显示过源码的第一次切过去也不整篇排；改了内容照旧重做。
    func testToggleReusesBothViews() {
        var renders = 0
        FileViewer.renderWork = { work, done in
            renders += 1
            done(work())
        }
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 400), styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = v
        let text = (1...3000).map { "## 第 \($0) 节\n\n正文 \($0)。\n" }.joined(separator: "\n")
        func md(_ t: String) -> [String: Any] {
            ["ok": true, "size": t.utf8.count, "binary": false, "text": t, "truncated": false, "lossy": false]
        }
        v.begin(path: "/p/long.md", root: "/p")
        v.show(reply: md(text), keepScroll: false)
        w.layoutIfNeeded()
        XCTAssertEqual(renders, 1)
        let renderTarget = (v.renderedText!.string as NSString).range(of: "第 1500 节").location
        v.scrollRenderForTesting(toChar: renderTarget)
        v.toggleRenderForTesting()
        XCTAssertEqual(v.shownText, text)
        XCTAssertFalse(v.sourceFullyLaidOutForTesting, "第一次切到源码不整篇排版")
        let sourceTarget = (text as NSString).range(of: "第 2500 节").location
        v.scrollSourceForTesting(toChar: sourceTarget)
        w.layoutIfNeeded()
        w.displayIfNeeded()
        // 只排可见部分时按坐标反查字符受估算影响，测的是「那个字符离顶端多远」（同 testRefreshKeepsTopCharacterInLongDocument）。
        XCTAssertLessThan(abs(v.sourceLineOffsetForTesting(char: sourceTarget)!), 30)
        for _ in 0..<3 {
            v.toggleRenderForTesting()
            w.layoutIfNeeded()
            w.displayIfNeeded()
            XCTAssertNotNil(v.renderedText)
            // 2026-11-25 起两边不再各记各的，切过去落在对应的内容处：源码停在第 2500 节，渲染也是。
            let want = (v.renderedText!.string as NSString).range(of: "第 2500 节").location
            XCTAssertLessThan(abs(v.renderTopCharForTesting! - want), 12, "渲染落在源码对应处")
            v.toggleRenderForTesting()
            w.layoutIfNeeded()
            w.displayIfNeeded()
            XCTAssertEqual(v.shownText, text)
            XCTAssertLessThan(abs(v.sourceLineOffsetForTesting(char: sourceTarget)!), 30, "来回切换后源码仍在第 2500 节")
        }
        XCTAssertEqual(renders, 1, "来回切换不重新解析")
        XCTAssertFalse(v.sourceFullyLaidOutForTesting)
        // 改了内容：切到渲染时重新解析。
        v.show(reply: md(text + "\n尾巴\n"), keepScroll: true)
        v.toggleRenderForTesting()
        XCTAssertEqual(renders, 2, "内容变了重新解析")
        XCTAssertTrue(v.renderedText?.string.hasSuffix("尾巴") == true)
        // 换了文件再回来：照旧重新渲染、回到开头。
        v.begin(path: "/p/b.md", root: "/p")
        v.show(reply: md("# 乙"), keepScroll: false)
        v.begin(path: "/p/long.md", root: "/p")
        v.show(reply: md(text + "\n尾巴\n"), keepScroll: false)
        w.layoutIfNeeded()
        XCTAssertEqual(renders, 4)
        XCTAssertEqual(v.renderTopCharForTesting, 0)
    }

    /// 源码视图改了内容重读：原来顶端那一行仍在顶端（只排可见部分后与渲染视图一样按字符保留）。
    func testSourceRefreshKeepsTopCharacter() {
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 400), styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = v
        let text = (1...20000).map { "第 \($0) 行" }.joined(separator: "\n")
        func reply(_ t: String) -> [String: Any] {
            ["ok": true, "size": t.utf8.count, "binary": false, "text": t, "truncated": false, "lossy": false]
        }
        v.begin(path: "/p/a.txt", root: "/p")
        v.show(reply: reply(text), keepScroll: false)
        w.layoutIfNeeded()
        let target = (text as NSString).range(of: "第 15000 行").location
        v.scrollSourceForTesting(toChar: target)
        w.layoutIfNeeded()
        w.displayIfNeeded()
        // 内容搜索跳行：跳到的那一行看得见（只排可见部分后仍对）。与文件栏同序：reveal 后重读。
        v.reveal(line: 5000)
        v.show(reply: reply(text), keepScroll: false)
        w.layoutIfNeeded()
        w.displayIfNeeded()
        let line5000 = (text as NSString).range(of: "第 5000 行").location
        XCTAssertTrue(NSLocationInRange(line5000, v.sourceVisibleRangeForTesting), "跳到第 5000 行：\(v.sourceVisibleRangeForTesting)")
        XCTAssertEqual(v.sourceSelectionForTesting.location, line5000)
        v.scrollSourceForTesting(toChar: target)
        w.layoutIfNeeded()
        w.displayIfNeeded()
        let before = v.sourceTopCharForTesting!
        XCTAssertLessThan(abs(before - target), 12, "顶端是滚到的那一行：\(before) 对 \(target)")
        XCTAssertLessThan(abs(v.sourceLineOffsetForTesting(char: before)!), 30)
        // 第一行改了但长度不变：原来顶端的字符位置不变。
        v.show(reply: reply("改 1 行" + text.dropFirst(5)), keepScroll: true)
        w.layoutIfNeeded()
        w.displayIfNeeded()
        // 看画出来的：可见范围从原来顶端那一行开始。
        let top = v.sourceVisibleRangeForTesting.location
        XCTAssertLessThan(abs(top - before), 12, "原来顶端那一行仍在顶端：\(top) 对 \(before)")
    }

    /// 不折行时能横向滚到最长一行的行尾（宽度按估算，不靠整篇排版）；横向滚到右边后打开折行，行首不被切掉（2026-11-25 测试版实测）。
    func testSourceWidthAndWrapToggle() {
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 400), styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = v
        var lines = (1...20000).map { "第 \($0) 行" }
        lines[15000] = String(repeating: "中文abc", count: 300) + "行尾"
        let text = lines.joined(separator: "\n")
        v.begin(path: "/p/a.txt", root: "/p")
        v.show(reply: ["ok": true, "size": text.utf8.count, "binary": false, "text": text, "truncated": false, "lossy": false], keepScroll: false)
        w.layoutIfNeeded()
        XCTAssertFalse(v.isWrapping)
        // 长行在后面、还没排到：横向滚到最右，长行的行尾在可见范围里。
        let end = (text as NSString).range(of: "行尾").location
        v.scrollSourceForTesting(toChar: end, x: 1e7)
        w.layoutIfNeeded()
        w.displayIfNeeded()
        XCTAssertTrue(v.sourceEndVisibleForTesting(char: end + 1), "看得到长行的行尾")
        XCTAssertFalse(v.sourceFullyLaidOutForTesting)
        v.clickWrapForTesting()
        // 打开折行后不会再有自动排版把宽度改对（真实窗口里如此），先不排版就查。
        XCTAssertTrue(v.sourceFitsWidthForTesting, "打开折行后正文宽度扣掉行号栏，行尾不被切掉")
        w.layoutIfNeeded()
        XCTAssertTrue(v.isWrapping)
        XCTAssertGreaterThanOrEqual(v.sourceTextLeft, v.rulerRight - 0.5, "打开折行后行首不压在行号栏下面、不被切掉")
        XCTAssertTrue(v.sourceFitsWidthForTesting, "打开折行后正文宽度扣掉行号栏，行尾不被切掉")
        // 拖窄、拖宽后仍然完整。
        for width in [420.0, 760.0] {
            w.setContentSize(NSSize(width: width, height: 400))
            w.layoutIfNeeded()
            XCTAssertTrue(v.sourceFitsWidthForTesting, "宽 \(width) 时行尾不被切掉")
        }
    }

    /// 文件已经打开时点内容搜索的结果（2026-11-25 测试版：回到开头、不跳行）：文件栏先 `reveal` 再重读一遍，
    /// 重读不能把跳到的那一行冲掉；同一个文件连点两条也都要跳。源码、渲染两种状态都测。
    func testRevealInAlreadyOpenFile() {
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 400), styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = v
        let text = (1...20000).map { "第 \($0) 行" }.joined(separator: "\n")
        let reply: [String: Any] = ["ok": true, "size": text.utf8.count, "binary": false, "text": text, "truncated": false, "lossy": false]
        func jump(_ line: Int) {
            // 与 FilesPanel.openFile 同序：begin、reveal、读到后 show（新打开，不保留滚动）。
            v.begin(path: v.path!, root: "/p")
            v.reveal(line: line)
            v.show(reply: reply, keepScroll: false)
            w.layoutIfNeeded()
            w.displayIfNeeded()
            let start = (text as NSString).range(of: "第 \(line) 行").location
            XCTAssertTrue(NSLocationInRange(start, v.sourceVisibleRangeForTesting), "跳到第 \(line) 行：\(v.sourceVisibleRangeForTesting)")
            XCTAssertEqual(v.sourceSelectionForTesting.location, start, "第 \(line) 行高亮")
        }
        for path in ["/p/a.txt", "/p/a.md"] {
            v.begin(path: path, root: "/p")
            v.show(reply: reply, keepScroll: false)
            w.layoutIfNeeded()
            jump(15000)
            jump(3000)
        }
    }

    /// 滚动条直接设到最底（只排可见部分时总高度是估的）：一次就到真正的末尾（2026-11-25 测试版：停在倒数十几行，再设一次才到）。
    func testScrollToBottomReachesEnd() {
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 400), styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = v
        // 长短不一的行、折行：估算的高度与实际差得多。
        var lines = (1...3000).map { "第 \($0) 行 " + String(repeating: "长短不一的内容", count: ($0 * 7) % 40) }
        lines[10] = String(repeating: "x", count: 2100)
        let text = lines.joined(separator: "\n")
        v.begin(path: "/p/a.txt", root: "/p")
        v.show(reply: ["ok": true, "size": text.utf8.count, "binary": false, "text": text, "truncated": false, "lossy": false], keepScroll: false)
        w.layoutIfNeeded()
        v.clickWrapForTesting() // 第 6 版 R1 起不再按行长自动折行，自己打开
        w.layoutIfNeeded()
        XCTAssertTrue(v.isWrapping)
        v.scrollToBottomForTesting(render: false)
        RunLoop.main.run(until: Date().addingTimeInterval(0.2))
        w.layoutIfNeeded()
        w.displayIfNeeded()
        let shown = v.sourceVisibleRangeForTesting
        XCTAssertEqual(NSMaxRange(shown), (text as NSString).length, "看得到最后一行：可见到 \(NSMaxRange(shown))")
        // 渲染视图同样。
        let md = (1...3000).map { "## 第 \($0) 节\n\n" + String(repeating: "长短不一的正文。", count: ($0 * 7) % 40) + "\n" }.joined(separator: "\n")
        v.begin(path: "/p/b.md", root: "/p")
        v.show(reply: ["ok": true, "size": md.utf8.count, "binary": false, "text": md, "truncated": false, "lossy": false], keepScroll: false)
        w.layoutIfNeeded()
        let total = v.renderedText!.length
        v.scrollToBottomForTesting(render: true)
        RunLoop.main.run(until: Date().addingTimeInterval(0.2))
        w.layoutIfNeeded()
        w.displayIfNeeded()
        XCTAssertEqual(v.renderVisibleEndForTesting, total, "渲染视图也看得到末尾")
    }

    /// 到底之后往上滚不被拉回（2026-11-25 方案会话复验：到底后滚轮往上、⌘↑ 都不动）。按真实顺序：先滚可见区（此刻滚动条的值还是旧的），
    /// 再让滚动条跟上。往上一小步、一大步、到顶都停在该停的位置；源码、渲染都测。
    func testLeaveBottomAfterReachingEnd() {
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 400), styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = v
        var lines = (1...3000).map { "第 \($0) 行 " + String(repeating: "长短不一的内容", count: ($0 * 7) % 40) }
        lines[10] = String(repeating: "x", count: 2100)
        let text = lines.joined(separator: "\n")
        let md = (1...3000).map { "## 第 \($0) 节\n\n" + String(repeating: "长短不一的正文。", count: ($0 * 7) % 40) + "\n" }.joined(separator: "\n")
        for (path, body, render) in [("/p/a.txt", text, false), ("/p/b.md", md, true)] {
            v.begin(path: path, root: "/p")
            v.show(reply: ["ok": true, "size": body.utf8.count, "binary": false, "text": body, "truncated": false, "lossy": false], keepScroll: false)
            w.layoutIfNeeded()
            v.scrollToBottomForTesting(render: render)
            RunLoop.main.run(until: Date().addingTimeInterval(0.1))
            w.layoutIfNeeded()
            w.displayIfNeeded()
            let end = v.clipMinYForTesting(render: render)
            for dy in [-40.0, -3000.0] {
                let before = v.clipMinYForTesting(render: render)
                v.scrollByForTesting(render: render, dy: dy)
                RunLoop.main.run(until: Date().addingTimeInterval(0.1))
                w.layoutIfNeeded()
                w.displayIfNeeded()
                // 往上一大步进到没排过的地方，排版后估算的高度会变，可见区跟着挪（单测里 500—1150 点，TextKit 只排可见部分时的行为，
                // 与跟底无关），所以大步只要求往上走了一半以上、没被拉回底部；小步要求原地不差。
                let got = v.clipMinYForTesting(render: render)
                if dy > -100 {
                    XCTAssertEqual(got, before + dy, accuracy: 1, "\(path) 往上 \(-dy) 停在那里（底部 \(end)）")
                } else {
                    XCTAssertLessThan(got, before + dy / 2, "\(path) 往上 \(-dy) 没被拉回底部：\(got)，滚之前 \(before)")
                }
            }
            v.scrollByForTesting(render: render, dy: -1e9)
            RunLoop.main.run(until: Date().addingTimeInterval(0.1))
            w.layoutIfNeeded()
            w.displayIfNeeded()
            XCTAssertLessThan(v.clipMinYForTesting(render: render), 1, "\(path) 滚到顶")
        }
    }

    /// 行号按字符位置数换行符（源码视图只排看得见的部分，不能靠排版）：开头、中间、末尾，末尾换行不另起一行。
    func testLineNumbersFromCharacters() {
        let text = "甲\n乙乙\n\n丁\n" as NSString
        let starts = LineNumberRuler.lineStarts(of: text)
        XCTAssertEqual(starts, [0, 2, 5, 6])
        XCTAssertEqual(LineNumberRuler.lineStarts(of: ""), [0])
        XCTAssertEqual(LineNumberRuler.lineStarts(of: "a"), [0])
        XCTAssertEqual(LineNumberRuler.lineIndex(of: 0, in: starts), 0)
        XCTAssertEqual(LineNumberRuler.lineIndex(of: 1, in: starts), 0, "换行符属于本行")
        XCTAssertEqual(LineNumberRuler.lineIndex(of: 3, in: starts), 1)
        XCTAssertEqual(LineNumberRuler.lineIndex(of: 5, in: starts), 2, "空行")
        XCTAssertEqual(LineNumberRuler.lineIndex(of: 7, in: starts), 3, "末尾")
        let big = (1...5000).map { "line \($0)" }.joined(separator: "\n") as NSString
        let bs = LineNumberRuler.lineStarts(of: big)
        XCTAssertEqual(bs.count, 5000)
        let mid = big.range(of: "line 2718").location
        XCTAssertEqual(LineNumberRuler.lineIndex(of: mid + 3, in: bs) + 1, 2718)
    }

    /// 不折行时的宽度按最长一行估算：全角按全角宽、代理对算一个全角、制表符算一个制表位。
    func testLineWidthEstimate() {
        XCTAssertEqual(FileViewer.lineWidth("ab\nabcd\n", narrow: 7, wide: 12, tab: 28), 28)
        XCTAssertEqual(FileViewer.lineWidth("中文a", narrow: 7, wide: 12, tab: 28), 31)
        XCTAssertEqual(FileViewer.lineWidth("\tx", narrow: 7, wide: 12, tab: 28), 35)
        XCTAssertEqual(FileViewer.lineWidth("😀", narrow: 7, wide: 12, tab: 28), 12)
        XCTAssertEqual(FileViewer.lineWidth("", narrow: 7, wide: 12, tab: 28), 0)
    }

    /// 源码视图原有的「文字没变不重设」不受影响：重读同样的文字，滚动位置不动、选区不丢。
    func testSourceViewKeepsSameText() {
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        let text = (1...300).map { "第 \($0) 行" }.joined(separator: "\n")
        let reply: [String: Any] = ["ok": true, "size": text.utf8.count, "binary": false, "text": text, "truncated": false, "lossy": false]
        v.begin(path: "/p/a.txt", root: "/p")
        v.show(reply: reply, keepScroll: false)
        v.selectSourceRangeForTesting(NSRange(location: 5, length: 3))
        v.show(reply: reply, keepScroll: true)
        XCTAssertEqual(v.sourceSelectionForTesting, NSRange(location: 5, length: 3), "同样的文字不重设，选区还在")
    }

    /// 长文档改了内容重渲染：原来顶端那一行仍在顶端（按字符保留，只排可见部分时按像素会对到别处，2026-11-24 测试版里跳到文末）。
    /// 按坐标反查字符同样受估算影响，所以测的是「那个字符离顶端多远」。
    func testRefreshKeepsTopCharacterInLongDocument() {
        let v = FileViewer(frame: NSRect(x: 0, y: 0, width: 600, height: 400))
        let w = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 600, height: 400), styleMask: [.titled], backing: .buffered, defer: false)
        w.contentView = v
        let body = (1...3000).map { "## 第 \($0) 节\n\n这一节的正文，带 **粗体** 与 `代码`。\n" }.joined(separator: "\n")
        func md(_ text: String) -> [String: Any] {
            ["ok": true, "size": text.utf8.count, "binary": false, "text": text, "truncated": false, "lossy": false]
        }
        v.begin(path: "/p/long.md", root: "/p")
        v.show(reply: md(body), keepScroll: false)
        w.layoutIfNeeded()
        XCTAssertEqual(v.renderTopCharForTesting, 0, "新打开停在开头")
        let target = (v.renderedText!.string as NSString).range(of: "第 2000 节").location
        v.scrollRenderForTesting(toChar: target)
        let before = v.renderTopCharForTesting!
        XCTAssertGreaterThan(before, target - 40)
        v.show(reply: md(body + "\n尾巴\n"), keepScroll: true)
        // 与真实窗口一样先排版、再画一次（只排可见部分时，位置在画之前还是估算的）。
        w.layoutIfNeeded()
        w.displayIfNeeded()
        let offset = v.renderLineOffsetForTesting(char: before)!
        XCTAssertLessThan(abs(offset), 30, "原来顶端那一行仍在顶端：偏了 \(offset) pt")
    }
}
