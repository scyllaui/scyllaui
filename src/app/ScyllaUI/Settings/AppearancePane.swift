// 设置「外观」（M4a，docs/design.md 10.4 节决策 2）：主题（前 10 个配色家族与深浅）、终端字体与字号、实时预览。
// 选定即写 `<数据目录>/theme.conf`（ScyllaUI 自己的外观覆盖，在用户的 Ghostty 配置之后加载）并重新载入配置，所有终端
// 即时生效；不读写用户的 Ghostty 配置。读字体列表与读写文件都在后台队列。

import AppKit
import CoreText

final class AppearancePane: NSViewController {
    private let runtime: GhosttyRuntime
    /// 设置里改了字号、新配置已应用之后调用：工作区把所有终端的字号恢复为新配置的字号（含手动调过的）。
    private let onFontSizeChange: () -> Void
    private let io = DispatchQueue(label: "dev.scyllaui.settings.appearance", qos: .userInitiated)
    private var overrides = AppearanceOverrides()
    private var fonts: [String] = []
    private let themePopup = NSPopUpButton(frame: .zero, pullsDown: false)
    private let modePopup = NSPopUpButton(frame: .zero, pullsDown: false)
    private let fontPopup = NSPopUpButton(frame: .zero, pullsDown: false)
    private let sizePopup = NSPopUpButton(frame: .zero, pullsDown: false)
    private let preview = NSTextView()
    static let sizes: [Double] = Array(9...24).map(Double.init)

    init(runtime: GhosttyRuntime, onFontSizeChange: @escaping () -> Void) {
        self.runtime = runtime
        self.onFontSizeChange = onFontSizeChange
        super.init(nibName: nil, bundle: nil)
        title = String(localized: "外观")
    }

    required init?(coder: NSCoder) { fatalError() }

