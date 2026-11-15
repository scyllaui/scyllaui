// App 是否不在能长期运行的位置（M4b 块 2，docs/design.md 10.6 节「块 2 实现」）：系统随机挪位运行（App Translocation，
// 从下载的 DMG 或隔离属性未清的位置直接打开时，系统把包挪到只读的随机目录）或直接在 DMG 卷里运行。这时后台会从临时路径启动、
// 授权记不住，所以先提示拖到「应用程序」；不替用户搬动。纯函数，可单测。

import Foundation

enum InstallLocation {
    static func needsMove(bundlePath: String) -> Bool {
        bundlePath.contains("/AppTranslocation/") || bundlePath.hasPrefix("/Volumes/")
    }
}
