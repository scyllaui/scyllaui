// 侧栏模型与差分的单测（M1 第 3 块）。只编译 SidebarModel.swift，不依赖 GhosttyKit，无宿主 App。

import XCTest

private let now = Date(timeIntervalSince1970: 1_800_000_000)
private let nowMs: Int64 = 1_800_000_000_000
private var utc: Calendar = {
    var c = Calendar(identifier: .gregorian)
    c.timeZone = TimeZone(identifier: "UTC")!
    return c
}()

/// 显示窗口内的时间点：相对顺序与参数一致（数值越大越新）。
private func at(_ x: Int64) -> Int64 { nowMs - 100_000 + x }
private let day: Int64 = 86_400_000

private func model() -> SidebarModel {
    let m = SidebarModel()
    m.now = { now }
    return m
}

private func project(_ id: Int64, _ name: String, order: Int64, branch: String? = nil, pinned: Bool? = nil) -> [String: Any] {
    var p: [String: Any] = ["project_id": id, "dir": "/w/\(name)", "name": name, "sort_order": order]
    p["branch"] = branch
    p["pinned"] = pinned
    return p
}

private func terminal(_ id: String, project: Int64, created: Int64 = 0, state: String = "unknown", tool: String? = nil,
                      fg: String = "shell", fgTool: String? = nil, unseen: Bool = false, conversation: Int64? = nil,
                      name: String? = nil, status: String = "alive") -> [String: Any] {
    var agent: [String: Any] = ["state": state]
    agent["tool"] = tool
    var f: [String: Any] = ["kind": fg]
    f["tool"] = fgTool
    var t: [String: Any] = ["terminal_id": id, "project_id": project, "cwd": "/w/p\(project)", "status": status,
                            "created_at": created, "unseen": unseen, "agent": agent, "foreground": f]
    t["conversation_id"] = conversation
    t["name"] = name
    return t
}

private func conversation(_ id: Int64, project: Int64?, ended: Int64? = nil, tool: String = "claude",
                          title: String? = nil, prompt: String? = "提问", reason: String? = nil) -> [String: Any] {
    var c: [String: Any] = ["conversation_id": id, "tool": tool, "native_id": "n\(id)", "last_event_at": ended ?? 1]
    c["project_id"] = project
    c["ended_at"] = ended
    c["title"] = title
    c["last_prompt"] = prompt
    c["end_reason"] = ended == nil ? nil : (reason ?? "session_end")
    return c
}

private func page(_ items: [[String: Any]], next: (Int64, Int64)? = nil) -> [String: Any] {
    var r: [String: Any] = ["type": "reply", "ok": true, "items": items]
    r["next"] = next.map { ["ended_at": $0.0, "conversation_id": $0.1] }
    return r
}

final class SidebarModelTests: XCTestCase {
    private func build(_ m: SidebarModel) -> SidebarTree { m.buildTree(now: now, calendar: utc) }

    private func task(_ tree: SidebarTree, _ key: String) -> TaskRow? {
        switch tree.rows[key] {
        case .terminal(let r)?, .conversation(let r)?: return r
        default: return nil
        }
    }

    func testGroupingAndOrdering() {
        let m = model()
        let fx = m.apply(["type": "snapshot", "rev": 5, "recent_total": 3,
                          "projects": [project(1, "b", order: 1), project(2, "a", order: 0)],
                          "terminals": [terminal("x", project: 1, created: 20), terminal("y", project: 1, created: 10),
                                        terminal("z", project: 2, created: 30)],
                          "conversations": []])
        XCTAssertEqual(fx, [.loadRecent(since: nowMs - 14 * day)])
        m.applyRecent(page([conversation(7, project: 1, ended: at(300)), conversation(8, project: 2, ended: at(200)),
                            conversation(9, project: 1, ended: at(100))]))
        let t = build(m)
        XCTAssertEqual(t.kids(""), ["sec:current", "sec:recent"])
        XCTAssertEqual(t.rows["sec:current"], .section(title: "当前任务", count: 3))
        XCTAssertEqual(t.kids("sec:current"), ["pc:2", "pc:1"], "项目按 sort_order")
        XCTAssertEqual(t.kids("pc:1"), ["t:y", "t:x"], "终端按创建时间")
        XCTAssertEqual(t.rows["sec:recent"], .section(title: "最近任务", count: 3))
        XCTAssertEqual(t.kids("sec:recent"), ["pr:1", "pr:2"], "组按组内最近结束时间")
        XCTAssertEqual(t.kids("pr:1"), ["c:7", "c:9"])
        XCTAssertEqual(t.rows["pc:1"], .project(name: "b", branch: nil, tooltip: "/w/b", dir: "/w/b", pinned: false), "项目行不带任务数（10.5 节决策 7）")
        XCTAssertEqual(t.rows["pr:2"], .project(name: "a", branch: nil, tooltip: "/w/a", dir: "/w/a"), "最近任务的项目行不能置顶")
    }

    /// 项目置顶（维护期 0.3.5）：只影响「当前任务」，置顶的组在前；置顶的组之间、未置顶的组之间都按 sort_order，组内终端
    /// 顺序不变；「最近任务」的分组不受影响；取消置顶即回原位；旧后台没有 pinned 字段按未置顶。
    func testPinnedProjectsComeFirstInCurrent() {
        let m = model()
        m.apply(["type": "snapshot", "rev": 1, "recent_total": 2,
                 "projects": [project(1, "a", order: 0), project(2, "b", order: 1, pinned: true),
                              project(3, "c", order: 2), project(4, "d", order: 3, pinned: true)],
                 "terminals": [terminal("w", project: 1, created: 1), terminal("x", project: 2, created: 2),
                               terminal("y2", project: 3, created: 4), terminal("y1", project: 3, created: 3),
                               terminal("z", project: 4, created: 5)],
                 "conversations": []])
        m.applyRecent(page([conversation(7, project: 1, ended: at(300)), conversation(8, project: 4, ended: at(200))]))
        var t = build(m)
        XCTAssertEqual(t.kids("sec:current"), ["pc:2", "pc:4", "pc:1", "pc:3"])
        XCTAssertEqual(t.kids("pc:3"), ["t:y1", "t:y2"], "组内终端仍按创建时间")
        XCTAssertEqual(t.kids("sec:recent"), ["pr:1", "pr:4"], "最近任务按结束时间，不看置顶")
        guard case .project(_, _, _, _, let pinned)? = t.rows["pc:4"], case .project(_, _, _, _, let recentPinned)? = t.rows["pr:4"] else {
            return XCTFail("缺项目行")
        }
        XCTAssertEqual(pinned, true)
        XCTAssertNil(recentPinned)
        m.apply(["type": "project.upsert", "rev": 2, "project": project(2, "b", order: 1, pinned: false)])
        t = build(m)
        XCTAssertEqual(t.kids("sec:current"), ["pc:4", "pc:1", "pc:2", "pc:3"], "取消置顶回到 sort_order 的位置")
        m.apply(["type": "project.upsert", "rev": 3, "project": project(4, "d", order: 3)])
        XCTAssertEqual(build(m).kids("sec:current"), ["pc:1", "pc:2", "pc:3", "pc:4"], "没有 pinned 字段按未置顶")
    }

    /// 「删除记录」推送 conversation.remove：从最近任务与当前搜索结果中去掉；「从最近任务移除」（unrecoverable）也从搜索结果中去掉
    /// （docs/design.md 第 5 节「移除与删除」）。
    func testConversationRemoveAndForget() {
        let m = model()
        m.apply(["type": "snapshot", "rev": 1, "recent_total": 3, "projects": [project(1, "p", order: 0)],
                 "terminals": [], "conversations": []])
        m.applyRecent(page([conversation(7, project: 1, ended: at(300)), conversation(8, project: 1, ended: at(200)),
                            conversation(9, project: 1, ended: at(100))]))
        XCTAssertEqual(build(m).kids("pr:1"), ["c:7", "c:8", "c:9"])
        m.apply(["type": "conversation.remove", "rev": 2, "conversation_id": 7, "recent_total": 2])
        XCTAssertEqual(build(m).kids("pr:1"), ["c:8", "c:9"])
        let s = m.setSearch("q")
        m.applySearch(["ok": true, "terminals": [],
                       "conversations": [conversation(8, project: 1, ended: at(200)), conversation(9, project: 1, ended: at(100))]], seq: s)
        XCTAssertEqual(build(m).kids("pr:1"), ["c:8", "c:9"])
        m.apply(["type": "conversation.upsert", "rev": 3, "recent_total": 1,
                 "conversation": conversation(8, project: 1, ended: at(200), reason: "unrecoverable")])
        XCTAssertEqual(build(m).kids("pr:1"), ["c:9"], "移除的也不在搜索结果里")
        m.apply(["type": "conversation.remove", "rev": 4, "conversation_id": 9, "recent_total": 0])
        XCTAssertFalse(build(m).kids("pr:1").contains("c:9"), "删除的不在搜索结果里")
        _ = m.setSearch(nil)
        let left = build(m).kids("pr:1")
        XCTAssertFalse(left.contains("c:7") || left.contains("c:8") || left.contains("c:9"), "\(left)")
    }

    func testProjectBranchFollowsProjectUpsert() {
        let m = model()
        m.apply(["type": "snapshot", "rev": 1, "recent_total": 0, "projects": [project(1, "p", order: 0, branch: "main")],
                 "terminals": [terminal("x", project: 1)], "conversations": []])
        XCTAssertEqual(build(m).rows["pc:1"], .project(name: "p", branch: "main", tooltip: "/w/p\n分支：main", dir: "/w/p", pinned: false))
        m.apply(["type": "project.upsert", "rev": 2, "project": project(1, "p", order: 0, branch: "dev")])
        let c = SidebarChanges.diff(from: SidebarTree.empty, to: build(m))
        XCTAssertFalse(c.isEmpty)
        XCTAssertEqual(build(m).rows["pc:1"], .project(name: "p", branch: "dev", tooltip: "/w/p\n分支：dev", dir: "/w/p", pinned: false))
        m.apply(["type": "project.upsert", "rev": 3, "project": project(1, "p", order: 0)])
        XCTAssertEqual(build(m).rows["pc:1"], .project(name: "p", branch: nil, tooltip: "/w/p", dir: "/w/p", pinned: false), "离开仓库")
    }

    /// 状态行左侧各段的悬停文字（2026-11-01 按段悬停）。
    private func tips(_ info: StatusInfo, _ now: Int64 = StatusInfo.nowMs()) -> [String] {
        info.segments(now: now).compactMap(\.tooltip)
    }

