// 报告问题（2026-11-02 用户采纳，docs/design.md 10.6 节「问题反馈」）：在本机把版本、各 Agent 的状态同步、日志与崩溃报告
// 收进一个文件夹，用户看过、确认后由用户自己的邮件或 GitHub 账号发出；App 不联网、不自动上报。
// 挑文件、截尾、脱敏与摘要都是纯函数，可单测；产品名、后台与目录都叫 ScyllaUI（2026-11-05 起内部标识也改名，D8）。

import Foundation

/// 收件地址（2026-11-05 改为发布与问题共用的公开仓库 `scyllaui/scyllaui`，D9）；邮箱 2026-11-08 改为专用地址
/// `support@thinkfarlabs.com`（Cloudflare 邮件转发）。
enum FeedbackChannel {
    static let email = "support@thinkfarlabs.com"
    static let issues = "https://github.com/scyllaui/scyllaui/issues/new"
    /// 引导页里给人看的写法。
    static let issuesPage = "github.com/scyllaui/scyllaui"
}

struct ProblemReport {
    /// 摘要里的环境信息，由 App 在打开「报告问题」时取好。
    struct Environment {
        var appVersion: String
        var buildNumber: String
        var buildID: String?
        var daemonBuildID: String?
        var system: String
        var language: String
        /// 终端引擎（libghostty）的版本，也用来认出它写下的崩溃报告。
        var engine: String?
        /// 后台快照与 `hooks.upsert` 里的 Hook 状态原样（docs/protocol.md 第 5 节「Hook 状态」）。
        var hooks: [[String: Any]]
        var inaccurate: [InaccurateItem]
        var lastUnexpectedExit: Date?
    }

    /// 要附上的一个文件。`crash` 为崩溃报告：可能带有崩溃瞬间内存里的文字，用户可以不附。
    struct Item: Equatable {
        let source: URL
        let name: String
        let size: Int
        let crash: Bool
    }

    static let appLogDays = 3
    static let tailBytes = 2 << 20
    static let crashDays = 30
    static let maxEngineCrashes = 3

    // MARK: 挑文件

    /// 要附上的日志与崩溃报告：`scyllauid.log`、最近 3 天的 App 日志（各取最后 2 MB）；30 天内系统为 ScyllaUI 各进程写的 `.ips`，
    /// 与终端引擎写的、版本与本 App 一致的 `.ghosttycrash`（目录与 Ghostty.app 共用，按版本筛掉它的）。终端尺寸日志不带。
    static func collect(logDir: String, home: String, engine: String?, now: Date, fm: FileManager = .default) -> [Item] {
        var items: [Item] = []
        func add(_ url: URL, crash: Bool) {
            guard let size = (try? fm.attributesOfItem(atPath: url.path))?[.size] as? Int else { return }
            items.append(Item(source: url, name: url.lastPathComponent, size: crash ? size : min(size, tailBytes), crash: crash))
        }
        let logs = URL(fileURLWithPath: logDir, isDirectory: true)
        add(logs.appendingPathComponent("scyllauid.log"), crash: false)
        let appLog = AppLogFile(directory: logDir, now: { now })
        for back in 0..<appLogDays {
            guard let day = Calendar.current.date(byAdding: .day, value: -back, to: now) else { continue }
            add(URL(fileURLWithPath: appLog.path(for: day)), crash: false)
        }
        let since = now.addingTimeInterval(-Double(crashDays) * 86_400)
        let reports = URL(fileURLWithPath: home).appendingPathComponent("Library/Logs/DiagnosticReports", isDirectory: true)
        for url in recent(in: reports, since: since, fm: fm) where isOwnDiagnosticReport(url.lastPathComponent) {
            add(url, crash: true)
        }
        if let engine {
            let dir = URL(fileURLWithPath: home).appendingPathComponent(".local/state/ghostty/crash", isDirectory: true)
            let own = recent(in: dir, since: since, fm: fm).filter { url in
                guard url.pathExtension == "ghosttycrash", let h = try? FileHandle(forReadingFrom: url) else { return false }
                defer { try? h.close() }
                return ghosttyRelease(in: (try? h.read(upToCount: 4096)) ?? Data()) == engine
            }
            own.prefix(maxEngineCrashes).forEach { add($0, crash: true) }
        }
        return items
    }

