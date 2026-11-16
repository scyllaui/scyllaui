//! 历史提示词的读取线程（docs/design.md 10.5 节决策 10，docs/protocol.md 第 4 节 `prompts.list`）：按需读 CLI 自己的
//! 会话记录，读完把整条应答交回核心线程发给请求方。不落库、不缓存；显示前脱敏。读文件不占核心线程：大的 Claude 会话记录
//! 有几十 MB。

use crate::core::Cmd;
use crate::title::{kimi_session_dir, tail};
use scyllaui_proto::adapters::prompts::{opencode_prompt, opencode_shell, opencode_v2_prompt, user_prompts, Prompt};
use scyllaui_proto::adapters::reply::ReplySource;
use scyllaui_proto::adapters::{adapter, valid_session_id};
use scyllaui_proto::Tool;
use scyllaui_proto::redact::redact;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::sync::mpsc::{self, Sender};

/// 每个会话记录最多读末尾这么多字节；更早的部分不读，应答标 `truncated`。
const MAX_BYTES: u64 = 64 << 20;
/// 一次最多返回这么多条（最新的），更早的不返回，同样标 `truncated`。
const MAX_ITEMS: usize = 500;

/// 一段对话：恢复链上的一个会话（docs/design.md 10.5 节决策 10：经「最近任务」恢复过的按会话分段合并）。
pub struct Segment {
    pub conversation_id: i64,
    pub tool: Tool,
    pub native_id: String,
    pub transcript_path: Option<String>,
    /// 库里记到过这段的提示词（`last_prompt` 非空）。没记到时会话记录还没生成（如 Claude 新会话还没提问）不算「记录已不在」。
    pub prompted: bool,
}

/// 一次 `prompts.list`：应答发回 `conn`，`segments` 由旧到新；`home` 为 CLI 配置所在的家目录（Kimi 的会话索引在其下）。
pub struct PromptJob {
    pub conn: u64,
    pub req_id: Value,
    pub home: PathBuf,
    pub segments: Vec<Segment>,
}

pub struct PromptWorker {
    tx: Sender<PromptJob>,
}

impl PromptWorker {
    pub fn new(core: Sender<Cmd>) -> PromptWorker {
        let (tx, rx) = mpsc::channel::<PromptJob>();
        std::thread::spawn(move || {
            for job in rx {
                let reply = crate::guarded("读历史提示词", || read(&job)).unwrap_or_else(|| {
                    json!({"type": "reply", "id": job.req_id, "ok": false,
                           "error": {"code": "internal", "message": "读会话记录出错"}})
                });
                if core.send(Cmd::PromptsListed { conn: job.conn, reply }).is_err() {
                    return;
                }
            }
        });
        PromptWorker { tx }
    }

    pub fn request(&self, job: PromptJob) {
        let _ = self.tx.send(job);
    }
}

/// 读出各段的提示词，组成应答：`segments` 每段是否读到会话记录，`items` 由旧到新（每条带所属对话）。还没提问过的段
/// 读不到记录也标 `available`（界面写「这段对话还没有提示词」，docs/ui-methodology.md 1.7 节），只有提问过的才算记录已不在。
pub fn read(job: &PromptJob) -> Value {
    let mut truncated = false;
    let mut segments = Vec::new();
    let mut items = Vec::new();
    for s in &job.segments {
        let a = adapter(s.tool);
        let source = a.reply_source;
        // `supported` 为 false：这款工具不支持历史提示词（不读它的记录），界面写明「暂不支持」，不说成记录已不在。
        let supported = source.has_prompts();
        if source == ReplySource::OpenCodeDb {
            // 查询出错（库被占用太久、表结构变了）标 `error`，界面说读取失败，不说成记录已不在。
            let (list, error) = match opencode_prompts(&job.home, &s.native_id) {
                Ok(list) => (list, false),
                Err(e) => {
                    log!("读历史提示词：OpenCode 库查询失败（{}）：{e}", s.native_id);
                    (None, true)
                }
            };
            segments.push(json!({"conversation_id": s.conversation_id, "tool": s.tool.as_str(), "available": list.is_some() || !s.prompted,
                                 "supported": true, "error": error, "native_id": s.native_id}));
            for p in list.unwrap_or_default() {
                items.push(json!({"conversation_id": s.conversation_id, "text": redact(&p.text), "at": p.at}));
            }
            continue;
        }
        // 记录位置：Copilot 按会话 ID 推出（适配器的 `session_record`；Antigravity 的那个是对话库，历史提示词读 Hook 给出的会话记录），
        // Kimi 查会话索引，其余用 Hook 给出的路径。
        let path = match (a.session_record, source) {
            _ if !supported => None,
            (Some(rel), ReplySource::CopilotEvents) if valid_session_id(&s.native_id) => Some(job.home.join(rel(&s.native_id))),
            (_, ReplySource::KimiWire) => kimi_session_dir(&job.home, &s.native_id).map(|d| d.join("agents/main/wire.jsonl")),
            _ => s.transcript_path.as_deref().map(PathBuf::from),
        };
        let text = path.as_deref().and_then(|p| {
            truncated |= fs::metadata(p).is_ok_and(|m| m.len() > MAX_BYTES);
            tail(p, MAX_BYTES)
        });
        segments.push(json!({"conversation_id": s.conversation_id, "tool": s.tool.as_str(),
                             "available": text.is_some() || !s.prompted, "supported": supported, "native_id": s.native_id}));
        for p in user_prompts(source, text.as_deref().unwrap_or("")) {
            items.push(json!({"conversation_id": s.conversation_id, "text": redact(&p.text), "at": p.at}));
        }
    }
    if items.len() > MAX_ITEMS {
        items.drain(..items.len() - MAX_ITEMS);
        truncated = true;
    }
    json!({"type": "reply", "id": job.req_id, "ok": true, "segments": segments, "items": items, "truncated": truncated})
}

