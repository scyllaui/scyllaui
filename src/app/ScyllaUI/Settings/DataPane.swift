// 设置「数据」（M4a，docs/design.md 10.4 节决策 7）：已结束对话的保留天数与条数（由后台存于 retention.json），
// 调小时先数出将清理的条数、确认后再改；「清除记录…」写明条数、确认后删除库中全部已结束的对话。
// 只影响 ScyllaUI 自己的库，CLI 自己的会话记录不受影响。

import AppKit

final class DataPane: NSViewController {
    private static let dayChoices = [7, 14, 30, 60, 90]
    private static let countChoices = [50, 100, 200, 500]

    private let core: CoreClient
    private let days = NSPopUpButton(frame: .zero, pullsDown: false)
    private let count = NSPopUpButton(frame: .zero, pullsDown: false)
    private let restore = NSButton(checkboxWithTitle: String(localized: "启动时恢复上次的终端"), target: nil, action: nil)
    /// 后台此刻的保留设置；取到之前两个下拉框不可用。
    private var current: (days: Int, count: Int)?

    init(core: CoreClient) {
        self.core = core
        super.init(nibName: nil, bundle: nil)
        title = String(localized: "数据")
    }

    required init?(coder: NSCoder) { fatalError() }

    override func loadView() {
        for d in Self.dayChoices {
            days.addItem(withTitle: d == 30 ? String(localized: "\(d) 天（默认）") : String(localized: "\(d) 天"))
            days.lastItem?.tag = d
        }
        for n in Self.countChoices {
            count.addItem(withTitle: n == 200 ? String(localized: "\(n) 条（默认）") : String(localized: "\(n) 条"))
            count.lastItem?.tag = n
        }
        for p in [days, count] {
            p.target = self
            p.action = #selector(changed)
            p.isEnabled = false
        }
        let keepNote = Self.note(String(localized: "只影响 ScyllaUI 自己的库；侧栏「最近任务」只显示近 14 天内的 20 条。Claude Code 默认 30 天后删除本地会话，超过它的记录多半已无法恢复。"))
        let clear = NSButton(title: String(localized: "清除记录…"), target: self, action: #selector(clearHistory))
        clear.bezelStyle = .rounded
        let clearNote = Self.note(String(localized: "删除 ScyllaUI 库中全部已结束的对话，正在运行的任务与 Agent 自己的会话记录不受影响。"))
        restore.target = self
        restore.action = #selector(restoreChanged)
        restore.state = Prefs.restoreTerminals ? .on : .off
        let restoreNote = Self.note(String(localized: "重启电脑或后台被结束后，下次打开 ScyllaUI 时把终端开回原来的目录，原来跑着的 Agent 对话接着恢复。退出 App 不影响终端，不需要恢复。"))
        let grid = NSGridView(views: [
            [AppearancePane.label(String(localized: "工作区：")), restore],
            [NSGridCell.emptyContentView, restoreNote],
            [AppearancePane.label(String(localized: "保留天数：")), days],
            [AppearancePane.label(String(localized: "最多保留：")), count],
            [NSGridCell.emptyContentView, keepNote],
            [AppearancePane.label(String(localized: "已结束的对话：")), clear],
            [NSGridCell.emptyContentView, clearNote],
        ])
        grid.column(at: 0).xPlacement = .trailing
        grid.rowAlignment = .firstBaseline
        grid.rowSpacing = 8
        grid.columnSpacing = 8
        grid.row(at: 2).topPadding = 12
        grid.row(at: 5).topPadding = 12
        view = AppearancePane.padded(grid)
    }

    @objc private func restoreChanged() {
        Prefs.restoreTerminals = restore.state == .on
    }

    private static func note(_ text: String) -> NSTextField {
        let note = NSTextField(wrappingLabelWithString: text)
        note.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        note.textColor = .secondaryLabelColor
        note.preferredMaxLayoutWidth = 380
        return note
    }

    override func viewWillAppear() {
        super.viewWillAppear()
        core.request(["type": "retention.get"]) { [weak self] reply in
            guard let r = reply["retention"] as? [String: Any],
                  let d = (r["keep_days"] as? NSNumber)?.intValue, let n = (r["keep_conversations"] as? NSNumber)?.intValue else { return }
            DispatchQueue.main.async { self?.show(days: d, count: n) }
        }
    }

    private func show(days d: Int, count n: Int) {
        current = (d, n)
        days.selectItem(withTag: d)
        count.selectItem(withTag: n)
        days.isEnabled = true
        count.isEnabled = true
    }

    private func revert() {
        if let c = current { show(days: c.days, count: c.count) }
    }

    /// 改了天数或条数：先数出按新设置将清理的条数，有要删的先确认（docs/protocol.md 第 4 节 `retention.set`）。
    @objc private func changed() {
        let d = days.selectedTag(), n = count.selectedTag()
        guard let c = current, (d, n) != c else { return }
        core.request(["type": "retention.set", "keep_days": d, "keep_conversations": n, "dry_run": true]) { [weak self] reply in
            let doomed = (reply["count"] as? NSNumber)?.intValue
            DispatchQueue.main.async {
                guard let self else { return }
                guard let doomed else { return self.failed(reply) }
                if doomed == 0 { return self.apply(days: d, count: n) }
                let alert = NSAlert()
                alert.alertStyle = .warning
                alert.messageText = String(localized: "将立即从 ScyllaUI 的库里删除 \(doomed) 条已结束的对话")
                alert.informativeText = String(localized: "按新的保留设置，超出的记录现在就会删除，删除后无法在 ScyllaUI 中恢复。Agent 自己的会话记录不受影响。")
                alert.addButton(withTitle: String(localized: "删除"))
                alert.addButton(withTitle: String(localized: "取消"))
                alert.buttons.first?.hasDestructiveAction = true
                guard let window = self.view.window else { return self.revert() }
                alert.beginSheetModal(for: window) { response in
                    if response == .alertFirstButtonReturn { self.apply(days: d, count: n) } else { self.revert() }
                }
            }
        }
    }

    private func apply(days d: Int, count n: Int) {
        core.request(["type": "retention.set", "keep_days": d, "keep_conversations": n]) { [weak self] reply in
            let removed = (reply["count"] as? NSNumber)?.intValue ?? 0
            scyllauiLog.notice("retention set days=\(d) count=\(n) ok=\(reply["ok"] as? Bool == true) removed=\(removed)")
            DispatchQueue.main.async {
                guard let self else { return }
                if reply["ok"] as? Bool == true { self.show(days: d, count: n) } else { self.failed(reply) }
            }
        }
    }

    private func failed(_ reply: [String: Any]) {
        revert()
        let alert = NSAlert()
        alert.messageText = String(localized: "没有改成")
        alert.informativeText = ((reply["error"] as? [String: Any])?["message"] as? String) ?? String(localized: "后台没有给出原因。")
        if let window = view.window { alert.beginSheetModal(for: window) }
    }

    /// 「清除记录…」：先数出条数，确认后删除库中全部已结束的对话（docs/protocol.md 第 4 节 `history.clear`）。
    @objc private func clearHistory() {
        core.request(["type": "history.clear", "dry_run": true]) { [weak self] reply in
            let n = (reply["count"] as? NSNumber)?.intValue ?? 0
            DispatchQueue.main.async {
                guard let self, let window = self.view.window else { return }
                let alert = NSAlert()
                guard n > 0 else {
                    alert.messageText = String(localized: "没有已结束的对话记录")
                    alert.beginSheetModal(for: window)
                    return
                }
                alert.useWideLayout()
                alert.alertStyle = .warning
                alert.messageText = String(localized: "清除全部 \(n) 条已结束的对话记录？")
                alert.informativeText = String(localized: "将从 ScyllaUI 的库里立即删除这些任务的标题、提示词、回复摘要与事件记录，删除后无法在 ScyllaUI 中恢复。正在运行的任务与 Agent 自己的会话记录不受影响。")
                alert.addButton(withTitle: String(localized: "清除"))
                alert.addButton(withTitle: String(localized: "取消"))
                alert.buttons.first?.hasDestructiveAction = true
                alert.beginSheetModal(for: window) { response in
                    guard response == .alertFirstButtonReturn else { return }
                    self.core.request(["type": "history.clear"]) { reply in
                        scyllauiLog.notice("history clear ok=\(reply["ok"] as? Bool == true) removed=\((reply["count"] as? NSNumber)?.intValue ?? 0)")
                    }
                }
            }
        }
    }
}
