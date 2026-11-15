import AppKit
import GhosttyKit

// 与 Ghostty.app 的 main.swift 相同：先初始化 libghostty 全局状态
if ghostty_init(UInt(CommandLine.argc), CommandLine.unsafeArgv) != GHOSTTY_SUCCESS {
    FileHandle.standardError.write("ghostty_init failed\n".data(using: .utf8)!)
    exit(1)
}

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.setActivationPolicy(.regular)
app.run()
