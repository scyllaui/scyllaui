//! 原生会话标题的解析（docs/design.md 3.1 节「会话标题来源」，第 5 节「任务标题」）。只含纯逻辑；
//! 读文件在 scyllauid 的 `title.rs`（平台层）。格式都不是官方承诺，取不到时返回 None，由调用方保留第一条提示词。

use serde_json::Value;

fn non_empty(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Claude Code 会话文件（JSON Lines）中与标题有关的一行记录。
#[derive(Debug, PartialEq, Eq)]
pub enum ClaudeRecord {
    /// `custom-title`（字段 `customTitle`）：用户在 CLI 里改的名。
    Custom(String),
    /// `ai-title`（字段 `aiTitle`）：自动生成的标题。
    Ai(String),
    /// `agent-name`（字段 `agentName`）：守护进程托管的后台会话的代理名。这类会话的 `ai-title` 与代理名相同、
    /// 为短横线英文，不作标题（2026-10-12 本机实测，docs/design.md 3.1 节）。
    AgentName(String),
}

pub fn claude_title_record(line: &str) -> Option<ClaudeRecord> {
    // 会话文件可能有几 MB，先按子串筛掉绝大多数行，不逐行解析 JSON。
    if !line.contains("-title\"") && !line.contains("\"agent-name\"") {
        return None;
    }
    let v: Value = serde_json::from_str(line).ok()?;
    match v["type"].as_str()? {
        "custom-title" => non_empty(&v["customTitle"]).map(ClaudeRecord::Custom),
        "ai-title" => non_empty(&v["aiTitle"]).map(ClaudeRecord::Ai),
        "agent-name" => non_empty(&v["agentName"]).map(ClaudeRecord::AgentName),
        _ => None,
    }
}

/// Codex 的 `session_index.jsonl`：按会话 `id` 取最后一行的 `thread_name`（改名会追加新行）。
pub fn codex_thread_name(index: &str, id: &str) -> Option<String> {
    index
        .lines()
        .filter(|l| l.contains(id))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["id"].as_str() == Some(id))
        .filter_map(|v| non_empty(&v["thread_name"]))
        .last()
}

/// Grok 的 `summary.json`：`generated_title`，没有时取非空的 `session_summary`（2026-10-11 核对：生成过标题的
/// 会话两者相同；未生成时只有空的 `session_summary`）。
pub fn grok_summary_title(summary: &Value) -> Option<String> {
    non_empty(&summary["generated_title"]).or_else(|| non_empty(&summary["session_summary"]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_records() {
        use ClaudeRecord::*;
        assert_eq!(
            claude_title_record(r#"{"type":"ai-title","aiTitle":"排查登录失败","sessionId":"s"}"#),
            Some(Ai("排查登录失败".into()))
        );
        assert_eq!(
            claude_title_record(r#"{"type":"custom-title","customTitle":" 我的任务 ","sessionId":"s"}"#),
            Some(Custom("我的任务".into()))
        );
        assert_eq!(
            claude_title_record(r#"{"type":"agent-name","agentName":"ask-today-weekday"}"#),
            Some(AgentName("ask-today-weekday".into()))
        );
        // 其他记录、空标题、坏行。
        assert_eq!(claude_title_record(r#"{"type":"user","message":{"content":"hi"}}"#), None);
        assert_eq!(claude_title_record(r#"{"type":"ai-title","aiTitle":"  "}"#), None);
        assert_eq!(claude_title_record(r#"{"type":"ai-title","aiTitle"#), None);
        // 正文里提到 ai-title 的普通消息不算。
        assert_eq!(
            claude_title_record(r#"{"type":"assistant","text":"字段 \"ai-title\" 是什么"}"#),
            None
        );
    }

    #[test]
    fn codex_index_takes_last_line_of_the_id() {
        let index = concat!(
            r#"{"id":"01a1-aaaa","thread_name":"确认当前时区","updated_at":"2026-10-11T03:23:26Z"}"#,
            "\n",
            r#"{"id":"01a1-bbbb","thread_name":"别的会话","updated_at":"2026-10-11T04:00:00Z"}"#,
            "\n",
            r#"{"id":"01a1-aaaa","thread_name":"改过的名字","updated_at":"2026-10-11T05:00:00Z"}"#,
            "\n",
            "坏行\n",
        );
        assert_eq!(
            codex_thread_name(index, "01a1-aaaa").as_deref(),
            Some("改过的名字")
        );
        assert_eq!(
            codex_thread_name(index, "01a1-bbbb").as_deref(),
            Some("别的会话")
        );
        assert_eq!(codex_thread_name(index, "01a1-cccc"), None);
        // id 只是另一个 id 的前缀时不算。
        assert_eq!(codex_thread_name(index, "01a1-aaa"), None);
    }

    #[test]
    fn grok_summary_fields() {
        let both = json!({"session_summary":"What Day of the Week Is Today","generated_title":"What Day of the Week Is Today"});
        assert_eq!(
            grok_summary_title(&both).as_deref(),
            Some("What Day of the Week Is Today")
        );
        let only_summary = json!({"session_summary":"Reply With Just OK Request"});
        assert_eq!(
            grok_summary_title(&only_summary).as_deref(),
            Some("Reply With Just OK Request")
        );
        // 尚未生成：只有空的 session_summary。
        assert_eq!(
            grok_summary_title(&json!({"session_summary":"","num_chat_messages":"2"})),
            None
        );
    }
}