    /// 标题栏状态行（M2 第 2 块）：为 0 的改动项不显示，读不到的段整段省略，不是仓库时没有括号。
    func testStatusLineText() {
        let g = SBGit(dirty: true, added: 12, deleted: 0, untracked: 3)
        var info = StatusInfo(projectID: 1, project: "scyllaui", branch: "main", git: g, model: "claude-opus-5-5")
        XCTAssertEqual(info.text(), "[scyllaui] (main* +12 ?3)   Model: claude-opus-5-5")
        // 0.3.5：CPU / Mem 移到标题栏右侧，左侧不再有；推理强度跟在模型后面，只在有模型时显示。
        func stats(_ cpu: Double?, _ mem: Int64?) -> String { StatusInfo.statsSpans(cpu: cpu, mem: mem).map(\.text).joined() }
        XCTAssertEqual(stats(12.4, 880_803_840), "CPU: 12% • Mem: 840 MB")
        XCTAssertEqual(stats(nil, 2_147_483_648), "Mem: 2.0 GB", "第一次采样没有 CPU")
        XCTAssertEqual(stats(0, nil), "CPU: 0%")
        XCTAssertEqual(stats(nil, nil), "")
        info.effort = "xhigh"
        XCTAssertEqual(info.text(), "[scyllaui] (main* +12 ?3)   Model: claude-opus-5-5 (xhigh)")
        XCTAssertEqual(tips(info), [], "行上已显示的不再写进悬停提示（2026-10-30）")
        info.git = SBGit(dirty: false, added: 0, deleted: 0, untracked: 0)
        info.model = nil
        XCTAssertEqual(info.text(), "[scyllaui] (main)", "干净的仓库、没有模型时也不显示推理强度")
        info.git = nil
        XCTAssertEqual(info.text(), "[scyllaui] (main)", "改动数读不到时只有分支")
        info.branch = nil
        XCTAssertEqual(info.text(), "[scyllaui]", "不是仓库")
        // 2026-10-18 改版：按段取色。项目名单独一段，+N 绿、-M 红（ScyllaUI 局部例外），Model / CPU / Mem 的值用主题蓝，
        // 推理强度连同括号用次要色。
        var colored = StatusInfo(projectID: 1, project: "p", branch: "dev", git: SBGit(dirty: true, added: 2, deleted: 1, untracked: 4), model: "m")
        colored.effort = "high"
        XCTAssertEqual(colored.spans().map { "\($0.role):\($0.text)" },
                       ["project:[p]", "muted: (dev*", "added: +2", "deleted: -1", "muted: ?4", "muted:)", "separator:   ",
                        "muted:Model: ", "accent:m", "muted: (high)"])
        XCTAssertEqual(StatusInfo.statsSpans(cpu: 3, mem: 1_048_576).map { "\($0.role):\($0.text)" },
                       ["muted:CPU: ", "accent:3%", "separator: • ", "muted:Mem: ", "accent:1 MB"])
        // 右侧的悬停提示写明统计口径（2026-10-16 用户确认：Shell 及其所有子进程，不含 root 的 login）；左侧不再写。
        let tip = StatusInfo.statsTooltip(cpu: 99.9, mem: 5_662_310, processes: 2) ?? ""
        XCTAssertTrue(tip.contains("Shell 及其所有子进程（2 个）：CPU 99.9%，内存 5 MB"), tip)
        XCTAssertTrue(tip.contains("不含系统以 root 身份运行的登录进程 login"), tip)
        XCTAssertNil(StatusInfo.statsTooltip(cpu: nil, mem: nil, processes: 0), "还没有采样")
        XCTAssertEqual(tips(info), [], "CPU / Mem 的口径只挂在右侧（2026-11-01 按段悬停），左侧不再拼")
    }

    /// 状态行信息取自推送：项目的 `git`、`usage.upsert` 的模型；只有这一栏显示的 AI 与绑定对话同款时才带模型。
    func testStatusInfoFromPushes() {
        let m = model()
        var p = project(1, "p", order: 0, branch: "main")
        p["git"] = ["dirty": true, "added": 2, "deleted": 1, "untracked": 0]
        m.apply(["type": "snapshot", "rev": 1, "recent_total": 0, "projects": [p],
                 "terminals": [terminal("a", project: 1, state: "ready", tool: "codex", fg: "agent", fgTool: "codex", conversation: 7),
                               terminal("b", project: 1, state: "ready", tool: "claude", fg: "agent", fgTool: "codex", conversation: 8)],
                 "conversations": [conversation(7, project: 1, tool: "codex"), conversation(8, project: 1, tool: "claude")],
                 "usage": [["conversation_id": 7, "usage": ["model": "gpt-6.1-sol", "model_source": "hook"]],
                           ["conversation_id": 8, "usage": ["model": "claude-opus-5-5", "model_source": "log"]]]])
        var infos = m.statusInfos(build(m))
        XCTAssertEqual(infos["a"], StatusInfo(projectID: 1, project: "p", projectDir: "/w/p", branch: "main",
                                              git: SBGit(dirty: true, added: 2, deleted: 1, untracked: 0), model: "gpt-6.1-sol",
                                              tool: "codex", usage: SBUsage(model: "gpt-6.1-sol"), fgAgent: "codex"))
        XCTAssertNil(infos["b"]?.model, "前台是 Codex、绑定的是 Claude 对话：不显示 Claude 的模型")
        m.apply(["type": "usage.upsert", "rev": 2, "conversation_id": 7, "usage": ["model": "gpt-6.2", "model_source": "hook", "effort": "xhigh"]])
        m.apply(["type": "project.upsert", "rev": 3, "project": project(1, "p", order: 0, branch: "main")])
        infos = m.statusInfos(build(m))
        XCTAssertEqual(infos["a"]?.model, "gpt-6.2")
        XCTAssertEqual(infos["a"]?.effort, "xhigh", "推理强度随用量推送（0.3.5）")
        XCTAssertNil(infos["b"]?.effort)
        XCTAssertNil(infos["a"]?.git, "不再是标题栏显示的项目：没有改动数")
    }

