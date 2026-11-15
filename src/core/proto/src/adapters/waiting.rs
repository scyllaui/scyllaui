//! 读会话记录补 Hook 的缺口（M2 第 1 块，docs/design.md 4.3 节「实现（M2 第 1 块）」）：CLI 在某些时刻不发 Hook 时，
//! 从会话记录新写入的行里认出发生了什么，按统一事件交给状态机。Claude 只在待审批期间看（拒绝审批后不发 Hook）；
//! Copilot 在运行中与待审批期间都看（批准、拒绝、Esc 中断都不发 Hook，M4b 块 9c）；Antigravity 同样都看，记录是
//! SQLite 对话库（等待审批也不发 Hook，M4b 块 9b）；Kimi 2.6.1 起一轮正常结束不发 Hook，轮次结束以记录为准
//! （2026-11-14，docs/design.md 3.1 节）。只含纯逻辑；监听文件、按行读取与查询对话库在 scyllauid 的
//! `waitwatch.rs`。格式均为本机实测，非官方承诺：认不出就什么也不做。

use crate::Kind;
use serde_json::Value;

/// 会话记录怎么读。
#[derive(Clone, Copy, Debug)]
pub enum Scan {
    /// JSON Lines，逐行交给这个函数判定（Claude、Copilot、Kimi、Grok）。
    Lines(LineScan),
    /// 整个文件是一个 JSON、被原地改写：每次有写入读出整份交给这个函数判定（Claude 的进程状态文件）。
    Whole(LineScan),
    /// Antigravity 的对话库 `~/.gemini/antigravity-cli/conversations/<ID>.db`：每次有写入查询一次（`AgyRead`），
    /// 交给 `antigravity` 判定。
    AntigravityDb,
}

/// 逐行判定：认出了就返回对应的统一事件（`TurnInterrupt` 回到 Ready，`PermissionDone` 回到运行中）。
pub type LineScan = fn(&mut WaitScan, &Value) -> Option<Kind>;

/// 判定时跨行、跨次保留的状态（Claude 的拒绝记录由两行组成，可能分两次写入；Antigravity 记着开始监听时的执行次数与
/// 上次读到时是否在等用户）。
#[derive(Default, Debug)]
pub struct WaitScan {
    /// 正在补读开始监听之前已写入的行（docs/design.md 4.3 节「会话记录监听」）：判定函数只记状态、不报事件，
    /// 补读完由 [`WaitScan::replayed`] 给出结论。
    pub replay: bool,
    /// 刚认出的事件所属的轮次（Kimi 的 `turnId`），由监听线程取走、随事件交给核心线程核对。
    pub turn: Option<String>,
    /// 监听的是哪个会话（监听线程填）：整份文件的判定据此排除别的会话。
    pub session: Option<String>,
    /// 正在判定的这份记录是子代理的（监听线程填：与主记录一起监听的子会话记录，或子代理审批指向的记录）。
    pub sub: bool,
    error_result: bool,
    /// Claude 进程状态文件：见到过「等审批」。
    claude_waiting: bool,
    /// Grok：还没处理完的审批数；补读时见过审批记录。
    grok_open: u32,
    grok_seen: bool,
    agy_runs: Option<u64>,
    agy_waiting: Option<bool>,
    /// Copilot：还没处理完的审批（`data.requestId`）；补读时见过审批记录。
    copilot_open: Vec<String>,
    copilot_seen: bool,
    /// Copilot：最近一条用户消息之后主会话被中断过（补读时记）。
    copilot_aborted: bool,
    /// Kimi：补读到的最后一条主代理轮次结束（事件，轮次）。
    kimi_end: Option<(Kind, String)>,
}

