// 设置「关于」（M4a，docs/design.md 10.4 节决策 8）：版本（构建号 · 构建标识）、后台的构建标识与是否一致、
// 数据目录与日志目录；底部是 GPL 声明与小号的「开源许可…」「使用须知…」「隐私说明…」「第三方许可…」（M4b 块 4、D15，
// docs/design.md 10.6 节）。菜单中的系统关于面板保留。

import AppKit

final class AboutPane: NSViewController {
    private let core: CoreClient
    private let version = NSTextField(labelWithString: "")
    private let daemon = NSTextField(labelWithString: "")
    // 自动更新（M4b 块 3）：开关、上次检查时间与「检查更新…」。
    private let autoCheck = NSButton(checkboxWithTitle: String(localized: "自动检查更新"), target: nil, action: nil)
    private let lastCheck = NSTextField(labelWithString: "")

    init(core: CoreClient) {
        self.core = core
        super.init(nibName: nil, bundle: nil)
        title = String(localized: "关于")
    }

    required init?(coder: NSCoder) { fatalError() }

    override func loadView() {
        let icon = NSImageView(image: NSApp.applicationIconImage)
        icon.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([icon.widthAnchor.constraint(equalToConstant: 64), icon.heightAnchor.constraint(equalToConstant: 64)])
        let name = NSTextField(labelWithString: Bundle.main.infoDictionary?["CFBundleDisplayName"] as? String ?? "ScyllaUI")
        name.font = .boldSystemFont(ofSize: 17)
        version.textColor = .secondaryLabelColor
        version.isSelectable = true
        let titles = NSStackView(views: [name, version])
        titles.orientation = .vertical
        titles.alignment = .leading
        titles.spacing = 4
        let head = NSStackView(views: [icon, titles])
        head.spacing = 14
        head.alignment = .centerY

        daemon.isSelectable = true
        let grid = NSGridView(views: [
            [AppearancePane.label(String(localized: "后台：")), daemon],
            [AppearancePane.label(String(localized: "数据目录：")), Self.pathRow(CoreClient.dataDirectory)],
            [AppearancePane.label(String(localized: "日志目录：")), Self.pathRow(core.logDir)],
            [AppearancePane.label(String(localized: "更新：")), updateRow()],
        ])
        grid.column(at: 0).xPlacement = .trailing
        grid.rowAlignment = .firstBaseline
        grid.rowSpacing = 10
        grid.columnSpacing = 8

        // 开源许可、使用须知、隐私说明与第三方许可：小号按钮，用系统默认的文本程序打开包内文件（须知与隐私说明按界面语言选中文
        // 或英文；开源许可是 GPL 英文原文，2026-11-07 取代原先的「许可协议…」）；界面其余部分不出现 Ghostty（10.5 节决策 25）。
        // 上面一行是 GPL 第 5(d) 条要求交互界面显示的「适当法律声明」：版权、无担保、怎么看许可全文。
        let statement = NSTextField(labelWithString: String(localized: "ScyllaUI 是自由软件，按 GNU GPL 3.0 或更高版本发布，不提供任何担保。"))
        statement.textColor = .secondaryLabelColor
        statement.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        let legal = NSStackView(views: [
            Self.smallButton(String(localized: "开源许可…"), self, #selector(openLicense)),
            Self.smallButton(String(localized: "使用须知…"), self, #selector(openNotice)),
            Self.smallButton(String(localized: "隐私说明…"), self, #selector(openPrivacy)),
            Self.smallButton(String(localized: "第三方许可…"), self, #selector(openNotices)),
        ])
        legal.spacing = 8
        let legalColumn = NSStackView(views: [statement, legal])
        legalColumn.orientation = .vertical
        legalColumn.alignment = .leading
        legalColumn.spacing = 8

        let stack = NSStackView(views: [head, grid, legalColumn])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 20
        view = AppearancePane.padded(stack)
    }

    override func viewWillAppear() {
        super.viewWillAppear()
        let info = Bundle.main.infoDictionary
        let short = info?["CFBundleShortVersionString"] as? String ?? "-"
        let number = info?["CFBundleVersion"] as? String ?? "-"
        let build = core.appBuildID ?? "-"
        version.stringValue = String(localized: "版本 \(short)（构建号 \(number) · 构建标识 \(build)）")
        if let d = core.daemonBuildID {
            daemon.stringValue = d == core.appBuildID
                ? String(localized: "scyllauid 已连接，构建标识 \(d)（与 App 一致）")
                : String(localized: "scyllauid 已连接，构建标识 \(d)（与 App 不一致）")
        } else {
            daemon.stringValue = String(localized: "scyllauid 未连接")
        }
        refreshUpdate()
    }

    /// 「自动检查更新」开关、「检查更新…」与上次检查时间（每天一次；关掉后只在点「检查更新…」时联网）。
    private func updateRow() -> NSView {
        autoCheck.target = self
        autoCheck.action = #selector(toggleAutoCheck)
        let check = Self.smallButton(String(localized: "检查更新…"), self, #selector(checkNow))
        lastCheck.textColor = .secondaryLabelColor
        lastCheck.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        let top = NSStackView(views: [autoCheck, check])
        top.spacing = 12
        top.alignment = .firstBaseline
        let col = NSStackView(views: [top, lastCheck])
        col.orientation = .vertical
        col.alignment = .leading
        col.spacing = 4
        return col
    }

    private func refreshUpdate() {
        #if canImport(Sparkle)
        guard let updater = AppUpdater.shared.updater else {
            autoCheck.isEnabled = false
            autoCheck.state = .off
            lastCheck.stringValue = String(localized: "从「应用程序」打开 ScyllaUI 后才能检查更新。")
            return
        }
        autoCheck.isEnabled = true
        autoCheck.state = updater.automaticallyChecksForUpdates ? .on : .off
        if let date = updater.lastUpdateCheckDate {
            lastCheck.stringValue = String(localized: "上次检查：\(date.formatted(date: .abbreviated, time: .shortened))")
        } else {
            lastCheck.stringValue = String(localized: "还没有检查过更新。")
        }
        #endif
    }

    @objc private func toggleAutoCheck() {
        #if canImport(Sparkle)
        AppUpdater.shared.updater?.automaticallyChecksForUpdates = autoCheck.state == .on
        #endif
        refreshUpdate()
    }

    @objc private func checkNow() {
        #if canImport(Sparkle)
        AppUpdater.shared.checkForUpdates()
        #endif
    }

    private static func smallButton(_ title: String, _ target: AnyObject, _ action: Selector) -> NSButton {
        let b = NSButton(title: title, target: target, action: action)
        b.bezelStyle = .rounded
        b.controlSize = .small
        return b
    }

    @objc private func openLicense() { LegalDocs.open(.license) }
    @objc private func openNotice() { LegalDocs.open(.notice) }
    @objc private func openPrivacy() { LegalDocs.open(.privacy) }

    @objc private func openNotices() {
        guard let url = Bundle.main.url(forResource: "ThirdPartyNotices", withExtension: "txt") else { return NSSound.beep() }
        NSWorkspace.shared.open(url)
    }

    /// 路径（主目录写作 ~，过长时中间截断，悬停看全）与「在 Finder 中显示」。
    static func pathRow(_ path: String) -> NSView {
        let home = NSHomeDirectory()
        let shown = path.hasPrefix(home + "/") ? "~" + path.dropFirst(home.count) : path
        let label = NSTextField(labelWithString: shown)
        label.lineBreakMode = .byTruncatingMiddle
        label.isSelectable = true
        label.toolTip = path
        label.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        label.widthAnchor.constraint(lessThanOrEqualToConstant: 300).isActive = true
        let button = NSButton(title: String(localized: "在 Finder 中显示"), target: nil, action: nil)
        button.bezelStyle = .rounded
        button.controlSize = .small
        let target = RevealTarget(path: path)
        button.target = target
        button.action = #selector(RevealTarget.reveal)
        objc_setAssociatedObject(button, &RevealTarget.key, target, .OBJC_ASSOCIATION_RETAIN)
        let row = NSStackView(views: [label, button])
        row.spacing = 8
        row.alignment = .firstBaseline
        return row
    }
}

/// 「在 Finder 中显示」按钮的目标：目录不存在时退到上一级。
private final class RevealTarget: NSObject {
    static var key = 0
    let path: String
    init(path: String) { self.path = path }
    @objc func reveal() {
        var p = path
        while !FileManager.default.fileExists(atPath: p), p != "/" { p = (p as NSString).deletingLastPathComponent }
        NSWorkspace.shared.selectFile(nil, inFileViewerRootedAtPath: p)
    }
}
/// 包内的使用须知、隐私说明（`Resources/Legal/`，按界面语言取英文或中文）与开源许可（根目录 `LICENSE`，构建时复制为
/// `LICENSE.txt`，只有英文原文）。
enum LegalDocs {
    enum Doc: String { case notice = "Notice", privacy = "Privacy", license = "LICENSE" }

    static func url(_ doc: Doc) -> URL? {
        if doc == .license { return Bundle.main.url(forResource: doc.rawValue, withExtension: "txt") }
        let lang = (Bundle.main.preferredLocalizations.first ?? "").hasPrefix("en") ? "en" : "zh-Hans"
        return Bundle.main.url(forResource: "\(doc.rawValue)-\(lang)", withExtension: "txt")
    }

    static func text(_ doc: Doc) -> String? { url(doc).flatMap { try? String(contentsOf: $0, encoding: .utf8) } }

    static func open(_ doc: Doc) {
        guard let u = url(doc) else { return NSSound.beep() }
        NSWorkspace.shared.open(u)
    }
}