    /// 状态行的 Total 与周额度（M2 第 3 块）：Total 缩写、倒计时不足 1 天改为「小时 + 分」、已过重置时间不显示；
    /// 悬停提示只写这款 AI 读不到、整段没显示的模型、Total、周额度为什么没有（2026-10-30 精简）；周额度按工具取自推送，
    /// 只给显示该 AI 的终端。
    func testStatusLineTotalAndQuota() {
        XCTAssertEqual([999, 1000, 12_345, 999_949, 999_950, 1_234_567].map(StatusInfo.tokens), ["999", "1.0K", "12.3K", "999.9K", "1.0M", "1.2M"])
        // 周额度百分比按风险语义着色：< 50% 绿、50%—80% 黄、≥ 80% 红。
        XCTAssertEqual([0, 49.9, 50, 79.9, 80, 100].map(StatusInfo.quotaRole), [.quotaLow, .quotaLow, .quotaMid, .quotaMid, .quotaHigh, .quotaHigh])
        let now: Int64 = 1_800_000_000_000
        let day: Int64 = 86_400_000
        var u = SBUsage(model: "gpt-6.1-sol", total: 1_234_567)
        (u.input, u.cacheRead, u.cacheWrite, u.output) = (200_000, 1_000_000, 0, 34_567)
        var info = StatusInfo(projectID: 1, project: "p", branch: nil, git: nil, model: "gpt-6.1-sol", tool: "codex", usage: u,
                              quota: SBQuota(usedPercent: 34.4, resetsAt: now + 2 * day + 5 * 3_600_000 + 59_000, asOf: now - 60_000, source: "log"))
        XCTAssertEqual(info.text(now: now), "[p]   Total: 1.2M • 7d: 34% (2d5h)   Model: gpt-6.1-sol")
        XCTAssertEqual(tips(info, now).count, 1, "只有额度一段带悬停（重置的具体时间）")
        XCTAssertTrue(tips(info, now)[0].hasPrefix("重置时间："), tips(info, now)[0])
        info.quota?.resetsAt = now + 5 * 3_600_000 + 12 * 60_000
        XCTAssertEqual(info.text(now: now), "[p]   Total: 1.2M • 7d: 34% (5h12m)   Model: gpt-6.1-sol")
        info.quota?.resetsAt = now
        XCTAssertEqual(info.text(now: now), "[p]   Total: 1.2M   Model: gpt-6.1-sol", "已过重置时间不显示")
        XCTAssertEqual(tips(info, now), ["额度：还没读到。Agent 启动或一轮结束后读取；用第三方 API 时没有额度"], "原因挂在 Total 上")
        // 5 小时额度（2026-10-31）：在 7d 之前；可以单独关掉。统一规则（2026-11-01）：账号没有或不限制的窗口不显示，不用「-」。
        info.quota?.resetsAt = now + 2 * day
        info.quota5h = SBQuota(usedPercent: 5.4, resetsAt: now + 77 * 60_000, asOf: now, source: "statusline", window: "5h")
        XCTAssertEqual(info.text(now: now), "[p]   Total: 1.2M • 5h: 5% (1h17m) • 7d: 34% (2d0h)   Model: gpt-6.1-sol")
        XCTAssertEqual(info.spans(now: now).first { $0.text == "5%" }?.role, .quotaLow)
        XCTAssertEqual(info.spans(now: now, hidden: [.quota5h]).map(\.text).joined(), "[p]   Total: 1.2M • 7d: 34% (2d0h)   Model: gpt-6.1-sol")
        info.quota5h = SBQuota(usedPercent: 0, resetsAt: 0, asOf: now, source: "log", window: "5h", unlimited: true)
        XCTAssertEqual(info.text(now: now), "[p]   Total: 1.2M • 7d: 34% (2d0h)   Model: gpt-6.1-sol", "不限制的窗口不显示")
        XCTAssertEqual(SBQuota(json: ["window": "5h", "unlimited": true, "as_of": 1, "source": "log"]),
                       SBQuota(usedPercent: 0, resetsAt: 0, asOf: 1, source: "log", window: "5h", unlimited: true))
        XCTAssertEqual(SBQuota(json: ["used_percent": 3.0, "resets_at": 2, "as_of": 1])?.window, "7d", "旧后台没有 window 时为 7 天")
        // 月额度（2026-11-01，Copilot）：在 7d 之后，可以单独关掉。
        (info.quota5h, info.quota, info.tool) = (nil, nil, "copilot")
        info.quotaMonth = SBQuota(usedPercent: 0.6, resetsAt: now + 29 * day + 3_600_000, asOf: now, source: "log", window: "month")
        XCTAssertEqual(info.text(now: now), "[p]   Total: 1.2M • Month: 1% (29d1h)   Model: gpt-6.1-sol")
        XCTAssertEqual(info.spans(now: now, hidden: [.quotaMonth]).map(\.text).joined(), "[p]   Total: 1.2M   Model: gpt-6.1-sol")
        // 按量计费（2026-11-01）：额度位置只显示 API，账号额度不显示；可以单独关掉。
        info.usage?.billing = "api"
        XCTAssertEqual(info.text(now: now), "[p]   Total: 1.2M • API   Model: gpt-6.1-sol")
        XCTAssertEqual(tips(info, now), ["按量计费，没有 5 小时、7 天或月额度"])
        XCTAssertEqual(info.spans(now: now, hidden: [.api]).map(\.text).joined(), "[p]   Total: 1.2M   Model: gpt-6.1-sol")
        XCTAssertEqual(tips(StatusInfo(projectID: 1, project: "p", model: "m", tool: "kimi", usage: info.usage), now), ["按量计费，没有 5 小时、7 天或月额度"])
        info.usage?.billing = nil
        info.quotaMonth = nil
        // 没有额度时的原因：Claude 只经 statusline 桥，OpenCode 暂不读取，Grok 没登录 grok.com；其余为还没读到。
        info.tool = "claude"
        XCTAssertEqual(tips(info, now).first?.contains("scyllaui statusline-bridge install"), true)
        info.tool = "grok"
        info.quotaNote = "login_required"
        XCTAssertEqual(tips(info, now), ["额度：查 Grok 的额度要先登录 grok.com（在终端运行 grok login）"])
        info.quotaNote = nil
        info.quota = SBQuota(usedPercent: 1, resetsAt: now + day, asOf: now, source: "command")
        XCTAssertTrue(tips(info, now)[0].hasSuffix("\n按当前登录的 grok.com 账号"), "Grok 的额度按当前登录的账号查")
        info.quota = nil
        info.usage = nil
        info.tool = "opencode"
        info.usageSupported = false
        XCTAssertEqual(info.text(now: now), "[p]   Total: -   Model: gpt-6.1-sol", "必显示项读不到时占位并写明")
        XCTAssertEqual(tips(info, now), ["Total：ScyllaUI 暂不读取 OpenCode 的用量\n额度：ScyllaUI 暂不读取 OpenCode 的额度"],
                       "写明是 ScyllaUI 没读，不说成工具不支持，也不写成「还没有记录」")
        info.tool = "copilot"
        (info.model, info.modelSupported) = (nil, false)
        XCTAssertEqual(tips(info, now), ["Total：ScyllaUI 暂不读取 Copilot 的用量\n额度：还没读到。Agent 启动或一轮结束后读取；用第三方 API 时没有额度",
                                         "模型：ScyllaUI 暂不读取 Copilot 的模型"])
        (info.model, info.modelSupported, info.effortSupported) = ("gpt-6-luna", true, false)
        XCTAssertEqual(tips(info, now).last, "推理强度：ScyllaUI 暂不读取 Copilot 的推理强度", "推理强度读不到时写在模型上")
        info.tool = nil
        XCTAssertEqual(tips(info, now), [], "纯 Shell 没有悬停提示")

        let m = model()
        m.apply(["type": "snapshot", "rev": 1, "recent_total": 0, "projects": [project(1, "p", order: 0)],
                 "terminals": [terminal("a", project: 1, state: "ready", tool: "codex", fg: "agent", fgTool: "codex", conversation: 7),
                               terminal("b", project: 1, state: "ready", tool: "claude", fg: "agent", fgTool: "claude", conversation: 8)],
                 "conversations": [conversation(7, project: 1, tool: "codex"), conversation(8, project: 1, tool: "claude")],
                 "usage": [["conversation_id": 7, "usage": ["model": "gpt-6.1-sol", "model_source": "hook", "total_tokens": 35,
                                                             "tokens": ["input": 20, "cache_read": 10, "cache_write": 0, "output": 5]]]],
                 "quotas": [["tool": "codex", "quota": ["window": "7d", "used_percent": 3.0, "resets_at": 1_792_675_611_000,
                                                        "as_of": 1_792_071_102_250, "source": "log"]]]])
        var infos = m.statusInfos(build(m))
        XCTAssertEqual(infos["a"]?.usage?.total, 35)
        XCTAssertEqual(infos["a"]?.usage?.cacheRead, 10)
        XCTAssertEqual(infos["a"]?.quota?.usedPercent, 3.0)
        XCTAssertNil(infos["b"]?.quota, "Claude 没有周额度")
        m.apply(["type": "quota.upsert", "rev": 2, "tool": "claude",
                 "quota": ["window": "7d", "used_percent": 34.5, "resets_at": 1_792_675_611_000, "as_of": 1, "source": "statusline"]])
        m.apply(["type": "usage.upsert", "rev": 3, "conversation_id": 8, "usage": ["model": "claude-opus-5-5", "model_source": "statusline"]])
        infos = m.statusInfos(build(m))
        XCTAssertEqual(infos["b"]?.quota?.source, "statusline")
        XCTAssertEqual(infos["b"]?.model, "claude-opus-5-5")
        XCTAssertNil(infos["b"]?.usage?.total, "还没读到 token")
        // 5 小时窗口另存一份，不覆盖 7 天（2026-10-31）。
        m.apply(["type": "quota.upsert", "rev": 4, "tool": "claude",
                 "quota": ["window": "5h", "used_percent": 5.0, "resets_at": 1_792_075_611_000, "as_of": 2, "source": "statusline"]])
        m.apply(["type": "quota.upsert", "rev": 5, "tool": "codex", "quota": ["window": "5h", "unlimited": true, "as_of": 3, "source": "log"]])
        infos = m.statusInfos(build(m))
        XCTAssertEqual([infos["b"]?.quota5h?.usedPercent, infos["b"]?.quota?.usedPercent], [5.0, 34.5])
        XCTAssertEqual(infos["a"]?.quota5h?.unlimited, true)
        XCTAssertEqual(infos["a"]?.quota?.usedPercent, 3.0)
        m.apply(["type": "quota.upsert", "rev": 6, "tool": "codex",
                 "quota": ["window": "month", "used_percent": 1.0, "resets_at": 1_796_083_200_000, "as_of": 4, "source": "log"]])
        XCTAssertEqual(m.statusInfos(build(m))["a"]?.quotaMonth?.usedPercent, 1.0, "月额度另存一份")
        // 2026-11-01：快照里没有了的窗口删掉；查不到的原因随推送；用量带计费方式。
        m.apply(["type": "quota.remove", "rev": 7, "tool": "codex", "window": "5h"])
        m.apply(["type": "quota.note", "rev": 8, "tool": "codex", "note": "login_required"])
        m.apply(["type": "usage.upsert", "rev": 9, "conversation_id": 7, "usage": ["model": "gpt-6.1-sol", "model_source": "hook", "billing": "api"]])
        infos = m.statusInfos(build(m))
        XCTAssertNil(infos["a"]?.quota5h)
        XCTAssertEqual(infos["a"]?.quotaNote, "login_required")
        XCTAssertEqual(infos["a"]?.usage?.billing, "api")
        m.apply(["type": "quota.note", "rev": 10, "tool": "codex", "note": NSNull()])
        XCTAssertNil(m.statusInfos(build(m))["a"]?.quotaNote, "读到额度后原因清掉")
    }

    /// 按可用宽度排版（2026-11-01 标准化）：先把项目名截到最少 12 个字符、再把分支截到最少 8 个（截在中间，悬停看全），
    /// 还放不下依次去掉倒计时、推理强度、改动数、Total、模型。测量按每个字符 1 个单位。
    func testStatusLineFitsWidth() {
        let now: Int64 = 1_800_000_000_000
        var u = SBUsage(model: "gpt-6.1-sol", total: 1_234_567)
        u.effort = "high"
        let info = StatusInfo(projectID: 1, project: "06-project-agent-terminal-with-a-long-name", projectDir: "/w/06-project-agent-terminal-with-a-long-name",
                              branch: "feature/status-line-layout", git: SBGit(dirty: true, added: 3, deleted: 1, untracked: 0),
                              model: "gpt-6.1-sol", effort: "high", tool: "codex", usage: u,
                              quota: SBQuota(usedPercent: 34, resetsAt: now + 2 * 86_400_000, asOf: now, source: "log"))
        func fit(_ w: CGFloat) -> String {
            info.fitted(width: w, now: now) { CGFloat($0.map(\.text).joined().count) }.flatMap(\.spans).map(\.text).joined()
        }
        let full = info.text(now: now)
        XCTAssertEqual(fit(CGFloat(full.count)), full, "放得下原样")
        XCTAssertEqual(StatusInfo.middle("abcdefghijklmnop", 12), "abcdef…lmnop")
        XCTAssertEqual(StatusInfo.middle("short", 12), "short")
        let tail = " (feature/status-line-layout* +3 -1)   Total: 1.2M • 7d: 34% (2d0h)   Model: gpt-6.1-sol (high)"
        XCTAssertEqual(fit(CGFloat(tail.count + 22)), "[06-project…long-name]" + tail, "先截项目名，取放得下的最长（20 个字符）")
        XCTAssertEqual(fit(CGFloat(tail.count + 4)),
                       "[06-pro…-name] (feature/…-layout* +3 -1)   Total: 1.2M • 7d: 34% (2d0h)   Model: gpt-6.1-sol (high)",
                       "项目名截到 12 个字符还放不下，再截分支（这里截到 16 个）")
        XCTAssertEqual(fit(60), "[06-pro…-name] (featu…ayout*)   7d: 34%   Model: gpt-6.1-sol",
                       "依次去掉倒计时、推理强度、改动数、Total 后余下 3 个字符，分支放宽到 11 个")
        XCTAssertEqual(fit(20), "[06-pro…-name] (feat…out*)   7d: 34%", "最后去掉模型；项目名与额度百分比留到最后，其余由文字框截断")
        // 让出次要部分后有余量，项目名放回能放下的最长（2026-11-02 正式版验收发现停在最短）。
        let repo = StatusInfo(projectID: 1, project: "06-project-agent-terminal", branch: "main", model: "claude-sonnet-5-5", effort: "medium",
                              tool: "claude", usage: u, quota: SBQuota(usedPercent: 35, resetsAt: now + 86_400_000, asOf: now, source: "statusline"))
        func fitRepo(_ w: CGFloat) -> String {
            repo.fitted(width: w, now: now) { CGFloat($0.map(\.text).joined().count) }.flatMap(\.spans).map(\.text).joined()
        }
        XCTAssertEqual(fitRepo(50), "[06-project-agent-terminal] (main)   7d: 35%", "去掉模型后项目名放回全名")
        XCTAssertEqual(fitRepo(40), "[06-project…t-terminal] (main)   7d: 35%", "余量不够全名时取放得下的最长（21 个字符）")
        let segs = info.fitted(width: 60, now: now) { CGFloat($0.map(\.text).joined().count) }
        XCTAssertEqual(segs.first?.tooltip, "/w/06-project-agent-terminal-with-a-long-name", "项目名悬停显示完整路径")
        XCTAssertEqual(segs.first { $0.kind == .branch }?.tooltip, "feature/status-line-layout", "被截的分支悬停看全")
    }

