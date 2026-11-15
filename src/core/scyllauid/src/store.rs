//! 元数据存储（docs/design.md 4.6 节，docs/protocol.md 第 7、8 节）：SQLite，WAL，只存元数据。
//! 只由核心线程使用（core.rs），不跨线程共享。对外的行一律转成协议中的 JSON 对象（列名即字段名）。

use scyllaui_proto::redact::redact;
use scyllaui_proto::{HookEvent, Kind};
use rusqlite::types::ValueRef;
use rusqlite::{params, Connection, OptionalExtension, Params, Row};
use serde_json::{json, Map, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

/// 升级脚本，按顺序执行；第 i 个执行后 `user_version = i + 1`。只能追加，已发布的脚本不再修改。
pub const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_init.sql"),
    include_str!("../migrations/0002_last_reply.sql"),
    include_str!("../migrations/0003_project_pinned.sql"),
];

/// 「最新提示词」与「最后一条 AI 回复」最多保留的字符数（2026-10-31 用户要求由 200 调到 400：侧栏悬停给全文，长提示词 200 字偏少）。
const PROMPT_CHARS: usize = 400;

/// 保留上限（4.6 节）：已结束的对话默认保留最近 30 天且最多 200 条（2026-10-18 由 500 条改为 100 条，2026-11-14 改为 200 条：
/// 第二轮长跑里 100 条一天就用满；M4a 起可在设置中修改，
/// 见 retention.rs）；事件最多 20000 条。
pub const KEEP_DAYS: i64 = 30;
/// 侧栏「最近任务」的显示窗口（docs/design.md 第 5 节）：`recent_total` 只计这段时间内结束的对话。
pub const RECENT_DAYS: i64 = 14;
pub const KEEP_CONVERSATIONS: i64 = 200;
pub const KEEP_EVENTS: i64 = 20_000;

/// `prunable` 与 `prune_conversations` 共用的条件（?1 为截止时间，?2 为保留条数）。
const PRUNABLE: &str = "ended_at IS NOT NULL AND (ended_at < ?1 OR id NOT IN \
     (SELECT id FROM conversation WHERE ended_at IS NOT NULL ORDER BY ended_at DESC, id DESC LIMIT ?2))";

const PROJECT_COLS: &str = "id AS project_id, dir, name, sort_order, pinned";
const TERMINAL_COLS: &str = "id AS terminal_id, project_id, cwd, name, status, exit_code, \
     close_reason, created_at, closed_at";
const CONVERSATION_COLS: &str = "id AS conversation_id, tool, native_id, project_id, terminal_id, \
     resumed_from, title, title_source, last_prompt, last_reply, last_reply_at, cwd, transcript_path, cli_version, \
     started_at, last_event_at, ended_at, end_reason";

pub struct Store {
    conn: Connection,
}

/// `record_event` 的结果：事件归属的对话，以及需要推送的对话（新建、重新打开、结束或标题等字段有变化）。
pub struct Recorded {
    pub conversation: Option<i64>,
    pub changed: Vec<i64>,
}

