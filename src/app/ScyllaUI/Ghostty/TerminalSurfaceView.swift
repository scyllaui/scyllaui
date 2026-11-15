// 移植自 Ghostty macOS 版（MIT，Copyright (c) 2024 Mitchell Hashimoto, Ghostty contributors），
// 固定 commit 34f39002c6e3，来源 macos/Sources/Ghostty/Surface View/SurfaceView_AppKit.swift。
// 按键、输入法（NSTextInputClient）、鼠标、滚动、拖放、尺寸与缩放的逻辑保持原样；
// 去掉了 SwiftUI / Combine 状态、分屏、搜索、检查器、QuickLook、通知、Secure Input、滚动条。
// 许可证全文见 src/app/THIRD_PARTY_NOTICES.md。

import AppKit
import CoreText
import GhosttyKit

final class TerminalSurfaceView: NSView {
    /// 事件回调，供外壳更新界面（S0 只用于调试面板）
    var onTitle: ((String) -> Void)?
    var onPwd: ((String) -> Void)?
    var onCommandFinished: ((Int16, UInt64) -> Void)?
    var onChildExited: ((UInt32) -> Void)?
    var onClose: ((Bool) -> Void)?
    /// 尺寸变化后回调；ScyllaUI 用于尺寸日志（GeometryLog.swift）。
    var onGridSize: ((ghostty_surface_size_s) -> Void)?
    /// 滚动条状态（总行数、视口偏移、视口行数），渲染时有变化才来；ScyllaUI 用于尺寸日志（GeometryLog.swift）。
    var onScrollbar: ((UInt64, UInt64, UInt64) -> Void)?

    private(set) var surface: ghostty_surface_t?
    var cellSize: CGSize = .zero {
        didSet { if cellSize != oldValue { onCellSize?(cellSize) } }
    }
    /// 字符格尺寸变化（点）；ScyllaUI 据此设最小窗口尺寸。
    var onCellSize: ((CGSize) -> Void)?
    private(set) var focused: Bool = true

    private var markedText = NSMutableAttributedString()
    private var prevPressureStage: Int = 0
    private var keyTextAccumulator: [String]?
    private var leadSurrogate: LeadSurrogate?
    private var suppressNextLeftMouseUp = false
    private var contentSizeBacking: NSSize?
    private var contentSize: NSSize {
        get { contentSizeBacking ?? frame.size }
        set { contentSizeBacking = newValue }
    }
    private var eventMonitor: Any?
    private var pointer: NSCursor = .iBeam

    /// See SurfaceView_AppKit.swift `lastPerformKeyEvent`.
    var lastPerformKeyEvent: TimeInterval?

    override var acceptsFirstResponder: Bool { true }

    /// `environment` 追加到子进程环境（ScyllaUI：SCYLLAUI_PANE_ID、SCYLLAUI_SOCK 等，docs/design.md 4.4 节）。
    /// `command`：窗格运行的命令（ScyllaUI 为 `scyllaui attach --terminal`）；为 nil 时与 Ghostty 相同，启动登录 Shell。
    /// `initialInput`：建终端时原样写进 PTY 的字节（Ghostty 的 `input` 配置项所用机制），Shell 在首个提示符读取；
    /// 用于「新建 AI」代输启动命令（docs/design.md 第 5 节）。
    init(app: ghostty_app_t, workingDirectory: String?, environment: [String: String] = [:],
         command: String? = nil, initialInput: String? = nil) {
        super.init(frame: NSRect(x: 0, y: 0, width: 800, height: 600))

        eventMonitor = NSEvent.addLocalMonitorForEvents(matching: [.keyUp, .leftMouseDown]) { [weak self] event in
            self?.localEventHandler(event)
        }

        var cfg = ghostty_surface_config_new()
        cfg.userdata = Unmanaged.passUnretained(self).toOpaque()
        cfg.platform_tag = GHOSTTY_PLATFORM_MACOS
        cfg.platform = ghostty_platform_u(macos: ghostty_platform_macos_s(
            nsview: Unmanaged.passUnretained(self).toOpaque()))
        cfg.scale_factor = Double(NSScreen.main?.backingScaleFactor ?? 2)
        cfg.font_size = 0
        cfg.context = GHOSTTY_SURFACE_CONTEXT_WINDOW
        // 与 Ghostty 官方 SurfaceConfiguration.withCValue 相同：字符串只需在 ghostty_surface_new 调用期间有效。
        var envVars = environment.map { ghostty_env_var_s(key: strdup($0.key), value: strdup($0.value)) }
        defer { envVars.forEach { free(UnsafeMutablePointer(mutating: $0.key)); free(UnsafeMutablePointer(mutating: $0.value)) } }
        let created: ghostty_surface_t? = (workingDirectory ?? "").withCString { wd in
            if workingDirectory != nil { cfg.working_directory = wd }
            return (command ?? "").withCString { cmd in
                if command != nil { cfg.command = cmd }
                return (initialInput ?? "").withCString { input in
                    if initialInput != nil { cfg.initial_input = input }
                    return envVars.withUnsafeMutableBufferPointer { buf in
                        cfg.env_vars = buf.baseAddress
                        cfg.env_var_count = buf.count
                        return ghostty_surface_new(app, &cfg)
                    }
                }
            }
        }
        surface = created

        updateTrackingAreas()
        registerForDraggedTypes([.string, .fileURL])
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) is not supported") }

