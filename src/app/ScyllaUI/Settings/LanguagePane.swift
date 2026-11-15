// 设置「语言」（M4a，docs/design.md 10.4 节决策 4）：跟随系统 / 简体中文 / English，写 App 自己域的 `AppleLanguages`
// （跟随系统即删除该键），提示重启后生效，不做即时切换。终端在 scyllauid 里，重启 App 不影响终端与其中的 AI（第 7 节）。

import AppKit

final class LanguagePane: NSViewController {
    private let popup = NSPopUpButton(frame: .zero, pullsDown: false)
    private static let key = "AppleLanguages"
    /// 下拉框各项对应写入的值；nil 为跟随系统。
    private static let choices: [[String]?] = [nil, ["zh-Hans"], ["en"]]

    override init(nibName: NSNib.Name?, bundle: Bundle?) {
        super.init(nibName: nil, bundle: nil)
        title = String(localized: "语言")
    }

    required init?(coder: NSCoder) { fatalError() }

    /// 跟随系统时界面会用的语言：按系统（全局域）的语言偏好在 App 带的语言里选，不受 App 自己域设置的影响。
    static var systemName: String {
        let prefs = UserDefaults.standard.persistentDomain(forName: UserDefaults.globalDomain)?[key] as? [String]
        var langs = Bundle.main.localizations
        if let dev = Bundle.main.developmentLocalization, !langs.contains(dev) { langs.append(dev) }
        let pick = Bundle.preferredLocalizations(from: langs, forPreferences: prefs).first ?? "zh-Hans"
        return pick.hasPrefix("en") ? "English" : String(localized: "简体中文")
    }

    /// App 自己域里设的语言（不含系统与启动参数的设置）；没有设时为 nil，即跟随系统。
    static var appSetting: [String]? {
        guard let id = Bundle.main.bundleIdentifier else { return nil }
        return UserDefaults.standard.persistentDomain(forName: id)?[key] as? [String]
    }

    override func loadView() {
        popup.target = self
        popup.action = #selector(changed(_:))
        popup.addItems(withTitles: [String(localized: "跟随系统（当前：\(Self.systemName)）"), "简体中文", "English"])
        let note = NSTextField(wrappingLabelWithString: String(localized: "重启 ScyllaUI 后生效。终端和其中的 Agent 在后台运行，重启时不受影响。"))
        note.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        note.textColor = .secondaryLabelColor
        note.preferredMaxLayoutWidth = 340
        let grid = NSGridView(views: [
            [AppearancePane.label(String(localized: "界面语言：")), popup],
            [NSGridCell.emptyContentView, note],
        ])
        grid.column(at: 0).xPlacement = .trailing
        grid.rowAlignment = .firstBaseline
        grid.rowSpacing = 10
        grid.columnSpacing = 8
        view = AppearancePane.padded(grid)
        let setting = Self.appSetting
        popup.selectItem(at: setting == nil ? 0 : (setting?.first?.hasPrefix("en") == true ? 2 : 1))
    }

    @objc private func changed(_ sender: NSPopUpButton) {
        let choice = Self.choices[max(0, sender.indexOfSelectedItem)]
        guard choice != Self.appSetting else { return }
        if let choice {
            UserDefaults.standard.set(choice, forKey: Self.key)
        } else {
            UserDefaults.standard.removeObject(forKey: Self.key)
        }
        scyllauiLog.notice("language set \(choice?.joined(separator: ",") ?? "system", privacy: .public)")
        let alert = NSAlert()
        alert.messageText = String(localized: "重启 ScyllaUI 后生效")
        alert.informativeText = String(localized: "终端和其中的 Agent 在后台运行，重启时不受影响，重新打开后自动接回。")
        alert.addButton(withTitle: String(localized: "立即重启"))
        alert.addButton(withTitle: String(localized: "稍后"))
        guard let window = view.window else { return }
        alert.beginSheetModal(for: window) { response in
            if response == .alertFirstButtonReturn { AppRelaunch.relaunch() }
        }
    }
}

/// 重启 App：起一个 /bin/sh 等当前进程退出后 `open` 本 App 包，然后正常退出。隔离的测试实例显式带上数据目录、CLI
/// 配置目录与测试夹具的环境变量，避免重启后连到真实后台（docs/design.md 10.4 节决策 4）。
enum AppRelaunch {
    static func relaunch() {
        let pid = ProcessInfo.processInfo.processIdentifier
        var args: [String] = []
        let env = ProcessInfo.processInfo.environment
        for k in ["SCYLLAUI_DATA_DIR", "SCYLLAUI_CLI_HOME", "SCYLLAUI_FIXTURE_TERMINALS"] {
            if let v = env[k], !v.isEmpty { args += ["--env", "\(k)=\(v)"] }
        }
        args.append(Bundle.main.bundlePath)
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/bin/sh")
        p.arguments = ["-c", "while /bin/kill -0 \(pid) 2>/dev/null; do /bin/sleep 0.2; done; exec /usr/bin/open \"$@\"", "sh"] + args
        do {
            try p.run()
            scyllauiLog.notice("relaunch scheduled pid=\(pid)")
            NSApp.terminate(nil)
        } catch {
            scyllauiLog.error("重启失败：\(error.localizedDescription, privacy: .public)")
        }
    }
}
