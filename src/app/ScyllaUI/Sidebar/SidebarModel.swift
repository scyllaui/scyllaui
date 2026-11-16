// 侧栏数据模型（M1 第 3 块，docs/design.md 第 5 节）：scyllauid 的快照、推送与 recent.query 分页 → 两区按项目分组的树，
// 以及相邻两棵树之间的差分。只依赖 Foundation，在后台串行队列中使用，可单测（ScyllaUITests）；主线程只执行差分结果。

import Foundation

// MARK: - 协议对象（docs/protocol.md 第 5 节）

struct SBProject: Equatable {
    let id: Int64
    var dir: String
    var name: String
    var sortOrder: Int64
    /// 当前 Git 分支（不是仓库为 nil）。
    var branch: String?
    /// 工作区改动数（M2 第 2 块）：只有标题栏显示的项目带，读不到或还没算出为 nil。
    var git: SBGit? = nil
    /// 置顶（维护期 0.3.5）：「当前任务」里排在最前；旧后台没有该字段，按 false。
    var pinned = false
}

/// 工作区改动数（docs/protocol.md 第 5 节项目的 `git`）。
struct SBGit: Equatable {
    var dirty: Bool
    var added: Int64
    var deleted: Int64
    var untracked: Int64
}

/// 对话的用量（docs/protocol.md 第 5 节「用量」）：模型与会话累计 token（M2 第 3 块）、推理强度（维护期 0.3.5）、计费方式
/// （2026-11-01，`api` 为按量计费，看不出时为 nil）。
struct SBUsage: Equatable {
    var model: String?
    var total: Int64?
    var effort: String? = nil
    var billing: String? = nil
    /// 输入（不含缓存）、缓存读、缓存写、输出。
    var input: Int64 = 0, cacheRead: Int64 = 0, cacheWrite: Int64 = 0, output: Int64 = 0

    init(model: String? = nil, total: Int64? = nil) {
        self.model = model
        self.total = total
    }

    init(json d: [String: Any]) {
        model = str(d, "model")
        total = int(d, "total_tokens")
        effort = str(d, "effort")
        billing = str(d, "billing")
        if let t = d["tokens"] as? [String: Any] {
            input = int(t, "input") ?? 0
            cacheRead = int(t, "cache_read") ?? 0
            cacheWrite = int(t, "cache_write") ?? 0
            output = int(t, "output") ?? 0
        }
    }
}

/// 某款 CLI 账号一个窗口的额度（docs/protocol.md 第 5 节「额度」，M2 第 3 块；窗口有 `5h`、`7d` 与 `month`）；时间为 Unix 毫秒。
/// `unlimited` 为来源明确写了这个窗口不限制（目前只有 Copilot），状态行不显示（2026-11-01 统一规则）。
struct SBQuota: Equatable {
    var usedPercent: Double
    var resetsAt: Int64
    var asOf: Int64
    var source: String
    var window = "7d"
    var unlimited = false

    init(usedPercent: Double, resetsAt: Int64, asOf: Int64, source: String, window: String = "7d", unlimited: Bool = false) {
        (self.usedPercent, self.resetsAt, self.asOf, self.source) = (usedPercent, resetsAt, asOf, source)
        (self.window, self.unlimited) = (window, unlimited)
    }

    init?(json d: [String: Any]) {
        let window = str(d, "window") ?? "7d"
        if d["unlimited"] as? Bool == true, let a = int(d, "as_of") {
            self.init(usedPercent: 0, resetsAt: 0, asOf: a, source: str(d, "source") ?? "", window: window, unlimited: true)
            return
        }
        guard let p = (d["used_percent"] as? NSNumber)?.doubleValue, let r = int(d, "resets_at"), let a = int(d, "as_of") else { return nil }
        self.init(usedPercent: p, resetsAt: r, asOf: a, source: str(d, "source") ?? "", window: window)
    }
}

/// 状态行的段（M4a，docs/design.md 10.4 节决策 3）：设置「状态栏」里每段一个开关，默认全开。推理强度只跟在模型后面，
/// 模型关闭时一并不显示。`quota5h` 为 5 小时额度（2026-10-31 加），`quota` 为 7 天周额度，`quotaMonth` 为月额度、`api` 为
/// 按量计费的标记（2026-11-01 加）。
enum StatusSegment: String, CaseIterable {
    case project, git, total, quota5h, quota, quotaMonth, api, model, effort, cpu, mem
}

/// 标题栏状态行（M2 第 2 块，docs/design.md 第 5 节「实现（M2 第 2 块）」）里一个终端的项目、分支、改动数与模型；
/// CPU / 内存由工作区按 `proc.stats` 另行填入，0.3.5 起显示在标题栏右侧（`statsSpans`）。
struct StatusInfo: Equatable {
    var projectID: Int64
    var project: String
    /// 项目目录：悬停项目名时显示完整路径（2026-11-01）。
    var projectDir = ""
    var branch: String?
    var git: SBGit?
    /// 只有这一栏显示的是 AI、且绑定的对话就是这款 AI 时才有。
    var model: String?
    /// 推理强度（维护期 0.3.5），条件同 `model`；只在有模型时显示。
    var effort: String? = nil
    /// 以下同上（M2 第 3 块）：这一栏显示的 AI、它的会话累计 token、它账号的 7 天、5 小时与月额度（5 小时 2026-10-31 加，
    /// 月 2026-11-01 加）。
    var tool: String? = nil
    var usage: SBUsage? = nil
    var quota: SBQuota? = nil
    var quota5h: SBQuota? = nil
    var quotaMonth: SBQuota? = nil
    /// 这款查不到额度的原因代码（`quota.note`，2026-11-01）：`login_required` 为 Grok 没登录 grok.com。
    var quotaNote: String? = nil
    /// 前台识别出的 AI（工具键），不论状态同步是否生效、状态行不显示：「在 Codex 里打开 /hooks」据此找正在运行 Codex 的终端
    /// （M4b 块 5 P1，docs/design.md 10.6 节）。
    var fgAgent: String? = nil
    /// 这款 AI 能否读出 token 用量（快照的 `tools`）：不能时悬停提示写明「暂不支持」（docs/ui-methodology.md 1.7 节）。
    var usageSupported = true
    /// 同上，能否读出模型（2026-10-30 起快照的 `tools` 带 `model`）。
    var modelSupported = true
    /// 同上，能否读出推理强度（2026-11-01 起快照的 `tools` 带 `effort`）。
    var effortSupported = true

    /// 状态行一小段文字的用途，界面按它取色（docs/design.md 第 5 节「实现（M2 第 2 块）」位置与样式，2026-10-18 改版）。
    enum Role: Equatable { case project, muted, separator, added, deleted, value, accent, quotaLow, quotaMid, quotaHigh }
    typealias Span = (text: String, role: Role)

    /// 左侧的一段（2026-11-01 标准化）：用途、文字片段与这一段自己的悬停文字（没有为 nil，docs/ui-methodology.md 1.8 节：
    /// 悬停挂在那段文字上，不挂整行）。组间、组内的分隔也是一段（`gap`）。
    struct Segment {
        enum Kind: Equatable { case project, branch, total, quota5h, quota7d, quotaMonth, api, model, gap }
        var kind: Kind
        var spans: [Span]
        var tooltip: String? = nil
    }

    /// 放不下时的收缩（`fitted` 依次加码）：项目名、分支最多显示几个字符（nil 为不限），以及还保留哪些部分。
    struct Trim: Equatable {
        var project: Int? = nil
        var branch: Int? = nil
        var countdown = true
        var effort = true
        var changes = true
        var total = true
        var model = true
    }

    /// 组间与组内的分隔。
    static let groupGap = "   "
    static let itemSeparator = " • "
    /// 收缩时项目名与分支至少保留的字符数（含省略号）。
    static let projectMin = 12
    static let branchMin = 8

    static func nowMs() -> Int64 { Int64(Date().timeIntervalSince1970 * 1000) }

