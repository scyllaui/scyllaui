// 右侧栏「Diff」（docs/design.md 5.12 节）：HEAD 对工作树的改动列表在右（按文件夹分组成树，D3），选中文件的逐行 diff 在左；
// 列表可隐藏、可按路径筛选。
// 列表来自 ChangesStore（与文件树的 M、A 标记共用），单个文件的 diff 经 `request` 取（`git.diff`）。
// 栏头横跨整栏，放大与关闭只在栏头右上角（Diff 改版 D1，同文件栏 F1）；改动与列表之间的分隔线、收起与滑动同文件栏 F3（D2，`ListSplit`）。

import AppKit

/// 改动列表的一项（D3）：文件夹（只套着一个子文件夹、自己没有文件的一串合并成一行）、文件，或最后的「还有 N 项未列出」。
final class ChangeNode {
    enum Kind { case dir, file, more }
    let kind: Kind
    /// 文件夹：合并后的完整相对路径（如 `a/b/c`）；文件：`item.path`。收起状态与选中都按它认。
    let path: String
    /// 显示的名字：合并的一串写全（`a/b/c`），文件写文件名。
    let name: String
    /// 合并进这一行的各级文件夹（相对路径，由外到里），标终端所在目录用。
    let chain: [String]
    let item: ChangesStore.Item?
    let more: Int
    private(set) var children: [ChangeNode] = []
    private(set) weak var parent: ChangeNode?
    /// 文件夹里全部改动的合计；`counted` 为 false 表示里面没有一个数得了行数（全是二进制或没数行数的新文件）。
    private(set) var added = 0
    private(set) var deleted = 0
    private(set) var counted = false

    private init(kind: Kind, path: String, name: String, chain: [String] = [], item: ChangesStore.Item? = nil, more: Int = 0) {
        self.kind = kind
        self.path = path
        self.name = name
        self.chain = chain
        self.item = item
        self.more = more
    }

    /// 由改动的路径搭树（顶层各项）：文件夹在前，按 `localizedStandardCompare` 排（同文件树）；`more` 大于 0 时最后加一行说明。
    static func build(_ items: [ChangesStore.Item], more: Int = 0) -> [ChangeNode] {
        final class Dir {
            var dirs: [String: Dir] = [:]
            var files: [ChangesStore.Item] = []
        }
        let top = Dir()
        for it in items {
            var d = top
            for part in it.path.split(separator: "/").dropLast().map(String.init) {
                let next = d.dirs[part] ?? Dir()
                d.dirs[part] = next
                d = next
            }
            d.files.append(it)
        }
        func convert(_ d: Dir, prefix: String, parent: ChangeNode?) -> [ChangeNode] {
            var dirs: [ChangeNode] = []
            for (name, sub) in d.dirs {
                var path = prefix.isEmpty ? name : prefix + "/" + name
                var display = name
                var chain = [path]
                var cur = sub
                while cur.files.isEmpty, cur.dirs.count == 1, let (n, next) = cur.dirs.first {
                    path += "/" + n
                    display += "/" + n
                    chain.append(path)
                    cur = next
                }
                let node = ChangeNode(kind: .dir, path: path, name: display, chain: chain)
                node.parent = parent
                node.children = convert(cur, prefix: path, parent: node)
                for c in node.children {
                    if c.kind == .dir, c.counted {
                        node.added += c.added
                        node.deleted += c.deleted
                        node.counted = true
                    } else if let it = c.item, !it.binary, let a = it.added, let dl = it.deleted {
                        node.added += a
                        node.deleted += dl
                        node.counted = true
                    }
                }
                dirs.append(node)
            }
            let files = d.files.map { it -> ChangeNode in
                let n = ChangeNode(kind: .file, path: it.path, name: (it.path as NSString).lastPathComponent, item: it)
                n.parent = parent
                return n
            }
            let byName = { (a: ChangeNode, b: ChangeNode) in a.name.localizedStandardCompare(b.name) == .orderedAscending }
            return dirs.sorted(by: byName) + files.sorted(by: byName)
        }
        var out = convert(top, prefix: "", parent: nil)
        if more > 0 { out.append(ChangeNode(kind: .more, path: "\u{0}more", name: "", more: more)) }
        return out
    }

    /// 按列表顺序（文件夹在前、深度优先）的全部文件。
    static func files(_ nodes: [ChangeNode]) -> [ChangeNode] {
        nodes.flatMap { $0.kind == .file ? [$0] : files($0.children) }
    }

    /// 打开时选哪个：终端当前目录（相对根，`""` 为根）里（含子目录）有改动就选其中第一个，没有就选第一个文件。
    static func initialFile(_ nodes: [ChangeNode], cwd: String?) -> ChangeNode? {
        let all = files(nodes)
        if let cwd, !cwd.isEmpty, let hit = all.first(where: { $0.path.hasPrefix(cwd + "/") }) { return hit }
        return all.first
    }
}

final class DiffPanel: NSView, NSOutlineViewDataSource, NSOutlineViewDelegate, NSSearchFieldDelegate {
    var request: (([String: Any], @escaping ([String: Any]?) -> Void) -> Void)?
    /// 关 Diff 栏（栏头的关闭在没有打开的改动时）。
    var onClose: (() -> Void)?
    var onLayoutChange: (() -> Void)?
    var onZoom: (() -> Void)?
    var onEscape: (() -> Bool)? {
        didSet { viewer.onEscape = onEscape }
    }
    /// 在「文件」里打开（绝对路径）。
    var onOpenInFiles: ((String) -> Void)?

    let viewer = DiffViewer()
    /// 查看器放大着（工作区设）：栏头的放大按钮换成缩回。
    var zoomed = false {
        didSet { updateHeader() }
    }
    /// 打开改动后改动列表的宽度，与文件树宽同一个值（`Prefs.filesTreeWidth`，D2）。
    var listWidth: CGFloat {
        get { split.width }
        set { split.width = newValue }
    }
    /// 拖分隔线改了列表宽（松手、双击时）：工作区记下来。
    var onListWidth: ((CGFloat) -> Void)?
    /// 栏的底色（同右侧栏宿主）：收起、展开的过渡里列表要盖住改动，得有底色。
    var fillColor: NSColor {
        get { split.fillColor }
        set { split.fillColor = newValue }
    }
    private(set) var root: String?
    private var snapshot: ChangesStore.Snapshot?
    /// 此刻列表里的树（筛过时是筛出来的项搭的），与按路径找文件一行。
    private var nodes: [ChangeNode] = []
    private var fileNodes: [String: ChangeNode] = [:]
    /// 筛出来（没筛时为全部）的文件数。
    private var shownCount = 0
    /// 每个项目根收起了哪些文件夹（按合并后的路径，只在内存里）；默认全部展开。
    private var collapsedDirs: [String: Set<String>] = [:]
    /// 终端当前目录（相对根，`""` 为根；不在根下为 nil）：标出所在文件夹，打开时优先选它里面的改动。
    private var cwd: String?
    /// 重载、按记下的状态展开时不记展开收起、不当作用户选中。
    private var restoring = false
    private(set) var selected: ChangesStore.Item?
    private var listHiddenFlag = false
    /// 改动区收起、只剩列表（改动仍打开着）：右侧栏左边缘把改动区拖到 160 以下（D2）。
    private var collapsedFlag = false
    private let readLatest = Latest()

    private let column = NSView()
    private let header = RightPanelHeader(symbol: "plus.forwardslash.minus", title: String(localized: "更改"), closeName: String(localized: "关闭更改"))
    private var zoomButton: NSButton?
    private var expandButton: NSButton?
    private let filter = NSSearchField()
    private let table = NSOutlineView()
    private let scroll = NSScrollView()
    private let note = NSTextField(wrappingLabelWithString: "")
    private let divider = NSBox()
    /// 改动与列表之间的分栏（分隔线、收起与滑动），与文件栏同一份。
    private var split: ListSplit!

