// libghostty 应用级运行时：配置加载、ghostty_app 创建与回调。
// 回调实现移植自 Ghostty macOS 版 macos/Sources/Ghostty/Ghostty.App.swift 与 Ghostty.Config.swift
// （MIT，固定 commit 34f39002c6e3），只处理 S0 需要的动作，其余返回 false 交还 libghostty。
// 所有 libghostty 的 C 调用集中在 Ghostty/ 目录（docs/design.md 第 11 节风险 1）。

import AppKit
import GhosttyKit
import os

final class GhosttyRuntime {
    private(set) var app: ghostty_app_t?
    /// 生效的配置：启动时读到的那一份；之后以 libghostty 经 CONFIG_CHANGE 交回的为准（M2 第 5 块）。
    private var config: ghostty_config_t?

    /// 生效的配置有变化（重新载入、深浅切换后选了另一个主题）：窗口与侧栏据此重算颜色（docs/design.md 第 5 节「实现（M2 第 5 块）」）。
    static let configDidChange = Notification.Name("ScyllaUIGhosttyConfigDidChange")

    /// 设置里选的主题写在这里，在用户配置与 scyllaui.conf 之后加载。
    static let themeFile = (CoreClient.dataDirectory as NSString).appendingPathComponent("theme.conf")

    /// 与 Ghostty.app 相同：读取用户的默认 Ghostty 配置文件，保证对照条件一致；之后加载 ScyllaUI 自己的覆盖。
    /// 会读文件：除启动外只在后台队列调用。
    private static func makeConfig() -> ghostty_config_t? {
        guard let cfg = ghostty_config_new() else { return nil }
        ghostty_config_load_default_files(cfg)
        ghostty_config_load_recursive_files(cfg)
        // 解除 super+k（清屏）与 super+f（start_search），Cmd+F 归搜索（docs/design.md 10.5 节决策 25）。
        if let path = Bundle.main.path(forResource: "scyllaui", ofType: "conf") {
            ghostty_config_load_file(cfg, path)
        }
        if FileManager.default.fileExists(atPath: themeFile) {
            ghostty_config_load_file(cfg, themeFile)
        }
        ghostty_config_finalize(cfg)
        let diags = ghostty_config_diagnostics_count(cfg)
        for i in 0..<diags {
            let d = ghostty_config_get_diagnostic(cfg, i)
            scyllauiLog.warning("config error: \(String(cString: d.message), privacy: .public)")
        }
        return cfg
    }

    init?() {
        guard let cfg = Self.makeConfig() else { return nil }
        config = cfg

        var runtime = ghostty_runtime_config_s(
            userdata: Unmanaged.passUnretained(self).toOpaque(),
            supports_selection_clipboard: true,
            wakeup_cb: { userdata in GhosttyRuntime.wakeup(userdata) },
            action_cb: { app, target, action in GhosttyRuntime.action(app!, target: target, action: action) },
            read_clipboard_cb: { userdata, loc, state, mimes, mimesLen, list in
                GhosttyRuntime.readClipboard(userdata, location: loc, state: state, mimes: mimes, mimesLen: mimesLen, list: list)
            },
            confirm_read_clipboard_cb: { userdata, confirm, state, request in
                GhosttyRuntime.confirmReadClipboard(userdata, confirm: confirm, state: state, request: request)
            },
            write_clipboard_cb: { userdata, loc, content, len, confirm in
                GhosttyRuntime.writeClipboard(userdata, location: loc, content: content, len: len, confirm: confirm)
            },
            close_surface_cb: { userdata, processAlive in
                GhosttyRuntime.surfaceView(fromUserdata: userdata)?.onClose?(processAlive)
            }
        )
        guard let app = ghostty_app_new(&runtime, cfg) else { return nil }
        self.app = app
        ghostty_app_set_focus(app, NSApp.isActive)

        let center = NotificationCenter.default
        center.addObserver(forName: NSTextInputContext.keyboardSelectionDidChangeNotification, object: nil, queue: .main) { [weak self] _ in
            guard let app = self?.app else { return }
            ghostty_app_keyboard_changed(app)
        }
        center.addObserver(forName: NSApplication.didBecomeActiveNotification, object: nil, queue: .main) { [weak self] _ in
            guard let app = self?.app else { return }
            ghostty_app_set_focus(app, true)
        }
        center.addObserver(forName: NSApplication.didResignActiveNotification, object: nil, queue: .main) { [weak self] _ in
            guard let app = self?.app else { return }
            ghostty_app_set_focus(app, false)
        }
    }