    /// 搜索与只看待处理（M2 第 4 块）：命中的终端与「搜索结果」原地替换；旧查询的应答丢弃；新应答到达前沿用旧结果；
    /// 清除后与搜索前一致；只看待处理只留「需确认 / 待查看」并随推送进出、隐藏最近任务；角标数不受筛选影响。
    func testSearchAndPendingFilter() {
        let m = model()
        m.apply(["type": "snapshot", "rev": 1, "recent_total": 0,
                 "projects": [project(1, "p", order: 0), project(2, "q", order: 1)],
                 "terminals": [terminal("a", project: 1, created: 1, state: "waiting", tool: "claude", fg: "agent", fgTool: "claude", conversation: 7),
                               terminal("b", project: 1, created: 2, state: "ready", tool: "codex", fg: "agent", fgTool: "codex", unseen: true, conversation: 8),
                               terminal("c", project: 2, created: 3)],
                 "conversations": [conversation(7, project: 1), conversation(8, project: 1, tool: "codex")]])
        let before = build(m)
        let s1 = m.setSearch("x")
        XCTAssertEqual(build(m), before, "应答到达前不变")
        let s2 = m.setSearch("xy")
        XCTAssertFalse(m.applySearch(["ok": true, "terminals": ["c"], "conversations": []], seq: s1), "旧查询的应答丢弃")
        XCTAssertTrue(m.applySearch(["ok": true, "terminals": ["b"],
                                     "conversations": [conversation(30, project: 2, ended: 100, title: "旧任务")]], seq: s2))
        var t = build(m)
        XCTAssertEqual(t.kids(SidebarKey.current), ["pc:1"], "只剩有命中的项目")
        XCTAssertEqual(t.kids("pc:1"), ["t:b"])
        XCTAssertEqual(t.rows[SidebarKey.current], .section(title: "当前任务", count: 1))
        XCTAssertEqual(t.rows[SidebarKey.recent], .section(title: "搜索结果", count: 1))
        XCTAssertEqual(t.kids(SidebarKey.recent), ["pr:2"])
        XCTAssertEqual(t.kids("pr:2"), ["c:30"])
        XCTAssertEqual(m.pendingTotal, 1, "角标按全部终端统计")
        _ = m.setSearch("zz")
        XCTAssertEqual(build(m), t, "新应答到达前沿用旧结果")
        let s3 = m.searchSeq
        m.applySearch(["ok": true, "terminals": [], "conversations": []], seq: s3)
        t = build(m)
        XCTAssertEqual(t.kids(SidebarKey.current), [SidebarKey.noMatch])
        XCTAssertEqual(t.kids(SidebarKey.recent), [SidebarKey.empty])
        // 搜索结果最多显示最新 50 条，多出时标「+」。
        let s4 = m.setSearch("many")
        m.applySearch(["ok": true, "terminals": [],
                       "conversations": (1...51).map { conversation(Int64(100 + $0), project: 2, ended: Int64(1000 - $0)) }], seq: s4)
        t = build(m)
        XCTAssertEqual(t.rows[SidebarKey.recent], .section(title: "搜索结果", count: 50, more: true))
        XCTAssertEqual(t.kids("pr:2").count, 50)
        _ = m.setSearch(nil)
        XCTAssertEqual(build(m), before, "清除后与搜索前一致")
        // 只看待处理。
        m.setPendingOnly(true)
        t = build(m)
        XCTAssertEqual(t.kids(SidebarKey.root), [SidebarKey.current], "隐藏最近任务")
        XCTAssertEqual(t.kids("pc:1"), ["t:a", "t:b"], "需确认的 a 与待查看的 b")
        XCTAssertNil(t.rows["pc:2"])
        m.apply(["type": "terminal.upsert", "rev": 2,
                 "terminal": terminal("a", project: 1, created: 1, state: "ready", tool: "claude", fg: "agent", fgTool: "claude", conversation: 7)])
        let filtered = build(m)
        XCTAssertEqual(filtered.kids("pc:1"), ["t:b"], "处理完即移出")
        XCTAssertEqual(m.pendingTotal, 0)
        // 移出列表的终端仍有行：右侧正在显示它时，标题栏、窗口标题与历史提示词照常按它显示（2026-11-13 长跑发现）。
        XCTAssertEqual(filtered.terminalRows["a"]?.toolKey, "claude")
        XCTAssertEqual(filtered.terminalRows["a"]?.conversationID, 7)
        let filteredInfo = m.statusInfos(filtered)["a"]
        XCTAssertEqual(filteredInfo?.tool, "claude", "移出列表也保留工具，标题栏照常显示模型与额度")
        m.apply(["type": "terminal.upsert", "rev": 3, "terminal": terminal("c", project: 2, created: 3, unseen: true)])
        XCTAssertEqual(build(m).kids("pc:2"), ["t:c"], "新的待查看实时进入")
        m.setPendingOnly(false)
        let all = build(m)
        XCTAssertEqual(all.kids(SidebarKey.root), [SidebarKey.current, SidebarKey.recent])
        XCTAssertEqual(filteredInfo, m.statusInfos(all)["a"], "状态行信息不受筛选影响")
    }

    /// 启动目录提示（M3；`cwd_missing` 2026-11-13 起与 `cwd_denied` 分开）：各对应一个徽标，没有提示时都不显示。
    func testCwdNotices() {
        let m = model()
        var a = terminal("a", project: 1), b = terminal("b", project: 1, created: 1), c = terminal("c", project: 1, created: 2)
        a["notice"] = "cwd_denied"
        b["notice"] = "cwd_missing"
        m.apply(["type": "snapshot", "rev": 1, "recent_total": 0, "projects": [project(1, "p", order: 0)],
                 "terminals": [a, b, c], "conversations": []])
        let t = build(m)
        XCTAssertEqual(task(t, "t:a").map { [$0.cwdDenied, $0.cwdMissing] }, [true, false])
        XCTAssertEqual(task(t, "t:b").map { [$0.cwdDenied, $0.cwdMissing] }, [false, true])
        XCTAssertEqual(task(t, "t:c").map { [$0.cwdDenied, $0.cwdMissing] }, [false, false])
    }

    /// 底部提示栏（M1 第 7 块）：状态同步未生效 2026-10-28 起不进提示栏（入口只留终端行的「未同步」）。M4b 块 6 起只是版本
    /// 未验证不提示，Hook 数据有异常时提示栏提示「状态可能不准」，原因与版本在「详情…」里；2026-10-30 起终端行的悬停提示
    /// 不再写版本与原因。
    func testHookBannerAndVersionTooltip() {
        let m = model()
        func hook(_ tool: String, _ state: String, fixable: Bool = false) -> [String: Any] {
            ["tool": tool, "state": state, "path": "/h/\(tool)", "problems": [], "fixable": fixable]
        }
        var claude = terminal("c", project: 1, state: "ready", tool: "claude", fg: "agent", fgTool: "claude")
        claude["foreground"] = ["kind": "agent", "tool": "claude", "version": "2.1.300", "version_verified": false]
        var codex = terminal("x", project: 1, fg: "agent", fgTool: "codex")
        // 2026-11-20 起「未同步」按后台给的这个进程的同步情况（`foreground.sync`）显示，不只看配置状态。
        codex["foreground"] = ["kind": "agent", "tool": "codex", "version": "0.162.1", "version_verified": true, "sync": "off"]
        m.apply(["type": "snapshot", "rev": 1, "recent_total": 0, "projects": [project(1, "p", order: 0)],
                 "terminals": [claude, codex], "conversations": [],
                 "hooks": [hook("claude", "ok"), hook("codex", "missing", fixable: true), hook("kimi", "absent"),
                           hook("grok", "ok")]])
        XCTAssertTrue(m.banner().isEmpty, "状态同步未生效不进提示栏")
        let t = build(m)
        XCTAssertEqual(task(t, "t:x")?.unsynced, true, "这个进程没在同步：终端行显示「未同步」")
        XCTAssertEqual(task(t, "t:c")?.unsynced, false)
        XCTAssertNil(task(t, "t:c")?.note, "版本不进悬停提示（2026-10-30）")
        // Hook 数据有异常：提示栏第二行列出工具（M4b 块 6）。
        claude["foreground"] = ["kind": "agent", "tool": "claude", "version": "2.1.300", "version_verified": false,
                                "anomaly": "unknown_event:SessionBegin"]
        m.apply(["type": "terminal.upsert", "rev": 2, "terminal": claude])
        XCTAssertEqual(m.banner().inaccurate, ["Claude"])
        // 「详情…」的内容：版本、原因与最新的已验证版本（后台 `verified_latest`）。
        XCTAssertEqual(m.banner().details, [InaccurateItem(tool: "claude", versions: ["2.1.300"],
                                                           reasons: ["收到不认识的事件 SessionBegin"], verifiedLatest: nil)])
        XCTAssertEqual(SBHookStatus(json: ["tool": "codex", "state": "ok", "verified_latest": "0.164.0"])?.verifiedLatest, "0.164.0")
        XCTAssertEqual(SidebarModel.anomalyText("missing_session:Stop"), "Stop 事件缺少会话 ID")
        XCTAssertEqual(SidebarModel.anomalyText("unparsable"), "收到无法解析的 Hook 数据")
        // 修好后「未同步」随之消失。
        m.apply(["type": "hooks.upsert", "rev": 3,
                 "hooks": [hook("claude", "policy"), hook("codex", "ok"), hook("kimi", "absent"), hook("grok", "ok")]])
        codex["foreground"] = ["kind": "agent", "tool": "codex", "version": "0.162.1", "version_verified": true]
        m.apply(["type": "terminal.upsert", "rev": 3, "terminal": codex])
        XCTAssertEqual(task(build(m), "t:x")?.unsynced, false, "后台推来新的同步情况后终端行重算")
        // 有异常的进程退出后，提示随之消失；全部正常时提示栏不出现。
        m.apply(["type": "terminal.upsert", "rev": 4, "terminal": terminal("c", project: 1)])
        m.apply(["type": "hooks.upsert", "rev": 5,
                 "hooks": [hook("claude", "ok"), hook("codex", "ok"), hook("kimi", "absent"), hook("grok", "ok")]])
        XCTAssertTrue(m.banner().isEmpty)
        // 不能修复的两种带具体原因。
        let reasons = [
            SBHookStatus(tool: "kimi", state: "unreadable", problems: ["hooks_inline"], fixable: false),
            SBHookStatus(tool: "claude", state: "unreadable", problems: ["parse:EOF at line 3"], fixable: false),
            SBHookStatus(tool: "grok", state: "policy", problems: ["policy:/h/.grok/requirements.toml"], fixable: false),
        ].map(\.reason)
        XCTAssertEqual(reasons, ["hooks 写成了行内数组（hooks = […]），ScyllaUI 只支持 [[hooks]] 写法，未改动",
                                 "配置文件格式有误（EOF at line 3），未改动",
                                 "被组织策略禁用（/h/.grok/requirements.toml），无法修复"])
        // 条目都在、只是要更新（重复、旧写法）：文字单独说；还缺条目时仍按缺失说。「设置 › Agent」的状态文字用同一个
        // 判断（`outdatedOnly`），AgentPane 不在测试目标里，那一句在真实窗口里核对。
        let outdated = SBHookStatus(tool: "kimi", state: "missing", problems: ["outdated:block"], fixable: true)
        XCTAssertTrue(outdated.outdatedOnly)
        XCTAssertEqual(outdated.reason, "Hook 条目需要更新")
        let mixed = SBHookStatus(tool: "claude", state: "missing", problems: ["missing:Stop", "outdated:SessionStart"], fixable: true)
        XCTAssertFalse(mixed.outdatedOnly)
        XCTAssertEqual(mixed.reason, "Hook 条目缺失")
        let gone = SBHookStatus(tool: "grok", state: "missing", problems: ["missing:hooks_file"], fixable: true)
        XCTAssertFalse(gone.outdatedOnly)
        XCTAssertEqual(gone.reason, "Hook 条目缺失")
        XCTAssertFalse(SBHookStatus(tool: "kimi", state: "ok", problems: [], fixable: false).outdatedOnly)
        // 旧推送（rev 不大于快照）不生效。
        m.apply(["type": "snapshot", "rev": 10, "recent_total": 0, "projects": [], "terminals": [], "conversations": [],
                 "hooks": [hook("grok", "disabled", fixable: true)]])
        m.apply(["type": "hooks.upsert", "rev": 9, "hooks": [hook("grok", "ok")]])
        XCTAssertEqual(m.hooks.map(\.state), ["disabled"])
    }

