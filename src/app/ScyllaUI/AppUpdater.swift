// 自动更新（M4b 块 3，docs/design.md 10.6 节「块 3 实现」）：Sparkle 的标准更新器。
// 默认行为（用户确认）：第一次打开不联网；第二次打开由 Sparkle 询问是否自动检查，同意后每天一次；有新版时弹窗，
// 由用户点「安装更新」。更新源、公钥与检查间隔在 ScyllaUI-Info.plist。

#if canImport(Sparkle)
import AppKit
import Sparkle

final class AppUpdater: NSObject, SPUUpdaterDelegate {
    static let shared = AppUpdater()

    /// 测试钩子：本机测试时用 `defaults write <包标识> ScyllaUIUpdateFeedOverride <地址>` 指向本机测试源，测完删除。
    static let feedOverrideKey = "ScyllaUIUpdateFeedOverride"

    private var controller: SPUStandardUpdaterController?

    var updater: SPUUpdater? { controller?.updater }

    /// 启动更新器。从 DMG 或系统挪位的位置运行时不启动（那里替换不了，块 2 已提示先拖到「应用程序」）。
    func start() {
        guard controller == nil else { return }
        controller = SPUStandardUpdaterController(startingUpdater: true, updaterDelegate: self, userDriverDelegate: nil)
        let auto = controller?.updater.automaticallyChecksForUpdates ?? false
        scyllauiLog.notice("updater start auto=\(auto) feed=\(self.controller?.updater.feedURL?.absoluteString ?? "-", privacy: .public)")
    }

    /// 「检查更新…」菜单项与设置里的按钮：交给 SPUStandardUpdaterController（它负责菜单项的可用状态）。
    func menuItem() -> NSMenuItem? {
        guard let controller else { return nil }
        let item = NSMenuItem(title: String(localized: "检查更新…"),
                              action: #selector(SPUStandardUpdaterController.checkForUpdates(_:)), keyEquivalent: "")
        item.target = controller
        return item
    }

    func checkForUpdates() {
        controller?.checkForUpdates(nil)
    }

    // MARK: SPUUpdaterDelegate

    func feedURLString(for updater: SPUUpdater) -> String? {
        UserDefaults.standard.string(forKey: Self.feedOverrideKey).flatMap { $0.isEmpty ? nil : $0 }
    }

    func updater(_ updater: SPUUpdater, didFindValidUpdate item: SUAppcastItem) {
        scyllauiLog.notice("updater found version=\(item.displayVersionString, privacy: .public) build=\(item.versionString, privacy: .public)")
    }

    func updaterDidNotFindUpdate(_ updater: SPUUpdater) {
        scyllauiLog.notice("updater no_update")
    }

    func updater(_ updater: SPUUpdater, didAbortWithError error: Error) {
        scyllauiLog.error("updater abort error=\(error.localizedDescription, privacy: .public)")
        // 系统要管理员授权、用户点了「取消」：Sparkle 什么也不显示（2026-11-12 macOS 26 客人账户实测），这里补一句说明。
        // 授权框的英文首句由 Sparkle 写死、不翻译（InstallerLauncher/SUInstallerLauncher.m），所以正文里点明它。
        let e = error as NSError
        guard e.domain == SUSparkleErrorDomain, e.code == Int(SUError.installationCanceledError.rawValue) else { return }
        let alert = NSAlert()
        alert.useWideLayout()
        alert.messageText = String(localized: "没有安装更新")
        alert.informativeText = String(localized: "安装更新需要管理员授权：ScyllaUI 在「应用程序」里，当前账户无权替换它。刚才弹出的英文授权框（ScyllaUI wants permission to update）就是这一步。\n\n需要更新时，请再点「ScyllaUI › 检查更新…」，由管理员输入名称和密码；或请管理员登录后更新。")
        alert.addButton(withTitle: String(localized: "好"))
        NSApp.activate(ignoringOtherApps: true)
        alert.runModal()
    }

    func updater(_ updater: SPUUpdater, willInstallUpdate item: SUAppcastItem) {
        scyllauiLog.notice("updater install version=\(item.displayVersionString, privacy: .public)")
    }
}
#endif