impl Store {
    /// 换版本自检（`scyllauid --preflight`）：只读打开，确认库的版本不高于本程序支持的版本；不改文件。库不存在也算通过。
    pub fn check(path: &Path) -> Result<(), String> {
        if !path.exists() {
            return Ok(());
        }
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("只读打开数据库 {} 失败：{e}", path.display()))?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| format!("读取库版本失败：{e}"))?;
        if version > MIGRATIONS.len() as i64 {
            return Err(format!("库版本 {version} 高于本程序支持的 {}", MIGRATIONS.len()));
        }
        Ok(())
    }

    pub fn open(path: &Path) -> Result<Store, String> {
        let mut conn = Connection::open(path)
            .map_err(|e| format!("打开数据库 {} 失败：{e}", path.display()))?;
        migrate(&mut conn, path, MIGRATIONS)?;
        let mode: String = conn
            .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
            .map_err(|e| format!("设置 WAL 失败：{e}"))?;
        if mode != "wal" {
            return Err(format!("数据库未能切换到 WAL（当前 {mode}）"));
        }
        conn.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")
            .and_then(|_| conn.busy_timeout(Duration::from_millis(2000)))
            .map_err(|e| format!("设置连接参数失败：{e}"))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("设置数据库权限失败：{e}"))?;
        Ok(Store { conn })
    }

    fn query_json(&self, sql: &str, params: impl Params) -> rusqlite::Result<Vec<Value>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params, row_json)?;
        rows.collect()
    }

    fn one_json(&self, sql: &str, params: impl Params) -> rusqlite::Result<Option<Value>> {
        Ok(self.query_json(sql, params)?.into_iter().next())
    }

    // ---- 项目 ----

    pub fn project(&self, id: i64) -> rusqlite::Result<Option<Value>> {
        Ok(self
            .one_json(&format!("SELECT {PROJECT_COLS} FROM project WHERE id = ?1"), [id])?
            .map(project_json))
    }

    pub fn projects(&self) -> rusqlite::Result<Vec<Value>> {
        Ok(self
            .query_json(&format!("SELECT {PROJECT_COLS} FROM project ORDER BY sort_order, id"), [])?
            .into_iter()
            .map(project_json)
            .collect())
    }

    /// 按目录取项目，不存在则新建（名称取目录名，排在最后）。返回 `(项目 ID, 是否新建)`。
    pub fn ensure_project(&self, dir: &str, now: i64) -> rusqlite::Result<(i64, bool)> {
        let found: Option<i64> = self
            .conn
            .query_row("SELECT id FROM project WHERE dir = ?1", [dir], |r| r.get(0))
            .optional()?;
        if let Some(id) = found {
            return Ok((id, false));
        }
        let name = Path::new(dir)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir.to_owned());
        self.conn.execute(
            "INSERT INTO project (dir, name, sort_order, created_at) \
             VALUES (?1, ?2, (SELECT COALESCE(MAX(sort_order), -1) + 1 FROM project), ?3)",
            params![dir, name, now],
        )?;
        Ok((self.conn.last_insert_rowid(), true))
    }

    /// 返回 false 表示项目不存在。
    pub fn update_project(
        &self,
        id: i64,
        name: Option<&str>,
        sort_order: Option<i64>,
        pinned: Option<bool>,
    ) -> rusqlite::Result<bool> {
        Ok(self.conn.execute(
            "UPDATE project SET name = COALESCE(?2, name), sort_order = COALESCE(?3, sort_order), \
             pinned = COALESCE(?4, pinned) WHERE id = ?1",
            params![id, name, sort_order, pinned.map(i64::from)],
        )? > 0)
    }

    // ---- 终端 ----

    pub fn terminal(&self, id: &str) -> rusqlite::Result<Option<Value>> {
        self.one_json(
            &format!("SELECT {TERMINAL_COLS} FROM terminal WHERE id = ?1"),
            [id],
        )
    }

    pub fn alive_terminals(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM terminal WHERE status = 'alive' ORDER BY created_at, id")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        rows.collect()
    }

    /// 登记终端或重新确认其存活。`name` 为空时保留原有命名（清除命名走 `rename_terminal`）。
    pub fn open_terminal(
        &self,
        id: &str,
        project_id: i64,
        cwd: &str,
        name: Option<&str>,
        now: i64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO terminal (id, project_id, cwd, name, status, created_at) \
             VALUES (?1, ?2, ?3, ?4, 'alive', ?5) \
             ON CONFLICT (id) DO UPDATE SET project_id = excluded.project_id, cwd = excluded.cwd, \
               name = COALESCE(excluded.name, terminal.name), status = 'alive', exit_code = NULL, \
               close_reason = NULL, closed_at = NULL",
            params![id, project_id, cwd, name, now],
        )?;
        Ok(())
    }

    /// 终端改归项目（跟随当前目录，docs/protocol.md 第 4 节 `terminal.cwd`）。返回是否有变化。
    pub fn set_terminal_project(&self, id: &str, project_id: i64) -> rusqlite::Result<bool> {
        Ok(self.conn.execute(
            "UPDATE terminal SET project_id = ?2 WHERE id = ?1 AND status = 'alive' AND project_id != ?2",
            params![id, project_id],
        )? > 0)
    }

    /// 返回 false 表示终端不存在或已关闭。
    pub fn close_terminal(
        &self,
        id: &str,
        reason: &str,
        exit_code: Option<i64>,
        now: i64,
    ) -> rusqlite::Result<bool> {
        Ok(self.conn.execute(
            "UPDATE terminal SET status = 'closed', close_reason = ?2, exit_code = ?3, closed_at = ?4 \
             WHERE id = ?1 AND status = 'alive'",
            params![id, reason, exit_code, now],
        )? > 0)
    }

    pub fn rename_terminal(&self, id: &str, name: Option<&str>) -> rusqlite::Result<bool> {
        Ok(self.conn.execute(
            "UPDATE terminal SET name = ?2 WHERE id = ?1",
            params![id, name],
        )? > 0)
    }

    // ---- 对话 ----

    pub fn conversation(&self, id: i64) -> rusqlite::Result<Option<Value>> {
        conversation_row(&self.conn, id)
    }

    pub fn active_conversations(&self) -> rusqlite::Result<Vec<Value>> {
        self.query_json(
            &format!(
                "SELECT {CONVERSATION_COLS} FROM conversation WHERE ended_at IS NULL \
                 ORDER BY started_at, id"
            ),
            [],
        )
    }

    /// 终端上最近一个未结束的对话，scyllauid 重启后恢复窗格绑定用：`(ID, 工具, 原生会话 ID, 标题)`。
    pub fn open_conversation_on(
        &self,
        terminal_id: &str,
    ) -> rusqlite::Result<Option<(i64, String, String, Option<String>)>> {
        self.conn
            .query_row(
                "SELECT id, tool, native_id, title FROM conversation \
                 WHERE terminal_id = ?1 AND ended_at IS NULL ORDER BY last_event_at DESC, id DESC",
                [terminal_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
    }

    /// 终端改归项目时，把它上面 `since` 之后开始、仍未结束的对话一并改归；有改动返回 true。
    pub fn set_conversation_project(&self, id: i64, project_id: i64, since: i64) -> rusqlite::Result<bool> {
        Ok(self.conn.execute(
            "UPDATE conversation SET project_id = ?2 \
             WHERE id = ?1 AND ended_at IS NULL AND started_at >= ?3 AND project_id IS NOT ?2",
            params![id, project_id, since],
        )? > 0)
    }

    /// 读到的最后一条 AI 回复（docs/design.md 第 5 节「实现（M1 第 6 块）」），脱敏后截前 `PROMPT_CHARS` 个字符（10.5 节决策 11）；
    /// 没有变化时返回 false。
    pub fn set_last_reply(&self, id: i64, reply: &str) -> rusqlite::Result<bool> {
        let reply: String = redact(reply.trim()).chars().take(PROMPT_CHARS).collect();
        Ok(self.conn.execute(
            "UPDATE conversation SET last_reply = ?2 WHERE id = ?1 AND last_reply IS NOT ?2",
            params![id, reply],
        )? > 0)
    }

    /// 对话的 CLI 版本：只在第一次写入（之后不改，docs/protocol.md 第 5 节）；写入了返回 true。
    pub fn set_cli_version(&self, id: i64, version: &str) -> rusqlite::Result<bool> {
        Ok(self.conn.execute(
            "UPDATE conversation SET cli_version = ?2 WHERE id = ?1 AND cli_version IS NULL",
            params![id, version],
        )? > 0)
    }

    /// 某款 CLI 最近一次对话记下的版本（M4b 块 6，「设置 › Agent」没在运行时显示）。
    pub fn latest_cli_version(&self, tool: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT cli_version FROM conversation WHERE tool = ?1 AND cli_version IS NOT NULL ORDER BY id DESC LIMIT 1",
                params![tool],
                |r| r.get(0),
            )
            .optional()
    }

    /// 读到的原生标题覆盖当前标题（docs/protocol.md 第 5 节 `title_source`），写入前脱敏；没有变化时返回 false。
    pub fn set_native_title(&self, id: i64, title: &str) -> rusqlite::Result<bool> {
        let title = redact(title);
        Ok(self.conn.execute(
            "UPDATE conversation SET title = ?2, title_source = 'native' \
             WHERE id = ?1 AND (title IS NOT ?2 OR title_source IS NOT 'native')",
            params![id, title],
        )? > 0)
    }

    /// 一键恢复（docs/protocol.md 第 7 节）：重开对话并绑到新终端；对话不存在时返回 false。
    pub fn reopen_on(&self, id: i64, terminal_id: &str) -> rusqlite::Result<bool> {
        Ok(self.conn.execute(
            "UPDATE conversation SET ended_at = NULL, end_reason = NULL, terminal_id = ?2 WHERE id = ?1",
            params![id, terminal_id],
        )? > 0)
    }

    /// 对话改绑到另一个终端（冲突中一方结束、另一方仍在运行，docs/protocol.md 第 7 节「冲突」）；有改动返回 true。
    pub fn set_conversation_terminal(&self, id: i64, terminal_id: &str) -> rusqlite::Result<bool> {
        Ok(self.conn.execute(
            "UPDATE conversation SET terminal_id = ?2 WHERE id = ?1 AND terminal_id IS NOT ?2",
            params![id, terminal_id],
        )? > 0)
    }

    /// 「删除记录」（docs/design.md 第 5 节「移除与删除」）：立即删除一个已结束的对话及其事件。开启 `secure_delete`
    /// 以零覆盖被删内容，删后重建库文件（`wipe`），库文件与 WAL 里都不再留有原文。
    /// 对话不存在为 `None`，未结束为 `Some(false)`（都不删）。
    pub fn delete_conversation(&mut self, id: i64) -> rusqlite::Result<Option<bool>> {
        let ended: Option<bool> = self
            .conn
            .query_row("SELECT ended_at IS NOT NULL FROM conversation WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        if ended != Some(true) {
            return Ok(ended);
        }
        self.conn.pragma_update(None, "secure_delete", true)?;
        let deleted = (|| {
            let tx = self.conn.transaction()?;
            tx.execute("DELETE FROM event WHERE conversation_id = ?1", [id])?;
            tx.execute("DELETE FROM conversation WHERE id = ?1", [id])?;
            tx.commit()?;
            self.wipe()
        })();
        self.conn.pragma_update(None, "secure_delete", false)?;
        deleted.map(|_| Some(true))
    }

    /// 结束一个对话；已结束时返回 false。
    pub fn end_conversation(&self, id: i64, reason: &str, now: i64) -> rusqlite::Result<bool> {
        Ok(self.conn.execute(
            "UPDATE conversation SET ended_at = ?2, end_reason = ?3 WHERE id = ?1 AND ended_at IS NULL",
            params![id, now, reason],
        )? > 0)
    }

    /// 结束终端上所有未结束的对话，返回这些对话的 ID。
    pub fn end_conversations_on(
        &mut self,
        terminal_id: &str,
        reason: &str,
        now: i64,
    ) -> rusqlite::Result<Vec<i64>> {
        let tx = self.conn.transaction()?;
        let ids: Vec<i64> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM conversation WHERE terminal_id = ?1 AND ended_at IS NULL",
            )?;
            let rows = stmt.query_map([terminal_id], |r| r.get(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        tx.execute(
            "UPDATE conversation SET ended_at = ?2, end_reason = ?3 \
             WHERE terminal_id = ?1 AND ended_at IS NULL",
            params![terminal_id, now, reason],
        )?;
        tx.commit()?;
        Ok(ids)
    }

    /// 写入一个已被状态机接受的 Hook 事件，一个事务（docs/protocol.md 第 7 节「对话」）。
    /// `bound` 是窗格此前绑定的对话；`keep_on` 是另一个仍在运行同一对话的终端，SessionEnd 时对话改绑过去而不结束
    /// （第 7 节「冲突」）。
    pub fn record_event(
        &mut self,
        terminal_id: &str,
        tool: &str,
        ev: &HookEvent,
        bound: Option<i64>,
        keep_on: Option<&str>,
        now: i64,
    ) -> rusqlite::Result<Recorded> {
        let tx = self.conn.transaction()?;
        let mut changed = Vec::new();
        let mut conversation = bound;
        // 标题与提示词写库前脱敏（docs/design.md 10.5 节决策 11）；提示词先脱敏再截断，截断不会留下半截密钥。
        let title = ev.title.as_deref().map(redact);
        if let Some(sid) = &ev.session_id {
            let existing: Option<i64> = tx
                .query_row(
                    "SELECT id FROM conversation WHERE tool = ?1 AND native_id = ?2",
                    params![tool, sid],
                    |r| r.get(0),
                )
                .optional()?;
            // 同一终端换了会话（如 Claude `/clear`）：上一个对话结束。转入后台继续的同一段对话（`forked`）
            // 以 `continued` 结束，不进入「最近任务」（docs/protocol.md 第 7 节）。
            if ev.kind == Kind::SessionStart {
                if let Some(old) = bound.filter(|b| Some(*b) != existing) {
                    let reason = if ev.forked { "continued" } else { "replaced" };
                    if tx.execute(
                        "UPDATE conversation SET ended_at = ?2, end_reason = ?3 \
                         WHERE id = ?1 AND ended_at IS NULL",
                        params![old, now, reason],
                    )? > 0
                    {
                        changed.push(old);
                    }
                }
            }
            let id = match existing {
                Some(id) => {
                    let before = conversation_row(&tx, id)?;
                    // 被接受的事件说明对话仍在进行：已结束的重新打开（恢复后原生 ID 不变），换绑到当前终端。
                    // Hook 带来的标题（Kimi）不覆盖已读到的原生标题（docs/protocol.md 第 5 节 `title_source`）。
                    tx.execute(
                        "UPDATE conversation SET terminal_id = ?2, \
                           title = CASE WHEN ?3 IS NOT NULL AND title_source IS NOT 'native' THEN ?3 ELSE title END, \
                           title_source = CASE WHEN ?3 IS NOT NULL AND title_source IS NOT 'native' \
                             THEN 'hook' ELSE title_source END, \
                           transcript_path = COALESCE(?4, transcript_path), cwd = COALESCE(cwd, ?5), \
                           last_event_at = ?6, ended_at = NULL, end_reason = NULL \
                         WHERE id = ?1",
                        params![id, terminal_id, title, ev.transcript_path, ev.cwd, now],
                    )?;
                    if !same_except_activity(before.as_ref(), conversation_row(&tx, id)?.as_ref()) {
                        changed.push(id);
                    }
                    id
                }
                None => {
                    tx.execute(
                        "INSERT INTO conversation (tool, native_id, project_id, terminal_id, title, \
                           title_source, cwd, transcript_path, started_at, last_event_at) \
                         VALUES (?1, ?2, (SELECT project_id FROM terminal WHERE id = ?3), ?3, ?4, \
                           CASE WHEN ?4 IS NULL THEN NULL ELSE 'hook' END, ?5, ?6, ?7, ?7)",
                        params![tool, sid, terminal_id, title, ev.cwd, ev.transcript_path, now],
                    )?;
                    let id = tx.last_insert_rowid();
                    // 转入后台继续的对话（Claude `source=fork`）接替终端上的上一段：记 resumed_from，
                    // 沿用上一段的任务标题，直到读到它自己的原生标题（docs/design.md 4.4 节）。
                    if let Some(old) = bound.filter(|_| ev.forked) {
                        tx.execute(
                            "UPDATE conversation SET resumed_from = ?2, \
                               title = COALESCE(title, (SELECT title FROM conversation WHERE id = ?2)), \
                               title_source = CASE WHEN title IS NULL \
                                 THEN (SELECT title_source FROM conversation WHERE id = ?2) ELSE title_source END \
                             WHERE id = ?1",
                            params![id, old],
                        )?;
                    }
                    changed.push(id);
                    id
                }
            };
            if let Some(prompt) = &ev.prompt {
                let summary: String = redact(prompt).chars().take(PROMPT_CHARS).collect();
                // 标题为空时第一条提示词就是任务标题，之后的提示词只更新 last_prompt，标题不随每轮变化。
                tx.execute(
                    "UPDATE conversation SET last_prompt = ?2, title = COALESCE(title, ?2), \
                       title_source = CASE WHEN title IS NULL THEN 'prompt' ELSE title_source END \
                     WHERE id = ?1",
                    params![id, summary],
                )?;
                if !changed.contains(&id) {
                    changed.push(id);
                }
            }
            // 一轮结束即最后一条 AI 回复的时间（回复正文由读标题的线程稍后读到）。
            if matches!(ev.kind, Kind::TurnEnd | Kind::TurnFail) {
                tx.execute(
                    "UPDATE conversation SET last_reply_at = ?2 WHERE id = ?1",
                    params![id, now],
                )?;
                if !changed.contains(&id) {
                    changed.push(id);
                }
            }
            if ev.kind == Kind::SessionEnd {
                match keep_on {
                    Some(other) => tx.execute(
                        "UPDATE conversation SET terminal_id = ?2 WHERE id = ?1",
                        params![id, other],
                    )?,
                    None => tx.execute(
                        "UPDATE conversation SET ended_at = ?2, end_reason = 'session_end' WHERE id = ?1",
                        params![id, now],
                    )?,
                };
                if !changed.contains(&id) {
                    changed.push(id);
                }
            }
            conversation = Some(id);
        }
        let detail = ev
            .notification_type
            .as_ref()
            .map(|n| json!({ "notification_type": n }).to_string());
        tx.execute(
            "INSERT INTO event (conversation_id, terminal_id, kind, detail, at) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![conversation, terminal_id, ev.name, detail, now],
        )?;
        tx.commit()?;
        Ok(Recorded {
            conversation,
            changed,
        })
    }

    /// 「最近任务」：已结束的对话，按结束时间倒序分页；`since` 只取此后结束的。从没提问过的（没有提示词也没有标题）
    /// 无法恢复，不算（docs/protocol.md 第 7 节）。被后续对话接替的（`continued`）
    /// 不算。返回 `(本页, 下一页游标或 null)`。
    pub fn recent(
        &self,
        project_id: Option<i64>,
        before: Option<(i64, i64)>,
        since: Option<i64>,
        limit: i64,
    ) -> rusqlite::Result<(Vec<Value>, Value)> {
        let (before_at, before_id) = before.unzip();
        let items = self.query_json(
            &format!(
                "SELECT {CONVERSATION_COLS} FROM conversation \
                 WHERE ended_at IS NOT NULL AND COALESCE(end_reason, '') NOT IN ('continued', 'unrecoverable') \
                   AND (last_prompt IS NOT NULL OR title IS NOT NULL) \
                   AND (?1 IS NULL OR project_id = ?1) \
                   AND (?2 IS NULL OR (ended_at, id) < (?2, ?3)) \
                   AND (?5 IS NULL OR ended_at >= ?5) \
                 ORDER BY ended_at DESC, id DESC LIMIT ?4"
            ),
            params![project_id, before_at, before_id, limit, since],
        )?;
        let next = match items.last() {
            Some(last) if items.len() as i64 == limit => {
                json!({"ended_at": last["ended_at"], "conversation_id": last["conversation_id"]})
            }
            _ => Value::Null,
        };
        Ok((items, next))
    }

    /// 搜索已结束的对话（M2 第 4 块，docs/protocol.md 第 7 节「搜索」）：范围与 `recent` 相同但不限显示窗口；
    /// 标题、最新提示词、最后回复、目录、项目名与项目目录任一含 `query`（`LIKE` 子串，只对 ASCII 不区分大小写）。
    pub fn search_ended(&self, query: &str, limit: i64) -> rusqlite::Result<Vec<Value>> {
        let mut pattern = String::from("%");
        for c in query.chars() {
            if matches!(c, '\\' | '%' | '_') {
                pattern.push('\\');
            }
            pattern.push(c);
        }
        pattern.push('%');
        self.query_json(
            &format!(
                "SELECT {CONVERSATION_COLS} FROM conversation \
                 WHERE ended_at IS NOT NULL AND COALESCE(end_reason, '') NOT IN ('continued', 'unrecoverable') \
                   AND (last_prompt IS NOT NULL OR title IS NOT NULL) \
                   AND (title LIKE ?1 ESCAPE '\\' OR last_prompt LIKE ?1 ESCAPE '\\' OR last_reply LIKE ?1 ESCAPE '\\' \
                        OR cwd LIKE ?1 ESCAPE '\\' OR project_id IN \
                          (SELECT id FROM project WHERE name LIKE ?1 ESCAPE '\\' OR dir LIKE ?1 ESCAPE '\\')) \
                 ORDER BY ended_at DESC, id DESC LIMIT ?2"
            ),
            params![pattern, limit],
        )
    }

    /// 显示窗口（最近 RECENT_DAYS 天）内结束的对话数。
    pub fn recent_total(&self, now: i64) -> rusqlite::Result<i64> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM conversation \
             WHERE ended_at IS NOT NULL AND COALESCE(end_reason, '') NOT IN ('continued', 'unrecoverable') \
               AND (last_prompt IS NOT NULL OR title IS NOT NULL) AND ended_at >= ?1",
            [now - RECENT_DAYS * 86_400_000],
            |r| r.get(0),
        )
    }

    // ---- 清理与关闭 ----

    /// 超出保留设置的已结束对话：`keep_days` 天前结束的，或按结束时间倒数排在第 `keep_conversations` 条之后的。
    pub fn prunable(&self, now: i64, keep_days: i64, keep_conversations: i64) -> rusqlite::Result<Vec<i64>> {
        let mut st = self.conn.prepare(&format!("SELECT id FROM conversation WHERE {PRUNABLE}"))?;
        let ids = st.query_map(params![now - keep_days * 86_400_000, keep_conversations], |r| r.get(0))?;
        ids.collect()
    }

    /// 按保留设置删除已结束的对话（事件随之级联删除），以及超期且不再被对话引用的已关闭终端。返回被删对话的 ID。
    pub fn prune_conversations(&self, now: i64, keep_days: i64, keep_conversations: i64) -> rusqlite::Result<Vec<i64>> {
        let cutoff = now - keep_days * 86_400_000;
        let ids = self.prunable(now, keep_days, keep_conversations)?;
        if !ids.is_empty() {
            self.conn.execute(
                &format!("DELETE FROM conversation WHERE {PRUNABLE}"),
                params![cutoff, keep_conversations],
            )?;
        }
        self.conn.execute(
            "DELETE FROM terminal WHERE status = 'closed' AND closed_at < ?1 AND id NOT IN \
               (SELECT terminal_id FROM conversation WHERE terminal_id IS NOT NULL)",
            [cutoff],
        )?;
        Ok(ids)
    }

    /// 全部已结束的对话（「清除记录」的条数）。
    pub fn ended_ids(&self) -> rusqlite::Result<Vec<i64>> {
        let mut st = self.conn.prepare("SELECT id FROM conversation WHERE ended_at IS NOT NULL")?;
        let ids = st.query_map([], |r| r.get(0))?;
        ids.collect()
    }

    /// 「清除记录」（M4a，docs/design.md 10.4 节决策 7）：删除全部已结束的对话及其事件，以及不再被对话引用的已关闭终端。
    /// 同「删除记录」开启 `secure_delete` 并在删后重建库文件（`wipe`），库文件与 WAL 里都不再留有原文。返回被删对话的 ID。
    pub fn delete_all_ended(&mut self) -> rusqlite::Result<Vec<i64>> {
        let ids = self.ended_ids()?;
        self.conn.pragma_update(None, "secure_delete", true)?;
        let deleted = (|| {
            let tx = self.conn.transaction()?;
            tx.execute(
                "DELETE FROM event WHERE conversation_id IN (SELECT id FROM conversation WHERE ended_at IS NOT NULL)",
                [],
            )?;
            tx.execute("DELETE FROM conversation WHERE ended_at IS NOT NULL", [])?;
            tx.execute(
                "DELETE FROM terminal WHERE status = 'closed' AND id NOT IN \
                   (SELECT terminal_id FROM conversation WHERE terminal_id IS NOT NULL)",
                [],
            )?;
            tx.commit()?;
            self.wipe()
        })();
        self.conn.pragma_update(None, "secure_delete", false)?;
        deleted.map(|_| ids)
    }

    /// 删除记录之后重建库文件并截断 WAL。`secure_delete` 只清零这次删掉的内容；之前保留清理（不开 `secure_delete`）
    /// 删掉的对话、行更新与页拆分留下的旧副本仍在空闲页与页内空闲区里（2026-10-23 实测），`VACUUM` 只保留现存数据重写整个库。
    fn wipe(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch("VACUUM")?;
        self.conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
    }

    pub fn prune_events(&self) -> rusqlite::Result<usize> {
        self.conn.execute(
            "DELETE FROM event WHERE id <= (SELECT id FROM event ORDER BY id DESC LIMIT 1 OFFSET ?1)",
            [KEEP_EVENTS],
        )
    }

    /// 退出前把 WAL 合并回主库。
    pub fn checkpoint(&self) -> rusqlite::Result<()> {
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
    }
}