/// OpenCode 库的位置（官方默认的数据目录，相对 CLI 家目录）。
const OPENCODE_DB: &str = ".local/share/opencode/opencode.db";

/// 只读打开 OpenCode 库（OpenCode 正在写时最多等 500 ms）；库不在为 Ok(None)。历史提示词与读回复、用量（`title.rs`）共用。
pub(crate) fn open_opencode(home: &Path) -> rusqlite::Result<Option<rusqlite::Connection>> {
    let db = home.join(OPENCODE_DB);
    if !db.exists() {
        return Ok(None);
    }
    let conn = rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(Duration::from_millis(500))?;
    Ok(Some(conn))
}

/// 这个会话在 OpenCode 2 的表里（`session_v2`、`session_message`，2026-10-31 本机 2.2.4 核对）：升级到 2 时 OpenCode 1 的
/// 会话也迁了过去；还是 OpenCode 1 的库没有这两张表，查旧表（`message`、`part`）。
pub(crate) fn opencode_v2(conn: &rusqlite::Connection, session: &str) -> rusqlite::Result<bool> {
    let tables: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN ('session_v2', 'session_message')",
        [],
        |r| r.get(0),
    )?;
    if tables < 2 {
        return Ok(false);
    }
    conn.query_row("SELECT count(*) FROM session_v2 WHERE id = ?1", [session], |r| r.get::<_, i64>(0)).map(|n| n > 0)
}

