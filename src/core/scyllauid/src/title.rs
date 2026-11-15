//! 读各 CLI 的原生会话标题（docs/design.md 第 5 节「任务标题」，3.1 节「会话标题来源」），以及最后一条 AI 回复
//! （第 5 节「实现（M1 第 6 块）」）、当前模型（「实现（M2 第 2 块）」）与 token、周额度（「实现（M2 第 3 块）」）、推理强度
//! （「实现（维护期 0.3.5）」），同一时机、同一线程。
//! 读文件在 `TitleWorker` 线程里做，核心线程只收结果：文件可能在受保护目录下，首次访问会弹系统授权并阻塞，
//! 与 `GitWorker`（git.rs）同理不能卡住 Hook 处理。解析是适配器里的纯函数（`scyllaui_proto::adapters::title`）。
//! 不轮询、不监听文件：核心在提交提示词、一轮结束、会话结束时请求，可带延迟；同一对话相近的请求合并。

use crate::cliquota::QuotaMiss;
use crate::core::Cmd;
use scyllaui_proto::adapters::title::{
    claude_title_record, codex_thread_name, grok_summary_title, ClaudeRecord,
};
use scyllaui_proto::adapters::reply::{last_reply, opencode_reply, opencode_v2_reply, ReplySource};
use scyllaui_proto::adapters::usage::{
    antigravity_quotas, antigravity_usage_line, claude_usage_line, codex_api, codex_last_usage, copilot_byok, copilot_quota_line, copilot_usage_line,
    ANTIGRAVITY_USAGE_ARGS, grok_summary_effort, grok_summary_model, grok_usage, kimi_api, kimi_usage_line, last_effort, last_model,
    opencode_step_tokens, EffortSource, ModelSource, Quota, Tokens, UsageSource,
};
use scyllaui_proto::adapters::{adapter, valid_session_id, TitleSource};
use scyllaui_proto::Tool;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// 一次读标题与最后一条回复的请求。
pub struct TitleJob {
    pub conversation_id: i64,
    pub tool: Tool,
    pub source: TitleSource,
    pub reply: ReplySource,
    pub model: ModelSource,
    pub usage: UsageSource,
    pub effort: EffortSource,
    pub native_id: String,
    /// 会话记录文件（Hook 的 `transcript_path`）；Kimi 没有，按会话索引找。
    pub transcript_path: Option<String>,
    /// Hook 报的当前模型（核心记着的）：Antigravity 的额度按模型组分，据此取组。
    pub model_hint: Option<String>,
}

/// 一次读到的结果；读不到的为 None，核心保留旧值。
#[derive(Default)]
pub struct Found {
    pub title: Option<String>,
    pub reply: Option<String>,
    pub model: Option<String>,
    pub tokens: Option<Tokens>,
    /// 这款 CLI 账号当前全部窗口的额度：Codex、Copilot 的在会话记录里；Antigravity、Kimi、Grok 另起命令查，见
    /// `Reader::query_quota`。
    pub quotas: Vec<Quota>,
    pub effort: Option<String>,
    /// 是否按量计费（2026-11-01 标题栏额度统一规则）：Codex、Kimi、Copilot、OpenCode 的记录里看得出时为 Some。
    pub api: Option<bool>,
}

/// 读最后一条回复时只看会话记录末尾这么多字节（记录只追加，最新一轮在末尾）。
const REPLY_TAIL: u64 = 1 << 20;

/// 同一对话两次请求的到期时间相差不到这么多时合并为一次。
const MERGE_WINDOW: Duration = Duration::from_secs(1);

pub struct TitleWorker {
    tx: Sender<(Instant, TitleJob)>,
}

impl TitleWorker {
    pub fn new(core: Sender<Cmd>) -> TitleWorker {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        // Codex 的数据目录可由 CODEX_HOME 改（后台继承 App 的环境）。
        let codex = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        let mut reader = Reader::new(home, codex);
        reader.quota_tx = Some(core.clone());
        TitleWorker::with_reader(core, move |job| reader.read(job))
    }

