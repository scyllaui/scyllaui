// 右侧栏「文件」（docs/design.md 5.12 节）：当前终端所在项目的文件树，逐级按需读（`fs.list`），隐藏文件照常显示、忽略的灰显；
// 按名称筛选（`fs.find`）；点文件在左边的只读查看器里看，树留在右边、可隐藏。每个项目根各记一份状态（只在内存里）。
// 栏头横跨整栏，放大与关闭只在栏头右上角（实现方案第 4 版 F1）。
// 后台请求经 `request` 发出（工作区接到 CoreClient，回调在主线程），便于单测替换。

import AppKit
import UniformTypeIdentifiers

/// 树上的一项。`children == nil` 表示这一层还没读。
final class FileNode {
    enum Kind: Equatable {
        case dir, file, link
        /// 说明行（「还有 N 项未列出」、读不到的原因），不可选。
        case note
    }

    let name: String
    let path: String
    var kind: Kind
    var ignored: Bool
    var children: [FileNode]?
    weak var parent: FileNode?

    init(name: String, path: String, kind: Kind, ignored: Bool = false, parent: FileNode? = nil) {
        self.name = name
        self.path = path
        self.kind = kind
        self.ignored = ignored
        self.parent = parent
    }

    var isDir: Bool { kind == .dir }

    /// 子项按名字找（不含说明行）。
    func child(named n: String) -> FileNode? { children?.first { $0.kind != .note && $0.name == n } }
}

/// 一个项目根的树与状态（展开了哪些、打开的文件、树是否隐藏），切走再切回原样恢复。
final class FileTree {
    let root: FileNode
    var gitRoot: String?
    var expanded: Set<String> = []
    var openFile: String?
    var treeHidden = false
    /// 打开着文件，但查看器收起、只剩树（F3：左边缘把查看器拖到 160 以下）。
    var contentCollapsed = false
    /// 上次展开到的终端目录：变了才重新展开。
    var revealed: String?
    /// 终端此刻所在的目录（树上标出）。
    var cwd: String?
    private var latest: [String: Latest] = [:]

    init(root: String, gitRoot: String?) {
        self.root = FileNode(name: (root as NSString).lastPathComponent, path: root, kind: .dir)
        self.gitRoot = gitRoot
    }

    func latest(_ key: String) -> Latest {
        if let l = latest[key] { return l }
        let l = Latest()
        latest[key] = l
        return l
    }

    /// 按绝对路径找已读到的节点（根本身也算）。
    func node(at path: String) -> FileNode? {
        if path == root.path { return root }
        let prefix = root.path.hasSuffix("/") ? root.path : root.path + "/"
        guard path.hasPrefix(prefix) else { return nil }
        var node = root
        for part in path.dropFirst(prefix.count).split(separator: "/") {
            guard let next = node.child(named: String(part)) else { return nil }
            node = next
        }
        return node
    }

    /// 后台的一层目录合并进 `node`：保留名字没变的节点对象（展开状态不丢），新的建节点，没了的去掉；文件夹在前，
    /// 按 `localizedStandardCompare` 排；父项被忽略的子项一律算忽略；超过上限的写一行说明。
    static func merge(into node: FileNode, items: [[String: Any]], more: Int) {
        var old: [String: FileNode] = [:]
        for c in node.children ?? [] where c.kind != .note { old[c.name] = c }
        var out: [FileNode] = []
        for item in items {
            guard let name = item["name"] as? String else { continue }
            let kind: FileNode.Kind = switch item["kind"] as? String {
            case "dir": .dir
            case "link": .link
            default: .file
            }
            let ignored = node.ignored || (item["ignored"] as? Bool ?? false)
            if let n = old[name], n.kind == kind {
                n.ignored = ignored
                out.append(n)
            } else {
                out.append(FileNode(name: name, path: (node.path as NSString).appendingPathComponent(name), kind: kind, ignored: ignored, parent: node))
            }
        }
        out.sort { a, b in
            if a.isDir != b.isDir { return a.isDir }
            return a.name.localizedStandardCompare(b.name) == .orderedAscending
        }
        if more > 0 {
            out.append(FileNode(name: String(localized: "还有 \(more) 项未列出 · 用筛选查找"), path: node.path + "/\u{0}more", kind: .note, parent: node))
        } else if out.isEmpty {
            out.append(FileNode(name: String(localized: "空文件夹"), path: node.path + "/\u{0}empty", kind: .note, parent: node))
        }
        node.children = out
    }

    /// 读不到这一层时的说明。
    static func failure(_ reply: [String: Any]?) -> String {
        switch (reply?["error"] as? [String: Any])?["code"] as? String {
        case "denied": String(localized: "没有权限读取这个文件夹")
        case "not_found": String(localized: "文件夹已不存在")
        default: String(localized: "暂时读不到这个文件夹")
        }
    }
}

final class FilesPanel: NSView, NSOutlineViewDataSource, NSOutlineViewDelegate, NSTableViewDataSource, NSTableViewDelegate, NSSearchFieldDelegate {
    /// 发后台请求；回调在主线程，nil 为没连上或断线。
    var request: (([String: Any], @escaping ([String: Any]?) -> Void) -> Void)?
    /// 关文件栏（栏头的关闭在没有打开的文件时）。
    var onClose: (() -> Void)?
    /// 查看器开合、树显示与否变了：工作区据此换右侧栏的宽度。
    var onLayoutChange: (() -> Void)?
    var onZoom: (() -> Void)?
    var onInsert: ((String) -> Void)?
    var onEscape: (() -> Bool)? {
        didSet { viewer.onEscape = onEscape }
    }

