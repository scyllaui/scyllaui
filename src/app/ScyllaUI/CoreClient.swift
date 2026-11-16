// 连接常驻的 scyllauid（协议 v3，docs/protocol.md）。连不上时以 posix_spawn 拉起包内 scyllauid：新会话、只保留 0/1/2，
// 不继承 App 的其他描述符，App 退出后继续运行。build_id 与包内版本不一致时换新：后台声明 `pty_host`（M3 起，持有
// 保活终端）时发 `upgrade` 让它原地 exec 接管，会话不断；老后台（不持有 PTY）仍让它退出再拉起（design.md 风险 15）。
// socket 通信全部在后台线程，主线程只更新界面（docs/design.md 第 11 节风险 3）。

import Foundation
import Darwin

final class CoreClient {
    static let protocolVersion = 3

    let socketPath: String
    let scyllauiPath: String
    /// 以 `SCYLLAUI_DATA_DIR` 连接独立的数据目录（测试用，docs/protocol.md 第 1 节），不碰真实数据。
    let isolated: Bool
    /// 收到推送（以及 `sync` 应答中的快照，类型改为 `snapshot`）时在**读线程**按到达顺序回调；`receivedUs` 是读到该行时的
    /// 墙钟微秒。回调里不能阻塞，界面更新自行转到主线程。
    var onMessage: (([String: Any], UInt64) -> Void)?
    var onError: ((String) -> Void)?
    /// 包内 scyllauid 的构建标识（主线程读写）；连接线程第一次读到后写入，供「关于 ScyllaUI」显示，读到之前为 nil。
    private(set) var appBuildID: String?
    /// 当前连着的后台的构建标识（主线程读写，设置「关于」显示，M4a）；未连接时为 nil。
    private(set) var daemonBuildID: String?

    private let daemonPath: String
    private let dataDir: String
    private let logPath: String
    /// 日志目录（后台日志所在目录；App 的终端尺寸日志也写在这里）。
    var logDir: String { (logPath as NSString).deletingLastPathComponent }
    /// 串行队列：socket 写入与已登记终端表只在这里访问。
    private let queue = DispatchQueue(label: "dev.scyllaui.core")
    private var fd: Int32 = -1
    private var terminals: [String: [String: Any]] = [:]
    private var nextID = 0
    /// 等待应答的请求（id → 回调，在读线程调用）；断线时丢弃。
    private var pending: [Int: ([String: Any]) -> Void] = [:]
    /// 最近一次 `terminal.focus`，重连后随 `sync` 重发。
    private var focused: String?
    /// 断开期间后台代答查询所用的外观（`appearance.set`，M3），重连后重发。
    private var appearance: [String: Any]?

    /// 数据目录：`SCYLLAUI_DATA_DIR`（测试用）或 `~/Library/Application Support/ScyllaUI`。主题文件也放在这里（M2 第 5 块）。
    static var dataDirectory: String {
        let real = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Application Support/ScyllaUI").path
        let custom = ProcessInfo.processInfo.environment["SCYLLAUI_DATA_DIR"].map { ($0 as NSString).standardizingPath }
        if let custom, !custom.isEmpty { return custom }
        #if DEBUG
        // 测试版的隔离也可以记在它自己的偏好设置里（`defaults write dev.scyllaui.app.debug ScyllaUIDataDir <目录>`）：
        // 自动更新替换后由系统重新打开，`open --env` 给的环境变量不会带过去，只靠环境变量会连上并接管真实后台（M4b 块 3）。
        if let saved = UserDefaults.standard.string(forKey: "ScyllaUIDataDir"), !saved.isEmpty {
            return (saved as NSString).standardizingPath
        }
        #endif
        return real
    }

    init() {
        let helpers = Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers")
        scyllauiPath = helpers.appendingPathComponent("scyllaui").path
        daemonPath = helpers.appendingPathComponent("scyllauid").path
        let library = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library")
        let real = library.appendingPathComponent("Application Support/ScyllaUI").path
        let custom: String? = Self.dataDirectory
        if let custom, !custom.isEmpty, custom != real {
            // 测试数据目录：后台日志也写进去，与真实后台完全隔离。
            dataDir = custom
            logPath = (custom as NSString).appendingPathComponent("scyllauid.log")
            isolated = true
            try? FileManager.default.createDirectory(atPath: custom, withIntermediateDirectories: true,
                                                     attributes: [.posixPermissions: 0o700])
        } else {
            dataDir = real
            logPath = library.appendingPathComponent("Logs/ScyllaUI/scyllauid.log").path
            isolated = false
        }
        socketPath = (dataDir as NSString).appendingPathComponent("scyllauid.sock")
    }