    /// 测试用：注入读取函数（如模拟慢读）。
    pub fn with_reader(
        core: Sender<Cmd>,
        mut read: impl FnMut(&TitleJob) -> Found + Send + 'static,
    ) -> TitleWorker {
        let (tx, rx) = mpsc::channel::<(Instant, TitleJob)>();
        std::thread::spawn(move || {
            let mut pending: Vec<(Instant, TitleJob)> = Vec::new();
            loop {
                let next = pending.iter().map(|(t, _)| *t).min();
                let got = match next {
                    None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                    Some(t) => rx.recv_timeout(t.saturating_duration_since(Instant::now())),
                };
                match got {
                    Ok((due, job)) => {
                        let dup = pending.iter().any(|(t, j)| {
                            j.conversation_id == job.conversation_id
                                && t.max(&due).duration_since(*t.min(&due)) < MERGE_WINDOW
                        });
                        if !dup {
                            pending.push((due, job));
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
                let now = Instant::now();
                let (ready, rest): (Vec<_>, Vec<_>) =
                    pending.drain(..).partition(|(t, _)| *t <= now);
                pending = rest;
                for (_, job) in ready {
                    let Some(got) = crate::guarded("读会话标题", || read(&job)) else { continue };
                    let (id, tool) = (job.conversation_id, job.tool);
                    let cmds = got
                        .title
                        .map(|title| Cmd::TitleRead { conversation_id: id, title })
                        .into_iter()
                        .chain(got.reply.map(|reply| Cmd::ReplyRead { conversation_id: id, reply }))
                        .chain(got.model.map(|model| Cmd::ModelRead { conversation_id: id, model }))
                        .chain(got.effort.map(|effort| Cmd::EffortRead { conversation_id: id, effort }))
                        .chain(got.tokens.map(|tokens| Cmd::TokensRead { conversation_id: id, tokens }))
                        .chain((!got.quotas.is_empty()).then(|| Cmd::QuotaSnapshot { tool, quotas: got.quotas, source: "log" }))
                        .chain(got.api.map(|api| Cmd::BillingRead { conversation_id: id, api }));
                    for cmd in cmds {
                        if core.send(cmd).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        TitleWorker { tx }
    }

    /// 请求在 `delay` 之后读一次。
    pub fn request(&self, job: TitleJob, delay: Duration) {
        let _ = self.tx.send((Instant::now() + delay, job));
    }
}

/// Claude 会话文件读到的位置与已见到的标题（会话文件只追加，按偏移量增量读，不每次从头扫）。
#[derive(Default)]
struct ClaudeFile {
    offset: u64,
    custom: Option<String>,
    ai: Option<String>,
    /// 后台会话的代理名；与之相同的 `ai-title` 不作标题（docs/design.md 3.1 节）。
    agent_names: Vec<String>,
}

/// Claude、Kimi 会话记录里 token 的累计（只追加，按偏移量增量读）；Claude 同一 `message.id` 只算一次。
#[derive(Default)]
struct UsageFile {
    offset: u64,
    tokens: Tokens,
    seen: HashSet<String>,
    any: bool,
    /// 读到的最新一条额度（Copilot 每次调用模型都带账号的月额度快照）。
    quota: Option<Quota>,
    /// 最近一次调用是否用自带 key 的模型（Copilot `isByok`，2026-11-01）。
    byok: Option<bool>,
}

pub struct Reader {
    home: PathBuf,
    codex: PathBuf,
    claude: HashMap<PathBuf, ClaudeFile>,
    usage: HashMap<PathBuf, UsageFile>,
    /// 另起线程查到的额度直接交给核心线程（Antigravity 的 `/usage` 约 4 秒，不在本线程等）；None 时不查。
    quota_tx: Option<Sender<Cmd>>,
    /// 每款上次开始查额度的时间：最多 `QUOTA_EVERY` 一次。
    quota_at: HashMap<Tool, Instant>,
    /// 已记过日志的认不出的 Codex 窗口时长（分钟），同一个值只记一次。
    odd_windows: HashSet<i64>,
}

/// 另起命令查的额度每款最多这么久查一次：每次另起 CLI 进程、由它联网，额度变化慢。
const QUOTA_EVERY: Duration = Duration::from_secs(300);
/// 查额度的命令超过这么久即结束（命令自己的 `--print-timeout` 为 20 秒）。
const AGY_QUOTA_TIMEOUT: Duration = Duration::from_secs(30);

impl Reader {
    pub fn new(home: PathBuf, codex: PathBuf) -> Reader {
        Reader {
            home,
            codex,
            claude: HashMap::new(),
            usage: HashMap::new(),
            quota_tx: None,
            quota_at: HashMap::new(),
            odd_windows: HashSet::new(),
        }
    }

    pub fn read(&mut self, job: &TitleJob) -> Found {
        let (tokens, quotas, api) = self.usage(job);
        let model = self.model(job);
        // Kimi 是否按量计费看会话的模型别名（服务商/模型）。
        let api = api.or_else(|| model.as_deref().filter(|_| job.usage == UsageSource::KimiWire).map(kimi_api));
        Found { title: self.title(job), reply: self.reply(job), model, tokens, quotas, effort: self.effort(job), api }
    }

    /// 推理强度（维护期 0.3.5）：Claude、Codex、Kimi 读会话记录末尾一段，Grok 读 `summary.json`。
    fn effort(&self, job: &TitleJob) -> Option<String> {
        match job.effort {
            EffortSource::None => None,
            EffortSource::GrokSummary => grok_summary_effort(&self.grok_file(&job.native_id, "summary.json")?),
            EffortSource::KimiWire => last_effort(job.effort, &tail(&self.kimi_wire(&job.native_id)?, REPLY_TAIL)?),
            EffortSource::ClaudeTranscript | EffortSource::CodexRollout => {
                last_effort(job.effort, &tail(Path::new(job.transcript_path.as_deref()?), REPLY_TAIL)?)
            }
        }
    }

    /// 会话累计 token 与额度（M2 第 3 块）：Claude、Kimi、Copilot、Antigravity 增量累计，Codex 读 rollout 末尾一段，Grok 读 `usage.json`，
    /// OpenCode 查它的库。额度：Codex 随 rollout、Copilot 随增量读到；Antigravity、Kimi、Grok 另起命令查（2026-11-01）。是否按量
    /// 计费：Codex 看 rollout 第一行的服务商，Copilot 看最近一次调用的 `isByok`，OpenCode 看这个会话的花费（Kimi 在 `read` 里看
    /// 模型别名）；看不出为 None。
    fn usage(&mut self, job: &TitleJob) -> (Option<Tokens>, Vec<Quota>, Option<bool>) {
        let path = || job.transcript_path.as_deref().map(PathBuf::from);
        match job.usage {
            UsageSource::ClaudeTranscript => (path().and_then(|p| self.accumulate(&p, job.usage)), Vec::new(), None),
            UsageSource::KimiWire => {
                self.kimi_quota();
                (self.kimi_wire(&job.native_id).and_then(|p| self.accumulate(&p, job.usage)), Vec::new(), None)
            }
            UsageSource::CodexRollout => {
                let api = path().and_then(|p| first_line(&p)).filter(|v| v["type"] == "session_meta").map(|v| codex_api(&v));
                match path().and_then(|p| tail(&p, REPLY_TAIL)).as_deref().and_then(codex_last_usage) {
                    Some((t, q, odd)) => {
                        for m in odd.into_iter().filter(|m| self.odd_windows.insert(*m)) {
                            log!("读 Codex 额度：认不出时长为 {m} 分钟的窗口，不显示");
                        }
                        (Some(t), q, api)
                    }
                    None => (None, Vec::new(), api),
                }
            }
            UsageSource::GrokUsageJson => {
                self.grok_quota();
                (self.grok_file(&job.native_id, "usage.json").as_ref().and_then(grok_usage), Vec::new(), None)
            }
            UsageSource::CopilotEvents => match self.copilot_record(job) {
                Some(p) => {
                    let tokens = self.accumulate(&p, job.usage);
                    let state = self.usage.get(&p);
                    (tokens, state.and_then(|s| s.quota).into_iter().collect(), state.and_then(|s| s.byok))
                }
                None => (None, Vec::new(), None),
            },
            UsageSource::AntigravityTranscript => {
                self.agy_quota(job.model_hint.clone());
                (path().and_then(|p| self.accumulate(&p, job.usage)), Vec::new(), None)
            }
            UsageSource::OpenCodeDb => match self.opencode(&job.native_id, "用量", opencode_usage) {
                Some((t, cost)) => (Some(t), Vec::new(), Some(cost > 0.0)),
                None => (None, Vec::new(), None),
            },
            UsageSource::None => (None, Vec::new(), None),
        }
    }

    /// Antigravity 的额度（2026-11-01 用户同意间接联网）：执行 `~/.local/bin/agy -p /usage --output-format json`。agy 不在
    /// 官方安装位置时不查。
    fn agy_quota(&mut self, model: Option<String>) {
        let exe = self.home.join(".local/bin/agy");
        if !exe.exists() {
            return;
        }
        let (home, detail) = (self.home.clone(), format!("，模型 {}", model.as_deref().unwrap_or("-")));
        self.query_quota(Tool::Antigravity, detail, move || {
            let out = run_quiet(&exe, &ANTIGRAVITY_USAGE_ARGS, &home, AGY_QUOTA_TIMEOUT).ok_or("命令失败或超时")?;
            let v = serde_json::from_slice::<serde_json::Value>(&out).map_err(|_| "有输出但认不出")?;
            Ok(antigravity_quotas(&v, model.as_deref(), scyllaui_proto::now_ms()))
        });
    }

    /// Kimi 的额度（同上，2026-11-01）：起 `~/.kimi-code/bin/kimi web` 读它的用量接口（cliquota.rs）。
    fn kimi_quota(&mut self) {
        let exe = self.home.join(".kimi-code/bin/kimi");
        if !exe.exists() {
            return;
        }
        let home = self.home.clone();
        self.query_quota(Tool::Kimi, String::new(), move || crate::cliquota::kimi_web(&exe, &home));
    }

    /// Grok 的额度（同上，2026-11-01）：起 `~/.grok/bin/grok agent stdio` 调 `_x.ai/billing`（cliquota.rs）。
    fn grok_quota(&mut self) {
        let exe = self.home.join(".grok/bin/grok");
        if !exe.exists() {
            return;
        }
        let home = self.home.clone();
        self.query_quota(Tool::Grok, String::new(), move || crate::cliquota::grok_billing(&exe, &home));
    }

    /// 另起线程执行 `query` 查一款的额度，读到的整份交给核心线程；每款最多 5 分钟一次，本线程不等。读不到记一行日志、不报错；
    /// 有能告诉用户的原因时（如 Grok 没登录 grok.com）交给核心推送 `quota.note`。`detail` 接在日志的耗时后面。
    fn query_quota(&mut self, tool: Tool, detail: String,
                   query: impl FnOnce() -> Result<Vec<Quota>, QuotaMiss> + Send + 'static) {
        let Some(tx) = self.quota_tx.clone() else { return };
        if self.quota_at.get(&tool).is_some_and(|t| t.elapsed() < QUOTA_EVERY) {
            return;
        }
        self.quota_at.insert(tool, Instant::now());
        std::thread::spawn(move || {
            let started = Instant::now();
            let got = query().and_then(|q| if q.is_empty() { Err(QuotaMiss::from("返回里没有认得的额度")) } else { Ok(q) });
            let ms = started.elapsed().as_millis();
            match &got {
                Ok(q) => log!("读 {tool:?} 额度：{}（{ms} ms{detail}）", quota_summary(q)),
                Err(m) => log!("读 {tool:?} 额度：没有读到（{ms} ms，{}{detail}）", m.why),
            }
            let cmd = match got {
                Ok(quotas) => Cmd::QuotaSnapshot { tool, quotas, source: "command" },
                Err(QuotaMiss { note: Some(note), .. }) => Cmd::QuotaNote { tool, note },
                Err(_) => return,
            };
            let _ = tx.send(cmd);
        });
    }

    /// 从上次读到的位置读新增的完整行并累计；文件变短（被替换）时从头再来。一条用量记录都没有时为 None。
    fn accumulate(&mut self, path: &Path, source: UsageSource) -> Option<Tokens> {
        let state = self.usage.entry(path.to_path_buf()).or_default();
        let mut f = File::open(path).ok()?;
        if f.metadata().ok()?.len() < state.offset {
            *state = UsageFile::default();
        }
        f.seek(SeekFrom::Start(state.offset)).ok()?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).ok()?;
        let end = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        for line in buf[..end].split(|&b| b == b'\n') {
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else { continue };
            let t = match source {
                UsageSource::ClaudeTranscript => claude_usage_line(&v).and_then(|(id, t)| state.seen.insert(id).then_some(t)),
                UsageSource::CopilotEvents => {
                    if let Some(q) = copilot_quota_line(&v) {
                        state.quota = Some(q);
                    }
                    if let Some(b) = copilot_byok(&v) {
                        state.byok = Some(b);
                    }
                    copilot_usage_line(&v)
                }
                UsageSource::AntigravityTranscript => antigravity_usage_line(&v),
                _ => kimi_usage_line(&v),
            };
            if let Some(t) = t {
                state.tokens.add(&t);
                state.any = true;
            }
        }
        state.offset += end as u64;
        state.any.then_some(state.tokens)
    }

    /// 当前模型：Claude、Kimi、Copilot 读会话记录末尾一段，Grok 读 `summary.json`（M2 第 2 块）。
    fn model(&self, job: &TitleJob) -> Option<String> {
        match job.model {
            ModelSource::None => None,
            ModelSource::GrokSummary => grok_summary_model(&self.grok_file(&job.native_id, "summary.json")?),
            ModelSource::KimiWire => last_model(job.model, &tail(&self.kimi_wire(&job.native_id)?, REPLY_TAIL)?),
            ModelSource::CopilotEvents => last_model(job.model, &tail(&self.copilot_record(job)?, REPLY_TAIL)?),
            ModelSource::OpenCodeDb => self.opencode(&job.native_id, "模型", opencode_model),
            ModelSource::ClaudeTranscript => {
                last_model(job.model, &tail(Path::new(job.transcript_path.as_deref()?), REPLY_TAIL)?)
            }
        }
    }

    /// Grok 会话目录里的 JSON 文件（`summary.json`、`usage.json`）：会话目录在「编码后的启动目录」下，编码方式随版本变化
    /// （3.1 节），按会话 ID 在各目录下找。
    fn grok_file(&self, native_id: &str, name: &str) -> Option<serde_json::Value> {
        let p = grok_session_dir(&self.home, native_id)?.join(name);
        fs::read(p).ok().and_then(|b| serde_json::from_slice(&b).ok())
    }

    /// 最后一条 AI 回复：只读会话记录末尾一段（`REPLY_TAIL`）的完整行；OpenCode 查它的库。
    fn reply(&self, job: &TitleJob) -> Option<String> {
        let path = match job.reply {
            ReplySource::None => return None,
            ReplySource::OpenCodeDb => return self.opencode(&job.native_id, "回复", opencode_last_reply),
            ReplySource::KimiWire => self.kimi_wire(&job.native_id)?,
            ReplySource::CopilotEvents => self.copilot_record(job)?,
            _ => PathBuf::from(job.transcript_path.as_deref()?),
        };
        last_reply(job.reply, &tail(&path, REPLY_TAIL)?)
    }

    /// Copilot 的会话记录：适配器按会话 ID 推出的 `events.jsonl`（同提示词足迹）。
    fn copilot_record(&self, job: &TitleJob) -> Option<PathBuf> {
        let rel = adapter(job.tool).session_record?;
        valid_session_id(&job.native_id).then(|| self.home.join(rel(&job.native_id)))
    }

    /// 只读查询 OpenCode 库。库或会话不在为 None；查询出错（库被占用太久、表结构变了）记一行日志，同样为 None，保留旧值。
    fn opencode<T>(&self, session: &str, what: &str, query: fn(&rusqlite::Connection, &str) -> rusqlite::Result<Option<T>>) -> Option<T> {
        let got = crate::prompts::open_opencode(&self.home).and_then(|c| match c {
            Some(conn) => query(&conn, session),
            None => Ok(None),
        });
        got.unwrap_or_else(|e| {
            log!("读 OpenCode 的{what}：库查询失败（{session}）：{e}");
            None
        })
    }

    /// Kimi 的会话目录：`~/.kimi-code/session_index.jsonl` 中该会话最后一行的 `sessionDir`（2026-10-13 本机核对）。
    fn kimi_wire(&self, native_id: &str) -> Option<PathBuf> {
        Some(kimi_session_dir(&self.home, native_id)?.join("agents/main/wire.jsonl"))
    }

    fn title(&mut self, job: &TitleJob) -> Option<String> {
        match job.source {
            TitleSource::TranscriptRecords => {
                self.claude(Path::new(job.transcript_path.as_deref()?))
            }
            TitleSource::CodexSessionIndex => codex_thread_name(
                &fs::read_to_string(self.codex.join("session_index.jsonl")).ok()?,
                &job.native_id,
            ),
            TitleSource::GrokSummary => grok_summary_title(&self.grok_file(&job.native_id, "summary.json")?),
            TitleSource::AntigravitySummaries => antigravity_title(&self.home, &job.native_id),
            // Hook 自带标题（Kimi），不读文件。
            TitleSource::HookField => None,
        }
    }

    fn claude(&mut self, path: &Path) -> Option<String> {
        let state = self.claude.entry(path.to_path_buf()).or_default();
        let mut f = File::open(path).ok()?;
        let len = f.metadata().ok()?.len();
        if len < state.offset {
            // 文件被截短或替换：从头再读。
            *state = ClaudeFile::default();
        }
        f.seek(SeekFrom::Start(state.offset)).ok()?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).ok()?;
        // 只处理完整的行；末尾未写完的半行留到下次。
        let end = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        for line in String::from_utf8_lossy(&buf[..end]).lines() {
            match claude_title_record(line) {
                Some(ClaudeRecord::Custom(t)) => state.custom = Some(t),
                Some(ClaudeRecord::Ai(t)) => state.ai = Some(t),
                Some(ClaudeRecord::AgentName(t)) => state.agent_names.push(t),
                None => {}
            }
        }
        state.offset += end as u64;
        let ai = state.ai.clone().filter(|a| !state.agent_names.contains(a));
        state.custom.clone().or(ai)
    }
}

/// OpenCode 这个会话最近一条有文字的助手消息（只看最近 50 条消息；只调了工具的往前找）。
fn opencode_last_reply(conn: &rusqlite::Connection, session: &str) -> rusqlite::Result<Option<String>> {
    if crate::prompts::opencode_v2(conn, session)? {
        let mut messages = conn.prepare(
            "SELECT data FROM session_message WHERE session_id = ?1 AND type = 'assistant' ORDER BY seq DESC LIMIT 50",
        )?;
        for data in messages.query_map([session], |r| r.get::<_, String>(0))? {
            let reply = serde_json::from_str::<serde_json::Value>(&data?).ok().and_then(|d| opencode_v2_reply(&d));
            if reply.is_some() {
                return Ok(reply);
            }
        }
        return Ok(None);
    }
    let mut messages = conn.prepare("SELECT id, data FROM message WHERE session_id = ?1 ORDER BY time_created DESC, id DESC LIMIT 50")?;
    let rows = messages
        .query_map([session], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut parts = conn.prepare("SELECT data FROM part WHERE message_id = ?1 ORDER BY id")?;
    for (id, data) in rows {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&data) else { continue };
        if message["role"] != "assistant" {
            continue;
        }
        let list: Vec<serde_json::Value> = parts
            .query_map([&id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .filter_map(|d| serde_json::from_str(d).ok())
            .collect();
        if let Some(text) = opencode_reply(&message, &list) {
            return Ok(Some(text));
        }
    }
    Ok(None)
}

/// OpenCode 这个会话累计的 token：OpenCode 2 直接读 `session_v2` 的累计列（`input` 不含缓存、推理算进输出，与 OpenCode 1
/// 同口径，2026-10-31 本机 2.2.4 与 `session.usage.updated` 事件核对一致）；OpenCode 1 为每一步结束的段相加。一步都没有为 None。
/// 会话累计 token 与花费（花费大于 0 即按量计费的依据，2026-11-01；本机免费模型为 0）。
fn opencode_usage(conn: &rusqlite::Connection, session: &str) -> rusqlite::Result<Option<(Tokens, f64)>> {
    if crate::prompts::opencode_v2(conn, session)? {
        let (input, output, reasoning, read, write): (i64, i64, i64, i64, i64) = conn.query_row(
            "SELECT tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write FROM session_v2 WHERE id = ?1",
            [session],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        // 花费另查：没有这一列的版本照样读 token（2026-11-01 本机 2.2.5 有 `cost`）。
        let cost: f64 = conn.query_row("SELECT cost FROM session_v2 WHERE id = ?1", [session], |r| r.get(0)).unwrap_or(0.0);
        let n = |v: i64| v.max(0) as u64;
        let t = Tokens { input: n(input), cache_read: n(read), cache_write: n(write), output: n(output) + n(reasoning),
                         total: n(input) + n(read) + n(write) + n(output) + n(reasoning) };
        return Ok((t.total > 0).then_some((t, cost)));
    }
    let mut steps = conn.prepare("SELECT data FROM part WHERE session_id = ?1 AND json_extract(data, '$.type') = 'step-finish'")?;
    let mut sum: Option<(Tokens, f64)> = None;
    for data in steps.query_map([session], |r| r.get::<_, String>(0))? {
        let step = serde_json::from_str::<serde_json::Value>(&data?).ok();
        if let Some(t) = step.as_ref().and_then(opencode_step_tokens) {
            let s = sum.get_or_insert_with(|| (Tokens::default(), 0.0));
            s.0.add(&t);
            s.1 += step.as_ref().and_then(|v| v["cost"].as_f64()).unwrap_or(0.0);
        }
    }
    Ok(sum)
}

/// 文件的第一行（Codex rollout 的 `session_meta`，带完整的说明文字，最多读 1 MiB）。
fn first_line(path: &Path) -> Option<serde_json::Value> {
    let mut line = String::new();
    BufReader::new(File::open(path).ok()?.take(1 << 20)).read_line(&mut line).ok()?;
    serde_json::from_str(&line).ok()
}

/// OpenCode 这个会话的当前模型：OpenCode 2 为 `session_v2.model`（`{"id", "providerID"}`）的 `id`；OpenCode 1 为最近一条
/// 助手消息的 `modelID`。
fn opencode_model(conn: &rusqlite::Connection, session: &str) -> rusqlite::Result<Option<String>> {
    let data: Option<String> = if crate::prompts::opencode_v2(conn, session)? {
        conn.query_row("SELECT model FROM session_v2 WHERE id = ?1", [session], |r| r.get(0))?
    } else {
        let mut q = conn.prepare(
            "SELECT data FROM message WHERE session_id = ?1 AND json_extract(data, '$.role') = 'assistant' ORDER BY time_created DESC, id DESC LIMIT 1",
        )?;
        let mut rows = q.query_map([session], |r| r.get::<_, String>(0))?;
        rows.next().transpose()?
    };
    let v = data.and_then(|d| serde_json::from_str::<serde_json::Value>(&d).ok());
    Ok(v.and_then(|v| v["id"].as_str().or(v["modelID"].as_str()).filter(|m| !m.is_empty()).map(str::to_owned)))
}

/// 文件末尾不超过 `max` 字节中的完整行（从中间开始时丢掉第一段残行，末尾未写完的半行也不要）。
/// Grok 的会话目录：`~/.grok/sessions/<编码后的启动目录>/<会话 ID>/`。编码方式随版本变化（3.1 节），按会话 ID 在各目录下找；
/// 会话 ID 只取最后一段，不能借路径跳出 sessions 目录。
/// Antigravity 生成的对话标题：`~/.gemini/antigravity-cli/conversation_summaries.db` 中这个对话的 `title`（第一轮之后才有，
/// 2026-11-01 本机 1.4.12 只读核对，表结构非官方承诺）。没有或查询出错为 None，由调用方保留旧值。
fn antigravity_title(home: &Path, native_id: &str) -> Option<String> {
    let conn = crate::waitwatch::open_sqlite_ro(&home.join(".gemini/antigravity-cli/conversation_summaries.db")).ok()??;
    let title: String = conn
        .query_row("SELECT title FROM conversation_summaries WHERE conversation_id = ?1", [native_id], |r| r.get(0))
        .ok()?;
    let title = title.trim();
    (!title.is_empty()).then(|| title.to_owned())
}

/// 日志里的额度摘要：「5 小时已用 0%，7 天已用 26%」；没有限制的窗口不写。
fn quota_summary(quotas: &[Quota]) -> String {
    use scyllaui_proto::adapters::usage::Window;
    let parts: Vec<String> = quotas.iter().filter(|q| !q.unlimited).map(|q| {
        let name = match q.window {
            Window::FiveHour => "5 小时",
            Window::SevenDay => "7 天",
            Window::Month => "本月",
        };
        format!("{name}已用 {:.0}%", q.used_percent)
    }).collect();
    if parts.is_empty() { "没有限制".to_owned() } else { parts.join("，") }
}

/// 执行命令取标准输出：环境只留 HOME 与最小 PATH，标准输入为空，在家目录下运行，超时即结束进程；退出码非零为 None。
fn run_quiet(exe: &Path, args: &[&str], home: &Path, timeout: Duration) -> Option<Vec<u8>> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(exe)
        .args(args)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let out = reader.join().ok()?;
    status.filter(|s| s.success()).map(|_| out)
}

pub fn grok_session_dir(home: &Path, native_id: &str) -> Option<PathBuf> {
    let id = Path::new(native_id).file_name()?;
    fs::read_dir(home.join(".grok/sessions")).ok()?.flatten().map(|e| e.path().join(id)).find(|p| p.is_dir())
}

/// Kimi 的会话目录：`~/.kimi-code/session_index.jsonl` 中该会话最后一行的 `sessionDir`（2026-10-13 本机核对）。
pub fn kimi_session_dir(home: &Path, native_id: &str) -> Option<PathBuf> {
    let index = fs::read_to_string(home.join(".kimi-code/session_index.jsonl")).ok()?;
    let dir = index
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["sessionId"] == native_id)
        .last()?["sessionDir"]
        .as_str()?
        .to_owned();
    Some(match dir.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(dir),
    })
}

pub(crate) fn tail(path: &Path, max: u64) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(max);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let begin = if start > 0 { buf.iter().position(|&b| b == b'\n').map_or(buf.len(), |i| i + 1) } else { 0 };
    let end = buf.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1).max(begin);
    Some(String::from_utf8_lossy(&buf[begin..end]).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_dir;
    use scyllaui_proto::adapters::usage::Window;
    use std::io::Write;

    fn job(source: TitleSource, native_id: &str, transcript: Option<&Path>) -> TitleJob {
        TitleJob {
            conversation_id: 1,
            tool: Tool::Claude,
            source,
            reply: ReplySource::ClaudeTranscript,
            model: ModelSource::ClaudeTranscript,
            usage: UsageSource::ClaudeTranscript,
            effort: EffortSource::ClaudeTranscript,
            native_id: native_id.into(),
            transcript_path: transcript.map(|p| p.to_string_lossy().into_owned()),
            model_hint: None,
        }
    }

    #[test]
    fn reads_each_source_from_files() {
        let home = test_dir("title-files");
        let mut r = Reader::new(home.clone(), home.join(".codex"));
        // Claude：增量读；用户改名优先于自动标题；半行等下次。
        let t = home.join("t.jsonl");
        fs::write(
            &t,
            "{\"type\":\"user\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"自动标题\"}\n",
        )
        .unwrap();
        let claude = job(TitleSource::TranscriptRecords, "c", Some(&t));
        assert_eq!(r.title(&claude).as_deref(), Some("自动标题"));
        let mut f = fs::OpenOptions::new().append(true).open(&t).unwrap();
        write!(f, "{{\"type\":\"custom-title\",\"customTitle\":\"我改的").unwrap();
        assert_eq!(r.title(&claude).as_deref(), Some("自动标题"), "半行不算");
        writeln!(f, "\"}}").unwrap();
        writeln!(
            f,
            "{{\"type\":\"ai-title\",\"aiTitle\":\"更新的自动标题\"}}"
        )
        .unwrap();
        assert_eq!(r.title(&claude).as_deref(), Some("我改的"));
        // 后台会话：ai-title 与代理名相同（短横线英文）不作标题；与代理名不同的仍算。
        let bg = home.join("bg.jsonl");
        fs::write(
            &bg,
            "{\"type\":\"ai-title\",\"aiTitle\":\"ask-today-weekday\"}\n{\"type\":\"agent-name\",\"agentName\":\"ask-today-weekday\"}\n",
        )
        .unwrap();
        let bg_job = job(TitleSource::TranscriptRecords, "b", Some(&bg));
        assert_eq!(r.title(&bg_job), None);
        let mut f = fs::OpenOptions::new().append(true).open(&bg).unwrap();
        writeln!(f, "{{\"type\":\"ai-title\",\"aiTitle\":\"控制技能的启用与关闭\"}}").unwrap();
        assert_eq!(r.title(&bg_job).as_deref(), Some("控制技能的启用与关闭"));
        // 没有文件、没有记录。
        assert_eq!(
            r.title(&job(
                TitleSource::TranscriptRecords,
                "c",
                Some(&home.join("无"))
            )),
            None
        );
        // Codex。
        fs::create_dir_all(home.join(".codex")).unwrap();
        fs::write(
            home.join(".codex/session_index.jsonl"),
            "{\"id\":\"01a1\",\"thread_name\":\"确认当前时区\",\"updated_at\":\"x\"}\n",
        )
        .unwrap();
        assert_eq!(
            r.title(&job(TitleSource::CodexSessionIndex, "01a1", None))
                .as_deref(),
            Some("确认当前时区")
        );
        // Grok：目录名是编码后的启动目录。
        let g = home.join(".grok/sessions/%2FUsers%2Fu/01a2");
        fs::create_dir_all(&g).unwrap();
        fs::write(g.join("summary.json"), r#"{"session_summary":"What Day of the Week Is Today","generated_title":"What Day of the Week Is Today"}"#).unwrap();
        assert_eq!(
            r.title(&job(TitleSource::GrokSummary, "01a2", None))
                .as_deref(),
            Some("What Day of the Week Is Today")
        );
        assert_eq!(r.title(&job(TitleSource::GrokSummary, "01a3", None)), None);
        // 会话 ID 不能借路径跳出 sessions 目录。
        assert_eq!(r.title(&job(TitleSource::GrokSummary, "../x", None)), None);
        assert_eq!(r.title(&job(TitleSource::HookField, "k", None)), None);
        let _ = fs::remove_dir_all(&home);
    }

    /// 最后一条回复：只读末尾一段；Kimi 按会话索引找到会话目录。
    #[test]
    fn reads_last_reply_from_the_tail() {
        let home = test_dir("reply-files");
        let r = Reader::new(home.clone(), home.join(".codex"));
        let t = home.join("c.jsonl");
        let old = format!("{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"{}\"}}]}}}}\n", "旧".repeat(200));
        let new = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"主人，已完成。\"}]}}\n";
        fs::write(&t, format!("{old}{new}{{\"type\":\"assistant\",\"message\"")).unwrap();
        let mut j = job(TitleSource::TranscriptRecords, "c", Some(&t));
        assert_eq!(r.reply(&j).as_deref(), Some("主人，已完成。"), "末尾半行不算");
        assert_eq!(tail(&t, (new.len() + 30) as u64).as_deref(), Some(new), "从中间开始时丢掉残行");
        let d = home.join(".kimi-code/sessions/wd_p_1/session_k");
        fs::create_dir_all(d.join("agents/main")).unwrap();
        fs::write(home.join(".kimi-code/session_index.jsonl"),
            format!("{{\"sessionId\":\"session_k\",\"sessionDir\":\"{}\",\"workDir\":\"/w\"}}\n", d.display())).unwrap();
        fs::write(d.join("agents/main/wire.jsonl"),
            "{\"type\":\"context.append_loop_event\",\"event\":{\"type\":\"content.part\",\"part\":{\"type\":\"text\",\"text\":\"主人，查证完成\"}}}\n").unwrap();
        j.reply = ReplySource::KimiWire;
        j.native_id = "session_k".into();
        j.transcript_path = None;
        assert_eq!(r.reply(&j).as_deref(), Some("主人，查证完成"));
        j.native_id = "session_none".into();
        assert_eq!(r.reply(&j), None);
        // 当前模型（M2 第 2 块）：Claude 读末尾的 assistant 记录；Kimi 读 usage.record；Grok 读 summary.json。
        let mut m = job(TitleSource::TranscriptRecords, "c", Some(&t));
        fs::write(&t, "{\"type\":\"assistant\",\"message\":{\"model\":\"claude-opus-5-5\"}}\n").unwrap();
        assert_eq!(r.model(&m).as_deref(), Some("claude-opus-5-5"));
        fs::write(d.join("agents/main/wire.jsonl"), "{\"type\":\"usage.record\",\"model\":\"kimi-code/kimi-for-coding\"}\n").unwrap();
        m.model = ModelSource::KimiWire;
        m.native_id = "session_k".into();
        assert_eq!(r.model(&m).as_deref(), Some("kimi-code/kimi-for-coding"));
        let g = home.join(".grok/sessions/%2Fw/g1");
        fs::create_dir_all(&g).unwrap();
        fs::write(g.join("summary.json"), r#"{"current_model_id":"grok-4.7"}"#).unwrap();
        m.model = ModelSource::GrokSummary;
        m.native_id = "g1".into();
        assert_eq!(r.model(&m).as_deref(), Some("grok-4.7"));
        m.model = ModelSource::None;
        assert_eq!(r.model(&m), None);
        // 推理强度（维护期 0.3.5）：Claude、Codex 读 transcript 末尾，Kimi 按会话索引找 wire.jsonl，Grok 读 summary.json。
        let mut e = job(TitleSource::TranscriptRecords, "c", Some(&t));
        fs::write(&t, "{\"type\":\"assistant\",\"effort\":\"high\"}\n{\"type\":\"assistant\",\"effort\":\"xhigh\"}\n").unwrap();
        assert_eq!(r.effort(&e).as_deref(), Some("xhigh"));
        fs::write(&t, "{\"type\":\"turn_context\",\"payload\":{\"effort\":\"medium\"}}\n").unwrap();
        e.effort = EffortSource::CodexRollout;
        assert_eq!(r.effort(&e).as_deref(), Some("medium"));
        fs::write(d.join("agents/main/wire.jsonl"), "{\"type\":\"llm.request\",\"thinkingEffort\":\"max\"}\n").unwrap();
        e.effort = EffortSource::KimiWire;
        e.native_id = "session_k".into();
        assert_eq!(r.effort(&e).as_deref(), Some("max"));
        fs::write(g.join("summary.json"), r#"{"current_model_id":"grok-4.7","reasoning_effort":"low"}"#).unwrap();
        e.effort = EffortSource::GrokSummary;
        e.native_id = "g1".into();
        assert_eq!(r.effort(&e).as_deref(), Some("low"));
        e.effort = EffortSource::None;
        assert_eq!(r.effort(&e), None);
        let _ = fs::remove_dir_all(&home);
    }

    /// Token（M2 第 3 块）：Claude 按 message.id 去重并增量累计、半行等下次、文件变短从头再算；Kimi 逐轮相加；
    /// Codex 取末尾最后一条 token_count 与 7 天额度；Grok 读 usage.json；没有记录为 None。
    #[test]
    fn reads_tokens_from_each_source() {
        let home = test_dir("usage-files");
        let mut r = Reader::new(home.clone(), home.join(".codex"));
        let t = home.join("c.jsonl");
        let line = |id: &str, out: u64| format!("{{\"type\":\"assistant\",\"message\":{{\"id\":\"{id}\",\"usage\":{{\"input_tokens\":2,\"cache_creation_input_tokens\":10,\"cache_read_input_tokens\":100,\"output_tokens\":{out}}}}}}}\n");
        fs::write(&t, format!("{{\"type\":\"user\"}}\n{}{}", line("m1", 5), line("m1", 5))).unwrap();
        let mut j = job(TitleSource::TranscriptRecords, "c", Some(&t));
        assert_eq!(r.usage(&j).0, Some(Tokens { input: 2, cache_read: 100, cache_write: 10, output: 5, total: 117 }), "同一消息两行只算一次");
        let mut f = fs::OpenOptions::new().append(true).open(&t).unwrap();
        let l2 = line("m2", 1);
        write!(f, "{}", &l2[..20]).unwrap();
        assert_eq!(r.usage(&j).0.map(|t| t.total), Some(117), "半行不算");
        write!(f, "{}", &l2[20..]).unwrap();
        assert_eq!(r.usage(&j).0.map(|t| t.total), Some(117 + 113));
        fs::write(&t, line("m3", 0)).unwrap();
        assert_eq!(r.usage(&j).0.map(|t| t.total), Some(112), "文件变短：从头再算");
        fs::write(&t, "{\"type\":\"user\"}\n").unwrap();
        assert_eq!(r.usage(&j).0, None, "还没有用量记录");
        // Kimi。
        let d = home.join(".kimi-code/sessions/wd/session_k");
        fs::create_dir_all(d.join("agents/main")).unwrap();
        fs::write(home.join(".kimi-code/session_index.jsonl"),
            format!("{{\"sessionId\":\"session_k\",\"sessionDir\":\"{}\"}}\n", d.display())).unwrap();
        let k = "{\"type\":\"usage.record\",\"usage\":{\"inputOther\":10,\"output\":2,\"inputCacheRead\":30,\"inputCacheCreation\":0}}\n";
        fs::write(d.join("agents/main/wire.jsonl"), k.repeat(2)).unwrap();
        j.usage = UsageSource::KimiWire;
        j.native_id = "session_k".into();
        assert_eq!(r.usage(&j).0, Some(Tokens { input: 20, cache_read: 60, cache_write: 0, output: 4, total: 84 }));
        // Codex。
        let roll = home.join("rollout.jsonl");
        fs::write(&roll, "{\"timestamp\":\"2026-10-15T13:31:42.250Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":{\"total_token_usage\":{\"input_tokens\":30,\"cached_input_tokens\":10,\"output_tokens\":5,\"total_tokens\":35}},\"rate_limits\":{\"primary\":{\"used_percent\":3.0,\"window_minutes\":10080,\"resets_at\":1792675611}}}}\n").unwrap();
        j.usage = UsageSource::CodexRollout;
        j.transcript_path = Some(roll.to_string_lossy().into_owned());
        let (tk, q, api) = r.usage(&j);
        assert_eq!(tk, Some(Tokens { input: 20, cache_read: 10, cache_write: 0, output: 5, total: 35 }));
        assert_eq!(q.iter().map(|q| (q.window.as_str(), q.used_percent)).collect::<Vec<_>>(), [("7d", 3.0)], "没有的 5 小时不补");
        assert_eq!(api, None, "第一行不是 session_meta：看不出");
        // rollout 第一行的服务商：别家为按量计费，openai 不是；认不出的窗口时长不收。
        let meta = |p: &str| format!("{{\"type\":\"session_meta\",\"payload\":{{\"model_provider\":\"{p}\"}}}}\n");
        let body = fs::read_to_string(&roll).unwrap();
        fs::write(&roll, meta("openrouter") + &body.replace("10080", "1440")).unwrap();
        let (_, q, api) = r.usage(&j);
        assert_eq!((q.len(), api), (0, Some(true)));
        fs::write(&roll, meta("openai") + &body).unwrap();
        assert_eq!(r.usage(&j).2, Some(false));
        // Grok。
        let g = home.join(".grok/sessions/%2Fw/g1");
        fs::create_dir_all(&g).unwrap();
        fs::write(g.join("usage.json"), r#"{"session":{"inputTokens":50,"outputTokens":3,"cachedReadTokens":40,"cacheCreationTokens":0,"totalTokens":53}}"#).unwrap();
        j.usage = UsageSource::GrokUsageJson;
        j.native_id = "g1".into();
        assert_eq!(r.usage(&j).0.map(|t| (t.input, t.total)), Some((10, 53)));
        j.native_id = "g2".into();
        assert_eq!(r.usage(&j), (None, Vec::new(), None));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn delays_and_merges_requests() {
        let (tx, rx) = mpsc::channel();
        let w = TitleWorker::with_reader(tx, |j| Found { title: Some(format!("标题{}", j.conversation_id)), ..Found::default() });
        let mk = |id| TitleJob {
            conversation_id: id,
            tool: Tool::Grok,
            source: TitleSource::GrokSummary,
            reply: ReplySource::GrokUpdates,
            model: ModelSource::None,
            usage: UsageSource::GrokUsageJson,
            effort: EffortSource::None,
            native_id: "x".into(),
            transcript_path: None,
            model_hint: None,
        };
        let start = Instant::now();
        w.request(mk(7), Duration::from_millis(300));
        w.request(mk(7), Duration::from_millis(350)); // 与上一条合并
        w.request(mk(8), Duration::ZERO);
        let mut got = Vec::new();
        while let Ok(Cmd::TitleRead {
            conversation_id,
            title,
        }) = rx.recv_timeout(Duration::from_secs(2))
        {
            got.push((conversation_id, title, start.elapsed()));
        }
        assert_eq!(got.len(), 2, "同一对话相近的请求只读一次");
        assert_eq!(got[0].0, 8);
        assert_eq!(got[1].0, 7);
        assert!(got[1].2 >= Duration::from_millis(300), "延迟到期才读");
    }

    /// Copilot 读 `events.jsonl` 的回复、模型与逐次累计的 token；OpenCode 只读查询它的库（2026-10-31 起，结构同本机
    /// 1.0.105、1.19.5）。库不在、表结构不对都按读不到处理，不报错。
    /// Antigravity 的额度（2026-11-01）：读用量时另起线程执行 `~/.local/bin/agy -p /usage --output-format json`，读到的
    /// 7 天额度（与「没有 5 小时限制」）交给核心线程；5 分钟内不再查；命令失败什么也不发。这里用打印固定 JSON 的脚本代替 agy。
    #[test]
    fn antigravity_quota_command() {
        let home = test_dir("title-agy-quota");
        let bin = home.join(".local/bin");
        fs::create_dir_all(&bin).unwrap();
        let out = r#"{"command":{"name":"usage","data":{"groups":[{"name":"Gemini Models","buckets":[{"window":"weekly","remaining_fraction":0.75,"reset_time":"2026-11-08T06:38:54Z"}]}]}}}"#;
        let agy = bin.join("agy");
        fs::write(&agy, format!("#!/bin/sh\n[ \"$1 $2 $3 $4\" = '-p /usage --output-format json' ] || exit 3\nprintf '%s' '{out}'\n")).unwrap();
        fs::set_permissions(&agy, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let (tx, rx) = mpsc::channel();
        let mut r = Reader::new(home.clone(), home.join(".codex"));
        r.quota_tx = Some(tx);
        let j = TitleJob { tool: Tool::Antigravity, usage: UsageSource::AntigravityTranscript, model_hint: Some("gemini-3.8-flash-low".into()),
                           ..job(TitleSource::AntigravitySummaries, "c1", None) };
        r.read(&j);
        let got: Vec<Quota> = match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Cmd::QuotaSnapshot { tool: Tool::Antigravity, quotas, source: "command" }) => quotas,
            _ => Vec::new(),
        };
        assert_eq!(got.iter().map(|q| (q.window, q.used_percent)).collect::<Vec<_>>(), [(Window::SevenDay, 25.0)], "只有 7 天，整份一次发");
        r.read(&j);
        assert!(rx.recv_timeout(Duration::from_millis(500)).is_err(), "5 分钟内不再查");
        fs::write(&agy, "#!/bin/sh\nexit 1\n").unwrap();
        r.quota_at.clear();
        r.read(&j);
        assert!(rx.recv_timeout(Duration::from_millis(800)).is_err(), "命令失败什么也不发");
        let _ = fs::remove_dir_all(&home);
    }

    /// 假的 `kimi web`：起本机服务、打印地址后回一次用量；假的 `grok agent stdio`：按 ACP 回初始化与 `_x.ai/billing`。
    /// 用 python3 写（测试辅助，不进产品运行时）。
    #[test]
    fn kimi_and_grok_quota_commands() {
        let home = test_dir("title-kimi-grok-quota");
        let script = |rel: &str, body: &str| {
            let p = home.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, format!("#!/usr/bin/python3 -I\n{body}")).unwrap();
            fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        };
        script(".kimi-code/bin/kimi", r#"
import http.server, json, signal, sys
assert sys.argv[1:] == ['web', '--no-open', '--port', '0'], sys.argv
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        ok = self.path == '/api/v1/oauth/usage' and self.headers['Authorization'] == 'Bearer t0k'
        body = json.dumps({'code': 0, 'data': {'kind': 'ok', 'quota': {'usages': {
            'limit5h': {'usedRatio': 0.1, 'resetAt': '2099-01-01T00:00:00Z'},
            'limit7d': {'usedRatio': 0.2573, 'resetAt': '2099-01-02T00:00:00Z'}}}}}).encode()
        self.send_response(200 if ok else 401)
        self.send_header('Content-Length', str(len(body)))
        self.send_header('Connection', 'close')
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *a): pass
s = http.server.HTTPServer(('127.0.0.1', 0), H)
signal.signal(signal.SIGTERM, lambda *a: sys.exit(0))
print('  Kimi server ready')
print('  Local:    http://127.0.0.1:%d/#token=t0k' % s.server_port, flush=True)
s.serve_forever()
"#);
        script(".grok/bin/grok", r#"
import json, sys
assert sys.argv[1:] == ['agent', '--no-leader', 'stdio'], sys.argv
for line in sys.stdin:
    m = json.loads(line)
    if m['method'] == 'initialize':
        r = {'protocolVersion': 1}
    elif m['method'] == '_x.ai/billing':
        print(json.dumps({'jsonrpc': '2.0', 'method': '_x.ai/mcp/servers_updated', 'params': {}}), flush=True)
        r = {'config': {'creditUsagePercent': 1.0, 'currentPeriod': {'type': 'USAGE_PERIOD_TYPE_WEEKLY',
             'end': '2099-01-02T00:00:00.000+00:00'}}, 'subscription_tier': 'SuperGrok'}
    print(json.dumps({'jsonrpc': '2.0', 'id': m['id'], 'result': r}), flush=True)
"#);
        let (tx, rx) = mpsc::channel();
        let mut r = Reader::new(home.clone(), home.join(".codex"));
        r.quota_tx = Some(tx);
        let collect = || -> Vec<(Tool, Quota)> {
            match rx.recv_timeout(Duration::from_secs(15)) {
                Ok(Cmd::QuotaSnapshot { tool, quotas, source: "command" }) => quotas.into_iter().map(|q| (tool, q)).collect(),
                _ => Vec::new(),
            }
        };
        r.read(&TitleJob { tool: Tool::Kimi, usage: UsageSource::KimiWire, ..job(TitleSource::HookField, "s1", None) });
        let kimi = collect();
        assert_eq!(kimi.iter().map(|(t, q)| (*t, q.window, q.unlimited)).collect::<Vec<_>>(),
                   [(Tool::Kimi, Window::FiveHour, false), (Tool::Kimi, Window::SevenDay, false)]);
        assert!((kimi[1].1.used_percent - 25.73).abs() < 1e-9);
        r.read(&TitleJob { tool: Tool::Grok, usage: UsageSource::GrokUsageJson, ..job(TitleSource::GrokSummary, "s2", None) });
        let grok = collect();
        assert_eq!(grok.iter().map(|(t, q)| (*t, q.window, q.used_percent)).collect::<Vec<_>>(), [(Tool::Grok, Window::SevenDay, 1.0)],
                   "没有的 5 小时不补");
        r.read(&TitleJob { tool: Tool::Kimi, usage: UsageSource::KimiWire, ..job(TitleSource::HookField, "s1", None) });
        assert!(rx.recv_timeout(Duration::from_millis(500)).is_err(), "每款 5 分钟内不再查");
        // 认证失败（如 grok 未登录）什么也不发。
        script(".grok/bin/grok", r#"
import json, sys
for line in sys.stdin:
    m = json.loads(line)
    out = {'result': {}} if m['method'] == 'initialize' else {'error': {'code': -32000, 'message': 'Authentication required'}}
    print(json.dumps({'jsonrpc': '2.0', 'id': m['id'], **out}), flush=True)
"#);
        r.quota_at.clear();
        r.read(&TitleJob { tool: Tool::Grok, usage: UsageSource::GrokUsageJson, ..job(TitleSource::GrokSummary, "s2", None) });
        assert!(matches!(rx.recv_timeout(Duration::from_secs(3)), Ok(Cmd::QuotaNote { tool: Tool::Grok, note: "login_required" })),
                "没登录 grok.com：告诉用户原因，不发额度");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn copilot_and_opencode_reply_model_usage() {
        let home = test_dir("title-copilot-opencode");
        let id = "ec7b8d9b-05bd-4af0-8f35-5e704a2197ff";
        let rec = home.join(format!(".copilot/session-state/{id}/events.jsonl"));
        fs::create_dir_all(rec.parent().unwrap()).unwrap();
        let usage = |input: u64, read: u64, write: u64, out: u64| {
            format!(r#"{{"type":"session.usage_record","data":{{"usage":{{"model":"gpt-6-luna","inputTokens":{input},"outputTokens":{out},"cacheReadTokens":{read},"cacheWriteTokens":{write}}}}}}}"#)
        };
        let snapshot = |left: f64| {
            format!(r#"{{"type":"model.model_call_success","timestamp":"2026-11-01T14:07:30.726Z","data":{{"quotaSnapshots":{{"chat":{{"isUnlimitedEntitlement":false,"entitlementRequests":200,"remainingPercentage":{left},"resetDate":"2026-12-01T00:00:00Z"}}}}}}}}"#)
        };
        let lines = [
            r#"{"type":"user.message","data":{"content":"只回复 en"}}"#.to_owned(),
            snapshot(99.5),
            usage(20192, 9893, 10296, 28),
            r#"{"type":"assistant.message","data":{"model":"gpt-6-luna","content":"en"}}"#.to_owned(),
            snapshot(99.0),
            usage(100, 50, 0, 10),
        ];
        fs::write(&rec, lines.join("\n") + "\n").unwrap();
        let mut r = Reader::new(home.clone(), home.join(".codex"));
        let copilot = TitleJob { tool: Tool::Copilot, reply: ReplySource::CopilotEvents, model: ModelSource::CopilotEvents,
                                 usage: UsageSource::CopilotEvents, effort: EffortSource::None, ..job(TitleSource::HookField, id, None) };
        let got = r.read(&copilot);
        assert_eq!((got.reply.as_deref(), got.model.as_deref()), (Some("en"), Some("gpt-6-luna")));
        assert_eq!(got.tokens, Some(Tokens { input: 53, cache_read: 9943, cache_write: 10296, output: 38, total: 20330 }));
        assert_eq!(got.quotas.iter().map(|q| (q.window, q.used_percent)).collect::<Vec<_>>(), [(Window::Month, 1.0)], "取最新的快照");
        let mut f = fs::OpenOptions::new().append(true).open(&rec).unwrap();
        writeln!(f, "{}", usage(10, 0, 0, 1)).unwrap();
        let got = r.read(&copilot);
        assert_eq!(got.tokens.map(|t| t.total), Some(20341), "只读新增的行");
        assert_eq!(got.api, Some(false), "isByok 不是 true：不是按量计费");
        writeln!(f, r#"{{"type":"session.usage_record","data":{{"usage":{{"model":"my-model","isByok":true,"inputTokens":1,"outputTokens":1}}}}}}"#).unwrap();
        assert_eq!(r.read(&copilot).api, Some(true), "自带 key 的模型为按量计费");
        assert_eq!(got.quotas.len(), 1, "新增的行没有快照时仍报上次的");

        let opencode = TitleJob { tool: Tool::OpenCode, reply: ReplySource::OpenCodeDb, model: ModelSource::None,
                                  usage: UsageSource::OpenCodeDb, effort: EffortSource::None, ..job(TitleSource::HookField, "ses_a", None) };
        let got = r.read(&opencode);
        assert_eq!((got.reply, got.tokens), (None, None), "没有库");
        let db = home.join(".local/share/opencode/opencode.db");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            r#"CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             INSERT INTO message VALUES ('m1','ses_a',100,100,'{"role":"user"}'), ('m2','ses_a',200,200,'{"role":"assistant"}'),
                                        ('m3','ses_a',300,300,'{"role":"user"}'), ('m4','ses_a',400,400,'{"role":"assistant"}'),
                                        ('m5','ses_b',500,500,'{"role":"assistant"}');
             INSERT INTO part VALUES ('p1','m1','ses_a',100,100,'{"type":"text","text":"第一问"}'),
                ('p2','m2','ses_a',200,200,'{"type":"text","text":"第一答"}'),
                ('p3','m2','ses_a',200,200,'{"type":"step-finish","tokens":{"total":16990,"input":15196,"output":2,"reasoning":0,"cache":{"write":0,"read":1792}}}'),
                ('p4','m3','ses_a',300,300,'{"type":"text","text":"看看目录"}'),
                ('p5','m4','ses_a',400,400,'{"type":"tool","tool":"bash"}'),
                ('p6','m4','ses_a',400,400,'{"type":"step-finish","tokens":{"total":17041,"input":15240,"output":2,"reasoning":7,"cache":{"write":0,"read":1792}}}'),
                ('p7','m5','ses_b',500,500,'{"type":"text","text":"别的会话"}');"#,
        )
        .unwrap();
        drop(c);
        let got = r.read(&opencode);
        assert_eq!(got.reply.as_deref(), Some("第一答"), "最近一条只调了工具，往前找有文字的");
        assert_eq!(got.tokens, Some(Tokens { input: 30436, cache_read: 3584, cache_write: 0, output: 11, total: 34031 }), "每一步相加");
        assert_eq!(got.api, Some(false), "免费模型花费为 0：不是按量计费");
        // 表结构变了：读不到，不报错（保留旧值）。
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch("DROP TABLE part; CREATE TABLE part (id TEXT, message_id TEXT);").unwrap();
        drop(c);
        let got = r.read(&opencode);
        assert_eq!((got.reply, got.tokens), (None, None));
        let _ = fs::remove_dir_all(&home);
    }

    /// OpenCode 2（2026-10-31 本机 2.2.4）：回复、token、模型都从新表读（`session_v2`、`session_message`）；不在新表里的会话
    /// 照旧查 OpenCode 1 的表。
    #[test]
    fn opencode_2_tables() {
        let home = test_dir("title-opencode2");
        let db = home.join(".local/share/opencode/opencode.db");
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            r#"CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT);
             CREATE TABLE session_v2 (id TEXT PRIMARY KEY, title TEXT, model TEXT, tokens_input INTEGER, tokens_output INTEGER,
                                      tokens_reasoning INTEGER, tokens_cache_read INTEGER, tokens_cache_write INTEGER);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER, time_created INTEGER,
                                           time_updated INTEGER, data TEXT);
             INSERT INTO session_v2 VALUES ('ses_2', '用 bash 执行 pwd', '{"id":"deepseek-v4.1-flash-free","providerID":"opencode"}',
                                            13300, 126, 11, 26880, 0);
             INSERT INTO session_message VALUES
                ('m1','ses_2','user',4,100,100,'{"text":"用 bash 执行 pwd"}'),
                ('m2','ses_2','assistant',7,200,200,'{"content":[{"type":"text","text":"当前目录是 /w"}]}'),
                ('m3','ses_2','assistant',14,300,300,'{"content":[{"type":"tool","name":"shell"}]}'),
                ('m4','ses_2','idle',19,400,400,'{"outcome":"succeeded"}');
             INSERT INTO message VALUES ('o1','ses_1',100,100,'{"role":"assistant","modelID":"old-model"}');
             INSERT INTO part VALUES ('p1','o1','ses_1',100,100,'{"type":"text","text":"旧版回复"}');"#,
        )
        .unwrap();
        drop(c);
        let mut r = Reader::new(home.clone(), home.join(".codex"));
        let job = |id: &str| TitleJob { tool: Tool::OpenCode, reply: ReplySource::OpenCodeDb, model: ModelSource::OpenCodeDb,
                                       usage: UsageSource::OpenCodeDb, effort: EffortSource::None, ..job(TitleSource::HookField, id, None) };
        let got = r.read(&job("ses_2"));
        assert_eq!((got.reply.as_deref(), got.model.as_deref()), (Some("当前目录是 /w"), Some("deepseek-v4.1-flash-free")),
                   "最近一条只调了工具，往前找有文字的");
        assert_eq!(got.tokens, Some(Tokens { input: 13300, cache_read: 26880, cache_write: 0, output: 137, total: 40317 }));
        let old = r.read(&job("ses_1"));
        assert_eq!((old.reply.as_deref(), old.model.as_deref()), (Some("旧版回复"), Some("old-model")), "不在新表里：查旧表");
        let _ = fs::remove_dir_all(&home);
    }
}