    /// 窗口外观，与 Ghostty.app 相同（移植自 macos/Sources/Helpers/Extensions/NSAppearance+Extension.swift 与
    /// OSColor+Extension.swift）：`window-theme` 为 auto（默认）时按终端背景色亮度决定，light / dark 固定，
    /// 其他（system 等）返回 nil，即跟随系统。docs/design.md 第 5 节。
    /// 终端背景色（Ghostty 配置的 `background`）。侧栏底色由它压暗得到（docs/design.md 第 5 节视觉第二版）。
    var backgroundColor: NSColor? { color("background") }

    /// 终端前景色：M3 起交给后台，断开期间代答 OSC 10（`appearance.set`）。
    var foregroundColor: NSColor? { color("foreground") }

    /// 生效配置的前 16 色调色板（设置「外观」的预览用，M4a）。
    var palette: [NSColor]? {
        guard let config else { return nil }
        var p = ghostty_config_palette_s()
        let key = "palette"
        guard ghostty_config_get(config, &p, key, UInt(key.lengthOfBytes(using: .utf8))) else { return nil }
        return withUnsafeBytes(of: &p.colors) { raw in
            raw.bindMemory(to: ghostty_config_color_s.self).prefix(16).map {
                NSColor(srgbRed: CGFloat($0.r) / 255, green: CGFloat($0.g) / 255, blue: CGFloat($0.b) / 255, alpha: 1)
            }
        }
    }

    /// 生效配置的字号（`font-size`，点）。字体族是可重复项，C 接口读不出，预览只能按设置里选的字体画。
    var fontSize: Double? {
        guard let config else { return nil }
        var v: Float = 0
        let key = "font-size"
        guard ghostty_config_get(config, &v, key, UInt(key.lengthOfBytes(using: .utf8))) else { return nil }
        return Double(v)
    }

    private func color(_ key: String) -> NSColor? {
        guard let config else { return nil }
        var c = ghostty_config_color_s()
        guard ghostty_config_get(config, &c, key, UInt(key.lengthOfBytes(using: .utf8))) else { return nil }
        return NSColor(srgbRed: CGFloat(c.r) / 255, green: CGFloat(c.g) / 255, blue: CGFloat(c.b) / 255, alpha: 1)
    }

    /// 与 Ghostty.app 相同的亮度算法（OSColor+Extension.swift）：> 0.5 为浅色。
    static func isLight(_ color: NSColor) -> Bool {
        guard let c = color.usingColorSpace(.sRGB) else { return false }
        return 0.299 * c.redComponent + 0.587 * c.greenComponent + 0.114 * c.blueComponent > 0.5
    }

    var windowAppearance: NSAppearance? {
        guard let config else { return nil }
        var v: UnsafePointer<Int8>?
        let key = "window-theme"
        guard ghostty_config_get(config, &v, key, UInt(key.lengthOfBytes(using: .utf8))), let v else { return nil }
        switch String(cString: v) {
        case "dark": return NSAppearance(named: .darkAqua)
        case "light": return NSAppearance(named: .aqua)
        case "auto":
            guard let bg = backgroundColor else { return nil }
            return NSAppearance(named: Self.isLight(bg) ? .aqua : .darkAqua)
        default: return nil
        }
    }

    /// 重新载入配置（菜单「重新载入配置」、libghostty 的 reload_config 动作）：后台队列读文件，主线程交给 libghostty，
    /// 由它应用到所有终端并经 CONFIG_CHANGE 交回生效的配置。`then` 在新配置应用到各终端之后于主线程调用。
    func reloadConfig(then: (() -> Void)? = nil) {
        DispatchQueue.global(qos: .userInitiated).async {
            guard let cfg = Self.makeConfig() else { return }
            DispatchQueue.main.async { [weak self] in
                if let app = self?.app {
                    ghostty_app_update_config(app, cfg)
                    scyllauiLog.notice("config reloaded")
                }
                ghostty_config_free(cfg)
                then?()
            }
        }
    }

    /// 把终端的字号恢复为当前配置的字号，并清掉「手动调过字号」的标记（libghostty 的 `reset_font_size`，同 Cmd+0）。
    /// 设置里改字号时对所有终端调用（docs/design.md 10.4 节决策 2）：重新载入配置本身不改手动调过字号的终端。
    static func resetFontSize(_ surface: ghostty_surface_t) {
        let action = "reset_font_size"
        _ = ghostty_surface_binding_action(surface, action, UInt(action.lengthOfBytes(using: .utf8)))
    }