    deinit {
        if let eventMonitor { NSEvent.removeMonitor(eventMonitor) }
        if let surface { ghostty_surface_free(surface) }
    }

    // MARK: 前台进程（S0 验证项）

    var foregroundPID: pid_t? {
        guard let surface else { return nil }
        let pid = ghostty_surface_foreground_pid(surface)
        return pid > 0 ? pid_t(pid) : nil
    }

    var ttyName: String? {
        guard let surface else { return nil }
        let s = ghostty_surface_tty_name(surface)
        defer { ghostty_string_free(s) }
        guard let ptr = s.ptr, s.len > 0 else { return nil }
        return String(decoding: UnsafeRawBufferPointer(start: ptr, count: Int(s.len)), as: UTF8.self)
    }

    // MARK: 光标

    func setCursorShape(_ shape: ghostty_action_mouse_shape_e) {
        switch shape {
        case GHOSTTY_MOUSE_SHAPE_DEFAULT: pointer = .arrow
        case GHOSTTY_MOUSE_SHAPE_TEXT: pointer = .iBeam
        case GHOSTTY_MOUSE_SHAPE_GRAB: pointer = .openHand
        case GHOSTTY_MOUSE_SHAPE_GRABBING: pointer = .closedHand
        case GHOSTTY_MOUSE_SHAPE_POINTER: pointer = .pointingHand
        case GHOSTTY_MOUSE_SHAPE_EW_RESIZE: pointer = .resizeLeftRight
        case GHOSTTY_MOUSE_SHAPE_NS_RESIZE: pointer = .resizeUpDown
        case GHOSTTY_MOUSE_SHAPE_VERTICAL_TEXT: pointer = .iBeamCursorForVerticalLayout
        case GHOSTTY_MOUSE_SHAPE_CONTEXT_MENU: pointer = .contextualMenu
        case GHOSTTY_MOUSE_SHAPE_CROSSHAIR: pointer = .crosshair
        case GHOSTTY_MOUSE_SHAPE_NOT_ALLOWED: pointer = .operationNotAllowed
        default: return
        }
        window?.invalidateCursorRects(for: self)
    }

