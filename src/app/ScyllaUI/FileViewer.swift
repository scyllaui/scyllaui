// 右侧栏的只读查看器（docs/design.md 5.12 节「查看器」）：文本用 TextKit 1 的 NSTextView 加行号，图片与 PDF 用 Quick Look，
// 其他二进制只写说明。标题区一行：文件名与元信息，右边折行、插入到终端、隐藏文件树与「…」菜单（拷贝相对路径、打开方式、
// 在访达中显示）；放大与关闭在文件栏的栏头（实现方案第 4 版 F1）。Markdown 默认渲染，可切回源码（F2，`MarkdownRender`）。
// 内容由文件栏读好交给它。

import AppKit
import QuickLookUI
import UniformTypeIdentifiers

final class FileViewer: NSView, NSTextViewDelegate {
    var onInsert: ((String) -> Void)?
    /// 文件树隐藏 / 显示（按钮在查看器的栏头，文件树隐藏后仍点得到）。
    var onToggleTree: (() -> Void)?
    /// `Esc`：只在查看器自己有键盘焦点时由这里收到；返回 true 表示已处理（缩回放大）。
    var onEscape: (() -> Bool)?

    private(set) var path: String?
    private var root = ""
    var treeHidden = false {
        didSet { updateButtons() }
    }

