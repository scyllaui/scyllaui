// 移植自 Ghostty macOS 版（MIT，Copyright (c) 2024 Mitchell Hashimoto, Ghostty contributors），
// 固定 commit 34f39002c6e3。来源文件：
//   macos/Sources/Ghostty/NSEvent+Extension.swift
//   macos/Sources/Ghostty/Ghostty.Input.swift（eventModifierFlags、ghosttyMods）
//   macos/Sources/Ghostty/Ghostty.Shell.swift（escape）
//   macos/Sources/Helpers/KeyboardLayout.swift（id）
//   macos/Sources/Helpers/Extensions/String+Extension.swift（keyEventText）
//   macos/Sources/Helpers/Extensions/NSPasteboard+Extension.swift
//   macos/Sources/Ghostty/Surface View/SurfaceView_AppKit.swift（LeadSurrogate、TrailSurrogate）
// 只保留终端视图用到的部分，逻辑未改。许可证全文见 src/app/THIRD_PARTY_NOTICES.md。

import AppKit
import Carbon
import GhosttyKit
import UniformTypeIdentifiers

enum GhosttyInput {
    /// Returns the event modifier flags set for the Ghostty mods enum.
    static func eventModifierFlags(mods: ghostty_input_mods_e) -> NSEvent.ModifierFlags {
        var flags = NSEvent.ModifierFlags(rawValue: 0)
        if mods.rawValue & GHOSTTY_MODS_SHIFT.rawValue != 0 { flags.insert(.shift) }
        if mods.rawValue & GHOSTTY_MODS_CTRL.rawValue != 0 { flags.insert(.control) }
        if mods.rawValue & GHOSTTY_MODS_ALT.rawValue != 0 { flags.insert(.option) }
        if mods.rawValue & GHOSTTY_MODS_SUPER.rawValue != 0 { flags.insert(.command) }
        return flags
    }

    /// Translate event modifier flags to a ghostty mods enum.
    static func ghosttyMods(_ flags: NSEvent.ModifierFlags) -> ghostty_input_mods_e {
        var mods: UInt32 = GHOSTTY_MODS_NONE.rawValue

        if flags.contains(.shift) { mods |= GHOSTTY_MODS_SHIFT.rawValue }
        if flags.contains(.control) { mods |= GHOSTTY_MODS_CTRL.rawValue }
        if flags.contains(.option) { mods |= GHOSTTY_MODS_ALT.rawValue }
        if flags.contains(.command) { mods |= GHOSTTY_MODS_SUPER.rawValue }
        if flags.contains(.capsLock) { mods |= GHOSTTY_MODS_CAPS.rawValue }

        // Handle sided input.
        let rawFlags = flags.rawValue
        if rawFlags & UInt(NX_DEVICERSHIFTKEYMASK) != 0 { mods |= GHOSTTY_MODS_SHIFT_RIGHT.rawValue }
        if rawFlags & UInt(NX_DEVICERCTLKEYMASK) != 0 { mods |= GHOSTTY_MODS_CTRL_RIGHT.rawValue }
        if rawFlags & UInt(NX_DEVICERALTKEYMASK) != 0 { mods |= GHOSTTY_MODS_ALT_RIGHT.rawValue }
        if rawFlags & UInt(NX_DEVICERCMDKEYMASK) != 0 { mods |= GHOSTTY_MODS_SUPER_RIGHT.rawValue }

        return ghostty_input_mods_e(mods)
    }

    // Characters to escape in the shell.
    private static let escapeCharacters = "\\ ()[]{}<>\"'`!#$&;|*?\t"

    /// Escape shell-sensitive characters in a string by prefixing each with a backslash.
    static func shellEscape(_ str: String) -> String {
        var result = str
        for char in escapeCharacters {
            result = result.replacingOccurrences(of: String(char), with: "\\\(char)")
        }
        return result
    }

    /// Return a string ID of the current keyboard input source.
    static var keyboardLayoutID: String? {
        if let source = TISCopyCurrentKeyboardInputSource()?.takeRetainedValue(),
           let sourceIdPointer = TISGetInputSourceProperty(source, kTISPropertyInputSourceID) {
            let sourceId = unsafeBitCast(sourceIdPointer, to: CFString.self)
            return sourceId as String
        }
        return nil
    }
}

extension NSEvent {
    /// Create a Ghostty key event for a given keyboard action.
    ///
    /// This will not set the "text" or "composing" fields since these can't safely be set
    /// with the information or lifetimes given.
    func ghosttyKeyEvent(
        _ action: ghostty_input_action_e,
        translationMods: NSEvent.ModifierFlags? = nil
    ) -> ghostty_input_key_s {
        var key_ev: ghostty_input_key_s = .init()
        key_ev.action = action
        key_ev.keycode = UInt32(keyCode)
        key_ev.text = nil
        key_ev.composing = false

        // Control and command never contribute to the translation of text,
        // assume everything else did.
        key_ev.mods = GhosttyInput.ghosttyMods(modifierFlags)
        key_ev.consumed_mods = GhosttyInput.ghosttyMods(
            (translationMods ?? modifierFlags)
                .subtracting([.control, .command]))

        // Our unshifted codepoint is the codepoint with no modifiers.
        key_ev.unshifted_codepoint = 0
        if type == .keyDown || type == .keyUp {
            if let chars = characters(byApplyingModifiers: []),
               let codepoint = chars.unicodeScalars.first {
                key_ev.unshifted_codepoint = codepoint.value
            }
        }

        return key_ev
    }