    override init(frame: NSRect) {
        super.init(frame: frame)
        expandButton = header.addTool(symbol: "sidebar.left", name: String(localized: "显示更改内容"), target: self, action: #selector(expandContent(_:)))
        zoomButton = header.addTool(symbol: "arrow.up.left.and.arrow.down.right", name: String(localized: "放大"), target: self, action: #selector(zoom(_:)))
        // 打开着改动时先关改动、回到只有列表；没有打开的改动时关栏。
        header.onClose = { [weak self] in
            guard let self else { return }
            if self.viewerOpen { self.select(nil) } else { self.onClose?() }
        }
        header.subtitle.font = ScyllaUIStyle.mono(11)
        header.subtitle.textColor = .tertiaryLabelColor
        filter.placeholderString = String(localized: "按路径筛选")
        filter.font = ScyllaUIStyle.detailFont
        filter.sendsSearchStringImmediately = true
        filter.target = self
        filter.action = #selector(filterChanged(_:))
        filter.delegate = self
        let col = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("change"))
        col.resizingMask = .autoresizingMask
        table.addTableColumn(col)
        table.headerView = nil
        table.style = .plain
        table.backgroundColor = .clear
        table.rowHeight = 22
        table.intercellSpacing = NSSize(width: 0, height: 0)
        table.columnAutoresizingStyle = .uniformColumnAutoresizingStyle
        table.outlineTableColumn = col
        table.indentationPerLevel = 12
        table.autoresizesOutlineColumn = false
        table.dataSource = self
        table.delegate = self
        table.target = self
        table.action = #selector(rowClicked(_:))
        scroll.documentView = table
        scroll.drawsBackground = false
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        note.font = ScyllaUIStyle.detailFont
        note.textColor = .secondaryLabelColor
        note.alignment = .center
        note.isHidden = true
        divider.boxType = .separator
        viewer.isHidden = true
        viewer.onToggleList = { [weak self] in self?.toggleList() }
        viewer.onOpenInFiles = { [weak self] in
            guard let self, let root = self.root, let item = self.selected else { return }
            self.onOpenInFiles?((root as NSString).appendingPathComponent(item.path))
        }
        for v in [filter, scroll, note, divider] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            column.addSubview(v)
        }
        // 列表在改动之上：收起、展开的过渡里由列表盖住改动（平时两者不重叠）。分隔线的热区与蓝线由 `ListSplit` 加在最上面。
        for v in [header, viewer, column] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            addSubview(v)
        }
        split = ListSplit(host: self, viewer: viewer, column: column, divider: divider)
        split.logName = "diff list"
        split.isOpen = { [weak self] in self?.viewerOpen ?? false }
        split.listHidden = { [weak self] in self?.listHidden ?? false }
        split.setListHidden = { [weak self] on in self?.listHiddenFlag = on }
        split.collapsed = { [weak self] in self?.contentCollapsed ?? false }
        split.onLayout = { [weak self] in self?.applyLayout() }
        split.onWidth = { [weak self] w in self?.onListWidth?(w) }
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
            scroll.topAnchor.constraint(equalTo: filter.bottomAnchor, constant: 8),
            scroll.leadingAnchor.constraint(equalTo: column.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: column.trailingAnchor),
            scroll.bottomAnchor.constraint(equalTo: column.bottomAnchor),
            note.topAnchor.constraint(equalTo: filter.bottomAnchor, constant: 24),
            note.leadingAnchor.constraint(equalTo: column.leadingAnchor, constant: 16),
            note.trailingAnchor.constraint(equalTo: column.trailingAnchor, constant: -16),
            divider.topAnchor.constraint(equalTo: column.topAnchor),
            divider.bottomAnchor.constraint(equalTo: column.bottomAnchor),
            divider.leadingAnchor.constraint(equalTo: column.leadingAnchor),
            divider.widthAnchor.constraint(equalToConstant: 1),
        ])
        applyLayout()
    }

    required init?(coder: NSCoder) { fatalError() }

    var viewerOpen: Bool { selected != nil }
    var listHidden: Bool {
        get { viewerOpen && listHiddenFlag }
        set {
            listHiddenFlag = newValue
            applyLayout()
        }
    }
    /// 改动区收起、只剩列表（改动仍打开着）。列表隐藏时不收起。
    var contentCollapsed: Bool {
        get { viewerOpen && collapsedFlag }
        set {
            let wasShown = contentShown
            let viewerWidth = viewer.frame.width
            collapsedFlag = newValue
            if wasShown != contentShown, !listHidden { split.startSlide(collapsing: !contentShown, viewerWidth: viewerWidth) }
            applyLayout()
        }
    }
    /// 改动此刻显示着（打开着且没收起）：工作区按它用查看宽。
    var contentShown: Bool { viewerOpen && !contentCollapsed }

    override func viewDidChangeEffectiveAppearance() {
        super.viewDidChangeEffectiveAppearance()
        split?.applyFill()
    }

    // MARK: 数据

    /// 换根：不在仓库里（`inRepo` 为 false）直接写明，不请求。`branch` 写在栏头。
    func setRoot(_ root: String?, inRepo: Bool, branch: String?, currentDir: String? = nil) {
        let changed = root != self.root
        self.root = root
        setCwd(currentDir)
        guard let root else {
            header.subtitle.stringValue = ""
            show(note: StatusPanel.noTerminal)
            return select(nil)
        }
        header.subtitle.toolTip = root
        if !inRepo {
            snapshot = nil
            header.subtitle.stringValue = (root as NSString).abbreviatingWithTildeInPath
            show(note: String(localized: "这个目录不在 Git 仓库里，没有可对比的更改") + "\n" + (root as NSString).abbreviatingWithTildeInPath)
            return select(nil)
        }
        self.branch = branch
        if changed {
            snapshot = nil
            select(nil)
            show(note: String(localized: "正在读取更改…"))
        }
        updateSubtitle()
    }

    private var branch: String?

    private func updateSubtitle() {
        var s = String(localized: "\(branch ?? "HEAD") → 工作树")
        if let snap = snapshot, snap.error == nil, !snap.items.isEmpty {
            let t = snap.totals
            s += " · +\(t.added) −\(t.deleted)"
        }
        header.subtitle.stringValue = s
    }

    /// ChangesStore 的新结果：重排列表；选中的文件还在就重读它的 diff（保留滚动位置），不在了改选第一个；第一次有结果时选中第一个。
    func update(_ snap: ChangesStore.Snapshot?) {
        let first = snapshot == nil
        snapshot = snap
        updateSubtitle()
        guard let snap else { return }
        if let code = snap.error {
            nodes = []
            fileNodes = [:]
            table.reloadData()
            show(note: Self.failure(code))
            return select(nil)
        }
        applyFilter()
        guard !snap.items.isEmpty else {
            show(note: String(localized: "工作树与 HEAD 一致，没有更改"))
            return select(nil)
        }
        show(note: nil)
        if let cur = selected, let now = snap.items.first(where: { $0.path == cur.path }) {
            select(now, keepScroll: true, expand: false)
        } else if first || selected != nil {
            // 打开时（或选中的文件没了）：终端目录里的第一个改动，没有就第一个文件；展开到它并滚到可见。
            let pick = ChangeNode.initialFile(nodes, cwd: cwd)?.item ?? snap.items.first
            select(pick, expand: false)
            if let pick { reveal(pick.path) }
        }
    }

    /// 终端当前目录变了：换标记（不改选中）。
    private func setCwd(_ dir: String?) {
        var rel: String?
        if let dir, let root {
            let r = FilesPanel.realPath(root)
            let d = FilesPanel.realPath(dir)
            rel = d == r ? "" : (d.hasPrefix(r + "/") ? String(d.dropFirst(r.count + 1)) : nil)
        }
        guard rel != cwd else { return }
        cwd = rel
        for row in 0..<table.numberOfRows {
            guard let node = table.item(atRow: row) as? ChangeNode, node.kind == .dir else { continue }
            table.reloadData(forRowIndexes: [row], columnIndexes: [0])
        }
    }

    static func failure(_ code: String) -> String {
        switch code {
        case "no_git": String(localized: "看更改需要 Xcode 命令行工具")
        case "not_repo": String(localized: "这个目录不在 Git 仓库里，没有可对比的更改")
        case "unsafe_filter": String(localized: "这个仓库配置了 ScyllaUI 不执行的过滤器，不读更改")
        case "timeout": String(localized: "读更改超时（超过 3 秒），文件再有变化时重试")
        default: String(localized: "暂时读不到更改")
        }
    }

    private func show(note text: String?) {
        note.stringValue = text ?? ""
        note.isHidden = text == nil
        scroll.isHidden = text != nil
    }

    // MARK: 选中与 diff

    /// `expand`：改动区收起着时展开它（在列表里点文件）；自动刷新时为 false，保持原样。关掉改动时收起状态一并清掉。
    func select(_ item: ChangesStore.Item?, keepScroll: Bool = false, expand: Bool = true) {
        let wasShown = contentShown
        let wasOpen = viewerOpen
        if item == nil || expand { collapsedFlag = false }
        selected = item
        restoring = true
        if let item, let node = fileNodes[item.path], case let row = table.row(forItem: node), row >= 0 {
            table.selectRowIndexes([row], byExtendingSelection: false)
            if !keepScroll { table.scrollRowToVisible(row) }
        } else {
            table.deselectAll(nil)
        }
        restoring = false
        if let item, let root {
            viewer.begin(item: item, root: root)
            load(item, keepScroll: keepScroll)
        } else {
            viewer.clear()
        }
        // 改动区的显隐由分栏按状态定（收起时不显示）。
        if wasOpen != viewerOpen || wasShown != contentShown { applyLayout() }
    }

    private func load(_ item: ChangesStore.Item, keepScroll: Bool) {
        guard let root else { return }
        readLatest.submit { [weak self] done in
            guard let self, let request = self.request else { return done() }
            let start = CACurrentMediaTime()
            request(["type": "git.diff", "root": root, "path": item.path, "untracked": item.untracked]) { [weak self] reply in
                defer { done() }
                // 从发请求到应答回到主线程（含后台取 diff 与解析、传输、App 解 JSON）；正文的搭建另记 `diff render`。
                scyllauiLog.notice("diff fetch ms=\(Int((CACurrentMediaTime() - start) * 1000))")
                guard let self, self.selected?.path == item.path, self.root == root else { return }
                self.viewer.show(reply: reply, keepScroll: keepScroll)
            }
        }
    }

    private func toggleList() {
        guard viewerOpen else { return }
        listHidden.toggle()
    }

    private func applyLayout() {
        split.applyConstraints()
        viewer.listHidden = listHidden
        updateHeader()
        onLayoutChange?()
    }

    @objc private func expandContent(_ sender: Any?) { contentCollapsed = false }

    /// 点列表里的文件：改动区收起着就展开（点的是已选中的文件时选中不变，`tableViewSelectionDidChange` 不会来）。
    @objc private func rowClicked(_ sender: Any?) { click(table.clickedRow) }

    /// 点一行：文件夹只展开 / 收起，不打开改动；文件在改动区收起着时展开它。
    private func click(_ row: Int) {
        guard row >= 0, let node = table.item(atRow: row) as? ChangeNode else { return }
        switch node.kind {
        case .dir: table.isItemExpanded(node) ? table.collapseItem(node) : table.expandItem(node)
        case .file: if contentCollapsed { contentCollapsed = false }
        case .more: break
        }
    }

    /// 栏头右上角：放大只在改动显示着时有，「显示改动内容」只在收起时有；关闭按钮的功能名随之换。
    private func updateHeader() {
        expandButton?.isHidden = !contentCollapsed
        zoomButton?.isHidden = !contentShown
        zoomButton?.image = NSImage(systemSymbolName: zoomed ? "arrow.down.right.and.arrow.up.left" : "arrow.up.left.and.arrow.down.right", accessibilityDescription: nil)
        zoomButton?.toolTip = zoomed ? String(localized: "缩回") : String(localized: "放大")
        zoomButton?.setAccessibilityLabel(zoomButton?.toolTip)
        header.closeName = viewerOpen ? String(localized: "关闭这个文件的更改") : String(localized: "关闭更改")
    }

    @objc private func zoom(_ sender: Any?) { onZoom?() }

    // MARK: 筛选与列表

    @objc private func filterChanged(_ sender: Any?) { applyFilter() }

    /// 按筛选重新搭树并重载：保留滚动位置与选中；没筛时按记下的收起状态展开，筛过时全部展开。
    private func applyFilter() {
        let q = filter.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        let all = snapshot?.items ?? []
        let items = q.isEmpty ? all : all.filter { $0.path.localizedCaseInsensitiveContains(q) }
        shownCount = items.count
        nodes = ChangeNode.build(items, more: snapshot?.more ?? 0)
        fileNodes = Dictionary(ChangeNode.files(nodes).map { ($0.path, $0) }, uniquingKeysWith: { a, _ in a })
        let origin = scroll.contentView.bounds.origin
        restoring = true
        table.reloadData()
        expand(nodes, all: !q.isEmpty)
        if let cur = selected, let node = fileNodes[cur.path], case let row = table.row(forItem: node), row >= 0 {
            table.selectRowIndexes([row], byExtendingSelection: false)
        }
        restoring = false
        scroll.contentView.scroll(to: origin)
        scroll.reflectScrolledClipView(scroll.contentView)
        header.count.stringValue = all.isEmpty ? "" : (q.isEmpty ? "\(all.count)" : "\(items.count) / \(all.count)")
    }

    private var filtering: Bool { !filter.stringValue.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
    private var collapsedHere: Set<String> { root.flatMap { collapsedDirs[$0] } ?? [] }

    /// 展开文件夹：`all` 为全部，否则跳过记着收起的（它下面的照样按记下的展开，等它展开时就是原样）。
    private func expand(_ list: [ChangeNode], all: Bool) {
        let collapsed = collapsedHere
        for n in list where n.kind == .dir {
            if all || !collapsed.contains(n.path) { table.expandItem(n) }
            if table.isItemExpanded(n) { expand(n.children, all: all) }
        }
    }

    /// 展开到这个文件（打开时选中的）并滚到它；展开的文件夹不再记为收起。
    private func reveal(_ path: String) {
        guard let node = fileNodes[path] else { return }
        var chain: [ChangeNode] = []
        var p = node.parent
        while let n = p { chain.insert(n, at: 0); p = n.parent }
        for n in chain where !table.isItemExpanded(n) { table.expandItem(n) }
        let row = table.row(forItem: node)
        guard row >= 0 else { return }
        restoring = true
        table.selectRowIndexes([row], byExtendingSelection: false)
        restoring = false
        table.scrollRowToVisible(row)
    }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
        guard selector == #selector(NSResponder.cancelOperation(_:)) else { return false }
        filter.stringValue = ""
        applyFilter()
        return true
    }

    func outlineView(_ outlineView: NSOutlineView, numberOfChildrenOfItem item: Any?) -> Int {
        (item as? ChangeNode)?.children.count ?? nodes.count
    }

    func outlineView(_ outlineView: NSOutlineView, child index: Int, ofItem item: Any?) -> Any {
        (item as? ChangeNode)?.children[index] ?? nodes[index]
    }

    func outlineView(_ outlineView: NSOutlineView, isItemExpandable item: Any) -> Bool {
        (item as? ChangeNode)?.kind == .dir
    }

    func outlineView(_ outlineView: NSOutlineView, viewFor tableColumn: NSTableColumn?, item: Any) -> NSView? {
        guard let node = item as? ChangeNode else { return nil }
        let cell = outlineView.makeView(withIdentifier: ChangeCell.id, owner: nil) as? ChangeCell ?? ChangeCell()
        switch node.kind {
        case .dir: cell.show(dir: node, here: cwd.map { node.chain.contains($0) } ?? false)
        case .file: node.item.map { cell.show($0) }
        case .more: cell.show(more: node.more)
        }
        return cell
    }

    /// 只有文件能选中：方向键上下在文件之间移动，点文件夹一行不打开改动。
    func outlineView(_ outlineView: NSOutlineView, shouldSelectItem item: Any) -> Bool {
        (item as? ChangeNode)?.kind == .file
    }

    func outlineViewSelectionDidChange(_ notification: Notification) {
        guard !restoring else { return }
        guard table.selectedRow >= 0 else {
            // 点了列表下方的空白处：改动仍打开着，列表里的选中跟着它，不清掉（D3 自查时发现，原来的表格也是这样）。
            if let cur = selected, let node = fileNodes[cur.path], case let row = table.row(forItem: node), row >= 0 {
                restoring = true
                table.selectRowIndexes([row], byExtendingSelection: false)
                restoring = false
            }
            return
        }
        guard let node = table.item(atRow: table.selectedRow) as? ChangeNode, let item = node.item, item.path != selected?.path else { return }
        select(item)
    }

    /// 展开时把下面记着展开的子文件夹也展开（同文件树）；没筛时记下展开、收起。
    func outlineViewItemDidExpand(_ notification: Notification) {
        guard !restoring, let node = notification.userInfo?["NSObject"] as? ChangeNode else { return }
        if !filtering, let root { collapsedDirs[root, default: []].remove(node.path) }
        restoring = true
        expand(node.children, all: filtering)
        restoring = false
    }

    func outlineViewItemDidCollapse(_ notification: Notification) {
        guard !restoring, !filtering, let node = notification.userInfo?["NSObject"] as? ChangeNode, let root else { return }
        collapsedDirs[root, default: []].insert(node.path)
    }

    // MARK: 测试与读屏

    /// 列表里看得见的行：文件夹写「名字/」与合计，文件写状态与文件名，按层级缩进两格。
    var listRows: [String] {
        (0..<table.numberOfRows).compactMap { row -> String? in
            guard let n = table.item(atRow: row) as? ChangeNode else { return nil }
            let pad = String(repeating: "  ", count: table.level(forRow: row))
            switch n.kind {
            case .dir: return pad + n.name + "/" + (n.counted ? " +\(n.added) −\(n.deleted)" : "") + (cwd.map { n.chain.contains($0) } == true ? " [终端]" : "")
            case .file: return pad + (n.item?.status ?? "") + " " + n.name
            case .more: return pad + "…\(n.more)"
            }
        }
    }
    func setFilterForTesting(_ q: String) {
        filter.stringValue = q
        applyFilter()
    }
    func clickEmptyListForTesting() { table.deselectAll(nil) }
    var selectedListPath: String? { (table.item(atRow: table.selectedRow) as? ChangeNode)?.path }
    func clickListRowForTesting(_ name: String) {
        guard let row = (0..<table.numberOfRows).first(where: { (table.item(atRow: $0) as? ChangeNode)?.name == name }) else { return }
        click(row)
    }
    var noteText: String? { note.isHidden ? nil : note.stringValue }
    var subtitleText: String { header.subtitle.stringValue }
    var closeName: String { header.closeName }
    var titleText: String { header.title.stringValue }
    var zoomShown: Bool { zoomButton?.isHidden == false }
    var expandShown: Bool { expandButton?.isHidden == false }
    func closeFromHeaderForTesting() { header.onClose?() }
    /// 测试用：模拟拖分隔线（`dx` 为本次移动，往右为正）、点列表的第 `row` 行、列表此刻的宽度。
    func dragSplitForTesting(_ dx: CGFloat, done: Bool) { split.drag(dx, done: done) }
    /// 测试用：点第 `row` 个文件（按列表顺序）。
    func clickRowForTesting(_ row: Int) {
        guard let node = ChangeNode.files(nodes).dropFirst(row).first else { return }
        let r = table.row(forItem: node)
        table.selectRowIndexes([r], byExtendingSelection: false)
        click(r)
    }
    var listColumnWidthForTesting: CGFloat { column.frame.width }
}