    override func loadView() {
        for p in [themePopup, modePopup, fontPopup, sizePopup] {
            p.target = self
            p.action = #selector(changed(_:))
            p.isEnabled = false
        }
        themePopup.widthAnchor.constraint(greaterThanOrEqualToConstant: 240).isActive = true
        fontPopup.widthAnchor.constraint(greaterThanOrEqualToConstant: 240).isActive = true
        modePopup.addItems(withTitles: [String(localized: "跟随系统"), String(localized: "深色"), String(localized: "浅色")])

        let scroll = NSScrollView()
        scroll.borderType = .bezelBorder
        scroll.hasVerticalScroller = false
        scroll.documentView = preview
        preview.isEditable = false
        preview.isSelectable = false
        preview.textContainerInset = NSSize(width: 6, height: 8)
        preview.textContainer?.lineFragmentPadding = 0
        // 与终端一样不折行，超出宽度的部分裁掉；高度够 24 号字的 4 行。
        preview.textContainer?.widthTracksTextView = false
        preview.textContainer?.containerSize = NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
        preview.isHorizontallyResizable = true
        scroll.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            scroll.widthAnchor.constraint(equalToConstant: 440),
            scroll.heightAnchor.constraint(equalToConstant: 132),
        ])
        preview.frame = NSRect(x: 0, y: 0, width: 440, height: 132)
        preview.autoresizingMask = [.height]

        let note = NSTextField(wrappingLabelWithString: String(localized: "选定即作用于所有终端。改字号时，用 Cmd+= / Cmd+- 临时调过字号的终端也换成这里的字号。"))
        note.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        note.textColor = .secondaryLabelColor
        note.preferredMaxLayoutWidth = 440

        let grid = NSGridView(views: [
            [Self.label(String(localized: "主题：")), themePopup],
            [Self.label(String(localized: "深浅：")), modePopup],
            [Self.label(String(localized: "终端字体：")), fontPopup],
            [Self.label(String(localized: "字号：")), sizePopup],
            [Self.label(String(localized: "预览：")), scroll],
            [NSGridCell.emptyContentView, note],
        ])
        grid.column(at: 0).xPlacement = .trailing
        grid.rowAlignment = .firstBaseline
        grid.row(at: 4).yPlacement = .top
        grid.rowSpacing = 10
        grid.columnSpacing = 8
        grid.row(at: 2).topPadding = 8
        grid.row(at: 4).topPadding = 8
        view = Self.padded(grid)
        NotificationCenter.default.addObserver(forName: GhosttyRuntime.configDidChange, object: runtime, queue: .main) { [weak self] _ in
            self?.refreshPreview()
        }
        load()
    }

    static func label(_ text: String) -> NSTextField {
        let l = NSTextField(labelWithString: text)
        l.alignment = .right
        return l
    }

    /// 分页内容四周留白（系统设置窗口的常见边距）。
    /// 分页内容加边距；设置窗口固定尺寸后（docs/design.md 10.5 节决策 16），内容靠上、水平居中，多出的空间留在下方与两侧。
    /// `fill`：内容铺满两侧边距之间的宽度（2026-10-28 用户要求，列表类分页不再缩在中间）。
    static func padded(_ content: NSView, fill: Bool = false) -> NSView {
        let v = NSView()
        content.translatesAutoresizingMaskIntoConstraints = false
        v.addSubview(content)
        NSLayoutConstraint.activate([
            content.topAnchor.constraint(equalTo: v.topAnchor, constant: 20),
            content.bottomAnchor.constraint(lessThanOrEqualTo: v.bottomAnchor, constant: -20),
            content.centerXAnchor.constraint(equalTo: v.centerXAnchor),
            content.leadingAnchor.constraint(greaterThanOrEqualTo: v.leadingAnchor, constant: 30),
        ])
        if fill { content.leadingAnchor.constraint(equalTo: v.leadingAnchor, constant: 30).isActive = true }
        return v
    }

    /// 后台读 theme.conf 与系统的等宽字体族，回主线程填下拉框。
    private func load() {
        io.async {
            let text = (try? String(contentsOfFile: GhosttyRuntime.themeFile, encoding: .utf8)) ?? ""
            let o = AppearanceOverrides.parse(text)
            let fonts = Self.monospacedFamilies()
            DispatchQueue.main.async {
                self.overrides = o
                self.fonts = fonts
                self.fillPopups()
                for p in [self.themePopup, self.fontPopup, self.sizePopup] { p.isEnabled = true }
                self.refreshPreview()
                scyllauiLog.notice("settings appearance fonts=\(fonts.count) theme=\(o.theme ?? "-", privacy: .public) font=\(o.fontFamily ?? "-", privacy: .public) size=\(o.fontSize.map { String($0) } ?? "-", privacy: .public)")
            }
        }
    }

    /// 系统报告为等宽的字体族（CoreText，线程安全），去掉系统隐藏的「.」开头的族。
    static func monospacedFamilies() -> [String] {
        let collection = CTFontCollectionCreateFromAvailableFonts(nil)
        let descriptors = (CTFontCollectionCreateMatchingFontDescriptors(collection) as? [CTFontDescriptor]) ?? []
        var names = Set<String>()
        for d in descriptors {
            let traits = CTFontDescriptorCopyAttribute(d, kCTFontTraitsAttribute) as? [CFString: Any]
            let symbolic = (traits?[kCTFontSymbolicTrait] as? NSNumber)?.uint32Value ?? 0
            guard symbolic & CTFontSymbolicTraits.traitMonoSpace.rawValue != 0,
                  let family = CTFontDescriptorCopyAttribute(d, kCTFontFamilyNameAttribute) as? String,
                  !family.hasPrefix(".") else { continue }
            names.insert(family)
        }
        return names.sorted { $0.localizedStandardCompare($1) == .orderedAscending }
    }

    // MARK: 下拉框

    /// 菜单项的 tag：清单项从 `base` 起编号，避开分隔线等菜单项默认的 0（按 tag 选中时会选到分隔线）。
    private enum Tag { static let follow = -1, other = -2, base = 100 }

    private func fillPopups() {
        let follow = String(localized: "默认配置")
        // 主题
        themePopup.removeAllItems()
        add(themePopup, follow, tag: Tag.follow)
        themePopup.menu?.addItem(.separator())
        for (i, f) in ThemeCatalog.families.enumerated() { add(themePopup, f.name, tag: Tag.base + i) }
        let choice = ThemeCatalog.choice(for: overrides.theme)
        if case .other(let v) = choice {
            themePopup.menu?.addItem(.separator())
            add(themePopup, String(localized: "其他：\(v)"), tag: Tag.other)
        }
        switch choice {
        case .follow: themePopup.selectItem(withTag: Tag.follow)
        case .other: themePopup.selectItem(withTag: Tag.other)
        case .family(let i, _): themePopup.selectItem(withTag: Tag.base + i)
        }
        syncMode(choice)
        // 字体
        fontPopup.removeAllItems()
        add(fontPopup, follow, tag: Tag.follow)
        fontPopup.menu?.addItem(.separator())
        for (i, name) in fonts.enumerated() { add(fontPopup, name, tag: Tag.base + i) }
        if let f = overrides.fontFamily {
            if let i = fonts.firstIndex(of: f) {
                fontPopup.selectItem(withTag: Tag.base + i)
            } else {
                fontPopup.menu?.addItem(.separator())
                add(fontPopup, String(localized: "其他：\(f)"), tag: Tag.other)
                fontPopup.selectItem(withTag: Tag.other)
            }
        } else {
            fontPopup.selectItem(withTag: Tag.follow)
        }
        // 字号
        sizePopup.removeAllItems()
        add(sizePopup, follow, tag: Tag.follow)
        sizePopup.menu?.addItem(.separator())
        for (i, s) in Self.sizes.enumerated() { add(sizePopup, AppearanceOverrides.number(s), tag: Tag.base + i) }
        if let s = overrides.fontSize {
            if let i = Self.sizes.firstIndex(of: s) {
                sizePopup.selectItem(withTag: Tag.base + i)
            } else {
                sizePopup.menu?.addItem(.separator())
                add(sizePopup, AppearanceOverrides.number(s), tag: Tag.other)
                sizePopup.selectItem(withTag: Tag.other)
            }
        } else {
            sizePopup.selectItem(withTag: Tag.follow)
        }
    }

    private func add(_ popup: NSPopUpButton, _ title: String, tag: Int) {
        let item = NSMenuItem(title: title, action: nil, keyEquivalent: "")
        item.tag = tag
        popup.menu?.addItem(item)
    }

    /// 深浅只对有官方深浅两版的家族可选；只有深色版的显示「深色」且不可改。
    private func syncMode(_ choice: ThemeChoice) {
        guard case .family(let i, let mode) = choice else {
            modePopup.selectItem(at: 0)
            modePopup.isEnabled = false
            return
        }
        let both = ThemeCatalog.families[i].light != nil
        modePopup.selectItem(at: !both ? 1 : mode == .system ? 0 : mode == .dark ? 1 : 2)
        modePopup.isEnabled = both
    }

    private var currentThemeChoice: ThemeChoice {
        let tag = themePopup.selectedTag()
        switch tag {
        case Tag.follow: return .follow
        case Tag.other: return ThemeCatalog.choice(for: overrides.theme)
        default:
            let mode: ThemeMode = [ThemeMode.system, .dark, .light][max(0, modePopup.indexOfSelectedItem)]
            return .family(tag - Tag.base, mode)
        }
    }

    @objc private func changed(_ sender: NSPopUpButton) {
        var o = overrides
        if sender === themePopup || sender === modePopup {
            // 换家族时深浅回到默认的「跟随系统」。
            if sender === themePopup { modePopup.selectItem(at: 0) }
            let choice = currentThemeChoice
            o.theme = ThemeCatalog.value(for: choice)
            syncMode(choice)
        } else if sender === fontPopup {
            let tag = fontPopup.selectedTag()
            o.fontFamily = tag == Tag.follow ? nil : tag == Tag.other ? overrides.fontFamily : fonts[tag - Tag.base]
        } else if sender === sizePopup {
            let tag = sizePopup.selectedTag()
            o.fontSize = tag == Tag.follow ? nil : tag == Tag.other ? overrides.fontSize : Self.sizes[tag - Tag.base]
        }
        guard o != overrides else { return }
        let sizeChanged = o.fontSize != overrides.fontSize
        overrides = o
        refreshPreview()
        let text = o.render()
        io.async {
            do {
                try text.write(toFile: GhosttyRuntime.themeFile, atomically: true, encoding: .utf8)
                scyllauiLog.notice("appearance set theme=\(o.theme ?? "follow", privacy: .public) font=\(o.fontFamily ?? "follow", privacy: .public) size=\(o.fontSize.map { String($0) } ?? "follow", privacy: .public)")
            } catch {
                scyllauiLog.error("写外观文件失败：\(error.localizedDescription, privacy: .public)")
            }
            DispatchQueue.main.async { self.runtime.reloadConfig(then: sizeChanged ? self.onFontSizeChange : nil) }
        }
    }

    // MARK: 预览

    /// 4 行样例：生效配置的背景、前景与调色板，所选字体与字号（跟随 Ghostty 时字体读不出来，用系统等宽字体近似）。
    private func refreshPreview() {
        let size = overrides.fontSize ?? runtime.fontSize ?? 13
        let font = overrides.fontFamily.flatMap {
            NSFontManager.shared.font(withFamily: $0, traits: [], weight: 5, size: size)
        } ?? NSFont.monospacedSystemFont(ofSize: size, weight: .regular)
        let bg = runtime.backgroundColor ?? .textBackgroundColor
        let fg = runtime.foregroundColor ?? .textColor
        let p = runtime.palette ?? []
        func c(_ i: Int) -> NSColor { i < p.count ? p[i] : fg }
        let lines: [[(String, NSColor)]] = [
            [("~/projects/scyllaui", c(4)), (" (main)", c(5)), (" ❯ ", c(2)), ("git diff --stat", fg)],
            [(" src/app/Settings.swift | 42 ", fg), ("++++++++++++", c(2)), ("------", c(1))],
            [(" 1 file changed, 28 insertions(+), 14 deletions(-)", fg)],
            [("❯ ", c(2)), ("claude", fg)],
        ]
        let out = NSMutableAttributedString()
        for (n, line) in lines.enumerated() {
            for (text, color) in line { out.append(NSAttributedString(string: text, attributes: [.font: font, .foregroundColor: color])) }
            if n < lines.count - 1 { out.append(NSAttributedString(string: "\n", attributes: [.font: font])) }
        }
        preview.backgroundColor = bg
        preview.textStorage?.setAttributedString(out)
    }
}