    func start() {
        Thread.detachNewThread { [weak self] in self?.run() }
    }

    /// 登记终端：已连接时立即发 `terminal.open`，否则在下次 `sync` 中上报。`resumeOf`：为一键恢复这条对话而开
    /// （M1 第 6 块），只随这一次 `terminal.open` 发出，`sync` 不带（未连接时恢复不成立，终端照常登记）。
    /// `held`（M3）：由后台持有 PTY，Surface 里运行 `scyllaui attach`；`initialInput` 由后台在起 Shell 后写入。
    func registerTerminal(id: String, projectDir: String, cwd: String, resumeOf: Int64? = nil,
                          held: Bool = false, initialInput: String? = nil) {
        var t: [String: Any] = ["terminal_id": id, "project_dir": projectDir, "cwd": cwd]
        if held { t["held"] = true }
        queue.async {
            guard self.fd >= 0 else {
                // 未连接：代输的命令随下次 `sync` 带上（只带这一次）。
                self.terminals[id] = initialInput.map { t.merging(["initial_input": $0]) { a, _ in a } } ?? t
                return
            }
            self.terminals[id] = t
            var msg = t.merging(["type": "terminal.open"]) { a, _ in a }
            if let resumeOf { msg["resume_of"] = resumeOf }
            if let initialInput { msg["initial_input"] = initialInput }
            self.send(msg)
        }
    }

    /// App 重启后接回后台仍持有的终端（M3）：只记下，之后的 `sync` 里列出它（带 `held`），不另发登记。
    func adoptTerminal(id: String, projectDir: String, cwd: String) {
        queue.async {
            self.terminals[id] = ["terminal_id": id, "project_dir": projectDir, "cwd": cwd, "held": true]
        }
    }

    /// 后台已关闭的终端：不再在 `sync` 中列出。
    func forgetTerminal(id: String) {
        queue.async { self.terminals[id] = nil }
    }

    /// 关闭终端（`terminal.close`，M3）：后台判定是否要先确认。应答在读线程回调；未连接时不发、不回调。
    func closeTerminal(id: String, confirmed: Bool, completion: @escaping ([String: Any]) -> Void) {
        request(["type": "terminal.close", "terminal_id": id, "confirmed": confirmed], completion: completion)
    }

    /// 断开期间后台代答终端查询所用的外观（`appearance.set`，M3）。相同值不重发。
    func setAppearance(_ a: [String: Any]) {
        queue.async {
            guard (self.appearance as NSDictionary?) != (a as NSDictionary) else { return }
            self.appearance = a
            if self.fd >= 0 { self.send(a.merging(["type": "appearance.set"]) { x, _ in x }) }
        }
    }

    /// 上报前台进程组，由 scyllauid 识别是 Shell、普通命令还是哪款 CLI（docs/design.md 4.2 节）。未连接时不补发：
    /// 重连后的下一次读取会再上报。
    func reportForeground(id: String, pgid: pid_t?) {
        queue.async {
            if self.fd >= 0 { self.send(["type": "terminal.foreground", "terminal_id": id, "pgid": pgid ?? 0]) }
        }
    }

    /// 用户正在看的终端（null = 没在看），后台据此判定「待查看」（docs/protocol.md 第 4、7 节）。相同值不重发。
    func focus(id: String?) {
        queue.async {
            guard id != self.focused || self.fd < 0 else { return }
            self.focused = id
            if self.fd >= 0 { self.send(["type": "terminal.focus", "terminal_id": id ?? NSNull()]) }
        }
    }

    /// 发请求并在读线程收到应答时回调（与推送按到达顺序交错）。未连接时不发、不回调。
    func request(_ obj: [String: Any], completion: @escaping ([String: Any]) -> Void) {
        queue.async {
            guard self.fd >= 0 else { return }
            self.pending[self.send(obj)] = completion
        }
    }

    /// 终端改名；空串清除命名（恢复自动标题）。未连接时不发。
    func rename(id: String, name: String) {
        queue.async {
            if self.fd >= 0 { self.send(["type": "terminal.rename", "terminal_id": id, "name": name]) }
        }
    }