    /// 「未同步」与「下次失效」（M4b 块 5 P2；2026-11-20 改为按后台给的这个进程真实的同步情况 `foreground.sync` 显示，
    /// docs/design.md 4.7 节「会话是否真的在同步」）：
    /// - `off`、`new_session`、`restart`：这个会话此刻没在同步，显示「未同步」，用户撤销过这款也显示；
    /// - `old`：配置已失效但这个会话还在同步，不显示「未同步」，没撤销过时显示「下次失效」；
    /// - 没有 `sync`：在同步，什么都不标，哪怕配置状态此刻不正常（以后台的判断为准）。
    func testUnsyncedBadge() {
        let m = model()
        func hook(_ tool: String, _ state: String) -> [String: Any] {
            ["tool": tool, "state": state, "path": "/h/\(tool)", "problems": [], "fixable": state == "missing"]
        }
        func agent(_ id: String, _ tool: String, sync: String? = nil, entriesAtStart: Bool? = nil) -> [String: Any] {
            var t = terminal(id, project: 1, fg: "agent", fgTool: tool)
            var f = t["foreground"] as? [String: Any] ?? [:]
            f["sync"] = sync
            f["entries_at_start"] = entriesAtStart
            t["foreground"] = f
            return t
        }
        m.apply(["type": "snapshot", "rev": 1, "recent_total": 0, "projects": [project(1, "p", order: 0)],
                 "terminals": [agent("x", "codex", sync: "off", entriesAtStart: false), agent("k", "kimi"), terminal("s", project: 1, fg: "shell"),
                               agent("o", "grok", sync: "old"), agent("n", "copilot", sync: "new_session"), agent("r", "kimi", sync: "restart"),
                               agent("q", "claude")],
                 "conversations": [],
                 "hooks": [hook("codex", "untrusted"), hook("kimi", "ok"), hook("grok", "missing"), hook("copilot", "ok"), hook("claude", "missing")]])
        var t = build(m)
        XCTAssertEqual(task(t, "t:x").map { [$0.unsynced, $0.syncNext] }, [true, false])
        XCTAssertEqual(task(t, "t:x")?.fgTool, "codex")
        XCTAssertEqual(task(t, "t:k").map { [$0.unsynced, $0.syncNext] }, [false, false], "在同步的不标")
        XCTAssertEqual(task(t, "t:s").map { [$0.unsynced, $0.syncNext] }, [false, false], "Shell 不标")
        XCTAssertNil(task(t, "t:s")?.fgTool)
        XCTAssertEqual(task(t, "t:o").map { [$0.unsynced, $0.syncNext] }, [false, true], "还在同步、下次失效")
        XCTAssertEqual(task(t, "t:n").map { [$0.unsynced, $0.syncNext] }, [true, false], "配置正常但这个进程读不到：未同步")
        XCTAssertEqual(task(t, "t:r").map { [$0.unsynced, $0.syncNext] }, [true, false])
        XCTAssertEqual(task(t, "t:q").map { [$0.unsynced, $0.syncNext] }, [false, false], "后台没说它没在同步就不标")
        let infos = m.statusInfos(t)
        XCTAssertEqual(infos["x"]?.fgAgent, "codex", "状态行信息带前台 AI，供找正在运行的 Codex")
        XCTAssertEqual([infos["x"]?.fgSync, infos["n"]?.fgSync, infos["k"]?.fgSync], ["off", "new_session", nil])
        XCTAssertEqual([infos["x"]?.fgEntriesAtStart, infos["k"]?.fgEntriesAtStart], [false, nil], "启动时条目在不在，供 Codex 信任引导分路")
        // 用户撤销过：真的没在同步的照样显示「未同步」；还在同步的不再提示「下次失效」。
        m.setHooksRemoved(["codex", "grok", "copilot"])
        t = build(m)
        XCTAssertEqual(task(t, "t:x")?.unsynced, true, "撤销过的也显示")
        XCTAssertEqual(task(t, "t:n")?.unsynced, true)
        XCTAssertEqual(task(t, "t:o").map { [$0.unsynced, $0.syncNext] }, [false, false], "自己撤的、还在同步：不提示")
        m.setHooksRemoved([])
        XCTAssertEqual(task(build(m), "t:o")?.syncNext, true)
        // 后台推来「在同步」：徽标消失。
        m.apply(["type": "terminal.upsert", "rev": 2, "terminal": agent("x", "codex")])
        m.apply(["type": "terminal.upsert", "rev": 3, "terminal": agent("o", "grok")])
        t = build(m)
        XCTAssertEqual(task(t, "t:x")?.unsynced, false, "修好即消失")
        XCTAssertEqual(task(t, "t:o")?.syncNext, false)
        // Hook 状态里带何时读取与引导命令。
        let codex = SBHookStatus(json: ["tool": "codex", "state": "ok", "load": "session", "command": "codex", "new_session": "/new",
                                        "quit": "/quit", "resume_last": "codex resume --last"])
        XCTAssertEqual(codex?.load, "session")
        XCTAssertEqual(codex?.commands, SyncGuide.Commands(command: "codex", newSession: "/new", quit: "/quit", resumeLast: "codex resume --last"))
        XCTAssertEqual(SBHookStatus(json: ["tool": "claude", "state": "ok", "load": "live"])?.commands, SyncGuide.Commands())
    }

    /// 重建时没能接回的 AI（M4b 块 5 P2）：状态同步未生效的要说明并修复，正常的只说还没开始对话；撤销过的、状态未知的不说。
    func testLostAgentsClassification() {
        let hooks = [
            SBHookStatus(tool: "claude", state: "ok", problems: [], fixable: false),
            SBHookStatus(tool: "codex", state: "untrusted", problems: [], fixable: false),
            SBHookStatus(tool: "kimi", state: "missing", problems: [], fixable: true),
            SBHookStatus(tool: "grok", state: "absent", problems: [], fixable: false),
        ]
        let l = LostAgents(tools: ["kimi", "codex", "claude", "codex", "grok", "other"], hooks: hooks, removed: [])
        XCTAssertEqual(l.broken, [.init(tool: "codex", count: 2), .init(tool: "kimi", count: 1)])
        XCTAssertEqual(l.idle, [.init(tool: "claude", count: 1)])
        let r = LostAgents(tools: ["codex", "claude"], hooks: hooks, removed: ["codex"])
        XCTAssertEqual(r.broken, [], "撤销过的不说")
        XCTAssertEqual(r.idle, [.init(tool: "claude", count: 1)])
        XCTAssertEqual(LostAgents(tools: [], hooks: hooks, removed: []), LostAgents(tools: ["grok"], hooks: hooks, removed: []))
        // OpenCode（M4b 块 9a）排在现有四款之后。
        let o = LostAgents(tools: ["opencode", "kimi"], hooks: hooks + [SBHookStatus(tool: "opencode", state: "missing", problems: [], fixable: true)],
                           removed: [])
        XCTAssertEqual(o.broken, [.init(tool: "kimi", count: 1), .init(tool: "opencode", count: 1)])
        XCTAssertEqual(SidebarModel.toolName("opencode"), "OpenCode")
        // Copilot（M4b 块 9c）排在 OpenCode 之后。
        let c = LostAgents(tools: ["copilot", "opencode"], hooks: hooks + [SBHookStatus(tool: "opencode", state: "missing", problems: [], fixable: true),
                                                                         SBHookStatus(tool: "copilot", state: "missing", problems: [], fixable: true)],
                           removed: [])
        XCTAssertEqual(c.broken, [.init(tool: "opencode", count: 1), .init(tool: "copilot", count: 1)])
        XCTAssertEqual(SidebarModel.toolName("copilot"), "Copilot")
        // Antigravity（M4b 块 9b）排在 Copilot 之后。
        let g = LostAgents(tools: ["antigravity", "copilot"], hooks: hooks + [SBHookStatus(tool: "copilot", state: "missing", problems: [], fixable: true),
                                                                            SBHookStatus(tool: "antigravity", state: "missing", problems: [], fixable: true)],
                           removed: [])
        XCTAssertEqual(g.broken, [.init(tool: "copilot", count: 1), .init(tool: "antigravity", count: 1)])
        XCTAssertEqual(SidebarModel.toolName("antigravity"), "Antigravity")
    }

    /// 第 1 行显示任务标题、第 2 行照旧是最新提示词，各款规则一致（2026-11-01 用户定）：标题就是第一条提示词的，问第一句
    /// 后两行相同，问第二句后第 2 行换成新的提示词。
    func testHeadingAndLatestPrompt() {
        let m = model()
        m.apply(["type": "snapshot", "rev": 1, "recent_total": 0, "projects": [project(1, "p", order: 0)],
                 "terminals": [terminal("k", project: 1, state: "ready", tool: "kimi", conversation: 5)],
                 "conversations": [conversation(5, project: 1, title: "整理发布说明", prompt: "整理发布说明")]])
        let k = task(build(m), "t:k")!
        XCTAssertEqual([k.heading, k.subtitle], ["整理发布说明", "整理发布说明"], "两行相同也照旧显示，不另设特例")
        XCTAssertFalse(k.subtitleIsPath)
        m.apply(["type": "conversation.upsert", "rev": 2,
                 "conversation": conversation(5, project: 1, title: "整理发布说明", prompt: "再补一段")])
        let k2 = task(build(m), "t:k")!
        XCTAssertEqual([k2.heading, k2.subtitle], ["整理发布说明", "再补一段"])
    }

