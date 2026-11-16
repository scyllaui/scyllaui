// 分隔线的悬停标识（2026-11-06 用户要求，参照 Claude 桌面版的视觉与动效）：鼠标停在分隔线的拖动热区上 0.15 s 后，
// 一条 2 pt 的蓝线用 0.15 s 淡入；移开后淡出；拖动中一直显示。线本身不接收鼠标事件，拖动与双击仍由各自的分隔线处理。
// 侧栏分隔线（SidebarSplitView）与历史提示词栏左边缘（PromptHistoryPanel 的 EdgeHandle）共用。

import AppKit

final class DividerHighlight: NSView {
    static let width: CGFloat = 2
    static let delay: TimeInterval = 0.15
    static let duration: TimeInterval = 0.15

    private var pending: DispatchWorkItem?
    private var hovering = false
    private var dragging = false

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        alphaValue = 0
    }

    required init?(coder: NSCoder) { fatalError() }

    override var wantsUpdateLayer: Bool { true }

    // 深浅外观切换时跟着系统蓝走。
    override func updateLayer() {
        layer?.backgroundColor = NSColor.systemBlue.cgColor
    }

    override func hitTest(_ point: NSPoint) -> NSView? { nil }

    func setHovering(_ on: Bool) {
        hovering = on
        refresh(immediate: false)
    }

    /// 按下即显示（不等延时），松开后若鼠标已离开热区再淡出。
    func setDragging(_ on: Bool) {
        dragging = on
        refresh(immediate: on)
    }

    private func refresh(immediate: Bool) {
        pending?.cancel()
        pending = nil
        let show = hovering || dragging
        if show && !immediate && alphaValue < 1 {
            let work = DispatchWorkItem { [weak self] in self?.fade(to: 1) }
            pending = work
            DispatchQueue.main.asyncAfter(deadline: .now() + Self.delay, execute: work)
        } else {
            fade(to: show ? 1 : 0)
        }
    }

    private func fade(to alpha: CGFloat) {
        NSAnimationContext.runAnimationGroup { ctx in
            ctx.duration = Self.duration
            animator().alphaValue = alpha
        }
    }
}
