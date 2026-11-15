// App 菜单开头的「关于」与日志入口（M4b 块 7，docs/design.md 10.6 节 D9）：单独成文件，App 单测可直接检查菜单项属性。

import AppKit

enum AppMenu {
    /// 「关于 ScyllaUI」及紧跟它的替代项「打开日志文件夹」：按住 Option 时「关于」原地换成它（同 Apple 菜单「关于本机」与
    /// 「系统信息」），所有版本都有，1.0.0 之后报问题也能找到日志。
    static func aboutItems(target: AnyObject, about: Selector, logs: Selector) -> [NSMenuItem] {
        let aboutItem = NSMenuItem(title: String(localized: "关于 ScyllaUI"), action: about, keyEquivalent: "")
        aboutItem.target = target
        let logsItem = NSMenuItem(title: String(localized: "打开日志文件夹"), action: logs, keyEquivalent: "")
        logsItem.target = target
        logsItem.keyEquivalentModifierMask = .option
        logsItem.isAlternate = true
        return [aboutItem, logsItem]
    }
}
