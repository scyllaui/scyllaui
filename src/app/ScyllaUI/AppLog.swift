// App 日志（docs/design.md 10.5 节决策 23）：同时写系统统一日志与日志目录下按天的文件 `app-yyyy-MM-dd.log`
// （真实环境 ~/Library/Logs/ScyllaUI/，与 scyllauid.log 同处）。统一日志里实测只留不到 2 天，试用阶段按用户给的时间排查
// 要靠文件。调用写法与 os.Logger 相同：`scyllauiLog.notice("… \(x, privacy: .public)")`；没标 public 的字符串等写成
// `<private>`（数字与布尔照写，同统一日志的默认规则），所以文件里不会有提示词与密钥。debug 级只进统一日志、不进文件。
// 文件保留 14 天，单个文件超过上限后当天不再写。写文件在后台串行队列上做；日期由写入时刻算出，不依赖跨天通知。

import Foundation
import os

let scyllauiLog = ScyllaUILogger(subsystem: "dev.scyllaui.app", category: "ghostty")

enum LogPrivacy {
    case `public`, `private`
}

/// 日志正文：插值按隐私规则在构造时就转成文字。
struct LogMessage: ExpressibleByStringInterpolation {
    let text: String

    init(stringLiteral value: String) { text = value }
    init(stringInterpolation: Interpolation) { text = stringInterpolation.text }

    struct Interpolation: StringInterpolationProtocol {
        var text = ""
        init(literalCapacity: Int, interpolationCount: Int) { text.reserveCapacity(literalCapacity) }
        mutating func appendLiteral(_ literal: String) { text += literal }
        mutating func appendInterpolation<T>(_ value: T, privacy: LogPrivacy) { text += privacy == .public ? "\(value)" : "<private>" }
        mutating func appendInterpolation<T: BinaryInteger>(_ value: T) { text += "\(value)" }
        mutating func appendInterpolation<T: BinaryFloatingPoint>(_ value: T) { text += "\(value)" }
        mutating func appendInterpolation(_ value: Bool) { text += "\(value)" }
        mutating func appendInterpolation<T>(_ value: T) { text += "<private>" }
    }
}

final class ScyllaUILogger {
    private let system: Logger
    /// 文件日志；日志目录确定后（WorkspaceController 启动时）才设置，之前的只进统一日志。
    var file: AppLogFile?

    init(subsystem: String, category: String) {
        system = Logger(subsystem: subsystem, category: category)
    }

    func debug(_ m: LogMessage) { system.debug("\(m.text, privacy: .public)") }
    func info(_ m: LogMessage) { system.info("\(m.text, privacy: .public)"); file?.write("INFO", m.text) }
    func notice(_ m: LogMessage) { system.notice("\(m.text, privacy: .public)"); file?.write("NOTICE", m.text) }
    func warning(_ m: LogMessage) { system.warning("\(m.text, privacy: .public)"); file?.write("WARNING", m.text) }
    func error(_ m: LogMessage) { system.error("\(m.text, privacy: .public)"); file?.write("ERROR", m.text) }
}

/// 按天的日志文件：每行「本地时间 级别 正文」，时间格式与 scyllauid.log 相同，便于按时间对照。
final class AppLogFile {
    static let keepDays = 14
    static let maxBytes: UInt64 = 20 << 20

    private let directory: String
    private let queue = DispatchQueue(label: "dev.scyllaui.app-log", qos: .utility)
    private let now: () -> Date
    private let day: DateFormatter
    private let stamp: DateFormatter
    private var handle: FileHandle?
    private var openDay: String?
    private var full = false

    init(directory: String, now: @escaping () -> Date = Date.init) {
        self.directory = directory
        self.now = now
        day = Self.formatter("yyyy-MM-dd")
        stamp = Self.formatter("yyyy-MM-dd HH:mm:ss.SSS")
    }

    private static func formatter(_ format: String) -> DateFormatter {
        let f = DateFormatter()
        f.locale = Locale(identifier: "en_US_POSIX")
        f.timeZone = .current
        f.dateFormat = format
        return f
    }

    func path(for date: Date) -> String {
        (directory as NSString).appendingPathComponent("app-\(day.string(from: date)).log")
    }

    func write(_ level: String, _ text: String) {
        let at = now()
        queue.async { [self] in append(level, text, at) }
    }

    /// 测试用：等已排队的写入完成。
    func flush() { queue.sync {} }

    private func append(_ level: String, _ text: String, _ at: Date) {
        let today = day.string(from: at)
        if today != openDay {
            try? handle?.close()
            handle = nil
            full = false
            openDay = today
            try? FileManager.default.createDirectory(atPath: directory, withIntermediateDirectories: true)
            let file = path(for: at)
            if !FileManager.default.fileExists(atPath: file) {
                FileManager.default.createFile(atPath: file, contents: nil, attributes: [.posixPermissions: 0o600])
            }
            handle = FileHandle(forWritingAtPath: file)
            _ = try? handle?.seekToEnd()
            prune(before: at)
        }
        guard let handle, !full else { return }
        let line = "\(stamp.string(from: at)) \(level) \(text.replacingOccurrences(of: "\n", with: " ⏎ "))\n"
        try? handle.write(contentsOf: Data(line.utf8))
        if let size = try? handle.offset(), size > Self.maxBytes {
            try? handle.write(contentsOf: Data("\(stamp.string(from: at)) NOTICE 今天的日志已达上限，之后不再写入\n".utf8))
            full = true
        }
    }

    /// 删除超过保留天数的 `app-*.log`（按文件名里的日期）。
    private func prune(before date: Date) {
        guard let names = try? FileManager.default.contentsOfDirectory(atPath: directory),
              let cutoff = Calendar.current.date(byAdding: .day, value: -Self.keepDays, to: date) else { return }
        let limit = day.string(from: cutoff)
        for name in names where name.hasPrefix("app-") && name.hasSuffix(".log") {
            let d = String(name.dropFirst(4).dropLast(4))
            if d.count == 10, d < limit {
                try? FileManager.default.removeItem(atPath: (directory as NSString).appendingPathComponent(name))
            }
        }
    }
}