    let viewer = FileViewer()
    /// 查看器放大着（工作区设）：栏头的放大按钮换成缩回。
    var zoomed = false {
        didSet { updateHeader() }
    }
    /// 树宽（F3）：打开文件后树栏的宽度（与 Diff 的改动列表共用一个值）。分隔线拖到 160 以下树隐藏，最宽到查看器剩 160。
    /// 分栏、拖动、收起与滑动在 `ListSplit`，与 Diff 栏共用；这里的常量与时钟只是转引。
    static let defaultTreeWidth = ListSplit.defaultWidth
    static let treeHideWidth = ListSplit.hideWidth
    static let viewerMinWidth = ListSplit.viewerMinWidth
    static let expandHysteresis = ListSplit.expandHysteresis
    static var reduceMotion: () -> Bool {
        get { ListSplit.reduceMotion }
        set { ListSplit.reduceMotion = newValue }
    }
    static var now: () -> CFTimeInterval {
        get { ListSplit.now }
        set { ListSplit.now = newValue }
    }
    var treeWidth: CGFloat {
        get { split.width }
        set { split.width = newValue }
    }

    static func edgeDrag(width: CGFloat, treeWidth: CGFloat, collapsed: Bool, room: CGFloat) -> (collapsed: Bool, width: CGFloat) {
        ListSplit.edgeDrag(width: width, listWidth: treeWidth, collapsed: collapsed, room: room)
    }