/// 只读查询 OpenCode 库里这个会话的用户提示词，由旧到新。库或会话不在为 Ok(None)；查询出错为 Err。
fn opencode_prompts(home: &Path, session: &str) -> rusqlite::Result<Option<Vec<Prompt>>> {
    let Some(conn) = open_opencode(home)? else { return Ok(None) };
    if opencode_v2(&conn, session)? {
        let mut messages = conn.prepare("SELECT type, time_created, data FROM session_message WHERE session_id = ?1 ORDER BY seq")?;
        let rows = messages
            .query_map([session], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let out = rows
            .iter()
            .filter_map(|(kind, at, data)| {
                let data = serde_json::from_str::<Value>(data).ok()?;
                Some(Prompt { text: opencode_v2_prompt(kind, &data)?, at: Some(*at) })
            })
            .collect();
        return Ok(Some(out));
    }
    let mut messages = conn.prepare("SELECT id, time_created, data FROM message WHERE session_id = ?1 ORDER BY time_created, id")?;
    let rows = messages
        .query_map([session], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.is_empty() {
        return Ok(None);
    }
    let mut parts = conn.prepare("SELECT data FROM part WHERE message_id = ?1 ORDER BY id")?;
    let mut parts_of = |id: &str| -> rusqlite::Result<Vec<Value>> {
        Ok(parts
            .query_map([id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .filter_map(|d| serde_json::from_str(d).ok())
            .collect())
    };
    let messages: Vec<(String, i64, Value)> =
        rows.into_iter().filter_map(|(id, at, d)| serde_json::from_str(&d).ok().map(|v| (id, at, v))).collect();
    let mut out = Vec::new();
    for (id, at, message) in &messages {
        if message["role"] != "user" {
            continue;
        }
        let text = match opencode_prompt(message, &parts_of(id)?) {
            Some(t) => Some(t),
            // 只有 OpenCode 补的段：可能是输入行的 `!命令`，看紧接着的那条回复。
            None => match messages.iter().find(|(_, _, m)| m["role"] == "assistant" && m["parentID"] == id.as_str()) {
                Some((reply, _, _)) => opencode_shell(&parts_of(reply)?),
                None => None,
            },
        };
        if let Some(text) = text {
            out.push(Prompt { text, at: Some(*at) });
        }
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_dir;
    use std::path::Path;

    #[test]
    fn reads_segments_in_order_and_redacts() {
        let d = test_dir("prompts");
        let old = d.join("old.jsonl");
        let new = d.join("new.jsonl");
        fs::write(&old, "{\"type\":\"user\",\"message\":{\"content\":\"第一段\"}}\n").unwrap();
        fs::write(&new, "{\"type\":\"user\",\"message\":{\"content\":\"用 API_KEY=abc123 调接口\"}}\n").unwrap();
        let seg = |id, p: &Path| Segment { conversation_id: id, tool: Tool::Claude, native_id: format!("n{id}"),
                                           transcript_path: Some(p.display().to_string()), prompted: true };
        let job = PromptJob { conn: 1, req_id: json!(9), home: d.clone(),
                              segments: vec![seg(1, &old), seg(2, &new), seg(3, &d.join("gone.jsonl"))] };
        let r = read(&job);
        assert_eq!(r["id"], 9);
        let texts: Vec<&str> = r["items"].as_array().unwrap().iter().map(|i| i["text"].as_str().unwrap()).collect();
        assert_eq!(texts, ["第一段", "用 API_KEY=[REDACTED] 调接口"]);
        assert_eq!(r["items"][1]["conversation_id"], 2);
        let available: Vec<bool> = r["segments"].as_array().unwrap().iter().map(|s| s["available"].as_bool().unwrap()).collect();
        assert_eq!(available, [true, true, false], "会话记录不在了的段标出来");
        assert_eq!(r["segments"][1]["native_id"], "n2", "栏头显示的会话 ID");
        assert_eq!(r["truncated"], false);
        let _ = fs::remove_dir_all(&d);
    }

    /// 还没提问的新会话（Claude 要等第一条提示词才写会话记录）：读不到记录不算「已不在」；提问过的照旧算（2026-10-31 正式版验收发现）。
    #[test]
    fn unprompted_segment_without_record_is_not_missing() {
        let d = test_dir("prompts-unprompted");
        let seg = |id, tool, prompted| Segment { conversation_id: id, tool, native_id: format!("n{id}"),
                                                 transcript_path: Some(d.join(format!("{id}.jsonl")).display().to_string()), prompted };
        let job = PromptJob { conn: 1, req_id: json!(1), home: d.clone(),
                              segments: vec![seg(1, Tool::Claude, false), seg(2, Tool::Claude, true), seg(3, Tool::OpenCode, false)] };
        let r = read(&job);
        let available: Vec<bool> = r["segments"].as_array().unwrap().iter().map(|s| s["available"].as_bool().unwrap()).collect();
        assert_eq!(available, [true, false, true]);
        assert!(r["items"].as_array().unwrap().is_empty());
        let _ = fs::remove_dir_all(&d);
    }

    /// Copilot 按会话 ID 在家目录下找 `events.jsonl`（库里没有路径）。
    #[test]
    fn copilot_record_by_session_id_and_unsupported_tools() {
        let d = test_dir("prompts-copilot");
        let sid = "20a11794-8e34-4845-8c3e-cc24f21c6f0a";
        let dir = d.join(format!(".copilot/session-state/{sid}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("events.jsonl"), "{\"type\":\"user.message\",\"data\":{\"content\":\"只回复 ready\"}}\n").unwrap();
        let seg = |id, tool, native: &str| Segment { conversation_id: id, tool, native_id: native.to_owned(), transcript_path: None, prompted: true };
        let job = PromptJob { conn: 1, req_id: json!(1), home: d.clone(),
                              segments: vec![seg(1, Tool::Copilot, sid), seg(2, Tool::Copilot, "gone")] };
        let r = read(&job);
        assert_eq!(r["items"][0]["text"], "只回复 ready");
        let flags: Vec<(bool, bool)> = r["segments"].as_array().unwrap().iter()
            .map(|s| (s["supported"].as_bool().unwrap(), s["available"].as_bool().unwrap())).collect();
        assert_eq!(flags, [(true, true), (true, false)]);
        let _ = fs::remove_dir_all(&d);
    }

    /// OpenCode：只读查询它的 SQLite 库（表结构同 1.19.5）；库不在为「记录已不在」，表结构不对为 `error`（读取失败）。
    #[test]
    fn opencode_db_prompts_and_errors() {
        let d = test_dir("prompts-opencode");
        let seg = |id, native: &str| Segment { conversation_id: id, tool: Tool::OpenCode, native_id: native.to_owned(), transcript_path: None, prompted: true };
        let job = |segments| PromptJob { conn: 1, req_id: json!(1), home: d.clone(), segments };
        let r = read(&job(vec![seg(1, "ses_a")]));
        assert_eq!((r["segments"][0]["available"].as_bool(), r["segments"][0]["error"].as_bool()), (Some(false), Some(false)), "没有库");
        let db = d.join(OPENCODE_DB);
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             INSERT INTO message VALUES ('m1','ses_a',100,100,'{\"role\":\"user\"}'), ('m2','ses_a',200,200,'{\"role\":\"assistant\"}'),
                                        ('m3','ses_a',300,300,'{\"role\":\"user\"}'), ('m4','ses_b',150,150,'{\"role\":\"user\"}');
             INSERT INTO part VALUES ('p1','m1','ses_a',100,100,'{\"type\":\"text\",\"text\":\"用 token=sk-abcdef123456 登录\"}'),
                                     ('p2','m2','ses_a',200,200,'{\"type\":\"text\",\"text\":\"好的\"}'),
                                     ('p3','m3','ses_a',300,300,'{\"type\":\"text\",\"text\":\"第二问\"}'),
                                     ('p4','m4','ses_b',150,150,'{\"type\":\"text\",\"text\":\"别的会话\"}');",
        )
        .unwrap();
        drop(c);
        let r = read(&job(vec![seg(1, "ses_a"), seg(2, "ses_gone")]));
        let texts: Vec<&str> = r["items"].as_array().unwrap().iter().map(|i| i["text"].as_str().unwrap()).collect();
        assert_eq!(texts.len(), 2);
        assert!(texts[0].contains("[REDACTED]") && !texts[0].contains("sk-abcdef"), "显示前脱敏：{}", texts[0]);
        assert_eq!(texts[1], "第二问");
        assert_eq!(r["items"][1]["at"], 300);
        assert_eq!(r["segments"][1]["available"], false, "库里没有这个会话");
        // 表结构变了（缺列）：读取失败，不说成记录已不在。
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch("DROP TABLE part; CREATE TABLE part (id TEXT, message_id TEXT);").unwrap();
        drop(c);
        let r = read(&job(vec![seg(1, "ses_a")]));
        assert_eq!((r["segments"][0]["available"].as_bool(), r["segments"][0]["error"].as_bool()), (Some(false), Some(true)));
        let _ = fs::remove_dir_all(&d);
    }

    /// OpenCode 2：历史提示词读 `session_message`，用户消息与输入行的 `!命令`（存为 `type=shell`）。
    #[test]
    fn opencode_2_prompts() {
        let d = test_dir("prompts-opencode2");
        let db = d.join(OPENCODE_DB);
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            r#"CREATE TABLE session_v2 (id TEXT PRIMARY KEY);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER, time_created INTEGER,
                                           time_updated INTEGER, data TEXT);
             INSERT INTO session_v2 VALUES ('ses_2');
             INSERT INTO session_message VALUES
                ('m1','ses_2','user',4,100,100,'{"text":"用 bash 执行 pwd"}'),
                ('m2','ses_2','assistant',7,200,200,'{"content":[{"type":"text","text":"/w"}]}'),
                ('m3','ses_2','shell',9,300,300,'{"command":"echo hi","status":"exited"}'),
                ('m4','ses_2','synthetic',10,400,400,'{"text":"补的"}');"#,
        )
        .unwrap();
        drop(c);
        let seg = Segment { conversation_id: 1, tool: Tool::OpenCode, native_id: "ses_2".to_owned(), transcript_path: None, prompted: true };
        let r = read(&PromptJob { conn: 1, req_id: json!(1), home: d.clone(), segments: vec![seg] });
        let texts: Vec<&str> = r["items"].as_array().unwrap().iter().map(|i| i["text"].as_str().unwrap()).collect();
        assert_eq!(texts, ["用 bash 执行 pwd", "!echo hi"]);
        assert_eq!(r["items"][1]["at"], 300);
        let _ = fs::remove_dir_all(&d);
    }
}