impl WaitScan {
    /// 补读完的结论。`turn` 为窗格当前的轮次，`waiting` 为窗格是否在待审批，`resync` 为换版本接管后的对账
    /// （此前可能漏了事件；平时开始监听时刚收到提问，记录里最后一轮可能还是上一轮，不能按它判断中断）。
    pub fn replayed(&mut self, turn: Option<&str>, waiting: bool, resync: bool) -> Option<Kind> {
        self.replay = false;
        if let Some((kind, t)) = self.kimi_end.take() {
            // 只认当前轮次的结束；不知道当前轮次时不认。
            if turn == Some(t.as_str()) {
                self.turn = Some(t);
                return Some(kind);
            }
        }
        if resync && self.copilot_aborted {
            return Some(Kind::TurnInterrupt);
        }
        if resync && waiting && self.copilot_seen && self.copilot_open.is_empty() {
            return Some(Kind::PermissionDone);
        }
        if resync && waiting && self.grok_seen && self.grok_open == 0 {
            return Some(Kind::PermissionDone);
        }
        None
    }
}

/// Antigravity 对话库的一次读取（scyllauid 查询）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgyRead {
    /// `executor_metadata` 的行数：每次执行（一轮）结束时 agy 写一行（2026-11-01 实测 1.4.12，Stop Hook 之后约 130 ms）。
    pub runs: u64,
    /// 最后一行的结束方式（`agy_run` 解出的字段 1）：正常结束为 4，拒绝审批与 Esc 中断为 2。
    pub last_end: Option<u64>,
    /// 本次执行的步骤（序号大于最后一行记的末步）里有状态为「等待用户」（`AGY_STEP_WAITING`）的：命令审批、AI 提问都是。
    pub waiting: bool,
}

/// 对话库 `steps.status` 中「等待用户」的取值（2026-11-01 实测 1.4.12：命令审批与 `ask_question` 都是 9；生成中 8、
/// 执行中 2、完成 3、审批里按 Esc 6、拒绝 7）。
pub const AGY_STEP_WAITING: i64 = 9;
/// `executor_metadata` 字段 1 中「正常结束」的取值（同上实测：正常结束 4，取消 2）。
const AGY_RUN_DONE: u64 = 4;

/// `executor_metadata.data`（protobuf）顶层的字段 1（结束方式）与字段 3（这次执行的末步序号）；只解顶层的变长整数，
/// 其余字段按线格式跳过，格式不对返回已解出的部分。
pub fn agy_run(data: &[u8]) -> (Option<u64>, Option<u64>) {
    fn varint(b: &[u8], i: &mut usize) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let c = *b.get(*i)?;
            *i += 1;
            v |= u64::from(c & 0x7f) << shift;
            if c < 0x80 {
                return Some(v);
            }
        }
        None
    }
    let (mut end, mut last) = (None, None);
    let mut i = 0;
    while i < data.len() {
        let Some(key) = varint(data, &mut i) else { break };
        let value = match key & 7 {
            0 => varint(data, &mut i),
            1 => {
                i += 8;
                None
            }
            2 => {
                let Some(n) = varint(data, &mut i) else { break };
                i = i.saturating_add(n as usize);
                None
            }
            5 => {
                i += 4;
                None
            }
            _ => break,
        };
        match (key >> 3, value) {
            (1, Some(v)) => end = Some(v),
            (3, Some(v)) => last = Some(v),
            _ => {}
        }
    }
    (end, last)
}

/// Antigravity：开始监听后执行次数变多即本轮结束（正常结束本应先收到 Stop Hook，此时状态已不在运行中，核心线程不理会；
/// Hook 丢了才靠这里），结束方式为正常结束时按一轮结束，否则按中断（拒绝审批、Esc）；「等待用户」出现即需确认、
/// 消失即审批已处理。第一次读只记下执行次数并报告当时是否在等用户（与当前状态一致时核心线程不理会）。
pub fn antigravity(scan: &mut WaitScan, read: &AgyRead) -> Option<Kind> {
    let base = *scan.agy_runs.get_or_insert(read.runs);
    if read.runs > base {
        scan.agy_runs = Some(read.runs);
        scan.agy_waiting = Some(false);
        return Some(if read.last_end == Some(AGY_RUN_DONE) { Kind::TurnEnd } else { Kind::TurnInterrupt });
    }
    if scan.agy_waiting == Some(read.waiting) {
        return None;
    }
    scan.agy_waiting = Some(read.waiting);
    Some(if read.waiting { Kind::PermissionWait } else { Kind::PermissionDone })
}