    func setCursorVisibility(_ visible: Bool) {
        NSCursor.setHiddenUntilMouseMoves(!visible)
    }

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: pointer)
    }

    // MARK: Local Events

    private func localEventHandler(_ event: NSEvent) -> NSEvent? {
        switch event.type {
        case .keyUp: return localEventKeyUp(event)
        case .leftMouseDown: return localEventLeftMouseDown(event)
        default: return event
        }
    }

    private func localEventLeftMouseDown(_ event: NSEvent) -> NSEvent? {
        guard let window, event.window != nil, window == event.window else { return event }
        guard let location = window.contentView?.convert(event.locationInWindow, from: nil) else { return event }
        guard window.contentView?.hitTest(location) == self else { return event }

        suppressNextLeftMouseUp = false
        guard window.firstResponder !== self else { return event }

        // A click that only moves focus is not forwarded to the terminal.
        if NSApp.isActive && window.isKeyWindow {
            window.makeFirstResponder(self)
            suppressNextLeftMouseUp = true
            return nil
        }

        window.makeFirstResponder(self)
        return event
    }

    private func localEventKeyUp(_ event: NSEvent) -> NSEvent? {
        // Command keyUp events are never sent to the normal responder chain.
        if !event.modifierFlags.contains(.command) { return event }
        guard focused else { return event }
        self.keyUp(with: event)
        return nil
    }

    // MARK: Focus & Size

    func focusDidChange(_ focused: Bool) {
        guard let surface else { return }
        guard self.focused != focused else { return }
        self.focused = focused
        if !focused { suppressNextLeftMouseUp = false }
        ghostty_surface_set_focus(surface, focused)
    }

    override func becomeFirstResponder() -> Bool {
        let result = super.becomeFirstResponder()
        if result { focusDidChange(true) }
        return result
    }

    override func resignFirstResponder() -> Bool {
        let result = super.resignFirstResponder()
        if result { focusDidChange(false) }
        return result
    }

    override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        sizeDidChange(newSize)
    }

    func sizeDidChange(_ size: CGSize) {
        let scaledSize = convertToBacking(size)
        setSurfaceSize(width: UInt32(scaledSize.width), height: UInt32(scaledSize.height))
        contentSize = size
    }

    private func setSurfaceSize(width: UInt32, height: UInt32) {
        guard let surface, width > 0, height > 0 else { return }
        ghostty_surface_set_size(surface, width, height)
        onGridSize?(ghostty_surface_size(surface))
    }

    /// 多终端切换（M1 第 4 块）：切走的终端移出视图层但进程照常运行。移出时注销窗口通知、取消聚焦并暂停渲染，
    /// 加回时重新登记并按窗口可见性恢复渲染（与 Ghostty 分屏缩放时移出 / 加回视图的处理一致）。
    override func viewWillMove(toWindow newWindow: NSWindow?) {
        super.viewWillMove(toWindow: newWindow)
        guard let old = window, newWindow !== old else { return }
        NotificationCenter.default.removeObserver(self, name: NSWindow.didChangeScreenNotification, object: old)
        NotificationCenter.default.removeObserver(self, name: NSWindow.didChangeOcclusionStateNotification, object: old)
        if newWindow == nil, let surface {
            focusDidChange(false)
            ghostty_surface_set_occlusion(surface, false)
        }
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        guard let window else { return }
        if let surface { ghostty_surface_set_occlusion(surface, window.occlusionState.contains(.visible)) }
        NotificationCenter.default.addObserver(
            self, selector: #selector(windowDidChangeScreen(_:)),
            name: NSWindow.didChangeScreenNotification, object: window)
        NotificationCenter.default.addObserver(
            self, selector: #selector(windowDidChangeOcclusion(_:)),
            name: NSWindow.didChangeOcclusionStateNotification, object: window)
        windowDidChangeScreen(Notification(name: NSWindow.didChangeScreenNotification))
    }

    @objc private func windowDidChangeScreen(_ notification: Notification) {
        guard let screen = window?.screen, let surface else { return }
        if let id = screen.deviceDescription[NSDeviceDescriptionKey("NSScreenNumber")] as? UInt32 {
            ghostty_surface_set_display_id(surface, id)
        }
        DispatchQueue.main.async { [weak self] in self?.viewDidChangeBackingProperties() }
    }

    @objc private func windowDidChangeOcclusion(_ notification: Notification) {
        guard let window, let surface else { return }
        ghostty_surface_set_occlusion(surface, window.occlusionState.contains(.visible))
    }

    override func updateTrackingAreas() {
        trackingAreas.forEach { removeTrackingArea($0) }
        addTrackingArea(NSTrackingArea(
            rect: frame,
            options: [.mouseEnteredAndExited, .mouseMoved, .inVisibleRect, .activeAlways],
            owner: self,
            userInfo: nil))
    }

    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        if let window {
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            layer?.contentsScale = window.backingScaleFactor
            CATransaction.commit()
        }
        guard let surface else { return }
        let fbFrame = convertToBacking(frame)
        let xScale = fbFrame.size.width / frame.size.width
        let yScale = fbFrame.size.height / frame.size.height
        ghostty_surface_set_content_scale(surface, xScale, yScale)
        let scaledSize = convertToBacking(contentSize)
        setSurfaceSize(width: UInt32(scaledSize.width), height: UInt32(scaledSize.height))
    }

    // MARK: Mouse

    override func mouseDown(with event: NSEvent) {
        guard let surface else { return }
        ghostty_surface_mouse_button(surface, GHOSTTY_MOUSE_PRESS, GHOSTTY_MOUSE_LEFT, GhosttyInput.ghosttyMods(event.modifierFlags))
    }

    override func mouseUp(with event: NSEvent) {
        if suppressNextLeftMouseUp {
            suppressNextLeftMouseUp = false
            return
        }
        prevPressureStage = 0
        guard let surface else { return }
        ghostty_surface_mouse_button(surface, GHOSTTY_MOUSE_RELEASE, GHOSTTY_MOUSE_LEFT, GhosttyInput.ghosttyMods(event.modifierFlags))
        ghostty_surface_mouse_pressure(surface, 0, 0)
    }

    private static func mouseButton(_ number: Int) -> ghostty_input_mouse_button_e {
        switch number {
        case 0: GHOSTTY_MOUSE_LEFT
        case 1: GHOSTTY_MOUSE_RIGHT
        case 2: GHOSTTY_MOUSE_MIDDLE
        case 3: GHOSTTY_MOUSE_EIGHT
        case 4: GHOSTTY_MOUSE_NINE
        case 5: GHOSTTY_MOUSE_SIX
        case 6: GHOSTTY_MOUSE_SEVEN
        case 7: GHOSTTY_MOUSE_FOUR
        case 8: GHOSTTY_MOUSE_FIVE
        case 9: GHOSTTY_MOUSE_TEN
        case 10: GHOSTTY_MOUSE_ELEVEN
        default: GHOSTTY_MOUSE_UNKNOWN
        }
    }

    override func otherMouseDown(with event: NSEvent) {
        guard let surface else { return }
        ghostty_surface_mouse_button(surface, GHOSTTY_MOUSE_PRESS, Self.mouseButton(event.buttonNumber), GhosttyInput.ghosttyMods(event.modifierFlags))
    }

    override func otherMouseUp(with event: NSEvent) {
        guard let surface else { return }
        ghostty_surface_mouse_button(surface, GHOSTTY_MOUSE_RELEASE, Self.mouseButton(event.buttonNumber), GhosttyInput.ghosttyMods(event.modifierFlags))
    }

    override func rightMouseDown(with event: NSEvent) {
        guard let surface else { return super.rightMouseDown(with: event) }
        if ghostty_surface_mouse_button(surface, GHOSTTY_MOUSE_PRESS, GHOSTTY_MOUSE_RIGHT, GhosttyInput.ghosttyMods(event.modifierFlags)) {
            return
        }
        super.rightMouseDown(with: event)
    }

    override func rightMouseUp(with event: NSEvent) {
        guard let surface else { return super.rightMouseUp(with: event) }
        if ghostty_surface_mouse_button(surface, GHOSTTY_MOUSE_RELEASE, GHOSTTY_MOUSE_RIGHT, GhosttyInput.ghosttyMods(event.modifierFlags)) {
            return
        }
        super.rightMouseUp(with: event)
    }

    override func mouseEntered(with event: NSEvent) {
        super.mouseEntered(with: event)
        guard let surface else { return }
        let pos = convert(event.locationInWindow, from: nil)
        ghostty_surface_mouse_pos(surface, pos.x, frame.height - pos.y, GhosttyInput.ghosttyMods(event.modifierFlags))
    }

    override func mouseExited(with event: NSEvent) {
        guard let surface else { return }
        if NSEvent.pressedMouseButtons != 0 { return }
        // Negative values indicate cursor has left the viewport
        ghostty_surface_mouse_pos(surface, -1, -1, GhosttyInput.ghosttyMods(event.modifierFlags))
    }

    override func mouseMoved(with event: NSEvent) {
        guard let surface else { return }
        let pos = convert(event.locationInWindow, from: nil)
        ghostty_surface_mouse_pos(surface, pos.x, frame.height - pos.y, GhosttyInput.ghosttyMods(event.modifierFlags))
    }

    override func mouseDragged(with event: NSEvent) { mouseMoved(with: event) }
    override func rightMouseDragged(with event: NSEvent) { mouseMoved(with: event) }
    override func otherMouseDragged(with event: NSEvent) { mouseMoved(with: event) }

    override func scrollWheel(with event: NSEvent) {
        guard let surface else { return }
        var x = event.scrollingDeltaX
        var y = event.scrollingDeltaY
        let precision = event.hasPreciseScrollingDeltas
        if precision {
            x *= 2
            y *= 2
        }
        // Scroll mods: bit 0 = precision, bits 1-3 = momentum phase (Ghostty.Input.ScrollMods)
        let momentum: Int32 = switch event.momentumPhase {
        case .began: 1
        case .stationary: 2
        case .changed: 3
        case .ended: 4
        case .cancelled: 5
        case .mayBegin: 6
        default: 0
        }
        let mods: Int32 = (precision ? 1 : 0) | (momentum << 1)
        ghostty_surface_mouse_scroll(surface, x, y, mods)
    }

    override func pressureChange(with event: NSEvent) {
        guard let surface else { return }
        ghostty_surface_mouse_pressure(surface, UInt32(event.stage), Double(event.pressure))
        guard prevPressureStage < 2 else { return }
        prevPressureStage = event.stage
    }

    // MARK: Keyboard

    override func keyDown(with event: NSEvent) {
        guard let surface else {
            interpretKeyEvents([event])
            return
        }

        // Translate the mods (maybe) to handle configs such as option-as-alt
        let translationModsGhostty = GhosttyInput.eventModifierFlags(
            mods: ghostty_surface_key_translation_mods(surface, GhosttyInput.ghosttyMods(event.modifierFlags)))

        var translationMods = event.modifierFlags
        for flag in [NSEvent.ModifierFlags.shift, .control, .option, .command] {
            if translationModsGhostty.contains(flag) {
                translationMods.insert(flag)
            } else {
                translationMods.remove(flag)
            }
        }

        // We MUST reuse the old event if mods are equal (keeps Korean input working).
        let translationEvent: NSEvent
        if translationMods == event.modifierFlags {
            translationEvent = event
        } else {
            translationEvent = NSEvent.keyEvent(
                with: event.type,
                location: event.locationInWindow,
                modifierFlags: translationMods,
                timestamp: event.timestamp,
                windowNumber: event.windowNumber,
                context: nil,
                characters: event.characters(byApplyingModifiers: translationMods) ?? "",
                charactersIgnoringModifiers: event.charactersIgnoringModifiers ?? "",
                isARepeat: event.isARepeat,
                keyCode: event.keyCode
            ) ?? event
        }

        let action = event.isARepeat ? GHOSTTY_ACTION_REPEAT : GHOSTTY_ACTION_PRESS

        keyTextAccumulator = []
        defer { keyTextAccumulator = nil }

        let markedTextBefore = markedText.length > 0
        let keyboardIdBefore: String? = markedTextBefore ? nil : GhosttyInput.keyboardLayoutID

        lastPerformKeyEvent = nil

        interpretKeyEvents([translationEvent])

        // If our keyboard changed from this we just assume an input method grabbed it.
        if !markedTextBefore && keyboardIdBefore != GhosttyInput.keyboardLayoutID {
            return
        }

        syncPreedit(clearIfNeeded: markedTextBefore)

        let composing = markedText.length > 0 || markedTextBefore

        if markedTextBefore, let list = keyTextAccumulator, list.count > 0 {
            for text in list {
                if Self.shouldSuppressComposingControlInput(text, composing: composing) { continue }
                _ = committedTextAction(action, text: text)
            }
            if shouldReplayCommittedPreeditKey(translationEvent) {
                _ = keyAction(action, event: event, translationEvent: translationEvent, composing: false)
            }
            return
        }

        if let list = keyTextAccumulator, list.count > 0 {
            for text in list {
                if Self.shouldSuppressComposingControlInput(text, composing: composing) { continue }
                _ = keyAction(action, event: event, translationEvent: translationEvent, text: text)
            }
        } else {
            if Self.shouldSuppressComposingControlInput(event.characters, composing: composing) { return }
            _ = keyAction(
                action,
                event: event,
                translationEvent: translationEvent,
                text: translationEvent.ghosttyCharacters,
                composing: composing)
        }
    }

    override func keyUp(with event: NSEvent) {
        _ = keyAction(GHOSTTY_ACTION_RELEASE, event: event)
    }

    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        guard event.type == .keyDown else { return false }
        if !focused { return false }

        // If this is a binding then we want to perform it.
        if let surface {
            var ghosttyEvent = event.ghosttyKeyEvent(GHOSTTY_ACTION_PRESS)
            var flags = ghostty_binding_flags_e(0)
            let isBinding = (event.characters ?? "").withCString { ptr in
                ghosttyEvent.text = ptr
                return ghostty_surface_key_is_binding(surface, ghosttyEvent, &flags)
            }
            if isBinding {
                // 与 Ghostty.app 相同（SurfaceView_AppKit.swift performKeyEquivalent）：被 libghostty 消费、且不是
                // all / performable 的绑定先试菜单项，使 Cmd+T / Cmd+W / Cmd+1…9 走 ScyllaUI 的「终端」菜单（M1 第 4 块）。
                let menuFirst = flags.rawValue & GHOSTTY_BINDING_FLAGS_CONSUMED.rawValue != 0
                    && flags.rawValue & (GHOSTTY_BINDING_FLAGS_ALL.rawValue | GHOSTTY_BINDING_FLAGS_PERFORMABLE.rawValue) == 0
                if menuFirst, NSApp.mainMenu?.performKeyEquivalent(with: event) == true {
                    return true
                }
                keyDown(with: event)
                return true
            }
        }

        let equivalent: String
        switch event.charactersIgnoringModifiers {
        case "\r":
            if !event.modifierFlags.contains(.control) { return false }
            equivalent = "\r"

        case "/":
            if !event.modifierFlags.contains(.control) ||
                !event.modifierFlags.isDisjoint(with: [.shift, .command, .option]) {
                return false
            }
            equivalent = "_"

        default:
            if event.timestamp == 0 { return false }
            if !event.modifierFlags.contains(.command) && !event.modifierFlags.contains(.control) {
                lastPerformKeyEvent = nil
                return false
            }
            if let lastPerformKeyEvent {
                self.lastPerformKeyEvent = nil
                if lastPerformKeyEvent == event.timestamp {
                    equivalent = event.characters ?? ""
                    break
                }
            }
            lastPerformKeyEvent = event.timestamp
            return false
        }

        guard let finalEvent = NSEvent.keyEvent(
            with: .keyDown,
            location: event.locationInWindow,
            modifierFlags: event.modifierFlags,
            timestamp: event.timestamp,
            windowNumber: event.windowNumber,
            context: nil,
            characters: equivalent,
            charactersIgnoringModifiers: equivalent,
            isARepeat: event.isARepeat,
            keyCode: event.keyCode
        ) else { return false }
        keyDown(with: finalEvent)
        return true
    }

    override func flagsChanged(with event: NSEvent) {
        let mod: UInt32
        switch event.keyCode {
        case 0x39: mod = GHOSTTY_MODS_CAPS.rawValue
        case 0x38, 0x3C: mod = GHOSTTY_MODS_SHIFT.rawValue
        case 0x3B, 0x3E: mod = GHOSTTY_MODS_CTRL.rawValue
        case 0x3A, 0x3D: mod = GHOSTTY_MODS_ALT.rawValue
        case 0x37, 0x36: mod = GHOSTTY_MODS_SUPER.rawValue
        default: return
        }

        if hasMarkedText() { return }

        let mods = GhosttyInput.ghosttyMods(event.modifierFlags)
        var action = GHOSTTY_ACTION_RELEASE
        if mods.rawValue & mod != 0 {
            let sidePressed: Bool
            switch event.keyCode {
            case 0x3C: sidePressed = event.modifierFlags.rawValue & UInt(NX_DEVICERSHIFTKEYMASK) != 0
            case 0x3E: sidePressed = event.modifierFlags.rawValue & UInt(NX_DEVICERCTLKEYMASK) != 0
            case 0x3D: sidePressed = event.modifierFlags.rawValue & UInt(NX_DEVICERALTKEYMASK) != 0
            case 0x36: sidePressed = event.modifierFlags.rawValue & UInt(NX_DEVICERCMDKEYMASK) != 0
            default: sidePressed = true
            }
            if sidePressed { action = GHOSTTY_ACTION_PRESS }
        }

        _ = keyAction(action, event: event)
    }

    private func keyAction(
        _ action: ghostty_input_action_e,
        event: NSEvent,
        translationEvent: NSEvent? = nil,
        text: String? = nil,
        composing: Bool = false
    ) -> Bool {
        guard let surface else { return false }

        var key_ev = event.ghosttyKeyEvent(action, translationMods: translationEvent?.modifierFlags)
        key_ev.composing = composing

        if let text = text?.keyEventText {
            return text.withCString { ptr in
                key_ev.text = ptr
                return ghostty_surface_key(surface, key_ev)
            }
        } else {
            return ghostty_surface_key(surface, key_ev)
        }
    }

    private func shouldReplayCommittedPreeditKey(_ event: NSEvent) -> Bool {
        // kVK_LeftArrow 0x7B, kVK_RightArrow 0x7C, kVK_DownArrow 0x7D, kVK_UpArrow 0x7E
        switch event.keyCode {
        case 0x7C, 0x7D, 0x7E:
            return true
        case 0x7B:
            return !event.modifierFlags.isDisjoint(with: [.shift, .control, .option, .command])
        default:
            return false
        }
    }

    private func committedTextAction(_ action: ghostty_input_action_e, text: String) -> Bool {
        guard let surface else { return false }
        var key_ev = ghostty_input_key_s()
        key_ev.action = action
        key_ev.keycode = 0
        key_ev.text = nil
        key_ev.composing = false
        key_ev.mods = GHOSTTY_MODS_NONE
        key_ev.consumed_mods = GHOSTTY_MODS_NONE
        key_ev.unshifted_codepoint = 0
        return text.withCString { ptr in
            key_ev.text = ptr
            return ghostty_surface_key(surface, key_ev)
        }
    }

    // MARK: Menu Handlers

    private func bindingAction(_ action: String) {
        guard let surface else { return }
        _ = ghostty_surface_binding_action(surface, action, UInt(action.lengthOfBytes(using: .utf8)))
    }

    @IBAction func copy(_ sender: Any?) { bindingAction("copy_to_clipboard") }
    @IBAction func paste(_ sender: Any?) { bindingAction("paste_from_clipboard") }
    @IBAction override func selectAll(_ sender: Any?) { bindingAction("select_all") }

    // MARK: Text

    func sendText(_ text: String) {
        guard let surface else { return }
        let len = text.utf8CString.count
        if len == 0 { return }
        text.withCString { ptr in
            ghostty_surface_text(surface, ptr, UInt(len - 1))
        }
    }

    /// True when `text` is a single C0 control character arriving while the IME is composing.
    static func shouldSuppressComposingControlInput(_ text: String?, composing: Bool) -> Bool {
        guard composing, let text else { return false }
        let scalars = text.unicodeScalars
        guard let scalar = scalars.first,
              scalars.index(after: scalars.startIndex) == scalars.endIndex else {
            return false
        }
        return scalar.value < 0x20
    }

    private func syncPreedit(clearIfNeeded: Bool = true) {
        guard let surface else { return }
        if markedText.length > 0 {
            let str = markedText.string
            let len = str.utf8CString.count
            if len > 0 {
                str.withCString { ptr in
                    ghostty_surface_preedit(surface, ptr, UInt(len - 1))
                }
            }
        } else if clearIfNeeded {
            ghostty_surface_preedit(surface, nil, 0)
        }
    }

    // MARK: Drag & Drop

    override func draggingEntered(_ sender: any NSDraggingInfo) -> NSDragOperation {
        guard let types = sender.draggingPasteboard.types else { return [] }
        if Set(types).isDisjoint(with: [.string, .fileURL]) { return [] }
        return .copy
    }

    override func performDragOperation(_ sender: any NSDraggingInfo) -> Bool {
        guard let content = sender.draggingPasteboard.getOpinionatedStringContents() else { return false }
        DispatchQueue.main.async { self.sendText(content) }
        return true
    }
}

