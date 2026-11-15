// 一个终端（M1 第 4 块，docs/design.md 第 5 节「一栏 = 一个终端」）：libghostty Surface + 终端 ID（= SCYLLAUI_PANE_ID）。
// M3 起是保活终端（docs/design.md 第 7 节「实现（M3）」）：PTY 与 Shell 在 scyllauid 里，Surface 只运行
// `scyllaui attach --terminal <ID>` 原样中继字节。前台进程、当前目录由后台从 PTY 与 vt 读取，App 不再上报；
// Surface 在第一次显示时才创建（attach 须等首次布局后再连接），attach 退出后可以重建、重新接回。

import AppKit
import GhosttyKit

final class TerminalSession {
    let id: String
    /// Shell 集成上报的当前目录（OSC 7，经 attach 原样到达 Surface）；尚未上报时为启动目录。只用于「在当前目录新建」。
    private(set) var directory: String

    /// Surface 关闭（attach 已退出：Shell 结束、终端被关闭，或与后台的连接断了）。
    var onSurfaceClosed: ((TerminalSession) -> Void)?

    private let app: ghostty_app_t
    private let core: CoreClient
    let geometry: GeometryTracker
    private var surfaceView: TerminalSurfaceView?

    /// 新建：先在后台登记再创建会话对象；Surface 在显示时创建。`initialInput` 为「新建 AI」时代输的「命令 + 回车」，
    /// 由后台在起 Shell 后写入（docs/design.md 第 5 节）。`resumeOf` 为一键恢复的对话 ID（M1 第 6 块）。
    init(app: ghostty_app_t, core: CoreClient, directory: String, initialInput: String?, resumeOf: Int64? = nil) {
        let id = UUID().uuidString.lowercased()
        self.id = id
        self.app = app
        self.core = core
        self.directory = directory
        geometry = GeometryTracker(id: id)
        core.registerTerminal(id: id, projectDir: directory, cwd: directory, resumeOf: resumeOf,
                              held: true, initialInput: initialInput)
    }

    /// App 重启后接回后台仍持有的终端（M3）。
    init(app: ghostty_app_t, core: CoreClient, restoring id: String, projectDir: String, directory: String) {
        self.id = id
        self.app = app
        self.core = core
        self.directory = directory
        geometry = GeometryTracker(id: id)
        core.adoptTerminal(id: id, projectDir: projectDir, cwd: directory)
    }

    var hasView: Bool { surfaceView != nil }

    /// 终端视图；第一次取用时创建 Surface（随之启动 attach）。
    var view: TerminalSurfaceView {
        if let v = surfaceView { return v }
        let env = ["SCYLLAUI_PANE_ID": id, "SCYLLAUI_SOCK": core.socketPath, "SCYLLAUI_BIN": core.scyllauiPath]
        let command = "\(Self.shellQuote(core.scyllauiPath)) attach --terminal \(Self.shellQuote(id))"
        let v = TerminalSurfaceView(app: app, workingDirectory: directory, environment: env, command: command)
        v.onPwd = { [weak self] pwd in self?.directory = pwd }
        // 行列与回滚变化写持久日志（GeometryLog.swift）：排查内容被推进回滚区、下方留空的问题。
        let geometry = self.geometry
        v.onGridSize = { [weak v] size in
            geometry.grid(cols: Int(size.columns), rows: Int(size.rows), widthPx: size.width_px,
                          heightPx: size.height_px, attached: v?.window != nil)
        }
        v.onScrollbar = { total, offset, len in geometry.scrollbar(total: total, offset: offset, len: len) }
        v.onClose = { [weak self] _ in
            guard let self else { return }
            self.onSurfaceClosed?(self)
        }
        surfaceView = v
        return v
    }

    /// 释放 Surface（libghostty 随之结束 attach，后台的终端不受影响）。之后再取 `view` 会重建并重新接回。
    func dropView() {
        surfaceView?.removeFromSuperview()
        surfaceView = nil
    }

    /// libghostty 以 `/bin/bash -c "exec -l <命令>"` 运行 Surface 的命令（Exec.zig execCommand），参数按 sh 规则加引号。
    static func shellQuote(_ s: String) -> String {
        "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }
}