    /// 一键恢复（`conversation.resume`，docs/protocol.md 第 4 节）：应答在读线程回调；未连接时立即以 nil 回调。
    /// `typeInto`：在这个已有终端里恢复，由后台把命令当键盘输入写进它的 PTY（应答 `typed`）。
    func resume(conversationID: Int64, typeInto: String? = nil, completion: @escaping ([String: Any]?) -> Void) {
        queue.async {
            guard self.fd >= 0 else { return completion(nil) }
            var req: [String: Any] = ["type": "conversation.resume", "conversation_id": conversationID]
            if let typeInto { req["type_into"] = typeInto }
            self.pending[self.send(req)] = { completion($0) }
        }
    }

    /// 历史提示词（`prompts.list`，M4a 追加块 9）：后台读完才应答，在读线程回调；未连接时立即以 nil 回调。
    func prompts(conversationID: Int64, completion: @escaping ([String: Any]?) -> Void) {
        queue.async {
            guard self.fd >= 0 else { return completion(nil) }
            self.pending[self.send(["type": "prompts.list", "conversation_id": conversationID])] = { completion($0) }
        }
    }

    /// 置顶 / 取消置顶项目（`project.update` 的 `pinned`，维护期 0.3.5）。未连接时不发；结果以 `project.upsert` 推送为准。
    func setPinned(projectID: Int64, pinned: Bool) {
        queue.async {
            if self.fd >= 0 { self.send(["type": "project.update", "project_id": projectID, "pinned": pinned]) }
        }
    }

    /// 「删除记录」（`conversation.delete`）。未连接时不发。
    func delete(conversationID: Int64) {
        queue.async {
            if self.fd >= 0 { self.send(["type": "conversation.delete", "conversation_id": conversationID]) }
        }
    }

    func terminalExited(id: String, exitCode: UInt32?) {
        queue.async {
            self.terminals[id] = nil
            var msg: [String: Any] = ["type": "terminal.exit", "terminal_id": id]
            if let exitCode { msg["exit_code"] = exitCode }
            if self.fd >= 0 { self.send(msg) }
        }
    }

    // MARK: - 连接（后台线程）

    private func run() {
        let bundled = bundledBuildID()
        DispatchQueue.main.async { self.appBuildID = bundled }
        var delay: useconds_t = 200_000
        var replaced = false
        while true {
            let fd = connectOrSpawn()
            guard fd >= 0 else {
                report("连接 scyllauid 失败（日志 \(logPath)），\(delay / 1000) ms 后重试")
                usleep(delay)
                delay = min(delay * 2, 2_000_000)
                continue
            }
            let reader = LineReader(fd: fd)
            writeLine(fd, ["type": "hello", "role": "app", "protocol_version": Self.protocolVersion,
                           "build_id": bundled ?? "unknown"])
            guard let hello = reader.next().flatMap(Self.parse) else {
                close(fd)
                usleep(delay)
                continue
            }
            let type = hello["type"] as? String, code = hello["code"] as? String
            let daemonBuild = hello["build_id"] as? String
            // 构建标识相同、位置不同（如正式版从构建目录换到 /Applications）也换新（M4b 块 2）；比较真实路径。
            let daemonExe = (hello["exe"] as? String).map { ($0 as NSString).resolvingSymlinksInPath }
            let moved = type == "hello_ok" && daemonExe != nil && daemonExe != (daemonPath as NSString).resolvingSymlinksInPath
            let stale = (type == "hello_ok" && bundled != nil && daemonBuild != bundled) || moved || code == "protocol_mismatch"
            if stale && !replaced {
                scyllauiLog.notice("scyllauid 需要换新（后台 \(daemonBuild ?? "-", privacy: .public) \(daemonExe ?? "-", privacy: .public)，包内 \(bundled ?? "-", privacy: .public) \(self.daemonPath, privacy: .public)）")
                replaced = true
                if hello["pty_host"] as? Bool == true {
                    upgrade(fd, reader: reader)
                } else {
                    shutdown(fd, reader: reader)
                }
                continue
            }
            if type != "hello_ok" {
                close(fd)
                report("scyllauid 拒绝连接：\(code ?? "-")（\(hello)）")
                return
            }
            if stale {
                scyllauiLog.error("换新后 build_id 仍不一致（后台 \(daemonBuild ?? "-", privacy: .public)），继续使用")
            }
            scyllauiLog.notice("scyllauid hello_ok build=\(daemonBuild ?? "-", privacy: .public) pid=\(String(describing: hello["pid"] ?? "-"), privacy: .public)")
            delay = 200_000
            DispatchQueue.main.async { self.daemonBuildID = daemonBuild }
            session(fd, reader: reader)
            DispatchQueue.main.async { self.daemonBuildID = nil }
            report("scyllauid 连接已断开，重连中")
        }
    }

