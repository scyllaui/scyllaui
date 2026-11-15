// 「报告问题」窗口（2026-11-02，docs/design.md 10.6 节「问题反馈」）：主窗口上的表单。打开时就在本机生成诊断包文件夹，
// 列出要附上的文件，用户可在 Finder 里逐个看；确认后由「邮件」或浏览器里的 GitHub 用用户自己的账号发出，App 不联网。

import AppKit

final class ProblemReportWindow: NSWindowController {
    private let env: ProblemReport.Environment
    private let home = NSHomeDirectory()
    private let folder: URL
    private let items: [ProblemReport.Item]
    private let scroll = NSTextView.scrollableTextView()
    private var text: NSTextView { scroll.documentView as! NSTextView }
    private let crashBox = NSButton(checkboxWithTitle: "", target: nil, action: nil)
    var onClose: (() -> Void)?

    init(env: ProblemReport.Environment, prefill: String, logDir: String) throws {
        self.env = env
        let now = Date()
        folder = try ProblemReport.makeFolder(now: now)
        items = ProblemReport.collect(logDir: logDir, home: home, engine: env.engine, now: now)
        ProblemReport.write(items.filter { !$0.crash }, to: folder, home: home)
        let panel = NSPanel(contentRect: NSRect(x: 0, y: 0, width: 560, height: 10), styleMask: [.titled], backing: .buffered, defer: false)
        super.init(window: panel)
        text.string = prefill
        panel.contentView = content()
    }

    required init?(coder: NSCoder) { fatalError() }

    private var crashes: [ProblemReport.Item] { items.filter(\.crash) }

