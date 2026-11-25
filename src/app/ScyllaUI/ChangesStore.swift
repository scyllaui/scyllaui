// 改动列表（docs/design.md 5.12 节「Diff」）：按项目根保存最新一次 `git.changes` 的结果，Diff 栏与文件树的 M、A 标记共用。
// 同一个根的请求同时只有一个在途（`Latest`），期间又有变化就记一笔，应答回来后补取一次。只在主线程用。

import Foundation

final class ChangesStore {
    struct Item: Equatable {
        let path: String
        /// `M`、`A`、`D`。
        let status: String
        let added: Int?
        let deleted: Int?
        let binary: Bool
        let untracked: Bool
    }

    struct Snapshot: Equatable {
        var items: [Item] = []
        var more = 0
        /// 读不到的原因（后台的错误码；nil 为读到了）。
        var error: String?
        /// 与哪个比：`HEAD` 或 `empty`（还没有提交）。
        var base = "HEAD"
        var truncated = false

        var totals: (added: Int, deleted: Int) {
            items.reduce((0, 0)) { ($0.0 + ($1.added ?? 0), $0.1 + ($1.deleted ?? 0)) }
        }
    }

    /// 发后台请求；回调在主线程，nil 为没连上或断线。
    var request: (([String: Any], @escaping ([String: Any]?) -> Void) -> Void)?
    /// 某个根的结果变了。
    var onUpdate: ((String) -> Void)?

    private(set) var snapshots: [String: Snapshot] = [:]
    private var latest: [String: Latest] = [:]

    func snapshot(_ root: String) -> Snapshot? { snapshots[root] }

    /// 重取这个根的改动列表（有变化时、打开 Diff 或文件栏时）。
    func refresh(root: String) {
        let l = latest[root] ?? Latest()
        latest[root] = l
        l.submit { [weak self] done in
            guard let self, let request = self.request else { return done() }
            request(["type": "git.changes", "root": root]) { [weak self] reply in
                defer { done() }
                guard let self else { return }
                let snap = Self.parse(reply)
                guard snap != self.snapshots[root] else { return }
                self.snapshots[root] = snap
                self.onUpdate?(root)
            }
        }
    }

    static func parse(_ reply: [String: Any]?) -> Snapshot {
        guard let r = reply else { return Snapshot(error: "unavailable") }
        guard r["ok"] as? Bool == true else {
            return Snapshot(error: (r["error"] as? [String: Any])?["code"] as? String ?? "internal")
        }
        let items = (r["items"] as? [[String: Any]] ?? []).compactMap { d -> Item? in
            guard let p = d["path"] as? String else { return nil }
            return Item(path: p, status: d["st"] as? String ?? "M", added: (d["add"] as? NSNumber)?.intValue,
                        deleted: (d["del"] as? NSNumber)?.intValue, binary: d["binary"] as? Bool ?? false,
                        untracked: d["untracked"] as? Bool ?? false)
        }
        return Snapshot(items: items, more: (r["more"] as? NSNumber)?.intValue ?? 0, error: nil,
                        base: r["base"] as? String ?? "HEAD", truncated: r["truncated"] as? Bool ?? false)
    }

    /// 文件树用：相对根的路径 → 状态字母（删除的不在树上，不给）。
    func marks(_ root: String) -> [String: String] {
        var out: [String: String] = [:]
        for i in snapshots[root]?.items ?? [] where i.status != "D" { out[i.path] = i.status }
        return out
    }

    /// 文件树用：有改动的文件所在的各级目录（相对根）。
    func dirtyDirs(_ root: String) -> Set<String> {
        var out = Set<String>()
        for i in snapshots[root]?.items ?? [] {
            var d = (i.path as NSString).deletingLastPathComponent
            while !d.isEmpty {
                out.insert(d)
                d = (d as NSString).deletingLastPathComponent
            }
        }
        return out
    }
}