/// 执行未执行过的升级脚本。库的版本高于 `migrations` 时拒绝，不改动文件；
/// 已有数据的库升级前先备份为 `<库文件名>.v<旧版本>.bak`。
pub fn migrate(conn: &mut Connection, path: &Path, migrations: &[&str]) -> Result<(), String> {
    let current: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| format!("读取数据库版本失败：{e}"))?;
    let target = migrations.len() as i64;
    if current > target {
        return Err(format!(
            "数据库 {} 的版本 {current} 高于本程序支持的 {target}，拒绝打开（未改动文件）",
            path.display()
        ));
    }
    if current == target {
        return Ok(());
    }
    if current > 0 {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let backup = path.with_file_name(format!("{name}.v{current}.bak"));
        // 上次升级失败时留下的同名备份内容相同（升级在事务中，失败即回滚），覆盖即可。
        let _ = fs::remove_file(&backup);
        conn.execute("VACUUM INTO ?1", [backup.to_string_lossy()])
            .map_err(|e| format!("升级前备份到 {} 失败：{e}", backup.display()))?;
    }
    for (i, sql) in migrations.iter().enumerate().skip(current as usize) {
        let version = i as i64 + 1;
        let tx = conn
            .transaction()
            .map_err(|e| format!("开始升级事务失败：{e}"))?;
        tx.execute_batch(sql)
            .and_then(|_| tx.pragma_update(None, "user_version", version))
            .and_then(|_| tx.commit())
            .map_err(|e| format!("升级到第 {version} 版失败：{e}"))?;
    }
    Ok(())
}