    private let name = NSTextField(labelWithString: "")
    private let meta = NSTextField(labelWithString: "")
    private let tools = NSStackView()
    private lazy var wrapButton = RightPanelHeader.wrapButton(target: self, action: #selector(toggleWrap(_:)))
    private lazy var renderButton = RightPanelHeader.iconButton(symbol: "chevron.left.forwardslash.chevron.right", name: String(localized: "显示源码"), target: self, action: #selector(toggleRender(_:)))
    private lazy var insertButton = RightPanelHeader.iconButton(symbol: "arrow.right.to.line", name: String(localized: "插入到终端"), target: self, action: #selector(insert(_:)))
    private lazy var treeButton = RightPanelHeader.iconButton(symbol: "sidebar.right", name: String(localized: "隐藏文件树"), target: self, action: #selector(toggleTree(_:)))
    private lazy var moreButton = RightPanelHeader.iconButton(symbol: "ellipsis", name: String(localized: "更多"), target: self, action: #selector(showMore(_:)))
    private let headerLine = NSBox()
    /// 「…」菜单里的拷贝相对路径、打开方式、在访达中显示（与 Diff 查看器共用）。
    private let actions = FileActions()
    private let body = NSView()
    private var bodyView: NSView?
    private var preview: QLPreviewView?

    // 文本
    private let scroll = NSScrollView()
    private let textView: ViewerTextView
    private let ruler: LineNumberRuler
    private var wrap = false
    /// 用户点折行按钮选的（只在内存里，换文件沿用，重启回到不折行；实现方案第 6 版 R1）。
    private var wrapChoice = false
    /// 内容搜索点的那一行：文本到了再滚过去并短暂高亮。
    private var pendingLine: Int?
    /// 源码视图里此刻是哪个文件的文字：换文件后、新文字到之前，跳行要等新文字（否则会滚在旧文件上并丢掉这一跳）。
    private var textPath: String?
    /// 源码视图里此刻的文字：与要显示的相同就不重放（相同的 String 比较先比存储，不逐字比）。
    private var sourceText: String?
    /// 这次读到的文字里有无法按 UTF-8 解码的字节（应答的 `lossy`）：正文里的 U+FFFD 着色（`UndecodableMarker`）。
    private var lossy = false
    private lazy var sourceMarker = UndecodableMarker(textView)
    private lazy var renderMarker = UndecodableMarker(renderView)
    /// 不折行时源码视图的宽度（按最长一行估算，见 `lineWidth`）。
    private var sourceWidth: CGFloat = 0
    /// 滚动条停止滚动后续显约 3 秒（源码、渲染各一个，见 `ScrollerLinger`）。
    private lazy var sourceLinger = ScrollerLinger(scroll)
    private lazy var renderLinger = ScrollerLinger(renderScroll)

    // Markdown 渲染（F2）
    private let renderBox = NSView()
    private let renderScroll = NSScrollView()
    /// 显式用 TextKit 1：表格（`NSTextTable`）与按栏宽缩放的图片都要它。
    private let renderView: ViewerTextView = {
        let storage = NSTextStorage()
        let layout = NSLayoutManager()
        storage.addLayoutManager(layout)
        let container = NSTextContainer(size: NSSize(width: 400, height: CGFloat.greatestFiniteMagnitude))
        container.widthTracksTextView = true
        layout.addTextContainer(container)
        return ViewerTextView(frame: .zero, textContainer: container)
    }()
    /// 每个文件上次选的是渲染（true）还是源码，只在内存里；没选过的 Markdown 默认渲染。
    private var renderChoice: [String: Bool] = [:]
    /// 从内容搜索跳行而临时显示源码的文件：不改 `renderChoice`，用到换文件、从文件树重新打开或自己切换为止。
    private var jumpSourcePath: String?
    /// 上一次收到的文本与它能不能渲染（切换时不用重读）。
    private var lastText: String?
    private var canRender = false
    /// 渲染在后台队列做（大文件要几百毫秒到几秒，不能卡住终端打字），结果回主线程。测试换成同步或手动放行。
    static var renderWork: (_ work: @escaping () -> NSAttributedString, _ done: @escaping (NSAttributedString) -> Void) -> Void = { work, done in
        renderQueue.async {
            let r = work()
            DispatchQueue.main.async { done(r) }
        }
    }
    private static let renderQueue = DispatchQueue(label: "dev.scyllaui.markdown-render", qos: .userInitiated)
    /// 渲染的代数：新的一次渲染、切到源码、换内容都加一，回来的结果代数不符就丢掉。
    private var renderGeneration = 0
    private var renderPending = false
    /// 渲染视图里此刻的是哪个文件的哪份文字（结果换上时记下）：重读后文字没变、从源码切回来都不重渲染。
    private var renderedSource: (path: String, text: String)?
    /// 正在渲染的是哪份：同一份不重复发起。
    private var pendingSource: (path: String, text: String)?
    /// 从源码切到渲染、渲染还没好时，好了要落到的源码位置（顶端字符）。
    private var renderTargetSourceChar: Int?
    /// 渲染视图里各块对应的源码行（按渲染后的位置排），换上新结果时作废，切换时才算。
    private var renderBlocks: [(range: NSRange, lines: (start: Int, end: Int))]?
    /// 新打开的文件渲染超过这么久还没好，先显示源码；快的直接换成渲染，不闪。679 KB 后台渲染 0.43—0.47 秒，0.6 秒内能好
    /// （2026-11-25 前为 0.15 秒：296 KB 的文件先闪一下源码再变渲染）。
    private static let sourceFallbackDelay: TimeInterval = 0.6

    override init(frame: NSRect) {
        let storage = NSTextStorage()
        let layout = NSLayoutManager()
        storage.addLayoutManager(layout)
        let container = NSTextContainer(size: NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude))
        layout.addTextContainer(container)
        textView = ViewerTextView(frame: .zero, textContainer: container)
        ruler = LineNumberRuler(scrollView: scroll, orientation: .verticalRuler)
        super.init(frame: frame)
        name.font = ScyllaUIStyle.mono(13, semibold: true)
        name.lineBreakMode = .byTruncatingMiddle
        name.setContentCompressionResistancePriority(.defaultLow + 1, for: .horizontal)
        meta.font = ScyllaUIStyle.mono(11)
        meta.textColor = .secondaryLabelColor
        meta.lineBreakMode = .byTruncatingTail
        meta.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        for b in [wrapButton, renderButton, insertButton, treeButton, moreButton] { tools.addArrangedSubview(b) }
        tools.spacing = 2
        actions.host = self
        headerLine.boxType = .separator
        // 只排看得见的部分：整篇排版在主线程上，679 KB 要 0.6—1.2 秒，期间终端不能响应（2026-11-25 测试版实测）。
        // 行号按字符位置算（`LineNumberRuler`），不折行时的宽度按最长一行估算（`applyWrap`），都不依赖整篇排版。
        textView.layoutManager?.allowsNonContiguousLayout = true
        textView.isEditable = false
        textView.isSelectable = true
        textView.isRichText = false
        // 只读查看：关掉拼写、数据检测等自动文本检查（选中文字时它们会去读通讯录等系统数据，测试进程里实测会连通讯录服务）。
        textView.isContinuousSpellCheckingEnabled = false
        textView.isGrammarCheckingEnabled = false
        textView.isAutomaticDataDetectionEnabled = false
        textView.isAutomaticLinkDetectionEnabled = false
        textView.isAutomaticTextCompletionEnabled = false
        textView.enabledTextCheckingTypes = 0
        textView.usesFindBar = true
        textView.isIncrementalSearchingEnabled = true
        textView.drawsBackground = false
        textView.font = ScyllaUIStyle.mono(12)
        textView.textColor = .labelColor
        textView.textContainerInset = NSSize(width: 4, height: 6)
        textView.isVerticallyResizable = true
        textView.minSize = .zero
        textView.maxSize = NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
        textView.autoresizingMask = [.width]
        textView.onEscape = { [weak self] in self?.onEscape?() ?? false }
        scroll.documentView = textView
        scroll.drawsBackground = false
        scroll.hasVerticalScroller = true
        scroll.hasHorizontalScroller = true
        scroll.autohidesScrollers = true
        scroll.verticalRulerView = ruler
        scroll.hasVerticalRuler = true
        scroll.rulersVisible = true
        ruler.textView = textView
        // 滚动时重画行号；滚到底时补到真正的末尾（见 `followEndIfAtBottom`）。不经队列、当场处理：滚动后一排版，估算的高度就变了，
        // 晚一步再看已经不在「底部」。滚动只发生在主线程上。
        scroll.contentView.postsBoundsChangedNotifications = true
        NotificationCenter.default.addObserver(forName: NSView.boundsDidChangeNotification, object: scroll.contentView, queue: nil) { [weak self] _ in
            self?.ruler.needsDisplay = true
            guard let self else { return }
            self.follower.scrolled(self.textView, self.scroll)
            self.sourceMarker.markVisible()
        }
        textView.postsFrameChangedNotifications = true
        NotificationCenter.default.addObserver(forName: NSView.frameDidChangeNotification, object: textView, queue: nil) { [weak self] _ in
            guard let self else { return }
            self.follower.noteFrame(self.textView, self.scroll)
        }
        setUpRenderView()
        _ = (sourceLinger, renderLinger)
        for v in [name, meta, tools, headerLine, body] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            addSubview(v)
        }
        NSLayoutConstraint.activate([
            name.topAnchor.constraint(equalTo: topAnchor, constant: 10),
            name.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 12),
            meta.firstBaselineAnchor.constraint(equalTo: name.firstBaselineAnchor),
            meta.leadingAnchor.constraint(equalTo: name.trailingAnchor, constant: 8),
            tools.centerYAnchor.constraint(equalTo: name.centerYAnchor),
            tools.leadingAnchor.constraint(greaterThanOrEqualTo: meta.trailingAnchor, constant: 8),
            tools.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
            headerLine.topAnchor.constraint(equalTo: name.bottomAnchor, constant: 10),
            // 分隔线定高：NSBox 的高度没有必需约束时，真实窗口里多出的高度可能分给它而不是正文（2026-11-23 实测正文被挤到只剩 6 pt）。
            headerLine.heightAnchor.constraint(equalToConstant: 1),
            headerLine.leadingAnchor.constraint(equalTo: leadingAnchor),
            headerLine.trailingAnchor.constraint(equalTo: trailingAnchor),
            body.topAnchor.constraint(equalTo: headerLine.bottomAnchor),
            body.leadingAnchor.constraint(equalTo: leadingAnchor),
            body.trailingAnchor.constraint(equalTo: trailingAnchor),
            body.bottomAnchor.constraint(equalTo: bottomAnchor),
        ])
        updateButtons()
    }

    required init?(coder: NSCoder) { fatalError() }

    override var acceptsFirstResponder: Bool { true }

    /// 查看器自己（文本或其他内容）是否持有键盘焦点。
    func holdsFocus(_ responder: NSResponder?) -> Bool {
        guard let v = responder as? NSView else { return false }
        return v === self || v.isDescendant(of: self)
    }

    /// 键盘交给查看器：有文本时给文本（可选中、可查找）。
    func takeFocus() {
        window?.makeFirstResponder(bodyView === scroll ? textView : bodyView === renderBox ? renderView : self)
    }

    /// `Cmd+F`：查看器里的查找条（只在显示文本时有）。
    func showFindBar() {
        let tv: NSTextView
        if bodyView === scroll { tv = textView } else if bodyView === renderBox { tv = renderView } else { return }
        window?.makeFirstResponder(tv)
        let item = NSMenuItem()
        item.tag = NSTextFinder.Action.showFindInterface.rawValue
        tv.performTextFinderAction(item)
    }

    override func cancelOperation(_ sender: Any?) {
        if onEscape?() != true { super.cancelOperation(sender) }
    }

    /// `Esc`（键码 53）：不论哪个子视图收到，先交给 `onEscape`。
    override func keyDown(with event: NSEvent) {
        if event.keyCode == 53, onEscape?() == true { return }
        super.keyDown(with: event)
    }

    // MARK: 内容

    /// 图片、PDF 等 Quick Look 能显示的，不经后台读取。
    static func usesQuickLook(_ path: String) -> Bool {
        guard let t = UTType(filenameExtension: (path as NSString).pathExtension) else { return false }
        return t.conforms(to: .image) || t.conforms(to: .pdf)
    }

    /// PDF 预览上限（F4，2026-11-23 阿远定）；图片的上限与 Markdown 里的图片共用 `MarkdownRender`。
    static let pdfMaxBytes: Int64 = 50_000_000

    /// 超过预览上限时不预览的原因，nil 为可以预览（docs/design.md 5.12 节「预览上限」）。
    /// `pixels`：图片文件头里的尺寸，读不到为 nil（交给 Quick Look 自己处理）。
    static func previewRefusal(path: String, size: Int64, pixels: (width: Int, height: Int)?) -> String? {
        let isPDF = UTType(filenameExtension: (path as NSString).pathExtension)?.conforms(to: .pdf) == true
        if isPDF {
            return size > pdfMaxBytes ? String(localized: "PDF 超过 \(sizeText(pdfMaxBytes))，不预览") : nil
        }
        if size > MarkdownRender.imageMaxBytes {
            return String(localized: "图片超过 \(sizeText(MarkdownRender.imageMaxBytes))，不预览")
        }
        if let p = pixels, MarkdownRender.exceedsPixels(width: p.width, height: p.height) {
            return String(localized: "图片超过 5000 万像素，不预览")
        }
        return nil
    }

    /// 换成另一个文件（或同一个文件重读前）：先写路径，内容等 `show(reply:)`。
    func begin(path: String, root: String) {
        let changed = path != self.path
        self.path = path
        self.root = root
        actions.path = path
        actions.root = root
        name.stringValue = (path as NSString).lastPathComponent
        name.toolTip = path
        if changed {
            setMeta("")
            pendingLine = nil
            // 换了文件：两个视图留着的内容不再复用，再打开回来照旧重新显示、回到顶端。
            renderedSource = nil
            sourceText = nil
            jumpSourcePath = nil
            // 新内容到之前正文留空：标题已换成新文件，正文不能还是上一个文件的。
            if Self.usesQuickLook(path) { showPreview(path) } else { setBody(NSView()) }
        }
        updateButtons()
    }

    /// 后台 `fs.read` 的应答；nil 为读不到（断线等）。`keepScroll`：同一个文件被改了重读时保留滚动位置。
    func show(reply: [String: Any]?, keepScroll: Bool) {
        guard let path else { return }
        guard let r = reply else { return showNote(String(localized: "暂时读不到这个文件"), size: nil) }
        guard r["ok"] as? Bool == true else {
            let code = (r["error"] as? [String: Any])?["code"] as? String
            switch code {
            case "not_found": return showNote(String(localized: "文件已被删除"), size: nil)
            case "denied": return showNote(String(localized: "没有权限读取这个文件"), size: nil)
            case "is_dir": return showNote(String(localized: "这是文件夹，在文件树里展开查看"), size: nil)
            default: return showNote(String(localized: "暂时读不到这个文件"), size: nil)
            }
        }
        let size = (r["size"] as? NSNumber)?.int64Value ?? 0
        if r["binary"] as? Bool == true {
            return showNote(String(localized: "二进制文件，不显示内容"), size: size, openWith: true)
        }
        let text = r["text"] as? String ?? ""
        let truncated = r["truncated"] as? Bool == true
        let markdown = MarkdownRender.isMarkdown(path)
        lossy = r["lossy"] as? Bool == true
        let m = Self.meta(size: size, truncated: truncated, markdown: markdown, lossy: lossy)
        setMeta(m.text, tip: m.tip)
        lastText = text
        canRender = markdown && !truncated
        // 从内容搜索跳到某一行：渲染后对不上行号，这一次改看源码，不改记住的选择（2026-11-25 前记为源码，之后从文件树再点也是源码）。
        // 文件被改了重读（keepScroll）时仍用这份源码；从文件树重新打开（不跳行、不保留滚动）时回到记住的选择。
        if pendingLine != nil { jumpSourcePath = path } else if !keepScroll { jumpSourcePath = nil }
        if canRender && jumpSourcePath != path && renderChoice[path] ?? true {
            showRendered(text, path: path, keepScroll: keepScroll)
            return updateButtons()
        }
        cancelRender()
        let same = keepScroll && bodyView === scroll && textPath == path && sourceText == text
        if !same {
            let shown = keepScroll && bodyView === scroll
            // 新打开的文件按记住的选择，默认不折行；不按行长自动折行：超长单行折行打开比不折行更慢（R1 实测，
            // docs/design.md 5.12 节「折行」）。同一个文件重读时不变。
            if !shown { wrap = wrapChoice }
            // 只排可见部分时高度是估算的，按像素保留位置会对到别处，按顶端字符保留（同渲染视图）；横向位置照旧。
            let anchor = shown ? Self.topAnchor(textView, scroll) : nil
            let x = scroll.contentView.bounds.minX
            // 换上文字、重排与定位都是程序自己做的：其间可见区的挪动不当作用户往下滚到底（见 `EndFollower.positioning`；
            // 2026-11-25 核对 Diff 时发现，重读后等下一轮主循环会被补到末尾）。
            follower.positioning(textView, scroll) {
                setBody(scroll)
                setSourceText(text, path: path)
                applyWrap()
                if let anchor {
                    Self.scroll(textView, scroll, to: anchor, x: x)
                } else {
                    scrollSourceToTop()
                }
            }
            // 新打开的文件：滚动条显示约 3 秒，让人知道能滚、有多长多宽（刷新保位时不显示）。
            if anchor == nil { sourceLinger.show() }
            sourceMarker.markVisible()
        }
        applyPendingLine()
        updateButtons()
    }

    /// 滚到第 `line` 行（从 1 起）并短暂高亮：只记下，等随后重读到的文字显示了再滚（文件栏每次跳行都会重读）。
    /// 文字已显示时也不当场滚：接着的重读按新打开回到顶端，会把这一跳冲掉（2026-11-25 测试版：已打开的文件里点内容搜索结果回到开头）。
    func reveal(line: Int) {
        pendingLine = line
    }

    private func applyPendingLine() {
        guard let n = pendingLine, bodyView === scroll, textPath == path, !textView.string.isEmpty else { return }
        pendingLine = nil
        let starts = ruler.lineStarts
        guard n >= 1, n <= starts.count else { return }
        let length = (textView.string as NSString).length
        let end = n < starts.count ? starts[n] - 1 : length
        let range = NSRange(location: starts[n - 1], length: max(0, end - starts[n - 1]))
        textView.layoutManager?.ensureLayout(forCharacterRange: range)
        textView.scrollRangeToVisible(range)
        textView.setSelectedRange(range)
        textView.showFindIndicator(for: range)
        revealedLine = n
    }

    /// 测试用：上一次滚到的行。
    private(set) var revealedLine: Int?

    /// 标题行文件名后面的元信息（`text`）与悬停提示（`tip`：各段的完整写法，放不下被截断时也能看全）。
    /// 被截断的 Markdown 把「不渲染」放在最前面：放在后面时标题行窄就被截掉，看不出为什么没有渲染按钮；
    /// 无法解码一段在标题行里只写短的，说明放进提示（2026-11-26 第 7 版验收返工）。
    static func meta(size: Int64, truncated: Bool, markdown: Bool, lossy: Bool) -> (text: String, tip: String) {
        var text = [sizeText(size)]
        if truncated {
            text = [markdown ? String(localized: "超过 1 MB，不渲染 · 只显示前 1 MB · 共 \(sizeText(size))")
                             : String(localized: "只显示前 1 MB · 共 \(sizeText(size))")]
        }
        var tip = text
        if lossy {
            text.append(String(localized: "有无法解码的字符"))
            tip.append(String(localized: "有无法解码的字符：文件不是 UTF-8 编码，解不了的地方在正文里用颜色标出。"))
        }
        return (text.joined(separator: " · "), tip.joined(separator: " · "))
    }

    /// 元信息与它的悬停提示（不给提示时与元信息相同）。
    private func setMeta(_ s: String, tip: String? = nil) {
        meta.stringValue = s
        let t = tip ?? s
        meta.toolTip = t.isEmpty ? nil : t
    }

    /// `range` 里的 U+FFFD（后台按 UTF-8 解不了的字节换成的替换字符），相邻的合成一段。取到缓冲区里逐字找：
    /// 用 `rangeOfCharacter(from: CharacterSet)` 每次都要桥接字符集，几乎全是 � 的 1 MB 文件要 146 毫秒，这样 8 毫秒（第 7 版实测）。
    static func undecodableRuns(_ ns: NSString, in range: NSRange) -> [NSRange] {
        guard range.length > 0 else { return [] }
        var buf = [unichar](repeating: 0, count: range.length)
        ns.getCharacters(&buf, range: range)
        var runs: [NSRange] = []
        buf.withUnsafeBufferPointer { p in
            var i = 0
            while i < p.count {
                guard p[i] == 0xFFFD else { i += 1; continue }
                let start = i
                while i < p.count, p[i] == 0xFFFD { i += 1 }
                runs.append(NSRange(location: range.location + start, length: i - start))
            }
        }
        return runs
    }

    /// 路径相对项目根的写法；不在根下时为绝对路径。
    static func relative(_ path: String, root: String) -> String {
        let r = root.hasSuffix("/") ? root : root + "/"
        return path.hasPrefix(r) ? String(path.dropFirst(r.count)) : path
    }

    static func sizeText(_ bytes: Int64) -> String {
        ByteCountFormatter.string(fromByteCount: bytes, countStyle: .file)
    }

    private func showPreview(_ path: String) {
        let url = URL(fileURLWithPath: path)
        let size = ((try? FileManager.default.attributesOfItem(atPath: path))?[.size] as? NSNumber)?.int64Value
        if let size {
            let isPDF = UTType(filenameExtension: url.pathExtension)?.conforms(to: .pdf) == true
            let pixels = isPDF || size > MarkdownRender.imageMaxBytes ? nil : MarkdownRender.pixelSize(url)
            if let reason = Self.previewRefusal(path: path, size: size, pixels: pixels) {
                scyllauiLog.notice("viewer preview refused size=\(size)")
                return showNote(reason, size: size, openWith: true)
            }
        }
        let p = preview ?? QLPreviewView(frame: .zero, style: .normal)
        preview = p
        p?.previewItem = url as NSURL
        setMeta(size.map(Self.sizeText) ?? "")
        if let p { setBody(p) }
    }

    private func showNote(_ text: String, size: Int64?, openWith: Bool = false) {
        setMeta(size.map(Self.sizeText) ?? "")
        let v = NSView()
        let label = NSTextField(wrappingLabelWithString: text)
        label.font = ScyllaUIStyle.detailFont
        label.textColor = .secondaryLabelColor
        label.alignment = .center
        let stack = NSStackView(views: [label])
        stack.orientation = .vertical
        stack.spacing = 10
        if openWith {
            let b = NSButton(title: String(localized: "打开方式"), target: self, action: #selector(showOpenWith(_:)))
            b.bezelStyle = .rounded
            stack.addArrangedSubview(b)
        }
        stack.translatesAutoresizingMaskIntoConstraints = false
        v.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.centerXAnchor.constraint(equalTo: v.centerXAnchor),
            stack.centerYAnchor.constraint(equalTo: v.centerYAnchor),
            stack.leadingAnchor.constraint(greaterThanOrEqualTo: v.leadingAnchor, constant: 24),
            stack.trailingAnchor.constraint(lessThanOrEqualTo: v.trailingAnchor, constant: -24),
        ])
        setBody(v)
        updateButtons()
    }

    /// 读屏与测试用：说明文字（没有时为 nil）。
    var noteText: String? {
        guard let v = bodyView, v !== scroll, v !== preview else { return nil }
        func find(_ v: NSView) -> String? { (v as? NSTextField)?.stringValue ?? v.subviews.lazy.compactMap(find).first }
        return find(v)
    }

    var metaText: String { meta.stringValue }
    var metaToolTipForTesting: String? { meta.toolTip }
    /// 测试用：源码视图里某个字符的前景色。
    func sourceColorForTesting(at i: Int) -> NSColor? { textView.layoutManager?.temporaryAttribute(.foregroundColor, atCharacterIndex: i, effectiveRange: nil) as? NSColor }
    func renderColorForTesting(at i: Int) -> NSColor? { renderView.layoutManager?.temporaryAttribute(.foregroundColor, atCharacterIndex: i, effectiveRange: nil) as? NSColor }
    var shownText: String? { bodyView === scroll ? textView.string : nil }
    func selectSourceRangeForTesting(_ r: NSRange) { textView.setSelectedRange(r) }
    var sourceSelectionForTesting: NSRange { textView.selectedRange() }
    /// 渲染后的文字（没在渲染时为 nil）与顶上那句「暂不支持」是否显示。
    var renderedText: NSAttributedString? { bodyView === renderBox ? renderView.attributedString() : nil }
    var renderToggleShown: Bool { !renderButton.isHidden }
    /// 测试用：源码文字左边与行号栏右边在查看器里的横坐标（前者不应小于后者）。
    var sourceTextLeft: CGFloat { textView.convert(NSPoint.zero, to: self).x }
    var rulerRight: CGFloat { ruler.convert(NSPoint(x: ruler.bounds.maxX, y: 0), to: self).x }
    var isWrapping: Bool { wrap }
    /// 测试与读屏：标题行右边看得见的图标按钮（从左到右的提示文字）、折行按钮的颜色、点折行按钮。
    var toolNamesForTesting: [String] { tools.arrangedSubviews.filter { !$0.isHidden }.compactMap { ($0 as? NSButton)?.toolTip } }
    var wrapTintForTesting: NSColor? { wrapButton.contentTintColor }
    func clickWrapForTesting() { wrapButton.performClick(nil) }

    private func setBody(_ v: NSView) {
        guard v !== bodyView else { return }
        if let old = bodyView {
            if old === preview { preview?.close(); preview = nil }
            old.removeFromSuperview()
        }
        bodyView = v
        v.translatesAutoresizingMaskIntoConstraints = false
        body.addSubview(v)
        NSLayoutConstraint.activate([
            v.topAnchor.constraint(equalTo: body.topAnchor),
            v.bottomAnchor.constraint(equalTo: body.bottomAnchor),
            v.leadingAnchor.constraint(equalTo: body.leadingAnchor),
            v.trailingAnchor.constraint(equalTo: body.trailingAnchor),
        ])
        updateButtons()
    }

    /// 关掉查看器前调用：Quick Look 视图移除前要 `close()`。
    func clear() {
        if bodyView === preview { preview?.close(); preview = nil }
        bodyView?.removeFromSuperview()
        bodyView = nil
        textView.string = ""
        renderView.string = ""
        cancelRender()
        renderedSource = nil
        textPath = nil
        sourceText = nil
        lastText = nil
        canRender = false
        path = nil
        actions.path = nil
    }

    /// 滚到末尾的补救（只排可见部分时），源码、渲染与 Diff 正文共用（`EndFollower`）。
    private let follower = EndFollower()

    /// 源码滚回左上角。行号栏盖在可滚区域左边，左上角不是 (0, 0)，而是滚动视图允许的最小位置（让出行号栏）；
    /// 源码视图从渲染等别的内容换回来时先重新排版，否则行首会压在行号栏下面（2026-11-23 F2 实测）。
    private func scrollSourceToTop() {
        layoutSubtreeIfNeeded()
        scroll.tile()
        let clip = scroll.contentView
        let far = NSRect(origin: NSPoint(x: -1e6, y: -1e6), size: clip.bounds.size)
        clip.scroll(to: clip.constrainBoundsRect(far).origin)
        scroll.reflectScrolledClipView(clip)
    }

    /// 源码视图换上新文字：行号、不折行时的宽度一并算好（都不依赖排版）。
    private func setSourceText(_ text: String, path: String) {
        textView.string = text
        sourceMarker.enabled = lossy
        sourceMarker.reset()
        textPath = path
        sourceText = text
        ruler.textChanged()
        let font = textView.font ?? ScyllaUIStyle.mono(12)
        let narrow = ("M" as NSString).size(withAttributes: [.font: font]).width
        let wide = ("中" as NSString).size(withAttributes: [.font: font]).width
        let padding = (textView.textContainer?.lineFragmentPadding ?? 0) * 2 + textView.textContainerInset.width * 2
        sourceWidth = ceil(Self.lineWidth(text, narrow: narrow, wide: wide, tab: 28)) + padding // 28：系统默认制表位间距
    }

    /// 最长一行的估计宽度：全角字符（U+2E80 起的中日韩等、代理对）按 `wide`，制表符按一个制表位 `tab`，其余按 `narrow`。
    /// 宁宽勿窄：宽了右边多一点空白，窄了看不到行尾。
    static func lineWidth(_ text: String, narrow: CGFloat, wide: CGFloat, tab: CGFloat) -> CGFloat {
        var longest: CGFloat = 0, run: CGFloat = 0
        for u in text.utf16 {
            switch u {
            case 10:
                longest = max(longest, run)
                run = 0
            case 9: run += tab
            case 0xDC00...0xDFFF: break // 代理对的后半，前半已算一个全角
            case 0x2E80...: run += wide
            default: run += narrow
            }
        }
        return max(longest, run)
    }

    /// 源码视图只排看得见的部分（见 init）；不折行时宽度用估算的最长一行，否则横向滚不到没排到的长行的行尾。
    private func applyWrap() {
        guard let container = textView.textContainer else { return }
        if wrap {
            textView.isHorizontallyResizable = false
            container.widthTracksTextView = true
            // 行号栏盖在可滚区域左边，可滚区域左侧留出同宽的内边距；正文宽度要扣掉，否则每行右边被切掉一段（2026-11-25 测试版）。
            let insets = scroll.contentView.contentInsets
            let width = scroll.contentSize.width - insets.left - insets.right
            container.containerSize = NSSize(width: width, height: CGFloat.greatestFiniteMagnitude)
            textView.minSize = .zero
            textView.frame.size.width = width
        } else {
            container.widthTracksTextView = false
            container.containerSize = NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
            textView.minSize = NSSize(width: sourceWidth, height: 0)
            textView.isHorizontallyResizable = true
        }
        scroll.hasHorizontalScroller = !wrap
        textView.sizeToFit()
        defer { follower.remember(textView, scroll) }
        // 折行时没有横向滚动，横向回到最左（让出行号栏）；否则仍停在原来偏右的地方、行首被切掉（2026-11-25 测试版实测）。
        let clip = scroll.contentView
        let x = wrap ? -1e6 : clip.bounds.minX
        clip.scroll(to: clip.constrainBoundsRect(NSRect(origin: NSPoint(x: x, y: clip.bounds.minY), size: clip.bounds.size)).origin)
        scroll.reflectScrolledClipView(clip)
        ruler.needsDisplay = true
        updateButtons()
    }

    private func updateButtons() {
        // 折行只对源码有效：渲染视图、图片、PDF、说明文字时隐藏（在最左边，出现或消失时别的按钮不挪位置）。
        wrapButton.isHidden = bodyView !== scroll
        RightPanelHeader.styleWrap(wrapButton, on: wrap)
        renderButton.isHidden = !(canRender && (bodyView === scroll || bodyView === renderBox))
        let rendered = bodyView === renderBox
        renderButton.image = NSImage(systemSymbolName: rendered ? "chevron.left.forwardslash.chevron.right" : "eye", accessibilityDescription: nil)
        renderButton.toolTip = rendered ? String(localized: "显示源码") : String(localized: "显示渲染")
        renderButton.setAccessibilityLabel(renderButton.toolTip)
        treeButton.toolTip = treeHidden ? String(localized: "显示文件树") : String(localized: "隐藏文件树")
        treeButton.setAccessibilityLabel(treeButton.toolTip)
        treeButton.contentTintColor = treeHidden ? .secondaryLabelColor : .controlAccentColor
    }

    // MARK: Markdown 渲染

    private func setUpRenderView() {
        // 大文件不整篇排版：只排看得见的部分（2026-11-24 复查：1 MB 整篇排版 1.2—3.3 秒）。
        renderView.layoutManager?.allowsNonContiguousLayout = true
        renderView.isEditable = false
        renderView.isSelectable = true
        renderView.isRichText = true
        renderView.isContinuousSpellCheckingEnabled = false
        renderView.isGrammarCheckingEnabled = false
        renderView.isAutomaticDataDetectionEnabled = false
        renderView.isAutomaticLinkDetectionEnabled = false
        renderView.isAutomaticTextCompletionEnabled = false
        renderView.enabledTextCheckingTypes = 0
        renderView.usesFindBar = true
        renderView.isIncrementalSearchingEnabled = true
        renderView.drawsBackground = false
        renderView.textContainerInset = NSSize(width: 12, height: 10)
        renderView.isVerticallyResizable = true
        renderView.isHorizontallyResizable = false
        renderView.textContainer?.widthTracksTextView = true
        renderView.minSize = .zero
        renderView.maxSize = NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
        renderView.autoresizingMask = [.width]
        renderView.delegate = self
        renderView.onEscape = { [weak self] in self?.onEscape?() ?? false }
        renderScroll.documentView = renderView
        // 渲染视图同样只排可见部分，滚到底时同样补到真正的末尾（见 `followEndIfAtBottom`）。
        renderScroll.contentView.postsBoundsChangedNotifications = true
        NotificationCenter.default.addObserver(forName: NSView.boundsDidChangeNotification, object: renderScroll.contentView, queue: nil) { [weak self] _ in
            guard let self else { return }
            self.follower.scrolled(self.renderView, self.renderScroll)
            self.renderMarker.markVisible()
        }
        renderView.postsFrameChangedNotifications = true
        NotificationCenter.default.addObserver(forName: NSView.frameDidChangeNotification, object: renderView, queue: nil) { [weak self] _ in
            guard let self else { return }
            self.follower.noteFrame(self.renderView, self.renderScroll)
        }
        renderScroll.drawsBackground = false
        renderScroll.hasVerticalScroller = true
        renderScroll.autohidesScrollers = true
        renderScroll.translatesAutoresizingMaskIntoConstraints = false
        renderBox.addSubview(renderScroll)
        NSLayoutConstraint.activate([
            renderScroll.topAnchor.constraint(equalTo: renderBox.topAnchor),
            renderScroll.leadingAnchor.constraint(equalTo: renderBox.leadingAnchor),
            renderScroll.trailingAnchor.constraint(equalTo: renderBox.trailingAnchor),
            renderScroll.bottomAnchor.constraint(equalTo: renderBox.bottomAnchor),
        ])
    }

    /// 渲染并显示。在后台队列解析（`renderWork`），回来时代数不符（期间换了文件、切了源码、又来了新内容）就丢掉。
    /// 文字与渲染视图里（或正在渲染）的相同就不重渲染：文件被别的程序写了一遍但内容没变时不再卡一次；从源码切回来
    /// 只换回渲染视图，滚动位置照旧（2026-11-25 前每次切换都重新解析，679 KB 要 0.47 秒）。
    private func showRendered(_ text: String, path: String, keepScroll: Bool) {
        if renderPending, let s = pendingSource, s.path == path, s.text == text { return }
        if let s = renderedSource, s.path == path, s.text == text {
            cancelRender()
            setBody(renderBox)
            return
        }
        renderGeneration += 1
        let gen = renderGeneration
        pendingSource = (path, text)
        renderPending = true
        let keep = keepScroll && bodyView === renderBox
        if !keep {
            DispatchQueue.main.asyncAfter(deadline: .now() + Self.sourceFallbackDelay) { [weak self] in
                guard let self, self.renderGeneration == gen, self.renderPending, self.path == path else { return }
                // 正显示着同一份源码（从源码切过来）：留着它和它的滚动位置，等渲染好再换。
                if self.bodyView === self.scroll, self.textPath == path, self.sourceText == text { return }
                self.showSourceBody(text, path: path)
                self.updateButtons()
            }
        }
        let base = URL(fileURLWithPath: (path as NSString).deletingLastPathComponent, isDirectory: true)
        let lossy = self.lossy
        let started = CACurrentMediaTime()
        var parsed: CFTimeInterval = 0
        Self.renderWork({
            // 连同 NSTextStorage 一起在后台建好、修好属性：主线程只换存储（setAttributedString 在主线程上，1 MB 要几百毫秒）。
            let t = CACurrentMediaTime()
            let storage = NSTextStorage(attributedString: MarkdownRender.render(text, baseURL: base))
            storage.fixAttributes(in: NSRange(location: 0, length: storage.length))
            parsed = CACurrentMediaTime() - t
            return storage
        }) { [weak self] rendered in
            guard let self, self.renderGeneration == gen else { return }
            self.renderPending = false
            guard self.path == path else { return }
            let arrived = CACurrentMediaTime()
            defer {
                // parse：后台解析与建存储；wait：从发起到主线程开始换内容，扣掉 parse 剩下的排队（主线程被别的事占着时变大）；
                // main：主线程上换内容、排版（期间终端不能响应）。
                let now = CACurrentMediaTime()
                let ms = { (t: CFTimeInterval) in Int(t * 1000) }
                scyllauiLog.notice("viewer render bytes=\(text.utf8.count) parse_ms=\(ms(parsed)) wait_ms=\(ms(arrived - started - parsed)) main_ms=\(ms(now - arrived))")
            }
            let wasShown = self.bodyView === self.renderBox
            let anchor = keep && wasShown ? Self.topAnchor(self.renderView, self.renderScroll) : nil
            self.setBody(self.renderBox)
            if let storage = rendered as? NSTextStorage, let layout = self.renderView.layoutManager {
                layout.replaceTextStorage(storage)
            } else {
                self.renderView.textStorage?.setAttributedString(rendered)
            }
            self.renderedSource = (path, text)
            self.renderBlocks = nil
            self.renderMarker.enabled = lossy
            self.renderMarker.reset()
            defer { self.renderMarker.markVisible() }
            defer { self.follower.remember(self.renderView, self.renderScroll) }
            self.renderBox.layoutSubtreeIfNeeded()
            // 换了内容：先按新内容记下位置与高度，接着的落位不拿上一份内容的高度判断「到底」。
            self.follower.remember(self.renderView, self.renderScroll)
            let target = self.renderTargetSourceChar.flatMap { self.renderChar(forSourceChar: $0) }
            self.renderTargetSourceChar = nil
            if let anchor {
                self.follower.positioning(self.renderView, self.renderScroll) { Self.scroll(self.renderView, self.renderScroll, to: anchor) }
            } else if let target {
                self.follower.positioning(self.renderView, self.renderScroll) { Self.scroll(self.renderView, self.renderScroll, to: (target, 0)) }
            } else {
                // 只排可见部分时，先排好开头一屏再回到顶端；scrollToBeginningOfDocument 会按估算的高度停在偏下的位置。
                self.renderView.layoutManager?.ensureLayout(forCharacterRange: NSRange(location: 0, length: min(rendered.length, 4000)))
                self.renderScroll.contentView.scroll(to: .zero)
                self.renderScroll.reflectScrolledClipView(self.renderScroll.contentView)
                self.renderLinger.show()
            }
            self.updateButtons()
        }
    }

    /// 文本视图顶端是第几个字符、在那一行里往下偏了多少。只排可见部分时高度是估算的，按像素保留位置会在换内容后
    /// 对到别处（2026-11-24 测试版：297 KB 的文件改一行后跳到了文末、先是一片空白），所以按字符保留。渲染、源码共用。
    static func topAnchor(_ tv: NSTextView, _ sv: NSScrollView) -> (char: Int, offset: CGFloat)? {
        guard let lm = tv.layoutManager, let tc = tv.textContainer, lm.numberOfGlyphs > 0 else { return nil }
        let y = sv.contentView.bounds.minY - tv.textContainerOrigin.y
        // 用画的时候同一个办法（按矩形取字形）找顶端那一行。只排可见部分时 glyphIndex(for:) 按点找会用另一套估算，
        // 与画出来的差出几百行（单测里 200 多行）。
        let glyphs = lm.glyphRange(forBoundingRect: NSRect(x: 0, y: max(0, y), width: max(1, tv.bounds.width), height: 1), in: tc)
        guard glyphs.length > 0 else { return nil }
        let glyph = glyphs.location
        let line = lm.lineFragmentRect(forGlyphAt: glyph, effectiveRange: nil)
        return (lm.characterIndexForGlyph(at: glyph), y - line.minY)
    }

    /// 只排顶端字符附近，再让它回到顶端；`x` 是横向位置。
    static func scroll(_ tv: NSTextView, _ sv: NSScrollView, to anchor: (char: Int, offset: CGFloat), x: CGFloat = 0) {
        guard let lm = tv.layoutManager, let storage = tv.textStorage, storage.length > 0 else { return }
        let c = min(anchor.char, storage.length - 1)
        lm.ensureLayout(forCharacterRange: NSRange(location: c, length: min(4000, storage.length - c)))
        // 文本视图的高度跟着估算的排版走，先更新，否则目标位置可能超出旧高度被截住。
        tv.sizeToFit()
        let line = lm.lineFragmentRect(forGlyphAt: lm.glyphIndexForCharacter(at: c), effectiveRange: nil)
        let clip = sv.contentView
        let y = line.minY + tv.textContainerOrigin.y + anchor.offset
        clip.scroll(to: clip.constrainBoundsRect(NSRect(origin: NSPoint(x: x, y: y), size: clip.bounds.size)).origin)
        sv.reflectScrolledClipView(clip)
    }

    /// 测试用：渲染视图顶端是第几个字符；滚到第几个字符。源码视图同。
    var renderTopCharForTesting: Int? { Self.topAnchor(renderView, renderScroll)?.char }
    func scrollRenderForTesting(toChar c: Int) { Self.scroll(renderView, renderScroll, to: (c, 0)) }
    var sourceTopCharForTesting: Int? { Self.topAnchor(textView, scroll)?.char }
    func scrollSourceForTesting(toChar c: Int) { Self.scroll(textView, scroll, to: (c, 0)) }
    /// 测试用：源码视图里看得见的字符范围（与画的时候同一个取法）。
    var sourceVisibleRangeForTesting: NSRange {
        guard let lm = textView.layoutManager, let tc = textView.textContainer else { return NSRange() }
        return lm.characterRange(forGlyphRange: lm.glyphRange(forBoundingRect: textView.visibleRect, in: tc), actualGlyphRange: nil)
    }
    /// 测试用：源码正文横向整个看得见（折行时每行行尾不被切掉）。
    var sourceFitsWidthForTesting: Bool { textView.visibleRect.maxX >= textView.bounds.maxX - 0.5 }
    /// 测试用：第 `c` 个字符横向在可见区里。
    func sourceEndVisibleForTesting(char c: Int) -> Bool {
        guard let lm = textView.layoutManager, let tc = textView.textContainer else { return false }
        let r = lm.boundingRect(forGlyphRange: lm.glyphRange(forCharacterRange: NSRange(location: c, length: 1), actualCharacterRange: nil), in: tc)
        let visible = textView.visibleRect
        return r.minX + textView.textContainerOrigin.x >= visible.minX && r.maxX + textView.textContainerOrigin.x <= visible.maxX
    }
    /// 测试用：像把滚动条设到最底那样，滚到文本视图此刻（估算）高度的底部；渲染视图此刻看得到的最后一个字符。
    func scrollToBottomForTesting(render: Bool) {
        let (tv, sv) = render ? (renderView as NSTextView, renderScroll) : (textView as NSTextView, scroll)
        let clip = sv.contentView
        clip.scroll(to: NSPoint(x: clip.bounds.minX, y: tv.frame.maxY - clip.bounds.height))
        sv.reflectScrolledClipView(clip)
    }
    /// 测试用：按真实顺序滚动 `dy`：先滚可见区（此刻滚动条的值还是旧的），再让滚动条跟上；可见区顶端的 y。
    func scrollByForTesting(render: Bool, dy: CGFloat) {
        let sv = render ? renderScroll : scroll
        let clip = sv.contentView
        clip.scroll(to: clip.constrainBoundsRect(NSRect(origin: NSPoint(x: clip.bounds.minX, y: clip.bounds.minY + dy), size: clip.bounds.size)).origin)
        sv.reflectScrolledClipView(clip)
    }
    func clipMinYForTesting(render: Bool) -> CGFloat { (render ? renderScroll : scroll).contentView.bounds.minY }
    var renderVisibleEndForTesting: Int {
        guard let lm = renderView.layoutManager, let tc = renderView.textContainer else { return 0 }
        return NSMaxRange(lm.characterRange(forGlyphRange: lm.glyphRange(forBoundingRect: renderView.visibleRect, in: tc), actualGlyphRange: nil))
    }
    func scrollSourceForTesting(toChar c: Int, x: CGFloat = 0) { Self.scroll(textView, scroll, to: (c, 0), x: x) }
    /// 测试用：在源码视图里按 `Cmd+↓`；可见区左边的 x。
    func sourceMoveToEndForTesting() { textView.moveToEndOfDocument(nil) }
    var sourceClipMinXForTesting: CGFloat { scroll.contentView.bounds.minX }
    var rulerClipsForTesting: Bool { ruler.clipsToBounds }
    /// 测试用：第 `c` 个字符所在的行离可见区顶端多远（pt）。
    func renderLineOffsetForTesting(char c: Int) -> CGFloat? { Self.lineOffset(renderView, renderScroll, char: c) }
    func sourceLineOffsetForTesting(char c: Int) -> CGFloat? { Self.lineOffset(textView, scroll, char: c) }
    private static func lineOffset(_ tv: NSTextView, _ sv: NSScrollView, char c: Int) -> CGFloat? {
        guard let lm = tv.layoutManager, let storage = tv.textStorage, c < storage.length else { return nil }
        let line = lm.lineFragmentRect(forGlyphAt: lm.glyphIndexForCharacter(at: c), effectiveRange: nil)
        return line.minY + tv.textContainerOrigin.y - sv.contentView.bounds.minY
    }

    /// 丢掉在途的渲染（切到源码、换成别的内容时）。
    private func cancelRender() {
        renderGeneration += 1
        renderPending = false
        renderTargetSourceChar = nil
    }

    // MARK: 源码与渲染之间对位置（2026-11-25：原来两边各记各的，第一次切过去就是开头）

    /// 渲染视图里各块与源码行的对应（`MarkdownRender.sourceLinesKey`）。
    private func blocks() -> [(range: NSRange, lines: (start: Int, end: Int))] {
        if let b = renderBlocks { return b }
        var out: [(range: NSRange, lines: (start: Int, end: Int))] = []
        if let storage = renderView.textStorage {
            storage.enumerateAttribute(MarkdownRender.sourceLinesKey, in: NSRange(location: 0, length: storage.length)) { v, r, _ in
                guard let lines = (v as? NSValue)?.rangeValue else { return }
                out.append((r, (lines.location, NSMaxRange(lines))))
            }
        }
        renderBlocks = out
        return out
    }

    /// 源码里第 `start`—`end` 行（从 1 起，含两端）的字符范围，按源码视图的行首算。
    private func sourceRange(lines: (start: Int, end: Int)) -> NSRange? {
        let starts = ruler.lineStarts
        guard lines.start >= 1, lines.start <= starts.count else { return nil }
        let lo = starts[lines.start - 1]
        let hi = lines.end < starts.count ? starts[lines.end] : textView.textStorage?.length ?? lo
        return NSRange(location: lo, length: max(0, hi - lo))
    }

    /// 渲染里第 `c` 个字符对应源码的哪个字符：找到所在的块（没标行号的往前找最近的一块），块内按比例。
    private func sourceChar(forRenderChar c: Int) -> Int? {
        let bs = blocks()
        guard let i = bs.lastIndex(where: { $0.range.location <= c }), let src = sourceRange(lines: bs[i].lines) else { return nil }
        let b = bs[i].range
        let f = b.length > 0 ? min(1, max(0, Double(c - b.location) / Double(b.length))) : 0
        return src.location + Int(f * Double(src.length))
    }

    /// 源码里第 `c` 个字符对应渲染的哪个字符：找到含这一行的块（没有就取之前最近的一块），块内按比例。
    private func renderChar(forSourceChar c: Int) -> Int? {
        let line = LineNumberRuler.lineIndex(of: c, in: ruler.lineStarts) + 1
        let bs = blocks()
        guard let i = bs.lastIndex(where: { $0.lines.start <= line }) else { return bs.first?.range.location }
        let b = bs[i]
        guard line <= b.lines.end, let src = sourceRange(lines: b.lines), src.length > 0 else { return b.range.location }
        let f = min(1, max(0, Double(c - src.location) / Double(src.length)))
        return b.range.location + Int(f * Double(b.range.length))
    }

    /// 显示源码（等渲染时先显示的、切到源码的；不改这个文件记住的选择）。
    private func showSourceBody(_ text: String, path: String) {
        wrap = wrapChoice
        setBody(scroll)
        setSourceText(text, path: path)
        applyWrap()
        scrollSourceToTop()
        sourceMarker.markVisible()
    }

    /// 测试用：有渲染在途。
    var renderPendingForTesting: Bool { renderPending }
    /// 测试用：源码是否已整篇排好。
    var sourceFullyLaidOutForTesting: Bool {
        guard let lm = textView.layoutManager else { return false }
        return lm.firstUnlaidCharacterIndex() >= (textView.string as NSString).length
    }

    @objc private func toggleRender(_ sender: Any?) {
        guard let path, canRender, let text = lastText else { return }
        let started = CACurrentMediaTime()
        let rendered = bodyView === renderBox
        renderChoice[path] = !rendered
        jumpSourcePath = nil
        // 切过去落在大致相同的内容处：按顶端那一块对应的源码行，块内按比例（见 `sourceChar` / `renderChar`）。
        if rendered {
            let top = Self.topAnchor(renderView, renderScroll)?.char
            cancelRender()
            if textPath == path, sourceText == text {
                // 源码视图里就是这份：只换回来。换回来后重新排一次子视图，否则行首会压在行号栏下面（同 `scrollSourceToTop`）。
                setBody(scroll)
                layoutSubtreeIfNeeded()
                scroll.tile()
            } else {
                showSourceBody(text, path: path)
            }
            if let top, let c = sourceChar(forRenderChar: top) { follower.positioning(textView, scroll) { Self.scroll(textView, scroll, to: (c, 0)) } }
        } else {
            let top = bodyView === scroll && textPath == path ? Self.topAnchor(textView, scroll)?.char : nil
            showRendered(text, path: path, keepScroll: false)
            if let top {
                if renderPending { renderTargetSourceChar = top } else if let c = renderChar(forSourceChar: top) {
                    follower.positioning(renderView, renderScroll) { Self.scroll(renderView, renderScroll, to: (c, 0)) }
                }
            }
        }
        updateButtons()
        scyllauiLog.notice("viewer toggle to=\(rendered ? "source" : "render", privacy: .public) ms=\(Int((CACurrentMediaTime() - started) * 1000)) pending=\(renderPending)")
    }

    /// 测试与菜单：切换源码 / 渲染。
    func toggleRenderForTesting() { toggleRender(nil) }

    /// 点链接：网址与邮件交给系统；其他（本地文件、相对路径等）不打开，悬停提示里写明。
    func textView(_ textView: NSTextView, clickedOnLink link: Any, at charIndex: Int) -> Bool {
        let url = (link as? URL) ?? (link as? String).flatMap(URL.init(string:))
        if let url, MarkdownRender.opensOnClick(url) { NSWorkspace.shared.open(url) }
        return true
    }

    /// 折行按钮：切换并记住，之后新打开的文件沿用；开关前后顶端仍是同一行（按顶端字符，同重读）。
    @objc private func toggleWrap(_ sender: Any?) {
        guard bodyView === scroll else { return }
        let anchor = Self.topAnchor(textView, scroll)
        wrap.toggle()
        wrapChoice = wrap
        follower.positioning(textView, scroll) {
            applyWrap()
            if let anchor { Self.scroll(textView, scroll, to: anchor, x: scroll.contentView.bounds.minX) }
        }
    }

    @objc private func insert(_ sender: Any?) {
        if let path { onInsert?(path) }
    }

    @objc private func toggleTree(_ sender: Any?) { onToggleTree?() }

    // MARK: 「…」菜单与打开方式

    /// 「打开方式」的顺序：默认的排第一，其余按名称（`localizedStandardCompare`），同一个 App（按 `key`）只列一次。
    static func openWithOrder(_ apps: [URL], default def: URL?, name: (URL) -> String, key: (URL) -> String) -> [(URL, Bool)] {
        var seen = Set<String>()
        var rest: [URL] = []
        let defKey = def.map(key)
        if let defKey { seen.insert(defKey) }
        for a in apps where seen.insert(key(a)).inserted { rest.append(a) }
        rest.sort { name($0).localizedStandardCompare(name($1)) == .orderedAscending }
        return (def.map { [($0, true)] } ?? []) + rest.map { ($0, false) }
    }

    static func appName(_ app: URL) -> String {
        let n = FileManager.default.displayName(atPath: app.path)
        return n.hasSuffix(".app") ? String(n.dropLast(4)) : n
    }

    /// 同一个 App 的判定：Bundle ID，读不到时用路径。
    static func appKey(_ app: URL) -> String {
        Bundle(url: app)?.bundleIdentifier ?? app.standardizedFileURL.path
    }

    /// 「…」菜单：拷贝相对路径、打开方式 ▸、在访达中显示（`FileActions`，与 Diff 共用）；折行在标题行的按钮上（第 6 版 R1）。
    func moreMenu() -> NSMenu {
        let menu = NSMenu()
        menu.autoenablesItems = false
        for i in actions.menuItems() { menu.addItem(i) }
        return menu
    }

    @objc private func showMore(_ sender: Any?) {
        moreMenu().popUp(positioning: nil, at: NSPoint(x: 0, y: moreButton.bounds.height + 4), in: moreButton)
    }

    /// 二进制说明里的「打开方式」按钮：在按钮下方弹出同一份 App 列表。
    @objc private func showOpenWith(_ sender: NSButton) {
        actions.openWithMenu().popUp(positioning: nil, at: NSPoint(x: 0, y: sender.bounds.height + 4), in: sender)
    }
}

/// 一个文件的「拷贝相对路径」「打开方式 ▸」「在访达中显示」：文件查看器与 Diff 查看器共用同一份（Diff 改版 D1）。
final class FileActions: NSObject {
    /// 绝对路径；nil 时菜单为空。
    var path: String?
    var root = ""
    /// 「其他…」的选择面板挂在它的窗口上。
    weak var host: NSView?