/// Claude Code：认出拒绝即按中断处理。开始监听之前的记录不看。
pub fn claude(scan: &mut WaitScan, line: &Value) -> Option<Kind> {
    if scan.replay {
        return None;
    }
    claude_rejected(scan, line).then_some(Kind::TurnInterrupt)
}

/// Claude Code 的进程状态文件 `~/.claude/sessions/<pid>.json`（2026-11-14 实测 2.1.325，格式非官方承诺）：审批框打开时
/// 写 `status: "waiting"`、`waitingFor: "permission prompt"`，批准后 11—15 ms 写回 `"busy"`（命令还在跑），拒绝后直接
/// 写 `"idle"`。主代理与子代理的审批框共用这一个状态，文件原地改写。只认「先见到等审批、再离开」：开始监听时文件
/// 可能还没写成等待。其他对话框（AskUserQuestion 为 `"input needed"`）不算。
pub fn claude_status(scan: &mut WaitScan, file: &Value) -> Option<Kind> {
    if scan.session.is_some() && file["sessionId"].as_str() != scan.session.as_deref() {
        return None;
    }
    let status = file["status"].as_str()?;
    if status == "waiting" {
        scan.claude_waiting = file["waitingFor"] == "permission prompt";
        return None;
    }
    if !std::mem::take(&mut scan.claude_waiting) || scan.replay {
        return None;
    }
    match status {
        "busy" => Some(Kind::PermissionDone),
        "idle" => Some(Kind::TurnInterrupt),
        _ => None,
    }
}

/// Grok Build 会话记录 `events.jsonl`（2026-11-14 实测 1.0.77，格式非官方承诺）：审批开始写 `permission_requested`，
/// 用户决定后 3—5 ms 写 `permission_resolved`（`decision` 为 `allow` 或 `deny`），没有配对的 ID，按个数配对。
/// 子代理的命令审批在主代理等它输出时也写进主会话的记录，否则只写在子会话自己的记录里（子会话的记录一起监听）。
/// 批准且没有别的审批在等时算审批已处理。拒绝：主会话的由随后的 StopCancelled Hook 处理（拒绝后也会写一行
/// `phase_changed`，不能按它判断）；子代理的拒绝只取消子代理、主会话收不到任何事件，这里按审批已处理报。
pub fn grok(scan: &mut WaitScan, line: &Value) -> Option<Kind> {
    match line["type"].as_str()? {
        "permission_requested" => {
            scan.grok_seen = true;
            scan.grok_open += 1;
            None
        }
        "permission_resolved" => {
            scan.grok_seen = true;
            scan.grok_open = scan.grok_open.saturating_sub(1);
            let done = line["decision"] == "allow" || scan.sub;
            (done && scan.grok_open == 0 && !scan.replay).then_some(Kind::PermissionDone)
        }
        _ => None,
    }
}

/// Codex 的 rollout 记录（2026-11-14 实测 0.169.0）：拒绝审批时立刻写
/// `{"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}`。拒绝主代理的审批另有 Interrupt Hook；
/// 拒绝子代理的审批不发任何 Hook，主线程继续等它，只有子线程的记录里有这一行——子代理的按审批已处理报（主会话回到
/// 等待之前的状态），主代理的按中断报。开始监听之前的记录不看。
pub fn codex(scan: &mut WaitScan, line: &Value) -> Option<Kind> {
    if scan.replay || line["type"] != "event_msg" || line["payload"]["type"] != "turn_aborted" {
        return None;
    }
    Some(if scan.sub { Kind::PermissionDone } else { Kind::TurnInterrupt })
}

/// Kimi Code `agents/main/wire.jsonl`（2026-11-14 核对 2.6.1 包内代码与本机 169 份记录）：每轮结束写
/// `{"type":"turn.ended","agentId":"main","turnId":N,"reason":"completed|cancelled|failed"}`；子代理的写在它自己的
/// 记录里。`completed` 按一轮结束，`cancelled` 按中断，`failed` 按出错；轮次号交给核心线程核对。
pub fn kimi(scan: &mut WaitScan, line: &Value) -> Option<Kind> {
    if line["type"] != "turn.ended" || line["agentId"] != "main" {
        return None;
    }
    let kind = match line["reason"].as_str()? {
        "completed" => Kind::TurnEnd,
        "cancelled" => Kind::TurnInterrupt,
        "failed" => Kind::TurnFail,
        _ => return None,
    };
    let turn = match &line["turnId"] {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => return None,
    };
    if scan.replay {
        scan.kimi_end = Some((kind, turn));
        return None;
    }
    scan.turn = Some(turn);
    Some(kind)
}