    /// 标题栏左侧分三组：`[项目] (分支* +N -M ?K)`｜`Total: 1.2M • 5h: 5% (1h17m) • 7d: 34% (2d5h)`｜`Model: … (xhigh)`；
    /// CPU / Mem 在右侧（`statsSpans`，维护期 0.3.5）。额度按统一规则（2026-11-01）：数据里有的
    /// 窗口才显示，不限制的、已过重置时间的不显示；这个会话按量计费时额度位置只显示 `API`。读不到的段整段省略、整组都没有时
    /// 整组省略，为 0 的改动项不显示，推理强度只跟在模型后面。`hidden` 为设置里关掉的段（M4a），`trim` 为放不下时的收缩。
    func segments(now: Int64 = nowMs(), hidden: Set<StatusSegment> = [], trim: Trim = Trim()) -> [Segment] {
        let name = tool.map(SidebarModel.toolName)
        var head: [Segment] = []
        if !hidden.contains(.project) {
            let tip = projectDir.isEmpty ? nil : (projectDir as NSString).abbreviatingWithTildeInPath
            head.append(Segment(kind: .project, spans: [("[\(StatusInfo.middle(project, trim.project))]", .project)], tooltip: tip))
        }
        if let branch, !hidden.contains(.git) {
            let shown = StatusInfo.middle(branch, trim.branch)
            var spans: [Span] = [((head.isEmpty ? "(" : " (") + shown + (git?.dirty == true ? "*" : ""), .muted)]
            if let g = git, trim.changes {
                if g.added > 0 { spans.append((" +\(g.added)", .added)) }
                if g.deleted > 0 { spans.append((" -\(g.deleted)", .deleted)) }
                if g.untracked > 0 { spans.append((" ?\(g.untracked)", .muted)) }
            }
            spans.append((")", .muted))
            head.append(Segment(kind: .branch, spans: spans, tooltip: shown == branch ? nil : branch))
        }
        var usageItems: [Segment] = []
        if !hidden.contains(.total), trim.total {
            if let total = usage?.total {
                usageItems.append(Segment(kind: .total, spans: [("Total: ", .muted), (StatusInfo.tokens(total), .value)]))
            } else if !usageSupported, let name {
                usageItems.append(Segment(kind: .total, spans: [("Total: ", .muted), ("-", .muted)],
                                          tooltip: String(localized: "Total：ScyllaUI 暂不读取 \(name) 的用量")))
            }
        }
        var quotaShown = false
        if usage?.billing == "api" {
            quotaShown = true
            if !hidden.contains(.api) {
                usageItems.append(Segment(kind: .api, spans: [("API", .muted)], tooltip: String(localized: "按量计费，没有 5 小时、7 天或月额度")))
            }
        } else {
            let windows: [(SBQuota?, String, StatusSegment, Segment.Kind)] = [
                (quota5h, "5h: ", .quota5h, .quota5h), (quota, "7d: ", .quota, .quota7d), (quotaMonth, "Month: ", .quotaMonth, .quotaMonth),
            ]
            for (q, label, segment, kind) in windows {
                guard let q, !q.unlimited, q.resetsAt > now else { continue }
                quotaShown = true
                guard !hidden.contains(segment) else { continue }
                var spans: [Span] = [(label, .muted), (String(format: "%.0f%%", q.usedPercent), StatusInfo.quotaRole(q.usedPercent))]
                if trim.countdown { spans.append((" (\(StatusInfo.remaining(q.resetsAt - now)))", .muted)) }
                usageItems.append(Segment(kind: kind, spans: spans, tooltip: quotaTip(q)))
            }
        }
        var runItems: [Segment] = []
        if !hidden.contains(.model), trim.model {
            if let model {
                var spans: [Span] = [("Model: ", .muted), (model, .accent)]
                var tip: String?
                if !hidden.contains(.effort) {
                    if let effort, trim.effort { spans.append((" (\(effort))", .muted)) }
                    if effort == nil, !effortSupported, let name { tip = String(localized: "推理强度：ScyllaUI 暂不读取 \(name) 的推理强度") }
                }
                runItems.append(Segment(kind: .model, spans: spans, tooltip: tip))
            } else if !modelSupported, let name {
                runItems.append(Segment(kind: .model, spans: [("Model: ", .muted), ("-", .muted)],
                                        tooltip: String(localized: "模型：ScyllaUI 暂不读取 \(name) 的模型")))
            }
        }
        var out = head
        for items in [usageItems, runItems] where !items.isEmpty {
            if !out.isEmpty { out.append(Segment(kind: .gap, spans: [(StatusInfo.groupGap, .separator)])) }
            for (i, item) in items.enumerated() {
                if i > 0 { out.append(Segment(kind: .gap, spans: [(StatusInfo.itemSeparator, .separator)])) }
                out.append(item)
            }
        }
        // 额度一段也没有时，原因挂在 Total 上，没有 Total 挂在模型上，再没有挂在项目名上。
        if !quotaShown, let note = quotaMissingNote(),
           let i = out.firstIndex(where: { $0.kind == .total }) ?? out.firstIndex(where: { $0.kind == .model })
               ?? out.firstIndex(where: { $0.kind == .project }) {
            out[i].tooltip = [out[i].tooltip, note].compactMap { $0 }.joined(separator: "\n")
        }
        return out
    }

    /// 按可用宽度排版（2026-11-01 标准化，docs/design.md 第 5 节）：放得下原样；放不下先把项目名截到最少 12 个字符、再把分支
    /// 截到最少 8 个（都截在中间，悬停看全），还放不下依次去掉重置倒计时、推理强度、改动数、Total、模型；之后有余量再把分支、
    /// 项目名放回能放下的最长。项目名与额度百分比留到最后，仍放不下的由文字框从尾部截断。`measure` 给出一串片段显示出来的宽度。
    func fitted(width available: CGFloat, now: Int64 = nowMs(), hidden: Set<StatusSegment> = [],
                measure: ([Span]) -> CGFloat) -> [Segment] {
        func build(_ t: Trim) -> [Segment] { segments(now: now, hidden: hidden, trim: t) }
        func fits(_ t: Trim) -> Bool { measure(build(t).flatMap(\.spans)) <= available }
        var trim = Trim()
        if fits(trim) { return build(trim) }
        let elastic: [(WritableKeyPath<Trim, Int?>, Int, Int)] = [
            (\.project, StatusInfo.projectMin, project.count), (\.branch, StatusInfo.branchMin, branch?.count ?? 0),
        ]
        for (path, minimum, full) in elastic where full > minimum {
            trim[keyPath: path] = minimum
        }
        if !fits(trim) {
            for drop: WritableKeyPath<Trim, Bool> in [\.countdown, \.effort, \.changes, \.total, \.model] {
                trim[keyPath: drop] = false
                if fits(trim) { break }
            }
        }
        // 有余量时放回能放下的最长：先分支、再项目名，所以放不下时还是项目名先让（2026-11-02 正式版验收发现：让出 Total、
        // 模型后项目名仍停在最短，后面空着一段）。
        for (path, minimum, full) in elastic.reversed() where full > minimum && fits(trim) {
            var (lo, hi) = (minimum, full)
            while lo < hi {
                var t = trim
                t[keyPath: path] = (lo + hi + 1) / 2
                if fits(t) { lo = (lo + hi + 1) / 2 } else { hi = (lo + hi + 1) / 2 - 1 }
            }
            trim[keyPath: path] = lo == full ? nil : lo
        }
        return build(trim)
    }

    /// 全部片段（单测与无障碍用）。
    func spans(now: Int64 = nowMs(), hidden: Set<StatusSegment> = []) -> [Span] {
        segments(now: now, hidden: hidden).flatMap(\.spans)
    }

    /// 纯文字（单测与无障碍用）。
    func text(now: Int64 = nowMs()) -> String {
        spans(now: now).map(\.text).joined()
    }

    /// 截在中间、总共 `n` 个字符（含省略号）；nil 或放得下时原样。
    static func middle(_ s: String, _ n: Int?) -> String {
        guard let n, n >= 2, s.count > n else { return s }
        let head = n / 2
        return String(s.prefix(head)) + "…" + String(s.suffix(n - 1 - head))
    }

    /// 额度一段的悬停：行上只有距重置多久，这里写重置的具体时间；Grok 的额度按当前登录的账号查，与会话无关时也会显示，写明。
    private func quotaTip(_ q: SBQuota) -> String {
        let at = StatusInfo.resetFormatter.string(from: Date(timeIntervalSince1970: TimeInterval(q.resetsAt) / 1000))
        var lines = [String(localized: "重置时间：\(at)")]
        if tool == "grok" { lines.append(String(localized: "按当前登录的 grok.com 账号")) }
        return lines.joined(separator: "\n")
    }

