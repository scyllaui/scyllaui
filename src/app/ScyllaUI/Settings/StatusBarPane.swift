// 设置「状态栏」（M4a，docs/design.md 10.4 节决策 3）：标题栏状态行每段一个开关与「恢复默认」，即时生效。
// 2026-11-21 项目名、分支与改动、模型、推理强度随状态行去掉，CPU 与内存移到左侧同一行，开关排成一列。

import AppKit

final class StatusBarPane: NSViewController {
    private var boxes: [StatusSegment: NSButton] = [:]

    override init(nibName: NSNib.Name?, bundle: Bundle?) {
        super.init(nibName: nil, bundle: nil)
        title = String(localized: "状态栏")
    }

    required init?(coder: NSCoder) { fatalError() }

    static func name(_ s: StatusSegment) -> String {
        switch s {
        case .total: return String(localized: "Total（会话累计 token）")
        case .quota5h: return String(localized: "5h 额度")
        case .quota: return String(localized: "7d 周额度")
        case .quotaMonth: return String(localized: "月额度")
        case .api: return String(localized: "按量计费（API）")
        case .cpu: return String(localized: "CPU")
        case .mem: return String(localized: "内存")
        }
    }

    override func loadView() {
        for s in StatusSegment.allCases {
            let b = NSButton(checkboxWithTitle: Self.name(s), target: self, action: #selector(toggled(_:)))
            b.identifier = NSUserInterfaceItemIdentifier(s.rawValue)
            boxes[s] = b
        }
        let reset = NSButton(title: String(localized: "恢复默认"), target: self, action: #selector(resetDefaults))
        reset.bezelStyle = .rounded
        let note = NSTextField(wrappingLabelWithString: String(localized: "关掉的段不再显示。CPU 与内存都关掉时不再统计。Claude 的 5h、7d 额度要先在「设置 › Agent」里接入额度。"))
        note.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        note.textColor = .secondaryLabelColor
        note.preferredMaxLayoutWidth = 360
        var rows: [[NSView]] = []
        for (n, s) in StatusSegment.allCases.enumerated() {
            rows.append([n == 0 ? AppearancePane.label(String(localized: "标题栏：")) : NSGridCell.emptyContentView, boxes[s]!])
        }
        rows.append([NSGridCell.emptyContentView, note])
        rows.append([NSGridCell.emptyContentView, reset])
        let grid = NSGridView(views: rows)
        grid.column(at: 0).xPlacement = .trailing
        grid.rowAlignment = .firstBaseline
        grid.rowSpacing = 8
        grid.columnSpacing = 8
        grid.row(at: rows.count - 2).topPadding = 10
        view = AppearancePane.padded(grid)
        sync()
    }

    private func sync() {
        let hidden = Prefs.statusHidden
        for (s, b) in boxes { b.state = hidden.contains(s) ? .off : .on }
    }

    @objc private func toggled(_ sender: NSButton) {
        guard let raw = sender.identifier?.rawValue, let s = StatusSegment(rawValue: raw) else { return }
        var hidden = Prefs.statusHidden
        if sender.state == .on { hidden.remove(s) } else { hidden.insert(s) }
        Prefs.statusHidden = hidden
        sync()
        scyllauiLog.notice("status segments hidden=\(hidden.map(\.rawValue).sorted().joined(separator: ","), privacy: .public)")
    }

    @objc private func resetDefaults() {
        Prefs.statusHidden = []
        sync()
        scyllauiLog.notice("status segments reset")
    }
}
