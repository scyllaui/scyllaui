// Markdown 渲染（docs/design.md 5.12 节「Markdown」）：系统 `AttributedString(markdown:)` 解析，按块读 `presentationIntent`，
// 拼成给只读 NSTextView（TextKit 1）用的带样式文本。表格用 `NSTextTable` 画成网格、按栏宽排；本地图片按栏宽缩小显示，
// 超过上限、找不到、网络图片不显示并写明原因。除了读本地图片（可替换的 `loadImage`），不碰界面与磁盘。

import AppKit
import ImageIO

enum MarkdownRender {
    static let bodySize: CGFloat = 13

    /// 图片上限（F4 实测后 2026-11-23 阿远定，查看器里的图片预览同用这一组）：文件 20 MB、5000 万像素。读文件头，不整张解码。
    static let imageMaxBytes: Int64 = 20_000_000
    static let imageMaxPixels = 50_000_000
    /// 解码时最长边最多取这么多像素（缩略图），显示尺寸仍按原图，栏不够宽就缩小。
    static let imageDecodeMaxPixel = 2400

    enum ImageLoad {
        case image(NSImage)
        /// 不显示，`reason` 写在替代文字后面。
        case unavailable(String)
    }

    /// 读图片；测试里换掉。
    static var loadImage: (URL) -> ImageLoad = defaultLoadImage

    /// 渲染后每一块对应源码的哪几行（NSValue 包的 NSRange：起始行从 1 起，`length` 是到结束行多几行）：查看器切换源码 / 渲染时按它对位置。
    static let sourceLinesKey = NSAttributedString.Key("dev.scyllaui.sourceLines")

    /// 用 `.md`、`.markdown` 判断（不区分大小写）。
    static func isMarkdown(_ path: String) -> Bool {
        ["md", "markdown"].contains((path as NSString).pathExtension.lowercased())
    }

    /// 点链接时直接打开的：网址与邮件。其他（本地文件、相对路径等）可能是脚本或 App，不打开。
    static func opensOnClick(_ url: URL) -> Bool {
        ["http", "https", "mailto"].contains(url.scheme?.lowercased() ?? "")
    }

    /// `baseURL`：文件所在目录，相对链接与图片按它补全。
    static func render(_ source: String, baseURL: URL? = nil) -> NSAttributedString {
        var options = AttributedString.MarkdownParsingOptions(interpretedSyntax: .full, failurePolicy: .returnPartiallyParsedIfPossible)
        // 带上每段文字在源码里的行号（`markdownSourcePosition`），记到渲染结果的 `sourceLinesKey` 上。
        options.appliesSourcePositionAttributes = true
        guard let parsed = try? AttributedString(markdown: source, options: options, baseURL: baseURL) else {
            return NSAttributedString(string: source, attributes: [.font: mono, .foregroundColor: NSColor.labelColor])
        }
        var b = Builder()
        for (intent, range) in parsed.runs[\.presentationIntent] {
            b.block(intent, parsed[range])
        }
        b.flushTable()
        return b.finish()
    }

    // MARK: 样式

    static var mono: NSFont { ScyllaUIStyle.mono(12) }
    static var body: NSFont { .systemFont(ofSize: bodySize) }

    static func headingFont(_ level: Int) -> NSFont {
        let sizes: [CGFloat] = [20, 17, 15, 14, 13, 13]
        return .boldSystemFont(ofSize: sizes[min(max(level, 1), 6) - 1])
    }

    // MARK: 图片

    static func exceedsPixels(width: Int, height: Int) -> Bool { width * height > imageMaxPixels }

    /// 图片的像素尺寸，只读文件头；读不到为 nil。
    static func pixelSize(_ url: URL) -> (width: Int, height: Int)? {
        guard let src = CGImageSourceCreateWithURL(url as CFURL, nil), CGImageSourceGetCount(src) > 0,
              let props = CGImageSourceCopyPropertiesAtIndex(src, 0, nil) as? [CFString: Any],
              let w = props[kCGImagePropertyPixelWidth] as? Int, let h = props[kCGImagePropertyPixelHeight] as? Int, w > 0, h > 0 else { return nil }
        return (w, h)
    }