    private static let resetFormatter: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("MMMd HH:mm")
        return f
    }()

    /// 一段额度也没有时的原因（docs/design.md 第 5 节「实现（M2 第 3 块）」，2026-11-01 改）：Claude 只经 statusline 桥，
    /// OpenCode 暂不读取，Grok 没登录 grok.com 查不到；其余各款只是还没读到（另起命令查的在 Agent 启动或一轮结束后读，
    /// 读会话记录的在第一轮回答后才有）。
    private func quotaMissingNote() -> String? {
        guard let tool else { return nil }
        switch (tool, quotaNote) {
        case ("claude", _): return String(localized: "额度：Claude 的 5 小时与 7 天额度只在官方 statusline 里，未接入（scyllaui statusline-bridge install）；非 Pro / Max 订阅没有此项")
        case ("opencode", _): return String(localized: "额度：ScyllaUI 暂不读取 OpenCode 的额度")
        case ("grok", "login_required"): return String(localized: "额度：查 Grok 的额度要先登录 grok.com（在终端运行 grok login）")
        default: return String(localized: "额度：还没读到。Agent 启动或一轮结束后读取；用第三方 API 时没有额度")
        }
    }

    /// 标题栏右侧（维护期 0.3.5）：`CPU: 12% • Mem: 840 MB`，口径为当前终端的 Shell 及其所有子进程；都没有时为空。
    static func statsSpans(cpu: Double?, mem: Int64?, hidden: Set<StatusSegment> = []) -> [Span] {
        var items: [[Span]] = []
        if let cpu, !hidden.contains(.cpu) { items.append([("CPU: ", .muted), (String(format: "%.0f%%", cpu), .accent)]) }
        if let mem, !hidden.contains(.mem) { items.append([("Mem: ", .muted), (bytes(mem), .accent)]) }
        var out: [Span] = []
        for (i, item) in items.enumerated() {
            if i > 0 { out.append((itemSeparator, .separator)) }
            out += item
        }
        return out
    }

    /// 周额度已用百分比的颜色：< 50% 绿、50%—80% 黄、≥ 80% 红（风险语义）。
    static func quotaRole(_ percent: Double) -> Role {
        percent >= 80 ? .quotaHigh : percent >= 50 ? .quotaMid : .quotaLow
    }

    /// 右侧的悬停提示：统计口径（2026-10-16 用户确认：Shell 及其所有子进程，不含 root 的 login）。还没有采样时为 nil。
    static func statsTooltip(cpu: Double?, mem: Int64?, processes: Int) -> String? {
        guard let mem else { return nil }
        let cpuText = cpu.map { String(format: "%.1f%%", $0) } ?? String(localized: "计算中")
        return [String(localized: "Shell 及其所有子进程（\(processes) 个）：CPU \(cpuText)，内存 \(bytes(mem))"),
                String(localized: "不含系统以 root 身份运行的登录进程 login（约 3 MB）")].joined(separator: "\n")
    }

    /// `1234`、`12.3K`、`1.2M`。
    static func tokens(_ n: Int64) -> String {
        switch n {
        case ..<1000: return "\(n)"
        case ..<999_950: return String(format: "%.1fK", Double(n) / 1000)
        default: return String(format: "%.1fM", Double(n) / 1_000_000)
        }
    }

    /// 距重置的时间：满 1 天为「2d5h」，不足 1 天为「5h12m」。
    static func remaining(_ ms: Int64) -> String {
        let m = ms / 60_000
        return m >= 1440 ? "\(m / 1440)d\(m % 1440 / 60)h" : "\(m / 60)h\(m % 60)m"
    }

    static func bytes(_ n: Int64) -> String {
        let mb = Double(n) / 1_048_576
        return mb >= 1024 ? String(format: "%.1f GB", mb / 1024) : String(format: "%.0f MB", mb)
    }
}

struct SBTerminal: Equatable {
    let id: String
    var projectID: Int64
    var cwd: String
    /// 当前目录（`terminal.cwd` 上报后才有）；侧栏副标题与「在此目录新建」用它，没有时用启动目录。
    var currentDir: String?
    var name: String?
    var createdAt: Int64
    var conversationID: Int64?
    var unseen: Bool
    /// 同一对话同时打开的其他终端（后台的 `conflict_with`，M1 第 6 块）。
    var conflictWith: [String] = []
    var agentTool: String?
    var agentState: String
    var sessionID: String?
    var fgKind: String
    var fgTool: String?
    var fgPID: Int64?
    var fgPath: String?
    /// 前台 AI 进程的启动时间（Unix 毫秒，M1 第 6 块）：还没提问、会话 ID 未知时显示「新会话 · 启动时间」。
    var fgStartedAt: Int64? = nil
    /// 前台 CLI 的版本（M1 第 7 块）；读不到或运行中被升级时为 nil。
    var fgVersion: String? = nil
    /// 这个 AI 进程最近一次 Hook 数据异常的原因代码（M4b 块 6，docs/protocol.md 第 5 节 `foreground.anomaly`）。
    var fgAnomaly: String? = nil
    /// 后台的终端提示（M3）：`cwd_denied`；`cwd_missing`（启动目录不存在、已在主目录打开，2026-11-13）。
    var notice: String? = nil
}

/// 一款 CLI 的 Hook 配置状态（docs/protocol.md 第 5 节「Hook 状态」，M1 第 7 块）。判定规则在后台，这里只显示。
struct SBHookStatus: Equatable {
    let tool: String
    var state: String
    var problems: [String]
    var fixable: Bool
    /// 已验证清单里这款最新的版本（M4b 块 6，「状态可能不准」详情里的退回参考）。
    var verifiedLatest: String? = nil

    /// 「状态同步未生效」：除了正常与没装，都要提示。
    var broken: Bool { state != "ok" && state != "absent" }

    /// 条目都在、只是要更新（重复、旧写法、自建文件不是当前版本）：后台仍归在 `missing`，界面上单独说（2026-11-16）。
    var outdatedOnly: Bool {
        state == "missing" && !problems.contains { $0.hasPrefix("missing:") } && problems.contains { $0.hasPrefix("outdated:") }
    }

    /// 悬停提示与确认框里的说明。不能修复的两种带上具体原因，用户才知道去看哪里（`problems` 的写法见 docs/protocol.md 第 5 节）。
    var reason: String {
        func after(_ prefix: String) -> String? {
            problems.first { $0.hasPrefix(prefix) }.map { String($0.dropFirst(prefix.count)) }
        }
        switch state {
        case "missing": return outdatedOnly ? String(localized: "Hook 条目需要更新") : String(localized: "Hook 条目缺失")
        case "disabled": return String(localized: "Hook 被停用")
        case "untrusted": return String(localized: "Hook 尚未在 Codex 里信任")
        case "policy":
            return after("policy:").map { String(localized: "被组织策略禁用（\($0)），无法修复") } ?? String(localized: "被组织策略禁用，无法修复")
        case "unreadable":
            if problems.contains("hooks_inline") { return String(localized: "hooks 写成了行内数组（hooks = […]），ScyllaUI 只支持 [[hooks]] 写法，未改动") }
            if let e = after("parse:") { return String(localized: "配置文件格式有误（\(e)），未改动") }
            if let e = after("read:") { return String(localized: "配置文件读不了（\(e)），未改动") }
            return String(localized: "配置文件读不了，未改动")
        default: return state
        }
    }
}

/// 侧栏底部的提示栏（docs/design.md 4.7 节「侧栏提示」）：只在有问题时出现。2026-10-28 起不再列状态同步未生效的工具，
/// 入口只留终端行的「未同步」（10.6 节「块 6 追加」）。
struct HookBanner: Equatable {
    /// 状态可能不准的工具（M4b 块 6）：正在某个终端里运行、发来的 Hook 数据有异常的，按 Claude、Codex、Kimi、Grok、OpenCode、Copilot、Antigravity 的顺序；
    /// 「详情…」显示其内容。只是版本不在已验证清单里不算。
    var details: [InaccurateItem] = []

    var inaccurate: [String] { details.map { SidebarModel.toolName($0.tool) } }
    var isEmpty: Bool { details.isEmpty }
}

/// 「状态可能不准」详情里的一款工具（M4b 块 6）。
struct InaccurateItem: Equatable {
    let tool: String
    /// 有异常的终端里正在运行的版本（读不到时为空）。
    let versions: [String]
    let reasons: [String]
    /// 已验证清单里这款最新的版本。
    let verifiedLatest: String?
}

/// 重建时没能接回的 AI（M4b 块 5 P2，docs/design.md 10.6 节）：终端原来前台在跑某款 AI，却没有可恢复的对话，已恢复为 Shell。
/// 按这款工具此刻的状态同步分两类：未生效的（缺失、停用、待信任等）说明原因并给「修复…」；正常的多半还没开始对话
/// （Codex、Kimi 首次提问前没有 Hook），只附带说一句，不算故障。用户在设置里撤销过的、状态未知的（没装等）不说。
struct LostAgents: Equatable {
    struct Item: Equatable {
        let tool: String
        let count: Int
    }
    /// 按 Claude、Codex、Kimi、Grok、OpenCode、Copilot、Antigravity 的顺序。
    var broken: [Item] = []
    var idle: [Item] = []

    init(tools: [String], hooks: [SBHookStatus], removed: Set<String>) {
        let order = ["claude", "codex", "kimi", "grok", "opencode", "copilot", "antigravity"]
        var counts: [String: Int] = [:]
        for t in tools where !removed.contains(t) { counts[t, default: 0] += 1 }
        let sorted = counts.keys.sorted { (order.firstIndex(of: $0) ?? order.count, $0) < (order.firstIndex(of: $1) ?? order.count, $1) }
        for tool in sorted {
            guard let h = hooks.first(where: { $0.tool == tool }) else { continue }
            let item = Item(tool: tool, count: counts[tool] ?? 0)
            if h.state == "ok" { idle.append(item) } else if h.broken { broken.append(item) }
        }
    }
}

struct SBConversation: Equatable {
    let id: Int64
    var tool: String
    var nativeID: String
    var projectID: Int64?
    var title: String?
    var lastPrompt: String?
    /// 最后一条 AI 回复及其时间（M1 第 6 块）：「最近任务」的副标题与右侧时间。
    var lastReply: String? = nil
    var lastReplyAt: Int64? = nil
    var cwd: String?
    var lastEventAt: Int64
    var endedAt: Int64?
    var endReason: String?
}