    /// 系统深浅：交给 libghostty 的 app 与各终端，`theme = light:A,dark:B` 据此选定主题。
    func setColorScheme(dark: Bool, surfaces: [ghostty_surface_t]) {
        let scheme = dark ? GHOSTTY_COLOR_SCHEME_DARK : GHOSTTY_COLOR_SCHEME_LIGHT
        if let app { ghostty_app_set_color_scheme(app, scheme) }
        for s in surfaces { ghostty_surface_set_color_scheme(s, scheme) }
    }

    deinit {
        if let app { ghostty_app_free(app) }
        if let config { ghostty_config_free(config) }
    }

    // MARK: Callbacks

    static func wakeup(_ userdata: UnsafeMutableRawPointer?) {
        let state = Unmanaged<GhosttyRuntime>.fromOpaque(userdata!).takeUnretainedValue()
        DispatchQueue.main.async {
            guard let app = state.app else { return }
            ghostty_app_tick(app)
        }
    }

    private static func runtime(of app: ghostty_app_t) -> GhosttyRuntime? {
        guard let userdata = ghostty_app_userdata(app) else { return nil }
        return Unmanaged<GhosttyRuntime>.fromOpaque(userdata).takeUnretainedValue()
    }

    static func surfaceView(fromUserdata userdata: UnsafeMutableRawPointer?) -> TerminalSurfaceView? {
        guard let userdata else { return nil }
        return Unmanaged<TerminalSurfaceView>.fromOpaque(userdata).takeUnretainedValue()
    }

    private static func surfaceView(from target: ghostty_target_s) -> TerminalSurfaceView? {
        guard target.tag == GHOSTTY_TARGET_SURFACE, let surface = target.target.surface else { return nil }
        return surfaceView(fromUserdata: ghostty_surface_userdata(surface))
    }

    static func action(_ app: ghostty_app_t, target: ghostty_target_s, action: ghostty_action_s) -> Bool {
        switch action.tag {
        case GHOSTTY_ACTION_QUIT:
            NSApp.terminate(nil)
            return true

        case GHOSTTY_ACTION_SET_TITLE:
            guard let view = surfaceView(from: target), let ptr = action.action.set_title.title else { return false }
            let title = String(cString: ptr)
            DispatchQueue.main.async { view.onTitle?(title) }
            return true

        case GHOSTTY_ACTION_PWD:
            guard let view = surfaceView(from: target), let ptr = action.action.pwd.pwd else { return false }
            let pwd = String(cString: ptr)
            DispatchQueue.main.async { view.onPwd?(pwd) }
            return true

        case GHOSTTY_ACTION_COMMAND_FINISHED:
            guard let view = surfaceView(from: target) else { return false }
            let v = action.action.command_finished
            DispatchQueue.main.async { view.onCommandFinished?(v.exit_code, v.duration) }
            return true

        case GHOSTTY_ACTION_SHOW_CHILD_EXITED:
            guard let view = surfaceView(from: target) else { return false }
            let v = action.action.child_exited
            DispatchQueue.main.async { view.onChildExited?(v.exit_code) }
            return true

        case GHOSTTY_ACTION_SCROLLBAR:
            guard let view = surfaceView(from: target) else { return false }
            let v = action.action.scrollbar
            DispatchQueue.main.async { view.onScrollbar?(v.total, v.offset, v.len) }
            return true

        case GHOSTTY_ACTION_CELL_SIZE:
            guard let view = surfaceView(from: target) else { return false }
            let v = action.action.cell_size
            let backingSize = NSSize(width: Double(v.width), height: Double(v.height))
            DispatchQueue.main.async { view.cellSize = view.convertFromBacking(backingSize) }
            return true

        case GHOSTTY_ACTION_MOUSE_SHAPE:
            guard let view = surfaceView(from: target) else { return false }
            view.setCursorShape(action.action.mouse_shape)
            return true

        case GHOSTTY_ACTION_MOUSE_VISIBILITY:
            guard let view = surfaceView(from: target) else { return false }
            view.setCursorVisibility(action.action.mouse_visibility == GHOSTTY_MOUSE_VISIBLE)
            return true

        case GHOSTTY_ACTION_OPEN_URL:
            let v = action.action.open_url
            guard let ptr = v.url else { return false }
            let str = String(decoding: Data(bytes: ptr, count: Int(v.len)), as: UTF8.self)
            guard let url = URL(string: str), let scheme = url.scheme?.lowercased(),
                  ["http", "https", "file", "mailto"].contains(scheme) else { return false }
            NSWorkspace.shared.open(url)
            return true

        case GHOSTTY_ACTION_RING_BELL:
            NSApp.requestUserAttention(.informationalRequest)
            return true

        case GHOSTTY_ACTION_DESKTOP_NOTIFICATION:
            // S0 只记录，系统通知在 M2 实现（docs/design.md 第 10 节）
            let v = action.action.desktop_notification
            let title = v.title.map { String(cString: $0) } ?? ""
            let body = v.body.map { String(cString: $0) } ?? ""
            scyllauiLog.info("desktop notification title=\(title, privacy: .public) body=\(body, privacy: .public)")
            return true

        case GHOSTTY_ACTION_CLOSE_WINDOW:
            surfaceView(from: target)?.window?.performClose(nil)
            return true

        case GHOSTTY_ACTION_RELOAD_CONFIG:
            // 与 Ghostty.app 相同（Ghostty.App.swift configReload）：soft（深浅切换等）用现有配置更新、不重读文件，
            // 目标是某个终端时只更新它；不是 soft 的一律整体重读（ScyllaUI 没有单个终端的配置）。
            guard let rt = runtime(of: app) else { return true }
            let soft = action.action.reload_config.soft
            if target.tag == GHOSTTY_TARGET_SURFACE, let surface = target.target.surface {
                if soft, let cfg = rt.config { ghostty_surface_update_config(surface, cfg) } else if !soft { rt.reloadConfig() }
            } else if soft {
                if let cfg = rt.config { ghostty_app_update_config(app, cfg) }
            } else {
                rt.reloadConfig()
            }
            return true

        case GHOSTTY_ACTION_CONFIG_CHANGE:
            // 只取应用级的那一份（各终端另有一份，内容相同）；复制后自己持有，替换旧的。
            guard target.tag == GHOSTTY_TARGET_APP, let rt = runtime(of: app),
                  let c = action.action.config_change.config, let copy = ghostty_config_clone(c) else { return true }
            if let old = rt.config { ghostty_config_free(old) }
            rt.config = copy
            scyllauiLog.notice("config change background=\(rt.backgroundColor?.description ?? "-", privacy: .public)")
            NotificationCenter.default.post(name: configDidChange, object: rt)
            return true

        default:
            return false
        }
    }