    /// 登记为当前连接，上报全部终端，然后读到断开为止。
    private func session(_ fd: Int32, reader: LineReader) {
        queue.sync {
            self.fd = fd
            self.send(["type": "sync", "terminals": Array(self.terminals.values)])
            for (id, t) in self.terminals where t["initial_input"] != nil { self.terminals[id]?["initial_input"] = nil }
            if let a = self.appearance { self.send(a.merging(["type": "appearance.set"]) { x, _ in x }) }
            if let id = self.focused { self.send(["type": "terminal.focus", "terminal_id": id]) }
        }
        while let line = reader.next() {
            let received = Self.nowUs()
            guard var msg = Self.parse(line) else { continue }
            if msg["type"] as? String == "reply" {
                let callback = (msg["id"] as? Int).flatMap { id in queue.sync { self.pending.removeValue(forKey: id) } }
                if let callback {
                    callback(msg)
                    continue
                }
                guard msg["ok"] as? Bool == true else {
                    report("scyllauid 请求失败：\(msg)")
                    continue
                }
                guard let snapshot = msg["snapshot"] as? [String: Any] else { continue }
                msg = snapshot
                msg["type"] = "snapshot"
            }
            onMessage?(msg, received)
        }
        queue.sync {
            self.fd = -1
            self.pending.removeAll()
        }
        close(fd)
    }

    /// 让持有保活终端的旧后台原地 exec 包内新版本（`upgrade`，M3）：应答后旧连接随 exec 关闭，新进程重新监听同一 socket，
    /// 下一轮照常连接。自检不过等失败时记日志，下一轮仍连到旧后台并继续使用（`replaced` 已置位，不再重试）。
    private func upgrade(_ fd: Int32, reader: LineReader) {
        writeLine(fd, ["type": "upgrade", "id": 0, "path": daemonPath])
        let reply = reader.next().flatMap(Self.parse)
        if reply?["ok"] as? Bool == true {
            scyllauiLog.notice("scyllauid 接管中")
            _ = reader.next()
        } else {
            scyllauiLog.error("scyllauid 接管失败，继续使用旧后台：\(String(describing: reply?["error"] ?? "-"), privacy: .public)")
        }
        close(fd)
        // exec 之前旧进程仍在监听：稍等再连，避免连上即被关闭（连上了也只是多重连一次）。
        usleep(100_000)
    }

    /// 让旧版本后台退出，等 socket 消失（最多 2 秒），下一轮由 connectOrSpawn 拉起新版本。
    private func shutdown(_ fd: Int32, reader: LineReader) {
        writeLine(fd, ["type": "shutdown", "id": 0])
        _ = reader.next()
        close(fd)
        var waited = 0
        while FileManager.default.fileExists(atPath: socketPath) && waited < 100 {
            usleep(20_000)
            waited += 1
        }
    }

    private func connectOrSpawn() -> Int32 {
        let fd = connectSocket()
        if fd >= 0 { return fd }
        guard spawnDaemon() else { return -1 }
        for _ in 0..<200 {
            let fd = connectSocket()
            if fd >= 0 { return fd }
            usleep(10_000)
        }
        return -1
    }

    /// 新会话（setsid）启动；CLOEXEC_DEFAULT 只保留下面显式设置的 0/1/2（与 S0 KeepDaemon 相同，见 design.md 第 7 节 M3 约束）。
    private func spawnDaemon() -> Bool {
        rotateLog()
        var args = [daemonPath, "--data-dir", dataDir]
        // 采集版本样本：启动 App 时以 `open --env SCYLLAUI_CAPTURE_DIR=<目录>` 指定；只在本次拉起后台时生效。
        if let capture = ProcessInfo.processInfo.environment["SCYLLAUI_CAPTURE_DIR"], !capture.isEmpty {
            args += ["--capture", capture]
        }
        var attr: posix_spawnattr_t?
        posix_spawnattr_init(&attr)
        posix_spawnattr_setflags(&attr, Int16(POSIX_SPAWN_SETSID | POSIX_SPAWN_CLOEXEC_DEFAULT))
        var actions: posix_spawn_file_actions_t?
        posix_spawn_file_actions_init(&actions)
        posix_spawn_file_actions_addopen(&actions, 0, "/dev/null", O_RDONLY, 0)
        posix_spawn_file_actions_addopen(&actions, 1, logPath, O_WRONLY | O_CREAT | O_APPEND, 0o600)
        posix_spawn_file_actions_adddup2(&actions, 1, 2)
        let argv: [UnsafeMutablePointer<CChar>?] = args.map { strdup($0) } + [nil]
        defer { argv.forEach { free($0) } }
        var pid: pid_t = 0
        let rc = posix_spawn(&pid, daemonPath, &actions, &attr, argv, environ)
        posix_spawn_file_actions_destroy(&actions)
        posix_spawnattr_destroy(&attr)
        guard rc == 0 else {
            scyllauiLog.error("scyllauid 启动失败 rc=\(rc)")
            return false
        }
        scyllauiLog.notice("scyllauid 启动 pid=\(pid) data=\(self.dataDir, privacy: .public)")
        return true
    }