/// 「最近任务」分页游标：(ended_at, conversation_id)，倒序。
struct RecentCursor: Equatable, Comparable {
    let endedAt: Int64
    let id: Int64
    static func < (a: RecentCursor, b: RecentCursor) -> Bool { (a.endedAt, a.id) < (b.endedAt, b.id) }
}

private func str(_ d: [String: Any], _ k: String) -> String? { d[k] as? String }
private func int(_ d: [String: Any], _ k: String) -> Int64? { (d[k] as? NSNumber)?.int64Value }

extension SBProject {
    init?(json d: [String: Any]) {
        guard let id = int(d, "project_id") else { return nil }
        self.init(id: id, dir: str(d, "dir") ?? "", name: str(d, "name") ?? "", sortOrder: int(d, "sort_order") ?? 0,
                  branch: str(d, "branch"), git: (d["git"] as? [String: Any]).map {
                      SBGit(dirty: $0["dirty"] as? Bool ?? false, added: int($0, "added") ?? 0,
                            deleted: int($0, "deleted") ?? 0, untracked: int($0, "untracked") ?? 0)
                  }, pinned: d["pinned"] as? Bool ?? false)
    }
}

extension SBTerminal {
    init?(json d: [String: Any]) {
        guard let id = str(d, "terminal_id"), let pid = int(d, "project_id") else { return nil }
        let a = d["agent"] as? [String: Any] ?? [:]
        let f = d["foreground"] as? [String: Any] ?? [:]
        self.init(
            id: id, projectID: pid, cwd: str(d, "cwd") ?? "", currentDir: str(d, "current_dir"), name: str(d, "name"),
            createdAt: int(d, "created_at") ?? 0, conversationID: int(d, "conversation_id"),
            unseen: d["unseen"] as? Bool ?? false, conflictWith: d["conflict_with"] as? [String] ?? [],
            agentTool: str(a, "tool"), agentState: str(a, "state") ?? "unknown",
            sessionID: str(a, "session_id"),
            fgKind: str(f, "kind") ?? "unknown", fgTool: str(f, "tool"), fgPID: int(f, "pid"), fgPath: str(f, "path"),
            fgStartedAt: int(f, "started_at"), fgVersion: str(f, "version"),
            fgAnomaly: str(f, "anomaly"), notice: str(d, "notice"))
    }
}

extension SBHookStatus {
    init?(json d: [String: Any]) {
        guard let tool = str(d, "tool"), let state = str(d, "state") else { return nil }
        self.init(tool: tool, state: state, problems: d["problems"] as? [String] ?? [], fixable: d["fixable"] as? Bool ?? false,
                  verifiedLatest: str(d, "verified_latest"))
    }
}

extension SBConversation {
    init?(json d: [String: Any]) {
        guard let id = int(d, "conversation_id") else { return nil }
        self.init(
            id: id, tool: str(d, "tool") ?? "", nativeID: str(d, "native_id") ?? "", projectID: int(d, "project_id"),
            title: str(d, "title"), lastPrompt: str(d, "last_prompt"),
            lastReply: str(d, "last_reply"), lastReplyAt: int(d, "last_reply_at"), cwd: str(d, "cwd"),
            lastEventAt: int(d, "last_event_at") ?? 0, endedAt: int(d, "ended_at"), endReason: str(d, "end_reason"))
    }

    var cursor: RecentCursor { RecentCursor(endedAt: endedAt ?? 0, id: id) }

    /// 被后续对话接替（`end_reason=continued`），不算已结束的任务；用户从「最近任务」移除的（`unrecoverable`，
    /// M1 第 6 块）同样不显示。
    var continued: Bool { endReason == "continued" || endReason == "unrecoverable" }
    /// 从没提问过（没有提示词也没有标题）：无法恢复，不进「最近任务」（docs/protocol.md 第 7 节）。
    var empty: Bool { title == nil && lastPrompt == nil }
}

// MARK: - 行数据（主线程只把这些值填进单元格）

enum Lamp: Equatable {
    case idle, ready, running, waiting, error
}

struct TaskRow: Equatable {
    var lamp: Lamp
    var tool: String
    /// 工具键（`claude` / `codex` / …），按它取图标 `tool-<键>`；Shell 为 nil（docs/design.md 第 5 节「布局」）。
    var toolKey: String? = nil
    var state: String?
    /// 待处理：待审批（`state=waiting`），只由后续 Hook 清除。
    var needsConfirm: Bool
    /// 未查看：后台的 `unseen`，打开该终端即清除。
    var unseen: Bool
    /// 同一对话同时打开的终端数（含本终端，M1 第 6 块）；0 为没有冲突。「冲突」徽标的悬停提示写这个数。
    var conflictTerminals = 0
    /// 终端提示（M3）：后台进不了启动目录（多半没有文件访问权限，docs/design.md 风险 14），终端行显示「目录无权限」徽标
    /// （0.3.5 起，原在终端上方的窄标题栏）。
    var cwdDenied = false
    /// 终端提示：启动目录不存在，Shell 已在主目录打开（`cwd_missing`，2026-11-13 起与「目录无权限」分开），同一位置显示
    /// 「目录不存在」徽标。
    var cwdMissing = false
    var subtitle: String
    var time: String
    /// 副标题的全文（保留换行；目录为完整路径），副标题被截断时悬停显示（2026-10-30 用户要求）。nil 同副标题。
    var fullText: String? = nil
    /// 副标题悬停时总是附上的一句：这一行该有、ScyllaUI 读不到的内容（docs/ui-methodology.md 1.7 节）。
    var note: String? = nil
    /// 终端行：用户命名（改名时的初值，有名称时代替工具名显示在第 1 行）。
    var name: String? = nil
    /// 第 1 行显示的任务标题（2026-11-01 用户要求，参照 GUI 版 Agent 工具）：CLI 生成的标题，没有时为第一条提示词；
    /// 没有对话（Shell、新会话还没提问）为 nil，显示工具名。用户命名优先于它。
    var heading: String? = nil
    /// 这一行的对话（终端行为最近绑定的那段，没有为 nil）：历史提示词按它读（docs/design.md 10.5 节决策 10）。
    var conversationID: Int64? = nil
    /// 这段对话的任务标题（标题，没有时为最新提示词，压成一行）：删除确认框与历史提示词的栏头用。没有对话为 nil。
    var taskTitle: String? = nil
    /// 副标题是目录（相对项目根的写法）：单行、过长时截掉开头，保留最有用的末段。
    var subtitleIsPath = false
    /// 终端行：前台识别出的 AI（工具键）。
    var fgTool: String? = nil
    /// 终端行：前台是不认识的命令时它的程序名（如 `agy`）；历史提示词据此说明（2026-10-30）。
    var fgCommand: String? = nil
    /// 终端行：前台这款 AI 的状态同步未生效、用户也没在设置里撤销过，第 1 行显示可点的「未同步」（M4b 块 5 P2）。
    var unsynced = false
}

enum SidebarRow: Equatable {
    /// `more`：实际不止 `count` 条（搜索结果只显示最新的一部分，M2 第 4 块），数字后显示「+」。
    case section(title: String, count: Int, more: Bool = false)
    /// `pinned`：「当前任务」的项目行为是否置顶（维护期 0.3.5）；「最近任务」的为 nil（不能置顶）。
    case project(name: String, branch: String?, tooltip: String, dir: String, pinned: Bool? = nil)
    case terminal(TaskRow)
    case conversation(TaskRow)
    case empty(String)

    var isExpandable: Bool {
        switch self {
        case .section, .project: return true
        default: return false
        }
    }
}

// MARK: - 树

/// 不可变的侧栏树。根节点 key 为 ""；每个节点有稳定的 key（docs/design.md 第 5 节实现说明）。
struct SidebarTree: Equatable {
    var rows: [String: SidebarRow] = [:]
    var children: [String: [String]] = ["": []]
    /// 建树时记下的父节点表（buildTree 填写；手工构造的树为空，差分时现算）。
    var parent: [String: String] = [:]
    /// 全部存活终端的行（终端 ID → 行），不受搜索与「只看待处理」影响：标题栏、窗口标题、历史提示词按右侧显示的终端取，
    /// 不能因为它此刻不在列表里就当成 Shell（2026-11-13 长跑发现）。不参与差分与 `==`。
    var terminalRows: [String: TaskRow] = [:]

    static let empty = SidebarTree()

    func kids(_ key: String) -> [String] { children[key] ?? [] }

    /// 父节点表（根的子节点父为 ""）。
    func parents() -> [String: String] {
        if !parent.isEmpty { return parent }
        var p: [String: String] = [:]
        for (parent, kids) in children { for k in kids { p[k] = parent } }
        return p
    }

    static func == (a: SidebarTree, b: SidebarTree) -> Bool { a.rows == b.rows && a.children == b.children }
}