    func testTerminalRowFollowsFourLayers() {
        let m = model()
        m.apply(["type": "snapshot", "rev": 1, "recent_total": 0, "projects": [project(1, "p", order: 0)],
                 "terminals": [
                    terminal("w", project: 1, state: "waiting", tool: "codex", unseen: true, conversation: 3),
                    terminal("r", project: 1, state: "running", tool: "claude", conversation: 4, name: "我的名字"),
                    terminal("u", project: 1, state: "unknown", fg: "agent", fgTool: "kimi"),
                    terminal("e", project: 1, state: "ended", tool: "grok", fg: "command"),
                    terminal("s", project: 1)],
                 "conversations": [conversation(3, project: 1, title: "修一下测试", prompt: "再看看\n日志"),
                                   conversation(4, project: 1, title: "原生标题", prompt: "第二问")]])
        let t = build(m)
        let w = task(t, "t:w")!
        XCTAssertEqual([w.tool, w.state], ["Codex", "Waiting"])
        XCTAssertEqual(w.lamp, .waiting)
        XCTAssertTrue(w.needsConfirm && w.unseen, "待处理与未查看可以同时出现")
        XCTAssertEqual(w.subtitle, "再看看 日志", "副标题为最新提示词（2026-10-14），换行压成一行")
        XCTAssertEqual(w.fullText, "再看看\n日志", "悬停给最新提示词的全文，保留换行（2026-10-30）")
        XCTAssertEqual(w.toolKey, "codex", "图标按工具键取")
        let r = task(t, "t:r")!
        XCTAssertEqual(r.lamp, .running)
        XCTAssertEqual(r.name, "我的名字", "用户命名代替工具名显示在第 1 行（2026-10-25）")
        XCTAssertEqual(r.subtitle, "第二问", "命名不再占用副标题")
        XCTAssertEqual(r.taskTitle, "原生标题", "任务标题供删除确认框与历史提示词栏头用")
        XCTAssertEqual(w.heading, "修一下测试", "第 1 行显示任务标题（2026-11-01），工具靠图标区分")
        XCTAssertEqual(r.heading, "原生标题", "有用户命名时第 1 行仍显示命名（TaskCell 取 name 优先）")
        var cleared = terminal("r", project: 1, state: "running", tool: "claude", conversation: 4)
        cleared["name"] = nil
        m.apply(["type": "terminal.upsert", "rev": 2, "terminal": cleared])
        XCTAssertNil(task(build(m), "t:r")?.name, "清空改名后第 1 行回到工具名")
        XCTAssertFalse(r.needsConfirm || r.unseen)
        let u = task(t, "t:u")!
        XCTAssertEqual([u.tool, u.state], ["Kimi", "未知"], "只有前台识别、没有 Hook")
        XCTAssertNil(u.heading, "没有对话时第 1 行为工具名")
        let e = task(t, "t:e")!
        XCTAssertEqual([e.tool, e.state], ["Shell", "运行命令"], "AI 已结束：回到前台层")
        XCTAssertEqual(e.fullText, "/w/p1", "目录副标题的全文为完整路径")
        XCTAssertEqual(e.subtitle, "/w/p1", "已结束的对话标题不再显示")
        let s = task(t, "t:s")!
        XCTAssertEqual(s.tool, "Shell")
        XCTAssertNil(s.toolKey, "Shell 没有工具键，用系统符号")
        XCTAssertNil(s.state)
        XCTAssertEqual(s.lamp, .idle)
        // 上报过当前目录后，副标题用当前目录（M1 第 4 块）。
        var moved = terminal("s", project: 1)
        moved["current_dir"] = "/w/repo/src"
        m.apply(["type": "terminal.upsert", "rev": 3, "terminal": moved])
        XCTAssertEqual(task(build(m), "t:s")?.subtitle, "/w/repo/src")
        XCTAssertEqual(task(build(m), "t:s")?.subtitleIsPath, true, "目录副标题单行、截掉开头")
        XCTAssertEqual(task(build(m), "t:w")?.subtitleIsPath, false)
    }

    /// 终端副标题里的目录相对项目根（2026-10-25 用户确认）：根为项目文件夹名、子目录为「项目/子目录」，主目录用「~」；
    /// `/var/…` 与项目根的 `/private/var/…` 视为同一路径；不在项目根下为完整路径。
    func testRelativeDir() {
        let m = model()
        let home = NSHomeDirectory()
        XCTAssertEqual(m.relativeDir("/w/alpha", root: "/w/alpha"), "alpha")
        XCTAssertEqual(m.relativeDir("/w/alpha/src/ui", root: "/w/alpha"), "alpha/src/ui")
        XCTAssertEqual(m.relativeDir("/var/folders/x/T/pw/alpha/src", root: "/private/var/folders/x/T/pw/alpha"), "alpha/src")
        XCTAssertEqual(m.relativeDir("/private/tmp/a", root: "/tmp/a"), "a")
        XCTAssertEqual(m.relativeDir(home, root: home), "~")
        XCTAssertEqual(m.relativeDir(home + "/notes", root: home), "~/notes")
        XCTAssertEqual(m.relativeDir("/w/alphabet", root: "/w/alpha"), "/w/alphabet", "前缀相同但不是子目录")
        XCTAssertEqual(m.relativeDir(home + "/x", root: nil), "~/x")
    }

    func testRecentWindowCapAndOldRevs() {
        let m = model()
        m.apply(["type": "snapshot", "rev": 10, "recent_total": 2, "projects": [], "terminals": [], "conversations": []])
        m.applyRecent(page([conversation(4, project: 1, ended: at(400)), conversation(3, project: 1, ended: at(300))]))
        // 新结束的对话插到顶部；显示窗口（14 天）之外的不显示。
        m.apply(["type": "conversation.upsert", "rev": 11, "recent_total": 3,
                 "conversation": conversation(5, project: 1, ended: at(500))])
        m.apply(["type": "conversation.upsert", "rev": 12, "recent_total": 3,
                 "conversation": conversation(1, project: 1, ended: nowMs - 15 * day, title: "旧")])
        XCTAssertEqual(m.recent.map(\.id), [5, 4, 3])
        XCTAssertEqual(build(m).rows["sec:recent"], .section(title: "最近任务", count: 3))
        // 旧 rev 的推送丢弃。
        m.apply(["type": "terminal.upsert", "rev": 9, "terminal": terminal("x", project: 1)])
        XCTAssertTrue(m.terminals.isEmpty)
        // 重新打开：离开最近任务；清理后总数变小：裁掉最旧的尾部。
        m.apply(["type": "conversation.upsert", "rev": 13, "recent_total": 2, "conversation": conversation(4, project: 1)])
        XCTAssertEqual(m.recent.map(\.id), [5, 3])
        m.apply(["type": "conversation.upsert", "rev": 14, "recent_total": 2,
                 "conversation": conversation(6, project: 2, ended: at(600))])
        XCTAssertEqual(m.recent.map(\.id), [6, 5])
        // 时间推移：结束时间落到窗口外的条目在下一次建树时移出（没有推送也一样）。
        m.now = { Date(timeIntervalSince1970: TimeInterval(at(550) + 14 * day) / 1000) }
        XCTAssertEqual(build(m).rows["sec:recent"], .section(title: "最近任务", count: 1))
        XCTAssertEqual(m.recent.map(\.id), [6])
    }

    /// 被后续对话接替的（Claude 转入后台继续，`end_reason=continued`）不进「最近任务」；普通替换（`/clear`）照常进入。
    func testContinuedConversationIsNotRecent() {
        let m = model()
        m.apply(["type": "snapshot", "rev": 1, "recent_total": 0, "projects": [], "terminals": [],
                 "conversations": [conversation(1, project: 1, title: "今天星期几？")]])
        m.applyRecent(page([conversation(7, project: 1, ended: at(100), reason: "continued")]))
        XCTAssertTrue(m.recent.isEmpty, "查询结果里的也排除")
        m.apply(["type": "conversation.upsert", "rev": 2, "recent_total": 0,
                 "conversation": conversation(1, project: 1, ended: at(200), title: "今天星期几？", reason: "continued")])
        XCTAssertTrue(m.recent.isEmpty, "推送来的也排除")
        XCTAssertNil(m.active[1], "已结束，不再算进行中")
        m.apply(["type": "conversation.upsert", "rev": 3, "recent_total": 1,
                 "conversation": conversation(2, project: 1, ended: at(300), reason: "replaced")])
        XCTAssertEqual(m.recent.map(\.id), [2])
    }

    /// 最近任务行（M1 第 6 块）：状态一律 Stopped；副标题为最后一条 AI 回复（原文），没有时为任务标题；
    /// 时间为最后一条回复的时间，没有时为结束时间；悬停只给副标题的全文（2026-10-30）。
    func testRecentRowShowsLastReply() {
        let m = model()
        m.apply(["type": "snapshot", "rev": 1, "recent_total": 2, "projects": [], "terminals": [], "conversations": []])
        var a = conversation(7, project: 1, ended: at(100), title: "解释 Argon", reason: "process_exit")
        a["last_reply"] = "主人，**Argon** 的中文意思是「**氩**」"
        a["last_reply_at"] = nowMs - 2 * day
        m.applyRecent(page([a, conversation(8, project: 1, ended: at(90), title: "旧对话")]))
        let tree = build(m)
        let r = task(tree, "c:7")
        XCTAssertEqual(r?.state, "Stopped")
        XCTAssertEqual(r?.subtitle, "主人，**Argon** 的中文意思是「**氩**」", "Markdown 原样显示")
        XCTAssertEqual(r?.time, TimeText(now: now, calendar: utc).format(nowMs - 2 * day), "取回复时间")
        XCTAssertEqual(r?.fullText, "主人，**Argon** 的中文意思是「**氩**」")
        XCTAssertNil(r?.note)
        XCTAssertEqual(task(tree, "c:8")?.subtitle, "旧对话", "没有回复时为任务标题")
        XCTAssertNil(task(tree, "c:8")?.note, "能读回复、只是还没有时不写不支持")
        // 读不到回复的工具（快照的 `tools`）：悬停提示写明 ScyllaUI 不读，不只是空着（docs/ui-methodology.md 1.7 节）。
        m.apply(["type": "snapshot", "rev": 2, "recent_total": 1, "projects": [], "terminals": [], "conversations": [],
                 "tools": [["tool": "opencode", "reply": false, "prompts": false, "usage": false]]])
        m.applyRecent(page([conversation(9, project: 1, ended: at(80), tool: "opencode", title: "升级依赖")]))
        XCTAssertEqual(task(build(m), "c:9")?.note, "ScyllaUI 暂不读取 OpenCode 的回复")
        XCTAssertEqual(task(tree, "c:8")?.state, "Stopped")
    }

    /// 还没提问的 AI 会话：知道会话 ID 时「新会话 · ID 前 8 位」，不知道时「新会话 · 启动时间」；纯 Shell 仍为目录。
    func testNewSessionPlaceholder() {
        let m = model()
        // fg 回到的旧 Claude：终端绑定过（已被替换的）对话，前台是 Claude 但没有 Hook 状态。
        var back = terminal("v", project: 1, created: 5, state: "ended", tool: "codex", fg: "agent", fgTool: "claude", conversation: 9)
        back["foreground"] = ["kind": "agent", "tool": "claude", "started_at": nowMs - 600_000]
        var codex = terminal("x", project: 1, created: 1, fg: "agent", fgTool: "codex")
        codex["foreground"] = ["kind": "agent", "tool": "codex", "started_at": nowMs - 60_000]
        var claude = terminal("y", project: 1, created: 2, state: "ready", tool: "claude", conversation: 3)
        claude["agent"] = ["tool": "claude", "state": "ready", "session_id": "12246079-f2b6-4936"]
        var kimi = terminal("z", project: 1, created: 3, state: "ready", tool: "kimi", conversation: 4)
        kimi["agent"] = ["tool": "kimi", "state": "ready", "session_id": "session_61f5a8db-b5a2"]
        m.apply(["type": "snapshot", "rev": 1, "recent_total": 0, "projects": [project(1, "p", order: 0)],
                 "terminals": [codex, claude, kimi, terminal("w", project: 1, created: 4), back],
                 "conversations": [conversation(3, project: 1, tool: "claude", prompt: nil),
                                   conversation(4, project: 1, tool: "kimi", prompt: nil)]])
        let tree = build(m)
        XCTAssertEqual(task(tree, "t:x")?.subtitle, "新会话 · \(TimeText(now: now, calendar: utc).format(nowMs - 60_000))")
        XCTAssertEqual(task(tree, "t:y")?.subtitle, "新会话 · 12246079")
        XCTAssertEqual(task(tree, "t:z")?.subtitle, "新会话 · 61f5a8db", "去掉 session_ 前缀")
        XCTAssertEqual(task(tree, "t:w")?.subtitle, "/w/p1")
        XCTAssertEqual(task(tree, "t:v")?.subtitle, "/w/p1", "绑定过对话的终端不显示「新会话 · 启动时间」")
        // 绑定着（被挂起的）Claude、前台是 Codex：按前台显示「Codex 未知」，不显示 Claude 的状态与提示词。
        var mixed = terminal("v", project: 1, created: 5, state: "ready", tool: "claude", fg: "agent", fgTool: "codex", conversation: 3)
        mixed["agent"] = ["tool": "claude", "state": "ready", "session_id": "12246079-f2b6"]
        m.apply(["type": "terminal.upsert", "rev": 2, "terminal": mixed])
        let r = task(build(m), "t:v")
        XCTAssertEqual([r?.tool, r?.state, r?.subtitle], ["Codex", "未知", "/w/p1"])
    }