// MARK: - NSTextInputClient

extension TerminalSurfaceView: NSTextInputClient {
    func hasMarkedText() -> Bool {
        markedText.length > 0
    }

    func markedRange() -> NSRange {
        guard markedText.length > 0 else { return NSRange() }
        return NSRange(0...(markedText.length - 1))
    }

    func selectedRange() -> NSRange {
        guard let surface else { return NSRange() }
        var text = ghostty_text_s()
        guard ghostty_surface_read_selection(surface, &text) else { return NSRange() }
        defer { ghostty_surface_free_text(surface, &text) }
        return NSRange(location: Int(text.offset_start), length: Int(text.offset_len))
    }

    func setMarkedText(_ string: Any, selectedRange: NSRange, replacementRange: NSRange) {
        switch string {
        case let v as NSAttributedString:
            markedText = NSMutableAttributedString(attributedString: v)
        case let v as String:
            markedText = NSMutableAttributedString(string: v)
        default:
            return
        }
        // Not in a keyDown (e.g. keyboard layout changed while composing): sync now.
        if keyTextAccumulator == nil {
            syncPreedit()
        }
    }

    func unmarkText() {
        if markedText.length > 0 {
            markedText.mutableString.setString("")
            syncPreedit()
        }
    }

    func validAttributesForMarkedText() -> [NSAttributedString.Key] {
        []
    }