enum SidebarKey {
    static let root = ""
    static let current = "sec:current"
    static let recent = "sec:recent"
    static let empty = "empty:recent"
    static let noMatch = "empty:current"
    static func currentProject(_ id: Int64) -> String { "pc:\(id)" }
    static func currentProjectID(_ key: String) -> Int64? { key.hasPrefix("pc:") ? Int64(key.dropFirst(3)) : nil }
    static func recentProject(_ id: Int64?) -> String { id.map { "pr:\($0)" } ?? "pr:none" }
    static func terminal(_ id: String) -> String { "t:\(id)" }
    static func conversation(_ id: Int64) -> String { "c:\(id)" }
    static func terminalID(_ key: String) -> String? { key.hasPrefix("t:") ? String(key.dropFirst(2)) : nil }
    static func conversationID(_ key: String) -> Int64? { key.hasPrefix("c:") ? Int64(key.dropFirst(2)) : nil }
}

// MARK: - 模型

final class SidebarModel {
    /// 「最近任务」只显示近 recentDays 天内结束的、最多 recentLimit 条，一次取完不分页
    /// （docs/design.md 第 5 节；recentDays 与 scyllauid 的 RECENT_DAYS 同值）。
    static let recentDays = 14
    static let recentLimit = 20
    /// 搜索结果最多显示的条数；请求时多要 1 条，用来判断是否还有更多（docs/design.md 第 5 节「实现（M2 第 4 块）」）。
    static let searchLimit = 50

    enum Effect: Equatable {
        /// 请求 recent.query：`since` 之后结束的、最多 recentLimit 条；应答整体替换已加载的列表。
        case loadRecent(since: Int64)
    }

    /// 当前时间（单测注入固定值）。
    var now: () -> Date = Date.init

    private(set) var projects: [Int64: SBProject] = [:]
    private(set) var terminals: [String: SBTerminal] = [:]
    /// 搜索（M2 第 4 块）：当前查询与它的序号；`hits` 为最近一次被采用的结果（新查询的应答到达前沿用，不闪烁）。
    private(set) var searchQuery: String?
    private(set) var searchSeq = 0
    private(set) var hits: (terminals: Set<String>, conversations: [SBConversation])?
    private var hitsMore = false
    /// 只看待处理：「需确认」或「待查看」的终端（docs/design.md 第 5 节「实现（M2 第 4 块）」）。
    private(set) var pendingOnly = false
    /// 「需确认」的终端数（Dock 角标，M2 第 1 块），按全部终端统计，不受搜索与筛选影响；每次建树更新。
    private(set) var pendingTotal = 0
    private(set) var active: [Int64: SBConversation] = [:]
    /// 对话的用量（`usage.upsert` 与快照的 `usage`，M2 第 2、3 块）。
    private(set) var usages: [Int64: SBUsage] = [:]
    /// 各款 CLI 账号的额度（`quota.upsert`、`quota.remove` 与快照的 `quotas`，M2 第 3 块），键为工具名与窗口（`5h`、`7d`、`month`）。
    private(set) var quotas: [String: [String: SBQuota]] = [:]
    /// 各款查不到额度的原因代码（`quota.note` 与快照的 `quota_notes`，2026-11-01）。
    private(set) var quotaNotes: [String: String] = [:]
    /// 各款读不到的信息（快照的 `tools`，工具键 → 读不到的项）：界面写明「暂不支持」。旧后台没有这一项时视为都支持。
    private(set) var unsupported: [String: Set<String>] = [:]
    /// 已加载的最近任务，按 (ended_at, id) 倒序。
    private(set) var recent: [SBConversation] = []
    /// 后台给出的显示窗口内已结束对话数，用于发现被清理的条目（清理总是删最旧的）。
    private(set) var recentTotal = 0
    /// 四款 CLI 的 Hook 配置状态（快照的 `hooks` 与 `hooks.upsert`，M1 第 7 块）。
    private(set) var hooks: [SBHookStatus] = []
    /// 用户在设置里撤销过状态同步的工具（`Prefs.hooksRemoved`，每批建树前由 SidebarFeed 设置）：终端行不显示「未同步」。
    private(set) var hooksRemoved: Set<String> = []
    private(set) var loading = false
    private var snapshotRev: UInt64 = 0
    private var hasSnapshot = false
    private let timeText = TimeText()
    /// 最近任务的行数据缓存：对话没变就不重算（每批都要重建整棵树，最近任务最多 500 行）。跨天时清空。
    private var conversationRows: [Int64: (SBConversation, TaskRow)] = [:]
    /// 终端行缓存：终端与其当前对话都没变就不重算（推送密集时每批都重建，字符串处理是建树的主要开销）。
    private var terminalRows: [String: (SBTerminal, SBConversation?, TaskRow)] = [:]

    /// 处理一条快照或推送，返回需要发出的请求。
    @discardableResult
    func apply(_ msg: [String: Any]) -> [Effect] {
        let type = msg["type"] as? String
        if type == "snapshot" { return applySnapshot(msg) }
        let rev = (msg["rev"] as? NSNumber)?.uint64Value ?? 0
        guard hasSnapshot, rev > snapshotRev else { return [] }
        switch type {
        case "project.upsert":
            if let p = (msg["project"] as? [String: Any]).flatMap(SBProject.init(json:)) { projects[p.id] = p }
        case "terminal.upsert":
            if let d = msg["terminal"] as? [String: Any], let t = SBTerminal(json: d) {
                if d["status"] as? String == "closed" { terminals[t.id] = nil } else { terminals[t.id] = t }
            }
        case "hooks.upsert":
            setHooks(msg["hooks"])
        case "usage.upsert":
            if let cid = int(msg, "conversation_id") {
                usages[cid] = (msg["usage"] as? [String: Any]).map(SBUsage.init(json:))
            }
        case "quota.upsert":
            if let tool = str(msg, "tool"), let q = (msg["quota"] as? [String: Any]).flatMap(SBQuota.init(json:)) { quotas[tool, default: [:]][q.window] = q }
        case "quota.remove":
            if let tool = str(msg, "tool"), let w = str(msg, "window") { quotas[tool]?[w] = nil }
        case "quota.note":
            if let tool = str(msg, "tool") { quotaNotes[tool] = str(msg, "note") }
        case "conversation.upsert", "conversation.remove":
            if type == "conversation.remove" {
                // 「删除记录」（docs/design.md 第 5 节「移除与删除」）：从最近任务与搜索结果中去掉。
                if let cid = int(msg, "conversation_id") {
                    recent.removeAll { $0.id == cid }
                    active[cid] = nil
                    usages[cid] = nil
                    dropHit(cid)
                }
            } else if let c = (msg["conversation"] as? [String: Any]).flatMap(SBConversation.init(json:)) {
                upsert(c)
                // 「从最近任务移除」的（`unrecoverable`）也不再出现在当前的搜索结果里。
                if c.continued { dropHit(c.id) }
            }
            if let total = (msg["recent_total"] as? NSNumber)?.intValue { recentTotal = total }
            trimRecent()
            // 满 50 条时有条目离开（恢复、清理），后台排在第 51 的那条本地没有：重取一次补齐。
            if !loading && recent.count < min(recentTotal, Self.recentLimit) {
                loading = true
                return [.loadRecent(since: cutoffMs)]
            }
        default:
            break
        }
        return []
    }

    private func applySnapshot(_ msg: [String: Any]) -> [Effect] {
        snapshotRev = (msg["rev"] as? NSNumber)?.uint64Value ?? 0
        hasSnapshot = true
        func list(_ k: String) -> [[String: Any]] { (msg[k] as? [[String: Any]]) ?? [] }
        projects = Dictionary(list("projects").compactMap(SBProject.init(json:)).map { ($0.id, $0) }) { _, b in b }
        terminals = Dictionary(list("terminals").compactMap(SBTerminal.init(json:)).map { ($0.id, $0) }) { _, b in b }
        active = Dictionary(list("conversations").compactMap(SBConversation.init(json:))
            .filter { $0.endedAt == nil }.map { ($0.id, $0) }) { _, b in b }
        recentTotal = (msg["recent_total"] as? NSNumber)?.intValue ?? 0
        setHooks(msg["hooks"])
        usages = [:]
        for u in list("usage") {
            if let cid = int(u, "conversation_id"), let d = u["usage"] as? [String: Any] { usages[cid] = SBUsage(json: d) }
        }
        unsupported = [:]
        for t in list("tools") {
            if let tool = str(t, "tool") { unsupported[tool] = Set(["reply", "usage", "prompts", "model", "effort"].filter { t[$0] as? Bool == false }) }
        }
        quotas = [:]
        for q in list("quotas") {
            if let tool = str(q, "tool"), let d = (q["quota"] as? [String: Any]).flatMap(SBQuota.init(json:)) { quotas[tool, default: [:]][d.window] = d }
        }
        quotaNotes = [:]
        for n in list("quota_notes") {
            if let tool = str(n, "tool") { quotaNotes[tool] = str(n, "note") }
        }
        // 已加载的最近任务保留到应答到达再整体替换，重连时不闪烁。
        recent.removeAll { active[$0.id] != nil }
        trimRecent()
        loading = true
        return [.loadRecent(since: cutoffMs)]
    }