/// 改动列表的一行（D3）：文件是状态字母、文件名、`+N −M` 或「二进制」「新文件」；文件夹是 `folder` 图标、名字、终端标记与合计。
private final class ChangeCell: NSTableCellView {
    static let id = NSUserInterfaceItemIdentifier("change")
    private let status = NSTextField(labelWithString: "")
    private let icon = NSImageView()
    private let name = NSTextField(labelWithString: "")
    private let hereMark = TerminalHereMark.make()
    private let counts = NSTextField(labelWithString: "")

    init() {
        super.init(frame: .zero)
        identifier = Self.id
        status.font = ScyllaUIStyle.mono(12, semibold: true)
        status.alignment = .center
        icon.image = NSImage(systemSymbolName: "folder", accessibilityDescription: nil)
        icon.symbolConfiguration = .init(pointSize: 12, weight: .regular)
        icon.contentTintColor = .secondaryLabelColor
        name.font = ScyllaUIStyle.detailFont
        name.lineBreakMode = .byTruncatingMiddle
        name.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        counts.font = ScyllaUIStyle.mono(11)
        counts.setContentCompressionResistancePriority(.required, for: .horizontal)
        for v in [status, icon, name, hereMark, counts] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            addSubview(v)
        }
        textField = name
        NSLayoutConstraint.activate([
            status.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 2),
            status.widthAnchor.constraint(equalToConstant: 16),
            status.centerYAnchor.constraint(equalTo: centerYAnchor),
            icon.centerXAnchor.constraint(equalTo: status.centerXAnchor),
            icon.centerYAnchor.constraint(equalTo: centerYAnchor),
            name.leadingAnchor.constraint(equalTo: status.trailingAnchor, constant: 5),
            name.centerYAnchor.constraint(equalTo: centerYAnchor),
            hereMark.leadingAnchor.constraint(greaterThanOrEqualTo: name.trailingAnchor, constant: 4),
            hereMark.trailingAnchor.constraint(equalTo: counts.leadingAnchor, constant: -6),
            hereMark.centerYAnchor.constraint(equalTo: centerYAnchor),
            counts.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -10),
            counts.firstBaselineAnchor.constraint(equalTo: name.firstBaselineAnchor),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    func show(_ item: ChangesStore.Item) {
        status.stringValue = item.status
        status.textColor = DiffViewer.statusColor(item.status)
        status.isHidden = false
        icon.isHidden = true
        hereMark.isHidden = true
        name.stringValue = (item.path as NSString).lastPathComponent
        name.textColor = .labelColor
        name.toolTip = item.path
        counts.attributedStringValue = DiffViewer.countsText(item)
    }