    /// 日志超过 10 MB 时先改名为 `.1`（覆盖上一份）。
    private func rotateLog() {
        let fm = FileManager.default
        try? fm.createDirectory(atPath: (logPath as NSString).deletingLastPathComponent, withIntermediateDirectories: true)
        if let size = (try? fm.attributesOfItem(atPath: logPath))?[.size] as? Int, size > 10 << 20 {
            let old = logPath + ".1"
            try? fm.removeItem(atPath: old)
            try? fm.moveItem(atPath: logPath, toPath: old)
        }
    }

    /// 包内 scyllauid 的构建标识，用于识别旧版本后台。
    private func bundledBuildID() -> String? {
        let p = Process()
        p.executableURL = URL(fileURLWithPath: daemonPath)
        p.arguments = ["--build-id"]
        let out = Pipe()
        p.standardOutput = out
        do { try p.run() } catch { return nil }
        let data = out.fileHandleForReading.readDataToEndOfFile()
        p.waitUntilExit()
        let id = String(decoding: data, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
        return p.terminationStatus == 0 && !id.isEmpty ? id : nil
    }

    // MARK: - 收发

    /// 只在 queue 上调用；返回请求 id。
    @discardableResult
    private func send(_ obj: [String: Any]) -> Int {
        nextID += 1
        var obj = obj
        obj["id"] = nextID
        writeLine(fd, obj)
        return nextID
    }

    private func writeLine(_ fd: Int32, _ obj: [String: Any]) {
        guard var data = try? JSONSerialization.data(withJSONObject: obj) else { return }
        data.append(0x0A)
        data.withUnsafeBytes { raw in
            var off = 0
            while off < raw.count {
                let n = write(fd, raw.baseAddress! + off, raw.count - off)
                if n <= 0 { return }
                off += n
            }
        }
    }

    private static func parse(_ line: Data) -> [String: Any]? {
        (try? JSONSerialization.jsonObject(with: line)) as? [String: Any]
    }

    private func report(_ text: String) {
        scyllauiLog.error("\(text, privacy: .public)")
        DispatchQueue.main.async { [weak self] in self?.onError?(text) }
    }

    static func nowUs() -> UInt64 {
        var tv = timeval()
        gettimeofday(&tv, nil)
        return UInt64(tv.tv_sec) * 1_000_000 + UInt64(tv.tv_usec)
    }

    private func connectSocket() -> Int32 {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return -1 }
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(socketPath.utf8)
        guard bytes.count < MemoryLayout.size(ofValue: addr.sun_path) else { close(fd); return -1 }
        withUnsafeMutableBytes(of: &addr.sun_path) { raw in
            raw.copyBytes(from: bytes)
            raw[bytes.count] = 0
        }
        let ok = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        if ok != 0 { close(fd); return -1 }
        var one: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout<Int32>.size))
        return fd
    }
}

/// 从 socket 按行读取（阻塞，只在后台线程使用）。
private final class LineReader {
    private let fd: Int32
    private var buffer = Data()
    private var chunk = [UInt8](repeating: 0, count: 64 << 10)

    init(fd: Int32) { self.fd = fd }

    /// 下一行（不含换行符）；对端关闭或出错时返回 nil。
    func next() -> Data? {
        while true {
            if let nl = buffer.firstIndex(of: 0x0A) {
                let line = buffer.subdata(in: buffer.startIndex..<nl)
                buffer.removeSubrange(buffer.startIndex...nl)
                return line
            }
            let n = read(fd, &chunk, chunk.count)
            if n <= 0 { return nil }
            buffer.append(chunk, count: n)
        }
    }
}