    private func setHooks(_ v: Any?) {
        let list = ((v as? [[String: Any]]) ?? []).compactMap(SBHookStatus.init(json:))
        if list != hooks {
            hooks = list
            terminalRows.removeAll()   // 终端行的悬停提示含状态同步说明
        }
    }

    func setHooksRemoved(_ removed: Set<String>) {
        if removed != hooksRemoved {
            hooksRemoved = removed
            terminalRows.removeAll()   // 终端行的「未同步」随之变化
        }
    }

    /// 提示栏的内容（M1 第 7 块）：「状态可能不准」，只算此刻正在终端里运行的（M4b 块 6）。
    func banner() -> HookBanner {
        HookBanner(details: inaccurateItems())
    }

    /// 「状态可能不准」的详情（M4b 块 6）：此刻在运行、Hook 数据有异常的工具，各自的版本、原因与最新的已验证版本。
    func inaccurateItems() -> [InaccurateItem] {
        let odd = terminals.values.filter { $0.fgKind == "agent" && $0.fgAnomaly != nil }
        return ["claude", "codex", "kimi", "grok", "opencode", "copilot", "antigravity"].compactMap { tool in
            let ts = odd.filter { $0.fgTool == tool }
            guard !ts.isEmpty else { return nil }
            return InaccurateItem(
                tool: tool, versions: Array(Set(ts.compactMap(\.fgVersion))).sorted(),
                reasons: Array(Set(ts.compactMap { $0.fgAnomaly.map(Self.anomalyText) })).sorted(),
                verifiedLatest: hooks.first { $0.tool == tool }?.verifiedLatest)
        }
    }

    /// Hook 数据异常的原因（M4b 块 6，代码见 docs/protocol.md 第 5 节 `foreground.anomaly`）。
    static func anomalyText(_ code: String) -> String {
        if code == "unparsable" { return String(localized: "收到无法解析的 Hook 数据") }
        if code == "missing_event_name" { return String(localized: "Hook 数据缺少事件名") }
        if code.hasPrefix("unknown_event:") { return String(localized: "收到不认识的事件 \(String(code.dropFirst(14)))") }
        if code.hasPrefix("missing_session:") { return String(localized: "\(String(code.dropFirst(16))) 事件缺少会话 ID") }
        return code
    }

    /// 显示窗口的起点（Unix 毫秒）。
    private var cutoffMs: Int64 {
        Int64(now().timeIntervalSince1970 * 1000) - Int64(Self.recentDays) * 86_400_000
    }

    /// recent.query 的应答，整体替换已加载的列表（`ok=false` 时只结束在途状态）。
    func applyRecent(_ reply: [String: Any]) {
        loading = false
        guard reply["ok"] as? Bool == true else { return }
        if let total = (reply["recent_total"] as? NSNumber)?.intValue { recentTotal = total }
        recent = ((reply["items"] as? [[String: Any]]) ?? []).compactMap(SBConversation.init(json:))
            .filter { $0.endedAt != nil && !$0.continued && !$0.empty && active[$0.id] == nil }
            .sorted { $0.cursor > $1.cursor }
        trimRecent()
    }

    private func upsert(_ c: SBConversation) {
        recent.removeAll { $0.id == c.id }
        guard c.endedAt != nil else {
            active[c.id] = c
            return
        }
        active[c.id] = nil
        // 被后续对话接替的（转入后台继续的同一段对话）与从没提问过的不进「最近任务」（docs/protocol.md 第 7 节）。
        if c.continued || c.empty { return }
        // 已满时比最后一条还旧的不在前 50 条内，不插入（否则会顶替真正的第 50 条）。
        if recent.count >= Self.recentLimit, let last = recent.last, c.cursor < last.cursor { return }
        let i = recent.firstIndex { $0.cursor < c.cursor } ?? recent.count
        recent.insert(c, at: i)
    }

    /// 去掉显示窗口外（老化）的条目；多于后台总数（被清理）或多于上限时裁掉最旧的尾部（docs/protocol.md 第 5 节）。
    private func trimRecent() {
        let cutoff = cutoffMs
        recent.removeAll { ($0.endedAt ?? 0) < cutoff }
        let keep = min(recentTotal, Self.recentLimit)
        if recent.count > keep { recent.removeLast(recent.count - keep) }
    }

    // MARK: 构建

    func buildTree(now: Date = Date(), calendar: Calendar = .current) -> SidebarTree {
        let fmt = timeText
        if fmt.prepare(now: now, calendar: calendar) {
            conversationRows.removeAll()
            terminalRows.removeAll()
        }
        // 显示窗口随时间推移：没有推送时，在下一次建树（窗口变为前台也会触发）时移出过期条目。
        trimRecent()
        var tree = SidebarTree()
        let searchHits = searchQuery == nil ? nil : hits
        tree.children[SidebarKey.root] = pendingOnly ? [SidebarKey.current] : [SidebarKey.current, SidebarKey.recent]

        // 当前任务：置顶的项目在前（维护期 0.3.5），其余按 sort_order，终端按创建时间（位置稳定）。搜索与「只看待处理」时
        // 只留符合的终端（M2 第 4 块）。
        var shownTerminals = 0
        pendingTotal = 0
        let byProject = Dictionary(grouping: terminals.values, by: \.projectID)
        let projectOrder = byProject.keys.sorted {
            let a = projects[$0], b = projects[$1]
            return (a?.pinned == true ? 0 : 1, a?.sortOrder ?? .max, $0) < (b?.pinned == true ? 0 : 1, b?.sortOrder ?? .max, $1)
        }
        var currentKids: [String] = []
        for pid in projectOrder {
            let key = SidebarKey.currentProject(pid)
            currentKids.append(key)
            var ts: [SBTerminal] = []
            for t in byProject[pid]!.sorted(by: { ($0.createdAt, $0.id) < ($1.createdAt, $1.id) }) {
                let conv = t.conversationID.flatMap { active[$0] }
                let row: TaskRow
                if let (ct, cc, r) = terminalRows[t.id], ct == t, cc == conv {
                    row = r
                } else {
                    row = terminalRow(t, fmt)
                    terminalRows[t.id] = (t, conv, row)
                }
                if row.needsConfirm { pendingTotal += 1 }
                tree.terminalRows[t.id] = row
                if let h = searchHits, !h.terminals.contains(t.id) { continue }
                if pendingOnly && !(row.needsConfirm || row.unseen) { continue }
                ts.append(t)
                tree.rows[SidebarKey.terminal(t.id)] = .terminal(row)
            }
            if ts.isEmpty {
                currentKids.removeLast()
                continue
            }
            shownTerminals += ts.count
            tree.rows[key] = projectRow(pid, pinnable: true)
            tree.children[key] = ts.map { SidebarKey.terminal($0.id) }
        }
        tree.rows[SidebarKey.current] = .section(title: String(localized: "当前任务"), count: shownTerminals)
        if currentKids.isEmpty && (searchHits != nil || pendingOnly) && !terminals.isEmpty {
            currentKids.append(SidebarKey.noMatch)
            tree.rows[SidebarKey.noMatch] = .empty(pendingOnly && searchHits == nil ? String(localized: "没有待处理的终端") : String(localized: "没有匹配的终端"))
        }
        tree.children[SidebarKey.current] = currentKids
        if terminalRows.count > 2 * terminals.count + 16 {
            terminalRows = terminalRows.filter { terminals[$0.key] != nil }
        }

        if pendingOnly {
            for (p, kids) in tree.children { for k in kids { tree.parent[k] = p } }
            return tree
        }

        // 最近任务：组按组内最近结束时间，组内按结束时间倒序（recent 已排好序）。搜索时换成命中的已结束对话。
        let list = searchHits?.conversations ?? recent
        let shown = searchHits != nil ? list.count : loading ? min(recentTotal, Self.recentLimit) : recent.count
        tree.rows[SidebarKey.recent] = .section(title: searchHits != nil ? String(localized: "搜索结果") : String(localized: "最近任务"), count: shown,
                                                more: searchHits != nil && hitsMore)
        var recentKids: [String] = []
        for c in list {
            let key = SidebarKey.recentProject(c.projectID)
            if tree.children[key] == nil {
                recentKids.append(key)
                tree.children[key] = []
            }
            tree.children[key]!.append(SidebarKey.conversation(c.id))
            let row: TaskRow
            if let (cached, r) = conversationRows[c.id], cached == c {
                row = r
            } else {
                row = conversationRow(c, fmt)
                conversationRows[c.id] = (c, row)
            }
            tree.rows[SidebarKey.conversation(c.id)] = .conversation(row)
        }
        if conversationRows.count > 2 * list.count + 64 {
            let keep = Set(list.map(\.id))
            conversationRows = conversationRows.filter { keep.contains($0.key) }
        }
        for key in recentKids {
            let pid = key == SidebarKey.recentProject(nil) ? nil : Int64(key.dropFirst(3))
            tree.rows[key] = pid.map { projectRow($0) } ?? .project(name: String(localized: "其他"), branch: nil, tooltip: "", dir: "")
        }
        if list.isEmpty && (searchHits != nil || !loading) {
            recentKids.append(SidebarKey.empty)
            tree.rows[SidebarKey.empty] = .empty(searchHits != nil ? String(localized: "没有匹配的已结束任务") : String(localized: "近 \(Self.recentDays) 天没有已结束的任务"))
        }
        tree.children[SidebarKey.recent] = recentKids
        for (p, kids) in tree.children { for k in kids { tree.parent[k] = p } }
        return tree
    }