    /// 一键恢复（M1 第 6 块）：用户从「最近任务」移除的（`unrecoverable`）不显示；恢复中（状态未知）的终端即以任务标题为副标题；
    /// 同一对话同时开在两个终端时两行都标「冲突」，徽标的悬停提示写终端数。
    func testResumeRowsAndConflict() {
        let m = model()
        m.apply(["type": "snapshot", "rev": 1, "recent_total": 1, "projects": [project(1, "p", order: 0)],
                 "terminals": [terminal("a", project: 1, created: 1), terminal("b", project: 1, created: 2)],
                 "conversations": []])
        m.applyRecent(page([conversation(7, project: 1, ended: at(100), title: "修导航焦点样式"),
                            conversation(8, project: 1, ended: at(90), reason: "unrecoverable")]))
        XCTAssertEqual(m.recent.map(\.id), [7], "移除的不显示")
        // 恢复：对话重开（移回当前任务），终端绑定它但 Agent 状态未知（Codex 首次提问前没有 Hook）。
        var t = terminal("a", project: 1, created: 1, tool: "codex", fg: "agent", fgTool: "codex", conversation: 7)
        m.apply(["type": "conversation.upsert", "rev": 2, "recent_total": 0,
                 "conversation": conversation(7, project: 1, title: "修导航焦点样式", prompt: "顺便检查键盘顺序")])
        m.apply(["type": "terminal.upsert", "rev": 3, "terminal": t])
        var tree = build(m)
        XCTAssertNil(tree.rows["c:7"], "移回当前任务")
        XCTAssertEqual(task(tree, "t:a")?.subtitle, "顺便检查键盘顺序", "恢复中（状态未知）即显示最新提示词")
        XCTAssertEqual(task(tree, "t:a")?.state, "未知")
        XCTAssertEqual(task(tree, "t:a")?.conflictTerminals, 0)
        // 另一个终端手动恢复了同一对话：两行都标冲突。
        t["conflict_with"] = ["b"]
        var u = terminal("b", project: 1, created: 2, state: "ready", tool: "codex", conversation: 7)
        u["conflict_with"] = ["a"]
        m.apply(["type": "terminal.upsert", "rev": 4, "terminal": t])
        m.apply(["type": "terminal.upsert", "rev": 5, "terminal": u])
        tree = build(m)
        XCTAssertEqual(task(tree, "t:a")?.conflictTerminals, 2)
        XCTAssertEqual(task(tree, "t:b")?.conflictTerminals, 2)
        u["conflict_with"] = [String]()
        m.apply(["type": "terminal.upsert", "rev": 6, "terminal": u])
        XCTAssertEqual(task(build(m), "t:b")?.conflictTerminals, 0)
        // b 里的 CLI 退出（会话在 b 结束），对话仍在 a 进行：b 不再显示它的标题。
        u = terminal("b", project: 1, created: 2, state: "ended", tool: "codex", conversation: 7)
        m.apply(["type": "terminal.upsert", "rev": 8, "terminal": u])
        XCTAssertEqual(task(build(m), "t:b")?.subtitle, "/w/p1")
        // 移除推送（end_reason=unrecoverable）：不进「最近任务」。
        m.apply(["type": "conversation.upsert", "rev": 9, "recent_total": 0,
                 "conversation": conversation(9, project: 1, ended: at(300), reason: "unrecoverable")])
        XCTAssertTrue(m.recent.isEmpty)
    }

    /// 从没提问过的对话（没有提示词也没有标题，如 Codex 不提问就退出时只发来的 SessionEnd）无法恢复，不进「最近任务」。
    func testEmptyConversationIsNotRecent() {
        let m = model()
        m.apply(["type": "snapshot", "rev": 1, "recent_total": 0, "projects": [], "terminals": [], "conversations": []])
        m.applyRecent(page([conversation(7, project: 1, ended: at(100), prompt: nil)]))
        XCTAssertTrue(m.recent.isEmpty, "查询结果里的也排除")
        m.apply(["type": "conversation.upsert", "rev": 2, "recent_total": 0,
                 "conversation": conversation(8, project: 1, ended: at(200), tool: "codex", prompt: nil)])
        XCTAssertTrue(m.recent.isEmpty, "推送来的也排除")
        m.apply(["type": "conversation.upsert", "rev": 3, "recent_total": 1,
                 "conversation": conversation(9, project: 1, ended: at(300), tool: "kimi", title: "你好", prompt: nil)])
        XCTAssertEqual(m.recent.map(\.id), [9], "只有标题（Kimi 的 Hook 标题）也算提问过")
    }

    /// 「最近任务」最多显示 recentLimit 条（2026-10-14 用户确认由 50 改为 20）。
    func testRecentIsCappedAtLimit() {
        let n = SidebarModel.recentLimit
        XCTAssertEqual(n, 20)
        let m = model()
        m.apply(["type": "snapshot", "rev": 1, "recent_total": 60, "projects": [], "terminals": [], "conversations": []])
        XCTAssertEqual(build(m).rows["sec:recent"], .section(title: "最近任务", count: n), "应答到达前按总数与上限显示")
        m.applyRecent(page((1...60).map { conversation(Int64($0), project: 1, ended: at(Int64($0))) }))
        XCTAssertEqual(m.recent.count, n)
        XCTAssertEqual(m.recent.first?.id, 60, "保留最新的 n 条")
        m.apply(["type": "conversation.upsert", "rev": 2, "recent_total": 61,
                 "conversation": conversation(61, project: 1, ended: at(61))])
        XCTAssertEqual(m.recent.count, n)
        XCTAssertEqual([m.recent.first?.id, m.recent.last?.id], [61, Int64(62 - n)])
        XCTAssertEqual(build(m).rows["sec:recent"], .section(title: "最近任务", count: n))
        // 比第 n 条还旧的推送不插入。
        m.apply(["type": "conversation.upsert", "rev": 3, "recent_total": 61,
                 "conversation": conversation(5, project: 1, ended: at(5), title: "改名")])
        XCTAssertEqual(m.recent.last?.id, Int64(62 - n))
        // 满 n 条时恢复一条：本地只剩 n − 1，后台第 n + 1 条本地没有，重取补齐。
        let fx = m.apply(["type": "conversation.upsert", "rev": 4, "recent_total": 60,
                          "conversation": conversation(61, project: 1)])
        XCTAssertEqual(fx, [.loadRecent(since: nowMs - 14 * day)])
        XCTAssertEqual(m.recent.count, n - 1)
        XCTAssertEqual(build(m).rows["sec:recent"], .section(title: "最近任务", count: n), "重取期间按总数显示")
    }

    func testReconnectKeepsListUntilFirstPage() {
        let m = model()
        m.apply(["type": "snapshot", "rev": 3, "recent_total": 1, "projects": [], "terminals": [], "conversations": []])
        m.applyRecent(page([conversation(1, project: 1, ended: at(100))]))
        let before = build(m)
        // 后台重启：rev 从头开始；首页应答到达前列表不变（不闪烁）。
        let fx = m.apply(["type": "snapshot", "rev": 1, "recent_total": 1, "projects": [], "terminals": [],
                          "conversations": []])
        XCTAssertEqual(fx, [.loadRecent(since: nowMs - 14 * day)])
        XCTAssertEqual(build(m).kids("pr:1"), before.kids("pr:1"))
        m.apply(["type": "terminal.upsert", "rev": 2, "terminal": terminal("x", project: 1)])
        XCTAssertEqual(m.terminals.count, 1, "新后台的推送按新基线接收")
        m.applyRecent(page([]))
        XCTAssertTrue(m.recent.isEmpty)
    }

    func testEmptyRecentShowsEmptyState() {
        let m = model()
        m.apply(["type": "snapshot", "rev": 1, "recent_total": 0, "projects": [], "terminals": [], "conversations": []])
        XCTAssertNil(build(m).rows["empty:recent"], "首页在途时不显示空态")
        m.applyRecent(page([]))
        XCTAssertEqual(build(m).kids("sec:recent"), ["empty:recent"])
    }

    /// 日期格式按界面语言取（M2 第 6 块）：中文与改动前相同，英文为月份缩写与美式日期。
    func testTimeText() {
        let f = TimeText(now: now, calendar: utc, locale: Locale(identifier: "zh-Hans"))
        XCTAssertEqual(f.format(nowMs - 3_600_000), "07:00")
        XCTAssertEqual(f.format(nowMs - 3 * 86_400_000), "1月12日")
        XCTAssertEqual(f.format(nowMs - 400 * 86_400_000), "2025/12/11")
        let e = TimeText(now: now, calendar: utc, locale: Locale(identifier: "en"))
        XCTAssertEqual(e.format(nowMs - 3_600_000), "07:00")
        XCTAssertEqual(e.format(nowMs - 3 * 86_400_000), "Jan 12")
        XCTAssertEqual(e.format(nowMs - 400 * 86_400_000), "12/11/2025")
    }

    // MARK: - 差分

    func testDiffOnHandPickedCase() {
        var a = SidebarTree()
        a.children[""] = ["s"]
        a.children["s"] = ["g1", "g2", "g3"]
        a.children["g1"] = ["x", "y"]
        a.children["g2"] = ["z"]
        a.children["g3"] = []
        for k in ["s", "g1", "g2", "g3"] { a.rows[k] = .section(title: k, count: 0) }
        for k in ["x", "y", "z"] { a.rows[k] = .empty(k) }
        var b = a
        b.children["s"] = ["g3", "g1", "g4"]
        b.children["g2"] = nil
        b.children["g1"] = ["y", "w", "x"]
        b.children["g4"] = ["v"]
        b.rows["g2"] = nil
        b.rows["z"] = nil
        b.rows["g4"] = .section(title: "g4", count: 0)
        b.rows["w"] = .empty("w")
        b.rows["v"] = .empty("v")
        b.rows["y"] = .empty("y2")
        let c = SidebarChanges.diff(from: a, to: b)
        XCTAssertEqual(c.ops, [
            .remove(parent: "s", index: 1, key: "g2"),
            .move(parent: "s", from: 1, to: 0, key: "g3"),
            .insert(parent: "s", index: 2, key: "g4"),
            .move(parent: "g1", from: 1, to: 0, key: "y"),
            .insert(parent: "g1", index: 1, key: "w"),
        ])
        XCTAssertEqual(c.updated, ["y"])
        XCTAssertEqual(c.inserted, ["g4"])
        XCTAssertNoThrow(try Mirror.check(old: a, new: b, changes: c))
    }