/// GitHub Copilot CLI `~/.copilot/session-state/<会话>/events.jsonl`（2026-10-30 实测 1.0.105；2026-11-14 按本机
/// 1.0.105、1.0.106 的记录补全）：审批开始写 `permission.requested`、结束写 `permission.completed`，两者靠
/// `data.requestId` 配对；子代理的记录在顶层带 `agentId`。这些都不发 Hook，审批通知 Hook 也分不出主、子代理，所以
/// 主、子代理的审批都在这里处理：还有没处理完的审批时不报「审批已处理」（几个审批并存时不提前离开「需确认」）。
/// `data.result.kind`：`approved` 批准（之后要等命令跑完才有 postToolUse）；`denied-interactively-by-user` 拒绝，
/// 带 `feedback` 时 Copilot 照用户的话继续这一轮，不带时主代理随后写 `abort`、子代理结束而主代理继续；`cancelled`
/// 为审批框里按 Esc，本轮随之中止。运行中按两次 Esc 写 `abort`。顶层带 `agentId` 的 `abort` 是子代理的，不算主会话
/// 中断（2026-11-13 长跑里它让侧栏闪成 Ready）。其他取值认不出，不改状态。
pub fn copilot(scan: &mut WaitScan, line: &Value) -> Option<Kind> {
    let sub = line.get("agentId").is_some_and(|a| !a.is_null());
    let id = || line["data"]["requestId"].as_str().map(str::to_owned);
    let kind = match line["type"].as_str()? {
        "user.message" if !sub => {
            scan.copilot_aborted = false;
            None
        }
        "permission.requested" => {
            scan.copilot_seen = true;
            if let Some(id) = id().filter(|id| !scan.copilot_open.contains(id)) {
                scan.copilot_open.push(id);
            }
            None
        }
        "permission.completed" => {
            scan.copilot_seen = true;
            let result = &line["data"]["result"];
            let done = match result["kind"].as_str()? {
                "approved" => Some(Kind::PermissionDone),
                "cancelled" if !sub => Some(Kind::TurnInterrupt),
                "cancelled" => Some(Kind::PermissionDone),
                "denied-interactively-by-user" if sub || result["feedback"].as_str().is_some_and(|f| !f.is_empty()) => {
                    Some(Kind::PermissionDone)
                }
                // 主代理不带理由的拒绝：等随后的 `abort`。
                "denied-interactively-by-user" => None,
                _ => return None,
            };
            if let Some(id) = id() {
                scan.copilot_open.retain(|x| *x != id);
            }
            match done {
                Some(Kind::PermissionDone) if !scan.copilot_open.is_empty() => None,
                Some(Kind::TurnInterrupt) => {
                    scan.copilot_open.clear();
                    Some(Kind::TurnInterrupt)
                }
                d => d,
            }
        }
        "abort" if !sub => {
            scan.copilot_open.clear();
            Some(Kind::TurnInterrupt)
        }
        _ => None,
    };
    if scan.replay {
        if kind == Some(Kind::TurnInterrupt) {
            scan.copilot_aborted = true;
        }
        return None;
    }
    kind
}

/// 用户在审批中拒绝的标记文本（Claude Code 2.1.292、2.1.301 实测）。
const CLAUDE_REJECTED: &str = "[Request interrupted by user for tool use]";

