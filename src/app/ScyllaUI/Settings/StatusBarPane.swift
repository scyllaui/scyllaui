// 设置「状态栏」（M4a，docs/design.md 10.4 节决策 3）：标题栏状态行每段一个开关与「恢复默认」，即时生效。
// 推理强度只跟在模型后面，模型关闭时不可选。

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
        case .project: return String(localized: "项目名")
        case .git: return String(localized: "分支与改动")
        case .total: return String(localized: "Total（会话累计 token）")
        case .quota5h: return String(localized: "5h 额度")
        case .quota: return String(localized: "7d 周额度")
        case .quotaMonth: return String(localized: "月额度")
        case .api: return String(localized: "按量计费（API）")
        case .model: return String(localized: "模型")
        case .effort: return String(localized: "推理强度")
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
        let note = NSTextField(wrappingLabelWithString: String(localized: "关掉的段不再显示。CPU 与内存都关掉时不再统计，分支与改动关掉时不再读取改动数。"))
        note.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        note.textColor = .secondaryLabelColor
        note.preferredMaxLayoutWidth = 360
        let left: [StatusSegment] = [.project, .git, .total, .quota5h, .quota, .quotaMonth, .api, .model, .effort]
        var rows: [[NSView]] = []
        for (n, s) in left.enumerated() {
            rows.append([n == 0 ? AppearancePane.label(String(localized: "标题栏左侧：")) : NSGridCell.emptyContentView, boxes[s]!])
        }
        rows.append([AppearancePane.label(String(localized: "标题栏右侧：")), boxes[.cpu]!])
        rows.append([NSGridCell.emptyContentView, boxes[.mem]!])
        rows.append([NSGridCell.emptyContentView, note])
        rows.append([NSGridCell.emptyContentView, reset])
        let grid = NSGridView(views: rows)
        grid.column(at: 0).xPlacement = .trailing
        grid.rowAlignment = .firstBaseline
        grid.rowSpacing = 8
        grid.columnSpacing = 8
        grid.row(at: left.count).topPadding = 10
        grid.row(at: rows.count - 2).topPadding = 10
        // 推理强度缩进一级，表示它跟在模型后面（自定义约束只在 xPlacement 为 .none 时生效）。
        grid.cell(for: boxes[.effort]!)?.xPlacement = .none
        grid.cell(for: boxes[.effort]!)?.customPlacementConstraints = [
            boxes[.effort]!.leadingAnchor.constraint(equalTo: boxes[.model]!.leadingAnchor, constant: 20),
        ]
        view = AppearancePane.padded(grid)
        sync()
    }

    private func sync() {
        let hidden = Prefs.statusHidden
        for (s, b) in boxes { b.state = hidden.contains(s) ? .off : .on }
        boxes[.effort]?.isEnabled = !hidden.contains(.model)
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