    /// 目录里修改时间不早于 `since` 的文件，新的在前。
    private static func recent(in dir: URL, since: Date, fm: FileManager) -> [URL] {
        let urls = (try? fm.contentsOfDirectory(at: dir, includingPropertiesForKeys: [.contentModificationDateKey])) ?? []
        let dated = urls.compactMap { url -> (URL, Date)? in
            guard let d = try? url.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate, d >= since else { return nil }
            return (url, d)
        }
        return dated.sorted { $0.1 > $1.1 }.map(\.0)
    }

    /// 系统崩溃报告按进程名开头：App 为 ScyllaUI、ScyllaUIBeta，后台为 scyllauid、scyllaui；2026-11-05 改名前的旧进程名不再收集。
    static func isOwnDiagnosticReport(_ name: String) -> Bool {
        let n = name.lowercased()
        return n.hasPrefix("scyllaui") && name.hasSuffix(".ips")
    }

    /// `.ghosttycrash` 开头的 Sentry 信封里的 `"release":"…"`。
    static func ghosttyRelease(in head: Data) -> String? {
        let text = String(decoding: head, as: UTF8.self)
        guard let r = text.range(of: #""release":"[^"]*""#, options: .regularExpression) else { return nil }
        return String(text[r].dropFirst(11).dropLast())
    }

    // MARK: 内容处理

    /// 只留最后 `limit` 字节，从其后第一个换行之后开始，不留半行。
    static func tail(_ data: Data, limit: Int) -> Data {
        guard data.count > limit else { return data }
        let cut = data.suffix(limit)
        guard let newline = cut.firstIndex(of: 0x0A) else { return Data(cut) }
        return Data(cut[cut.index(after: newline)...])
    }

    /// 家目录写成 `~`（后面紧跟的是路径分隔或非名字字符才算，`/Users/example` 不碰 `/Users/example2`）。
    static func redact(_ text: String, home: String) -> String {
        guard !home.isEmpty else { return text }
        let pattern = NSRegularExpression.escapedPattern(for: home) + "(?![A-Za-z0-9._-])"
        guard let re = try? NSRegularExpression(pattern: pattern) else { return text }
        return re.stringByReplacingMatches(in: text, range: NSRange(text.startIndex..., in: text), withTemplate: "~")
    }

    /// 给开发者看的摘要，字段用英文，不随界面语言变化。
    static func summary(_ env: Environment, description: String, now: Date) -> String {
        let iso = ISO8601DateFormatter()
        iso.timeZone = .current
        var lines = ["ScyllaUI problem report", "Generated: \(iso.string(from: now))"]
        lines.append("App: \(env.appVersion) (build \(env.buildNumber), id \(env.buildID ?? "-"))")
        if let d = env.daemonBuildID {
            lines.append("scyllauid: connected, id \(d)" + (d == env.buildID ? " (same as app)" : " (differs from app)"))
        } else {
            lines.append("scyllauid: not connected")
        }
        lines.append("System: \(env.system)")
        lines.append("Language: \(env.language)")
        lines.append("Terminal engine: \(env.engine ?? "-")")
        if let exit = env.lastUnexpectedExit { lines.append("Last unexpected exit: \(iso.string(from: exit))") }
        lines.append("")
        lines.append("Agents:")
        for h in env.hooks {
            let tool = h["tool"] as? String ?? "?"
            var line = "  \(tool): \(h["state"] as? String ?? "?")"
            if let problems = h["problems"] as? [String], !problems.isEmpty { line += " [\(problems.joined(separator: ", "))]" }
            if let v = h["version"] as? String {
                let running = h["version_running"] as? Bool == true ? "running" : "last conversation"
                let verified = h["version_verified"] as? Bool == true ? "verified" : "unverified"
                line += ", \(v) (\(running), \(verified))"
            }
            lines.append(line)
        }
        if !env.inaccurate.isEmpty {
            lines.append("")
            lines.append("State may be inaccurate:")
            for i in env.inaccurate {
                lines.append("  \(([i.tool] + i.versions).joined(separator: " ")): \(i.reasons.joined(separator: "; "))")
            }
        }
        lines.append("")
        lines.append("Description:")
        lines.append(description.isEmpty ? "-" : description)
        return lines.joined(separator: "\n") + "\n"
    }

    /// GitHub 新建问题页：预填标题与正文。正文过长时截断，网址不超出浏览器与 GitHub 的上限。
    static func issueURL(title: String, body: String, maxBody: Int = 4000) -> URL {
        let text = body.count > maxBody ? String(body.prefix(maxBody)) + "\n…" : body
        var c = URLComponents(string: FeedbackChannel.issues)!
        c.queryItems = [URLQueryItem(name: "title", value: title), URLQueryItem(name: "body", value: text)]
        // URLComponents 不转义「+」，GitHub 会把它读成空格。
        c.percentEncodedQuery = c.percentEncodedQuery?.replacingOccurrences(of: "+", with: "%2B")
        return c.url!
    }

    // MARK: 写出

    /// 每次报告一个新文件夹 `~/Library/Caches/ScyllaUI/reports/scyllaui-report-<时间>/`；顺手清掉 7 天前的旧报告。
    static func makeFolder(now: Date, fm: FileManager = .default) throws -> URL {
        let caches = try fm.url(for: .cachesDirectory, in: .userDomainMask, appropriateFor: nil, create: true)
        let root = caches.appendingPathComponent("ScyllaUI/reports", isDirectory: true)
        try fm.createDirectory(at: root, withIntermediateDirectories: true)
        for old in recentlyOlder(in: root, than: now.addingTimeInterval(-7 * 86_400), fm: fm) { try? fm.removeItem(at: old) }
        let f = DateFormatter()
        f.locale = Locale(identifier: "en_US_POSIX")
        f.dateFormat = "yyyyMMdd-HHmmss"
        let dir = root.appendingPathComponent("scyllaui-report-\(f.string(from: now))", isDirectory: true)
        try? fm.removeItem(at: dir)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    private static func recentlyOlder(in dir: URL, than date: Date, fm: FileManager) -> [URL] {
        let urls = (try? fm.contentsOfDirectory(at: dir, includingPropertiesForKeys: [.contentModificationDateKey])) ?? []
        return urls.filter { url in
            url.lastPathComponent.hasPrefix("scyllaui-report-")
                && ((try? url.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantFuture) < date
        }
    }

    /// 把文件放进文件夹：日志取尾部并脱敏，`.ips` 脱敏，`.ghosttycrash` 是二进制、原样复制。
    static func write(_ items: [Item], to dir: URL, home: String, fm: FileManager = .default) {
        for item in items {
            let sub = dir.appendingPathComponent(item.crash ? "crash" : "logs", isDirectory: true)
            try? fm.createDirectory(at: sub, withIntermediateDirectories: true)
            let dest = sub.appendingPathComponent(item.name)
            if item.source.pathExtension == "ghosttycrash" {
                try? fm.copyItem(at: item.source, to: dest)
                continue
            }
            guard var data = try? Data(contentsOf: item.source) else { continue }
            if !item.crash { data = tail(data, limit: tailBytes) }
            try? Data(redact(String(decoding: data, as: UTF8.self), home: home).utf8).write(to: dest)
        }
    }

    /// 写入摘要并打成同名 zip（系统的「上传用」打包，不起子进程）。旧的 zip 先删掉，重复发送时内容是最新的描述。
    static func zip(_ dir: URL, summary: String, fm: FileManager = .default) throws -> URL {
        try Data(summary.utf8).write(to: dir.appendingPathComponent("summary.txt"))
        let dest = dir.deletingLastPathComponent().appendingPathComponent(dir.lastPathComponent + ".zip")
        try? fm.removeItem(at: dest)
        var coordinated: NSError?
        var copyError: Error?
        NSFileCoordinator().coordinate(readingItemAt: dir, options: .forUploading, error: &coordinated) { zipped in
            do { try fm.copyItem(at: zipped, to: dest) } catch { copyError = error }
        }
        if let e = coordinated ?? copyError { throw e }
        return dest
    }
}
