// 说明文字较多的提示框统一用宽版（docs/ui-methodology.md 6.1 节「提示框的宽度」，2026-11-17）。

import AppKit

extension NSAlert {
    /// 把提示框固定为 448pt 宽，中英文一致。系统只有窄（260pt）、宽（448pt）两档，按文字排出来的高度自动选：同一段说明，
    /// 英文字多用宽版，中文字少仍是窄版、竖排得很长。苹果没有规定提示框的尺寸，也没有公开的开关可选宽版，
    /// 这里放一个不占高度的附加视图把宽度撑到 448pt（附加视图宽 416pt，两侧边距各 16pt，macOS 27 实测）。
    /// 只用于说明文字多的提示框（窄版里任一语言超过 5 行）；一两句话的确认框沿用系统默认。
    func useWideLayout() {
        accessoryView = NSView(frame: NSRect(x: 0, y: 0, width: 416, height: 0))
    }
}