    /// 固定种子随机 1 万步：模拟后台推送、最近任务应答（延迟到达、与推送交错）、重连、保留上限清理与 50 条上限；
    /// 每一步把差分按顺序套到「镜像列表」上，必须与新树一致。
    func testRandomizedDiffAgainstMirror() throws {
        var rng = SplitMix64(seed: 20261010)
        let server = FakeServer(cap: 120)
        let m = model()
        var pending: [(SidebarModel.Effect)] = []
        func run(_ effects: [SidebarModel.Effect]) { pending += effects }
        run(m.apply(server.snapshot()))
        var shown = SidebarTree.empty
        var maxRows = 0
        for step in 0..<10_000 {
            let roll = rng.next(100)
            if roll < 70 {
                for msg in server.randomEvent(&rng) { run(m.apply(msg)) }
            } else if roll < 85, !pending.isEmpty {
                // 后台此刻处理请求：应答反映此刻的状态，随后才是之后的推送。
                let fx = pending.removeFirst()
                if case let .loadRecent(since) = fx {
                    m.applyRecent(server.recent(since: since, limit: SidebarModel.recentLimit))
                }
            } else if roll < 98 {
                pending.removeAll()
                run(m.apply(server.snapshot()))
            }
            let tree = build(m)
            let changes = SidebarChanges.diff(from: shown, to: tree)
            do {
                try Mirror.check(old: shown, new: tree, changes: changes)
            } catch {
                XCTFail("第 \(step) 步：\(error)")
                return
            }
            shown = tree
            maxRows = max(maxRows, tree.rows.count)
            if !m.loading {
                XCTAssertEqual(m.recent.map(\.id), server.endedSorted().prefix(SidebarModel.recentLimit).map(\.id),
                               "第 \(step) 步：最近任务应是后台最新的至多 50 条")
            }
            XCTAssertEqual(m.terminals.count, server.terminals.count)
        }
        XCTAssertGreaterThan(maxRows, 60, "随机过程应覆盖最近任务满 50 条的规模")
    }
}

// MARK: - 镜像列表

/// 按 NSOutlineView 的语义执行差分：下标按顺序生效；删除带走子树；新插入的节点在需要时才向数据源（新树）要子节点。
enum Mirror {
    struct Mismatch: Error, CustomStringConvertible { let description: String }

    static func check(old: SidebarTree, new: SidebarTree, changes: SidebarChanges) throws {
        var kids = old.children
        var rows = old.rows
        var freshKeys = Set<String>()
        func drop(_ k: String) {
            for c in kids[k] ?? [] { drop(c) }
            kids[k] = nil
            rows[k] = nil
        }
        for op in changes.ops {
            switch op {
            case let .remove(p, i, k):
                guard var list = kids[p], i < list.count, list[i] == k else {
                    throw Mismatch(description: "删除 \(k) 于 \(p)[\(i)]：当前为 \(kids[p] ?? [])")
                }
                list.remove(at: i)
                kids[p] = list
                drop(k)
            case let .insert(p, i, k):
                guard var list = kids[p], i <= list.count, !list.contains(k) else {
                    throw Mismatch(description: "插入 \(k) 于 \(p)[\(i)]：当前为 \(kids[p] ?? [])")
                }
                list.insert(k, at: i)
                kids[p] = list
                drop(k)
                freshKeys.insert(k)
            case let .move(p, from, to, k):
                guard var list = kids[p], from < list.count, list[from] == k, to < list.count else {
                    throw Mismatch(description: "移动 \(k) 于 \(p) \(from)→\(to)：当前为 \(kids[p] ?? [])")
                }
                list.remove(at: from)
                list.insert(k, at: to)
                kids[p] = list
            }
        }
        for k in changes.updated { rows[k] = new.rows[k] }
        // 新插入的节点及其子树按新树取；其余必须完全由差分得到。
        func walk(_ k: String, fresh: Bool) throws {
            let isFresh = fresh || freshKeys.contains(k)
            let got = isFresh ? new.kids(k) : (kids[k] ?? [])
            guard got == new.kids(k) else {
                throw Mismatch(description: "\(k) 的子节点 \(got) ≠ \(new.kids(k))")
            }
            if k != "", !isFresh, rows[k] != new.rows[k] {
                throw Mismatch(description: "\(k) 的行数据未更新")
            }
            for c in got { try walk(c, fresh: isFresh) }
        }
        try walk("", fresh: false)
    }
}

// MARK: - 随机后台

struct SplitMix64 {
    var state: UInt64
    init(seed: UInt64) { state = seed }
    mutating func next() -> UInt64 {
        state &+= 0x9E37_79B9_7F4A_7C15
        var z = state
        z = (z ^ (z >> 30)) &* 0xBF58_476D_1CE4_E5B9
        z = (z ^ (z >> 27)) &* 0x94D0_49BB_1331_11EB
        return z ^ (z >> 31)
    }
    mutating func next(_ n: Int) -> Int { Int(next() % UInt64(n)) }
}

/// 简化的 scyllauid：项目、存活终端、对话，按协议生成推送与 recent.query 应答。
final class FakeServer {
    struct Conv { var id: Int64; var project: Int64; var ended: Int64?; var title: String? }
    let cap: Int
    var rev: UInt64 = 0
    var clock: Int64 = nowMs - 1_000_000   // 都在 14 天显示窗口内
    var projects: [Int64: Int64] = [:]          // id → sort_order
    var terminals: [String: [String: Any]] = [:]
    var convs: [Int64: Conv] = [:]
    var nextConv: Int64 = 1
    var nextTerminal = 0

    init(cap: Int) { self.cap = cap }

    func endedSorted() -> [Conv] {
        convs.values.filter { $0.ended != nil }.sorted { ($0.ended!, $0.id) > ($1.ended!, $1.id) }
    }

    func json(_ c: Conv) -> [String: Any] {
        conversation(c.id, project: c.project, ended: c.ended, title: c.title)
    }

    func snapshot() -> [String: Any] {
        ["type": "snapshot", "rev": rev, "recent_total": endedSorted().count,
         "projects": projects.map { project($0.key, "p\($0.key)", order: $0.value) },
         "terminals": Array(terminals.values),
         "conversations": convs.values.filter { $0.ended == nil }.map(json)]
    }

    func recent(since: Int64, limit: Int) -> [String: Any] {
        var r = page(endedSorted().filter { $0.ended! >= since }.prefix(limit).map(json))
        r["recent_total"] = endedSorted().count
        return r
    }

    private func push(_ type: String, _ body: [String: Any]) -> [String: Any] {
        rev += 1
        return body.merging(["type": type, "rev": rev]) { a, _ in a }
    }

    private func pushConv(_ c: Conv) -> [String: Any] {
        push("conversation.upsert", ["conversation": json(c), "recent_total": endedSorted().count])
    }

    /// 保留上限：删最旧的已结束对话（不推送，由 recent_total 体现）。
    private func prune() {
        for c in endedSorted().dropFirst(cap) { convs[c.id] = nil }
    }

    func randomEvent(_ rng: inout SplitMix64) -> [[String: Any]] {
        clock += 1
        var roll = rng.next(10)
        if roll < 2 && terminals.count >= 40 { roll = 2 }   // 规模保持在真实范围（12 个并行会话的数倍）
        switch roll {
        case 0, 1:
            // 新终端（可能新项目）。
            let pid = Int64(rng.next(8) + 1)
            var out: [[String: Any]] = []
            if projects[pid] == nil {
                projects[pid] = Int64(rng.next(5))
                out.append(push("project.upsert", ["project": project(pid, "p\(pid)", order: projects[pid]!)]))
            }
            nextTerminal += 1
            let t = terminal("t\(nextTerminal)", project: pid, created: clock)
            terminals["t\(nextTerminal)"] = t
            return out + [push("terminal.upsert", ["terminal": t])]
        case 2:
            // 关闭终端。
            guard let id = terminals.keys.sorted().randomElement(using: &rng) else { return [] }
            var t = terminals.removeValue(forKey: id)!
            t["status"] = "closed"
            return [push("terminal.upsert", ["terminal": t])]
        case 3, 4:
            // 终端状态与「待查看」变化；约三成同时改归项目（跟随当前目录，M1 第 4 块），侧栏行跨父节点移动。
            guard let id = terminals.keys.sorted().randomElement(using: &rng) else { return [] }
            let states = ["ready", "running", "waiting", "error", "ended", "unknown"]
            var t = terminals[id]!
            if rng.next(10) < 3, let pid = projects.keys.sorted().randomElement(using: &rng) {
                t["project_id"] = pid
            }
            t["agent"] = ["state": states[rng.next(states.count)], "tool": ["claude", "codex", "kimi", "grok"][rng.next(4)]]
            t["unseen"] = rng.next(2) == 0
            terminals[id] = t
            return [push("terminal.upsert", ["terminal": t])]
        case 5, 6:
            // 新对话或结束一个对话。
            if let open = convs.values.filter({ $0.ended == nil }).sorted(by: { $0.id < $1.id }).randomElement(using: &rng),
               rng.next(2) == 0 {
                var c = open
                c.ended = clock
                convs[c.id] = c
                prune()
                return [pushConv(c)]
            }
            let pids = projects.keys.sorted()
            guard let pid = pids.randomElement(using: &rng) else { return [] }
            let c = Conv(id: nextConv, project: pid, ended: nil, title: nil)
            nextConv += 1
            convs[c.id] = c
            return [pushConv(c)]
        case 7:
            // 恢复一个已结束的对话（重新打开）。
            guard var c = endedSorted().randomElement(using: &rng) else { return [] }
            c.ended = nil
            convs[c.id] = c
            return [pushConv(c)]
        case 8:
            // 已结束对话的标题变化（可能在已加载窗口之外）。
            guard var c = endedSorted().randomElement(using: &rng) else { return [] }
            c.title = "T\(clock)"
            convs[c.id] = c
            return [pushConv(c)]
        default:
            // 一次结束一批（制造保留上限清理）。
            var out: [[String: Any]] = []
            let pids = projects.keys.sorted()
            guard !pids.isEmpty else { return [] }
            for _ in 0..<rng.next(6) {
                clock += 1
                let c = Conv(id: nextConv, project: pids[rng.next(pids.count)], ended: clock, title: nil)
                nextConv += 1
                convs[c.id] = c
                prune()
                out.append(pushConv(c))
            }
            return out
        }
    }
}

extension SplitMix64: RandomNumberGenerator {}