    /// 栏的底色（同右侧栏宿主）：收起、展开的过渡里树要盖住查看器，得有底色。
    var fillColor: NSColor {
        get { split.fillColor }
        set { split.fillColor = newValue }
    }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        split?.applyFill()
    }
    /// 拖分隔线改了树宽（松手时）：工作区记下来。
    var onTreeWidth: ((CGFloat) -> Void)?
    private(set) var tree: FileTree?
    private var trees: [String: FileTree] = [:]

    private let column = NSView()
    private let header = RightPanelHeader(symbol: "folder", title: String(localized: "文件"), closeName: String(localized: "关闭文件栏"))
    private var collapseButton: NSButton?
    private var zoomButton: NSButton?
    private let filter = NSSearchField()
    private let outline = NSOutlineView()
    private let outlineScroll = NSScrollView()
    private let results = NSTableView()
    private let resultsScroll = NSScrollView()
    private let note = NSTextField(wrappingLabelWithString: "")
    private let divider = NSBox()
    /// 查看器与树之间的分栏（分隔线、收起与滑动），与 Diff 栏同一份。
    private var split: ListSplit!
    private var expandButton: NSButton?
    /// 筛选结果：按名称时为平铺的文件与文件夹；`?` 开头搜内容时按文件分组（文件一行，下面是命中的行）。
    private enum Result {
        case item(path: String, kind: FileNode.Kind)
        case group(path: String)
        case hit(path: String, line: Int, text: String)
        case note(String)
    }
    private var found: [Result] = []
    /// 这一组内容搜索结果搜的是什么：结果行从匹配处附近显示、标出匹配的字。
    private var hitQuery = ""
    /// 有改动的文件（相对根的路径 → M、A）与它们所在的各级目录（ChangesStore，第 3 期）。
    private var marks: [String: String] = [:]
    private var dirtyDirs: Set<String> = []
    private let findLatest = Latest()
    private var findWork: DispatchWorkItem?
    private let readLatest = Latest()

    override init(frame: NSRect) {
        super.init(frame: frame)
        collapseButton = header.addTool(symbol: "rectangle.compress.vertical", name: String(localized: "全部折叠"), target: self, action: #selector(collapseAll(_:)))
        expandButton = header.addTool(symbol: "sidebar.left", name: String(localized: "显示文件内容"), target: self, action: #selector(expandContent(_:)))
        zoomButton = header.addTool(symbol: "arrow.up.left.and.arrow.down.right", name: String(localized: "放大"), target: self, action: #selector(zoom(_:)))
        // 打开着文件时先关文件、回到只有树；没有文件时关栏。
        header.onClose = { [weak self] in
            guard let self else { return }
            if self.viewerOpen { self.closeFile() } else { self.onClose?() }
        }
        header.subtitle.font = ScyllaUIStyle.mono(11)
        header.subtitle.textColor = .tertiaryLabelColor
        filter.placeholderString = String(localized: "筛选，或输入 ? 搜索内容")
        filter.font = ScyllaUIStyle.detailFont
        filter.sendsSearchStringImmediately = true
        filter.target = self
        filter.action = #selector(filterChanged(_:))
        filter.delegate = self
        for (table, scroll) in [(outline as NSTableView, outlineScroll), (results, resultsScroll)] {
            let col = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("name"))
            col.resizingMask = .autoresizingMask
            table.addTableColumn(col)
            table.headerView = nil
            table.style = .plain
            table.backgroundColor = .clear
            table.rowHeight = 22
            table.intercellSpacing = NSSize(width: 0, height: 0)
            table.columnAutoresizingStyle = .uniformColumnAutoresizingStyle
            table.target = self
            table.action = #selector(rowClicked(_:))
            scroll.documentView = table
            scroll.drawsBackground = false
            scroll.hasVerticalScroller = true
            scroll.autohidesScrollers = true
        }
        outline.outlineTableColumn = outline.tableColumns.first
        outline.indentationPerLevel = 12
        outline.autoresizesOutlineColumn = false
        outline.dataSource = self
        outline.delegate = self
        results.dataSource = self
        results.delegate = self
        resultsScroll.isHidden = true
        note.font = ScyllaUIStyle.detailFont
        note.textColor = .secondaryLabelColor
        note.alignment = .center
        note.isHidden = true
        divider.boxType = .separator
        viewer.isHidden = true
        viewer.onInsert = { [weak self] p in self?.onInsert?(p) }
        viewer.onToggleTree = { [weak self] in self?.toggleTree() }
        for v in [filter, outlineScroll, resultsScroll, note, divider] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            column.addSubview(v)
        }
        // 树在查看器之上：收起、展开的过渡里由树盖住查看器（平时两者不重叠）。分隔线的热区与蓝线由 `ListSplit` 加在最上面。
        for v in [header, viewer, column] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            addSubview(v)
        }
        split = ListSplit(host: self, viewer: viewer, column: column, divider: divider)
        split.logName = "files tree"
        split.isOpen = { [weak self] in self?.viewerOpen ?? false }
        split.listHidden = { [weak self] in self?.treeHidden ?? false }
        split.setListHidden = { [weak self] on in self?.tree?.treeHidden = on }
        split.collapsed = { [weak self] in self?.contentCollapsed ?? false }
        split.onLayout = { [weak self] in self?.applyLayout() }
        split.onWidth = { [weak self] w in self?.onTreeWidth?(w) }
        NSLayoutConstraint.activate([
            header.topAnchor.constraint(equalTo: topAnchor),
            header.leadingAnchor.constraint(equalTo: leadingAnchor),
            header.trailingAnchor.constraint(equalTo: trailingAnchor),
            column.topAnchor.constraint(equalTo: header.bottomAnchor, constant: 10),
            column.bottomAnchor.constraint(equalTo: bottomAnchor),
            column.trailingAnchor.constraint(equalTo: trailingAnchor),
            viewer.topAnchor.constraint(equalTo: header.bottomAnchor, constant: 4),
            viewer.bottomAnchor.constraint(equalTo: bottomAnchor),
            viewer.leadingAnchor.constraint(equalTo: leadingAnchor),
            filter.topAnchor.constraint(equalTo: column.topAnchor),
            filter.leadingAnchor.constraint(equalTo: column.leadingAnchor, constant: 10),
            filter.trailingAnchor.constraint(equalTo: column.trailingAnchor, constant: -10),
            note.topAnchor.constraint(equalTo: filter.bottomAnchor, constant: 24),
            note.leadingAnchor.constraint(equalTo: column.leadingAnchor, constant: 16),
            note.trailingAnchor.constraint(equalTo: column.trailingAnchor, constant: -16),
            divider.topAnchor.constraint(equalTo: column.topAnchor),
            divider.bottomAnchor.constraint(equalTo: column.bottomAnchor),
            divider.leadingAnchor.constraint(equalTo: column.leadingAnchor),
            divider.widthAnchor.constraint(equalToConstant: 1),
        ])
        for scroll in [outlineScroll, resultsScroll] {
            NSLayoutConstraint.activate([
                scroll.topAnchor.constraint(equalTo: filter.bottomAnchor, constant: 8),
                scroll.leadingAnchor.constraint(equalTo: column.leadingAnchor),
                scroll.trailingAnchor.constraint(equalTo: column.trailingAnchor),
                scroll.bottomAnchor.constraint(equalTo: column.bottomAnchor),
            ])
        }
        applyLayout()
    }

    required init?(coder: NSCoder) { fatalError() }

    // MARK: 状态

    var viewerOpen: Bool { tree?.openFile != nil }
    var treeHidden: Bool {
        get { viewerOpen && (tree?.treeHidden ?? false) }
        set {
            tree?.treeHidden = newValue
            applyLayout()
        }
    }
    /// 查看器收起、只剩树（文件仍打开着）。树隐藏时不收起。
    var contentCollapsed: Bool {
        get { viewerOpen && (tree?.contentCollapsed ?? false) }
        set {
            let wasShown = contentShown
            let viewerWidth = viewer.frame.width
            tree?.contentCollapsed = newValue
            if wasShown != contentShown, !treeHidden { split.startSlide(collapsing: !contentShown, viewerWidth: viewerWidth) }
            applyLayout()
        }
    }
    /// 查看器此刻显示着（打开了文件且没收起）：工作区按它用查看宽。
    var contentShown: Bool { viewerOpen && !contentCollapsed }

    /// 换到另一个项目根（或同一根下终端换了目录）。nil 为没有终端。
    func setRoot(_ root: String?, gitRoot: String?, currentDir: String?) {
        guard let root else {
            tree = nil
            header.subtitle.stringValue = ""
            outline.reloadData()
            showNote(StatusPanel.noTerminal)
            closeViewerOnly()
            applyLayout()
            return
        }
        let cwd = currentDir.map(Self.realPath)
        if tree?.root.path != root {
            let t = trees[root] ?? FileTree(root: root, gitRoot: gitRoot)
            trees[root] = t
            tree = t
            header.subtitle.stringValue = (root as NSString).abbreviatingWithTildeInPath
            header.subtitle.toolTip = root
            if !filter.stringValue.isEmpty {
                filter.stringValue = ""
                showTree()
            }
            showNote(nil)
            outline.reloadData()
            if t.root.children == nil {
                load(t.root)
            } else {
                restoreExpansion(t.root)
                load(t.root)
            }
            if let f = t.openFile { openFile(f, expand: false) } else { closeViewerOnly() }
            applyLayout()
        }
        guard let t = tree else { return }
        t.gitRoot = gitRoot
        t.cwd = cwd
        if let cwd, cwd != t.revealed {
            t.revealed = cwd
            reveal(cwd, select: true)
        }
    }

    /// FSEvents 报来的有变化的目录：重读已读过的（展开着的立即读，收着的下次展开再读）；打开的文件所在目录变了就重读它。
    func refresh(dirs: [String]) {
        guard let t = tree else { return }
        for raw in Set(dirs) {
            let dir = raw.count > 1 && raw.hasSuffix("/") ? String(raw.dropLast()) : raw
            if let node = t.node(at: dir), node.isDir, node.children != nil {
                if node === t.root || outline.isItemExpanded(node) {
                    load(node)
                } else {
                    node.children = nil
                    outline.reloadItem(node, reloadChildren: true)
                }
            }
            if let f = t.openFile, (f as NSString).deletingLastPathComponent == dir, !FileViewer.usesQuickLook(f) {
                read(f, keepScroll: true)
            }
        }
    }

    /// 解析符号链接后的真实路径（`/tmp` → `/private/tmp`），与后台的项目目录同一写法；解析失败按原样。
    static func realPath(_ p: String) -> String {
        guard let r = realpath(p, nil) else { return p }
        defer { free(r) }
        return String(cString: r)
    }

    // MARK: 读目录

    private func load(_ node: FileNode, then: (() -> Void)? = nil) {
        guard let t = tree else { return }
        t.latest(node.path).submit { [weak self] done in
            guard let self else { return done() }
            var req: [String: Any] = ["type": "fs.list", "dir": node.path]
            if let g = t.gitRoot, !node.ignored { req["git_root"] = g }
            guard let request = self.request else { return done() }
            request(req) { [weak self] reply in
                defer { done() }
                guard let self, self.tree === t else { return }
                self.apply(reply, to: node, in: t)
                then?()
            }
        }
    }

    private func apply(_ reply: [String: Any]?, to node: FileNode, in t: FileTree) {
        if let r = reply, r["ok"] as? Bool == true {
            FileTree.merge(into: node, items: r["items"] as? [[String: Any]] ?? [], more: (r["more"] as? NSNumber)?.intValue ?? 0)
        } else {
            node.children = [FileNode(name: FileTree.failure(reply), path: node.path + "/\u{0}error", kind: .note, parent: node)]
        }
        outline.reloadItem(node === t.root ? nil : node, reloadChildren: true)
        restoreExpansion(node)
        if node === t.root { showNote(nil) }
    }

    /// 重新展开记着的子文件夹（重读后、切回这个根时）。
    private func restoreExpansion(_ node: FileNode) {
        guard let t = tree else { return }
        for c in node.children ?? [] where c.isDir && t.expanded.contains(c.path) {
            outline.expandItem(c)
            if c.children != nil { restoreExpansion(c) }
        }
    }

    /// 逐级展开到 `path`（终端所在目录、筛选结果里点的文件夹，或清空查找框回到树时当前打开的文件），选中并滚到那一行。
    private func reveal(_ path: String, select: Bool) {
        guard let t = tree else { return }
        let prefix = t.root.path.hasSuffix("/") ? t.root.path : t.root.path + "/"
        let parts = path == t.root.path ? [] : (path.hasPrefix(prefix) ? path.dropFirst(prefix.count).split(separator: "/").map(String.init) : nil)
        guard let parts else { return }
        func step(_ node: FileNode, _ i: Int) {
            guard self.tree === t else { return }
            guard i < parts.count else {
                if select, node !== t.root {
                    let row = outline.row(forItem: node)
                    if row >= 0 {
                        outline.selectRowIndexes([row], byExtendingSelection: false)
                        outline.scrollRowToVisible(row)
                    }
                }
                return
            }
            guard let children = node.children else { return load(node) { step(node, i) } }
            let last = i == parts.count - 1
            guard let next = children.first(where: { $0.name == parts[i] && ($0.isDir || last) }) else { return }
            // 最后一级是文件：不展开，直接选中。
            guard next.isDir else { return step(next, i + 1) }
            outline.expandItem(next)
            t.expanded.insert(next.path)
            if next.children == nil { load(next) { step(next, i + 1) } } else { step(next, i + 1) }
        }
        step(t.root, 0)
    }

    @objc private func collapseAll(_ sender: Any?) {
        outline.collapseItem(nil, collapseChildren: true)
        tree?.expanded = []
    }

    // MARK: 打开文件

    /// `expand`：查看器收起着时展开它（点文件、从别处打开）；切回原根恢复时为 false，保持原样。
    func openFile(_ path: String, line: Int? = nil, expand: Bool = true) {
        guard let t = tree else { return }
        let wasOpen = viewerOpen && !(expand && t.contentCollapsed)
        if expand { t.contentCollapsed = false }
        t.openFile = path
        viewer.begin(path: path, root: t.root.path)
        if let line { viewer.reveal(line: line) }
        viewer.isHidden = false
        if !FileViewer.usesQuickLook(path) { read(path, keepScroll: false) }
        if !wasOpen { applyLayout() }
    }

    /// 从别处（Diff 的「在『文件』里打开」）打开：打开文件，并在树里逐级展开、选中它（D2 顺手修）。
    func openAndReveal(_ path: String) {
        openFile(path)
        reveal(path, select: true)
    }

    func closeFile() {
        tree?.openFile = nil
        tree?.contentCollapsed = false
        closeViewerOnly()
        applyLayout()
    }

    private func closeViewerOnly() {
        viewer.clear()
        viewer.isHidden = true
    }

    private func read(_ path: String, keepScroll: Bool) {
        readLatest.submit { [weak self] done in
            guard let self, let request = self.request else { return done() }
            request(["type": "fs.read", "path": path]) { [weak self] reply in
                defer { done() }
                guard let self, self.viewer.path == path else { return }
                self.viewer.show(reply: reply, keepScroll: keepScroll)
            }
        }
    }

    private func toggleTree() {
        guard viewerOpen else { return }
        treeHidden.toggle()
    }

    private func applyLayout() {
        applyConstraints()
        updateHeader()
        onLayoutChange?()
    }

    private func applyConstraints() {
        split.applyConstraints()
        viewer.treeHidden = treeHidden
    }

    @objc private func expandContent(_ sender: Any?) { contentCollapsed = false }

    /// 测试用：过渡进行中、树此刻的宽度。
    var slidingForTesting: Bool { split.sliding }
    func stepSlideForTesting() { split.step() }
    var treeColumnWidthForTesting: CGFloat { column.frame.width }

    /// 测试用：模拟拖分隔线（`dx` 为本次移动，往右为正）。
    func dragSplitForTesting(_ dx: CGFloat, done: Bool) { split.drag(dx, done: done) }

    /// 栏头右上角：全部折叠只在树显示时有，放大只在打开文件后有；关闭按钮的功能名随之换。
    private func updateHeader() {
        let open = viewerOpen
        collapseButton?.isHidden = contentShown && treeHidden
        zoomButton?.isHidden = !contentShown
        expandButton?.isHidden = !contentCollapsed
        zoomButton?.image = NSImage(systemSymbolName: zoomed ? "arrow.down.right.and.arrow.up.left" : "arrow.up.left.and.arrow.down.right", accessibilityDescription: nil)
        zoomButton?.toolTip = zoomed ? String(localized: "缩回") : String(localized: "放大")
        zoomButton?.setAccessibilityLabel(zoomButton?.toolTip)
        header.closeName = open ? String(localized: "关闭文件") : String(localized: "关闭文件栏")
    }

    @objc private func zoom(_ sender: Any?) { onZoom?() }

    /// 测试与读屏：栏头关闭按钮此刻的功能名、放大按钮是否显示。
    var closeName: String { header.closeName }
    var zoomShown: Bool { zoomButton?.isHidden == false }
    var expandShown: Bool { expandButton?.isHidden == false }
    func closeFileFromHeaderForTesting() { header.onClose?() }

    // MARK: 筛选

    @objc private func filterChanged(_ sender: Any?) {
        findWork?.cancel()
        let raw = filter.stringValue
        let q = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !q.isEmpty else {
            showTree()
            // 回到树时选中当前打开的文件：从搜索结果打开别的文件后，树里还高亮着原来那个（2026-11-25 方案会话复验）。
            if let open = tree?.openFile { reveal(open, select: true) }
            return
        }
        // `?` 开头搜内容，停顿 300 ms 再搜；只有一个 `?` 时等输入。
        let content = q.hasPrefix("?")
        let text = content ? String(q.dropFirst()).trimmingCharacters(in: .whitespaces) : q
        if content && text.isEmpty {
            found = [.note(String(localized: "在 ? 后输入要搜的内容"))]
            return presentResults()
        }
        let work = DispatchWorkItem { [weak self] in self?.find(q) }
        findWork = work
        DispatchQueue.main.asyncAfter(deadline: .now() + (content ? 0.3 : 0.15), execute: work)
    }

    /// 发筛选请求：`q` 以 `?` 开头为内容搜索（`fs.grep`），否则按名称（`fs.find`）。
    private func find(_ q: String) {
        guard let t = tree else { return }
        let content = q.hasPrefix("?")
        let text = content ? String(q.dropFirst()).trimmingCharacters(in: .whitespaces) : q
        findLatest.submit { [weak self] done in
            guard let self, let request = self.request else { return done() }
            request(["type": content ? "fs.grep" : "fs.find", "root": t.root.path, "query": text]) { [weak self] reply in
                defer { done() }
                guard let self, self.tree === t,
                      self.filter.stringValue.trimmingCharacters(in: .whitespacesAndNewlines) == q else { return }
                content ? self.showHits(reply, query: text) : self.showResults(reply)
            }
        }
    }

    private func showResults(_ reply: [String: Any]?) {
        guard let r = reply, r["ok"] as? Bool == true else {
            found = [.note(String(localized: "暂时搜不了，稍后再试"))]
            return presentResults()
        }
        found = (r["items"] as? [[String: Any]] ?? []).compactMap { d in
            guard let p = d["path"] as? String else { return nil }
            let kind: FileNode.Kind = d["kind"] as? String == "dir" ? .dir : (d["kind"] as? String == "link" ? .link : .file)
            return .item(path: p, kind: kind)
        }
        if found.isEmpty {
            found = [.note(String(localized: "没有名称符合的文件"))]
        } else if r["truncated"] as? Bool == true {
            found.append(.note(String(localized: "只列出了一部分，输入更多字缩小范围")))
        }
        presentResults()
    }

    /// 内容搜索的结果按文件分组，保持后台给的顺序。
    private func showHits(_ reply: [String: Any]?, query: String) {
        hitQuery = query
        guard let r = reply, r["ok"] as? Bool == true else {
            let code = (reply?["error"] as? [String: Any])?["code"] as? String
            found = [.note(code == "no_git" ? String(localized: "搜内容需要 Xcode 命令行工具")
                           : code == "unsafe_filter" ? String(localized: "这个仓库配置了 ScyllaUI 不执行的过滤器，不搜内容")
                           : String(localized: "暂时搜不了，稍后再试"))]
            return presentResults()
        }
        var out: [Result] = []
        var last: String?
        for d in r["items"] as? [[String: Any]] ?? [] {
            guard let p = d["path"] as? String, let n = (d["line"] as? NSNumber)?.intValue else { continue }
            if p != last {
                out.append(.group(path: p))
                last = p
            }
            out.append(.hit(path: p, line: n, text: d["text"] as? String ?? ""))
        }
        if out.isEmpty {
            out = [.note(String(localized: "没有文件包含这段内容"))]
        } else if r["truncated"] as? Bool == true {
            out.append(.note(String(localized: "只列出了一部分（每个文件最多 50 条，总共最多 1000 条）")))
        }
        found = out
        presentResults()
    }

    /// 改动标记（ChangesStore 的新结果）：只重画可见的行，不重载树（展开状态不动）。
    func setMarks(_ marks: [String: String], dirty: Set<String>) {
        guard marks != self.marks || dirty != dirtyDirs else { return }
        self.marks = marks
        dirtyDirs = dirty
        let rows = outline.numberOfRows
        if rows > 0 { outline.reloadData(forRowIndexes: IndexSet(0..<rows), columnIndexes: [0]) }
    }

    private func relative(_ path: String) -> String {
        guard let root = tree?.root.path else { return path }
        return FileViewer.relative(path, root: root)
    }

    private func presentResults() {
        outlineScroll.isHidden = true
        resultsScroll.isHidden = false
        results.reloadData()
    }

    private func showTree() {
        found = []
        resultsScroll.isHidden = true
        outlineScroll.isHidden = false
    }

    private func showNote(_ text: String?) {
        note.stringValue = text ?? ""
        note.isHidden = text == nil
        outlineScroll.isHidden = text != nil || !filter.stringValue.isEmpty
    }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
        guard control === filter, selector == #selector(NSResponder.cancelOperation(_:)) else { return false }
        filter.stringValue = ""
        filterChanged(nil)
        return true
    }

    // MARK: 点击

    @objc private func rowClicked(_ sender: Any?) {
        if sender as AnyObject === outline {
            guard let node = outline.item(atRow: outline.clickedRow) as? FileNode else { return }
            switch node.kind {
            case .dir: outline.isItemExpanded(node) ? outline.collapseItem(node) : outline.expandItem(node)
            case .file, .link: openFile(node.path)
            case .note: break
            }
        } else {
            openResult(results.clickedRow)
        }
    }

    /// 点筛选结果的第 `row` 行。
    private func openResult(_ row: Int) {
        guard let t = tree, row >= 0, row < found.count else { return }
        let abs = { (p: String) in (t.root.path as NSString).appendingPathComponent(p) }
        switch found[row] {
        case .item(let path, .dir):
            filter.stringValue = ""
            showTree()
            reveal(abs(path), select: true)
        case .item(let path, _), .group(let path):
            openFile(abs(path))
        case .hit(let path, let line, _):
            openFile(abs(path), line: line)
        case .note:
            break
        }
    }

    // MARK: 树的数据源

    func outlineView(_ outlineView: NSOutlineView, numberOfChildrenOfItem item: Any?) -> Int {
        guard let t = tree else { return 0 }
        let node = (item as? FileNode) ?? t.root
        return node.children?.count ?? 0
    }

    func outlineView(_ outlineView: NSOutlineView, child index: Int, ofItem item: Any?) -> Any {
        let node = (item as? FileNode) ?? tree!.root
        return node.children![index]
    }

    func outlineView(_ outlineView: NSOutlineView, isItemExpandable item: Any) -> Bool {
        (item as? FileNode)?.isDir ?? false
    }

    func outlineView(_ outlineView: NSOutlineView, shouldSelectItem item: Any) -> Bool {
        (item as? FileNode)?.kind != .note
    }

    func outlineViewItemDidExpand(_ notification: Notification) {
        guard let node = notification.userInfo?["NSObject"] as? FileNode, let t = tree else { return }
        t.expanded.insert(node.path)
        if node.children == nil { load(node) }
    }

    func outlineViewItemDidCollapse(_ notification: Notification) {
        guard let node = notification.userInfo?["NSObject"] as? FileNode else { return }
        tree?.expanded.remove(node.path)
    }

    func outlineView(_ outlineView: NSOutlineView, viewFor tableColumn: NSTableColumn?, item: Any) -> NSView? {
        guard let node = item as? FileNode else { return nil }
        let cell = outlineView.makeView(withIdentifier: FileCell.id, owner: nil) as? FileCell ?? FileCell()
        cell.show(name: node.name, detail: nil, kind: node.kind, ignored: node.ignored, path: node.path,
                  here: node.isDir && node.path == tree?.cwd)
        let rel = relative(node.path)
        if node.kind == .file || node.kind == .link {
            cell.setMark(marks[rel], dirty: false)
        } else {
            cell.setMark(nil, dirty: node.isDir && !outlineView.isItemExpanded(node) && dirtyDirs.contains(rel))
        }
        return cell
    }

    // MARK: 筛选结果的数据源

    func numberOfRows(in tableView: NSTableView) -> Int { found.count }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        let cell = tableView.makeView(withIdentifier: FileCell.id, owner: nil) as? FileCell ?? FileCell()
        switch found[row] {
        case .note(let text):
            cell.show(name: text, detail: nil, kind: .note, ignored: false, path: "", here: false)
        case .item(let path, let kind):
            Self.showPath(cell, path, kind)
        case .group(let path):
            Self.showPath(cell, path, .file)
        case .hit(_, let line, let text):
            cell.showHit(line: line, text: text, query: hitQuery)
        }
        cell.setMark(nil, dirty: false)
        return cell
    }

    func tableView(_ tableView: NSTableView, shouldSelectRow row: Int) -> Bool {
        if case .note = found[row] { return false }
        return true
    }

    /// 筛选结果里的文件或文件夹：名字后接灰色的所在目录。
    private static func showPath(_ cell: FileCell, _ path: String, _ kind: FileNode.Kind) {
        let parent = (path as NSString).deletingLastPathComponent
        cell.show(name: (path as NSString).lastPathComponent, detail: parent.isEmpty ? nil : parent, kind: kind,
                  ignored: false, path: path, here: false)
    }

    // MARK: 测试与读屏

    /// 树上此刻可见的行（缩进按层级加两个空格）。
    var visibleRows: [String] {
        (0..<outline.numberOfRows).compactMap { r in
            guard let n = outline.item(atRow: r) as? FileNode else { return nil }
            return String(repeating: "  ", count: outline.level(forRow: r)) + n.name
        }
    }

    var resultRows: [String] {
        found.map {
            switch $0 {
            case .item(let p, _): p
            case .group(let p): "[\(p)]"
            case .hit(_, let n, let t): "\(n): \(t)"
            case .note(let t): t
            }
        }
    }

    /// 内容搜索结果行要显示的文字与其中匹配的范围：匹配按后台同样的规则找（字面、不分大小写）；匹配前超过 12 个字时从匹配前
    /// 10 个字起，前面以「…」开头。找不到（大小写规则不同的少数字符）时原样显示。
    static func hitSnippet(_ text: String, query: String) -> (String, NSRange?) {
        let ns = text as NSString
        let r = query.isEmpty ? NSRange(location: NSNotFound, length: 0) : ns.range(of: query, options: [.caseInsensitive])
        guard r.location != NSNotFound else { return (text, nil) }
        guard r.location > 12 else { return (text, r) }
        let from = ns.rangeOfComposedCharacterSequence(at: r.location - 10).location
        return ("…" + ns.substring(from: from), NSRange(location: r.location - from + 1, length: r.length))
    }

    /// 测试用：筛选结果第 `row` 行的文字在 `width` 宽里排出来有多高（只占一行时不超过行高）。
    func resultTextHeightForTesting(row: Int, width: CGFloat) -> CGFloat {
        guard let cell = tableView(results, viewFor: nil, row: row) as? NSTableCellView, let c = cell.textField?.cell else { return 0 }
        return c.cellSize(forBounds: NSRect(x: 0, y: 0, width: width, height: 1000)).height
    }

    func clickResultForTesting(_ row: Int) { openResult(row) }
    /// 测试用：树里选中的那一行的路径。
    var selectedTreePathForTesting: String? { (outline.item(atRow: outline.selectedRow) as? FileNode)?.path }
    func clearFilterForTesting() {
        filter.stringValue = ""
        filterChanged(nil)
    }

    func setFilterForTesting(_ q: String) {
        filter.stringValue = q
        find(q)
    }
}