fn conversation_row(conn: &Connection, id: i64) -> rusqlite::Result<Option<Value>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CONVERSATION_COLS} FROM conversation WHERE id = ?1"
    ))?;
    let row = stmt.query_map([id], row_json)?.next().transpose();
    row
}

/// 两行对话除最近活动时间外完全相同：`last_event_at` 变化本身不触发推送。
fn same_except_activity(a: Option<&Value>, b: Option<&Value>) -> bool {
    let strip = |v: Option<&Value>| {
        v.cloned().map(|mut v| {
            v.as_object_mut().map(|o| o.remove("last_event_at"));
            v
        })
    };
    strip(a) == strip(b)
}

/// 项目行：库里的 `pinned` 是 0 / 1，对外为布尔（docs/protocol.md 第 5 节）。
fn project_json(mut v: Value) -> Value {
    v["pinned"] = json!(v["pinned"].as_i64() == Some(1));
    v
}

fn row_json(row: &Row) -> rusqlite::Result<Value> {
    let stmt = row.as_ref();
    let mut obj = Map::new();
    for i in 0..stmt.column_count() {
        let v = match row.get_ref(i)? {
            ValueRef::Null | ValueRef::Blob(_) => Value::Null,
            ValueRef::Integer(n) => json!(n),
            ValueRef::Real(f) => json!(f),
            ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        };
        obj.insert(stmt.column_name(i)?.to_owned(), v);
    }
    Ok(Value::Object(obj))
}