    func show(dir node: ChangeNode, here: Bool) {
        status.isHidden = true
        icon.isHidden = false
        hereMark.isHidden = !here
        name.stringValue = node.name
        name.textColor = .labelColor
        name.toolTip = node.path
        counts.attributedStringValue = node.counted ? DiffViewer.countsText(added: node.added, deleted: node.deleted) : NSAttributedString()
    }

    func show(more: Int) {
        status.isHidden = true
        icon.isHidden = true
        hereMark.isHidden = true
        name.stringValue = String(localized: "还有 \(more) 项未列出 · 用筛选查找")
        name.textColor = .tertiaryLabelColor
        name.toolTip = nil
        counts.stringValue = ""
    }
}

/// 一个文件的逐行 diff：标题一行（文件名、次要颜色的目录与元信息，右边是折行、上一处 / 下一处改动、在「文件」里打开、隐藏 / 显示列表、
/// 「…」菜单），正文是只读文本视图（D4，与文件查看器同一种）：旧、新行号两列，整行着色，每段的 `@@` 头单独一行。
final class DiffViewer: NSView {
    var onToggleList: (() -> Void)?
    var onOpenInFiles: (() -> Void)?
    var onEscape: (() -> Bool)?
    var listHidden = false {
        didSet { updateButtons() }
    }

    enum Row: Equatable {
        case hunk(String)
        case line(sign: Character, old: Int?, new: Int?, text: String)
    }

    /// 搭好的正文：带样式的文字、每行的种类（`+`、`-`、空格、`@` 为段头）与旧、新行号（0 为没有）、每行行首、每处改动的第一行。
    struct Body {
        let rows: [Row]
        let text: NSAttributedString
        let kinds: [UInt8]
        let olds: [Int32]
        let news: [Int32]
        let lineStarts: [Int]
        let changes: [Int]
        let digits: Int
        let width: CGFloat
        let bytes: Int
    }