    private func dropHit(_ id: Int64) {
        hits?.conversations.removeAll { $0.id == id }
    }

    /// 开始一次搜索（`query` 为 nil 即清除），返回它的序号；只有这一序号的应答会被采用。
    func setSearch(_ query: String?) -> Int {
        searchSeq += 1
        searchQuery = query
        if query == nil { hits = nil }
        return searchSeq
    }

    /// `search` 的应答；不是最新一次查询的丢弃。返回是否采用。
    @discardableResult
    func applySearch(_ reply: [String: Any], seq: Int) -> Bool {
        guard seq == searchSeq, searchQuery != nil, reply["ok"] as? Bool == true else { return false }
        let ts = Set(reply["terminals"] as? [String] ?? [])
        let cs = ((reply["conversations"] as? [[String: Any]]) ?? []).compactMap(SBConversation.init(json:))
        hits = (ts, Array(cs.prefix(Self.searchLimit)))
        hitsMore = cs.count > Self.searchLimit
        return true
    }

    func setPendingOnly(_ on: Bool) { pendingOnly = on }

    /// 各存活终端的状态行信息（M2 第 2 块）；`tree` 为同一次建树的结果（取其中终端行显示的工具，含被筛掉的终端）。
    func statusInfos(_ tree: SidebarTree) -> [String: StatusInfo] {
        var out: [String: StatusInfo] = [:]
        for t in terminals.values {
            let p = projects[t.projectID]
            var tool: String?
            var usage: SBUsage?
            if let row = tree.terminalRows[t.id], let key = row.toolKey,
               let cid = t.conversationID, active[cid]?.tool == key {
                tool = key
                usage = usages[cid]
            }
            out[t.id] = StatusInfo(
                projectID: t.projectID,
                project: p.map { $0.name.isEmpty ? ($0.dir as NSString).lastPathComponent : $0.name } ?? String(localized: "项目 \(Int(t.projectID))"),
                projectDir: p?.dir ?? "",
                branch: p?.branch, git: p?.git, model: usage?.model, effort: usage?.effort, tool: tool, usage: usage,
                quota: tool.flatMap { quotas[$0]?["7d"] }, quota5h: tool.flatMap { quotas[$0]?["5h"] },
                quotaMonth: tool.flatMap { quotas[$0]?["month"] }, quotaNote: tool.flatMap { quotaNotes[$0] },
                fgAgent: t.fgKind == "agent" ? t.fgTool : nil,
                usageSupported: !(tool.flatMap { unsupported[$0]?.contains("usage") } ?? false),
                modelSupported: !(tool.flatMap { unsupported[$0]?.contains("model") } ?? false),
                effortSupported: !(tool.flatMap { unsupported[$0]?.contains("effort") } ?? false))
        }
        return out
    }

    /// `pinnable`：「当前任务」的项目行带置顶状态，「最近任务」的不带。
    private func projectRow(_ id: Int64, pinnable: Bool = false) -> SidebarRow {
        guard let p = projects[id] else {
            return .project(name: String(localized: "项目 \(Int(id))"), branch: nil, tooltip: "", dir: "")
        }
        let tip = p.branch.map { p.dir + "\n" + String(localized: "分支：\($0)") } ?? p.dir
        return .project(name: p.name.isEmpty ? (p.dir as NSString).lastPathComponent : p.name, branch: p.branch,
                        tooltip: tip, dir: p.dir, pinned: pinnable ? p.pinned : nil)
    }

    static func toolName(_ tool: String?) -> String {
        switch tool {
        case "claude": return "Claude"
        case "codex": return "Codex"
        case "kimi": return "Kimi"
        case "grok": return "Grok"
        case "opencode": return "OpenCode"
        case "copilot": return "Copilot"
        case "antigravity": return "Antigravity"
        case let t?: return t
        case nil: return "Shell"
        }
    }

    /// 四层状态分开（design.md 4.1 节），Hook 给出的 Agent 状态优先，其次是前台识别。
    func terminalRow(_ t: SBTerminal, _ fmt: TimeText) -> TaskRow {
        // 前台识别出的 AI 与绑定的会话不是同一款（如挂起 Claude 后在同一终端运行 Codex、其首次提问前）：按前台显示，
        // 不显示绑定会话的状态与提示词（docs/design.md 第 5 节「实现（M1 第 6 块）」）。
        let otherAI = t.fgKind == "agent" && t.fgTool != nil && t.agentTool != nil && t.fgTool != t.agentTool
        let live = !otherAI && t.agentTool != nil && ["ready", "running", "waiting", "error"].contains(t.agentState)
        let conv = t.conversationID.flatMap { active[$0] }
        // 时间：最后一条 AI 回复的时间，还没回复时为最近一次事件（M1 第 6 块）。
        var row = TaskRow(lamp: .idle, tool: "Shell", state: nil, needsConfirm: false, unseen: t.unseen,
                          subtitle: "", time: fmt.format(conv?.lastReplyAt ?? conv?.lastEventAt ?? t.createdAt))
        if live {
            row.tool = Self.toolName(t.agentTool)
            row.toolKey = t.agentTool
            switch t.agentState {
            case "running": row.lamp = .running; row.state = "Running"
            case "waiting": row.lamp = .waiting; row.state = "Waiting"; row.needsConfirm = true
            case "error": row.lamp = .error; row.state = "Error"
            default: row.lamp = .ready; row.state = "Ready"
            }
        } else if t.fgKind == "agent", let tool = t.fgTool {
            row.tool = Self.toolName(tool)
            row.toolKey = tool
            row.state = String(localized: "未知")
        } else if t.fgKind == "command" {
            row.state = String(localized: "运行命令")
        }
        // 副标题（docs/design.md 第 5 节「任务标题」）：最新一条提示词 → 任务标题 → 新会话 → 当前目录（相对项目根）。
        // 2026-10-14 由任务标题改为最新提示词，只在提交时变化。用户命名不再放在副标题，改为代替工具名
        // 显示在第 1 行（2026-10-25 用户确认，TaskCell）；窗口标题为名称，没有时同副标题（WorkspaceController）。
        // 终端绑定着未结束的会话即显示，不等 Agent 状态已知：一键恢复的 Codex 首次提问前没有 Hook（M1 第 6 块）。
        // 会话已在本终端结束的不显示，即使同一对话后来在别的终端重开（对话仍在 active 中）。
        let bound = !otherAI && t.agentTool != nil && t.agentState != "ended"
        let title = bound ? (conv?.lastPrompt ?? conv?.title) : nil
        let dir = t.currentDir ?? t.cwd
        if let text = title ?? newSession(t, conv, live: bound, fmt) {
            row.subtitle = oneLine(text)
            row.fullText = text
        } else {
            row.subtitle = relativeDir(dir, root: projects[t.projectID]?.dir)
            row.subtitleIsPath = true
            row.fullText = (dir as NSString).abbreviatingWithTildeInPath
        }
        row.name = t.name
        row.conversationID = t.conversationID
        row.taskTitle = bound ? (conv?.title ?? conv?.lastPrompt).map(oneLine) : nil
        // 第 2 行照旧是最新提示词，各款规则一致：标题就是第一条提示词的（Kimi、Copilot）问第一句后两行相同，第二句起
        // 就不同了（2026-11-01 用户定，不另设特例）。
        row.heading = row.taskTitle
        row.cwdDenied = t.notice == "cwd_denied"
        row.cwdMissing = t.notice == "cwd_missing"
        // 悬停只给副标题被截断时的全文（2026-10-30 用户要求）。目录、前台进程、会话 ID、CLI 版本不再放：
        // 冲突写在「冲突」徽标上，状态同步问题在「未同步」与提示栏的「详情…」里，会话 ID 在历史提示词的栏头。
        if !t.conflictWith.isEmpty { row.conflictTerminals = t.conflictWith.count + 1 }
        if t.fgKind == "command" { row.fgCommand = t.fgPath.map { ($0 as NSString).lastPathComponent } }
        // 状态同步（M1 第 7 块）：只说前台这款 CLI。
        if t.fgKind == "agent", let tool = t.fgTool {
            row.fgTool = tool
            if let h = hooks.first(where: { $0.tool == tool }), h.broken { row.unsynced = !hooksRemoved.contains(tool) }
        }
        return row
    }

