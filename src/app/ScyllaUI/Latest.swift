// 「只留最新」（docs/design.md 5.12 节）：同一种请求同时只有一个在途；在途期间又要发，只记最后一次，应答回来后再发它。
// 右侧栏的目录读取、筛选、改动列表与内容搜索都用它，避免文件频繁变化时请求堆积，也避免旧应答盖住新结果。只在主线程用。

import Foundation

final class Latest {
    /// 一次请求：做完（应答已处理）时调用传入的 `done`。
    typealias Job = (_ done: @escaping () -> Void) -> Void

    private var busy = false
    private var queued: Job?

    /// 有请求在途。
    var isBusy: Bool { busy }

    func submit(_ job: @escaping Job) {
        guard !busy else {
            queued = job
            return
        }
        busy = true
        var finished = false
        job { [weak self] in
            // 同一次请求只认第一次 done，重复调用不会把后面排着的请求发两遍。
            guard !finished else { return }
            finished = true
            self?.finish()
        }
    }

    /// 丢掉排着的请求（换项目根、关栏时）；在途的那个照常回来，由调用方按需忽略。
    func cancelQueued() {
        queued = nil
    }

    private func finish() {
        busy = false
        if let next = queued {
            queued = nil
            submit(next)
        }
    }
}

/// 右侧栏三栏（文件、更改、状态栏的「更改」）发往后台的请求共用的闸（docs/design.md 5.12 节「同时在途上限」）：
/// 后台对 `fs.*`、`git.*` 同时在途超过 8 个回 `busy`，所以这里同时最多发 8 个，多出的按先后排队；仍回 `busy`
/// （如重连后旧连接的请求还在做）时暂停 `retryDelay` 秒再从这一个起重发，`busy` 不交给调用方。只在主线程用。
final class RequestGate {
    typealias Send = ([String: Any], @escaping ([String: Any]?) -> Void) -> Void
    /// 同后台 `files.rs` 的 `MAX_INFLIGHT`（docs/protocol.md 第 4 节）。
    static let limit = 8
    var retryDelay: TimeInterval = 0.2

    private let send: Send
    private var inflight = 0
    private var paused = false
    private var waiting: [(req: [String: Any], done: ([String: Any]?) -> Void)] = []

    init(_ send: @escaping Send) { self.send = send }

    func request(_ req: [String: Any], _ done: @escaping ([String: Any]?) -> Void) {
        waiting.append((req, done))
        pump()
    }

    private func pump() {
        while !paused, inflight < Self.limit, !waiting.isEmpty {
            let job = waiting.removeFirst()
            inflight += 1
            send(job.req) { [weak self] reply in
                guard let self else { return job.done(reply) }
                self.inflight -= 1
                if (reply?["error"] as? [String: Any])?["code"] as? String == "busy" {
                    self.waiting.insert(job, at: 0)
                    if !self.paused {
                        self.paused = true
                        DispatchQueue.main.asyncAfter(deadline: .now() + self.retryDelay) { [weak self] in
                            self?.paused = false
                            self?.pump()
                        }
                    }
                    return
                }
                job.done(reply)
                self.pump()
            }
        }
    }
}
