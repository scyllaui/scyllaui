// 与 scyllauid 连接上按行读（JSON Lines，docs/protocol.md）：CoreClient 的读线程用；单独成文件好进单测。

import Foundation

/// 从 socket 按行读取（阻塞，只在后台线程使用）。
final class LineReader {
    private let fd: Int32
    private var buffer = Data()
    private var chunk = [UInt8](repeating: 0, count: 64 << 10)
    /// `buffer` 开头这么多字节里已确认没有换行：每读进一块只在新的部分里找。原来每次都从头找，2.7 MB 的一行（Diff 的大文件）
    /// 要找四十多遍，测试版（Debug）里光找换行就 1.5—2.8 秒（2026-11-25 Diff 改版 D4 打点）。
    private var scanned = 0

    init(fd: Int32) { self.fd = fd }

    /// 下一行（不含换行符）；对端关闭或出错时返回 nil。
    func next() -> Data? {
        while true {
            if let off = newline(after: scanned) {
                let nl = buffer.startIndex + off
                let line = buffer.subdata(in: buffer.startIndex..<nl)
                buffer.removeSubrange(buffer.startIndex...nl)
                scanned = 0
                return line
            }
            scanned = buffer.count
            let n = read(fd, &chunk, chunk.count)
            if n <= 0 { return nil }
            buffer.append(chunk, count: n)
        }
    }

    /// 从第 `offset` 个字节起第一个换行的位置（相对开头）。
    private func newline(after offset: Int) -> Int? {
        buffer.withUnsafeBytes { raw -> Int? in
            guard let base = raw.baseAddress, raw.count > offset, let p = memchr(base + offset, 0x0A, raw.count - offset) else { return nil }
            return base.distance(to: UnsafeRawPointer(p))
        }
    }
}