    /// 最近任务行（docs/design.md 第 5 节「实现（M1 第 6 块）」）：状态一律 Stopped；副标题为最后一条 AI 回复
    /// （原文），读不到时为任务标题；右侧时间为最后一条回复的时间，没有时为结束时间。悬停只给副标题被截断时的全文（2026-10-30）。
    /// 还没提问的 AI 会话：「新会话 · 会话 ID 前 8 位」；会话 ID 未知（Codex、Kimi 首次提问前没有 Hook）且这个
    /// 终端从没绑定过对话时为「新会话 · 启动时间」（`fg` 回到的旧进程不算新会话）。不是 AI 或已提问过的返回 nil。
    private func newSession(_ t: SBTerminal, _ conv: SBConversation?, live: Bool, _ fmt: TimeText) -> String? {
        if live, let s = t.sessionID, conv?.lastPrompt == nil {
            // 去掉 `session_` 这类前缀（Kimi），取 ID 本身的前 8 位。
            let id = s.split(separator: "_").last.map(String.init) ?? s
            return String(localized: "新会话 · \(String(id.prefix(8)))")
        }
        if !live, t.fgKind == "agent", t.conversationID == nil, let at = t.fgStartedAt { return String(localized: "新会话 · \(fmt.format(at))") }
        return nil
    }

    func conversationRow(_ c: SBConversation, _ fmt: TimeText) -> TaskRow {
        let title = c.title ?? c.lastPrompt ?? String(c.nativeID.prefix(8))
        var row = TaskRow(lamp: .idle, tool: Self.toolName(c.tool), toolKey: c.tool, state: "Stopped", needsConfirm: false,
                          unseen: false, subtitle: oneLine(c.lastReply ?? title), time: fmt.format(c.lastReplyAt ?? c.endedAt ?? c.lastEventAt),
                          fullText: c.lastReply ?? title, conversationID: c.id, taskTitle: oneLine(title))
        row.heading = (c.title ?? c.lastPrompt).map(oneLine)
        // 读不到回复的工具，副标题退回显示任务标题，悬停写明原因（docs/ui-methodology.md 1.7 节）。
        if c.lastReply == nil, unsupported[c.tool]?.contains("reply") == true {
            row.note = String(localized: "ScyllaUI 暂不读取 \(Self.toolName(c.tool)) 的回复")
        }
        return row
    }

    private func oneLine(_ s: String) -> String {
        s.split(whereSeparator: \.isNewline).joined(separator: " ")
    }

    /// 终端副标题里的目录（2026-10-25 用户确认）：相对项目根，项目根显示为项目文件夹名、子目录为「项目/子目录」，
    /// 项目根是主目录时用「~」。项目根是真实路径而终端报的可能是 `/var/…`（macOS 的 `/var`、`/tmp` 是指向 `/private/…`
    /// 的符号链接），按字符串视为同一路径，不访问文件系统。不在项目根下时为缩写后的完整路径。
    func relativeDir(_ dir: String, root: String?) -> String {
        func plain(_ p: String) -> String {
            ["/private/var/", "/private/tmp/", "/private/etc/"].contains { p.hasPrefix($0) } ? String(p.dropFirst("/private".count)) : p
        }
        guard let root else { return abbreviate(dir) }
        let (d, r) = (plain(dir), plain(root))
        let head = r == NSHomeDirectory() ? "~" : (r as NSString).lastPathComponent
        if d == r { return head }
        if r != "/", d.hasPrefix(r + "/") { return head + d.dropFirst(r.count) }
        return abbreviate(dir)
    }

    private func abbreviate(_ path: String) -> String {
        let home = NSHomeDirectory()
        if path == home { return "~" }
        return path.hasPrefix(home + "/") ? "~" + path.dropFirst(home.count) : path
    }
}

/// 绝对时间：今天 `HH:mm`，今年 `M月d日`，更早 `yyyy/M/d`。不需要定时刷新。
/// 结果只取决于所在分钟和「今天」是哪天，按分钟缓存（DateFormatter 较慢，最近任务每批都要重建）；跨天时清空。
final class TimeText {
    /// 界面语言对应的 locale（跟随 App 实际采用的本地化；M2 第 6 块），日期格式按它取。
    static let uiLocale = Locale(identifier: Bundle.main.preferredLocalizations.first ?? "zh-Hans")
    private let time: DateFormatter
    private let day: DateFormatter
    private let full: DateFormatter
    private var calendar = Calendar.current
    private var today = DateInterval(start: .distantPast, duration: 0)
    private var year = 0
    private var cache: [Int64: String] = [:]

    init(now: Date = Date(), calendar: Calendar = .current, locale: Locale = TimeText.uiLocale) {
        time = Self.formatter("HHmm", locale)
        day = Self.formatter("MMMd", locale)
        full = Self.formatter("yMd", locale)
        prepare(now: now, calendar: calendar)
    }

    /// 按模板取该语言的格式：中文为「HH:mm」「M月d日」「y/M/d」，英文为「HH:mm」「Oct 16」「10/16/2026」。
    private static func formatter(_ template: String, _ locale: Locale) -> DateFormatter {
        let d = DateFormatter()
        d.locale = locale
        d.setLocalizedDateFormatFromTemplate(template)
        return d
    }

    /// 返回 true 表示「今天」或时区变了，此前格式化的结果作废。
    @discardableResult
    func prepare(now: Date, calendar: Calendar) -> Bool {
        if calendar == self.calendar && today.contains(now) { return false }
        self.calendar = calendar
        for f in [time, day, full] { f.timeZone = calendar.timeZone }
        today = calendar.dateInterval(of: .day, for: now) ?? DateInterval(start: now, duration: 86_400)
        year = calendar.component(.year, from: now)
        cache.removeAll()
        return true
    }

    func format(_ ms: Int64) -> String {
        let minute = ms >= 0 ? ms / 60_000 : (ms - 59_999) / 60_000
        if let s = cache[minute] { return s }
        let date = Date(timeIntervalSince1970: TimeInterval(minute) * 60)
        let f = today.contains(date) ? time : calendar.component(.year, from: date) == year ? day : full
        let s = f.string(from: date)
        cache[minute] = s
        return s
    }
}

// MARK: - 差分

/// 相邻两棵树之间的变化，按顺序执行即可把旧树变成新树（NSOutlineView 在 beginUpdates/endUpdates 内按顺序应用下标）。
struct SidebarChanges: Equatable {
    enum Op: Equatable {
        case remove(parent: String, index: Int, key: String)
        case insert(parent: String, index: Int, key: String)
        case move(parent: String, from: Int, to: Int, key: String)
    }

    var ops: [Op] = []
    /// 仍然存在、但行数据变化的节点。
    var updated: [String] = []
    /// 新插入的可展开节点（插入后默认展开）。
    var inserted: [String] = []

    var isEmpty: Bool { ops.isEmpty && updated.isEmpty }

    /// 先全树删除（父节点保留、子节点消失的），再自上而下插入与移动。新插入节点的子节点不单独插入：
    /// NSOutlineView 会在需要时向数据源要。
    static func diff(from old: SidebarTree, to new: SidebarTree) -> SidebarChanges {
        var out = SidebarChanges()
        let oldParent = old.parents(), newParent = new.parents()
        // 节点「留存」：新旧树都有、父节点相同且父节点留存。
        var survives: [String: Bool] = [SidebarKey.root: true]
        survives.reserveCapacity(old.rows.count + 1)
        func survive(_ k: String) -> Bool {
            if let s = survives[k] { return s }
            let s = oldParent[k] != nil && oldParent[k] == newParent[k] && survive(oldParent[k]!)
            survives[k] = s
            return s
        }
        // 删除：只在留存的父节点下进行；被删节点的子树随之消失。
        var level = [SidebarKey.root]
        while !level.isEmpty {
            var next: [String] = []
            for p in level {
                let kids = old.kids(p)
                for (i, k) in kids.enumerated().reversed() where !survive(k) {
                    out.ops.append(.remove(parent: p, index: i, key: k))
                }
                next += kids.filter(survive)
            }
            level = next
        }
        // 插入与移动：自上而下，逐位把当前序列对齐到目标序列。
        level = [SidebarKey.root]
        while !level.isEmpty {
            var next: [String] = []
            for p in level {
                var cur = old.kids(p).filter(survive)
                let target = new.kids(p)
                for (i, k) in target.enumerated() {
                    if i < cur.count && cur[i] == k { continue }
                    if let j = cur.firstIndex(of: k) {
                        out.ops.append(.move(parent: p, from: j, to: i, key: k))
                        cur.remove(at: j)
                    } else {
                        out.ops.append(.insert(parent: p, index: i, key: k))
                        if new.rows[k]?.isExpandable == true { out.inserted.append(k) }
                    }
                    cur.insert(k, at: i)
                }
                next += target.filter(survive)
            }
            level = next
        }
        for (k, row) in new.rows where survive(k) && old.rows[k] != row {
            out.updated.append(k)
        }
        out.updated.sort()
        return out
    }
}