    static func readClipboard(
        _ userdata: UnsafeMutableRawPointer?,
        location: ghostty_clipboard_e,
        state: UnsafeMutableRawPointer?,
        mimes: UnsafePointer<UnsafePointer<CChar>?>?,
        mimesLen: Int,
        list: Bool
    ) -> ghostty_clipboard_read_result_e {
        guard let view = surfaceView(fromUserdata: userdata), let surface = view.surface,
              let pasteboard = NSPasteboard.ghostty(location) else {
            return GHOSTTY_CLIPBOARD_READ_UNSUPPORTED
        }

        var contents: [(mime: String, data: Data)] = []
        var seen = Set<String>()
        if let mimes {
            for i in 0..<mimesLen {
                guard let ptr = mimes[i] else { continue }
                let mime = String(cString: ptr)
                guard seen.insert(mime).inserted else { continue }
                guard let data = pasteboard.ghosttyData(forMime: mime) else { continue }
                contents.append((mime, data))
            }
        }
        let available = list ? pasteboard.ghosttyAvailableMimes() : []
        if contents.isEmpty && !list {
            return GHOSTTY_CLIPBOARD_READ_UNAVAILABLE
        }
        completeClipboardRequest(surface, contents: contents, available: available, state: state)
        return GHOSTTY_CLIPBOARD_READ_STARTED
    }