/// Claude Code：拒绝后依次写入「`is_error: true` 的 `tool_result`」与内容为 [`CLAUDE_REJECTED`] 的用户记录
/// （2026-10-16 实测 2.1.301，S0 时 2.1.292 相同）。两行之间出现 AI 的记录说明 Claude 带着结果继续了本轮
/// （如批准后工具出错），重新开始判定。返回 true 表示认出了拒绝。
pub fn claude_rejected(scan: &mut WaitScan, line: &Value) -> bool {
    match line["type"].as_str() {
        Some("assistant") => {
            scan.error_result = false;
            false
        }
        Some("user") => {
            let content = &line["message"]["content"];
            let texts: Vec<&str> = match content {
                Value::String(s) => vec![s.as_str()],
                Value::Array(parts) => {
                    if parts.iter().any(|p| p["type"] == "tool_result" && p["is_error"] == true) {
                        scan.error_result = true;
                        return false;
                    }
                    parts.iter().filter(|p| p["type"] == "text").filter_map(|p| p["text"].as_str()).collect()
                }
                _ => vec![],
            };
            scan.error_result && texts.iter().any(|t| t.trim() == CLAUDE_REJECTED)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    impl WaitScan {
        fn clone_replayed(&self, turn: Option<&str>, waiting: bool, resync: bool) -> Option<Kind> {
            let mut c = WaitScan {
                copilot_open: self.copilot_open.clone(),
                copilot_seen: self.copilot_seen,
                copilot_aborted: self.copilot_aborted,
                kimi_end: self.kimi_end.clone(),
                ..Default::default()
            };
            c.replayed(turn, waiting, resync)
        }
    }

    fn run(lines: &[Value]) -> bool {
        let mut s = WaitScan::default();
        lines.iter().any(|l| claude_rejected(&mut s, l))
    }

    fn error_result() -> Value {
        json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "t1", "is_error": true,
            "content": "The user doesn't want to proceed with this tool use."}]}})
    }

    fn marker() -> Value {
        json!({"type": "user", "message": {"content": [{"type": "text", "text": CLAUDE_REJECTED}]}})
    }

    /// 2.1.301 实测的拒绝序列（结构取自本机会话记录，正文已删）。
    #[test]
    fn rejection_sequence_is_recognized() {
        assert!(run(&[error_result(), marker(), json!({"type": "system", "subtype": "turn_duration"})]));
        // 内容为字符串的写法同样认。
        assert!(run(&[error_result(), json!({"type": "user", "message": {"content": CLAUDE_REJECTED}})]));
    }

    /// Copilot 1.0.105 实测的三种记录（结构取自本机会话记录）。
    #[test]
    fn copilot_record_lines() {
        let mut s = WaitScan::default();
        let done = |kind: &str| json!({"type": "permission.completed", "data": {"toolCallId": "c1", "result": {"kind": kind}}});
        assert_eq!(copilot(&mut s, &done("approved")), Some(Kind::PermissionDone));
        assert_eq!(copilot(&mut s, &done("cancelled")), Some(Kind::TurnInterrupt));
        assert_eq!(copilot(&mut s, &done("something-new")), None, "认不出的取值不改状态");
        assert_eq!(copilot(&mut s, &json!({"type": "abort", "data": {"reason": "user_abort"}})), Some(Kind::TurnInterrupt));
        assert_eq!(copilot(&mut s, &json!({"type": "abort", "data": {"reason": "user_initiated"}, "agentId": "a1"})), None, "子代理的中止不算主会话中断");
        for t in ["assistant.turn_end", "tool.execution_start", "permission.requested", "hook.start"] {
            assert_eq!(copilot(&mut s, &json!({"type": t, "data": {}})), None, "{t}");
        }
        assert_eq!(claude(&mut s, &json!({"type": "assistant"})), None);
    }

    /// Copilot 的审批按 requestId 配对（2026-11-14，结构取自本机 1.0.106 的记录）：两个审批并存时全部处理完才报；
    /// 拒绝带理由、子代理的拒绝按已处理，主代理不带理由的拒绝等 abort。
    #[test]
    fn copilot_requests_are_paired() {
        let req = |id: &str, agent: Option<&str>| json!({"type": "permission.requested", "data": {"requestId": id}, "agentId": agent});
        let done = |id: &str, result: Value, agent: Option<&str>| json!({"type": "permission.completed", "data": {"requestId": id, "result": result}, "agentId": agent});
        let mut s = WaitScan::default();
        assert_eq!(copilot(&mut s, &req("r1", Some("a1"))), None);
        assert_eq!(copilot(&mut s, &req("r2", Some("a2"))), None);
        assert_eq!(copilot(&mut s, &done("r1", json!({"kind": "approved"}), Some("a1"))), None, "还有一个在等");
        assert_eq!(copilot(&mut s, &done("r2", json!({"kind": "approved"}), Some("a2"))), Some(Kind::PermissionDone));
        let denied = |feedback: Option<&str>| json!({"kind": "denied-interactively-by-user", "feedback": feedback});
        assert_eq!(copilot(&mut s, &req("r3", None)), None);
        assert_eq!(copilot(&mut s, &done("r3", denied(Some("do it differently")), None)), Some(Kind::PermissionDone), "带理由：继续这一轮");
        assert_eq!(copilot(&mut s, &req("r4", None)), None);
        assert_eq!(copilot(&mut s, &done("r4", denied(None), None)), None, "主代理不带理由：等 abort");
        assert_eq!(copilot(&mut s, &json!({"type": "abort", "data": {"reason": "user_initiated"}})), Some(Kind::TurnInterrupt));
        assert_eq!(copilot(&mut s, &req("r5", Some("a1"))), None);
        assert_eq!(copilot(&mut s, &done("r5", denied(None), Some("a1"))), Some(Kind::PermissionDone), "子代理被拒，主代理继续");
    }

    /// 补读（开始监听之前的行）只记状态：平时不据此报事件；换版本接管后的对账才报空档里漏掉的批准与中断。
    #[test]
    fn copilot_replay() {
        let req = json!({"type": "permission.requested", "data": {"requestId": "r1"}});
        let done = json!({"type": "permission.completed", "data": {"requestId": "r1", "result": {"kind": "approved"}}});
        let mut s = WaitScan { replay: true, ..Default::default() };
        assert_eq!(copilot(&mut s, &req), None);
        assert_eq!(s.replayed(None, true, true), None, "还在等");
        assert_eq!(copilot(&mut s, &done), Some(Kind::PermissionDone), "补读后照常判定");
        let mut s = WaitScan { replay: true, ..Default::default() };
        copilot(&mut s, &req);
        assert_eq!(copilot(&mut s, &done), None);
        assert_eq!(s.clone_replayed(None, true, false), None, "平时开始监听不按旧记录报");
        assert_eq!(s.replayed(None, true, true), Some(Kind::PermissionDone), "接管空档里批准了");
        let mut s = WaitScan { replay: true, ..Default::default() };
        copilot(&mut s, &json!({"type": "abort", "data": {}}));
        copilot(&mut s, &json!({"type": "user.message", "data": {}}));
        assert_eq!(s.replayed(None, false, true), None, "中断在最近一次提问之前");
        let mut s = WaitScan { replay: true, ..Default::default() };
        copilot(&mut s, &json!({"type": "user.message", "data": {}}));
        copilot(&mut s, &json!({"type": "abort", "data": {}}));
        assert_eq!(s.replayed(None, false, true), Some(Kind::TurnInterrupt));
    }

    /// Claude 2.1.325 进程状态文件的实测序列：等审批 → 批准（busy）；等审批 → 拒绝（idle）；别的对话框与别的会话不算；
    /// 开始监听时已经是 busy 的不算批准。
    #[test]
    fn claude_status_file() {
        let f = |status: &str, wf: Option<&str>| json!({"pid": 1, "sessionId": "s1", "status": status, "waitingFor": wf});
        let mut s = WaitScan { session: Some("s1".into()), ..Default::default() };
        assert_eq!(claude_status(&mut s, &f("busy", None)), None, "没见过等审批");
        assert_eq!(claude_status(&mut s, &f("waiting", Some("permission prompt"))), None);
        assert_eq!(claude_status(&mut s, &f("busy", None)), Some(Kind::PermissionDone));
        assert_eq!(claude_status(&mut s, &f("busy", None)), None);
        assert_eq!(claude_status(&mut s, &f("waiting", Some("permission prompt"))), None);
        assert_eq!(claude_status(&mut s, &f("idle", None)), Some(Kind::TurnInterrupt), "拒绝");
        assert_eq!(claude_status(&mut s, &f("waiting", Some("input needed"))), None);
        assert_eq!(claude_status(&mut s, &f("busy", None)), None, "AskUserQuestion 不是审批");
        let mut other = WaitScan { session: Some("s2".into()), ..Default::default() };
        claude_status(&mut other, &f("waiting", Some("permission prompt")));
        assert_eq!(claude_status(&mut other, &f("busy", None)), None, "别的会话的文件");
        let mut s = WaitScan { session: Some("s1".into()), replay: true, ..Default::default() };
        assert_eq!(claude_status(&mut s, &f("waiting", Some("permission prompt"))), None);
        assert_eq!(s.replayed(None, true, false), None);
        assert_eq!(claude_status(&mut s, &f("busy", None)), Some(Kind::PermissionDone), "开始时读到等待，之后批准");
    }

    /// Grok 1.0.77 的审批记录（原始行取自实测）：批准回到运行中，两个并存时全部处理完才报，拒绝不报。
    #[test]
    fn grok_permission_events() {
        let req = json!({"ts": "2026-11-14T02:45:20.710Z", "type": "permission_requested", "tool_name": "run_terminal_command"});
        let res = |d: &str| json!({"ts": "2026-11-14T02:45:33.984Z", "type": "permission_resolved", "tool_name": "run_terminal_command", "decision": d, "wait_ms": 13274});
        let mut s = WaitScan::default();
        assert_eq!(grok(&mut s, &req), None);
        assert_eq!(grok(&mut s, &json!({"type": "phase_changed", "phase": "permission_prompt"})), None);
        assert_eq!(grok(&mut s, &res("allow")), Some(Kind::PermissionDone));
        assert_eq!(grok(&mut s, &req), None);
        assert_eq!(grok(&mut s, &req), None);
        assert_eq!(grok(&mut s, &res("allow")), None, "还有一个在等");
        assert_eq!(grok(&mut s, &res("allow")), Some(Kind::PermissionDone));
        assert_eq!(grok(&mut s, &req), None);
        assert_eq!(grok(&mut s, &res("deny")), None, "拒绝由 StopCancelled Hook 处理");
        // 子会话的记录：拒绝只取消子代理，主会话收不到事件，按审批已处理。
        let mut c = WaitScan { sub: true, ..Default::default() };
        grok(&mut c, &req);
        assert_eq!(grok(&mut c, &res("deny")), Some(Kind::PermissionDone));
        // Codex：拒绝时记录里的 turn_aborted，子线程的按已处理、主线程的按中断。
        let aborted = json!({"timestamp": "2026-11-14T04:59:35.911Z", "type": "event_msg", "payload": {"type": "turn_aborted", "reason": "interrupted"}});
        assert_eq!(codex(&mut WaitScan { sub: true, ..Default::default() }, &aborted), Some(Kind::PermissionDone));
        assert_eq!(codex(&mut WaitScan::default(), &aborted), Some(Kind::TurnInterrupt));
        assert_eq!(codex(&mut WaitScan::default(), &json!({"type": "event_msg", "payload": {"type": "task_complete"}})), None);
        assert_eq!(codex(&mut WaitScan { replay: true, ..Default::default() }, &aborted), None);
        assert_eq!(grok(&mut s, &json!({"type": "phase_changed", "phase": "tool_execution"})), None);
        // 待审批时才开始监听：补读到开始的那一行，之后的批准照常报。
        let mut s = WaitScan { replay: true, ..Default::default() };
        grok(&mut s, &req);
        assert_eq!(s.replayed(None, true, false), None);
        assert_eq!(grok(&mut s, &res("allow")), Some(Kind::PermissionDone));
    }

    /// Kimi 2.6.1 的轮次结束记录（结构取自本机记录）：只认主代理的；补读到的只在轮次对得上时才算。
    #[test]
    fn kimi_turn_ended() {
        let end = |agent: &str, turn: u64, reason: &str| json!({"type": "turn.ended", "agentId": agent, "turnId": turn, "reason": reason, "durationMs": 1});
        let mut s = WaitScan::default();
        assert_eq!(kimi(&mut s, &end("main", 3, "completed")), Some(Kind::TurnEnd));
        assert_eq!(s.turn.take().as_deref(), Some("3"));
        assert_eq!(kimi(&mut s, &end("main", 4, "cancelled")), Some(Kind::TurnInterrupt));
        assert_eq!(kimi(&mut s, &end("main", 5, "failed")), Some(Kind::TurnFail));
        assert_eq!(kimi(&mut s, &end("agent-0", 0, "completed")), None, "子代理的不算");
        assert_eq!(kimi(&mut s, &end("main", 6, "blocked")), None);
        assert_eq!(kimi(&mut s, &json!({"type": "agent.turn.ended", "turnId": 6, "outcome": "done"})), None);
        let mut s = WaitScan { replay: true, ..Default::default() };
        assert_eq!(kimi(&mut s, &end("main", 1, "completed")), None);
        assert_eq!(kimi(&mut s, &end("main", 2, "completed")), None);
        assert_eq!(s.clone_replayed(Some("3"), false, false), None, "上一轮的结束不算");
        assert_eq!(s.clone_replayed(None, false, false), None, "不知道当前轮次时不认");
        assert_eq!(s.replayed(Some("2"), false, false), Some(Kind::TurnEnd));
        assert_eq!(s.turn.as_deref(), Some("2"));
    }

    /// Antigravity 1.4.12 实测的对话库变化（2026-11-01）：审批出现 → 批准 → 正常结束；审批出现 → 拒绝；生成中 Esc。
    #[test]
    fn antigravity_db_reads() {
        let r = |runs, last_end, waiting| AgyRead { runs, last_end, waiting };
        let mut s = WaitScan::default();
        assert_eq!(antigravity(&mut s, &r(12, Some(4), false)), Some(Kind::PermissionDone), "第一次读报告当时不在等");
        assert_eq!(antigravity(&mut s, &r(12, Some(4), false)), None);
        assert_eq!(antigravity(&mut s, &r(12, Some(4), true)), Some(Kind::PermissionWait));
        assert_eq!(antigravity(&mut s, &r(12, Some(4), true)), None, "同一个等待只报一次");
        assert_eq!(antigravity(&mut s, &r(12, Some(4), false)), Some(Kind::PermissionDone));
        assert_eq!(antigravity(&mut s, &r(13, Some(4), false)), Some(Kind::TurnEnd), "Stop Hook 丢了时补上一轮结束");
        let mut s = WaitScan::default();
        assert_eq!(antigravity(&mut s, &r(3, Some(2), true)), Some(Kind::PermissionWait), "开始监听时已在等");
        assert_eq!(antigravity(&mut s, &r(4, Some(2), false)), Some(Kind::TurnInterrupt), "拒绝审批");
        let mut s = WaitScan::default();
        antigravity(&mut s, &r(0, None, false));
        assert_eq!(antigravity(&mut s, &r(1, None, false)), Some(Kind::TurnInterrupt), "解不出结束方式按中断");
    }

    /// 执行记录的 protobuf：字段 1、3 是变长整数，其余字段跳过（结构同本机记录：1:2 2:1 3:46 9:<UUID> 10:{…}）。
    #[test]
    fn antigravity_run_fields() {
        let mut b = vec![0x08, 0x02, 0x10, 0x01, 0x18, 46, 0x4a, 3, b'a', b'b', b'c', 0x52, 2, 0x08, 0x01];
        assert_eq!(agy_run(&b), (Some(2), Some(46)));
        b[1] = 0x04;
        assert_eq!(agy_run(&b), (Some(4), Some(46)));
        assert_eq!(agy_run(&[0x18, 0x96, 0x01]), (None, Some(150)), "多字节变长整数");
        assert_eq!(agy_run(&[0x08]), (None, None), "截断");
        assert_eq!(agy_run(&[]), (None, None));
    }

    /// 只有出错的结果（批准后工具失败）、只有标记、中间夹着 AI 记录、成功的结果，都不算拒绝。
    #[test]
    fn other_sequences_are_not_rejection() {
        assert!(!run(&[error_result()]));
        assert!(!run(&[marker()]));
        assert!(!run(&[error_result(), json!({"type": "assistant", "message": {"content": []}}), marker()]));
        let ok = json!({"type": "user", "message": {"content": [{"type": "tool_result", "is_error": false}]}});
        assert!(!run(&[ok, marker()]));
        assert!(!run(&[json!({"type": "permission-mode"}), json!("x"), json!(null)]));
    }
}
