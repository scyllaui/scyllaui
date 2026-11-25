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