    private func content() -> NSView {
        let title = NSTextField(labelWithString: String(localized: "报告问题"))
        title.font = .boldSystemFont(ofSize: NSFont.systemFontSize)
        let intro = Self.note(String(localized: "在本机生成诊断包，您看过、确认后，用您自己的邮件或 GitHub 账号发出；ScyllaUI 不会自动上传任何内容。"))

        let ask = NSTextField(labelWithString: String(localized: "问题描述（发生了什么、大约几点、怎么复现）："))
        text.isRichText = false
        text.font = .systemFont(ofSize: NSFont.systemFontSize)
        text.isAutomaticQuoteSubstitutionEnabled = false
        text.textContainerInset = NSSize(width: 4, height: 4)
        scroll.borderType = .bezelBorder
        scroll.heightAnchor.constraint(equalToConstant: 110).isActive = true

        let listTitle = NSTextField(labelWithString: String(localized: "将附上的文件："))
        let size = ByteCountFormatter()
        size.countStyle = .file
        var lines = [String(localized: "summary.txt：版本、各 Agent 的状态同步与您的描述")]
        lines += items.filter { !$0.crash }.map { "logs/\($0.name)  \(size.string(fromByteCount: Int64($0.size)))" }
        let list = NSTextField(wrappingLabelWithString: lines.joined(separator: "\n"))
        list.isSelectable = true
        let privacy = Self.note(String(localized: "日志里只有事件与状态，不含提示词、回复和密钥；家目录已写成 ~，目录名仍可见。"))

        var views: [NSView] = [title, intro, ask, scroll, listTitle, list, privacy]
        if !crashes.isEmpty {
            crashBox.title = String(localized: "附上崩溃报告（\(crashes.count) 份）")
            crashBox.state = .on
            views.append(crashBox)
            views.append(Self.note(String(localized: "崩溃报告可能带有崩溃瞬间内存里的文字，例如终端里正在显示的内容。")))
        }
        let reveal = NSButton(title: String(localized: "在 Finder 中显示"), target: self, action: #selector(revealFolder))
        reveal.bezelStyle = .rounded
        reveal.controlSize = .small
        views.append(reveal)

        let cancel = NSButton(title: String(localized: "取消"), target: self, action: #selector(dismiss(_:)))
        cancel.keyEquivalent = "\u{1b}"
        let github = NSButton(title: String(localized: "在 GitHub 提交…"), target: self, action: #selector(sendGitHub))
        let mail = NSButton(title: String(localized: "用邮件发送…"), target: self, action: #selector(sendMail))
        // 描述框里回车是换行，发送用 Cmd+回车。
        mail.keyEquivalent = "\r"
        mail.keyEquivalentModifierMask = .command
        let buttons = NSStackView(views: [NSView(), cancel, github, mail])
        buttons.spacing = 10
        views.append(buttons)

        let stack = NSStackView(views: views)
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 8
        stack.setCustomSpacing(16, after: intro)
        stack.setCustomSpacing(16, after: scroll)
        stack.setCustomSpacing(18, after: reveal)
        stack.edgeInsets = NSEdgeInsets(top: 20, left: 20, bottom: 20, right: 20)
        stack.translatesAutoresizingMaskIntoConstraints = false
        for v in [intro, scroll, list, privacy, buttons] { v.widthAnchor.constraint(equalTo: stack.widthAnchor, constant: -40).isActive = true }
        stack.widthAnchor.constraint(equalToConstant: 560).isActive = true
        return stack
    }

    private static func note(_ s: String) -> NSTextField {
        let f = NSTextField(wrappingLabelWithString: s)
        f.textColor = .secondaryLabelColor
        f.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        return f
    }

    // MARK: 动作

    /// 摘要与崩溃报告按此刻的描述和勾选写进文件夹。
    private func refreshFolder() {
        let crashDir = folder.appendingPathComponent("crash", isDirectory: true)
        try? FileManager.default.removeItem(at: crashDir)
        if crashBox.state == .on { ProblemReport.write(crashes, to: folder, home: home) }
        try? Data(summary.utf8).write(to: folder.appendingPathComponent("summary.txt"))
    }

    private var summary: String { ProblemReport.summary(env, description: problemText, now: Date()) }
    private var problemText: String { text.string.trimmingCharacters(in: .whitespacesAndNewlines) }

    @objc private func revealFolder() {
        refreshFolder()
        NSWorkspace.shared.activateFileViewerSelecting([folder])
    }

    /// 写好摘要、打包；失败时说明原因，不关窗口。
    private func makeZip() -> URL? {
        refreshFolder()
        do {
            return try ProblemReport.zip(folder, summary: summary)
        } catch {
            scyllauiLog.error("problem_report zip_failed error=\(error.localizedDescription, privacy: .public)")
            let alert = NSAlert()
            alert.messageText = String(localized: "诊断包打包失败")
            alert.informativeText = String(localized: "可以点「在 Finder 中显示」，把文件夹里的文件直接附上。") + "\n\n" + error.localizedDescription
            alert.beginSheetModal(for: window!)
            return nil
        }
    }

    /// 邮件与 GitHub 正文：描述加上版本，诊断包另附。
    private func body(zip: URL, dragHint: Bool) -> String {
        var lines = [problemText.isEmpty ? String(localized: "（请描述发生了什么、大约几点、怎么复现）") : problemText, "", "---"]
        lines.append(String(localized: "版本：\(env.appVersion)（构建标识 \(env.buildID ?? "-")）"))
        lines.append(String(localized: "系统：\(env.system)"))
        lines.append(dragHint ? String(localized: "诊断包：请把 \(zip.lastPathComponent) 拖到这里") : String(localized: "诊断包：\(zip.lastPathComponent)（见附件）"))
        return lines.joined(separator: "\n")
    }

    private var subject: String {
        let first = problemText.split(separator: "\n").first.map(String.init) ?? ""
        return "ScyllaUI \(env.appVersion)" + String(localized: " 问题反馈") + (first.isEmpty ? "" : String(localized: "：") + first.prefix(60))
    }

    @objc private func sendMail() {
        guard let zip = makeZip() else { return }
        let items: [Any] = [body(zip: zip, dragHint: false), zip]
        if let service = NSSharingService(named: .composeEmail), service.canPerform(withItems: items) {
            service.recipients = [FeedbackChannel.email]
            service.subject = subject
            service.perform(withItems: items)
            scyllauiLog.notice("problem_report sent=mail files=\(self.items.count)")
            dismiss(nil)
            return
        }
        // 没有可用的邮件账户：在 Finder 里显示诊断包，告诉用户发到哪里。
        scyllauiLog.notice("problem_report mail_unavailable")
        NSWorkspace.shared.activateFileViewerSelecting([zip])
        let alert = NSAlert()
        alert.messageText = String(localized: "没有找到可用的邮件账户")
        alert.informativeText = String(localized: "诊断包已在 Finder 中显示。请用您常用的邮箱把它发到 \(FeedbackChannel.email)，或改用「在 GitHub 提交…」。")
        alert.beginSheetModal(for: window!)
    }

    @objc private func sendGitHub() {
        guard let zip = makeZip() else { return }
        NSWorkspace.shared.open(ProblemReport.issueURL(title: subject, body: body(zip: zip, dragHint: true)))
        NSWorkspace.shared.activateFileViewerSelecting([zip])
        scyllauiLog.notice("problem_report sent=github files=\(self.items.count)")
        dismiss(nil)
    }

    @objc func dismiss(_ sender: Any?) {
        guard let window else { return }
        window.sheetParent?.endSheet(window)
        window.orderOut(nil)
        onClose?()
    }
}
