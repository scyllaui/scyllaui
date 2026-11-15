// 侧栏底部的提示栏（M1 第 7 块，docs/design.md 4.7 节「侧栏提示」）：只在有问题时出现，沿用原生控件。
// 只有一行：次要文字色的「状态可能不准：<工具>」（M4b 块 6：Hook 数据真有异常时才出现，只是版本未验证不提示）+ 小按钮「详情…」
// （说明影响、原因与已验证版本）。原来的第一行「状态同步未生效」2026-10-28 去掉，入口只留终端行的「未同步」（10.6 节「块 6 追加」）。
// 内容由侧栏模型算好（HookBanner），这里只显示。

import AppKit

final class HookStatusBar: NSView {
    var onDetails: (([InaccurateItem]) -> Void)?
    private var details: [InaccurateItem] = []

    private let detail = NSTextField(labelWithString: "")
    private let detailButton = NSButton(title: String(localized: "详情…"), target: nil, action: nil)
    private let detailRow = NSStackView()
    /// 没有提示时高度为 0，不占侧栏的空间。
    private var collapse: NSLayoutConstraint!

    override init(frame: NSRect) {
        super.init(frame: frame)
        detail.font = ScyllaUIStyle.detailFont
        detail.textColor = .secondaryLabelColor
        detail.lineBreakMode = .byTruncatingTail
        detail.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        detailButton.bezelStyle = .rounded
        detailButton.controlSize = .small
        detailButton.target = self
        detailButton.action = #selector(showDetails)
        detailButton.setContentHuggingPriority(.required, for: .horizontal)
        detailRow.orientation = .horizontal
        detailRow.spacing = 6
        detailRow.alignment = .centerY
        detailRow.edgeInsets = NSEdgeInsets(top: 8, left: 12, bottom: 8, right: 10)
        for v in [detail, detailButton] as [NSView] { detailRow.addArrangedSubview(v) }
        detailRow.translatesAutoresizingMaskIntoConstraints = false
        addSubview(detailRow)
        collapse = heightAnchor.constraint(equalToConstant: 0)
        NSLayoutConstraint.activate([
            detailRow.topAnchor.constraint(equalTo: topAnchor),
            detailRow.leadingAnchor.constraint(equalTo: leadingAnchor),
            detailRow.trailingAnchor.constraint(lessThanOrEqualTo: trailingAnchor),
            detailRow.bottomAnchor.constraint(equalTo: bottomAnchor).withPriority(.defaultHigh),
        ])
        show(HookBanner())
    }

    required init?(coder: NSCoder) { fatalError() }

    func show(_ b: HookBanner) {
        isHidden = b.isEmpty
        collapse.isActive = b.isEmpty
        details = b.details
        detail.stringValue = String(localized: "状态可能不准：") + b.inaccurate.joined(separator: String(localized: "、"))
        detail.toolTip = String(localized: "这款 Agent 发来的 Hook 数据与 ScyllaUI 认识的不一致，侧栏显示的状态可能不准。版本与原因点「详情…」查看。")
    }

    @objc private func showDetails() { onDetails?(details) }

    override func draw(_ dirtyRect: NSRect) {
        // 上方 1 pt 分隔线（翻转前坐标：顶边为 bounds.maxY）。
        NSColor.separatorColor.setFill()
        NSRect(x: 0, y: bounds.maxY - 1, width: bounds.width, height: 1).fill()
    }
}

private extension NSLayoutConstraint {
    func withPriority(_ p: NSLayoutConstraint.Priority) -> NSLayoutConstraint {
        priority = p
        return self
    }
}