    /// 三项菜单。`exists` 为 false（文件已删除）时只有拷贝相对路径可用。
    func menuItems(exists: Bool = true) -> [NSMenuItem] {
        let copy = item(String(localized: "拷贝相对路径"), #selector(copyRelative(_:)))
        let open = NSMenuItem(title: String(localized: "打开方式"), action: nil, keyEquivalent: "")
        open.submenu = openWithMenu()
        open.isEnabled = exists
        let reveal = item(String(localized: "在访达中显示"), #selector(revealInFinder(_:)))
        reveal.isEnabled = exists
        return [copy, open, reveal]
    }

    /// 「打开方式」子菜单：能打开这个文件的 App，默认的排第一并标「（默认）」；最后是「其他…」。
    func openWithMenu() -> NSMenu {
        let menu = NSMenu()
        menu.autoenablesItems = false
        guard let path else { return menu }
        let ws = NSWorkspace.shared
        let file = URL(fileURLWithPath: path)
        let apps = FileViewer.openWithOrder(ws.urlsForApplications(toOpen: file), default: ws.urlForApplication(toOpen: file),
                                            name: FileViewer.appName, key: FileViewer.appKey)
        if apps.isEmpty {
            let none = NSMenuItem(title: String(localized: "没有能打开它的 App"), action: nil, keyEquivalent: "")
            none.isEnabled = false
            menu.addItem(none)
        }
        for (app, isDefault) in apps {
            let n = FileViewer.appName(app)
            let i = item(isDefault ? String(localized: "\(n)（默认）") : n, #selector(openWith(_:)))
            i.representedObject = app
            let icon = ws.icon(forFile: app.path)
            icon.size = NSSize(width: 16, height: 16)
            i.image = icon
            // macOS 27 起 AppKit 默认隐藏菜单项的图片，要显式设为可见（SDK 头文件 NSMenuItem.h 的 `preferredImageVisibility`）。
            if #available(macOS 27.0, *) { i.preferredImageVisibility = .visible }
            menu.addItem(i)
        }
        menu.addItem(.separator())
        menu.addItem(item(String(localized: "其他…"), #selector(openWithOther(_:))))
        return menu
    }

    private func item(_ title: String, _ action: Selector) -> NSMenuItem {
        let i = NSMenuItem(title: title, action: action, keyEquivalent: "")
        i.target = self
        return i
    }

    @objc private func copyRelative(_ sender: Any?) {
        guard let path else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(FileViewer.relative(path, root: root), forType: .string)
    }

    @objc private func openWith(_ sender: NSMenuItem) {
        guard let path, let app = sender.representedObject as? URL else { return }
        open(URL(fileURLWithPath: path), with: app)
    }

    /// 「其他…」：在「应用程序」里任选一个 App，只打开这一次，不改系统的默认设置。
    @objc private func openWithOther(_ sender: Any?) {
        guard let path else { return }
        let panel = NSOpenPanel()
        panel.directoryURL = URL(fileURLWithPath: "/Applications")
        panel.allowedContentTypes = [.applicationBundle]
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.message = String(localized: "选择用来打开「\((path as NSString).lastPathComponent)」的 App")
        panel.prompt = String(localized: "打开")
        let done: (NSApplication.ModalResponse) -> Void = { [weak self] r in
            guard r == .OK, let app = panel.url else { return }
            self?.open(URL(fileURLWithPath: path), with: app)
        }
        if let w = host?.window { panel.beginSheetModal(for: w, completionHandler: done) } else { done(panel.runModal()) }
    }

    private func open(_ file: URL, with app: URL) {
        NSWorkspace.shared.open([file], withApplicationAt: app, configuration: NSWorkspace.OpenConfiguration()) { _, error in
            guard let error else { return }
            scyllauiLog.warning("open with failed app=\(app.lastPathComponent, privacy: .public)")
            DispatchQueue.main.async { NSApp.presentError(error) }
        }
    }

    @objc private func revealInFinder(_ sender: Any?) {
        if let path { NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)]) }
    }
}

/// 查看器的文本：`Esc` 先交给查看器（放大时缩回），不走 NSTextView 默认的补全。
/// 查看器的只读文本视图：文件查看器与 Diff 的正文共用（D4）。
class ViewerTextView: NSTextView {
    var onEscape: (() -> Bool)?

    /// `Cmd+↓`、`End` 只动纵向：系统把插入点移到文末再滚到它，最后一行比可见区宽时可见区会被带到行尾（2026-11-25 方案会话
    /// 验收 D4：huge.txt 按 `Cmd+↓` 后每行只剩一串 y）。文件查看器与 Diff 正文共用。
    override func moveToEndOfDocument(_ sender: Any?) { keepingX { super.moveToEndOfDocument(sender) } }
    override func scrollToEndOfDocument(_ sender: Any?) { keepingX { super.scrollToEndOfDocument(sender) } }

    func keepingX(_ body: () -> Void) {
        guard let sv = enclosingScrollView else { return body() }
        let x = sv.contentView.bounds.minX
        body()
        let clip = sv.contentView
        guard abs(clip.bounds.minX - x) > 0.5 else { return }
        clip.scroll(to: clip.constrainBoundsRect(NSRect(origin: NSPoint(x: x, y: clip.bounds.minY), size: clip.bounds.size)).origin)
        sv.reflectScrolledClipView(clip)
    }

    override func cancelOperation(_ sender: Any?) {
        if onEscape?() != true { super.cancelOperation(sender) }
    }

    // 鼠标在压在上面的拖动边缘（EdgeHandle）上时不设文字光标：有键盘焦点时每次移动都会设一次，盖掉边缘的左右箭头
    // （docs/design.md 5.12 节「共用的宿主」）。
    override func mouseMoved(with event: NSEvent) {
        if underMouse(event) { super.mouseMoved(with: event) }
    }

    override func cursorUpdate(with event: NSEvent) {
        if underMouse(event) { super.cursorUpdate(with: event) }
    }

    private func underMouse(_ event: NSEvent) -> Bool {
        guard let content = window?.contentView, let hit = content.hitTest(event.locationInWindow) else { return true }
        return hit === self || hit.isDescendant(of: self)
    }
}

/// 行号（TextKit 1）：只画可见范围里的行，折行时只在每行的第一段画。
final class LineNumberRuler: NSRulerView {
    weak var textView: NSTextView?
    /// 每行第一个字符的 UTF-16 位置。
    private(set) var lineStarts: [Int] = [0]
    private let font = ScyllaUIStyle.mono(11)
    /// 每行写什么（Diff 是旧、新两列行号，已按位数补齐）；nil 时写行号。`labelWidth` 为最宽的一个有几个字符，定栏宽。
    var label: ((Int) -> String)?
    var labelWidth: Int?
    /// 每行的底色（Diff 的增删行与段头，同正文的整行底色）；nil 时不画。折行时铺满这一行的各段。
    var rowColor: ((Int) -> NSColor?)?

    override init(scrollView: NSScrollView?, orientation: NSRulerView.Orientation) {
        super.init(scrollView: scrollView, orientation: orientation)
        clientView = scrollView?.documentView
        ruleThickness = 40
        // 只画在自己范围内：macOS 14 起视图默认不裁剪，系统给的重画范围会超出边界，行号栏照着填底色、画右边线，
        // 伸进了上面的标题行与栏头（2026-11-25 方案会话验收 D4，文件查看器同样）。
        clipsToBounds = true
    }

    required init(coder: NSCoder) { fatalError() }

    override var isFlipped: Bool { true }

    func textChanged() {
        let starts = Self.lineStarts(of: (textView?.string ?? "") as NSString)
        lineStarts = starts
        let digits = labelWidth ?? max(3, String(starts.count).count)
        ruleThickness = CGFloat(digits) * 7.5 + 14
        needsDisplay = true
    }

    /// 每行第一个字符的 UTF-16 位置：数换行符，不依赖排版（源码视图只排看得见的部分）。末尾的换行不另起一行。
    static func lineStarts(of s: NSString) -> [Int] {
        var starts = [0]
        var i = 0
        let n = s.length
        while i < n {
            let r = s.range(of: "\n", options: .literal, range: NSRange(location: i, length: n - i))
            if r.location == NSNotFound { break }
            i = r.location + 1
            starts.append(i)
        }
        if starts.count > 1, starts.last == n { starts.removeLast() }
        return starts
    }

    /// 第 `char` 个字符在第几行（从 0 起）：二分找到不大于它的最后一个行首。
    static func lineIndex(of char: Int, in starts: [Int]) -> Int {
        var lo = 0, hi = starts.count - 1
        while lo < hi {
            let mid = (lo + hi + 1) / 2
            if starts[mid] <= char { lo = mid } else { hi = mid - 1 }
        }
        return lo
    }

    override func drawHashMarksAndLabels(in rect: NSRect) {
        guard let tv = textView, let layout = tv.layoutManager, let container = tv.textContainer else { return }
        let visible = tv.visibleRect
        let glyphs = layout.glyphRange(forBoundingRect: visible, in: container)
        let chars = layout.characterRange(forGlyphRange: glyphs, actualGlyphRange: nil)
        let lo = Self.lineIndex(of: chars.location, in: lineStarts)
        let attrs: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: NSColor.tertiaryLabelColor]
        let offset = convert(NSPoint.zero, from: tv).y
        var line = lo
        let end = NSMaxRange(chars)
        let length = (tv.string as NSString).length
        while line < lineStarts.count, lineStarts[line] <= end {
            let start = lineStarts[line]
            let rectY: CGFloat
            if start >= length {
                rectY = layout.extraLineFragmentRect.minY
            } else {
                let g = layout.glyphIndexForCharacter(at: start)
                rectY = layout.lineFragmentRect(forGlyphAt: g, effectiveRange: nil).minY
            }
            if let color = rowColor?(line) {
                let next = line + 1 < lineStarts.count ? lineStarts[line + 1] : length
                let bottom: CGFloat
                if next < length {
                    bottom = layout.lineFragmentRect(forGlyphAt: layout.glyphIndexForCharacter(at: next), effectiveRange: nil).minY
                } else if length > 0 {
                    bottom = layout.lineFragmentRect(forGlyphAt: layout.glyphIndexForCharacter(at: length - 1), effectiveRange: nil).maxY
                } else {
                    bottom = rectY
                }
                color.setFill()
                NSRect(x: 0, y: rectY + tv.textContainerInset.height + offset, width: ruleThickness, height: bottom - rectY).fill()
            }
            let label = (self.label?(line) ?? "\(line + 1)") as NSString
            let size = label.size(withAttributes: attrs)
            let y = rectY + tv.textContainerInset.height + offset
            label.draw(at: NSPoint(x: ruleThickness - size.width - 6, y: y + 1), withAttributes: attrs)
            line += 1
        }
    }
}

/// 正文里的 U+FFFD（后台按 UTF-8 解不了的字节换成的替换字符）用提醒色标出，相邻的合成一段（2026-11-26 第 7 版）。
/// 只标看得见的部分，用临时属性（`addTemporaryAttribute`，只影响绘制，不改文字存储、不重排），滚到哪里标到哪里，每块只标一次。
/// 整篇一次改存储属性代价太大（第 7 版实测，几乎全是 � 的 1 MB GBK 文件：源码打开多约 0.2 秒，渲染换内容多约 0.23 秒，
/// 「更改」正文换内容多约 1.6 秒）。文件查看器的源码、渲染视图与「更改」正文共用。
final class UndecodableMarker {
    var enabled = false
    private weak var textView: NSTextView?
    private var marked = IndexSet()
    private static let block = 4096