    /// Returns the text to set for a key event for Ghostty.
    var ghosttyCharacters: String? {
        guard let characters else { return nil }

        if characters.count == 1,
           let scalar = characters.unicodeScalars.first {
            // Control characters are encoded by Ghostty's KeyEncoder.
            if scalar.value < 0x20 {
                return self.characters(byApplyingModifiers: modifierFlags.subtracting(.control))
            }

            // Function keys in the PUA are not sent down to Ghostty.
            if scalar.value >= 0xF700 && scalar.value <= 0xF8FF {
                return nil
            }
        }

        return characters
    }
}

extension String {
    /// The string as the text of a terminal key event, or nil when it is empty
    /// or begins with an ASCII control character.
    var keyEventText: String? {
        guard let scalar = unicodeScalars.first else { return nil }
        if scalar.value < 0x20 || scalar.value == 0x7F { return nil }
        return self
    }
}

extension NSPasteboard.PasteboardType {
    /// Initialize a pasteboard type from a MIME type string
    init?(mimeType: String) {
        if mimeType == "text/plain" {
            self = .string
            return
        }
        guard let utType = UTType(mimeType: mimeType) else {
            self.init(mimeType)
            return
        }
        self.init(utType.identifier)
    }
}

extension NSPasteboard {
    /// The pasteboard to used for Ghostty selection.
    static var ghosttySelection: NSPasteboard = {
        NSPasteboard(name: .init("dev.scyllaui.selection"))
    }()

    /// File paths are shell-escaped; otherwise any string.
    func getOpinionatedStringContents() -> String? {
        let strings = (pasteboardItems ?? []).compactMap { item in
            if let plist = item.propertyList(forType: .fileURL),
               let fileURL = NSURL(pasteboardPropertyList: plist, ofType: .fileURL) as URL?,
               fileURL.isFileURL {
                return GhosttyInput.shellEscape(fileURL.path)
            } else {
                return item.string(forType: .string)
            }
        }
        guard !strings.isEmpty else { return nil }
        return strings.joined(separator: " ")
    }

    private var ghosttyFileURLs: [URL] {
        (pasteboardItems ?? []).compactMap { item in
            guard let plist = item.propertyList(forType: .fileURL),
                  let url = NSURL(pasteboardPropertyList: plist, ofType: .fileURL) as URL?,
                  url.isFileURL else { return nil }
            return url
        }
    }

    /// The data for the given MIME type, if the pasteboard can serve it.
    func ghosttyData(forMime mime: String) -> Data? {
        switch mime {
        case "text/plain":
            guard let str = getOpinionatedStringContents() else { return nil }
            return Data(str.utf8)

        case "text/uri-list":
            let urls = ghosttyFileURLs
            guard !urls.isEmpty else { return nil }
            return Data(urls.map { $0.absoluteString + "\r\n" }.joined().utf8)

        default:
            guard let type = NSPasteboard.PasteboardType(mimeType: mime) else { return nil }
            return data(forType: type)
        }
    }

    /// The MIME types available on the pasteboard. Never reads data.
    func ghosttyAvailableMimes() -> [String] {
        var result: [String] = []
        var seen = Set<String>()
        let availableTypes = types ?? []
        let mimeType: (NSPasteboard.PasteboardType) -> String? = { type in
            guard let mime = UTType(type.rawValue)?.preferredMIMEType else { return nil }
            return mime == "text/plain;charset=utf-8" ? "text/plain" : mime
        }

        let hasFileURL = availableTypes.contains(.fileURL)
        let hasPlainText = hasFileURL || availableTypes.contains { type in
            mimeType(type) == "text/plain"
        }
        if hasPlainText {
            result.append("text/plain")
            seen.insert("text/plain")
        }
        if hasFileURL {
            result.append("text/uri-list")
            seen.insert("text/uri-list")
        }
        for type in availableTypes {
            guard let mime = mimeType(type), !seen.contains(mime) else { continue }
            seen.insert(mime)
            result.append(mime)
        }
        return result
    }

    /// The pasteboard for the Ghostty enum type.
    static func ghostty(_ clipboard: ghostty_clipboard_e) -> NSPasteboard? {
        switch clipboard {
        case GHOSTTY_CLIPBOARD_STANDARD: return Self.general
        case GHOSTTY_CLIPBOARD_SELECTION: return Self.ghosttySelection
        default: return nil
        }
    }
}

/// Check if a UTF16 text is a single lead surrogate character
struct LeadSurrogate {
    let char: UTF16Char

    init?(_ text: NSString) {
        guard text.length == 1 else { return nil }
        let char = text.character(at: 0)
        guard UTF16.isLeadSurrogate(char) else { return nil }
        self.char = char
    }

    func encode(trail: TrailSurrogate) -> String {
        String(decoding: [char, trail.char], as: UTF16.self)
    }
}

/// Check if a UTF16 text is a single trail surrogate character
struct TrailSurrogate {
    let char: UTF16Char

    init?(_ text: NSString) {
        guard text.length == 1 else { return nil }
        let char = text.character(at: 0)
        guard UTF16.isTrailSurrogate(char) else { return nil }
        self.char = char
    }
}
