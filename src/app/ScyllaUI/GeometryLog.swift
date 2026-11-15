// 终端尺寸与回滚的持久日志（2026-10-11 用户要求）：排查「前几行进了回滚区、下方留空」一类问题，出现后可随时回溯。
// 写在日志目录的 terminal-geometry.log（真实环境 ~/Library/Logs/ScyllaUI/，测试数据目录下与后台日志同处），
// 超过 2 MB 轮换为 .1。只记尺寸、回滚行数、切换与窗口事件，不记终端内容。写文件在后台串行队列上做。

import Foundation

final class GeometryLog {
    static var shared: GeometryLog?

    private static let maxBytes: UInt64 = 2 << 20
    private let path: String
    private let queue = DispatchQueue(label: "dev.scyllaui.geometry-log", qos: .utility)
    private let stamp: ISO8601DateFormatter = {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        f.timeZone = .current
        return f
    }()
    private var handle: FileHandle?

    init(directory: String) {
        path = (directory as NSString).appendingPathComponent("terminal-geometry.log")
        queue.async { try? FileManager.default.createDirectory(atPath: directory, withIntermediateDirectories: true) }
    }

    /// 写一行：`时间 事件 键=值…`。
    func write(_ event: String, _ fields: String = "") {
        let now = Date()
        queue.async { [self] in
            if handle == nil {
                if !FileManager.default.fileExists(atPath: path) {
                    FileManager.default.createFile(atPath: path, contents: nil)
                }
                handle = FileHandle(forWritingAtPath: path)
                _ = try? handle?.seekToEnd()
            }
            guard let handle else { return }
            let line = "\(stamp.string(from: now)) \(event)\(fields.isEmpty ? "" : " " + fields)\n"
            try? handle.write(contentsOf: Data(line.utf8))
            if let size = try? handle.offset(), size > Self.maxBytes {
                try? handle.close()
                self.handle = nil
                let old = path + ".1"
                try? FileManager.default.removeItem(atPath: old)
                try? FileManager.default.moveItem(atPath: path, toPath: old)
            }
        }
    }
}

/// 一个终端的尺寸与回滚跟踪：行列变化、变化后 1 秒内的回滚变化，以及变化结束 1 秒后的判定。
/// 判定「可疑」：一串尺寸变化前后，行数没有减少而回滚行数增加了，即内容被推进回滚区却没拉回（用户所见现象）；
/// 期间若终端有新输出，回滚也会增加，所以只是线索，需结合前后事件判断。
final class GeometryTracker {
    private let id: String
    private var grid: (cols: Int, rows: Int)?
    /// libghostty 的滚动条状态：回滚行数 = total - len。终端未渲染（被切走）时不更新。
    private var scrollbar: (total: UInt64, offset: UInt64, len: UInt64)?
    /// 本串尺寸变化开始前的行数与回滚行数；结束判定后清空。
    private var burstStart: (rows: Int, scrollback: UInt64?)?
    private var settle: DispatchWorkItem?

    init(id: String) { self.id = String(id.prefix(8)) }

    private var scrollback: UInt64? { scrollbar.map { $0.total &- $0.len } }

    func grid(cols: Int, rows: Int, widthPx: UInt32, heightPx: UInt32, attached: Bool) {
        guard grid.map({ $0 != (cols, rows) }) ?? true else { return }
        let old = grid
        grid = (cols, rows)
        if let old, burstStart == nil { burstStart = (old.rows, scrollback) }
        let from = old.map { "\($0.cols)x\($0.rows)" } ?? "-"
        GeometryLog.shared?.write("grid", "terminal=\(id) \(from)->\(cols)x\(rows) px=\(widthPx)x\(heightPx) "
            + "scrollback=\(scrollback.map(String.init) ?? "?") attached=\(attached)")
        settle?.cancel()
        let item = DispatchWorkItem { [weak self] in self?.judge() }
        settle = item
        DispatchQueue.main.asyncAfter(deadline: .now() + 1, execute: item)
    }

    func scrollbar(total: UInt64, offset: UInt64, len: UInt64) {
        let changed = scrollbar.map { $0.total != total || $0.len != len } ?? true
        scrollbar = (total, offset, len)
        // 只在尺寸变化后的 1 秒内记录，平时输出滚动不写日志。
        if changed, burstStart != nil {
            GeometryLog.shared?.write("scrollback", "terminal=\(id) total=\(total) len=\(len) offset=\(offset) "
                + "scrollback=\(total &- len)")
        }
    }

    /// 写下当前状态（「记录终端尺寸」菜单）。
    func dump(current: Bool) {
        GeometryLog.shared?.write("state", "terminal=\(id) current=\(current) "
            + "grid=\(grid.map { "\($0.cols)x\($0.rows)" } ?? "?") "
            + "scrollbar=\(scrollbar.map { "\($0.total)/\($0.len)/\($0.offset)" } ?? "?")")
    }

    private func judge() {
        guard let start = burstStart, let rows = grid?.rows else { return }
        burstStart = nil
        guard let before = start.scrollback, let after = scrollback, rows >= start.rows, after > before else { return }
        GeometryLog.shared?.write("suspect", "terminal=\(id) rows=\(start.rows)->\(rows) "
            + "scrollback=\(before)->\(after) reason=scrollback_grew_without_shrink")
    }
}