    private(set) var rows: [Row] = []
    private(set) var item: ChangesStore.Item?
    /// 这次的 diff 被截在 2 MB：标题里写明。
    private var truncated = false
    private let name = NSTextField(labelWithString: "")
    private let meta = NSTextField(labelWithString: "")
    private let tools = NSStackView()
    private lazy var wrapButton = RightPanelHeader.wrapButton(target: self, action: #selector(toggleWrap(_:)))
    private lazy var prevButton = RightPanelHeader.iconButton(symbol: "chevron.up", name: String(localized: "上一处更改"), target: self, action: #selector(prevChange(_:)))
    private lazy var nextButton = RightPanelHeader.iconButton(symbol: "chevron.down", name: String(localized: "下一处更改"), target: self, action: #selector(nextChange(_:)))
    private lazy var filesButton = RightPanelHeader.iconButton(symbol: "folder", name: String(localized: "在「文件」里打开"), target: self, action: #selector(openInFiles(_:)))
    private lazy var listButton = RightPanelHeader.iconButton(symbol: "sidebar.right", name: String(localized: "隐藏更改列表"), target: self, action: #selector(toggleList(_:)))
    private lazy var moreButton = RightPanelHeader.iconButton(symbol: "ellipsis", name: String(localized: "更多"), target: self, action: #selector(showMore(_:)))
    /// 「…」菜单里的拷贝相对路径、打开方式、在访达中显示（与文件查看器同一份）。
    private let actions = FileActions()
    private let headerLine = NSBox()
    private let scroll = NSScrollView()
    private let textView: DiffTextView
    private let ruler: LineNumberRuler
    private let note = NSTextField(wrappingLabelWithString: "")
    /// 滚到末尾的补救，与文件查看器同一套。
    private let follower = EndFollower()
    /// 滚动条停止滚动后续显约 3 秒（同文件查看器，见 `ScrollerLinger`）。
    private lazy var linger = ScrollerLinger(scroll)
    /// 非 UTF-8 的文件：后台把解不了的字节换成了 U+FFFD，应答里没有标记（协议不改），正文里一律着提醒色（同文件查看器）。
    private lazy var marker: UndecodableMarker = {
        let m = UndecodableMarker(textView)
        m.enabled = true
        return m
    }()
    private var body: Body?
    private var wrap = false
    /// 用户点折行按钮选的（只在内存里，换文件沿用；实现方案第 6 版 R1，与文件查看器各记各的）。
    private var wrapChoice = false
    /// 搭正文的代数：换文件、新的应答都加一，回来的结果代数不符就丢掉。
    private var generation = 0
    private static let font = ScyllaUIStyle.mono(12)

    /// 搭带样式的文字在后台队列做（2.7 MB 的 diff 在主线程上会卡住终端打字），结果回主线程。测试换成同步。
    static var buildWork: (_ work: @escaping () -> Body, _ done: @escaping (Body) -> Void) -> Void = { work, done in
        buildQueue.async {
            let b = work()
            DispatchQueue.main.async { done(b) }
        }
    }
    private static let buildQueue = DispatchQueue(label: "dev.scyllaui.diff-build", qos: .userInitiated)

    override init(frame: NSRect) {
        let storage = NSTextStorage()
        let layout = NSLayoutManager()
        storage.addLayoutManager(layout)
        let container = NSTextContainer(size: NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude))
        layout.addTextContainer(container)
        textView = DiffTextView(frame: .zero, textContainer: container)
        ruler = LineNumberRuler(scrollView: scroll, orientation: .verticalRuler)
        super.init(frame: frame)
        name.font = ScyllaUIStyle.mono(13, semibold: true)
        name.lineBreakMode = .byTruncatingMiddle
        name.setContentCompressionResistancePriority(.defaultLow + 1, for: .horizontal)
        meta.font = ScyllaUIStyle.mono(11)
        meta.textColor = .secondaryLabelColor
        meta.lineBreakMode = .byTruncatingTail
        meta.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        for b in [wrapButton, prevButton, nextButton, filesButton, listButton, moreButton] { tools.addArrangedSubview(b) }
        tools.spacing = 2
        actions.host = self
        headerLine.boxType = .separator
        // 同文件查看器的源码视图：只排看得见的部分，只读、可选中、可查找，关掉自动文本检查。
        textView.layoutManager?.allowsNonContiguousLayout = true
        textView.isEditable = false
        textView.isSelectable = true
        textView.isRichText = false
        textView.isContinuousSpellCheckingEnabled = false
        textView.isGrammarCheckingEnabled = false
        textView.isAutomaticDataDetectionEnabled = false
        textView.isAutomaticLinkDetectionEnabled = false
        textView.isAutomaticTextCompletionEnabled = false
        textView.enabledTextCheckingTypes = 0
        textView.usesFindBar = true
        textView.isIncrementalSearchingEnabled = true
        textView.drawsBackground = false
        textView.font = Self.font
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
        ruler.label = { [weak self] i in self?.lineLabel(i) ?? "" }
        // 行号栏跟着整行着色（同改版前的表格；方案会话验收 D4 时定）。
        ruler.rowColor = { [weak self] i in
            guard let b = self?.body, i < b.kinds.count else { return nil }
            return DiffTextView.color(b.kinds[i])
        }
        // 滚动时重画行号、更新上一处 / 下一处、到底时补到真正的末尾；当场处理（见 `EndFollower`）。
        scroll.contentView.postsBoundsChangedNotifications = true
        NotificationCenter.default.addObserver(forName: NSView.boundsDidChangeNotification, object: scroll.contentView, queue: nil) { [weak self] _ in
            guard let self else { return }
            self.ruler.needsDisplay = true
            self.follower.scrolled(self.textView, self.scroll)
            self.updateJumpButtons()
            self.marker.markVisible()
        }
        _ = linger
        textView.postsFrameChangedNotifications = true
        NotificationCenter.default.addObserver(forName: NSView.frameDidChangeNotification, object: textView, queue: nil) { [weak self] _ in
            guard let self else { return }
            self.follower.noteFrame(self.textView, self.scroll)
        }
        note.font = ScyllaUIStyle.detailFont
        note.textColor = .secondaryLabelColor
        note.alignment = .center
        note.isHidden = true
        for v in [name, meta, tools, headerLine, scroll, note] as [NSView] {
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
            // 分隔线必须定高：固有高度 5 与正文同优先级，测试版里被拉伸成整块，正文高 0、逐行改动空白（2026-11-24）。
            headerLine.heightAnchor.constraint(equalToConstant: 1),
            headerLine.leadingAnchor.constraint(equalTo: leadingAnchor),
            headerLine.trailingAnchor.constraint(equalTo: trailingAnchor),
            scroll.topAnchor.constraint(equalTo: headerLine.bottomAnchor),
            scroll.leadingAnchor.constraint(equalTo: leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: trailingAnchor),
            scroll.bottomAnchor.constraint(equalTo: bottomAnchor),
            note.centerYAnchor.constraint(equalTo: scroll.centerYAnchor),
            note.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 24),
            note.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -24),
        ])
        updateButtons()
        updateJumpButtons()
    }

    required init?(coder: NSCoder) { fatalError() }

    override var acceptsFirstResponder: Bool { true }

    func holdsFocus(_ responder: NSResponder?) -> Bool {
        guard let v = responder as? NSView else { return false }
        return v === self || v.isDescendant(of: self)
    }

    func takeFocus() { window?.makeFirstResponder(scroll.isHidden ? self : textView) }

    /// `Cmd+F`：正文的查找条（只在显示着正文时有）。
    func showFindBar() {
        guard !scroll.isHidden, body != nil else { return }
        window?.makeFirstResponder(textView)
        let item = NSMenuItem()
        item.tag = NSTextFinder.Action.showFindInterface.rawValue
        textView.performTextFinderAction(item)
    }

    override func cancelOperation(_ sender: Any?) {
        if onEscape?() != true { super.cancelOperation(sender) }
    }

    /// `Esc`（键码 53）：不论哪个子视图收到，先交给 `onEscape`。
    override func keyDown(with event: NSEvent) {
        if event.keyCode == 53, onEscape?() == true { return }
        super.keyDown(with: event)
    }

    /// 栏宽变了：不折行时正文至少和可见区一样宽（整行底色铺满）；折行时按新宽度重排。
    override func layout() {
        super.layout()
        if body != nil { fitWidth() }
    }

    static func statusColor(_ st: String) -> NSColor {
        switch st {
        case "A": ScyllaUIStyle.ready
        case "D": ScyllaUIStyle.error
        default: ScyllaUIStyle.waiting
        }
    }

    /// `+N −M`（绿、红），二进制写「二进制」，数不了行的未跟踪文件只写「新文件」。
    static func countsText(_ item: ChangesStore.Item) -> NSAttributedString {
        let font = ScyllaUIStyle.mono(11)
        if item.binary {
            return NSAttributedString(string: String(localized: "二进制"), attributes: [.font: font, .foregroundColor: NSColor.tertiaryLabelColor])
        }
        guard let a = item.added, let d = item.deleted else {
            return NSAttributedString(string: String(localized: "新文件"), attributes: [.font: font, .foregroundColor: NSColor.tertiaryLabelColor])
        }
        return countsText(added: a, deleted: d)
    }

    static func countsText(added a: Int, deleted d: Int) -> NSAttributedString {
        let font = ScyllaUIStyle.mono(11)
        let s = NSMutableAttributedString(string: "+\(a)", attributes: [.font: font, .foregroundColor: ScyllaUIStyle.ready])
        s.append(NSAttributedString(string: " −\(d)", attributes: [.font: font, .foregroundColor: ScyllaUIStyle.error]))
        return s
    }

    func begin(item: ChangesStore.Item, root: String) {
        let changed = item.path != self.item?.path
        self.item = item
        let full = (root as NSString).appendingPathComponent(item.path)
        actions.path = full
        actions.root = root
        name.stringValue = (item.path as NSString).lastPathComponent
        name.toolTip = full
        if changed { truncated = false }
        updateMeta()
        // 已删除的文件：在「文件」里打开、打开方式、在访达中显示都没有对象，置灰。
        filesButton.isEnabled = item.status != "D"
        if changed {
            // 新内容到之前正文留空：标题已换成新文件，正文不能还是上一个文件的行或说明（D2 顺手修，D4 换成文本视图后保持）。
            setBody(nil)
            note.isHidden = true
            scroll.isHidden = false
        }
    }

    /// 标题里文件名后面的一串（次要颜色，放不下就截断）：所在目录、`+N −M`、「文件已删除」「未跟踪的新文件」「改动太大，只显示前 2 MB」。
    static func metaText(_ item: ChangesStore.Item, truncated: Bool) -> String {
        var parts: [String] = []
        let parent = (item.path as NSString).deletingLastPathComponent
        if !parent.isEmpty { parts.append(parent.split(separator: "/").joined(separator: " / ")) }
        if item.binary {
            parts.append(String(localized: "二进制"))
        } else if let a = item.added, let d = item.deleted {
            parts.append("+\(a) −\(d)")
        }
        if item.status == "D" { parts.append(String(localized: "文件已删除")) }
        if item.untracked { parts.append(String(localized: "未跟踪的新文件")) }
        if truncated { parts.append(String(localized: "更改太大，只显示前 2 MB")) }
        return parts.joined(separator: " · ")
    }

    private func updateMeta() {
        meta.stringValue = item.map { Self.metaText($0, truncated: truncated) } ?? ""
        meta.toolTip = meta.stringValue
    }

    /// 后台 `git.diff` 的应答：在后台转成行、搭好带样式的文字（`buildWork`），回来换上；二进制、读不到时写说明。
    /// `keepScroll`：同一个文件被改了重读时按顶端那一行保留位置。
    func show(reply: [String: Any]?, keepScroll: Bool) {
        generation += 1
        guard let r = reply, r["ok"] as? Bool == true else {
            let code = ((reply?["error"] as? [String: Any])?["code"] as? String) ?? "internal"
            return showNote(code == "not_found" ? String(localized: "文件已被删除") : DiffPanel.failure(code))
        }
        if r["binary"] as? Bool == true { return showNote(String(localized: "二进制文件，不显示更改")) }
        truncated = r["truncated"] as? Bool == true
        updateMeta()
        let hunks = r["hunks"] as? [[String: Any]] ?? []
        let gen = generation
        let path = item?.path
        let start = CACurrentMediaTime()
        Self.buildWork({ Self.build(Self.rows(from: hunks)) }, { [weak self] b in
            guard let self, gen == self.generation, self.item?.path == path else { return }
            let built = CACurrentMediaTime()
            self.apply(b, keepScroll: keepScroll)
            let done = CACurrentMediaTime()
            scyllauiLog.notice("diff render bytes=\(b.bytes) lines=\(b.kinds.count) build_ms=\(Int((built - start) * 1000)) apply_ms=\(Int((done - built) * 1000))")
        })
    }

    /// 换上搭好的正文。内容没变就不动（自动刷新时常见）；新打开的文件回到顶端，按记住的折行选择（默认不折行，不按行长自动折行）。
    private func apply(_ b: Body, keepScroll: Bool) {
        if b.rows.isEmpty { return showNote(String(localized: "没有内容上的更改（可能只改了权限）")) }
        note.isHidden = true
        scroll.isHidden = false
        guard b.rows != rows || body == nil else { return }
        // 按顶端那一行在文件里的行号保留位置（不按像素，也不按正文的第几行：上面多了、少了一处改动，正文的行数就变了，
        // 按第几行会窜几行；2026-11-25 方案会话验收 D4：顶端 57 刷新后变 63）。
        var anchor: (key: LineKey, delta: Int, offset: CGFloat)?
        if keepScroll, let old = body, let a = FileViewer.topAnchor(textView, scroll),
           case let row = LineNumberRuler.lineIndex(of: a.char, in: old.lineStarts), let k = Self.key(old, at: row) {
            anchor = (k.key, k.row - row, a.offset)
        }
        let x = scroll.contentView.bounds.minX
        if anchor == nil { wrap = wrapChoice }
        // 换上正文与定位都是程序自己做的：其间可见区的挪动不当作用户往下滚到底（见 `EndFollower.positioning`）。
        follower.positioning(textView, scroll) {
            setBody(b)
            if let anchor {
                let line = max(0, min(Self.row(b, for: anchor.key) - anchor.delta, b.lineStarts.count - 1))
                FileViewer.scroll(textView, scroll, to: (b.lineStarts[line], anchor.offset), x: x)
            } else {
                scrollToTop()
            }
        }
        // 新打开一处更改：滚动条显示约 3 秒（刷新保位时不显示）。
        if anchor == nil { linger.show() }
        marker.markVisible()
        updateJumpButtons()
    }

    /// 一行在文件里的位置：新文件的行号（上下文与新增行），只在旧文件里的用旧行号（删除行）。
    enum LineKey: Equatable {
        case new(Int32)
        case old(Int32)
    }

    /// 第 `row` 行（段头取它下面第一行）的文件行号，与它实际在第几行。
    static func key(_ b: Body, at row: Int) -> (key: LineKey, row: Int)? {
        var r = row
        while r < b.kinds.count, b.kinds[r] == UInt8(ascii: "@") { r += 1 }
        guard r < b.kinds.count else { return nil }
        return (b.news[r] != 0 ? .new(b.news[r]) : .old(b.olds[r]), r)
    }

    /// 新正文里这个文件行号在第几行；没有这一行（落在两段之间）时取它后面最近的一行，再没有就取最后一行。
    static func row(_ b: Body, for key: LineKey) -> Int {
        switch key {
        case .new(let n):
            if let i = b.news.firstIndex(of: n) { return i }
            if let i = b.news.firstIndex(where: { $0 > n }) { return i }
        case .old(let n):
            if let i = b.olds.firstIndex(of: n) { return i }
            if let i = b.olds.firstIndex(where: { $0 > n }) { return i }
        }
        return max(0, b.kinds.count - 1)
    }

    private func setBody(_ b: Body?) {
        body = b
        rows = b?.rows ?? []
        textView.kinds = b?.kinds ?? []
        textView.lineStarts = b?.lineStarts ?? [0]
        textView.textStorage?.setAttributedString(b?.text ?? NSAttributedString())
        marker.reset()
        ruler.labelWidth = b.map { $0.digits * 2 + 1 }
        ruler.textChanged()
        applyWrap()
        updateJumpButtons()
        updateButtons()
    }

    static func rows(from hunks: [[String: Any]]) -> [Row] {
        var out: [Row] = []
        for h in hunks {
            out.append(.hunk(h["head"] as? String ?? "@@"))
            for l in h["lines"] as? [[Any]] ?? [] where l.count == 4 {
                let sign = (l[0] as? String)?.first ?? " "
                out.append(.line(sign: sign, old: (l[1] as? NSNumber)?.intValue, new: (l[2] as? NSNumber)?.intValue, text: l[3] as? String ?? ""))
            }
        }
        return out
    }

    /// 搭正文：一行 diff 是「符号 + 空格 + 内容」，段头原样；符号着色（绿、红、三级），段头次要颜色。不依赖排版，可在后台做。
    static func build(_ rows: [Row]) -> Body {
        var text = ""
        var kinds: [UInt8] = []
        var olds: [Int32] = []
        var news: [Int32] = []
        var starts: [Int] = []
        var changes: [Int] = []
        var maxNum = 0
        var pos = 0
        kinds.reserveCapacity(rows.count)
        olds.reserveCapacity(rows.count)
        news.reserveCapacity(rows.count)
        starts.reserveCapacity(rows.count)
        for (i, r) in rows.enumerated() {
            let line: String
            let kind: UInt8
            switch r {
            case .hunk(let h):
                line = h
                kind = UInt8(ascii: "@")
                olds.append(0)
                news.append(0)
            case .line(let sign, let old, let new, let t):
                line = String(sign) + " " + t
                kind = sign == "+" ? UInt8(ascii: "+") : sign == "-" ? UInt8(ascii: "-") : UInt8(ascii: " ")
                olds.append(Int32(old ?? 0))
                news.append(Int32(new ?? 0))
                maxNum = max(maxNum, old ?? 0, new ?? 0)
            }
            let changing = kind == UInt8(ascii: "+") || kind == UInt8(ascii: "-")
            if changing, i == 0 || !(kinds.last == UInt8(ascii: "+") || kinds.last == UInt8(ascii: "-")) { changes.append(i) }
            kinds.append(kind)
            starts.append(pos)
            let n = (line as NSString).length
            pos += n + 1
            text += line
            text += "\n"
        }
        if !text.isEmpty { text.removeLast() }
        let s = NSMutableAttributedString(string: text, attributes: [.font: font, .foregroundColor: NSColor.labelColor])
        s.beginEditing()
        for (i, k) in kinds.enumerated() {
            switch k {
            case UInt8(ascii: "@"):
                let end = i + 1 < starts.count ? starts[i + 1] - 1 : s.length
                s.addAttribute(.foregroundColor, value: NSColor.secondaryLabelColor, range: NSRange(location: starts[i], length: end - starts[i]))
            case UInt8(ascii: "+"): s.addAttribute(.foregroundColor, value: ScyllaUIStyle.ready, range: NSRange(location: starts[i], length: 1))
            case UInt8(ascii: "-"): s.addAttribute(.foregroundColor, value: ScyllaUIStyle.error, range: NSRange(location: starts[i], length: 1))
            default: s.addAttribute(.foregroundColor, value: NSColor.tertiaryLabelColor, range: NSRange(location: starts[i], length: 1))
            }
        }
        s.endEditing()
        let narrow = ("M" as NSString).size(withAttributes: [.font: font]).width
        let wide = ("中" as NSString).size(withAttributes: [.font: font]).width
        let width = ceil(FileViewer.lineWidth(text, narrow: narrow, wide: wide, tab: 28))
        return Body(rows: rows, text: s, kinds: kinds, olds: olds, news: news, lineStarts: starts.isEmpty ? [0] : starts, changes: changes,
                    digits: max(3, String(maxNum).count), width: width, bytes: text.utf8.count)
    }

    /// 行号栏第 `i` 行：旧、新两列，按位数补齐；段头一行不写。
    private func lineLabel(_ i: Int) -> String {
        guard let b = body, i < b.kinds.count, b.kinds[i] != UInt8(ascii: "@") else { return "" }
        let pad = { (n: Int32) -> String in
            let s = n == 0 ? "" : String(n)
            return String(repeating: " ", count: max(0, b.digits - s.count)) + s
        }
        return pad(b.olds[i]) + " " + pad(b.news[i])
    }

    private func showNote(_ text: String) {
        setBody(nil)
        note.stringValue = text
        note.isHidden = false
        scroll.isHidden = true
        updateButtons()
    }

    var noteText: String? { note.isHidden ? nil : note.stringValue }
    /// 测试与读屏：标题一行的元信息、在「文件」里打开是否可用。
    var metaTextForTesting: String { meta.stringValue }
    var openInFilesEnabled: Bool { filesButton.isEnabled }
    func colorForTesting(at i: Int) -> NSColor? { textView.layoutManager?.temporaryAttribute(.foregroundColor, atCharacterIndex: i, effectiveRange: nil) as? NSColor }

    func clear() {
        item = nil
        actions.path = nil
        truncated = false
        generation += 1
        setBody(nil)
    }

    // MARK: 折行与宽度

    /// 同文件查看器的源码视图：不折行时宽度用估算的最长一行（只排可见部分，横向滚不到没排到的长行的行尾），且不窄于可见区
    /// （整行底色铺满）；折行时按可见区宽度、扣掉行号栏留出的内边距。
    private func applyWrap() {
        guard let container = textView.textContainer else { return }
        if wrap {
            textView.isHorizontallyResizable = false
            container.widthTracksTextView = true
        } else {
            container.widthTracksTextView = false
            container.containerSize = NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
            textView.isHorizontallyResizable = true
        }
        scroll.hasHorizontalScroller = !wrap
        fitWidth()
        follower.remember(textView, scroll)
        let clip = scroll.contentView
        let x = wrap ? -1e6 : clip.bounds.minX
        clip.scroll(to: clip.constrainBoundsRect(NSRect(origin: NSPoint(x: x, y: clip.bounds.minY), size: clip.bounds.size)).origin)
        scroll.reflectScrolledClipView(clip)
        ruler.needsDisplay = true
    }

    private func fitWidth() {
        guard let container = textView.textContainer else { return }
        let insets = scroll.contentView.contentInsets
        let visible = scroll.contentSize.width - insets.left - insets.right
        if wrap {
            guard abs(container.containerSize.width - visible) > 0.5 || abs(textView.frame.width - visible) > 0.5 else { return }
            container.containerSize = NSSize(width: visible, height: CGFloat.greatestFiniteMagnitude)
            textView.minSize = .zero
            textView.frame.size.width = visible
        } else {
            let padding = container.lineFragmentPadding * 2 + textView.textContainerInset.width * 2
            let w = max((body?.width ?? 0) + padding, visible)
            guard abs(textView.minSize.width - w) > 0.5 else { return }
            textView.minSize = NSSize(width: w, height: 0)
        }
        textView.sizeToFit()
    }

    /// 折行按钮：切换并记住，之后新打开的文件沿用；开关前后顶端仍是同一行（按顶端字符）。
    @objc private func toggleWrap(_ sender: Any?) {
        guard body != nil, !scroll.isHidden else { return }
        let anchor = FileViewer.topAnchor(textView, scroll)
        wrap.toggle()
        wrapChoice = wrap
        follower.positioning(textView, scroll) {
            applyWrap()
            if let anchor { FileViewer.scroll(textView, scroll, to: anchor, x: scroll.contentView.bounds.minX) }
        }
        updateJumpButtons()
        updateButtons()
    }

    private func scrollToTop() {
        layoutSubtreeIfNeeded()
        scroll.tile()
        let clip = scroll.contentView
        clip.scroll(to: clip.constrainBoundsRect(NSRect(origin: NSPoint(x: -1e6, y: -1e6), size: clip.bounds.size)).origin)
        scroll.reflectScrolledClipView(clip)
    }

    // MARK: 上一处、下一处改动

    /// 跳过去后改动的第一行离顶部约 3 行。
    private static let jumpMargin = 3

    /// 顶端是第几行（按画的时候同一个取法）。
    private var topLine: Int? {
        guard let b = body, let a = FileViewer.topAnchor(textView, scroll) else { return nil }
        return LineNumberRuler.lineIndex(of: a.char, in: b.lineStarts)
    }

    /// 往下、往上要跳到的那一处（第几行）：跳过去后顶端会在它上面 3 行（不小于 0），只算顶端真会往下、往上挪的
    /// （开头就是改动时，在顶端不算「上面还有」；测试版 huge.txt 第 1 行就是改动）。
    private func target(next: Bool) -> Int? {
        guard let b = body, !b.changes.isEmpty else { return nil }
        let top = topLine ?? 0
        let dest = { (c: Int) in max(0, c - Self.jumpMargin) }
        // 最后一行已在可见区里：顶端挪不下去了，后面的那一处也已经看得见。
        if next, endVisible { return nil }
        return next ? b.changes.first { dest($0) > top } : b.changes.last { dest($0) < top }
    }

    /// 最后一行在可见区里（与画的时候同一个取法）。
    private var endVisible: Bool {
        guard let lm = textView.layoutManager, let tc = textView.textContainer, let n = textView.textStorage?.length, n > 0 else { return false }
        let o = textView.textContainerOrigin
        let visible = textView.visibleRect.offsetBy(dx: -o.x, dy: -o.y)
        let chars = lm.characterRange(forGlyphRange: lm.glyphRange(forBoundingRect: visible, in: tc), actualGlyphRange: nil)
        return NSMaxRange(chars) >= n
    }

    private func jump(next: Bool) {
        guard let b = body, let line = target(next: next) else { return }
        let top = max(0, line - Self.jumpMargin)
        FileViewer.scroll(textView, scroll, to: (b.lineStarts[top], 0), x: scroll.contentView.bounds.minX)
        updateJumpButtons()
    }

    private func updateJumpButtons() {
        prevButton.isEnabled = target(next: false) != nil
        nextButton.isEnabled = target(next: true) != nil
    }

    @objc private func prevChange(_ sender: Any?) { jump(next: false) }
    @objc private func nextChange(_ sender: Any?) { jump(next: true) }

    // MARK: 「…」菜单与按钮

    /// 「…」菜单：拷贝相对路径、打开方式 ▸、在访达中显示；已删除的文件后两项置灰。折行在标题行的按钮上（第 6 版 R1）。
    func moreMenu() -> NSMenu {
        let menu = NSMenu()
        menu.autoenablesItems = false
        for i in actions.menuItems(exists: item?.status != "D") { menu.addItem(i) }
        return menu
    }

    @objc private func showMore(_ sender: Any?) {
        moreMenu().popUp(positioning: nil, at: NSPoint(x: 0, y: moreButton.bounds.height + 4), in: moreButton)
    }

    private func updateButtons() {
        // 折行只在显示着正文时有（读取中、说明文字时隐藏；在最左边，出现或消失时别的按钮不挪位置）。
        wrapButton.isHidden = body == nil || scroll.isHidden
        RightPanelHeader.styleWrap(wrapButton, on: wrap)
        listButton.toolTip = listHidden ? String(localized: "显示更改列表") : String(localized: "隐藏更改列表")
        listButton.setAccessibilityLabel(listButton.toolTip)
        listButton.contentTintColor = listHidden ? .secondaryLabelColor : .controlAccentColor
    }

    @objc private func openInFiles(_ sender: Any?) { onOpenInFiles?() }
    @objc private func toggleList(_ sender: Any?) { onToggleList?() }

    // MARK: 测试

    var text: String { textView.string }
    var wrapOn: Bool { wrap }
    var toolNamesForTesting: [String] { tools.arrangedSubviews.filter { !$0.isHidden }.compactMap { ($0 as? NSButton)?.toolTip } }
    var wrapTintForTesting: NSColor? { wrapButton.contentTintColor }
    var prevEnabled: Bool { prevButton.isEnabled }
    var nextEnabled: Bool { nextButton.isEnabled }
    var topLineForTesting: Int? { topLine }
    /// 顶端那一行的新文件行号（段头、删除行为 0）。
    var topNewLineForTesting: Int? { topLine.flatMap { i in body.map { Int($0.news[i]) } } }
    func moveToEndForTesting() { textView.moveToEndOfDocument(nil) }
    /// 测试用：在「保持横向」里把可见区往右挪 `dx`（模拟系统把可见区带到行尾）。
    func shiftRightKeepingXForTesting(_ dx: CGFloat) {
        textView.keepingX {
            let clip = scroll.contentView
            clip.scroll(to: clip.constrainBoundsRect(NSRect(origin: NSPoint(x: clip.bounds.minX + dx, y: clip.bounds.minY), size: clip.bounds.size)).origin)
            scroll.reflectScrolledClipView(clip)
        }
    }
    var clipMinXForTesting: CGFloat { scroll.contentView.bounds.minX }
    var rulerClipsForTesting: Bool { ruler.clipsToBounds }
    func rulerColorForTesting(_ line: Int) -> NSColor? { ruler.rowColor?(line) }
    func nextChangeForTesting() { jump(next: true) }
    func prevChangeForTesting() { jump(next: false) }
    func toggleWrapForTesting() { wrapButton.performClick(nil) }
    func scrollToLineForTesting(_ line: Int) {
        guard let b = body else { return }
        FileViewer.scroll(textView, scroll, to: (b.lineStarts[min(line, b.lineStarts.count - 1)], 0))
    }
    func labelForTesting(_ line: Int) -> String { lineLabel(line) }
    /// 像把滚动条设到最底那样，滚到正文此刻（估算）高度的底部。
    func scrollToBottomForTesting() {
        let clip = scroll.contentView
        clip.scroll(to: NSPoint(x: clip.bounds.minX, y: textView.frame.maxY - clip.bounds.height))
        scroll.reflectScrolledClipView(clip)
    }
    /// 按真实顺序滚动 `dy`：先滚可见区（此刻滚动条的值还是旧的），再让滚动条跟上。
    func scrollByForTesting(dy: CGFloat) {
        let clip = scroll.contentView
        clip.scroll(to: clip.constrainBoundsRect(NSRect(origin: NSPoint(x: clip.bounds.minX, y: clip.bounds.minY + dy), size: clip.bounds.size)).origin)
        scroll.reflectScrolledClipView(clip)
    }
    var clipMinYForTesting: CGFloat { scroll.contentView.bounds.minY }
    var endVisibleForTesting: Bool { endVisible }
    /// 正文至少和可见区一样宽（不折行时整行底色铺满）。
    var bodyCoversVisibleWidth: Bool { textView.frame.width >= scroll.contentView.bounds.width - scroll.contentView.contentInsets.left - 0.5 }
}

/// Diff 正文：整行底色（新增绿底、删除红底、`@@` 头淡蓝底）按行画、铺满整行宽度，不只盖住文字；折行时每一段都画。
final class DiffTextView: ViewerTextView {
    var kinds: [UInt8] = []
    var lineStarts: [Int] = [0]

    override func drawBackground(in rect: NSRect) {
        super.drawBackground(in: rect)
        guard !kinds.isEmpty, let lm = layoutManager, let tc = textContainer else { return }
        let origin = textContainerOrigin
        let glyphs = lm.glyphRange(forBoundingRect: rect.offsetBy(dx: -origin.x, dy: -origin.y), in: tc)
        guard glyphs.length > 0 else { return }
        let right = max(bounds.maxX, visibleRect.maxX)
        lm.enumerateLineFragments(forGlyphRange: glyphs) { frag, _, _, range, _ in
            let c = lm.characterIndexForGlyph(at: range.location)
            let line = LineNumberRuler.lineIndex(of: c, in: self.lineStarts)
            guard line < self.kinds.count, let color = Self.color(self.kinds[line]) else { return }
            color.setFill()
            NSRect(x: 0, y: frag.minY + origin.y, width: right, height: frag.height).fill()
        }
    }

    static func color(_ kind: UInt8) -> NSColor? {
        switch kind {
        case UInt8(ascii: "+"): ScyllaUIStyle.ready.withAlphaComponent(0.14)
        case UInt8(ascii: "-"): ScyllaUIStyle.error.withAlphaComponent(0.13)
        case UInt8(ascii: "@"): NSColor.systemBlue.withAlphaComponent(0.08)
        default: nil
        }
    }
}