#[cfg(test)]
impl Store {
    /// 测试用：执行一条返回单个整数的查询。
    pub fn count(&self, sql: &str) -> i64 {
        self.conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    pub fn exec(&self, sql: &str) {
        self.conn.execute_batch(sql).unwrap();
    }

    /// 测试用：对话的标题与来源。
    pub fn title_of(&self, native_id: &str) -> (Option<String>, Option<String>) {
        self.conn
            .query_row(
                "SELECT title, title_source FROM conversation WHERE native_id = ?1",
                [native_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
    }
}

#[cfg(test)]
pub fn test_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("scyllauid-test-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use scyllaui_proto::{normalize, Tool};

    fn ev(v: Value) -> HookEvent {
        normalize(Tool::Claude, &v).unwrap()
    }

    fn version(path: &Path) -> i64 {
        Connection::open(path)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn fresh_database_is_migrated_once() {
        let d = test_dir("fresh");
        let db = d.join("t.db");
        let s = Store::open(&db).unwrap();
        assert_eq!(s.count("PRAGMA user_version"), MIGRATIONS.len() as i64);
        assert_eq!(
            s.count("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'"),
            4
        );
        let mode: String = s
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        assert_eq!(
            fs::metadata(&db).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(s);
        Store::open(&db).unwrap();
        assert_eq!(version(&db), MIGRATIONS.len() as i64);
        assert!(!d.join("t.db.v1.bak").exists(), "新建与重开都不需要备份");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn newer_database_is_refused_without_touching_it() {
        let d = test_dir("newer");
        let db = d.join("t.db");
        drop(Store::open(&db).unwrap());
        let c = Connection::open(&db).unwrap();
        c.execute_batch("PRAGMA user_version = 99; PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        drop(c);
        let before = fs::read(&db).unwrap();
        let err = Store::open(&db).err().unwrap();
        assert!(err.contains("99"), "{err}");
        assert_eq!(fs::read(&db).unwrap(), before, "拒绝打开时不得改动文件");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn upgrade_backs_up_first_and_failed_step_rolls_back() {
        let d = test_dir("upgrade");
        let db = d.join("t.db");
        let mut c = Connection::open(&db).unwrap();
        migrate(&mut c, &db, &MIGRATIONS[..1]).unwrap();
        c.execute("INSERT INTO project (dir, name, sort_order, created_at) VALUES ('/w/a', 'a', 0, 1)", []).unwrap();
        let v2 = "ALTER TABLE project ADD COLUMN color TEXT;";
        migrate(&mut c, &db, &[MIGRATIONS[0], v2]).unwrap();
        assert_eq!(version(&db), 2);
        let bak = d.join("t.db.v1.bak");
        assert_eq!(version(&bak), 1);
        let n: i64 = Connection::open(&bak)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM project", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1, "备份含升级前的数据");
        let color: Option<String> = c
            .query_row("SELECT color FROM project WHERE dir = '/w/a'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(color, None);
        let err = migrate(&mut c, &db, &[MIGRATIONS[0], v2, "NOT SQL;"]).unwrap_err();
        assert!(err.contains("第 3 版"), "{err}");
        assert_eq!(version(&db), 2, "失败的升级整体回滚");
        let _ = fs::remove_dir_all(&d);
    }

    /// 第 6 块的 0002：第 1 版的库打开即升级（先备份），已有对话的回复与回复时间为空，可写入。
    #[test]
    fn version_one_database_gains_last_reply_columns() {
        let d = test_dir("upgrade-0002");
        let db = d.join("t.db");
        let mut c = Connection::open(&db).unwrap();
        migrate(&mut c, &db, &MIGRATIONS[..1]).unwrap();
        c.execute_batch(
            "INSERT INTO conversation (tool, native_id, last_prompt, started_at, last_event_at, ended_at, end_reason) \
             VALUES ('codex', 'x', '问', 1, 2, 3, 'session_end');",
        )
        .unwrap();
        drop(c);
        let s = Store::open(&db).unwrap();
        assert_eq!(version(&db), MIGRATIONS.len() as i64);
        assert_eq!(version(&d.join("t.db.v1.bak")), 1);
        let conv = s.conversation(1).unwrap().unwrap();
        assert_eq!((conv["last_reply"].clone(), conv["last_reply_at"].clone()), (Value::Null, Value::Null));
        assert!(s.set_last_reply(1, &format!("  主人，{}", "好".repeat(500))).unwrap());
        let reply = s.conversation(1).unwrap().unwrap()["last_reply"].as_str().unwrap().to_owned();
        assert!(reply.starts_with("主人") && reply.chars().count() == 400);
        assert!(!s.set_last_reply(1, &reply).unwrap(), "没有变化不算更新");
        let _ = fs::remove_dir_all(&d);
    }

    /// 0.3.5 的 0003：第 2 版（0.3.4）带数据的库打开即升级到第 3 版，先备份为 `.v2.bak`；原有项目、终端、对话都在，
    /// 项目全部未置顶，对外为布尔；置顶可写可撤，库里只允许 0 / 1；sort_order 不受影响。
    #[test]
    fn version_two_database_gains_pinned_column() {
        let d = test_dir("upgrade-0003");
        let db = d.join("t.db");
        let mut c = Connection::open(&db).unwrap();
        migrate(&mut c, &db, &MIGRATIONS[..2]).unwrap();
        c.execute_batch(
            "INSERT INTO project (dir, name, sort_order, created_at) VALUES ('/w/a', 'a', 0, 1), ('/w/b', 'b', 1, 1);
             INSERT INTO terminal (id, project_id, cwd, status, created_at) VALUES ('t1', 2, '/w/b', 'alive', 1);
             INSERT INTO conversation (tool, native_id, project_id, last_prompt, last_reply, started_at, last_event_at, ended_at, end_reason) \
             VALUES ('codex', 'x', 1, '问', '答', 1, 2, 3, 'session_end');",
        )
        .unwrap();
        drop(c);
        let s = Store::open(&db).unwrap();
        assert_eq!(version(&db), 3);
        let bak = d.join("t.db.v2.bak");
        assert_eq!(version(&bak), 2, "升级前备份为第 2 版");
        let projects = s.projects().unwrap();
        assert_eq!(projects.len(), 2);
        assert!(projects.iter().all(|p| p["pinned"] == json!(false)), "{projects:?}");
        assert_eq!(s.count("SELECT COUNT(*) FROM terminal"), 1);
        assert_eq!(s.conversation(1).unwrap().unwrap()["last_reply"], "答", "原有数据不丢");
        assert!(s.update_project(2, None, None, Some(true)).unwrap());
        let p = s.project(2).unwrap().unwrap();
        assert_eq!((p["pinned"].clone(), p["sort_order"].clone(), p["name"].clone()), (json!(true), json!(1), json!("b")));
        assert!(s.update_project(2, Some("B"), None, None).unwrap());
        assert_eq!(s.project(2).unwrap().unwrap()["pinned"], json!(true), "不带 pinned 时不变");
        assert!(s.update_project(2, None, None, Some(false)).unwrap());
        assert_eq!(s.project(2).unwrap().unwrap()["pinned"], json!(false));
        assert!(!s.update_project(9, None, None, Some(true)).unwrap(), "项目不存在");
        assert!(s.conn.execute("UPDATE project SET pinned = 2 WHERE id = 1", []).is_err(), "只允许 0 / 1");
        // 0.3.4 的后台（只认到第 2 版）拒绝打开第 3 版的库，不改文件。
        drop(s);
        let before = fs::read(&db).unwrap();
        let mut c = Connection::open(&db).unwrap();
        let err = migrate(&mut c, &db, &MIGRATIONS[..2]).unwrap_err();
        assert!(err.contains("版本 3 高于本程序支持的 2"), "{err}");
        drop(c);
        assert_eq!(fs::read(&db).unwrap(), before);
        let _ = fs::remove_dir_all(&d);
    }

    fn store_with_terminal(name: &str) -> (Store, std::path::PathBuf) {
        let d = test_dir(name);
        let s = Store::open(&d.join("t.db")).unwrap();
        let (p, created) = s.ensure_project("/w/a", 1).unwrap();
        assert!(created);
        assert_eq!(s.ensure_project("/w/a", 2).unwrap(), (p, false));
        s.open_terminal("t1", p, "/w/a", None, 1).unwrap();
        (s, d)
    }

    #[test]
    fn conversation_is_unique_per_tool_and_native_id() {
        let (mut s, d) = store_with_terminal("unique");
        let start = ev(json!({"hook_event_name":"SessionStart","session_id":"x","cwd":"/w/a"}));
        let a = s.record_event("t1", "claude", &start, None, None, 10).unwrap();
        let b = s
            .record_event("t1", "claude", &start, a.conversation, None, 11)
            .unwrap();
        assert_eq!(a.conversation, b.conversation);
        assert!(b.changed.is_empty(), "只有 last_event_at 变化时不推送");
        let c = s.record_event("t1", "codex", &start, None, None, 12).unwrap();
        assert_ne!(a.conversation, c.conversation);
        assert_eq!(s.count("SELECT COUNT(*) FROM conversation"), 2);
        assert_eq!(s.count("SELECT COUNT(*) FROM event"), 3);
        let conv = s.conversation(a.conversation.unwrap()).unwrap().unwrap();
        assert_eq!(
            (conv["cwd"].as_str(), conv["project_id"].as_i64()),
            (Some("/w/a"), Some(1))
        );
        let _ = fs::remove_dir_all(&d);
    }

    /// 写库前脱敏（docs/design.md 10.5 节决策 11）：提示词、Hook 标题、原生标题与最后回复里的密钥都不进库文件。
    #[test]
    fn secrets_are_redacted_before_writing() {
        let (mut s, d) = store_with_terminal("redact");
        let key = "sk-ant-api03-ZyXwVuTsRqPoNmLk";
        let start = ev(json!({"hook_event_name":"SessionStart","session_id":"x","cwd":"/w/a"}));
        let a = s.record_event("t1", "claude", &start, None, None, 10).unwrap();
        let id = a.conversation.unwrap();
        let prompt = ev(json!({"hook_event_name":"UserPromptSubmit","session_id":"x","cwd":"/w/a",
                               "prompt": format!("用 OPENAI_API_KEY={key} 调一下接口，密码：Hunter2!")}));
        s.record_event("t1", "claude", &prompt, Some(id), None, 11).unwrap();
        assert!(s.set_native_title(id, &format!("排查 Bearer {key}")).unwrap());
        assert!(s.set_last_reply(id, &format!("已用 {key} 调通")).unwrap());
        let conv = s.conversation(id).unwrap().unwrap();
        assert_eq!(conv["last_prompt"], "用 OPENAI_API_KEY=[REDACTED] 调一下接口，密码：[REDACTED]");
        assert_eq!(conv["title"], "排查 Bearer [REDACTED]");
        assert_eq!(conv["last_reply"], "已用 [REDACTED] 调通");
        s.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
        drop(s);
        for f in ["t.db", "t.db-wal"] {
            let bytes = fs::read(d.join(f)).unwrap_or_default();
            for secret in [key, "Hunter2!"] {
                assert!(!bytes.windows(secret.len()).any(|w| w == secret.as_bytes()), "{f} 里不应有 {secret}");
            }
        }
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn retention_limits_apply_to_ended_conversations_and_events() {
        let (s, d) = store_with_terminal("retention");
        let now = 1_000 * 86_400_000;
        let old = now - (KEEP_DAYS + 1) * 86_400_000;
        let mut sql = String::from("BEGIN;");
        for i in 0..KEEP_CONVERSATIONS + 5 {
            sql += &format!(
                "INSERT INTO conversation (tool, native_id, started_at, last_event_at, ended_at) \
                 VALUES ('claude', 'n{i}', 0, 0, {});",
                now - 1000 + i
            );
        }
        for i in 0..3 {
            sql += &format!(
                "INSERT INTO conversation (tool, native_id, started_at, last_event_at, ended_at) \
                 VALUES ('codex', 'old{i}', 0, 0, {old});"
            );
        }
        sql += "INSERT INTO conversation (tool, native_id, started_at, last_event_at) \
                VALUES ('kimi', 'active', 0, 0);";
        for i in 0..(KEEP_EVENTS + 10) {
            sql += &format!("INSERT INTO event (kind, at) VALUES ('Stop', {i});");
        }
        sql += "COMMIT;";
        s.exec(&sql);
        let doomed = s.prunable(now, KEEP_DAYS, KEEP_CONVERSATIONS).unwrap();
        assert_eq!(doomed.len(), 8);
        assert_eq!(s.prune_conversations(now, KEEP_DAYS, KEEP_CONVERSATIONS).unwrap(), doomed, "清理的正是预数的那几条");
        assert_eq!(
            s.count("SELECT COUNT(*) FROM conversation WHERE ended_at IS NOT NULL"),
            KEEP_CONVERSATIONS
        );
        assert_eq!(
            s.count("SELECT MIN(ended_at) FROM conversation"),
            now - 1000 + 5
        );
        assert_eq!(
            s.count("SELECT COUNT(*) FROM conversation WHERE ended_at IS NULL"),
            1
        );
        assert_eq!(s.prune_events().unwrap(), 10);
        assert_eq!(s.count("SELECT MIN(at) FROM event"), 10, "保留最新的事件");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn recent_pages_by_end_time_and_project() {
        let (s, d) = store_with_terminal("recent");
        let (p2, _) = s.ensure_project("/w/b", 1).unwrap();
        s.exec(&format!(
            "INSERT INTO conversation (tool, native_id, project_id, last_prompt, started_at, last_event_at, ended_at) VALUES \
             ('claude','a',1,'问',0,0,100), ('claude','b',1,'问',0,0,300), ('claude','c',{p2},'问',0,0,200), \
             ('claude','d',1,'问',0,0,300), ('claude','e',1,'问',0,0,NULL), ('claude','empty',1,NULL,0,0,400);"
        ));
        let (page, next) = s.recent(None, None, None, 2).unwrap();
        let ids: Vec<_> = page
            .iter()
            .map(|c| c["native_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["d", "b"], "同一结束时间按 ID 倒序；从没提问过的 empty 不算");
        let cursor = (
            next["ended_at"].as_i64().unwrap(),
            next["conversation_id"].as_i64().unwrap(),
        );
        let (page, next) = s.recent(None, Some(cursor), None, 2).unwrap();
        let ids: Vec<_> = page
            .iter()
            .map(|c| c["native_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["c", "a"]);
        let cursor = (
            next["ended_at"].as_i64().unwrap(),
            next["conversation_id"].as_i64().unwrap(),
        );
        let (page, next) = s.recent(None, Some(cursor), None, 2).unwrap();
        assert!(page.is_empty() && next.is_null());
        let (page, next) = s.recent(Some(p2), None, None, 10).unwrap();
        assert_eq!((page.len(), next.is_null()), (1, true));
        // since：只取此后结束的。
        let (page, _) = s.recent(None, None, Some(250), 10).unwrap();
        let ids: Vec<_> = page.iter().map(|c| c["native_id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["d", "b"]);
        // 显示窗口：RECENT_DAYS 天之前结束的不计入。
        let now = 150 + RECENT_DAYS * 86_400_000;
        assert_eq!(s.recent_total(now).unwrap(), 3, "100 结束的那条在窗口外");
        let _ = fs::remove_dir_all(&d);
    }
}