    func attributedSubstring(forProposedRange range: NSRange, actualRange: NSRangePointer?) -> NSAttributedString? {
        guard let surface, range.length > 0 else { return nil }
        var text = ghostty_text_s()
        guard ghostty_surface_read_selection(surface, &text) else { return nil }
        defer { ghostty_surface_free_text(surface, &text) }

        var attributes: [NSAttributedString.Key: Any] = [:]
        if let fontRaw = ghostty_surface_quicklook_font(surface) {
            let font = Unmanaged<CTFont>.fromOpaque(fontRaw)
            attributes[.font] = font.takeUnretainedValue()
            font.release()
        }
        return .init(string: String(cString: text.text), attributes: attributes)
    }

    func characterIndex(for point: NSPoint) -> Int {
        0
    }

    func firstRect(forCharacterRange range: NSRange, actualRange: NSRangePointer?) -> NSRect {
        guard let surface else {
            return NSRect(x: frame.origin.x, y: frame.origin.y, width: 0, height: 0)
        }

        // Ghostty will tell us where it thinks an IME keyboard should render.
        var x: Double = 0
        var y: Double = 0
        var width: Double = cellSize.width
        var height: Double = cellSize.height

        if range.length > 0 && range != selectedRange() {
            var text = ghostty_text_s()
            if ghostty_surface_read_selection(surface, &text) {
                x = text.tl_px_x - 2
                y = text.tl_px_y + 2
                ghostty_surface_free_text(surface, &text)
            } else {
                ghostty_surface_ime_point(surface, &x, &y, &width, &height)
            }
        } else {
            ghostty_surface_ime_point(surface, &x, &y, &width, &height)
        }
        if range.length == 0, width > 0 {
            width = 0
            x += cellSize.width * Double(range.location + range.length)
        }
        // Ghostty coordinates are top-left; AppKit expects bottom-left.
        let viewRect = NSRect(x: x, y: frame.size.height - y, width: width, height: max(height, cellSize.height))
        let winRect = convert(viewRect, to: nil)
        guard let window else { return winRect }
        return window.convertToScreen(winRect)
    }

    func insertText(_ string: Any, replacementRange: NSRange) {
        guard NSApp.currentEvent != nil else { return }

        var chars = ""
        switch string {
        case let v as NSAttributedString:
            chars = v.string
        case let v as NSString:
            if let lead = LeadSurrogate(v) {
                leadSurrogate = lead
                chars = ""
            } else if let trail = TrailSurrogate(v) {
                chars = leadSurrogate?.encode(trail: trail) ?? ""
                leadSurrogate = nil
            } else {
                chars = v as String
                leadSurrogate = nil
            }
        default:
            return
        }

        // If insertText is called, our preedit must be over.
        unmarkText()

        if var acc = keyTextAccumulator {
            acc.append(chars)
            keyTextAccumulator = acc
            return
        }

        // Committed text (IME, dictation) is sent as key events, never as a paste.
        if !chars.isEmpty {
            _ = committedTextAction(GHOSTTY_ACTION_PRESS, text: chars)
        }
    }

    /// Prevents NSBeep for unimplemented actions and re-dispatches command keys.
    override func doCommand(by selector: Selector) {
        if let lastPerformKeyEvent,
           let current = NSApp.currentEvent,
           lastPerformKeyEvent == current.timestamp {
            NSApp.sendEvent(current)
        }
    }
}