    init(_ textView: NSTextView) { self.textView = textView }

    /// 换了文字：之前标的作废。
    func reset() {
        marked = IndexSet()
        guard let tv = textView, let lm = tv.layoutManager, let n = tv.textStorage?.length, n > 0 else { return }
        lm.removeTemporaryAttribute(.foregroundColor, forCharacterRange: NSRange(location: 0, length: n))
    }

    /// 标可见区里还没标过的块（滚动后、换上文字定位后调用）。
    func markVisible() {
        guard enabled, let tv = textView, tv.window != nil, let lm = tv.layoutManager, let tc = tv.textContainer,
              let ns = tv.textStorage?.mutableString, ns.length > 0 else { return }
        let o = tv.textContainerOrigin
        let glyphs = lm.glyphRange(forBoundingRect: tv.visibleRect.offsetBy(dx: -o.x, dy: -o.y), in: tc)
        let chars = lm.characterRange(forGlyphRange: glyphs, actualGlyphRange: nil)
        guard chars.length > 0 else { return }
        let b = Self.block
        for i in chars.location / b...(NSMaxRange(chars) - 1) / b where !marked.contains(i) {
            marked.insert(i)
            for run in FileViewer.undecodableRuns(ns, in: NSRange(location: i * b, length: min(b, ns.length - i * b))) {
                lm.addTemporaryAttribute(.foregroundColor, value: ScyllaUIStyle.waiting, forCharacterRange: run)
            }
        }
    }
}

/// 只排可见部分时滚到末尾的补救（docs/design.md 5.12 节「滚动条设到最底」）：每个滚动视图记下上一次滚动后的顶端与其间出现过的底部，
/// 往下滚到底时把末尾滚进可见区。文件查看器的源码、渲染与 Diff 的正文共用（Diff 改版 D4 抽出）。
final class EndFollower {
    private var following = false
    /// 每个滚动视图上一次滚动后的可见区顶端，与此后文本出现过的底部（文本变高变低都记，见 `noteFrame`）。
    private var last: [ObjectIdentifier: (minY: CGFloat, bottoms: [CGFloat])] = [:]