/// 树与筛选结果的一行：图标、名字、灰色目录（筛选结果）、终端所在的标记。
private final class FileCell: NSTableCellView {
    static let id = NSUserInterfaceItemIdentifier("file")
    private let icon = NSImageView()
    private let label = NSTextField(labelWithString: "")
    private let detail = NSTextField(labelWithString: "")
    private let hereMark = TerminalHereMark.make()
    private let mark = NSTextField(labelWithString: "")

    init() {
        super.init(frame: .zero)
        identifier = Self.id
        icon.symbolConfiguration = .init(pointSize: 12, weight: .regular)
        label.font = ScyllaUIStyle.detailFont
        label.lineBreakMode = .byTruncatingMiddle
        label.setContentCompressionResistancePriority(.defaultLow + 1, for: .horizontal)
        detail.font = ScyllaUIStyle.mono(11)
        detail.textColor = .tertiaryLabelColor
        detail.lineBreakMode = .byTruncatingHead
        detail.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        mark.font = ScyllaUIStyle.mono(11, semibold: true)
        mark.alignment = .center
        for v in [icon, label, detail, hereMark, mark] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            addSubview(v)
        }
        textField = label
        imageView = icon
        NSLayoutConstraint.activate([
            icon.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 2),
            icon.centerYAnchor.constraint(equalTo: centerYAnchor),
            icon.widthAnchor.constraint(equalToConstant: 16),
            label.leadingAnchor.constraint(equalTo: icon.trailingAnchor, constant: 5),
            label.centerYAnchor.constraint(equalTo: centerYAnchor),
            detail.leadingAnchor.constraint(equalTo: label.trailingAnchor, constant: 6),
            detail.firstBaselineAnchor.constraint(equalTo: label.firstBaselineAnchor),
            hereMark.leadingAnchor.constraint(greaterThanOrEqualTo: detail.trailingAnchor, constant: 4),
            hereMark.trailingAnchor.constraint(equalTo: mark.leadingAnchor, constant: -2),
            hereMark.centerYAnchor.constraint(equalTo: centerYAnchor),
            mark.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
            mark.widthAnchor.constraint(equalToConstant: 14),
            mark.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    /// 改动标记：文件右边的 M、A（颜色同 Diff）；收起的文件夹里有改动时打一个点。
    func setMark(_ status: String?, dirty: Bool) {
        if let status {
            mark.stringValue = status
            mark.textColor = DiffViewer.statusColor(status)
            mark.toolTip = status == "A" ? String(localized: "新增") : String(localized: "已修改")
        } else {
            mark.stringValue = dirty ? "•" : ""
            mark.textColor = ScyllaUIStyle.waiting
            mark.toolTip = dirty ? String(localized: "里面有更改") : nil
        }
    }

    /// 内容搜索命中的一行：行号（三级文字色）与那一行，缩进到文件名下。从匹配处附近显示（匹配前留约 10 个字，前面省掉的以「…」
    /// 开头），匹配的字用正文色加粗（2026-11-25 前显示行首，栏宽只放得下二十来个字，看不到匹配处）。
    func showHit(line: Int, text: String, query: String) {
        icon.image = nil
        icon.isHidden = false
        detail.isHidden = true
        hereMark.isHidden = true
        // 只占一行、放不下在末尾截断：带属性的文字不设段落样式时按折行排，长行会折成两行、第二行只露出半截字（2026-11-25 测试版）。
        let oneLine = NSMutableParagraphStyle()
        oneLine.lineBreakMode = .byTruncatingTail
        let s = NSMutableAttributedString(string: "\(line)  ", attributes: [.font: ScyllaUIStyle.mono(11), .foregroundColor: NSColor.tertiaryLabelColor])
        let (shown, match) = FilesPanel.hitSnippet(text.trimmingCharacters(in: .whitespaces), query: query)
        let body = NSMutableAttributedString(string: shown, attributes: [.font: ScyllaUIStyle.mono(12), .foregroundColor: NSColor.secondaryLabelColor])
        if let match {
            body.addAttributes([.font: ScyllaUIStyle.mono(12, semibold: true), .foregroundColor: NSColor.labelColor], range: match)
        }
        s.append(body)
        s.addAttribute(.paragraphStyle, value: oneLine, range: NSRange(location: 0, length: s.length))
        label.attributedStringValue = s
        label.toolTip = text
    }

    func show(name: String, detail text: String?, kind: FileNode.Kind, ignored: Bool, path: String, here: Bool) {
        label.stringValue = name
        label.toolTip = kind == .note ? nil : name
        detail.stringValue = text ?? ""
        detail.isHidden = text == nil
        hereMark.isHidden = !here
        let symbol = FileIcon.symbol(kind: kind, path: path)
        icon.image = symbol.flatMap { NSImage(systemSymbolName: $0, accessibilityDescription: nil) }
        icon.isHidden = symbol == nil
        let muted = ignored || kind == .note
        label.textColor = muted ? .tertiaryLabelColor : .labelColor
        icon.contentTintColor = muted ? .tertiaryLabelColor : .secondaryLabelColor
    }
}

/// 终端所在目录的标记（文件树与 Diff 的改动列表共用，D3）：`terminal` 符号、三级文字色。
enum TerminalHereMark {
    static func make() -> NSImageView {
        let v = NSImageView()
        v.image = NSImage(systemSymbolName: "terminal", accessibilityDescription: String(localized: "终端所在目录"))
        v.symbolConfiguration = .init(pointSize: 10, weight: .regular)
        v.contentTintColor = .tertiaryLabelColor
        v.toolTip = String(localized: "终端所在目录")
        return v
    }
}

/// 文件树与筛选结果的图标（docs/design.md 5.12 节「图标」）：系统符号、单色，按扩展名查表，只在这一处。
enum FileIcon {
    static let text: Set<String> = ["md", "markdown", "mdx", "txt", "text", "log", "rst", "adoc", "org", "tex", "rtf"]
    static let code: Set<String> = [
        "swift", "rs", "py", "js", "mjs", "cjs", "jsx", "ts", "tsx", "sh", "bash", "zsh", "fish", "c", "h", "cc", "cpp", "hpp",
        "m", "mm", "go", "java", "kt", "kts", "rb", "php", "html", "htm", "css", "scss", "less", "vue", "svelte", "lua", "pl",
        "sql", "zig", "cs", "scala", "dart", "r", "ex", "exs", "hs", "ml", "clj", "vim", "ps1", "bat", "cmd",
    ]
    static let config: Set<String> = [
        "json", "jsonl", "yaml", "yml", "toml", "plist", "xml", "csv", "tsv", "ini", "conf", "cfg", "lock", "env",
        "properties", "entitlements", "xcstrings", "strings",
        // 点文件（文件名以点开头、没有扩展名，按去掉点的名字查）
        "gitignore", "gitattributes", "gitmodules", "gitconfig", "npmrc", "nvmrc", "yarnrc", "editorconfig", "prettierrc",
        "eslintrc", "dockerignore", "zshrc", "zshenv", "zprofile", "bashrc", "bash_profile", "profile", "vimrc", "inputrc",
    ]

    /// 说明行没有图标（nil）。
    static func symbol(kind: FileNode.Kind, path: String) -> String? {
        switch kind {
        case .dir: return "folder"
        case .link: return "arrow.turn.up.right"
        case .note: return nil
        case .file: break
        }
        var ext = (path as NSString).pathExtension.lowercased()
        let name = (path as NSString).lastPathComponent
        // `.env`、`.gitignore` 这类点文件没有扩展名：用去掉点之后的名字查表（2026-11-24 复查）。
        if ext.isEmpty, name.hasPrefix("."), name.count > 1 { ext = String(name.dropFirst()).lowercased() }
        if ext.isEmpty { return "doc" }
        if text.contains(ext) { return "doc.text" }
        if code.contains(ext) { return "chevron.left.forwardslash.chevron.right" }
        if config.contains(ext) { return "curlybraces" }
        if ext == "pdf" { return "doc.richtext" }
        if let t = UTType(filenameExtension: ext), t.conforms(to: .image) { return "photo" }
        return "doc"
    }
}