    static func defaultLoadImage(_ url: URL) -> ImageLoad {
        guard url.isFileURL else { return .unavailable(String(localized: "网络图片不加载")) }
        guard let size = (try? FileManager.default.attributesOfItem(atPath: url.path))?[.size] as? NSNumber else {
            return .unavailable(String(localized: "找不到这张图片"))
        }
        if size.int64Value > imageMaxBytes {
            return .unavailable(String(localized: "图片超过 \(ByteCountFormatter.string(fromByteCount: imageMaxBytes, countStyle: .file))，不显示"))
        }
        guard let src = CGImageSourceCreateWithURL(url as CFURL, nil), CGImageSourceGetCount(src) > 0,
              let props = CGImageSourceCopyPropertiesAtIndex(src, 0, nil) as? [CFString: Any],
              let w = props[kCGImagePropertyPixelWidth] as? Int, let h = props[kCGImagePropertyPixelHeight] as? Int, w > 0, h > 0 else {
            return .unavailable(String(localized: "不是能显示的图片"))
        }
        if exceedsPixels(width: w, height: h) { return .unavailable(String(localized: "图片超过 5000 万像素，不显示")) }
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceThumbnailMaxPixelSize: imageDecodeMaxPixel,
        ]
        guard let thumb = CGImageSourceCreateThumbnailAtIndex(src, 0, options as CFDictionary) else {
            return .unavailable(String(localized: "不是能显示的图片"))
        }
        // 显示尺寸按原图（1 像素 = 1 点），缩略图只是为了少占内存。
        let k = CGFloat(max(w, h)) / CGFloat(max(thumb.width, thumb.height))
        return .image(NSImage(cgImage: thumb, size: NSSize(width: CGFloat(thumb.width) * k, height: CGFloat(thumb.height) * k)))
    }

    private struct Builder {
        let out = NSMutableAttributedString()
        private var seenItems = Set<Int>()
        /// 正在收集的表格：identity、各列对齐、单元格（行的顺序按出现先后，表头在前）。
        private var table: (id: Int, columns: [PresentationIntent.TableColumn], rows: [String], cells: [String: [Int: AttributedSubstring]], header: Set<String>)?
        /// 正在收集的表格覆盖的源码行。
        private var tableLines: (Int, Int)?

        /// 一段文字在源码里的起止行。
        private static func lines(_ text: AttributedSubstring) -> (Int, Int)? {
            var lo = Int.max, hi = 0
            for run in text.runs {
                guard let p = run.markdownSourcePosition else { continue }
                lo = min(lo, p.startLine)
                hi = max(hi, p.endLine)
            }
            return lo <= hi ? (lo, hi) : nil
        }

        /// 把 `start` 到末尾这一块标上源码行。
        private func mark(from start: Int, _ lines: (Int, Int)?) {
            guard let (lo, hi) = lines, out.length > start else { return }
            // 值用 NSValue（起始行、行数）。试过 `[lo, hi] as NSArray`：单测里后台修属性（fixAttributes）卡住、内存涨到 2 GB 以上，换成 NSValue 后正常，原因没查清。
            out.addAttribute(MarkdownRender.sourceLinesKey, value: NSValue(range: NSRange(location: lo, length: hi - lo)), range: NSRange(location: start, length: out.length - start))
        }

        mutating func block(_ intent: PresentationIntent?, _ text: AttributedSubstring) {
            let kinds = intent?.components ?? []
            if let t = kinds.first(where: { if case .table = $0.kind { return true }; return false }) {
                collectCell(kinds, table: t, text: text)
                return
            }
            flushTable()
            let blockStart = out.length
            defer { mark(from: blockStart, Self.lines(text)) }
            var depth = 0
            var quote = false
            for c in kinds {
                switch c.kind {
                case .orderedList, .unorderedList: depth += 1
                case .blockQuote: quote = true; depth += 1
                default: break
                }
            }
            let indent = CGFloat(depth) * 18
            let style = NSMutableParagraphStyle()
            style.paragraphSpacing = 8
            style.firstLineHeadIndent = indent
            style.headIndent = indent
            var font = MarkdownRender.body
            var color = quote ? NSColor.secondaryLabelColor : NSColor.labelColor
            var prefix = ""
            var code = false
            let leaf = kinds.first?.kind
            switch leaf {
            case .header(let level)?:
                font = MarkdownRender.headingFont(level)
                style.paragraphSpacingBefore = level <= 2 ? 10 : 6
            case .codeBlock?:
                code = true
                style.firstLineHeadIndent = indent + 8
                style.headIndent = indent + 8
            case .thematicBreak?:
                // 短一些，窄栏里也不折成两行。
                append(String(repeating: "─", count: 12) + "\n", [.font: MarkdownRender.body, .foregroundColor: NSColor.tertiaryLabelColor, .paragraphStyle: style])
                return
            default:
                break
            }
            // 列表项的第一段前面放序号或圆点。
            if let i = kinds.firstIndex(where: { if case .listItem = $0.kind { return true }; return false }),
               seenItems.insert(kinds[i].identity).inserted {
                let ordered = kinds[(i + 1)...].first.map { if case .orderedList = $0.kind { return true }; return false } ?? false
                if case .listItem(let ordinal) = kinds[i].kind {
                    prefix = ordered ? "\(ordinal). " : "• "
                }
                style.paragraphSpacing = 3
                // 折行后对齐到序号之后。
                style.headIndent = indent
                style.firstLineHeadIndent = max(0, indent - 14)
            } else if kinds.contains(where: { if case .listItem = $0.kind { return true }; return false }) {
                style.paragraphSpacing = 3
            }
            // 引用：左边一道竖线，免得和上面的列表连成一片。
            let quoteBar = quote && prefix.isEmpty && !code
            if quoteBar { style.firstLineHeadIndent = max(0, indent - 14) }
            if code { font = MarkdownRender.mono; color = .labelColor }
            var base: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: color, .paragraphStyle: style]
            if code { base[.backgroundColor] = NSColor.quaternarySystemFill }
            if !prefix.isEmpty { append(prefix, base) }
            if quoteBar {
                var bar = base
                bar[.foregroundColor] = NSColor.tertiaryLabelColor
                append("▎ ", bar)
            }
            var segment = text
            if code, segment.characters.last == "\n" {
                segment = segment[segment.startIndex..<segment.characters.index(before: segment.endIndex)]
            }
            let start = out.length
            inlines(segment, base: base, code: code)
            if code { tighten(from: start, style) }
            out.append(NSAttributedString(string: "\n"))
        }

        private mutating func inlines(_ segment: AttributedSubstring, base: [NSAttributedString.Key: Any], code: Bool) {
            for run in segment.runs {
                inline(String(segment[run.range].characters), intent: run.inlinePresentationIntent, link: run.link, image: run.imageURL,
                       base: base, code: code)
            }
        }

        private mutating func inline(_ s: String, intent: InlinePresentationIntent?, link url: URL?, image: URL?,
                                     base: [NSAttributedString.Key: Any], code: Bool) {
            var a = base
            var font = base[.font] as! NSFont
            let i = intent ?? []
            if i.contains(.code), !code {
                font = MarkdownRender.mono
                a[.backgroundColor] = NSColor.tertiarySystemFill
            }
            if i.contains(.stronglyEmphasized) { font = NSFontManager.shared.convert(font, toHaveTrait: .boldFontMask) }
            if i.contains(.emphasized) { font = NSFontManager.shared.convert(font, toHaveTrait: .italicFontMask) }
            if i.contains(.strikethrough) { a[.strikethroughStyle] = NSUnderlineStyle.single.rawValue }
            a[.font] = font
            if let image {
                return self.image(alt: s, url: image.absoluteURL, base: a)
            }
            if let url { link(&a, url.absoluteURL) }
            append(s, a)
        }

        /// 图片：能显示的按栏宽缩小放进正文；不能显示的写替代文字和原因，带链接样式。
        private func image(alt raw: String, url: URL, base: [NSAttributedString.Key: Any]) {
            // 没有替代文字时，解析结果是空白或占位符 U+FFFC。
            let trimmed = raw.trimmingCharacters(in: CharacterSet.whitespaces.union(["\u{FFFC}"]))
            let alt = trimmed.isEmpty ? String(localized: "图片") : trimmed
            switch MarkdownRender.loadImage(url) {
            case .image(let img):
                let attachment = NSTextAttachment()
                attachment.attachmentCell = ScaledImageCell(imageCell: img)
                let s = NSMutableAttributedString(attachment: attachment)
                var a = base
                a[.toolTip] = alt
                s.addAttributes(a, range: NSRange(location: 0, length: s.length))
                out.append(s)
            case .unavailable(let reason):
                var a = base
                a[.foregroundColor] = NSColor.secondaryLabelColor
                link(&a, url)
                append(String(localized: "「\(alt)」（\(reason)）"), a)
            }
        }

        private func link(_ a: inout [NSAttributedString.Key: Any], _ url: URL) {
            a[.link] = url
            if !MarkdownRender.opensOnClick(url) {
                a[.toolTip] = String(localized: "暂不支持打开这类链接：\(url.isFileURL ? url.path : url.absoluteString)")
            }
        }

        private mutating func collectCell(_ kinds: [PresentationIntent.IntentType], table t: PresentationIntent.IntentType, text: AttributedSubstring) {
            if let cur = table, cur.id != t.identity { flushTable() }
            if table == nil {
                var columns: [PresentationIntent.TableColumn] = []
                if case .table(let c) = t.kind { columns = c }
                table = (t.identity, columns, [], [:], [])
            }
            var row = "", column = 0, header = false
            for c in kinds {
                switch c.kind {
                case .tableCell(let col): column = col
                case .tableRow(let r): row = "r\(r)"
                case .tableHeaderRow: header = true
                default: break
                }
            }
            if header { row = "h" }
            if let (lo, hi) = Self.lines(text) {
                tableLines = tableLines.map { (min($0.0, lo), max($0.1, hi)) } ?? (lo, hi)
            }
            if table?.cells[row] == nil { table?.rows.append(row) }
            if header { table?.header.insert(row) }
            table?.cells[row, default: [:]][column] = text
        }

        /// 表格画成网格（`NSTextTable`）：铺满栏宽，各列按内容分宽，单元格里折行；表头加底色、加粗；对齐照 Markdown。
        mutating func flushTable() {
            guard let t = table else { return }
            table = nil
            let tableStart = out.length
            let lines = tableLines
            tableLines = nil
            defer { mark(from: tableStart, lines) }
            let maxCol = t.cells.values.flatMap(\.keys).max() ?? 0
            let n = max(t.columns.count, maxCol + 1)
            let grid = NSTextTable()
            grid.numberOfColumns = n
            grid.layoutAlgorithm = .automaticLayoutAlgorithm
            grid.collapsesBorders = true
            grid.hidesEmptyCells = false
            grid.setContentWidth(100, type: .percentageValueType)
            for (r, key) in t.rows.enumerated() {
                let header = t.header.contains(key)
                for c in 0..<n {
                    let block = NSTextTableBlock(table: grid, startingRow: r, rowSpan: 1, startingColumn: c, columnSpan: 1)
                    block.setWidth(0.5, type: .absoluteValueType, for: .border)
                    block.setBorderColor(.separatorColor)
                    block.setWidth(6, type: .absoluteValueType, for: .padding)
                    if header { block.backgroundColor = .quaternarySystemFill }
                    let style = NSMutableParagraphStyle()
                    style.textBlocks = [block]
                    if c < t.columns.count {
                        switch t.columns[c].alignment {
                        case .center: style.alignment = .center
                        case .right: style.alignment = .right
                        default: style.alignment = .left
                        }
                    }
                    let font = header ? NSFont.boldSystemFont(ofSize: MarkdownRender.bodySize) : MarkdownRender.body
                    let base: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: NSColor.labelColor, .paragraphStyle: style]
                    if let cell = t.cells[key]?[c] { inlines(cell, base: base, code: false) }
                    append("\n", base)
                }
            }
            // 表格后留一点空，再接下一段。
            append("\n", [.font: NSFont.systemFont(ofSize: 4)])
        }

        /// 代码块：每行是一个段落（换行保持 `\n`，拷出去照常），行与行之间不留段间距，只在最后一行后留。
        private func tighten(from start: Int, _ style: NSParagraphStyle) {
            let range = NSRange(location: start, length: out.length - start)
            guard range.length > 0 else { return }
            let inner = style.mutableCopy() as! NSMutableParagraphStyle
            inner.paragraphSpacing = 0
            out.addAttribute(.paragraphStyle, value: inner, range: range)
            let lastBreak = (out.string as NSString).range(of: "\n", options: .backwards, range: range)
            let lastStart = lastBreak.location == NSNotFound ? start : NSMaxRange(lastBreak)
            out.addAttribute(.paragraphStyle, value: style, range: NSRange(location: lastStart, length: out.length - lastStart))
        }

        private func append(_ s: String, _ a: [NSAttributedString.Key: Any]) {
            out.append(NSAttributedString(string: s, attributes: a))
        }

        func finish() -> NSAttributedString {
            while out.length > 0, out.string.hasSuffix("\n") { out.deleteCharacters(in: NSRange(location: out.length - 1, length: 1)) }
            return out
        }
    }
}

/// 正文里的图片：原尺寸显示，栏不够宽时等比缩小到栏宽（栏宽变了随排版重算），不放大。
final class ScaledImageCell: NSTextAttachmentCell {
    override func cellFrame(for textContainer: NSTextContainer, proposedLineFragment lineFrag: NSRect, glyphPosition position: NSPoint,
                            characterIndex charIndex: Int) -> NSRect {
        guard let img = image, img.size.width > 0 else { return .zero }
        let maxWidth = max(16, lineFrag.width - textContainer.lineFragmentPadding * 2 - 2)
        let k = min(1, maxWidth / img.size.width)
        return NSRect(x: 0, y: 0, width: floor(img.size.width * k), height: floor(img.size.height * k))
    }

    override func cellSize() -> NSSize { image?.size ?? .zero }

    override func draw(withFrame cellFrame: NSRect, in controlView: NSView?) {
        image?.draw(in: cellFrame, from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: true, hints: nil)
    }
}