    /// 记下此刻的位置与高度（每次滚动后、换了内容排版后调用）。
    func remember(_ tv: NSTextView, _ sv: NSScrollView) {
        last[ObjectIdentifier(sv)] = (sv.contentView.bounds.minY, [tv.frame.maxY])
    }

    /// 程序自己定位（刷新保位、源码与渲染切换的落点）：期间的滚动不当作用户往下滚到底、不补到末尾，完了记下新位置。
    /// 否则换上新内容后按位置滚过去的那一下，会被当成「往下滚到了底」再补到末尾（2026-11-25 方案会话验收 D4：刷新后顶端从 57 跳到末尾附近）。
    func positioning(_ tv: NSTextView, _ sv: NSScrollView, _ body: () -> Void) {
        let was = following
        following = true
        body()
        following = was
        remember(tv, sv)
    }

    /// 文本高度变了（只排可见部分时随排版变）：记下新的底部。
    func noteFrame(_ tv: NSTextView, _ sv: NSScrollView) {
        let key = ObjectIdentifier(sv)
        guard var s = last[key] else { return }
        s.bottoms = Array((s.bottoms + [tv.frame.maxY]).suffix(16))
        last[key] = s
    }

    /// 只排可见部分时总高度是估的：滚动条直接设到最底，停在估算的底部；排好看得见的这一屏后实际更高，末尾还有十几行没到
    /// （2026-11-25 测试版：1488 行的 Markdown 文件停在第 1474 行附近，再设一次才到；渲染视图同样）。源码、渲染共用。
    /// 只在这一步是往下滚、且落点到了文本的底部（此刻的，或上次滚动以来出现过的最低的：设到最底时落点按滚动开始时的高度算，
    /// 收到通知前文本已按新排的那一屏变了高）时，把末尾滚进可见区；只排末尾附近，不整篇排版。
    /// 不能按滚动条的值判断：滚轮、键盘滚动时通知先到，值还是滚动前的，到底后往上滚的每一步都会被当成在底部又贴回去
    /// （2026-11-25 方案会话复验：到底后滚轮往上、⌘↑ 都不动）。⌘↓、End 走 `scrollRangeToVisible`，本来就一次到底。
    func scrolled(_ tv: NSTextView, _ sv: NSScrollView) {
        let clip = sv.contentView
        let prev = last[ObjectIdentifier(sv)]
        defer { remember(tv, sv) }
        guard sv.window != nil, !following, let prev, let length = tv.textStorage?.length, length > 0,
              clip.bounds.minY > prev.minY + 0.5 else { return }
        let bottom = clip.bounds.maxY
        // 落点到了上次滚动以来文本出现过的最低的底部（滚动前的底部，或其间排版变低后的）就算到底：设到最底时落点按滚动开始时
        // 的高度算，收到通知时文本已变高（测试版日志：落点 37716，其间底部 33056—33115，此刻 40153）。往下滚到底之前都到不了它。
        guard bottom >= tv.frame.maxY - 1 || bottom >= (prev.bottoms.min() ?? .infinity) - 1 else { return }
        // 与 ⌘↓ 同一个办法：把末尾滚进可见区，只排末尾附近（原来一屏一屏往下追最多 10 次，表格多的渲染视图追不完）。
        // 等这一轮事件处理完再滚：滚动条设值时，通知之后滚动条还会按它的值把位置再设一次，当场滚过去会被改回（测试版日志：
        // 滚到 40153 后又回到 37716）。
        // 横向不动：末尾那一行比可见区宽时，`scrollRangeToVisible` 会把可见区往右挪到行尾（Diff 的 huge.txt 测试版实测）。
        DispatchQueue.main.async { [weak self, weak tv, weak sv] in
            guard let self, let tv, let sv, let n = tv.textStorage?.length, n > 0 else { return }
            self.following = true
            defer { self.following = false }
            let x = sv.contentView.bounds.minX
            tv.scrollRangeToVisible(NSRange(location: n, length: 0))
            let clip = sv.contentView
            clip.scroll(to: clip.constrainBoundsRect(NSRect(origin: NSPoint(x: x, y: clip.bounds.minY), size: clip.bounds.size)).origin)
            sv.reflectScrolledClipView(clip)
        }
    }
}