    /// 需要确认的读取（如含换行的不安全粘贴、OSC 52 读取），用简单对话框确认。
    static func confirmReadClipboard(
        _ userdata: UnsafeMutableRawPointer?,
        confirm: UnsafePointer<ghostty_clipboard_confirm_s>?,
        state: UnsafeMutableRawPointer?,
        request: ghostty_clipboard_request_e
    ) {
        guard let view = surfaceView(fromUserdata: userdata), let surface = view.surface else { return }
        guard let confirm else {
            ghostty_surface_deny_clipboard_request(surface, state)
            return
        }
        let c = confirm.pointee
        var reps: [(mime: String, data: Data)] = []
        if let contents = c.contents {
            for i in 0..<c.contents_len {
                let content = contents[i]
                let data = content.len > 0 ? Data(bytes: content.data, count: content.len) : Data()
                reps.append((String(cString: content.mime), data))
            }
        }
        var avail: [String] = []
        if let available = c.available {
            for i in 0..<c.available_len {
                guard let ptr = available[i] else { continue }
                avail.append(String(cString: ptr))
            }
        }
        let display = reps.first(where: { $0.mime == "text/plain" }).flatMap { String(data: $0.data, encoding: .utf8) }
            ?? reps.map { "\($0.mime) (\($0.data.count) bytes)" }.joined(separator: "\n")

        DispatchQueue.main.async {
            let alert = NSAlert()
            alert.messageText = request == GHOSTTY_CLIPBOARD_REQUEST_PASTE ? String(localized: "确认粘贴") : String(localized: "程序请求读取剪贴板")
            alert.informativeText = String(display.prefix(2000))
            alert.addButton(withTitle: String(localized: "允许"))
            alert.addButton(withTitle: String(localized: "拒绝"))
            if alert.runModal() == .alertFirstButtonReturn {
                completeClipboardRequest(surface, contents: reps, available: avail, state: state, confirmed: true)
            } else {
                ghostty_surface_deny_clipboard_request(surface, state)
            }
        }
    }

    private static func completeClipboardRequest(
        _ surface: ghostty_surface_t,
        contents: [(mime: String, data: Data)],
        available: [String],
        state: UnsafeMutableRawPointer?,
        confirmed: Bool = false
    ) {
        var cStrings: [UnsafeMutablePointer<CChar>] = []
        var cDatas: [UnsafeMutableRawPointer] = []
        defer {
            cStrings.forEach { free($0) }
            cDatas.forEach { $0.deallocate() }
        }

        var cContents: [ghostty_clipboard_content_s] = []
        for entry in contents {
            guard let mime = strdup(entry.mime) else { continue }
            cStrings.append(mime)
            let buf = UnsafeMutableRawPointer.allocate(byteCount: max(entry.data.count, 1), alignment: 1)
            cDatas.append(buf)
            entry.data.withUnsafeBytes { src in
                if let base = src.baseAddress { buf.copyMemory(from: base, byteCount: src.count) }
            }
            cContents.append(ghostty_clipboard_content_s(
                mime: mime, data: buf.assumingMemoryBound(to: CChar.self), len: entry.data.count))
        }

        var cAvailable: [UnsafePointer<CChar>?] = []
        for mime in available {
            guard let str = strdup(mime) else { continue }
            cStrings.append(str)
            cAvailable.append(UnsafePointer(str))
        }

        cContents.withUnsafeBufferPointer { contentsBuf in
            cAvailable.withUnsafeBufferPointer { availableBuf in
                var complete = ghostty_clipboard_complete_s(
                    contents: contentsBuf.baseAddress,
                    contents_len: contentsBuf.count,
                    available: availableBuf.baseAddress,
                    available_len: availableBuf.count,
                    confirmed: confirmed,
                    remember: false)
                ghostty_surface_complete_clipboard_request(surface, &complete, state)
            }
        }
    }

    static func writeClipboard(
        _ userdata: UnsafeMutableRawPointer?,
        location: ghostty_clipboard_e,
        content: UnsafePointer<ghostty_clipboard_content_s>?,
        len: Int,
        confirm: Bool
    ) {
        guard let pasteboard = NSPasteboard.ghostty(location), let content, len > 0 else { return }
        var items: [(mime: String, data: Data)] = []
        for i in 0..<len {
            let c = content[i]
            guard let mimePtr = c.mime else { continue }
            let data = c.data.map { Data(bytes: $0, count: c.len) } ?? Data()
            items.append((String(cString: mimePtr), data))
        }
        guard !items.isEmpty else { return }

        let write = {
            let types = items.compactMap { NSPasteboard.PasteboardType(mimeType: $0.mime) }
            pasteboard.declareTypes(types, owner: nil)
            for item in items {
                guard let type = NSPasteboard.PasteboardType(mimeType: item.mime) else { continue }
                pasteboard.setData(item.data, forType: type)
            }
        }
        if !confirm {
            write()
            return
        }
        // OSC 52 写入需要确认时（Ghostty 默认策略），询问用户
        let text = items.first(where: { $0.mime == "text/plain" }).flatMap { String(data: $0.data, encoding: .utf8) } ?? ""
        DispatchQueue.main.async {
            let alert = NSAlert()
            alert.messageText = String(localized: "程序请求写入剪贴板")
            alert.informativeText = String(text.prefix(2000))
            alert.addButton(withTitle: String(localized: "允许"))
            alert.addButton(withTitle: String(localized: "拒绝"))
            if alert.runModal() == .alertFirstButtonReturn { write() }
        }
    }
}
