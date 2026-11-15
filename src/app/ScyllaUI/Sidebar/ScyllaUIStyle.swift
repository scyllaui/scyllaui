// 侧栏视觉常量，取自作者的共用视觉规范（未随本仓库公开；适用章节 2026-10-10 用户确认）：
// 1.2 主题色、1.3 语义色（按状态语境）、1.4 ScyllaUI 深色值、1.6 圆角、2.3 ScyllaUI 字号阶梯、7.2 徽标。
// 1.1 灰阶不套固定值，用系统语义色（labelColor 等），以适配毛玻璃侧栏与深色模式。

import AppKit

enum ScyllaUIStyle {
    // 1.2 主题色：Running、「待查看」。
    static let accent = dynamic(light: 0x0a9bf5, dark: 0x3db0f7)
    static let accentBg = dynamicBg(light: 0xe6f4fe, darkFg: 0x3db0f7)
    // 1.3 语义色（状态语境）：出错红、需确认黄、Ready 绿。
    static let error = dynamic(light: 0xd44c47, dark: 0xe5736e)
    static let errorBg = dynamicBg(light: 0xfdebec, darkFg: 0xe5736e)
    static let waiting = dynamic(light: 0xa87608, dark: 0xd9a43a)
    static let waitingBg = dynamicBg(light: 0xfbf3db, darkFg: 0xd9a43a)
    static let ready = dynamic(light: 0x448361, dark: 0x6fb48a)
    static let readyBg = dynamicBg(light: 0xedf3ee, darkFg: 0x6fb48a)
    // ScyllaUI 专用：分支徽标的 Claude 橙（1.4 节 ScyllaUI 约定，只用于项目行的分支）。
    static let branch = dynamic(light: 0xc15f3c, dark: 0xd97757)
    static let branchBg = NSColor(name: nil) { isDark($0) ? rgb(0xd97757, alpha: 0.15) : rgb(0xc15f3c, alpha: 0.12) }
    // 工具识别色（1.4 节 ScyllaUI 约定，2026-10-18 用户确认）：只给工具图标着色，按工具键查表；不在表里的（Shell 等）不着色。
    // Kimi 用自绘新月（开口朝左上、凹处三颗四角星，月亮黄深浅各一版，见资源 tool-kimi；docs/design.md 10.6 节 D12），
    // 按原色显示，不在表里。Claude 用自绘墨迹（D12，2026-10-30 用户选定方案 D），深浅外观都是 #d97757。
    // Antigravity 用彩色像素「A」（D13，2026-10-30 用户改定，资源 tool-antigravity），按原色显示，不在表里。
    // Codex 用紫色圆润色块加 >_（D12，2026-11-02 用户给定，深色外观白 >_、浅色外观黑 >_，资源 tool-codex），按原色显示，不在表里。
    // Grok 用 xAI 官方标志，原样引用、不着识别色（2026-11-15 用户决定），与 OpenCode、Copilot 一样跟随文字色，不在表里。
    static let toolColors: [String: NSColor] = [
        "claude": dynamic(light: 0xd97757, dark: 0xd97757),
    ]
    /// 「最近任务」对话行图标的不透明度。
    static let stoppedIconAlpha: CGFloat = 0.55

    // 1.6 圆角：全站唯一值；状态灯本身是圆形（规范允许的例外）。
    static let radius: CGFloat = 2
    // 7.2 徽标内边距 3 / 8。
    static let badgeInsets = NSEdgeInsets(top: 3, left: 8, bottom: 3, right: 8)

    // 2.3 ScyllaUI 字号阶梯（pt）。2026-10-12 参照 ChatGPT 桌面版 Codex 侧栏改大后再大 1 号（用户确认），原为 11—13。
    static let sectionFont = NSFont.systemFont(ofSize: 14, weight: .semibold)
    static let projectFont = NSFont.systemFont(ofSize: 15, weight: .medium)
    static let toolFont = NSFont.systemFont(ofSize: 15, weight: .semibold)
    static let stateFont = NSFont.systemFont(ofSize: 14, weight: .regular)
    static let detailFont = NSFont.systemFont(ofSize: 14, weight: .regular)
    static let badgeFont = NSFont.systemFont(ofSize: 12, weight: .medium)

    // 固定行高（不用自动行高）。
    static let sectionRowHeight: CGFloat = 34
    static let projectRowHeight: CGFloat = 32
    /// 副标题一行的高度；副标题最多两行（M1 第 6 块），行高按两行固定，单行时下方留空、行距不跳。
    static let subtitleLineHeight: CGFloat = ceil(NSLayoutManager().defaultLineHeight(for: detailFont))
    /// 任务行高 = 上下内边距 + 工具名一行 + 与副标题的间距 + 两行副标题。2026-10-25
    /// 收紧，字号不变，由约 93 pt 收到约 75 pt（docs/design.md 10.5 节决策 8）。
    static let taskRowHeight: CGFloat = 2 * taskRowInset + ceil(NSLayoutManager().defaultLineHeight(for: toolFont)) + 6
        + 2 * subtitleLineHeight
    static let noteRowHeight: CGFloat = 30
    /// 任务行第 1 行上方、第 2 行下方的内边距。
    static let taskRowInset: CGFloat = 8

    // M1 第 5 块（docs/design.md 第 5 节）：工具图标、终端上方的窄标题栏、侧栏宽度。
    static let toolIconSize: CGFloat = 17
    /// 图标到工具名的约束间距：加上文字框内边距后，墨迹间距约 8 pt，与工具名到状态词一致（SidebarCellsTests 实测）。
    static let toolIconGap: CGFloat = 7
    /// 侧栏默认宽度即最小宽度，只能拉宽（2026-10-11 用户确认：收窄对工作没有帮助，要腾空间就收起侧栏）。
    static let sidebarMinWidth: CGFloat = 300
    static let sidebarMaxWidth: CGFloat = 420

    private static func rgb(_ hex: UInt32, alpha: CGFloat = 1) -> NSColor {
        NSColor(srgbRed: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255,
                blue: CGFloat(hex & 0xff) / 255, alpha: alpha)
    }

    private static func isDark(_ a: NSAppearance) -> Bool {
        a.bestMatch(from: [.darkAqua, .aqua, .vibrantDark, .vibrantLight]).map { [.darkAqua, .vibrantDark].contains($0) } ?? false
    }

    private static func dynamic(light: UInt32, dark: UInt32) -> NSColor {
        NSColor(name: nil) { isDark($0) ? rgb(dark) : rgb(light) }
    }

    /// 浅色取规范的底色；深色取前景色 18% 透明度（1.4 节 ScyllaUI 列）。
    private static func dynamicBg(light: UInt32, darkFg: UInt32) -> NSColor {
        NSColor(name: nil) { isDark($0) ? rgb(darkFg, alpha: 0.18) : rgb(light) }
    }
}
