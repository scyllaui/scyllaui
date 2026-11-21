//! 核心线程：独占状态与数据库，处理连接线程送来的命令，生成应答与推送（docs/protocol.md）。
//! 核心线程从不在 socket 上阻塞：输出经各连接的有界队列交给写线程，队列满即断开该连接，
//! App 重连后重新取快照。

use crate::git::{GitStatus, GitWorker, Job};
use crate::hooks::{analyze_all, HookWatch};
use crate::hooktime::{self, Facts, Sync, Timeline};
use crate::title::{TitleJob, TitleWorker};
use crate::version::{VersionJob, VersionWorker};
use crate::waitwatch::WaitWatch;
use crate::procinfo::{self, ExitWatch, Foreground, TreeSample};
use crate::prompts::{PromptJob, PromptWorker, Segment};
use crate::pty::{self, spawn::SpawnSpec, Host};
use crate::route::{Probe, RouteJob, RouteWorker};
use crate::retention::Retention;
use crate::store::Store;
use crate::workspace::WorkspaceDirs;
use scyllaui_proto::adapters::usage::{claude_statusline_quotas, Quota, Tokens, Window};
use scyllaui_proto::adapters::{adapter, valid_session_id, verified, verified_list, TitleSource};
use scyllaui_proto::hookcfg::{self, Action as HookAction, Plan, State as HookState, Status};
use scyllaui_proto::{
    normalize, now_ms, now_us, AgentState, Applied, HookEvent, Kind, PaneState, Tool, BUILD_ID,
    PROTOCOL_VERSION,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 送往写线程的输出。`Flush` 在之前的行都写完后应答，用于退出前确保回复已送达。
pub enum Out {
    Line(Vec<u8>),
    Flush(mpsc::Sender<()>),
}

pub enum Cmd {
    /// app / cli 连接的第一行；`out` 是该连接写线程的队列。
    Hello {
        conn: u64,
        hello: Value,
        out: SyncSender<Out>,
    },
    Request {
        conn: u64,
        msg: Value,
    },
    Hook {
        frame: Value,
        recv_us: u64,
    },
    /// `scyllaui statusline` 送来的 Claude statusline 内容（M2 第 3 块，docs/protocol.md 第 4 节「statusline 帧」）。
    Statusline {
        frame: Value,
    },
    Closed {
        conn: u64,
    },
    /// kqueue 报告被监听的 Agent 进程已退出（procinfo.rs）。
    ProcExit {
        pid: i32,
    },
    /// kqueue 报告某个被监听的 git 目录有写入（git.rs）；`fd` 标识该目录。
    GitChanged {
        fd: RawFd,
    },
    /// 以下三条是 GitWorker 的结果（git.rs `Job`）。
    RepoFound {
        project_id: i64,
        git_dir: Option<PathBuf>,
        branch: Option<String>,
        /// 已监听时为该 git 目录的描述符。
        fd: Option<RawFd>,
    },
    CwdResolved {
        terminal_id: String,
        cwd: String,
        root: String,
        at: i64,
    },
    BranchRead {
        git_dir: PathBuf,
        branch: Option<String>,
    },
    /// RouteWorker 反查到守护进程托管会话的归属终端（route.rs）；`pane` 为 None 表示没有终端连着它。
    HookRouted {
        frame: Value,
        recv_us: u64,
        pane: Option<String>,
    },
    /// PromptWorker 读完一次 `prompts.list`（prompts.rs）：整条应答发给请求方连接（docs/design.md 10.5 节决策 10）。
    PromptsListed {
        conn: u64,
        reply: Value,
    },
    /// TitleWorker 读到对话的原生标题（title.rs）。
    TitleRead {
        conversation_id: i64,
        title: String,
    },
    /// TitleWorker 读到对话的最后一条 AI 回复（M1 第 6 块）。
    ReplyRead {
        conversation_id: i64,
        reply: String,
    },
    /// HookWatch 分析出四款的 Hook 配置状态有变化（hooks.rs，M1 第 7 块）。
    HooksChanged {
        statuses: Vec<Status>,
    },
    /// VersionWorker 读到某终端前台 CLI 的版本（version.rs，M1 第 7 块）；`version` 为 None 表示未知。
    VersionRead {
        terminal_id: String,
        pid: i32,
        version: Option<String>,
    },
    /// TitleWorker 读到对话的当前模型（M2 第 2 块）。
    ModelRead {
        conversation_id: i64,
        model: String,
    },
    /// TitleWorker 读到对话的推理强度（维护期 0.3.5）。
    EffortRead {
        conversation_id: i64,
        effort: String,
    },
    /// TitleWorker 读到对话的会话累计 token（M2 第 3 块）。
    TokensRead {
        conversation_id: i64,
        tokens: Tokens,
    },
    /// 读到某款 CLI 账号当前全部窗口的额度（M2 第 3 块；2026-11-01 起整份替换，快照里没有、又比它旧的窗口删掉）；`source`
    /// 为 `log`、`statusline` 或 `command`。空快照不算数（还没读到，或来源这次没带额度）。
    QuotaSnapshot {
        tool: Tool,
        quotas: Vec<Quota>,
        source: &'static str,
    },
    /// 另起命令查额度没查到、而且能告诉用户原因（协议 `quota.note`，2026-11-01）：Grok 没登录 grok.com 为 `login_required`。
    QuotaNote {
        tool: Tool,
        note: &'static str,
    },
    /// TitleWorker 读到对话是否按量计费（2026-11-01 标题栏额度统一规则）：只在记录里有依据时为 true。
    BillingRead {
        conversation_id: i64,
        api: bool,
    },
    /// GitWorker 算出的改动数（M2 第 2 块）；None 表示不是仓库、没有 git、超时或出错。
    GitStatus {
        project_id: i64,
        status: Option<GitStatus>,
    },
    /// WaitWatch 从会话记录认出 Hook 之外发生的事（waitwatch.rs，M2 第 1 块）：Claude 的拒绝审批，Copilot 的批准、
    /// 拒绝与中断（M4b 块 9c）。
    SessionRecord {
        terminal_id: String,
        session_id: Option<String>,
        kind: Kind,
        /// 事件所属的轮次（记录里带轮次号时，Kimi）：与窗格当前轮次不同的不理会。
        turn: Option<String>,
        /// 事件属于哪个子代理（Antigravity 子代理的对话库）：按这个子代理的事件处理。
        agent: Option<String>,
    },
    /// WaitWatch 在会话记录里读到一条用户提交的提示词（`Scan::Submits`，docs/design.md 4.7 节「用会话记录对账」）；
    /// `at` 为记录里的时间（毫秒），没有时为 None。
    RecordSubmit {
        terminal_id: String,
        session_id: Option<String>,
        at: Option<i64>,
    },
    /// WaitWatch 打开会话记录的结果（2026-11-14）：`ok` 为假表示重试到时间仍定位不到或打不开。
    WatchOpened {
        /// 监听的标识（`Start::key`）。
        key: String,
        session_id: Option<String>,
        ok: bool,
    },
    /// `pty` 角色的握手（M3，docs/protocol.md 第 3.1 节）：核心线程判定是否接受、需要时起 Shell，
    /// 经 `reply` 交回连接线程，由它完成接回与中继。
    PtyHello {
        hello: Value,
        reply: mpsc::Sender<PtyAdmit>,
    },
    /// 宿主的等待线程回收了保活终端的 Shell（login）进程（pty/mod.rs）；`code` 为退出码，被信号结束为 128 + 信号号。
    ShellExited {
        pid: i32,
        code: Option<i32>,
    },
    /// PTY 宿主的事件（pty/mod.rs）。
    Pty {
        terminal_id: String,
        event: pty::Event,
    },
}

/// `pty` 握手的结果：接受时为会话与 `hello_ok` 行，拒绝时为 `hello_err` 行（都带结尾换行）。
pub type PtyAdmit = Result<(Arc<pty::Session>, Vec<u8>), Vec<u8>>;

/// 一次待审批的等待（M2 第 1 块，docs/protocol.md 第 7 节「通知」）：发出的通知 ID（开始时用户正看着它则为 None，
/// 用户切过来后也置为 None）。只在内存中。是否在监听会话记录另记在 `watching`（运行中也可能在监听）。
struct Wait {
    notice: Option<String>,
}

/// 项目的 Git 仓库与当前分支（docs/protocol.md 第 7 节）。只在内存中，不入库。
struct Repo {
    git_dir: Option<PathBuf>,
    branch: Option<String>,
    /// 已请求监听其 git 目录。
    watched: bool,
}

/// Agent 进程退出后等 SessionEnd 的宽限时间；超时仍未收到，按进程退出结束对话（docs/protocol.md 第 7 节）。
const EXIT_GRACE: Duration = Duration::from_secs(1);
/// 终端还开着时对话结束了，后台再活这么久才确认是正常结束（缺陷 B，docs/design.md「重启恢复」）：真实关机时 Agent
/// 先于后台被结束，后台随后几秒内消失，来不及确认，重建时据此接回这段对话。
const ENDED_CONFIRM: Duration = Duration::from_secs(30);
/// 换版本交出状态之前，把已经收到的命令处理完：这么久没有新的为止，最多等这么久。
/// 每个终端另开监听的子代理记录的个数上限。
const CHILD_RECORDS_MAX: usize = 8;
const UPGRADE_QUIET: Duration = Duration::from_millis(30);
const UPGRADE_DRAIN: Duration = Duration::from_millis(300);

/// 保活终端标题变化后补读前台的延迟（docs/design.md 4.2 节：Shell 集成在 exec 之前就发出标题）。
const TITLE_FOLLOWUP: Duration = Duration::from_millis(500);

/// 读原生标题的延迟：提交提示词后、以及一轮结束后的第二次读（标题通常在首轮中途或结束后不久生成）。
const TITLE_DELAY: Duration = Duration::from_secs(5);

/// 每写入这么多条事件清理一次事件表。
const PRUNE_EVENTS_EVERY: u32 = 1000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    App,
    Cli,
}

struct Conn {
    role: Role,
    out: SyncSender<Out>,
    /// 握手成功；`hello_err` 之后只处理 `shutdown`。
    ready: bool,
    /// `sync` 应答之后开始接收推送。
    subscribed: bool,
}

/// 存活终端的 Agent 状态、当前（或最后）对话与前台程序。
/// 一次会话记录监听针对的会话与轮次；变了就重开。
#[derive(Clone, Debug, PartialEq, Eq)]
struct WatchKey {
    session: Option<String>,
    turn: Option<String>,
}

struct Pane {
    state: PaneState,
    /// 换版本接管后还原了状态：下一次开始监听会话记录时按对账处理（补上空档里漏掉的事件）。只在内存中。
    resync: bool,
    conversation: Option<i64>,
    foreground: Foreground,
    /// 正在监听退出的 Agent 进程（进程组首进程）及其工具。
    watched: Option<(i32, Tool)>,
    /// Agent 进程已退出，到这个时间仍未收到 SessionEnd 就按进程退出处理。
    exit_deadline: Option<(Instant, Tool)>,
    /// 待确认的结束（`workspace.json` 的 `ended`）到这个时间后台还活着就清掉：这次结束不是关机造成的。只在内存中。
    ended_until: Option<Instant>,
    /// 「待查看」：一轮结束时用户没在看这个终端（docs/protocol.md 第 7 节）。只在内存中。
    unseen: bool,
    /// 最近一次 `terminal.cwd` 上报的当前目录。只在内存中。
    current_dir: Option<String>,
    /// 同一对话同时打开的其他终端（第 7 节「冲突」）。只在内存中。
    conflict: Vec<String>,
    /// 前台 CLI 的版本（M1 第 7 块）：读到之前为 None；只对这个进程号有效。只在内存中。
    version: Option<PaneVersion>,
    /// 控制终端设备号（M2 第 2 块，进程树统计）：上报前台进程组时取组首进程的。只在内存中。
    tty: Option<u32>,
    /// 保活终端（M3）：App 以 `held` 登记、等 attach 来起 Shell，或后台已持有其 PTY。只在内存中。
    held: bool,
    /// 起 Shell 时要写进 PTY 的字节（`terminal.open` 的 `initial_input`），起 Shell 时取走。
    pending_input: Option<Vec<u8>>,
    /// 终端提示（M3）：`cwd_denied`；启动目录不存在、已退回主目录为 `cwd_missing`（2026-11-13）。只在内存中；换版本接管时随接管文件带到新进程（2026-10-23 起）。
    notice: Option<&'static str>,
    /// 标题变化后补读前台的时间（M3）。
    fg_followup: Option<Instant>,
    /// 本进程启动时从库里接回的终端（旧后台留下的）；第一次 attach 时取走。为它新起 Shell 说明旧后台崩溃过，
    /// 绑着的对话要交给 App 接回（docs/design.md 10.5 节决策 21）。本进程登记的新终端（含一键恢复预先绑了对话的）不算。
    inherited: bool,
    /// 最近一次 Hook 异常（M4b 块 6，docs/design.md 10.6 节 6a）：「状态可能不准」的原因。只在内存中。
    anomaly: Option<PaneAnomaly>,
    /// 最近一次从这个终端收到 Hook 事件的工具与时间（Unix 毫秒）：判断前台进程在不在同步的事实依据
    /// （docs/design.md 4.7 节「会话是否真的在同步」）。只在内存中。
    last_hook: Option<(Tool, i64)>,
    /// 前台 Agent 进程的同步情况（同上）；在同步且配置正常、或前台不是 Agent 时为 None。只在内存中，随配置、前台、事件重算。
    sync: Option<Sync>,
    /// 会话记录对账（4.7 节「用会话记录对账」）：记录里读到一条用户提交的提示词，等到这个时间看它的事件到了没有。
    /// 依次为到期时间、前台 Agent 的工具与进程号、提示词的时刻（Unix 毫秒）。只在内存中。
    submit_check: Option<(Instant, Tool, i32, i64)>,
    /// 对账的结论：这个进程（工具、进程号）在这个时刻提交了提示词而事件没到。收到它的事件即清除，换了进程即无效。
    /// 只在内存中；换版本接管时随接管文件带到新进程。
    silent: Option<(Tool, i32, i64)>,
}

struct PaneVersion {
    pid: i32,
    tool: Tool,
    /// None：读不到，或可执行文件在进程启动后被替换。
    version: Option<String>,
}

/// 与版本一样只对那个 AI 进程有效：记下时前台还没识别出它为 None，识别出后补上；换了进程或回到 Shell 时清除。
struct PaneAnomaly {
    tool: Tool,
    pid: Option<i32>,
    /// 适配器给的原因代码（`Adapter::anomaly`）。
    code: String,
}

impl Pane {
    fn new(state: PaneState, conversation: Option<i64>) -> Pane {
        Pane {
            state,
            conversation,
            foreground: Foreground::Unknown,
            watched: None,
            exit_deadline: None,
            ended_until: None,
            unseen: false,
            current_dir: None,
            conflict: Vec::new(),
            version: None,
            tty: None,
            held: false,
            pending_input: None,
            notice: None,
            fg_followup: None,
            inherited: false,
            anomaly: None,
            resync: false,
            last_hook: None,
            sync: None,
            submit_check: None,
            silent: None,
        }
    }
}

/// 请求失败：错误码与说明（docs/protocol.md 第 4 节）。
struct Fail(&'static str, String);

impl From<rusqlite::Error> for Fail {
    fn from(e: rusqlite::Error) -> Fail {
        log!("数据库错误：{e}");
        Fail("internal", e.to_string())
    }
}

fn bad(msg: &str) -> Fail {
    Fail("bad_request", msg.to_owned())
}

pub struct Core {
    store: Store,
    sock: PathBuf,
    capture: Option<PathBuf>,
    rev: u64,
    conns: HashMap<u64, Conn>,
    app: Option<u64>,
    panes: HashMap<String, Pane>,
    /// 用户正在看的终端（`terminal.focus`）；App 断开时清空。
    focused: Option<String>,
    events_since_prune: u32,
    exits: ExitWatch,
    git: GitWorker,
    /// 项目 ID → 仓库与分支（GitWorker 读到之前为空）。
    repos: HashMap<i64, Repo>,
    /// 已监听的 git 目录：描述符 → 目录。
    git_fds: HashMap<RawFd, PathBuf>,
    titles: TitleWorker,
    /// 历史提示词的读取线程（M4a 追加块 9）；测试里多数不设。
    prompts: Option<PromptWorker>,
    /// 存活终端此刻所在的目录（workspace.json，M4a 追加块 10）：重启后恢复终端时开回原目录。
    workspace: WorkspaceDirs,
    routes: RouteWorker,
    probe: Probe,
    /// Hook 配置监听（M1 第 7 块）与它最近一次给出的四款状态；`cli_home` 为 CLI 配置所在的家目录
    /// （`SCYLLAUI_CLI_HOME`，默认 HOME；docs/protocol.md 第 1 节）。
    hook_watch: HookWatch,
    hooks: Vec<Status>,
    cli_home: String,
    /// 每款 Hook 配置状态变化的时间（hook-timeline.json）：判断一个 Agent 进程启动时条目在不在。
    hook_times: Timeline,
    versions: VersionWorker,
    /// 测试后台（`--data-dir` 指向别处）没有指定 `SCYLLAUI_CLI_HOME` 时为 true：拒绝修复，避免测试改到真实配置。
    real_config_guard: bool,
    /// 正在待审批的终端 → 这次等待；每个终端的等待序号（M2 第 1 块）。
    waits: HashMap<String, Wait>,
    /// 正在监听会话记录的终端（waitwatch.rs）：待审批，适配器声明了 `scan_while_running` 时的运行中，声明了
    /// `turns_by_record` 时会话还在的整段时间。
    watching: HashMap<String, WatchKey>,
    /// 其中监听线程确认已打开记录的（适配器声明 `turns_by_record` 的工具据此决定要不要按 Hook 结束一轮）。
    watch_open: HashSet<String>,
    wait_seq: HashMap<String, u64>,
    /// 已发完成通知的终端 → 通知 ID；每个终端的完成序号（M4a，docs/protocol.md 第 7 节「完成通知」）。
    dones: HashMap<String, String>,
    done_seq: HashMap<String, u64>,
    wait_watch: WaitWatch,
    /// 本条命令中最后处理的 Hook 的延迟打点，附在因它发出的通知上（供 App 统计「Hook → 通知」延迟）。
    last_trace: Option<Value>,
    /// 对话的当前模型与来源（`hook` / `log` / `statusline`，M2 第 2 块）和会话累计 token（第 3 块）。只在内存中，对话结束即移除。
    usage: HashMap<i64, Usage>,
    /// 各款 CLI 账号的额度与来源（M2 第 3 块），键为工具名与窗口（5 小时、7 天、月）。只在内存中。
    quotas: HashMap<(&'static str, Window), (Quota, &'static str)>,
    /// 各款查不到额度的原因代码（`quota.note`，2026-11-01），读到额度即清掉。只在内存中。
    quota_notes: HashMap<&'static str, &'static str>,
    /// 标题栏显示的项目（`status.watch`）与它最近一次算出的改动数（M2 第 2 块）。
    watch_project: Option<i64>,
    git_status: Option<(i64, Option<GitStatus>)>,
    /// 各终端上一次进程树采样（`proc.stats`）。
    cpu_prev: HashMap<String, (Instant, TreeSample)>,
    /// PTY 宿主（M3）。Shell 的退出由宿主的等待线程回收后以 `Cmd::ShellExited` 送来。
    host: Arc<Host>,
    /// App 断开前最后一次非空的 `terminal.focus`（快照的 `last_focus`，M3）。
    last_focus: Option<String>,
    /// 后台崩溃重启后在原终端重起 Shell、原对话已结束的终端（`terminal.respawned`）：App 还没订阅推送时先存着，
    /// 随下一次 `sync` 的快照交出（docs/design.md 10.5 节决策 21）。
    respawned: Vec<Value>,
    /// 监听 socket 与接收连接的暂停阀（换版本接管时交给新进程，docs/design.md 第 7 节「换版本」）；测试里没有。
    listener: Option<(RawFd, crate::conn::AcceptGate)>,
    /// 「需确认」那个事件给出的记录路径与它是不是子代理的（适配器声明 `record_from_approval`，Codex）；离开待审批即清。
    approval_records: HashMap<String, (String, bool)>,
    /// 终端上见过的子代理的会话 ID（适配器声明 `child_records`，Antigravity），最多 `CHILD_RECORDS_MAX` 个；换会话即清。
    child_records: HashMap<String, (Option<String>, Vec<String>)>,
    /// 按进程认批准的监听（approval.rs，Codex）与正在盯的终端；测试里没有。
    approval_watch: Option<crate::approval::ApprovalWatch>,
    approval_watching: HashSet<String>,
    /// 收到的 `upgrade` 请求（连接、请求 ID、新程序路径）：处理完这条命令后由 `run` 执行，它要用到命令队列。
    pending_upgrade: Option<(u64, Value, String)>,
    /// `pty` 握手的 `spawn.command`（直接执行的 argv）只在测试后台中生效。
    allow_command: bool,
    /// 单实例锁的描述符：换版本接管时交给新进程（M3）。
    lock_fd: RawFd,
    /// 已结束对话的保留设置与它的文件（M4a，docs/protocol.md 第 7 节「保留上限」）。
    retention: Retention,
    retention_path: PathBuf,
}

impl Core {
    /// 恢复存活终端上未结束的对话绑定（状态为「未知」），读取保留设置（`retention_path`）并按它清理一次。
    pub fn new(
        store: Store,
        sock: PathBuf,
        capture: Option<PathBuf>,
        exits: ExitWatch,
        git: GitWorker,
        titles: TitleWorker,
        routes: RouteWorker,
        hook_watch: HookWatch,
        versions: VersionWorker,
        wait_watch: WaitWatch,
        cli_home: String,
        host: Arc<Host>,
        retention_path: PathBuf,
    ) -> rusqlite::Result<Core> {
        let now = now_ms();
        let retention = Retention::load(&retention_path);
        let pruned = store.prune_conversations(now, retention.keep_days, retention.keep_conversations)?.len()
            + store.prune_events()?;
        let mut panes = HashMap::new();
        for id in store.alive_terminals()? {
            let pane = match store.open_conversation_on(&id)? {
                Some((cid, tool, native, title)) => Pane::new(
                    Tool::parse(&tool)
                        .map(|t| PaneState::restored(t, native, title))
                        .unwrap_or_default(),
                    Some(cid),
                ),
                None => Pane::new(PaneState::default(), None),
            };
            panes.insert(id, Pane { inherited: true, ..pane });
        }
        log!("恢复存活终端 {} 个，清理过期记录 {pruned} 条", panes.len());
        let alive_projects: HashSet<i64> = store
            .alive_terminals()?
            .iter()
            .filter_map(|id| store.terminal(id).ok().flatten())
            .filter_map(|t| t["project_id"].as_i64())
            .collect();
        let projects = store.projects()?;
        let mut core = Core {
            store,
            sock,
            capture,
            rev: 0,
            conns: HashMap::new(),
            app: None,
            panes,
            focused: None,
            events_since_prune: 0,
            exits,
            git,
            repos: HashMap::new(),
            git_fds: HashMap::new(),
            titles,
            prompts: None,
            workspace: WorkspaceDirs::default(),
            routes,
            probe: Probe::system(),
            hook_watch,
            hooks: Vec::new(),
            cli_home,
            hook_times: Timeline::default(),
            versions,
            real_config_guard: false,
            waits: HashMap::new(),
            watching: HashMap::new(),
            watch_open: HashSet::new(),
            dones: HashMap::new(),
            wait_seq: HashMap::new(),
            done_seq: HashMap::new(),
            wait_watch,
            last_trace: None,
            usage: HashMap::new(),
            quotas: HashMap::new(),
            quota_notes: HashMap::new(),
            watch_project: None,
            git_status: None,
            cpu_prev: HashMap::new(),
            host,
            last_focus: None,
            respawned: Vec::new(),
            listener: None,
            approval_records: HashMap::new(),
            child_records: HashMap::new(),
            approval_watch: None,
            approval_watching: HashSet::new(),
            pending_upgrade: None,
            allow_command: false,
            lock_fd: -1,
            retention,
            retention_path,
        };
        // 分支：有存活终端的项目监听 git 目录，其余只读一次。都交给 GitWorker，启动不等结果。
        for p in projects {
            if let (Some(id), Some(dir)) = (p["project_id"].as_i64(), p["dir"].as_str()) {
                core.track_repo(id, dir, alive_projects.contains(&id));
            }
        }
        Ok(core)
    }

    /// 没有待到期的计时时一直阻塞等命令；有则最多等到最早的到期时间，不轮询。
    pub fn run(mut self, rx: Receiver<Cmd>) {
        loop {
            let next = self
                .panes
                .values()
                .flat_map(|p| [p.exit_deadline.map(|(t, _)| t), p.fg_followup, p.ended_until, p.submit_check.map(|c| c.0)])
                .flatten()
                .min();
            let cmd = match next {
                None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                Some(t) => rx.recv_timeout(t.saturating_duration_since(Instant::now())),
            };
            match cmd {
                Ok(cmd) => self.handle(cmd),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            if let Some((conn, req_id, path)) = self.pending_upgrade.take() {
                self.upgrade(&rx, conn, req_id, &path);
            }
            self.expire_exits(Instant::now());
            self.expire_followups(Instant::now());
            self.expire_ended(Instant::now());
            self.expire_submits(Instant::now());
        }
    }

    pub fn handle(&mut self, cmd: Cmd) {
        self.last_trace = None;
        match cmd {
            Cmd::Hello { conn, hello, out } => self.hello(conn, &hello, out),
            Cmd::Request { conn, msg } => self.request(conn, &msg),
            Cmd::Hook { frame, recv_us } => self.hook(&frame, recv_us, false),
            Cmd::Statusline { frame } => self.statusline(&frame),
            Cmd::HookRouted { mut frame, recv_us, pane } => match pane.or_else(|| self.infer_pane(&frame)) {
                Some(p) => {
                    frame["pane_id"] = json!(p);
                    self.hook(&frame, recv_us, true);
                }
                None => log!(
                    "丢弃未连到任何终端的后台会话 Hook：event={} session={}",
                    frame["payload"]["hook_event_name"].as_str().unwrap_or("-"),
                    frame["payload"]["session_id"].as_str().unwrap_or("-")
                ),
            },
            Cmd::Closed { conn } => self.drop_conn(conn, "连接已关闭"),
            Cmd::ProcExit { pid } => self.proc_exit(pid, Instant::now()),
            Cmd::GitChanged { fd } => self.git_changed(fd),
            Cmd::RepoFound { project_id, git_dir, branch, fd } => {
                self.repo_found(project_id, git_dir, branch, fd)
            }
            Cmd::CwdResolved { terminal_id, cwd, root, at } => {
                if let Err(Fail(_, e)) = self.cwd_resolved(&terminal_id, &cwd, &root, at) {
                    log!("终端改归项目失败：{e}");
                }
            }
            Cmd::BranchRead { git_dir, branch } => self.branch_read(&git_dir, branch),
            Cmd::PromptsListed { conn, reply } => self.send_to(conn, &reply),
            Cmd::TitleRead { conversation_id, title } => {
                let r = self.store.set_native_title(conversation_id, &title).map_err(Fail::from);
                if let Err(Fail(_, e)) = r.and_then(|changed| match changed {
                    true => self.push_conversation(conversation_id),
                    false => Ok(()),
                }) {
                    log!("写入原生标题失败：{e}");
                }
            }
            Cmd::ReplyRead { conversation_id, reply } => {
                let r = self.store.set_last_reply(conversation_id, &reply).map_err(Fail::from);
                if let Err(Fail(_, e)) = r.and_then(|changed| match changed {
                    true => self.push_conversation(conversation_id),
                    false => Ok(()),
                }) {
                    log!("写入最后一条回复失败：{e}");
                }
            }
            Cmd::HooksChanged { statuses } => self.hooks_changed(statuses),
            Cmd::VersionRead { terminal_id, pid, version } => {
                if let Err(Fail(_, e)) = self.version_read(&terminal_id, pid, version) {
                    log!("记录 CLI 版本失败：{e}");
                }
            }
            Cmd::SessionRecord { terminal_id, session_id, kind, turn, agent } => {
                self.session_record(&terminal_id, session_id, kind, turn, agent)
            }
            Cmd::RecordSubmit { terminal_id, session_id, at } => self.record_submit(&terminal_id, session_id, at),
            Cmd::WatchOpened { key, session_id, ok } => {
                let current = self.watching.get(&key).is_some_and(|k| k.session == session_id);
                if !ok {
                    log!("会话记录打不开，这次不监听：{key}");
                } else if current {
                    self.watch_open.insert(key);
                }
            }
            Cmd::ModelRead { conversation_id, model } => {
                if self.conversation_active(conversation_id) {
                    self.set_model(conversation_id, model, "log");
                }
            }
            Cmd::EffortRead { conversation_id, effort } => {
                if self.conversation_active(conversation_id) {
                    self.set_effort(conversation_id, effort);
                }
            }
            Cmd::TokensRead { conversation_id, tokens } => {
                if self.conversation_active(conversation_id) {
                    self.set_tokens(conversation_id, tokens);
                }
            }
            Cmd::QuotaSnapshot { tool, quotas, source } => self.set_quotas(tool, quotas, source),
            Cmd::QuotaNote { tool, note } => self.set_quota_note(tool, Some(note)),
            Cmd::BillingRead { conversation_id, api } => {
                if self.conversation_active(conversation_id) {
                    self.set_billing(conversation_id, api);
                }
            }
            Cmd::GitStatus { project_id, status } => self.git_status_read(project_id, status),
            Cmd::PtyHello { hello, reply } => {
                let r = self.pty_hello(&hello);
                let _ = reply.send(r);
            }
            Cmd::Pty { terminal_id, event } => self.pty_event(&terminal_id, event),
            Cmd::ShellExited { pid, code } => self.shell_exited(pid, code),
        }
        self.refresh_conflicts();
        self.refresh_waits();
        self.refresh_dones();
    }

    // ---- 连接 ----

    fn hello(&mut self, id: u64, hello: &Value, out: SyncSender<Out>) {
        let role = match hello["role"].as_str() {
            Some("app") => Some(Role::App),
            Some("cli") => Some(Role::Cli),
            _ => None,
        };
        let code = if hello["type"] != "hello" || role.is_none() {
            Some("bad_hello")
        } else if hello["protocol_version"] != PROTOCOL_VERSION {
            Some("protocol_mismatch")
        } else if role == Some(Role::App) && self.app.is_some() {
            Some("app_already_connected")
        } else {
            None
        };
        let mut msg = json!({
            "type": "hello_ok",
            "protocol_version": PROTOCOL_VERSION,
            "build_id": BUILD_ID,
            "pid": std::process::id(),
            "pty_host": true,
            // 自身可执行文件的路径（M4b 块 2）：App 据此发现构建标识相同、位置不同的后台（如正式版换到 /Applications）并换新。
            "exe": std::env::current_exe().ok().map(|p| p.to_string_lossy().into_owned()),
        });
        if let Some(code) = code {
            msg["type"] = json!("hello_err");
            msg["code"] = json!(code);
            log!("连接 {id} 握手被拒：{code}（{hello}）");
        }
        let conn = Conn {
            role: role.unwrap_or(Role::Cli),
            out,
            ready: code.is_none(),
            subscribed: false,
        };
        if !send(&conn.out, &msg) {
            return;
        }
        if conn.ready && conn.role == Role::App {
            log!("App 已连接（连接 {id}，build={}）", hello["build_id"]);
            self.app = Some(id);
        }
        self.conns.insert(id, conn);
    }

    /// 连接关闭或被丢弃。App 断开即关闭它的非保活终端（M1 的 PTY 在 App 里；保活终端由后台持有，照常运行，docs/protocol.md 第 7 节）。
    fn drop_conn(&mut self, id: u64, why: &str) {
        if self.conns.remove(&id).is_none() {
            return;
        }
        if self.app == Some(id) {
            self.app = None;
            self.focused = None;
            log!("App 断开（连接 {id}，{why}），关闭其非保活终端");
            if let Err(Fail(_, e)) = self.close_all("app_exit") {
                log!("关闭终端失败：{e}");
            }
        }
    }

    fn send_to(&mut self, id: u64, msg: &Value) {
        let ok = self.conns.get(&id).is_some_and(|c| send(&c.out, msg));
        if !ok {
            self.drop_conn(id, "输出队列已满或连接已断开");
        }
    }

    /// 推送给所有已订阅的连接，`rev` 单调递增。
    fn push(&mut self, mut msg: Value) {
        self.rev += 1;
        msg["rev"] = json!(self.rev);
        let dead: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, c)| c.subscribed && !send(&c.out, &msg))
            .map(|(id, _)| *id)
            .collect();
        for id in dead {
            self.drop_conn(id, "推送队列已满或连接已断开");
        }
    }

    // ---- 请求 ----

    fn request(&mut self, id: u64, msg: &Value) {
        let Some((ready, is_app)) = self.conns.get(&id).map(|c| (c.ready, c.role == Role::App))
        else {
            return;
        };
        let req_id = msg.get("id").cloned().unwrap_or(Value::Null);
        let kind = msg["type"].as_str().unwrap_or("");
        if kind == "shutdown" {
            return self.shutdown(id, req_id, msg["force"].as_bool().unwrap_or(false));
        }
        if kind == "upgrade" && ready && is_app {
            match msg["path"].as_str().filter(|p| p.starts_with('/')) {
                Some(path) => self.pending_upgrade = Some((id, req_id, path.to_owned())),
                None => self.send_to(id, &json!({"type": "reply", "id": req_id, "ok": false,
                    "error": {"code": "bad_request", "message": "path 须为绝对路径"}})),
            }
            return;
        }
        // 只读，命令行也可用（排查用，与 hooks.status 相同）。
        if kind == "prompts.list" && ready {
            return self.prompts_list(id, req_id, msg);
        }
        if !ready {
            return;
        }
        let result = match kind {
            "sync"
            | "terminal.open"
            | "terminal.exit"
            | "terminal.rename"
            | "project.update"
            | "terminal.foreground"
            | "terminal.focus"
            | "terminal.cwd"
            | "conversation.resume"
            | "conversation.delete"
            | "status.watch"
            | "proc.stats"
            | "appearance.set"
            | "retention.set"
            | "history.clear"
                if !is_app =>
            {
                Err(bad("只有 app 角色可用"))
            }
            "sync" => self.sync(msg),
            "terminal.open" => self.terminal_open(msg),
            "terminal.exit" => self.terminal_exit(msg),
            "terminal.close" => self.terminal_close(msg),
            "appearance.set" => self.appearance_set(msg),
            "terminal.rename" => self.terminal_rename(msg),
            "project.update" => self.project_update(msg),
            "recent.query" => self.recent(msg),
            "terminal.foreground" => self.terminal_foreground(msg),
            "terminal.focus" => self.terminal_focus(msg),
            "terminal.cwd" => self.terminal_cwd(msg),
            "conversation.resume" => self.conversation_resume(msg),
            "conversation.delete" => self.conversation_delete(msg),
            "status.watch" => self.status_watch(msg),
            "proc.stats" => self.proc_stats(msg),
            "search" => self.search(msg),
            "snapshot.get" => self.snapshot().map(|s| json!({ "snapshot": s })),
            "hooks.status" => Ok(json!({ "hooks": self.hooks_json() })),
            "hooks.plan" => self.hooks_plan(msg),
            "hooks.repair" => self.hooks_repair(msg),
            "retention.get" => Ok(json!({ "retention": self.retention.to_json() })),
            "retention.set" => self.retention_set(msg),
            "history.clear" => self.history_clear(msg),
            _ => Err(Fail("unknown_type", format!("未知请求类型「{kind}」"))),
        };
        let ok = result.is_ok();
        let mut reply = json!({"type": "reply", "id": req_id, "ok": ok});
        match result {
            Ok(Value::Object(extra)) => reply.as_object_mut().unwrap().extend(extra),
            Ok(_) => {}
            Err(Fail(code, message)) => {
                reply["error"] = json!({"code": code, "message": message});
            }
        }
        self.send_to(id, &reply);
        // 快照之后才开始推送，推送的 rev 一定大于快照的 rev。
        if kind == "sync" && ok {
            if let Some(c) = self.conns.get_mut(&id) {
                c.subscribed = true;
            }
        }
    }

    /// 已有 App 连接时，只接受该 App 自己发来的 `shutdown`；有存活的保活终端时只接受带 `force` 的，并先结束
    /// 全部会话（docs/protocol.md 第 1 节）。
    fn shutdown(&mut self, id: u64, req_id: Value, force: bool) {
        if self.app.is_some_and(|app| app != id) {
            let msg = json!({"type": "reply", "id": req_id, "ok": false,
                "error": {"code": "app_connected", "message": "已有 App 连接，只接受该 App 的 shutdown"}});
            return self.send_to(id, &msg);
        }
        let held = self.host.len();
        if held > 0 && !force {
            let msg = json!({"type": "reply", "id": req_id, "ok": false,
                "error": {"code": "sessions_alive", "message": format!("还有 {held} 个保活终端，带 force 才结束它们并退出")}});
            return self.send_to(id, &msg);
        }
        if held > 0 {
            log!("shutdown（force）：结束 {held} 个保活终端");
            self.host.hangup_all();
        }
        log!("收到 shutdown（连接 {id}），退出");
        let _ = fs::remove_file(&self.sock);
        if let Err(e) = self.store.checkpoint() {
            log!("合并 WAL 失败：{e}");
        }
        if let Some(c) = self.conns.get(&id) {
            send(&c.out, &json!({"type": "reply", "id": req_id, "ok": true}));
            let (tx, rx) = mpsc::channel();
            if c.out.try_send(Out::Flush(tx)).is_ok() {
                let _ = rx.recv_timeout(Duration::from_millis(500));
            }
        }
        std::process::exit(0);
    }

    fn sync(&mut self, msg: &Value) -> Result<Value, Fail> {
        let list = msg["terminals"]
            .as_array()
            .ok_or_else(|| bad("缺少 terminals"))?;
        let now = now_ms();
        let mut listed = HashSet::new();
        // 后台被结束或重启电脑后丢掉的终端（下面两种关闭原因）：关闭前记下目录、名称与当时的对话，随快照交给 App
        // 按设置恢复（docs/design.md 10.5 节决策 12）。用户主动结束的终端此前已正常关闭，不在其中。
        let mut restorable = Vec::new();
        for t in list {
            let id = t["terminal_id"].as_str().unwrap_or("");
            // 保活终端而后台没有它的 PTY，且不是本进程刚登记、还在等 attach 的：后台崩溃重启过（M3）。
            let pending = self.panes.get(id).is_some_and(|p| p.held);
            if t["held"] == true && !self.host.has(id) && !pending {
                if let Some(row) = self.store.terminal(id)?.filter(|t| t["status"] == "alive") {
                    restorable.push(self.restorable(id, &row)?);
                    self.close_terminal(id, "pty_lost", None, now)?;
                }
                continue;
            }
            let id = self.open_terminal(t, now)?;
            listed.insert(id);
        }
        for id in self.store.alive_terminals()? {
            // 后台持有 PTY 的终端即使没被列出也保留：App 重启后据快照接回（M3）。
            if !listed.contains(&id) && !self.host.has(&id) {
                if let Some(row) = self.store.terminal(&id)? {
                    restorable.push(self.restorable(&id, &row)?);
                }
                self.close_terminal(&id, "missing_on_sync", None, now)?;
            }
        }
        let mut snapshot = self.snapshot()?;
        if !restorable.is_empty() {
            restorable.sort_by_key(|r| r["created_at"].as_i64().unwrap_or(0));
            log!("同步：{} 个终端已随上一个后台丢失，交给 App 按设置恢复", restorable.len());
            snapshot["restorable"] = json!(restorable);
        }
        if !self.respawned.is_empty() {
            let list: Vec<Value> = self
                .respawned
                .drain(..)
                .map(|m| json!({"terminal_id": m["terminal_id"], "conversation_id": m["conversation_id"], "fg_tool": m["fg_tool"]}))
                .collect();
            snapshot["respawned"] = json!(list);
        }
        Ok(json!({ "snapshot": snapshot }))
    }

    /// 一个要恢复的终端：断电前的当前目录（没有记录时为启动目录）、启动目录、名称与当时绑定的未结束对话。
    fn restorable(&self, id: &str, row: &Value) -> Result<Value, Fail> {
        // 没有未结束的对话、但有待确认的结束：上一个后台在确认期内就没了，这段对话是关机时被结束的（缺陷 B）。
        let conversation = match self.store.open_conversation_on(id)?.map(|c| c.0) {
            Some(c) => Some(c),
            None => self.workspace.ended(id).inspect(|c| log!("终端 {id} 在关机时结束的对话 {c}，交给 App 恢复")),
        };
        let dir = self.workspace.get(id).map(str::to_owned).or_else(|| row["cwd"].as_str().map(str::to_owned));
        Ok(json!({"terminal_id": id, "dir": dir, "cwd": row["cwd"], "name": row["name"],
                  "conversation_id": conversation, "created_at": row["created_at"],
                  "fg_tool": self.workspace.agent(id)}))
    }

    fn terminal_open(&mut self, msg: &Value) -> Result<Value, Fail> {
        self.open_terminal(msg, now_ms())?;
        Ok(Value::Null)
    }

    fn terminal_exit(&mut self, msg: &Value) -> Result<Value, Fail> {
        let id = msg["terminal_id"]
            .as_str()
            .ok_or_else(|| bad("缺少 terminal_id"))?;
        if !self.close_terminal(id, "exit", msg["exit_code"].as_i64(), now_ms())? {
            return Err(Fail("not_found", format!("终端 {id} 不存在或已关闭")));
        }
        Ok(Value::Null)
    }

    fn terminal_rename(&mut self, msg: &Value) -> Result<Value, Fail> {
        let id = msg["terminal_id"]
            .as_str()
            .ok_or_else(|| bad("缺少 terminal_id"))?;
        let name = msg["name"].as_str().filter(|n| !n.is_empty());
        if !self.store.rename_terminal(id, name)? {
            return Err(Fail("not_found", format!("终端 {id} 不存在")));
        }
        self.push_terminal(id, None)?;
        Ok(Value::Null)
    }

    fn project_update(&mut self, msg: &Value) -> Result<Value, Fail> {
        let id = msg["project_id"]
            .as_i64()
            .ok_or_else(|| bad("缺少 project_id"))?;
        let name = msg["name"].as_str().filter(|n| !n.is_empty());
        let pinned = match &msg["pinned"] {
            Value::Null => None,
            v => Some(v.as_bool().ok_or_else(|| bad("pinned 须为布尔"))?),
        };
        if !self
            .store
            .update_project(id, name, msg["sort_order"].as_i64(), pinned)?
        {
            return Err(Fail("not_found", format!("项目 {id} 不存在")));
        }
        self.push_project(id)?;
        Ok(Value::Null)
    }

    fn recent(&self, msg: &Value) -> Result<Value, Fail> {
        let before = match &msg["before"] {
            Value::Null => None,
            b => Some((
                b["ended_at"]
                    .as_i64()
                    .ok_or_else(|| bad("before 缺少 ended_at"))?,
                b["conversation_id"]
                    .as_i64()
                    .ok_or_else(|| bad("before 缺少 conversation_id"))?,
            )),
        };
        let limit = msg["limit"].as_i64().unwrap_or(50).clamp(1, 200);
        let (items, next) =
            self.store
                .recent(msg["project_id"].as_i64(), before, msg["since"].as_i64(), limit)?;
        let total = self.store.recent_total(now_ms())?;
        Ok(json!({"items": items, "next": next, "recent_total": total}))
    }

    /// 搜索（M2 第 4 块，docs/protocol.md 第 7 节「搜索」）：存活终端逐个比对（Unicode 小写），已结束的对话交给 SQLite。
    fn search(&self, msg: &Value) -> Result<Value, Fail> {
        let query = msg["query"].as_str().unwrap_or("").trim();
        if query.is_empty() || query.chars().count() > 200 {
            return Err(bad("query 为空或超过 200 个字符"));
        }
        let limit = msg["limit"].as_i64().unwrap_or(500).clamp(1, 500);
        let q = query.to_lowercase();
        let mut terminals = Vec::new();
        for id in self.store.alive_terminals()? {
            let Some(t) = self.store.terminal(&id)? else { continue };
            let pane = self.panes.get(&id);
            let conv = match pane.and_then(|p| p.conversation) {
                Some(cid) => self.store.conversation(cid)?,
                None => None,
            };
            let project = match t["project_id"].as_i64() {
                Some(pid) => self.store.project(pid)?,
                None => None,
            };
            let field = |v: Option<&Value>, k: &str| v.and_then(|v| v[k].as_str()).map(str::to_owned);
            let fields = [
                field(Some(&t), "name"),
                field(Some(&t), "cwd"),
                pane.and_then(|p| p.current_dir.clone()),
                field(conv.as_ref(), "title"),
                field(conv.as_ref(), "last_prompt"),
                field(conv.as_ref(), "last_reply"),
                field(project.as_ref(), "name"),
                field(project.as_ref(), "dir"),
            ];
            if fields.iter().flatten().any(|f| f.to_lowercase().contains(&q)) {
                terminals.push(id);
            }
        }
        let conversations = self.store.search_ended(query, limit)?;
        Ok(json!({"terminals": terminals, "conversations": conversations}))
    }

    fn snapshot(&self) -> Result<Value, Fail> {
        let mut terminals = Vec::new();
        for id in self.store.alive_terminals()? {
            terminals.extend(self.terminal_json(&id)?);
        }
        let projects: Vec<Value> = self
            .store
            .projects()?
            .into_iter()
            .map(|p| self.with_branch(p))
            .collect();
        Ok(json!({
            "rev": self.rev,
            "projects": projects,
            "terminals": terminals,
            "conversations": self.store.active_conversations()?,
            "recent_total": self.store.recent_total(now_ms())?,
            "hooks": self.hooks_json(),
            "usage": self.usage.iter().map(|(cid, u)| json!({"conversation_id": cid, "usage": usage_json(u)})).collect::<Vec<_>>(),
            "quotas": self.quotas.iter().map(|((tool, _), q)| json!({"tool": tool, "quota": quota_json(q)})).collect::<Vec<_>>(),
            "quota_notes": self.quota_notes.iter().map(|(tool, note)| json!({"tool": tool, "note": note})).collect::<Vec<_>>(),
            "last_focus": self.last_focus.as_ref().filter(|id| self.panes.contains_key(id.as_str())),
            "tools": tools_json(),
        }))
    }

    // ---- Hook 配置（M1 第 7 块，docs/protocol.md 第 4、7 节） ----

    pub fn guard_real_config(mut self, on: bool) -> Core {
        self.real_config_guard = on;
        self
    }

    /// 测试后台：`pty` 握手的 `spawn.command` 生效（docs/protocol.md 第 3.1 节）。
    pub fn allow_command(mut self, on: bool) -> Core {
        self.allow_command = on;
        self
    }

    pub fn lock_fd(mut self, fd: RawFd) -> Core {
        self.lock_fd = fd;
        self
    }

    pub fn listener(mut self, fd: RawFd, gate: crate::conn::AcceptGate) -> Core {
        self.listener = Some((fd, gate));
        self
    }

    pub fn approvals(mut self, watch: crate::approval::ApprovalWatch) -> Core {
        self.approval_watch = Some(watch);
        self
    }

    pub fn prompts(mut self, worker: PromptWorker) -> Core {
        self.prompts = Some(worker);
        self
    }

    pub fn workspace(mut self, dirs: WorkspaceDirs) -> Core {
        self.workspace = dirs;
        self
    }

    pub fn hook_times(mut self, times: Timeline) -> Core {
        self.hook_times = times;
        self
    }

    /// 接管旧进程的会话后（main.rs 已交给 PTY 宿主，宿主已开始等待 Shell 退出）：标为保活终端，还原只在内存中的终端提示
    /// 与当前目录（否则换版本后「目录无权限」消失、副标题退回启动目录，2026-10-23 修正）。库里已不存活的终端直接挂断。
    pub fn adopt(&mut self, adopted: Vec<pty::takeover::Adopted>) {
        for a in adopted {
            let id = a.terminal_id;
            let agent = a.agent;
            match self.panes.get_mut(&id) {
                Some(p) => {
                    p.held = true;
                    if let Some(n) = a.notice.as_deref().and_then(|n| ["cwd_denied", "cwd_missing"].into_iter().find(|k| *k == n)) {
                        p.notice = Some(n);
                    }
                    if a.current_dir.is_some() {
                        p.current_dir = a.current_dir;
                    }
                }
                None => {
                    log!("接管来的终端 {id} 在库里已不存活，挂断");
                    self.host.hangup(&id);
                }
            }
            // 前台立即读一次：否则侧栏要等下一次触发才认出正在运行的 CLI（2026-10-17 实测四款 CLI 都暂时显示为 Shell）。
            // 读到之后按它核对并还原 Agent 状态（2026-11-14；旧版本的接管文件不带状态，回到「未知」）。
            self.refresh_foreground(&id);
            if let Some(agent) = &agent {
                self.restore_agent(&id, agent);
            }
        }
    }

    /// 换版本接管（docs/protocol.md 第 1 节「接管」）：新程序自检通过后冻结读线程、写接管文件、应答，再 execv 新程序。
    /// 失败时留在本进程，会话不动。
    /// 不丢事件（2026-11-14，docs/design.md 第 7 节「换版本」）：先叫停接收连接（此后连进来的留在监听 socket 的队列里，
    /// 监听 socket 交给新进程），把已经收到的命令处理完，再把各终端的 Agent 状态写进接管文件。
    fn upgrade(&mut self, rx: &Receiver<Cmd>, id: u64, req_id: Value, path: &str) {
        let fail = |code: &str, message: String| {
            json!({"type": "reply", "id": req_id, "ok": false, "error": {"code": code, "message": message}})
        };
        let data_dir = self.sock.parent().map(Path::to_path_buf).unwrap_or_default();
        if let Err(e) = preflight(path, &data_dir) {
            log!("换版本自检失败，留在本进程：{e}");
            return self.send_to(id, &fail("preflight_failed", e));
        }
        let paused = self.listener.as_ref().is_some_and(|(_, gate)| gate.pause());
        let resume = |core: &Core| {
            if let Some((_, gate)) = core.listener.as_ref().filter(|_| paused) {
                gate.resume();
            }
        };
        // 已经接下的连接送来的命令处理完：`UPGRADE_QUIET` 内没有新的为止，最多 `UPGRADE_DRAIN`。接回进程的握手不处理
        // （丢掉应答通道，它稍后重连到新进程）。
        let until = Instant::now() + UPGRADE_DRAIN;
        while let Ok(cmd) = rx.recv_timeout(UPGRADE_QUIET) {
            if !matches!(cmd, Cmd::PtyHello { .. }) {
                self.handle(cmd);
            }
            if Instant::now() >= until {
                break;
            }
        }
        // 处理期间又来的 `upgrade` 不重复执行。
        self.pending_upgrade = None;
        if !self.host.freeze(Duration::from_secs(2)) {
            self.host.thaw();
            resume(self);
            log!("换版本：读线程没能全部停下，留在本进程");
            return self.send_to(id, &fail("internal", "读线程没能全部停下".into()));
        }
        let mut entries = self.host.export();
        for e in &mut entries {
            if let Some(p) = self.panes.get(&e.terminal_id) {
                e.notice = p.notice.map(str::to_owned);
                e.current_dir = p.current_dir.clone();
            }
            e.agent = self.agent_handoff(&e.terminal_id);
        }
        let listen_fd = self.listener.as_ref().filter(|_| paused).map(|(fd, _)| *fd);
        let keep: Vec<RawFd> = entries.iter().map(|e| e.master_fd).chain([self.lock_fd]).chain(listen_fd).collect();
        let file = pty::takeover::file_path(&data_dir);
        let handoff = pty::takeover::Handoff { lock_fd: self.lock_fd, entries, appearance: self.host.appearance(), listen_fd };
        let n = handoff.entries.len();
        if let Err(e) = pty::takeover::write(&file, &handoff) {
            self.host.thaw();
            resume(self);
            log!("写接管文件失败，留在本进程：{e}");
            return self.send_to(id, &fail("internal", e.to_string()));
        }
        if let Err(e) = self.store.checkpoint() {
            log!("合并 WAL 失败：{e}");
        }
        log!("换版本：交出 {n} 个保活终端，exec {path}");
        if let Some(c) = self.conns.get(&id) {
            send(&c.out, &json!({"type": "reply", "id": req_id, "ok": true}));
            let (tx, rx) = mpsc::channel();
            if c.out.try_send(Out::Flush(tx)).is_ok() {
                let _ = rx.recv_timeout(Duration::from_millis(500));
            }
        }
        // socket 文件不删：监听描述符是 CLOEXEC，exec 后关闭，新进程（沿用锁）按启动流程删掉遗留文件再绑定；
        // exec 失败时本进程照常监听。
        pty::takeover::prepare_exec(&keep);
        let (dir, file_arg) = (data_dir.to_string_lossy().into_owned(), file.to_string_lossy().into_owned());
        let mut args = vec!["--data-dir", &dir, "--takeover", &file_arg];
        let capture = self.capture.as_ref().map(|c| c.to_string_lossy().into_owned());
        if let Some(c) = &capture {
            args.extend(["--capture", c.as_str()]);
        }
        let err = exec(path, &args);
        // 走到这里说明 exec 失败：恢复描述符的 CLOEXEC，会话照旧，继续接收连接。
        log!("exec 新程序失败，留在本进程：{err}");
        for fd in &keep {
            pty::takeover::set_cloexec(*fd, true);
        }
        let _ = fs::remove_file(&file);
        self.host.thaw();
        resume(self);
    }

    /// 交给新进程的 Agent 状态（接管文件每个终端的 `agent`，docs/protocol.md 第 1 节「接管」）；没有识别出工具的不带。
    /// 前台 AI 的进程号与启动时间供新进程核对还是不是同一个进程。
    fn agent_handoff(&self, id: &str) -> Option<Value> {
        let p = self.panes.get(id)?;
        let tool = p.state.tool?;
        let fg_pid = match p.foreground {
            Foreground::Agent { pid, tool: t, .. } if t == tool => Some(pid),
            _ => None,
        };
        Some(json!({
            "state": p.state.handoff(),
            "conversation_id": p.conversation,
            "unseen": p.unseen,
            "fg_pid": fg_pid,
            "fg_start_ms": fg_pid.and_then(procinfo::start_ms),
            "silent": p.silent.filter(|(t, q, _)| *t == tool && Some(*q) == fg_pid).map(|(_, _, at)| at),
            "waiting": self.waits.contains_key(id),
            "wait_notice": self.waits.get(id).and_then(|w| w.notice.clone()),
            "wait_seq": self.wait_seq.get(id),
            "done_notice": self.dones.get(id),
            "done_seq": self.done_seq.get(id),
        }))
    }

    /// 还原接管文件带来的 Agent 状态：前台仍是同一个进程（进程号与启动时间）、同一款 AI，绑定的对话与会话也相同，
    /// 且存下的状态不是「未知」「已结束」，才还原；否则维持「未知」，不猜。通知的 ID 与序号一并还原，不重发。
    fn restore_agent(&mut self, id: &str, a: &Value) {
        let Some(p) = self.panes.get_mut(id) else { return };
        let Some(state) = PaneState::from_handoff(&a["state"]) else { return };
        let same_process = match p.foreground {
            Foreground::Agent { pid, tool, .. } => {
                Some(tool) == state.tool
                    && a["fg_pid"].as_i64() == Some(pid as i64)
                    && a["fg_start_ms"].as_i64().is_some_and(|t| procinfo::start_ms(pid) == Some(t))
            }
            _ => false,
        };
        // 会话记录对账的结论（4.7 节）跟着进程走：还是同一个进程就带过来，不论下面状态还不还原。
        if let (true, Some(at), Foreground::Agent { pid, tool, .. }) = (same_process, a["silent"].as_i64(), &p.foreground) {
            p.silent = Some((*tool, *pid, at));
        }
        if matches!(state.state, AgentState::Unknown | AgentState::Ended) {
            return;
        }
        if !same_process || p.conversation != a["conversation_id"].as_i64() || p.state.session_id != state.session_id {
            log!("接管：终端 {id} 的前台或对话已不是交接时的那个，状态维持未知");
            return;
        }
        log!("接管：终端 {id} 还原为 {}", state.state.as_str());
        let (tool, cid) = (state.tool, p.conversation);
        // 状态能还原，说明交接前这个进程在同步：记作刚收到过它的事件，免得换版本后误判成没在同步。交接前已经对账出
        // 没在同步的不记。
        if let (Some(t), None) = (tool, p.silent) {
            p.last_hook = Some((t, now_ms()));
        }
        p.state = state;
        p.unseen = a["unseen"] == true;
        p.resync = true;
        if let Some(n) = a["wait_seq"].as_u64() {
            self.wait_seq.insert(id.to_owned(), n);
        }
        if a["waiting"] == true {
            self.waits.insert(id.to_owned(), Wait { notice: a["wait_notice"].as_str().map(str::to_owned) });
        }
        if let Some(n) = a["done_seq"].as_u64() {
            self.done_seq.insert(id.to_owned(), n);
        }
        if let Some(n) = a["done_notice"].as_str() {
            self.dones.insert(id.to_owned(), n.to_owned());
        }
        // 模型、用量等只在内存里的补充信息重新读一次。
        if let (Some(tool), Some(cid)) = (tool, cid) {
            self.request_title(tool, cid, Kind::SessionStart);
        }
    }

    // ---- 保活终端（M3，docs/design.md 第 7 节「实现（M3）」） ----

    /// `pty` 握手：终端须已登记且存活；还没有会话时按 hello 起 Shell。
    fn pty_hello(&mut self, hello: &Value) -> PtyAdmit {
        let line = |mut v: Value| {
            v["protocol_version"] = json!(PROTOCOL_VERSION);
            v["build_id"] = json!(BUILD_ID);
            v["pid"] = json!(std::process::id());
            let mut b = v.to_string().into_bytes();
            b.push(b'\n');
            b
        };
        let err = |code: &str| line(json!({"type": "hello_err", "code": code}));
        if hello["protocol_version"] != PROTOCOL_VERSION {
            return Err(err("protocol_mismatch"));
        }
        let Some(id) = hello["terminal_id"].as_str() else {
            return Err(err("bad_hello"));
        };
        if !self.panes.contains_key(id) {
            log!("pty 握手被拒：终端 {id} 不存在或已关闭");
            return Err(err("not_found"));
        }
        let ok = |created: bool| line(json!({"type": "hello_ok", "pty_host": true, "created": created}));
        if let Some(s) = self.host.get(id) {
            return Ok((s, ok(false)));
        }
        let sp = &hello["spawn"];
        // 启动目录以 App 登记的为准：attach 进不了该目录时 libghostty 会悄悄退回别处，它报的目录不可靠
        // （2026-10-17 实测，目录已删除时 Shell 起在了上一个目录、CLI 在错误的目录启动）。
        let cwd = self
            .store
            .terminal(id)
            .ok()
            .flatten()
            .and_then(|t| t["cwd"].as_str().filter(|c| c.starts_with('/')).map(str::to_owned))
            .or_else(|| sp["cwd"].as_str().filter(|c| c.starts_with('/')).map(str::to_owned))
            .unwrap_or_else(|| "/".into());
        let base_env = sp["env"]
            .as_object()
            .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned()))).collect())
            .unwrap_or_default();
        let command = sp["command"]
            .as_array()
            .filter(|_| self.allow_command)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect());
        let (cols, rows, cell_w, cell_h) = pty::size_of(hello);
        let spec = SpawnSpec { cwd, base_env, command, cols, rows, cell_w, cell_h };
        let input = self.panes.get_mut(id).and_then(|p| p.pending_input.take());
        let session = match self.host.spawn(id, &spec, input) {
            Ok(s) => s,
            Err(e) => {
                log!("起 Shell 失败：terminal={id} {e}");
                return Err(err("spawn_failed"));
            }
        };
        let inherited = self.panes.get_mut(id).is_some_and(|p| {
            p.held = true;
            std::mem::take(&mut p.inherited)
        });
        if inherited {
            if let Err(Fail(_, e)) = self.respawned_with_conversation(id) {
                log!("处理重起 Shell 的终端 {id} 失败：{e}");
            }
        }
        if let Err(Fail(_, e)) = self.push_terminal(id, None) {
            log!("推送终端失败：{e}");
        }
        Ok((session, ok(true)))
    }

    /// 为旧后台留下的终端（`Pane::inherited`）新起了 Shell，而它还绑着未结束的对话：后台崩溃重启过（换版本走 exec 接管、
    /// 会话不断），原来的 AI 已随旧后台结束。只看 `inherited`：一键恢复新开的终端登记时已预先绑了对话，不能算在内
    /// （2026-10-25 实测误判，把恢复命令又打进了正在运行的 Codex）。结束这段对话，请 App 按设置在同一终端代输恢复命令
    /// （docs/design.md 10.5 节决策 21）。App 已订阅推送即推，否则随下一次 `sync` 的快照交出。
    fn respawned_with_conversation(&mut self, id: &str) -> Result<(), Fail> {
        // 原来前台的 AI 已随旧后台结束，记录随之清除（M4b 块 5 P2）。待确认的结束也清掉：后台崩溃时 Agent 随后才死、
        // 对话仍未结束，走下面未结束对话的路径；之前已结束的那段是用户自己退出的，不接回。
        let agent = self.workspace.agent(id).map(str::to_owned);
        self.workspace.set_agent(id, None);
        self.workspace.set_ended(id, None);
        let Some((cid, ..)) = self.store.open_conversation_on(id)? else {
            // 没有可恢复的对话、但前台原来在跑某款 AI（多半状态同步没生效）：也告诉 App，由它说明没能接回的原因
            // （M4b 块 5 P2，docs/design.md 10.6 节）。
            if let Some(tool) = agent {
                log!("终端 {id} 随后台重启重起了 Shell，原来前台的 {tool} 没有可恢复的对话");
                let msg = json!({"type": "terminal.respawned", "terminal_id": id, "fg_tool": tool});
                if self.conns.values().any(|c| c.subscribed) {
                    self.push(msg);
                } else {
                    self.respawned.push(msg);
                }
            }
            return Ok(());
        };
        if let Some(p) = self.panes.get_mut(id) {
            p.state.state = AgentState::Ended;
        }
        self.store.end_conversation(cid, "process_exit", now_ms())?;
        self.push_conversation(cid)?;
        log!("终端 {id} 随后台重启重起了 Shell，原对话 {cid} 已结束，交给 App 恢复");
        let msg = json!({"type": "terminal.respawned", "terminal_id": id, "conversation_id": cid});
        if self.conns.values().any(|c| c.subscribed) {
            self.push(msg);
        } else {
            self.respawned.push(msg);
        }
        Ok(())
    }

    fn pty_event(&mut self, id: &str, event: pty::Event) {
        if !self.panes.contains_key(id) {
            return;
        }
        let r = match event {
            pty::Event::Title => {
                self.refresh_foreground(id);
                if let Some(p) = self.panes.get_mut(id) {
                    p.fg_followup = Some(Instant::now() + TITLE_FOLLOWUP);
                }
                Ok(())
            }
            pty::Event::Cwd(cwd) => self.set_current_dir(id, &cwd).map(|_| self.refresh_foreground(id)),
            pty::Event::CommandEnd | pty::Event::Attached => {
                self.refresh_foreground(id);
                Ok(())
            }
            pty::Event::ChdirFailed(errno) => {
                // 目录不存在时 Shell 已退回主目录（`spawn.rs`），提示「目录不存在」而不是「无权限」。
                let notice = if matches!(errno, libc::ENOENT | libc::ENOTDIR) { "cwd_missing" } else { "cwd_denied" };
                log!("终端 {id} 进不了启动目录（errno {errno}），标 {notice}");
                if let Some(p) = self.panes.get_mut(id) {
                    p.notice = Some(notice);
                }
                self.push_terminal(id, None)
            }
            pty::Event::Failed => {
                self.host.hangup(id);
                self.close_terminal(id, "exit", None, now_ms()).map(|_| ())
            }
        };
        if let Err(Fail(_, e)) = r {
            log!("处理终端 {id} 的 PTY 事件失败：{e}");
        }
    }

    /// 保活终端：对主端读前台进程组并识别（M3）。非保活终端什么也不做（由 App 上报）。
    fn refresh_foreground(&mut self, id: &str) {
        let Some(pgid) = self.host.foreground_pgid(id) else {
            return;
        };
        if let Err(Fail(_, e)) = self.apply_foreground(id, pgid) {
            log!("读取终端 {id} 的前台失败：{e}");
        }
    }

    fn expire_followups(&mut self, now: Instant) {
        let due: Vec<String> = self
            .panes
            .iter_mut()
            .filter_map(|(id, p)| match p.fg_followup {
                Some(t) if t <= now => {
                    p.fg_followup = None;
                    Some(id.clone())
                }
                _ => None,
            })
            .collect();
        for id in due {
            self.refresh_foreground(&id);
        }
    }

    /// 关闭终端（docs/protocol.md 第 4 节 `terminal.close`）：保活终端在光标不在提示符时先要确认。
    fn terminal_close(&mut self, msg: &Value) -> Result<Value, Fail> {
        let id = msg["terminal_id"].as_str().ok_or_else(|| bad("缺少 terminal_id"))?;
        if !self.panes.contains_key(id) {
            return Err(Fail("not_found", format!("终端 {id} 不存在或已关闭")));
        }
        if let Some(at_prompt) = self.host.at_prompt(id) {
            if msg["confirmed"] != true && !at_prompt {
                return Ok(json!({"closed": false, "needs_confirm": true}));
            }
            log!("关闭保活终端 {id}（在提示符={at_prompt}）");
            self.host.hangup(id);
        }
        self.close_terminal(id, "exit", None, now_ms())?;
        Ok(json!({"closed": true, "needs_confirm": false}))
    }

    /// 断开期间代答查询所用的外观（M3）。
    fn appearance_set(&mut self, msg: &Value) -> Result<Value, Fail> {
        let scheme = match msg["color_scheme"].as_str() {
            None => None,
            Some("light") => Some(1),
            Some("dark") => Some(2),
            Some(_) => return Err(bad("color_scheme 须为 light 或 dark")),
        };
        let color = |k: &str| -> Result<Option<[u8; 3]>, Fail> {
            match msg[k].as_str() {
                None => Ok(None),
                Some(c) => parse_rgb(c).map(Some).ok_or_else(|| bad(&format!("{k} 须为 #rrggbb"))),
            }
        };
        let (fg, bg) = (color("foreground")?, color("background")?);
        self.host.set_appearance(|a| {
            if let Some(s) = scheme {
                a.color_scheme = s;
            }
            if fg.is_some() {
                a.foreground = fg;
            }
            if bg.is_some() {
                a.background = bg;
            }
        });
        Ok(Value::Null)
    }

    /// 四款的状态；监听线程还没给出结果时当场分析一次。
    fn current_hooks(&mut self) -> &[Status] {
        if self.hooks.is_empty() {
            let statuses = analyze_all(&self.cli_home);
            self.observe_hooks(&statuses, true);
            self.hooks = statuses;
        }
        &self.hooks
    }

    /// 每款附上版本（M4b 块 6，「设置 › Agent」显示）：优先正在终端里运行的，没在运行时取最近一次对话记下的；都没有不附。
    /// 另附清单里最新的已验证版本（「状态可能不准」的详情里给用户作退回参考）。
    /// 再附主配置文件的修改时间 `changed_at`（2026-11-17）：App 拿它与 Codex 的启动时间比较，决定怎样引导用户信任
    /// （docs/design.md 4.7 节「引导用户信任」）；每次现读，不走缓存，文件不存在或读不到时不附。
    fn hooks_json(&self) -> Value {
        let list = if self.hooks.is_empty() { analyze_all(&self.cli_home) } else { self.hooks.clone() };
        json!(list
            .iter()
            .map(|s| {
                let mut v = s.to_json();
                // 何时读取 Hook 与引导里要写的命令（docs/design.md 4.7 节「会话是否真的在同步」）。
                let a = adapter(s.tool);
                v["load"] = json!(a.hook_load.as_str());
                v["command"] = json!(a.command);
                // 这款配置状态最近一次变化的时间（时间线）：引导里写「写入 Hook 的时间」用，比文件修改时间准。
                if let Some(at) = self.hook_times.changed_at(s.tool) {
                    v["state_changed_at"] = json!(at);
                }
                for (key, cmd) in [("new_session", a.guide.new_session), ("quit", a.guide.quit), ("resume_last", a.guide.resume_last)] {
                    if let Some(cmd) = cmd {
                        v[key] = json!(cmd);
                    }
                }
                let changed = std::fs::metadata(&s.path).and_then(|m| m.modified()).ok();
                if let Some(d) = changed.and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()) {
                    v["changed_at"] = json!(d.as_millis() as i64);
                }
                let running = self
                    .panes
                    .values()
                    .filter_map(|p| match p.foreground {
                        Foreground::Agent { pid, tool, .. } if tool == s.tool => {
                            p.version.as_ref().filter(|v| v.pid == pid).and_then(|v| v.version.clone())
                        }
                        _ => None,
                    })
                    .max();
                let is_running = running.is_some();
                let version = running.or_else(|| self.store.latest_cli_version(s.tool.as_str()).ok().flatten());
                if let Some(ver) = version {
                    v["version_verified"] = json!(verified(s.tool, &ver));
                    v["version_running"] = json!(is_running);
                    v["version"] = json!(ver);
                }
                if let Some(latest) = verified_list(s.tool).last() {
                    v["verified_latest"] = json!(latest);
                }
                v
            })
            .collect::<Vec<_>>())
    }

    fn hooks_changed(&mut self, statuses: Vec<Status>) {
        if statuses == self.hooks {
            return;
        }
        for s in &statuses {
            let before = self.hooks.iter().find(|x| x.tool == s.tool).map(|x| x.state.as_str());
            if before != Some(s.state.as_str()) {
                log!(
                    "Hook 配置状态：{} {} → {}（{}）",
                    s.tool.as_str(),
                    before.unwrap_or("-"),
                    s.state.as_str(),
                    s.problems.join("、")
                );
            }
        }
        // 本进程第一次分析出的结果：与时间线最近一条不同的话，变化发生在后台没在运行的时候，时刻不明。
        let first = self.hooks.is_empty();
        self.observe_hooks(&statuses, first);
        self.hooks = statuses;
        let hooks = self.hooks_json();
        self.push(json!({"type": "hooks.upsert", "hooks": hooks}));
        self.refresh_sync_all();
    }

    /// 把各款此刻的状态记进时间线（与最近一条相同的不记）。
    fn observe_hooks(&mut self, statuses: &[Status], gap: bool) {
        let now = now_ms();
        for s in statuses {
            self.hook_times.observe(s.tool, s.state, now, gap);
        }
    }

    /// 判断这个终端前台 Agent 进程同步情况要用的事实（docs/design.md 4.7 节「会话是否真的在同步」）。前台不是 Agent、
    /// 或配置还没分析出来时为 None。
    fn sync_facts(&self, p: &Pane) -> Option<Facts> {
        let Foreground::Agent { pid, tool, .. } = p.foreground else {
            return None;
        };
        let now = self.hooks.iter().find(|s| s.tool == tool)?.state;
        let started = procinfo::start_ms(pid);
        Some(Facts {
            load: adapter(tool).hook_load,
            now,
            changed: self.hook_times.changed_at(tool),
            started,
            last_hook: p.last_hook.filter(|(t, _)| *t == tool).map(|(_, at)| at),
            at_start: started.and_then(|s| self.hook_times.state_at(tool, s)),
            silent: p.silent.filter(|(t, q, _)| *t == tool && *q == pid).map(|(_, _, at)| at),
        })
    }

    /// 这个终端前台 Agent 进程的同步情况。
    fn sync_of_pane(&self, p: &Pane) -> Option<Sync> {
        hooktime::sync_of(&self.sync_facts(p)?)
    }

    /// 会话记录对账要开的监听（docs/design.md 4.7 节「用会话记录对账」）：前台这款声明了 `submit_record`、结论靠推断
    /// （`hooktime::inferred`）、会话记录定位得到。记录的位置与历史提示词相同（`prompts.rs`）。同一对话在别的终端也
    /// 开着时不核对：记录里的提示词可能是那边提交的。等待核对结果期间（`submit_check`）监听不停。
    fn submit_watch(&self, id: &str, p: &Pane) -> Option<(WatchKey, crate::waitwatch::Start)> {
        use crate::waitwatch::{Source, Start};
        use scyllaui_proto::adapters::reply::ReplySource;
        use scyllaui_proto::adapters::waiting::Scan;
        let Foreground::Agent { pid, tool, .. } = p.foreground else {
            return None;
        };
        let a = adapter(tool);
        if !a.submit_record || p.state.tool != Some(tool) || !p.conflict.is_empty() {
            return None;
        }
        if p.submit_check.is_none() && !hooktime::inferred(&self.sync_facts(p)?) {
            return None;
        }
        let sid = p.state.session_id.as_ref().filter(|s| valid_session_id(s))?;
        let source = match a.reply_source {
            ReplySource::CopilotEvents => Source::Path(Path::new(&self.cli_home).join(a.session_record?(sid))),
            ReplySource::KimiWire => Source::KimiIndex { home: PathBuf::from(&self.cli_home), session: sid.clone() },
            _ => Source::Path(PathBuf::from(p.state.transcript_path.as_ref()?)),
        };
        let key = WatchKey { session: Some(sid.clone()), turn: Some(format!("sync:{pid}")) };
        let job = Start {
            key: format!("{id}#sync"),
            terminal_id: id.to_owned(),
            session_id: Some(sid.clone()),
            source,
            scan: Scan::Submits(a.reply_source),
            turn: None,
            waiting: false,
            resync: false,
            sub: false,
            agent: None,
        };
        Some((key, job))
    }

    /// 会话记录里读到一条用户提交的提示词（`Cmd::RecordSubmit`）：等 `SUBMIT_WAIT_MS`，到时看这个终端的事件到了没有
    /// （`expire_submits`）。已经在等的不重记：先到的那条最早到期。
    fn record_submit(&mut self, tid: &str, session_id: Option<String>, at: Option<i64>) {
        let Some(p) = self.panes.get_mut(tid) else { return };
        if p.state.session_id != session_id || p.submit_check.is_some() {
            return;
        }
        let Foreground::Agent { pid, tool, .. } = p.foreground else { return };
        // 记录里的时间不会晚于现在；没有时间的用读到它的时刻。
        let now = now_ms();
        let at = at.map_or(now, |t| t.min(now));
        let wait = Duration::from_millis(hooktime::SUBMIT_WAIT_MS as u64);
        p.submit_check = Some((Instant::now() + wait, tool, pid, at));
    }

    /// 到期的对账：从提示词时刻前 `hooktime::SUBMIT_EARLY_MS` 起到现在都没收到这个终端这款工具的事件，而前台还是
    /// 那个进程，记为没在同步并重算、推送。
    fn expire_submits(&mut self, now: Instant) {
        let due: Vec<String> =
            self.panes.iter().filter(|(_, p)| p.submit_check.is_some_and(|c| c.0 <= now)).map(|(id, _)| id.clone()).collect();
        if due.is_empty() {
            return;
        }
        for id in due {
            let Some(p) = self.panes.get_mut(&id) else { continue };
            let Some((_, tool, pid, at)) = p.submit_check.take() else { continue };
            let same = matches!(p.foreground, Foreground::Agent { pid: q, tool: t, .. } if q == pid && t == tool);
            let heard = p.last_hook.is_some_and(|(t, e)| t == tool && e >= at - hooktime::SUBMIT_EARLY_MS);
            if !same || heard {
                continue;
            }
            log!("会话记录对账：{id} 的 {} 提交了提示词而事件没到，记为没在同步", tool.as_str());
            p.silent = Some((tool, pid, at));
            if self.refresh_sync(&id) {
                if let Err(Fail(_, e)) = self.push_terminal(&id, None) {
                    log!("推送终端失败：{e}");
                }
            }
        }
        self.refresh_watches();
    }

    /// 重算一个终端的同步情况，返回有没有变。变成「没在同步」的那一刻把 Agent 状态置为「未知」：不保留最后一次事件的
    /// 状态（4.3 节：同步未生效时为「未知」），绑定的对话不动。
    fn refresh_sync(&mut self, id: &str) -> bool {
        let Some(sync) = self.panes.get(id).map(|p| self.sync_of_pane(p)) else {
            return false;
        };
        let Some(p) = self.panes.get_mut(id) else {
            return false;
        };
        if p.sync == sync {
            return false;
        }
        log!("同步情况：{id} {} → {}", p.sync.map_or("-", Sync::as_str), sync.map_or("-", Sync::as_str));
        p.sync = sync;
        if sync == Some(Sync::Off) && !matches!(p.state.state, AgentState::Unknown | AgentState::Ended) {
            log!("同步未生效：{id} 的 Agent 状态由 {} 置为未知", p.state.state.as_str());
            p.state.state = AgentState::Unknown;
        }
        true
    }

    /// Hook 配置状态变化后重算全部终端，有变化的推送。
    fn refresh_sync_all(&mut self) {
        let ids: Vec<String> = self.panes.keys().cloned().collect();
        for id in ids {
            if self.refresh_sync(&id) {
                if let Err(Fail(_, e)) = self.push_terminal(&id, None) {
                    log!("推送终端失败：{e}");
                }
            }
        }
    }

    /// 请求中的 `action`（M4a）：`repair`（缺省）或 `uninstall`。
    fn hook_action(msg: &Value) -> Result<HookAction, Fail> {
        match msg.get("action").and_then(Value::as_str) {
            None | Some("repair") => Ok(HookAction::Repair),
            Some("uninstall") => Ok(HookAction::Uninstall),
            Some(a) => Err(bad(&format!("action 只能是 repair 或 uninstall，收到「{a}」"))),
        }
    }

    /// 请求中的 `tools`；缺省时修复为可修复或未信任的工具，撤销为四款（没有本项目条目的生成不出改动）。
    fn hook_tools(&mut self, msg: &Value, action: HookAction) -> Result<Vec<Tool>, Fail> {
        match msg.get("tools") {
            None | Some(Value::Null) if action == HookAction::Uninstall => Ok(hookcfg::TOOLS.to_vec()),
            None | Some(Value::Null) => Ok(self
                .current_hooks()
                .iter()
                .filter(|s| s.fixable || s.state == HookState::Untrusted)
                .map(|s| s.tool)
                .collect()),
            Some(Value::Array(a)) => a
                .iter()
                .map(|t| t.as_str().and_then(Tool::parse).ok_or_else(|| bad("tools 中有未知工具")))
                .collect(),
            Some(_) => Err(bad("tools 须为数组")),
        }
    }

    /// 为修复或撤销生成改动（不写文件）。不能处理的工具（修复时的策略锁定、读不了）不在结果里，原因见 `hooks.status`。
    fn plans_for(&self, tools: &[Tool], action: HookAction) -> Vec<Plan> {
        tools
            .iter()
            .filter_map(|t| match hookcfg::plan(&hookcfg::read(*t, &self.cli_home), action) {
                Ok(p) => p,
                Err(e) => {
                    log!("{} 不能{}：{e}", t.as_str(), if action == HookAction::Uninstall { "撤销" } else { "修复" });
                    None
                }
            })
            .collect()
    }

    fn hooks_plan(&mut self, msg: &Value) -> Result<Value, Fail> {
        let action = Self::hook_action(msg)?;
        let tools = self.hook_tools(msg, action)?;
        let plans = self.plans_for(&tools, action);
        Ok(json!({"plan": {
            "changes": plans.iter().map(Plan::to_json).collect::<Vec<_>>(),
            "fingerprint": hookcfg::fingerprint(&plans),
        }}))
    }

    /// 按 `hooks.plan` 的结果写入；文件此后被改过（指纹不符）即放弃，请求方重新生成。
    fn hooks_repair(&mut self, msg: &Value) -> Result<Value, Fail> {
        let action = Self::hook_action(msg)?;
        let tools = self.hook_tools(msg, action)?;
        let fingerprint = msg["fingerprint"].as_str().ok_or_else(|| bad("缺少 fingerprint"))?;
        if self.real_config_guard {
            return Err(bad("测试后台没有指定 SCYLLAUI_CLI_HOME，不修改真实的 CLI 配置"));
        }
        let plans = self.plans_for(&tools, action);
        if hookcfg::fingerprint(&plans) != fingerprint {
            return Err(Fail("conflict", "配置文件在确认之后被改动，请重新生成改动".into()));
        }
        let backups = hookcfg::apply(&plans, &hookcfg::local_stamp()).map_err(|e| match e.starts_with("conflict:") {
            true => Fail("conflict", e),
            false => Fail("internal", e),
        })?;
        let verb = if action == HookAction::Uninstall { "撤销" } else { "修复" };
        for p in &plans {
            log!("{verb} Hook 配置：{}：{}", p.tool.as_str(), p.actions.join("；"));
        }
        self.hooks_changed(analyze_all(&self.cli_home));
        self.hook_watch.recheck();
        Ok(json!({
            "hooks": self.hooks_json(),
            "backups": backups.iter().map(|b| b.display().to_string()).collect::<Vec<_>>(),
        }))
    }

    /// 读到前台 CLI 的版本：记在终端上（推送），并写入该终端当前对话的 `cli_version`（只写第一次）。
    fn version_read(&mut self, id: &str, pid: i32, version: Option<String>) -> Result<(), Fail> {
        let Some(pane) = self.panes.get_mut(id) else { return Ok(()) };
        let Foreground::Agent { pid: fg_pid, tool, .. } = pane.foreground else { return Ok(()) };
        if fg_pid != pid {
            return Ok(());
        }
        pane.version = Some(PaneVersion { pid, tool, version });
        self.note_cli_version(id)?;
        self.push_terminal(id, None)
    }

    /// 终端当前对话与前台是同一款 CLI、版本已知时，写入对话的 `cli_version`。
    fn note_cli_version(&mut self, id: &str) -> Result<(), Fail> {
        let Some(pane) = self.panes.get(id) else { return Ok(()) };
        let (Some(cid), Some(v)) = (pane.conversation, pane.version.as_ref()) else { return Ok(()) };
        let Some(version) = v.version.clone() else { return Ok(()) };
        if pane.state.tool != Some(v.tool) {
            return Ok(());
        }
        if self.store.set_cli_version(cid, &version)? {
            self.push_conversation(cid)?;
        }
        Ok(())
    }

    // ---- 终端与对话 ----

    /// 登记（或重新确认）一个终端，按目录自动建项目。返回终端 ID。
    fn open_terminal(&mut self, t: &Value, now: i64) -> Result<String, Fail> {
        let (Some(id), Some(dir), Some(cwd)) = (
            t["terminal_id"].as_str().filter(|s| !s.is_empty()),
            t["project_dir"].as_str(),
            t["cwd"].as_str(),
        ) else {
            return Err(bad("终端需要 terminal_id、project_dir、cwd"));
        };
        if !dir.starts_with('/') {
            return Err(bad("project_dir 必须是绝对路径"));
        }
        let dir = match dir.trim_end_matches('/') {
            "" => "/",
            d => d,
        };
        let new = !self.panes.contains_key(id);
        // 新终端的启动目录正是某个存活终端此刻所在的目录（「新建」、Cmd+T 在当前终端所在目录）：沿用那个终端已判定的
        // 项目。这个目录来自 Shell 上报，可能是符号链接路径（`/var/…`），按字符串建项目会先多出一个同名项目组，等新终端
        // 的 Shell 报出真实路径才并回（2026-10-23）。核心线程不碰文件系统，所以沿用已判定的结果，而不在这里解析路径。
        let start = match cwd.trim_end_matches('/') {
            "" => "/",
            d => d,
        };
        let sibling = self
            .panes
            .iter()
            .find(|(_, p)| new && p.current_dir.as_deref() == Some(start))
            .and_then(|(t, _)| self.store.terminal(t).ok().flatten())
            .and_then(|t| t["project_id"].as_i64());
        let project_id = match sibling {
            Some(p) => p,
            None => {
                let (project_id, created) = self.store.ensure_project(dir, now)?;
                self.track_repo(project_id, dir, true);
                if created {
                    self.push_project(project_id)?;
                }
                project_id
            }
        };
        self.store
            .open_terminal(id, project_id, cwd, t["name"].as_str(), now)?;
        let pane = self
            .panes
            .entry(id.to_owned())
            .or_insert_with(|| Pane::new(PaneState::default(), None));
        // 保活终端：等它的第一个 attach 来起 Shell（M3）。
        if t["held"] == true {
            pane.held = true;
            if new && !self.host.has(id) {
                pane.pending_input = t["initial_input"].as_str().map(|s| s.as_bytes().to_vec());
            }
        }
        // 为恢复对话而开的新终端（docs/protocol.md 第 7 节「一键恢复」）；重复登记与 `sync` 不带。
        if let Some(cid) = t["resume_of"].as_i64().filter(|_| new) {
            self.bind_resume(id, cid)?;
        }
        self.push_terminal(id, None)?;
        Ok(id.to_owned())
    }

    /// 关闭终端并结束其上的对话。返回 false 表示终端不存在或已关闭。
    fn close_terminal(
        &mut self,
        id: &str,
        reason: &str,
        exit_code: Option<i64>,
        now: i64,
    ) -> Result<bool, Fail> {
        if !self.store.close_terminal(id, reason, exit_code, now)? {
            return Ok(false);
        }
        self.workspace.remove(id);
        // 冲突中另一终端仍在运行同一对话时改绑过去，不结束（docs/protocol.md 第 7 节「冲突」）。
        let mut moved = Vec::new();
        if let Some(p) = self.panes.get(id) {
            if let (Some(tool), Some(sid), Some(cid)) = (p.state.tool, p.state.session_id.clone(), p.conversation) {
                if let Some(other) = self.bound_runners(tool, &sid, Some(id)).first() {
                    if self.store.set_conversation_terminal(cid, other)? {
                        moved.push(cid);
                    }
                }
            }
        }
        let mut ended = self
            .store
            .end_conversations_on(id, "terminal_closed", now)?;
        ended.extend(moved);
        // 先清理再推送，推送里的 recent_total 才是清理后的值。
        self.prune(now)?;
        for cid in ended {
            self.push_conversation(cid)?;
        }
        self.push_terminal(id, None)?;
        self.panes.remove(id);
        self.wait_seq.remove(id);
        self.cpu_prev.remove(id);
        if self.focused.as_deref() == Some(id) {
            self.focused = None;
        }
        Ok(true)
    }

    /// 关闭全部非保活终端（App 断开时）：后台持有 PTY 的终端照常运行（M3）。
    fn close_all(&mut self, reason: &str) -> Result<(), Fail> {
        let now = now_ms();
        for id in self.store.alive_terminals()? {
            if !self.host.has(&id) {
                self.close_terminal(&id, reason, None, now)?;
            }
        }
        Ok(())
    }

    /// `routed` 为 true 表示守护进程托管会话的帧已由 RouteWorker 改写为实际显示它的终端。
    fn hook(&mut self, frame: &Value, recv_us: u64, routed: bool) {
        if !routed {
            if let Some(dir) = &self.capture {
                capture(dir, frame, recv_us);
            }
        }
        if frame["dropped"].as_bool().unwrap_or(false) {
            return;
        }
        if !routed {
            if let Some(tool) = frame["source"].as_str().and_then(Tool::parse) {
                if let Some(code) = adapter(tool).anomaly(&frame["payload"]) {
                    self.note_anomaly(tool, frame, &code);
                }
            }
        }
        let (Some(tool), Some(tid), Some(ev)) = (
            frame["source"].as_str().and_then(Tool::parse),
            frame["pane_id"].as_str(),
            frame["source"]
                .as_str()
                .and_then(Tool::parse)
                .and_then(|t| normalize(t, &frame["payload"])),
        ) else {
            return;
        };
        // CLI 守护进程托管的会话：SCYLLAUI_PANE_ID 是拉起守护进程那个终端的，不可信；交 RouteWorker 反查
        // 连着它的终端（docs/design.md 4.4 节）。候选为前台识别为同一款 CLI 的存活终端。
        // 帧里的 `hosted` 由 scyllaui 按适配器规则算出（Codex 看上级进程参数）；旧帧没有时按环境报告判断。
        let hosted = frame["hosted"].as_bool().unwrap_or_else(|| adapter(tool).is_hosted(&frame["env"]));
        if !routed && adapter(tool).hosted.is_some() && hosted {
            let job = adapter(tool)
                .hosted
                .as_ref()
                .and_then(|h| (h.job_of)(ev.session_id.as_deref()?));
            let Some(job) = job else {
                return log!("丢弃无会话 ID 的后台会话 Hook：event={}", ev.name);
            };
            let candidates = self
                .panes
                .iter()
                .filter_map(|(id, p)| match &p.foreground {
                    Foreground::Agent { pid, tool: t, .. } if *t == tool => Some((id.clone(), *pid)),
                    _ => None,
                })
                .collect();
            self.routes.request(RouteJob { frame: frame.clone(), recv_us, tool, job, candidates });
            return;
        }
        let trace = json!({
            "event": ev.name,
            "notification_type": ev.notification_type,
            "hook_t0_us": frame["hook_t0_us"],
            "daemon_recv_us": recv_us,
        });
        // 「需确认」给出的记录路径（Codex：子代理的审批指向子线程的记录）；子代理的会话 ID（Antigravity：它的等待只写在
        // 它自己的对话库里）。记下来供会话记录监听用。
        if ev.kind == Kind::PermissionWait && adapter(tool).record_from_approval {
            if let Some(path) = &ev.transcript_path {
                self.approval_records.insert(tid.to_owned(), (path.clone(), ev.subagent));
            }
        }
        if ev.subagent && adapter(tool).child_records {
            if let (Some(child), Some(p)) = (ev.agent_key.as_ref().filter(|c| valid_session_id(c)), self.panes.get(tid)) {
                let main = p.state.session_id.clone();
                let entry = self.child_records.entry(tid.to_owned()).or_default();
                if entry.0 != main {
                    *entry = (main, Vec::new());
                }
                if !entry.1.contains(child) {
                    entry.1.push(child.clone());
                    if entry.1.len() > CHILD_RECORDS_MAX {
                        entry.1.remove(0);
                    }
                }
            }
        }
        // 轮次结束以会话记录为准的工具（Kimi，docs/design.md 3.1 节）：记录的监听已打开时，Hook 报的一轮结束、出错、
        // 中断只记录、不改状态（它们分不出主代理与子代理），等记录里的。
        let mut ev = ev;
        // 提交不带轮次号的（Kimi）：以收到提交的时刻（毫秒）作这一轮的标识。会话记录监听按它重开，补读时只认这一刻之后
        // 写下的那次提问的结束（`WaitScan::replayed`）。
        if adapter(tool).turns_by_record && ev.kind == Kind::PromptSubmit && ev.prompt_id.is_none() {
            ev.prompt_id = Some((recv_us / 1000).to_string());
        }
        if adapter(tool).turns_by_record
            && self.watch_open.contains(tid)
            && matches!(ev.kind, Kind::TurnEnd | Kind::TurnFail | Kind::TurnInterrupt)
        {
            ev.kind = Kind::Other;
        }
        // 收到这个终端的事件：它在同步的事实依据（docs/design.md 4.7 节「会话是否真的在同步」）。先重算，这次事件的推送就带上。
        if let Some(p) = self.panes.get_mut(tid) {
            p.last_hook = Some((tool, now_ms()));
            if p.silent.is_some_and(|(t, ..)| t == tool) {
                log!("会话记录对账：{tid} 又收到 {} 的事件，不再记为没在同步", tool.as_str());
                p.silent = None;
            }
        }
        let sync_changed = self.refresh_sync(tid);
        self.apply_event(tool, tid, &ev, trace);
        if sync_changed {
            if let Err(Fail(_, e)) = self.push_terminal(tid, None) {
                log!("推送终端失败：{e}");
            }
        }
        // Hook 到达时读一次前台（原由 App 在收到带 trace 的推送时读，docs/design.md 4.2 节）。
        self.refresh_foreground(tid);
        // 「需确认」带着待批准的命令原文（Codex）：盯这个 AI 进程，后代里出现执行这条命令的进程即批准（approval.rs）。
        if let (Kind::PermissionWait, Some(command), Some(watch)) = (ev.kind, &ev.command, &self.approval_watch) {
            if let Some(p) = self.panes.get(tid).filter(|p| p.state.state == AgentState::Waiting) {
                if let Foreground::Agent { pid, tool: t, .. } = p.foreground {
                    if t == tool {
                        watch.start(tid, p.state.session_id.clone(), pid, command.clone());
                        self.approval_watching.insert(tid.to_owned());
                    }
                }
            }
        }
    }

    /// Hook 载荷异常（M4b 块 6，docs/design.md 10.6 节 6a）：写日志；记到发来它的终端并推送，App 提示「状态可能不准」。
    /// 守护进程托管的帧的 `SCYLLAUI_PANE_ID` 不可信，只写日志。
    fn note_anomaly(&mut self, tool: Tool, frame: &Value, code: &str) {
        let tid = frame["pane_id"].as_str().unwrap_or("-");
        log!("Hook 异常：tool={} terminal={tid} {code}", tool.as_str());
        let hosted = frame["hosted"].as_bool().unwrap_or_else(|| adapter(tool).is_hosted(&frame["env"]));
        if hosted && adapter(tool).hosted.is_some() {
            return;
        }
        let Some(pane) = self.panes.get_mut(tid) else {
            return;
        };
        let pid = match &pane.foreground {
            Foreground::Agent { pid, tool: t, .. } if *t == tool => Some(*pid),
            _ => None,
        };
        pane.anomaly = Some(PaneAnomaly { tool, pid, code: code.to_owned() });
        if let Err(Fail(_, e)) = self.push_terminal(tid, None) {
            log!("推送终端失败：{e}");
        }
    }

    /// 按状态机处理一个规范化后的事件：推进窗格状态、待查看、写库、读标题与推送（Hook 与 WaitWatch 共用）。
    fn apply_event(&mut self, tool: Tool, tid: &str, ev: &HookEvent, trace: Value) {
        let Some(pane) = self.panes.get_mut(tid) else {
            log!("丢弃未登记终端的 Hook：terminal={tid} event={}", ev.name);
            return;
        };
        let before = pane.state.state;
        let applied = pane.state.apply(tool, ev);
        if applied == Applied::Ignored {
            return;
        }
        // 被接受的子代理事件只有审批（4.3 节「子代理事件」例外）：记到窗格已绑的对话上，不按它的会话 ID 建对话或换绑，
        // 也不取它的标题、会话记录、目录与模型（OpenCode 子会话的会话 ID 与主会话不同）。
        let sub;
        let ev = if ev.subagent {
            sub = HookEvent { session_id: None, title: None, transcript_path: None, cwd: None, model: None, prompt: None, ..ev.clone() };
            &sub
        } else {
            ev
        };
        // 待查看：一轮结束时用户没在看这个终端；提交提示词即视为已看（docs/protocol.md 第 7 节）。
        let turn_done = matches!(ev.kind, Kind::TurnEnd | Kind::TurnFail | Kind::Idle)
            && matches!(before, AgentState::Running | AgentState::Waiting)
            && matches!(pane.state.state, AgentState::Ready | AgentState::Error);
        let unseen = if turn_done && self.focused.as_deref() != Some(tid) {
            true
        } else if ev.kind == Kind::PromptSubmit {
            false
        } else {
            pane.unseen
        };
        let unseen_changed = unseen != pane.unseen;
        pane.unseen = unseen;
        let bound = pane.conversation;
        let now = now_ms();
        // 冲突中另一终端仍在运行同一对话：SessionEnd 时对话改绑过去，不结束（第 7 节「冲突」）。
        let keep_on = match (ev.kind, ev.session_id.as_deref()) {
            (Kind::SessionEnd, Some(sid)) => self.bound_runners(tool, sid, Some(tid)).into_iter().next(),
            _ => None,
        };
        let recorded = match self.store.record_event(tid, tool.as_str(), ev, bound, keep_on.as_deref(), now) {
            Ok(r) => r,
            Err(e) => return log!("写入事件失败：{e}"),
        };
        if let Some(pane) = self.panes.get_mut(tid) {
            pane.conversation = recorded.conversation;
        }
        // 待确认的结束（缺陷 B）：SessionEnd 在这个终端上结束了对话（改绑到别的终端的不算）记下；这个终端随后又有了
        // 别的对话的动静，之前那条作废。
        match (ev.kind, recorded.conversation) {
            (Kind::SessionEnd, Some(cid)) if keep_on.is_none() => self.note_ended(tid, cid, Instant::now()),
            (_, Some(cid)) if self.workspace.ended(tid).is_some_and(|e| e != cid) => {
                if let Some(p) = self.panes.get_mut(tid) {
                    p.ended_until = None;
                }
                self.workspace.set_ended(tid, None);
            }
            _ => {}
        }
        if let Some(cid) = recorded.conversation {
            self.request_title(tool, cid, ev.kind);
            if let Some(model) = ev.model.clone().filter(|_| ev.kind != Kind::SessionEnd) {
                self.set_model(cid, model, "hook");
            }
        }
        // Agent 执行了工具或一轮结束：工作区可能有变化（M2 第 2 块，Git 改动数的触发）。
        if matches!(ev.kind, Kind::ToolActivity | Kind::TurnEnd | Kind::TurnFail | Kind::Idle) {
            self.nudge_git(tid);
        }
        if let Err(Fail(_, e)) = self.note_cli_version(tid) {
            log!("记录 CLI 版本失败：{e}");
        }
        self.last_trace = Some(trace.clone());
        let result = (|| -> Result<(), Fail> {
            // 先清理再推送，推送里的 recent_total 才是清理后的值。
            if ev.kind == Kind::SessionStart || ev.kind == Kind::SessionEnd {
                self.prune(now)?;
            }
            for cid in &recorded.changed {
                self.push_conversation(*cid)?;
            }
            if applied == Applied::Changed || recorded.conversation != bound || unseen_changed {
                self.push_terminal(tid, Some(trace))?;
            }
            self.events_since_prune += 1;
            if self.events_since_prune >= PRUNE_EVENTS_EVERY {
                self.events_since_prune = 0;
                self.store.prune_events()?;
            }
            Ok(())
        })();
        if let Err(Fail(_, e)) = result {
            log!("处理 Hook 后续步骤失败：{e}");
        }
    }

    /// 从会话记录认出的事（waitwatch.rs，M2 第 1 块）：仍是同一会话、且状态还对得上时，与收到 Hook 相同。
    /// 待审批时：中断即拒绝（`PermissionRejected`），批准回到运行中（`PermissionApproved`）；运行中（只对声明了
    /// `scan_while_running` 的工具）：中断（`SessionAborted`）、等待用户（`PermissionAsked`，Antigravity 等待审批不发 Hook）。
    /// 运行中或待审批时记录里一轮已正常结束（`ExecutionDone`，Antigravity：Stop Hook 先到时状态已不对，不会走到这里）。
    fn session_record(&mut self, tid: &str, session_id: Option<String>, kind: Kind, turn: Option<String>, agent: Option<String>) {
        let Some(pane) = self.panes.get(tid) else { return };
        if session_id.is_some() && pane.state.session_id != session_id {
            return;
        }
        // 带轮次号的：窗格已在另一轮时不理会（上一轮的结束记录晚到）。
        if let (Some(t), Some(current)) = (&turn, pane.state.turn()) {
            if t != current {
                return;
            }
        }
        let Some(tool) = pane.state.tool else { return };
        let while_running = adapter(tool).scan_while_running;
        let name = match (pane.state.state, kind) {
            // 子代理自己的记录里认出的（Antigravity 子代理的对话库）：等待用户、等待消失、这一次执行结束，按这个子代理的
            // 事件交给状态机（只有「需确认」与其后的动静改窗格状态，4.3 节「子代理事件」例外）。
            (_, Kind::PermissionWait) if agent.is_some() => "SubagentPermissionAsked",
            (_, _) if agent.is_some() => "SubagentRecord",
            (AgentState::Waiting, Kind::TurnInterrupt) => "PermissionRejected",
            (AgentState::Waiting, Kind::PermissionDone) => "PermissionApproved",
            (AgentState::Running, Kind::TurnInterrupt) if while_running => "SessionAborted",
            (AgentState::Running, Kind::PermissionWait) if while_running => "PermissionAsked",
            (AgentState::Running | AgentState::Waiting, Kind::TurnEnd) if while_running => "ExecutionDone",
            (AgentState::Running | AgentState::Waiting, Kind::TurnFail) if while_running => "ExecutionFailed",
            // 记录里新开了一轮而窗格已回到 Ready 或出错（Kimi：一轮刚结束时插话、排队消息另开一轮，不发提交 Hook）。
            (AgentState::Ready | AgentState::Error, Kind::PromptSubmit) if adapter(tool).turns_by_record => "ExecutionStarted",
            _ => return,
        };
        log!("会话记录：{name} terminal={tid} tool={}", tool.as_str());
        // 记录里新开的一轮同样以此刻作轮次标识（同提交 Hook，见 hook()）。
        let prompt_id = (kind == Kind::PromptSubmit).then(|| now_ms().to_string());
        let ev = HookEvent {
            kind,
            name: name.to_owned(),
            session_id,
            prompt_id,
            notification_type: None,
            title: None,
            transcript_path: None,
            cwd: None,
            model: None,
            prompt: None,
            turn_scoped: false,
            keeps_error: false,
            subagent: agent.is_some(),
            agent_key: agent.clone(),
            approval_id: None,
            command: None,
            background: false,
            forked: false,
            all_approvals: kind == Kind::PermissionDone && agent.is_none(),
        };
        let now = now_us();
        let trace = json!({"event": ev.name, "notification_type": null, "hook_t0_us": now, "daemon_recv_us": now});
        self.apply_event(tool, tid, &ev, trace);
    }

    /// 待审批的对账（M2 第 1 块，docs/protocol.md 第 7 节「通知」）：每条命令处理完后执行。新进入待审批的终端开始
    /// 一次等待，用户没在看它就发通知；等待结束或用户切到该终端时撤回通知。会话记录的监听另行对账（`refresh_watches`）。
    fn refresh_waits(&mut self) {
        self.refresh_watches();
        let waiting: Vec<(String, Option<Tool>, Option<String>, Option<String>)> = self
            .panes
            .iter()
            .filter(|(_, p)| p.state.state == AgentState::Waiting)
            .map(|(id, p)| (id.clone(), p.state.tool, p.state.session_id.clone(), p.state.transcript_path.clone()))
            .collect();
        let mut withdraw = Vec::new();
        let ended: Vec<String> =
            self.waits.keys().filter(|id| !waiting.iter().any(|w| &w.0 == *id)).cloned().collect();
        for id in ended {
            let w = self.waits.remove(&id).unwrap();
            withdraw.extend(w.notice);
        }
        let mut posts = Vec::new();
        for (id, tool, _, _) in waiting {
            if self.waits.contains_key(&id) {
                continue;
            }
            let seq = self.wait_seq.entry(id.clone()).or_insert(0);
            *seq += 1;
            let notice = (self.focused.as_deref() != Some(id.as_str())).then(|| format!("{id}:{seq}"));
            if let Some(n) = &notice {
                posts.push((id.clone(), tool, n.clone()));
            }
            self.waits.insert(id, Wait { notice });
        }
        if let Some(f) = self.focused.clone() {
            if let Some(n) = self.waits.get_mut(&f).and_then(|w| w.notice.take()) {
                withdraw.push(n);
            }
        }
        for (id, tool, nid) in posts {
            let mut msg = self.notice_json(&id, tool, &nid);
            msg["kind"] = json!("waiting");
            if let Some(mut trace) = self.last_trace.clone() {
                trace["daemon_send_us"] = json!(now_us());
                msg["trace"] = trace;
            }
            log!("通知：{nid}");
            self.push(msg);
        }
        if !withdraw.is_empty() {
            log!("撤回通知：{}", withdraw.join(" "));
            self.push(json!({"type": "notify.withdraw", "ids": withdraw}));
        }
    }

    /// 完成通知的对账（M4a，docs/protocol.md 第 7 节「完成通知」）：每条命令处理完后执行。终端的「待查看」由 false
    /// 变为 true 即发一条（`kind: done`，带此刻的状态），「待查看」清除、终端关闭或移除即撤回。是否显示由 App 决定。
    fn refresh_dones(&mut self) {
        let unseen: Vec<(String, Option<Tool>, AgentState)> = self
            .panes
            .iter()
            .filter(|(_, p)| p.unseen)
            .map(|(id, p)| (id.clone(), p.state.tool, p.state.state))
            .collect();
        let gone: Vec<String> = self.dones.keys().filter(|id| !unseen.iter().any(|u| &u.0 == *id)).cloned().collect();
        let withdraw: Vec<String> = gone.iter().filter_map(|id| self.dones.remove(id)).collect();
        for (id, tool, state) in unseen {
            if self.dones.contains_key(&id) {
                continue;
            }
            let seq = self.done_seq.entry(id.clone()).or_insert(0);
            *seq += 1;
            let nid = format!("{id}:done:{seq}");
            let mut msg = self.notice_json(&id, tool, &nid);
            msg["kind"] = json!("done");
            msg["state"] = json!(if state == AgentState::Error { "error" } else { "ready" });
            log!("完成通知：{nid}");
            self.push(msg);
            self.dones.insert(id, nid);
        }
        if !withdraw.is_empty() {
            log!("撤回完成通知：{}", withdraw.join(" "));
            self.push(json!({"type": "notify.withdraw", "ids": withdraw}));
        }
    }

    /// `notify.post` 的内容（docs/protocol.md 第 5 节）：项目名（没有命名时取目录名）与终端名或任务标题。
    fn notice_json(&self, id: &str, tool: Option<Tool>, nid: &str) -> Value {
        let term = self.store.terminal(id).ok().flatten();
        let project = term
            .as_ref()
            .and_then(|t| t["project_id"].as_i64())
            .and_then(|p| self.store.project(p).ok().flatten())
            .and_then(|p| {
                p["name"].as_str().map(str::to_owned).or_else(|| {
                    p["dir"].as_str().and_then(|d| Path::new(d).file_name()).map(|n| n.to_string_lossy().into_owned())
                })
            });
        let title = term.as_ref().and_then(|t| t["name"].as_str().map(str::to_owned)).or_else(|| {
            let cid = self.panes.get(id)?.conversation?;
            self.store.conversation(cid).ok().flatten()?["title"].as_str().map(str::to_owned)
        });
        json!({"type": "notify.post", "id": nid, "terminal_id": id, "tool": tool.map(Tool::as_str),
               "project": project, "title": title})
    }

    /// 反查不到连着它的终端时，对没有登记文件的工具（Codex）推断归属（docs/design.md 4.4 节）：
    /// 该对话已绑定的存活终端 → 前台为该工具且当前目录等于 Hook `cwd` 的唯一终端 → 多个时取用户正在看的那个。
    /// 都不满足返回 None（不归属，终端显示前台识别的「未知」）。
    fn infer_pane(&self, frame: &Value) -> Option<String> {
        let tool = frame["source"].as_str().and_then(Tool::parse)?;
        if !adapter(tool).hosted.as_ref().is_some_and(|h| h.infer) {
            return None;
        }
        let ev = normalize(tool, &frame["payload"])?;
        let session = ev.session_id.as_deref()?;
        if let Some((id, _)) = self.panes.iter().find(|(_, p)| {
            p.state.tool == Some(tool)
                && p.state.session_id.as_deref() == Some(session)
                && p.state.state != AgentState::Ended
        }) {
            return Some(id.clone());
        }
        let cwd = ev.cwd.as_deref().map(|c| c.trim_end_matches('/'))?;
        let candidates: Vec<&String> = self
            .panes
            .iter()
            .filter(|(id, p)| {
                matches!(&p.foreground, Foreground::Agent { tool: t, .. } if *t == tool)
                    && self.pane_dir(id, p).as_deref().map(|d| d.trim_end_matches('/')) == Some(cwd)
            })
            .map(|(id, _)| id)
            .collect();
        match candidates.as_slice() {
            [only] => Some((*only).clone()),
            many => self.focused.as_ref().filter(|f| many.contains(f)).cloned(),
        }
    }

    /// 终端此刻所在目录：Shell 上报的当前目录，未上报时为启动目录。
    fn pane_dir(&self, id: &str, pane: &Pane) -> Option<String> {
        pane.current_dir.clone().or_else(|| {
            self.store.terminal(id).ok().flatten().and_then(|t| t["cwd"].as_str().map(str::to_owned))
        })
    }

    /// 按事件请求读原生标题与最后一条 AI 回复（docs/design.md 第 5 节「任务标题」「实现（M1 第 6 块）」）：
    /// 提交提示词后延迟读标题（标题来自 Hook 字段的工具不读）；一轮结束（含中断）、会话结束时立即读并延迟再读一次。
    /// 中断也读（2026-10-31）：OpenCode 2 中断后不再补发空闲事件，不读的话最后一条回复与用量停在上一轮。
    fn request_title(&self, tool: Tool, cid: i64, kind: Kind) {
        let source = adapter(tool).title_source;
        let delays: &[Duration] = match kind {
            Kind::PromptSubmit if source == TitleSource::HookField => return,
            Kind::PromptSubmit => &[TITLE_DELAY],
            Kind::TurnEnd | Kind::TurnFail | Kind::TurnInterrupt | Kind::Idle | Kind::SessionEnd => &[Duration::ZERO, TITLE_DELAY],
            // Agent 一启动就读一次，另起命令查的额度（Antigravity、Kimi、Grok）不必等第一轮结束（2026-11-01）。
            Kind::SessionStart => &[Duration::ZERO],
            _ => return,
        };
        let conv = match self.store.conversation(cid) {
            Ok(Some(c)) => c,
            _ => return,
        };
        let Some(native_id) = conv["native_id"].as_str() else { return };
        for d in delays {
            let job = TitleJob {
                conversation_id: cid,
                tool,
                source,
                reply: adapter(tool).reply_source,
                model: adapter(tool).model_source,
                usage: adapter(tool).usage_source,
                effort: adapter(tool).effort_source,
                native_id: native_id.to_owned(),
                transcript_path: conv["transcript_path"].as_str().map(str::to_owned),
                model_hint: self.usage.get(&cid).and_then(|u| u.model.as_ref()).map(|m| m.0.clone()),
            };
            self.titles.request(job, *d);
        }
    }

    /// 会话记录监听的对账：靠推断算作在同步的会话盯它新提交的提示词（`submit_watch`）；适配器声明了 `waiting_scan` 的工具在待审批时监听，声明了 `scan_while_running` 的在运行中也
    /// 监听，声明了 `turns_by_record` 的在 Ready、出错时也监听；状态离开这些即停止。记录路径优先用适配器按会话 ID
    /// 推出的位置（在 `cli_home` 下），其次用 Hook 给出的。
    fn refresh_watches(&mut self) {
        use crate::waitwatch::{Source, Start};
        use scyllaui_proto::adapters::RecordLookup;
        let mut want: HashMap<String, (WatchKey, Start)> = HashMap::new();
        self.approval_records.retain(|id, _| self.panes.get(id).is_some_and(|p| p.state.state == AgentState::Waiting));
        self.child_records.retain(|id, (main, _)| {
            self.panes.get(id).is_some_and(|p| p.state.session_id == *main && !matches!(p.state.state, AgentState::Ended | AgentState::Unknown))
        });
        for (id, p) in &self.panes {
            if let Some((key, job)) = self.submit_watch(id, p) {
                want.insert(job.key.clone(), (key, job));
            }
            let Some(tool) = p.state.tool else { continue };
            let a = adapter(tool);
            let Some(scan) = a.waiting_scan else { continue };
            let waiting = p.state.state == AgentState::Waiting;
            // 子代理自己的记录（Antigravity 子代理的对话库）：主会话这一轮结束了子代理也可能还在跑、在等审批，所以只要
            // 会话还在就一直监听。
            if let (true, Some(rel), Some((_, children))) = (a.child_records, a.session_record, self.child_records.get(id)) {
                for child in children {
                    let key = WatchKey { session: p.state.session_id.clone(), turn: Some(child.clone()) };
                    let job = Start {
                        key: format!("{id}#child:{child}"),
                        terminal_id: id.clone(),
                        session_id: p.state.session_id.clone(),
                        source: Source::Path(Path::new(&self.cli_home).join(rel(child))),
                        scan,
                        turn: None,
                        waiting: false,
                        resync: false,
                        sub: true,
                        agent: Some(child.clone()),
                    };
                    want.insert(job.key.clone(), (key, job));
                }
            }
            // 轮次以记录为准的工具（Kimi）会话还在就一直看：一轮刚结束时 CLI 可能不发提交 Hook 就另开一轮。
            let alive = matches!(p.state.state, AgentState::Ready | AgentState::Running | AgentState::Error);
            let active = waiting || (p.state.state == AgentState::Running && a.scan_while_running) || (alive && a.turns_by_record);
            if !active {
                continue;
            }
            let sid = p.state.session_id.as_ref().filter(|s| valid_session_id(s));
            // 定位只在这里给出办法，查找与打开在监听线程里做（Kimi 要读索引文件）。
            let source = match (a.session_record, a.record_lookup, sid) {
                (Some(rel), _, Some(sid)) => Some(Source::Path(Path::new(&self.cli_home).join(rel(sid)))),
                (None, Some(RecordLookup::KimiIndex), Some(sid)) => {
                    Some(Source::KimiIndex { home: PathBuf::from(&self.cli_home), session: sid.clone() })
                }
                (None, Some(RecordLookup::GrokSessions), Some(sid)) => {
                    Some(Source::GrokSessions { home: PathBuf::from(&self.cli_home), session: sid.clone() })
                }
                _ => p.state.transcript_path.as_ref().map(|t| Source::Path(PathBuf::from(t))),
            };
            // 监听「需确认」那个事件给出的记录（Codex）：没有就不监听。
            let from_approval = self.approval_records.get(id).filter(|_| a.record_from_approval);
            let (source, sub) = match (a.record_from_approval, from_approval) {
                (true, Some((path, sub))) => (Some(Source::Path(PathBuf::from(path))), *sub),
                (true, None) => (None, false),
                (false, _) => (source, false),
            };
            // CLI 进程自己的状态文件（Claude）：待审批期间另开一个监听，按前台 AI 的进程号定位。
            if let (true, Some((dir, f)), Foreground::Agent { pid, tool: t, .. }) = (waiting, a.pid_status, &p.foreground) {
                if *t == tool {
                    let key = WatchKey { session: p.state.session_id.clone(), turn: Some(pid.to_string()) };
                    let job = Start {
                        key: format!("{id}#status"),
                        terminal_id: id.clone(),
                        session_id: p.state.session_id.clone(),
                        source: Source::Path(Path::new(&self.cli_home).join(dir).join(format!("{pid}.json"))),
                        scan: scyllaui_proto::adapters::waiting::Scan::Whole(f),
                        turn: None,
                        waiting,
                        resync: false,
                        sub: false,
                        agent: None,
                    };
                    want.insert(job.key.clone(), (key, job));
                }
            }
            let Some(source) = source else { continue };
            // 轮次结束以记录为准的工具按轮次重开监听：补读要按新的轮次核对。
            let turn = a.turns_by_record.then(|| p.state.turn().map(str::to_owned)).flatten();
            let key = WatchKey { session: p.state.session_id.clone(), turn: turn.clone() };
            let job = Start {
                key: id.clone(),
                terminal_id: id.clone(),
                session_id: p.state.session_id.clone(),
                source,
                scan,
                turn,
                waiting,
                resync: p.resync,
                sub,
                agent: None,
            };
            want.insert(id.clone(), (key, job));
        }
        // 按进程认批准的监听：离开待审批即停。
        let over: Vec<String> = self
            .approval_watching
            .iter()
            .filter(|id| self.panes.get(*id).is_none_or(|p| p.state.state != AgentState::Waiting))
            .cloned()
            .collect();
        for id in over {
            self.approval_watching.remove(&id);
            if let Some(w) = &self.approval_watch {
                w.stop(&id);
            }
        }
        let stale: Vec<String> = self.watching.keys().filter(|id| !want.contains_key(*id)).cloned().collect();
        for id in stale {
            self.wait_watch.stop(&id);
            self.watching.remove(&id);
            self.watch_open.remove(&id);
        }
        for (id, (key, job)) in want {
            if self.watching.get(&id) == Some(&key) {
                continue;
            }
            self.watching.insert(id.clone(), key);
            self.watch_open.remove(&id);
            if let Some(p) = self.panes.get_mut(&id) {
                p.resync = false;
            }
            log!("监听会话记录：{id}");
            self.wait_watch.start(job);
        }
    }

    fn terminal_json(&self, id: &str) -> Result<Option<Value>, Fail> {
        let Some(mut t) = self.store.terminal(id)? else {
            return Ok(None);
        };
        let pane = self.panes.get(id);
        t["agent"] = pane.map_or(Value::Null, |p| p.state.to_json());
        t["conversation_id"] = json!(pane.and_then(|p| p.conversation));
        t["foreground"] = pane.map_or(Value::Null, |p| {
            let mut fg = p.foreground.to_json();
            if let Foreground::Agent { pid, tool, .. } = p.foreground {
                // 版本只对读到它的那个进程有效（M1 第 7 块，docs/protocol.md 第 5 节）。
                let v = p.version.as_ref().filter(|v| v.pid == pid).and_then(|v| v.version.clone());
                fg["version_verified"] = json!(v.as_deref().map(|v| verified(tool, v)));
                fg["version"] = json!(v);
                // 「状态可能不准」（M4b 块 6）：只给记下它的那个进程。
                if let Some(a) = p.anomaly.as_ref().filter(|a| a.tool == tool && a.pid.is_none_or(|x| x == pid)) {
                    fg["anomaly"] = json!(a.code);
                }
                // 这个进程的同步情况，以及它启动时本项目的条目在不在（查得到才附；Codex 信任引导据此分路）。
                if let Some(s) = p.sync {
                    fg["sync"] = json!(s.as_str());
                }
                let at_start = procinfo::start_ms(pid).and_then(|s| self.hook_times.state_at(tool, s));
                if let Some(present) = at_start.and_then(hooktime::entries_present) {
                    fg["entries_at_start"] = json!(present);
                }
            }
            fg
        });
        t["unseen"] = json!(pane.is_some_and(|p| p.unseen));
        t["current_dir"] = json!(pane.and_then(|p| p.current_dir.clone()));
        t["conflict_with"] = json!(pane.map(|p| p.conflict.clone()).unwrap_or_default());
        t["held"] = json!(self.host.has(id) || pane.is_some_and(|p| p.held));
        t["notice"] = json!(pane.and_then(|p| p.notice));
        Ok(Some(t))
    }

    /// 用户正在看的终端（docs/protocol.md 第 4 节）：null 表示没在看；指向带「待查看」的终端时清除并推送。
    fn terminal_focus(&mut self, msg: &Value) -> Result<Value, Fail> {
        let id = match &msg["terminal_id"] {
            Value::Null => {
                self.focused = None;
                return Ok(Value::Null);
            }
            v => v.as_str().ok_or_else(|| bad("terminal_id 须为字符串或 null"))?,
        };
        let Some(pane) = self.panes.get_mut(id) else {
            return Err(Fail("not_found", format!("终端 {id} 不存在或已关闭")));
        };
        self.focused = Some(id.to_owned());
        self.last_focus = Some(id.to_owned());
        if pane.unseen {
            pane.unseen = false;
            self.push_terminal(id, None)?;
        }
        // 切到保活终端时读一次前台（App 原来在切换时读，docs/design.md 4.2 节）。
        self.refresh_foreground(id);
        Ok(Value::Null)
    }

    /// 终端的当前目录（docs/protocol.md 第 4 节）：按 Git 工作区根（不在仓库里则为目录本身）改归项目，有变化即推送。
    fn terminal_cwd(&mut self, msg: &Value) -> Result<Value, Fail> {
        let (Some(id), Some(cwd)) = (msg["terminal_id"].as_str(), msg["cwd"].as_str()) else {
            return Err(bad("需要 terminal_id 与 cwd"));
        };
        if !cwd.starts_with('/') {
            return Err(bad("cwd 必须是绝对路径"));
        }
        if !self.panes.contains_key(id) {
            return Err(Fail("not_found", format!("终端 {id} 不存在或已关闭")));
        }
        // 保活终端的当前目录取后台 vt 收到的 OSC 7（M3）。
        if self.host.has(id) {
            return Ok(Value::Null);
        }
        self.set_current_dir(id, cwd)
    }

    fn set_current_dir(&mut self, id: &str, cwd: &str) -> Result<Value, Fail> {
        let cwd = match cwd.trim_end_matches('/') {
            "" => "/",
            d => d,
        };
        let Some(pane) = self.panes.get_mut(id) else {
            return Err(Fail("not_found", format!("终端 {id} 不存在或已关闭")));
        };
        if pane.current_dir.as_deref() == Some(cwd) {
            return Ok(Value::Null);
        }
        // 「目录不存在」说的是启动时退回了主目录：Shell 之后又去了别的目录，这条提示就过时了（2026-11-14）。
        // 第一次上报是退回后的主目录本身，不算。
        if pane.current_dir.is_some() && pane.notice == Some("cwd_missing") {
            pane.notice = None;
        }
        pane.current_dir = Some(cwd.to_owned());
        self.workspace.set(id, cwd);
        self.push_terminal(id, None)?;
        // 所在仓库由 GitWorker 判定，结果到达后再改归项目（cwd_resolved）。
        self.git.send(Job::Resolve { terminal_id: id.to_owned(), cwd: cwd.to_owned(), at: now_ms() });
        Ok(Value::Null)
    }

    /// GitWorker 判定了终端当前目录所在的项目目录：终端改归该项目。期间终端又换了目录则作废。
    /// 判定前已在这个终端里开始的对话（如 `cd x && claude` 时授权弹窗未处理）一并改归。
    fn cwd_resolved(&mut self, id: &str, cwd: &str, root: &str, at: i64) -> Result<(), Fail> {
        let Some(pane) = self.panes.get(id) else {
            return Ok(());
        };
        if pane.current_dir.as_deref() != Some(cwd) {
            return Ok(());
        }
        let conversation = pane.conversation;
        let (project_id, created) = self.store.ensure_project(root, now_ms())?;
        self.track_repo(project_id, root, true);
        if created {
            self.push_project(project_id)?;
        }
        if self.store.set_terminal_project(id, project_id)? {
            self.push_terminal(id, None)?;
            if let Some(c) = conversation {
                if self.store.set_conversation_project(c, project_id, at)? {
                    self.push_conversation(c)?;
                }
            }
        }
        Ok(())
    }

    // ---- 进程层 ----

    /// App 上报前台进程组（docs/design.md 4.2 节的读取时机由 App 决定），识别后推送变化；
    /// 识别为 Agent 时监听其退出。
    fn terminal_foreground(&mut self, msg: &Value) -> Result<Value, Fail> {
        let id = msg["terminal_id"]
            .as_str()
            .ok_or_else(|| bad("缺少 terminal_id"))?;
        // 保活终端的前台由后台对主端读取（M3），App 读到的是 attach。
        if self.host.has(id) {
            return match self.panes.contains_key(id) {
                true => Ok(Value::Null),
                false => Err(Fail("not_found", format!("终端 {id} 不存在或已关闭"))),
            };
        }
        self.apply_foreground(id, msg["pgid"].as_i64().unwrap_or(0) as i32)
    }

    /// 识别前台进程组并推送变化；识别为 Agent 时监听其退出（App 上报与后台自读共用）。
    fn apply_foreground(&mut self, id: &str, pgid: i32) -> Result<Value, Fail> {
        let (fg, version_src) = procinfo::classify_full(pgid);
        if let Foreground::Agent { tool, .. } = fg {
            // 之前没装的 CLI 第一次被识别：配置目录可能刚建好，立即重查 Hook 配置。
            if self.hooks.iter().any(|s| s.tool == tool && s.state == HookState::Absent) {
                self.hook_watch.recheck();
            }
        }
        let Some(pane) = self.panes.get_mut(id) else {
            return Err(Fail("not_found", format!("终端 {id} 不存在或已关闭")));
        };
        if let (Foreground::Agent { pid, .. }, Some((source, exe))) = (&fg, version_src) {
            if pane.version.as_ref().map(|v| v.pid) != Some(*pid) {
                pane.version = None;
                self.versions.request(VersionJob {
                    terminal_id: id.to_owned(),
                    pid: *pid,
                    start_ms: procinfo::start_ms(*pid),
                    source,
                    exe,
                });
            }
        }
        let Some(pane) = self.panes.get_mut(id) else {
            return Err(Fail("not_found", format!("终端 {id} 不存在或已关闭")));
        };
        if let Foreground::Agent { pid, tool, .. } = fg {
            if pane.watched.map(|(p, _)| p) != Some(pid) {
                pane.watched = Some((pid, tool));
                pane.exit_deadline = None;
                if !self.exits.watch(pid) {
                    // 识别与注册之间进程已退出：按退出处理。
                    self.proc_exit(pid, Instant::now());
                    return Ok(Value::Null);
                }
            }
        }
        if !self.panes.contains_key(id) {
            return Ok(Value::Null);
        }
        // 重建时说明没能接回的原因（M4b 块 5 P2，docs/design.md 10.6 节）：记下前台在跑的 AI，回到 Shell 或普通命令时清除；
        // 未知（AI 刚退出、等 App 重新上报）时不动。
        match &fg {
            Foreground::Agent { tool, .. } => self.workspace.set_agent(id, Some(tool.as_str())),
            Foreground::Shell { .. } | Foreground::Command { .. } => self.workspace.set_agent(id, None),
            Foreground::Unknown => {}
        }
        let Some(pane) = self.panes.get_mut(id) else {
            return Ok(Value::Null);
        };
        if let Some(tty) = procinfo::tty_of(pgid) {
            pane.tty = Some(tty);
        }
        // 「状态可能不准」只对记下它的那个 AI 进程有效（M4b 块 6）：换了进程或回到 Shell 时清除，记下时还没识别出进程的补上进程号。
        match &fg {
            Foreground::Agent { pid, tool, .. } => {
                if pane.anomaly.as_ref().is_some_and(|a| a.tool != *tool || a.pid.is_some_and(|p| p != *pid)) {
                    pane.anomaly = None;
                } else if let Some(a) = &mut pane.anomaly {
                    a.pid = Some(*pid);
                }
            }
            Foreground::Shell { .. } | Foreground::Command { .. } => pane.anomaly = None,
            Foreground::Unknown => {}
        }
        let fg_changed = pane.foreground != fg;
        if fg_changed {
            pane.foreground = fg;
        }
        // 前台换了进程：同步情况跟着重算，与前台变化一起推送。
        let sync_changed = self.refresh_sync(id);
        if fg_changed || sync_changed {
            self.push_terminal(id, None)?;
        }
        // Shell 命令结束时 App 也会上报前台（M2 第 2 块，Git 改动数的触发）。
        self.nudge_git(id);
        Ok(Value::Null)
    }

    /// 保活终端的 Shell（login）已退出并被回收（M3）：记退出码、关闭终端，再让 attach 收到 EXIT。
    /// 终端已被关闭（`terminal.close`）时什么也不做。
    fn shell_exited(&mut self, pid: i32, code: Option<i32>) {
        let Some(id) = self.host.find_by_pid(pid) else {
            return;
        };
        log!("Shell 退出：terminal={id} pid={pid} exit={code:?}");
        self.shell_gone(&id, code);
        self.host.end(&id, code);
    }

    /// Shell 没了之后终端的去向。用户自己 `exit` 时 login 不传退出码，总是 0；被信号结束为 128 + 信号。
    /// App 不在时被信号结束（真实关机、注销时系统几乎同时结束各 Shell 与后台，谁先谁后不定）：不当作用户关闭，
    /// 终端留在库里、目录与待确认的结束留在 workspace，只丢掉内存里的窗格；后台随后也没了，下次按「随后台丢失」
    /// 恢复，后台还在，App 下次同步时按「保活终端没有 PTY」恢复（docs/design.md「重启恢复」）。其余照常关闭。
    fn shell_gone(&mut self, id: &str, code: Option<i32>) {
        if self.app.is_none() && code.is_some_and(|c| c > 128) {
            log!("终端 {id} 的 Shell 在 App 不在时被信号结束，留待恢复");
            self.panes.remove(id);
            self.wait_seq.remove(id);
            self.cpu_prev.remove(id);
            return;
        }
        if let Err(Fail(_, e)) = self.close_terminal(id, "exit", code.map(i64::from), now_ms()) {
            log!("关闭终端失败：{e}");
        }
    }

    /// 被监听的 Agent 进程退出：前台改为未知（等 App 重新上报），开始等 SessionEnd 的宽限。
    fn proc_exit(&mut self, pid: i32, now: Instant) {
        let Some((id, pane)) = self
            .panes
            .iter_mut()
            .find(|(_, p)| p.watched.is_some_and(|(w, _)| w == pid))
        else {
            return;
        };
        let (_, tool) = pane.watched.take().unwrap();
        pane.exit_deadline = Some((now + EXIT_GRACE, tool));
        pane.foreground = Foreground::Unknown;
        let id = id.clone();
        log!(
            "Agent 进程退出：terminal={id} pid={pid} tool={}",
            tool.as_str()
        );
        if let Err(Fail(_, e)) = self.push_terminal(&id, None) {
            log!("推送终端失败：{e}");
        }
    }

    /// 宽限期满仍未收到 SessionEnd：窗格改为已结束，对话以 `process_exit` 结束。
    /// 同一终端里已换成另一款 CLI 的会话不受影响。
    fn expire_exits(&mut self, now: Instant) {
        let due: Vec<(String, Tool)> = self
            .panes
            .iter_mut()
            .filter_map(|(id, p)| match p.exit_deadline {
                Some((t, tool)) if t <= now => {
                    p.exit_deadline = None;
                    Some((id.clone(), tool))
                }
                _ => None,
            })
            .collect();
        for (id, tool) in &due {
            let Some(pane) = self.panes.get_mut(id) else {
                continue;
            };
            if pane.state.tool != Some(*tool) || pane.state.state == AgentState::Ended {
                continue;
            }
            pane.state.state = AgentState::Ended;
            let conversation = pane.conversation;
            let session = pane.state.session_id.clone();
            let result = (|| -> Result<(), Fail> {
                if let Some(cid) = conversation {
                    // 冲突中另一终端仍在运行同一对话：改绑过去，不结束（第 7 节「冲突」）。
                    let other = session.and_then(|s| self.bound_runners(*tool, &s, Some(id)).into_iter().next());
                    let changed = match other {
                        Some(o) => self.store.set_conversation_terminal(cid, &o)?,
                        None => {
                            let ended = self.store.end_conversation(cid, "process_exit", now_ms())?;
                            self.note_ended(id, cid, now);
                            ended
                        }
                    };
                    if changed {
                        self.push_conversation(cid)?;
                    }
                }
                self.push_terminal(id, None)
            })();
            if let Err(Fail(_, e)) = result {
                log!("按进程退出结束对话失败：{e}");
            }
        }
        if !due.is_empty() {
            self.refresh_conflicts();
            self.refresh_waits();
            self.refresh_dones();
        }
    }

    /// 终端还开着时对话结束了（进程退出、SessionEnd）：记为待确认的结束，后台活过 `ENDED_CONFIRM` 再清掉。
    fn note_ended(&mut self, id: &str, cid: i64, now: Instant) {
        if let Some(p) = self.panes.get_mut(id) {
            p.ended_until = Some(now + ENDED_CONFIRM);
            self.workspace.set_ended(id, Some(cid));
        }
    }

    /// 待确认的结束到期：后台还活着，这次结束是正常的，不再作为重启后接回的依据。
    fn expire_ended(&mut self, now: Instant) {
        let due: Vec<String> = self
            .panes
            .iter_mut()
            .filter_map(|(id, p)| match p.ended_until {
                Some(t) if t <= now => {
                    p.ended_until = None;
                    Some(id.clone())
                }
                _ => None,
            })
            .collect();
        for id in due {
            self.workspace.set_ended(&id, None);
        }
    }

    fn push_terminal(&mut self, id: &str, trace: Option<Value>) -> Result<(), Fail> {
        let Some(t) = self.terminal_json(id)? else {
            return Ok(());
        };
        let mut msg = json!({"type": "terminal.upsert", "terminal": t});
        if let Some(mut trace) = trace {
            trace["daemon_send_us"] = json!(now_us());
            msg["trace"] = trace;
        }
        self.push(msg);
        Ok(())
    }

    fn push_project(&mut self, id: i64) -> Result<(), Fail> {
        if let Some(p) = self.store.project(id)? {
            let p = self.with_branch(p);
            self.push(json!({"type": "project.upsert", "project": p}));
        }
        Ok(())
    }

    // ---- Git 分支 ----

    fn with_branch(&self, mut p: Value) -> Value {
        let branch = p["project_id"]
            .as_i64()
            .and_then(|id| self.repos.get(&id))
            .and_then(|r| r.branch.clone());
        p["branch"] = json!(branch);
        // 只有标题栏显示的项目带改动数（M2 第 2 块）；算出之前没有该字段。
        if let Some((pid, status)) = &self.git_status {
            if p["project_id"].as_i64() == Some(*pid) {
                p["git"] = status.as_ref().map_or(Value::Null, GitStatus::to_json);
            }
        }
        p
    }

    /// 首次见到项目时交 GitWorker 找仓库、读分支；`watch` 为 true 时同时监听其 git 目录（之前只读过的项目补上监听）。
    fn track_repo(&mut self, id: i64, dir: &str, watch: bool) {
        match self.repos.get_mut(&id) {
            None => {
                self.repos.insert(id, Repo { git_dir: None, branch: None, watched: watch });
            }
            Some(r) if watch && !r.watched => r.watched = true,
            Some(_) => return,
        }
        self.git.send(Job::Track { project_id: id, dir: dir.to_owned(), watch });
    }

    /// GitWorker 读到项目的仓库与分支：记下监听描述符，分支有变化时推送项目。
    fn repo_found(&mut self, id: i64, git_dir: Option<PathBuf>, branch: Option<String>, fd: Option<RawFd>) {
        if let (Some(fd), Some(g)) = (fd, &git_dir) {
            self.git_fds.insert(fd, g.clone());
        }
        let Some(r) = self.repos.get_mut(&id) else {
            return;
        };
        r.git_dir = git_dir;
        if r.branch == branch {
            return;
        }
        r.branch = branch;
        if let Err(Fail(_, e)) = self.push_project(id) {
            log!("推送项目失败：{e}");
        }
    }

    /// git 目录有写入：交 GitWorker 重读分支。
    fn git_changed(&mut self, fd: RawFd) {
        if let Some(g) = self.git_fds.get(&fd).cloned() {
            // 标题栏显示的项目在这个仓库里：顺带重算改动数（M2 第 2 块）。
            if let Some(pid) = self.watch_project {
                if self.repos.get(&pid).and_then(|r| r.git_dir.as_ref()) == Some(&g) {
                    self.request_git_status(pid);
                }
            }
            self.git.send(Job::Reread { git_dir: g });
        }
    }

    // ---- 标题栏状态行（M2 第 2 块，docs/protocol.md 第 4、7 节） ----

    /// 标题栏此刻显示的项目：只为它算改动数；null 表示不显示。换了项目即丢掉旧结果。
    fn status_watch(&mut self, msg: &Value) -> Result<Value, Fail> {
        let pid = match &msg["project_id"] {
            Value::Null => None,
            v => Some(v.as_i64().ok_or_else(|| bad("project_id 须为整数或 null"))?),
        };
        if let Some(pid) = pid {
            if self.store.project(pid)?.is_none() {
                return Err(Fail("not_found", format!("项目 {pid} 不存在")));
            }
        }
        if self.watch_project != pid {
            self.watch_project = pid;
            if self.git_status.as_ref().is_some_and(|(p, _)| Some(*p) != pid) {
                self.git_status = None;
            }
        }
        if let Some(pid) = pid {
            self.request_git_status(pid);
        }
        Ok(Value::Null)
    }

    fn request_git_status(&self, pid: i64) {
        if let Ok(Some(p)) = self.store.project(pid) {
            if let Some(dir) = p["dir"].as_str() {
                self.git.send(Job::Status { project_id: pid, dir: dir.to_owned() });
            }
        }
    }

    /// 终端所在项目正显示在标题栏时，请求重算改动数（GitWorker 防抖）。
    fn nudge_git(&self, tid: &str) {
        let Some(pid) = self.watch_project else { return };
        let in_project = self.store.terminal(tid).ok().flatten().and_then(|t| t["project_id"].as_i64()) == Some(pid);
        if in_project {
            self.request_git_status(pid);
        }
    }

    fn git_status_read(&mut self, pid: i64, status: Option<GitStatus>) {
        if self.watch_project != Some(pid) {
            return;
        }
        if self.git_status.as_ref().is_some_and(|(p, s)| *p == pid && *s == status) {
            return;
        }
        self.git_status = Some((pid, status));
        if let Err(Fail(_, e)) = self.push_project(pid) {
            log!("推送项目失败：{e}");
        }
    }

    /// 终端进程树的 CPU 与内存（docs/protocol.md 第 7 节「进程树统计」）。
    fn proc_stats(&mut self, msg: &Value) -> Result<Value, Fail> {
        let id = msg["terminal_id"].as_str().ok_or_else(|| bad("缺少 terminal_id"))?;
        let Some(pane) = self.panes.get(id) else {
            return Err(Fail("not_found", format!("终端 {id} 不存在或已关闭")));
        };
        let Some(tty) = pane.tty else {
            return Ok(json!({"cpu_percent": null, "mem_bytes": null, "processes": 0}));
        };
        let start = Instant::now();
        let cur = procinfo::tree_sample(tty);
        let took = start.elapsed();
        if took > Duration::from_millis(20) {
            log!("进程树采样较慢：{} ms（{} 个进程）", took.as_millis(), cur.cpu_ns.len());
        }
        let cpu = self.cpu_prev.get(id).map(|(at, prev)| procinfo::cpu_percent(prev, &cur, start.duration_since(*at)));
        let reply = json!({
            "cpu_percent": cpu.map(|c| (c * 10.0).round() / 10.0),
            "mem_bytes": (!cur.cpu_ns.is_empty()).then_some(cur.mem_bytes),
            "processes": cur.cpu_ns.len(),
        });
        self.cpu_prev.insert(id.to_owned(), (start, cur));
        Ok(reply)
    }

    fn conversation_active(&self, cid: i64) -> bool {
        self.store.conversation(cid).ok().flatten().is_some_and(|c| c["ended_at"].is_null())
    }

    /// 记下对话的当前模型，有变化即推送 `usage.upsert`。
    fn set_model(&mut self, cid: i64, model: String, source: &'static str) {
        let u = self.usage.entry(cid).or_default();
        if u.model.as_ref().is_some_and(|(m, _)| *m == model) {
            return;
        }
        u.model = Some((model, source));
        self.push_usage(cid);
    }

    /// 记下对话的推理强度（维护期 0.3.5），有变化即推送 `usage.upsert`。
    fn set_effort(&mut self, cid: i64, effort: String) {
        let u = self.usage.entry(cid).or_default();
        if u.effort.as_ref() == Some(&effort) {
            return;
        }
        u.effort = Some(effort);
        self.push_usage(cid);
    }

    /// 记下对话的会话累计 token（M2 第 3 块），有变化即推送 `usage.upsert`。
    fn set_tokens(&mut self, cid: i64, tokens: Tokens) {
        let u = self.usage.entry(cid).or_default();
        if u.tokens == Some(tokens) {
            return;
        }
        u.tokens = Some(tokens);
        self.push_usage(cid);
    }

    /// 记下对话是否按量计费（2026-11-01），有变化即推送 `usage.upsert`。
    fn set_billing(&mut self, cid: i64, api: bool) {
        let u = self.usage.entry(cid).or_default();
        if u.api == api {
            return;
        }
        u.api = api;
        self.push_usage(cid);
    }

    fn push_usage(&mut self, cid: i64) {
        let u = usage_json(&self.usage[&cid]);
        self.push(json!({"type": "usage.upsert", "conversation_id": cid, "usage": u}));
    }

    /// 记下某款 CLI 账号当前全部窗口的额度（2026-11-01 整份替换）：逐个窗口按 `set_quota` 记；快照里没有、又比它旧的窗口
    /// 删掉并推送 `quota.remove`（账号没有了这个窗口，如换了套餐）。空快照不算数。读到额度即清掉查不到的原因。
    fn set_quotas(&mut self, tool: Tool, quotas: Vec<Quota>, source: &'static str) {
        let Some(as_of) = quotas.iter().map(|q| q.as_of).max() else { return };
        self.set_quota_note(tool, None);
        for q in &quotas {
            self.set_quota(tool, *q, source);
        }
        let gone: Vec<Window> = self
            .quotas
            .iter()
            .filter(|((t, w), (old, _))| *t == tool.as_str() && old.as_of < as_of && !quotas.iter().any(|q| q.window == *w))
            .map(|((_, w), _)| *w)
            .collect();
        for w in gone {
            self.quotas.remove(&(tool.as_str(), w));
            self.push(json!({"type": "quota.remove", "tool": tool.as_str(), "window": w.as_str()}));
        }
    }

    /// Claude statusline 帧里的额度（2026-11-05 查明）：是发帧那个会话最后一次 API 返回时的值，没有时间戳；窗口过了重置时间
    /// Claude 会从该会话的数据里删掉它并重跑 statusline，每个窗口也可能单独缺（官方文档「Fields that may be absent」）。所以
    /// 不按整份快照处理：帧里缺的窗口不删；同一窗口只收更新的——重置时间更晚（新窗口），或重置时间相同、已用不低于已知值。
    /// 否则闲置会话的旧帧会删掉或压低活跃会话刚报的额度（0.4.26 验收：重启 App 前后 5h 消失）。
    fn merge_statusline_quotas(&mut self, quotas: Vec<Quota>) {
        if quotas.is_empty() {
            return;
        }
        self.set_quota_note(Tool::Claude, None);
        for q in quotas {
            let stale = self.quotas.get(&(Tool::Claude.as_str(), q.window)).is_some_and(|(old, _)| {
                q.resets_at < old.resets_at || (q.resets_at == old.resets_at && q.used_percent < old.used_percent)
            });
            if !stale {
                self.set_quota(Tool::Claude, q, "statusline");
            }
        }
    }

    /// 记下某款查不到额度的原因代码（None 为清掉），有变化即推送 `quota.note`。
    fn set_quota_note(&mut self, tool: Tool, note: Option<&'static str>) {
        if self.quota_notes.get(tool.as_str()).copied() == note {
            return;
        }
        match note {
            Some(n) => self.quota_notes.insert(tool.as_str(), n),
            None => self.quota_notes.remove(tool.as_str()),
        };
        self.push(json!({"type": "quota.note", "tool": tool.as_str(), "note": note}));
    }

    /// 记下某款 CLI 账号一个窗口的额度（M2 第 3 块），有变化即推送 `quota.upsert`。比已知的旧（别的会话较早的记录）不要；
    /// 百分比、重置时间与有无限制都没变时，「截至」时间满 1 分钟才推送（statusline 在回复过程中频繁调用）。
    fn set_quota(&mut self, tool: Tool, q: Quota, source: &'static str) {
        let key = (tool.as_str(), q.window);
        if let Some((old, _)) = self.quotas.get(&key) {
            let same = old.used_percent == q.used_percent && old.resets_at == q.resets_at && old.unlimited == q.unlimited;
            if q.as_of <= old.as_of || (same && q.as_of - old.as_of < 60_000) {
                return;
            }
        }
        self.quotas.insert(key, (q, source));
        let quota = quota_json(&self.quotas[&key]);
        self.push(json!({"type": "quota.upsert", "tool": tool.as_str(), "quota": quota}));
    }

    /// Claude statusline 桥送来的一帧（M2 第 3 块）：周额度按账号记；`model.id` 记到正在运行该会话的对话上。
    fn statusline(&mut self, frame: &Value) {
        if frame["source"] != "claude" {
            return;
        }
        let p = &frame["payload"];
        self.merge_statusline_quotas(claude_statusline_quotas(p, now_ms()));
        let (Some(sid), Some(model)) = (p["session_id"].as_str(), p["model"]["id"].as_str().filter(|m| !m.is_empty())) else {
            return;
        };
        let cid = self.panes.values().find_map(|pane| {
            (pane.state.tool == Some(Tool::Claude) && pane.state.session_id.as_deref() == Some(sid)).then_some(pane.conversation)?
        });
        if let Some(cid) = cid.filter(|c| self.conversation_active(*c)) {
            self.set_model(cid, model.to_owned(), "statusline");
        }
    }

    /// 重读到的分支：该仓库下各项目分支有变化时推送。
    fn branch_read(&mut self, g: &Path, branch: Option<String>) {
        let changed: Vec<i64> = self
            .repos
            .iter_mut()
            .filter(|(_, r)| r.git_dir.as_deref() == Some(g) && r.branch != branch)
            .map(|(id, r)| {
                r.branch = branch.clone();
                *id
            })
            .collect();
        for id in changed {
            if let Err(Fail(_, e)) = self.push_project(id) {
                log!("推送项目失败：{e}");
            }
        }
    }

    // ---- 一键恢复与冲突（docs/protocol.md 第 7 节） ----

    /// 一键恢复：正在运行 → `switch`；会话记录已不存在 → `gone`；否则 → `open`（命令与目录）。
    /// 带 `type_into`（后台重启后重起了 Shell 的终端，`terminal.respawned`）时不交给 App 代输，由后台把
    /// `cd <目录> && <命令>` 与回车原样写进那个终端的 PTY → `typed`。App 用 Ghostty 的输入文字接口（`ghostty_surface_text`）
    /// 代输时按粘贴处理：Shell 已画出提示符、开了括号粘贴（zsh 默认开）时，整段连同回车被当作粘贴内容放在输入行上不执行
    /// （2026-11-13 长跑缺陷 C，`kill -9` 后台后约一半终端的恢复命令停在输入行）。
    fn conversation_resume(&self, msg: &Value) -> Result<Value, Fail> {
        let cid = msg["conversation_id"].as_i64().ok_or_else(|| bad("缺少 conversation_id"))?;
        let into = msg["type_into"].as_str();
        if let Some(t) = into {
            match self.panes.get(t).map(|p| &p.foreground) {
                None => return Err(Fail("not_found", format!("终端 {t} 不存在"))),
                Some(Foreground::Agent { .. } | Foreground::Command { .. }) => {
                    return Err(Fail("busy", format!("终端 {t} 的前台不是 Shell，不代输")));
                }
                Some(_) => {}
            }
        }
        let c = self
            .store
            .conversation(cid)?
            .ok_or_else(|| Fail("not_found", format!("对话 {cid} 不存在")))?;
        let tool = c["tool"].as_str().and_then(Tool::parse).ok_or_else(|| bad("对话的工具未知"))?;
        let native = c["native_id"].as_str().ok_or_else(|| bad("对话没有会话 ID"))?;
        let mut runners = self.bound_runners(tool, native, None);
        if runners.is_empty() {
            runners = self.attached_runners(tool, native);
        }
        if !runners.is_empty() {
            let t = c["terminal_id"]
                .as_str()
                .filter(|t| runners.iter().any(|r| r == t))
                .map_or_else(|| runners[0].clone(), str::to_owned);
            log!("恢复：对话 {cid} 已在终端 {t} 运行，切过去");
            return Ok(json!({"action": "switch", "terminal_id": t}));
        }
        // 会话记录：Hook 给出的路径；没有时用适配器按会话 ID 推出的位置（Copilot、Antigravity，在 `cli_home` 下）。推出的
        // 位置只在存放各会话记录的目录确实存在时才作数（CLI 改了数据目录时说不准，宁可照常恢复，不误报「已不存在」）。
        let record = c["transcript_path"].as_str().map(str::to_owned).or_else(|| {
            let rel = adapter(tool).session_record?;
            let rel = valid_session_id(native).then(|| rel(native))?;
            let base = Path::new(&self.cli_home).join(&rel[..rel.find(native)?]);
            (self.probe.exists)(base.to_str()?).then(|| Path::new(&self.cli_home).join(&rel).to_string_lossy().into_owned())
        });
        if let Some(path) = record.filter(|p| !(self.probe.exists)(p)) {
            log!("恢复：对话 {cid} 的会话记录已不存在：{path}");
            return Ok(json!({"action": "gone", "reason": "transcript_missing", "path": path, "conversation": c}));
        }
        let command = resume_command(tool, native, &self.probe).ok_or_else(|| bad("会话 ID 含不允许的字符"))?;
        let dir = self.conversation_dir(&c)?;
        if let Some(t) = into {
            let line = format!("cd {} && {command}\n", shell_quote(&dir));
            if !self.host.type_input(t, line.into_bytes()) {
                return Err(Fail("not_found", format!("终端 {t} 没有 PTY")));
            }
            log!("恢复：对话 {cid} 在终端 {t} 代输 {command}（目录 {dir}）");
            return Ok(json!({"action": "typed", "terminal_id": t, "command": command, "dir": dir, "conversation": c}));
        }
        log!("恢复：对话 {cid} 在 {dir} 执行 {command}");
        Ok(json!({"action": "open", "command": command, "dir": dir, "conversation": c}))
    }

    /// 对话的启动目录：记录的 `cwd`，没有时用项目目录（一键恢复与「拷贝恢复命令」共用）。
    fn conversation_dir(&self, c: &Value) -> Result<String, Fail> {
        match c["cwd"].as_str() {
            Some(d) => Ok(d.to_owned()),
            None => c["project_id"]
                .as_i64()
                .and_then(|p| self.store.project(p).ok().flatten())
                .and_then(|p| p["dir"].as_str().map(str::to_owned))
                .ok_or_else(|| bad("对话没有目录")),
        }
    }

    /// 历史提示词（docs/design.md 10.5 节决策 10，docs/protocol.md 第 4 节）：沿 `resumed_from` 找出恢复链，由旧到新交给
    /// 读取线程，读完由它把应答交回（`Cmd::PromptsListed`）。对话不存在或读取线程未启用时立即回错误。
    fn prompts_list(&mut self, id: u64, req_id: Value, msg: &Value) {
        let mut chain = Vec::new();
        let mut next = msg["conversation_id"].as_i64();
        // 恢复链最多 20 段，防止库里出现环。
        while let Some(cid) = next.filter(|_| chain.len() < 20) {
            let Ok(Some(c)) = self.store.conversation(cid) else { break };
            next = c["resumed_from"].as_i64().filter(|r| !chain.iter().any(|s: &Segment| s.conversation_id == *r));
            if let (Some(tool), Some(native)) = (c["tool"].as_str().and_then(Tool::parse), c["native_id"].as_str()) {
                chain.push(Segment { conversation_id: cid, tool, native_id: native.to_owned(),
                                     transcript_path: c["transcript_path"].as_str().map(str::to_owned),
                                     prompted: !c["last_prompt"].is_null() });
            }
        }
        chain.reverse();
        let fail = |code: &str, message: &str| json!({"type": "reply", "id": req_id, "ok": false, "error": {"code": code, "message": message}});
        match &self.prompts {
            _ if chain.is_empty() => self.send_to(id, &fail("not_found", "对话不存在")),
            None => self.send_to(id, &fail("unavailable", "历史提示词未启用")),
            Some(worker) => worker.request(PromptJob { conn: id, req_id, home: PathBuf::from(&self.cli_home), segments: chain }),
        }
    }

    /// 「删除记录」（docs/design.md 第 5 节「移除与删除」）：只删已结束的对话；仍绑在存活终端上的（AI 已退出、
    /// 终端还开着）解除绑定、状态回到未知并推送该终端，之后推送 `conversation.remove`。
    fn conversation_delete(&mut self, msg: &Value) -> Result<Value, Fail> {
        let cid = msg["conversation_id"].as_i64().ok_or_else(|| bad("缺少 conversation_id"))?;
        match self.store.delete_conversation(cid)? {
            None => return Err(Fail("not_found", format!("对话 {cid} 不存在"))),
            Some(false) => return Err(bad("对话未结束")),
            Some(true) => {}
        }
        log!("对话 {cid} 已删除（删除记录）");
        self.removed(&[cid])?;
        Ok(Value::Null)
    }

    /// 对话已从库里删除（删除记录、保留清理、清除记录）：去掉用量；仍绑在存活终端上的（AI 已退出、终端还开着）
    /// 解除绑定、状态回到未知并推送该终端；之后逐条推送 `conversation.remove`。
    fn removed(&mut self, ids: &[i64]) -> Result<(), Fail> {
        if ids.is_empty() {
            return Ok(());
        }
        let gone: HashSet<i64> = ids.iter().copied().collect();
        self.usage.retain(|cid, _| !gone.contains(cid));
        let bound: Vec<String> = self
            .panes
            .iter()
            .filter(|(_, p)| p.conversation.is_some_and(|c| gone.contains(&c)))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &bound {
            if let Some(p) = self.panes.get_mut(id) {
                p.conversation = None;
                p.state = PaneState::default();
            }
            self.push_terminal(id, None)?;
        }
        let total = self.store.recent_total(now_ms())?;
        for cid in ids {
            self.push(json!({"type": "conversation.remove", "conversation_id": cid, "recent_total": total}));
        }
        Ok(())
    }

    /// 按保留设置清理（docs/protocol.md 第 7 节「保留上限」），被删的对话按 `removed` 处理。返回清理的条数。
    fn prune(&mut self, now: i64) -> Result<usize, Fail> {
        let ids = self.store.prune_conversations(now, self.retention.keep_days, self.retention.keep_conversations)?;
        self.removed(&ids)?;
        Ok(ids.len())
    }

    /// 修改保留设置（M4a 第 3 块）：`dry_run` 只数按新设置将被清理的条数；否则先写文件，再立即按新设置清理。
    fn retention_set(&mut self, msg: &Value) -> Result<Value, Fail> {
        let r = Retention::parse(msg).ok_or_else(|| {
            bad("keep_days 只能是 7、14、30、60、90，keep_conversations 只能是 50、100、200、500")
        })?;
        let now = now_ms();
        if msg["dry_run"] == true {
            let n = self.store.prunable(now, r.keep_days, r.keep_conversations)?.len();
            return Ok(json!({ "count": n }));
        }
        r.save(&self.retention_path)
            .map_err(|e| Fail("internal", format!("写入 {} 失败：{e}", self.retention_path.display())))?;
        self.retention = r;
        let n = self.prune(now)?;
        log!("保留设置改为 {} 天、{} 条，清理 {n} 条", r.keep_days, r.keep_conversations);
        Ok(json!({ "count": n, "retention": r.to_json() }))
    }

    /// 「清除记录」（M4a 第 3 块）：删除全部已结束的对话；`dry_run` 只数条数。
    fn history_clear(&mut self, msg: &Value) -> Result<Value, Fail> {
        if msg["dry_run"] == true {
            return Ok(json!({ "count": self.store.ended_ids()?.len() }));
        }
        let ids = self.store.delete_all_ended()?;
        log!("清除记录：删除已结束的对话 {} 条", ids.len());
        self.removed(&ids)?;
        Ok(json!({ "count": ids.len() }))
    }

    /// 新终端为恢复对话 `cid` 而开：重开对话、绑到该终端（状态未知），之后按普通对话处理（docs/protocol.md 第 7 节）。
    /// 此时已有终端在运行它（如双击）则不绑定，终端照常运行命令，同时打开时按冲突标出。
    fn bind_resume(&mut self, id: &str, cid: i64) -> Result<(), Fail> {
        let Some(c) = self.store.conversation(cid)? else {
            log!("恢复：对话 {cid} 不存在，终端 {id} 不绑定");
            return Ok(());
        };
        let (Some(tool), Some(native)) = (c["tool"].as_str().and_then(Tool::parse), c["native_id"].as_str()) else {
            return Ok(());
        };
        if let Some(other) = self.bound_runners(tool, native, Some(id)).first() {
            log!("恢复：对话 {cid} 已在终端 {other} 运行，终端 {id} 不绑定");
            return Ok(());
        }
        if !self.store.reopen_on(cid, id)? {
            return Ok(());
        }
        if let Some(p) = self.panes.get_mut(id) {
            p.state = PaneState::restored(tool, native.to_owned(), c["title"].as_str().map(str::to_owned));
            p.conversation = Some(cid);
        }
        log!("恢复：对话 {cid} 重开并绑到终端 {id}");
        self.push_conversation(cid)
    }

    /// 状态机绑定了这条对话且未结束的存活终端（含待确认的恢复与重启后状态未知的），按 ID 排序。
    fn bound_runners(&self, tool: Tool, native: &str, except: Option<&str>) -> Vec<String> {
        let mut v: Vec<String> = self
            .panes
            .iter()
            .filter(|(id, p)| {
                Some(id.as_str()) != except
                    && p.state.tool == Some(tool)
                    && p.state.session_id.as_deref() == Some(native)
                    && p.state.state != AgentState::Ended
            })
            .map(|(id, _)| id.clone())
            .collect();
        v.sort();
        v
    }

    /// 前台进程正连着这条对话的终端（适配器 `Hosted.attached`：Codex `resume <ID>`、Claude `attach <任务>` 与
    /// `parkedJobId`；这两种在首次提问前没有 SessionStart）。
    fn attached_runners(&self, tool: Tool, native: &str) -> Vec<String> {
        let Some(job) = adapter(tool).hosted.as_ref().and_then(|h| (h.job_of)(native)) else {
            return Vec::new();
        };
        let mut v: Vec<String> = self
            .panes
            .iter()
            .filter_map(|(id, p)| match &p.foreground {
                Foreground::Agent { pid, tool: t, .. } if *t == tool => Some((id, *pid)),
                _ => None,
            })
            .filter(|(_, pgid)| (self.probe.attached)(tool, *pgid).as_deref() == Some(job.as_str()))
            .map(|(id, _)| id.clone())
            .collect();
        v.sort();
        v
    }

    /// 重算各终端的 `conflict_with`（两个及以上存活终端绑定同一对话且都未结束），有变化的推送。
    fn refresh_conflicts(&mut self) {
        let mut groups: HashMap<(&'static str, String), Vec<String>> = HashMap::new();
        for (id, p) in &self.panes {
            if let (Some(t), Some(s)) = (p.state.tool, &p.state.session_id) {
                if p.state.state != AgentState::Ended {
                    groups.entry((t.as_str(), s.clone())).or_default().push(id.clone());
                }
            }
        }
        let mut changed = Vec::new();
        for (id, p) in self.panes.iter_mut() {
            let mut others: Vec<String> = match (p.state.tool, &p.state.session_id) {
                (Some(t), Some(s)) if p.state.state != AgentState::Ended => groups
                    .get(&(t.as_str(), s.clone()))
                    .map(|g| g.iter().filter(|x| *x != id).cloned().collect())
                    .unwrap_or_default(),
                _ => Vec::new(),
            };
            others.sort();
            if others != p.conflict {
                if !others.is_empty() {
                    log!("冲突：终端 {id} 与 {others:?} 打开了同一对话");
                }
                p.conflict = others;
                changed.push(id.clone());
            }
        }
        for id in changed {
            if let Err(Fail(_, e)) = self.push_terminal(&id, None) {
                log!("推送终端失败：{e}");
            }
        }
    }

    fn push_conversation(&mut self, id: i64) -> Result<(), Fail> {
        if let Some(c) = self.store.conversation(id)? {
            if !c["ended_at"].is_null() {
                self.usage.remove(&id);
            }
            let total = self.store.recent_total(now_ms())?;
            self.push(json!({"type": "conversation.upsert", "conversation": c, "recent_total": total}));
        }
        Ok(())
    }
}

/// 对话的用量（只在内存中）：模型与来源、会话累计 token、推理强度（维护期 0.3.5）、是否按量计费（2026-11-01）。
#[derive(Default)]
struct Usage {
    model: Option<(String, &'static str)>,
    tokens: Option<Tokens>,
    effort: Option<String>,
    api: bool,
}

/// 用量对象（docs/protocol.md 第 5 节）：没读到 token、推理强度时不带对应字段。
/// 各款工具读得到哪些信息（docs/protocol.md 第 4 节 `snapshot.get` 的 `tools`，2026-10-30）：读不到的由 App 写明「暂不支持」，
/// 不空着、不借用别的原因（docs/ui-methodology.md 1.7 节）。按适配器的声明给出，App 不按工具名判断。
fn tools_json() -> Value {
    json!(scyllaui_proto::hookcfg::TOOLS
        .iter()
        .map(|t| {
            let a = adapter(*t);
            json!({"tool": t.as_str(), "reply": a.reply_source.has_reply(), "prompts": a.reply_source.has_prompts(),
                   "usage": a.usage_source != scyllaui_proto::adapters::usage::UsageSource::None, "model": a.has_model(),
                   "effort": a.effort_source != scyllaui_proto::adapters::usage::EffortSource::None})
        })
        .collect::<Vec<_>>())
}

fn usage_json(u: &Usage) -> Value {
    let mut v = json!({"model": u.model.as_ref().map(|m| &m.0), "model_source": u.model.as_ref().map(|m| m.1)});
    if let Some(e) = &u.effort {
        v["effort"] = json!(e);
    }
    if u.api {
        v["billing"] = json!("api");
    }
    if let Some(t) = u.tokens {
        v["total_tokens"] = json!(t.total);
        v["tokens"] = t.to_json();
    }
    v
}

/// 周额度对象（docs/protocol.md 第 5 节）。
fn quota_json((q, source): &(Quota, &'static str)) -> Value {
    if q.unlimited {
        return json!({"window": q.window.as_str(), "unlimited": true, "as_of": q.as_of, "source": source});
    }
    json!({"window": q.window.as_str(), "used_percent": q.used_percent, "resets_at": q.resets_at, "as_of": q.as_of, "source": source})
}

/// 单引号括起，供代输的命令行使用（与 App 的 `TerminalSession.shellQuote` 相同）。
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 恢复命令（docs/protocol.md 第 7 节「一键恢复」）：守护进程托管的后台任务（还活着，或停止了但记录还在）用适配器的
/// 接回命令（Claude `attach <任务>`），否则用适配器的恢复命令。ID 不合规则时为 None。
fn resume_command(tool: Tool, native: &str, probe: &Probe) -> Option<String> {
    let a = adapter(tool);
    let live = a
        .hosted
        .as_ref()
        .filter(|h| h.reattach_args.is_some())
        .and_then(|h| (h.job_of)(native))
        .filter(|job| (probe.is_job)(tool, job, native));
    live.and_then(|job| a.reattach(&job)).or_else(|| a.resume(native))
}

/// 交给写线程；队列满或写线程已退出时返回 false，由调用方丢弃该连接。
/// 换版本前让新程序自检（docs/protocol.md 第 1 节「接管」）：能启动、能打开库、库版本受支持、认识接管文件格式。最多等 5 秒。
fn preflight(path: &str, data_dir: &Path) -> Result<(), String> {
    let mut child = std::process::Command::new(path)
        .arg("--preflight")
        .arg("--data-dir")
        .arg(data_dir)
        .args(["--takeover-format", &pty::takeover::FORMAT.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("启动 {path} 失败：{e}"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => return Ok(()),
            Ok(Some(st)) => return Err(format!("自检退出码 {st}")),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("自检 5 秒未结束".into());
            }
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// execv 新程序；只在失败时返回。
fn exec(path: &str, args: &[&str]) -> std::io::Error {
    let c = |s: &str| std::ffi::CString::new(s).unwrap_or_default();
    let argv: Vec<std::ffi::CString> = std::iter::once(path).chain(args.iter().copied()).map(c).collect();
    let mut ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    unsafe { libc::execv(argv[0].as_ptr(), ptrs.as_ptr()) };
    std::io::Error::last_os_error()
}

fn parse_rgb(s: &str) -> Option<[u8; 3]> {
    let h = s.strip_prefix('#').filter(|h| h.len() == 6 && h.is_ascii())?;
    let n = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some([n(0)?, n(2)?, n(4)?])
}

fn send(out: &SyncSender<Out>, msg: &Value) -> bool {
    let mut buf = serde_json::to_vec(msg).unwrap_or_default();
    buf.push(b'\n');
    out.try_send(Out::Line(buf)).is_ok()
}

/// 版本样本：按来源追加到 `<目录>/<source>.jsonl`（格式与 S0 相同，供 s0-curate-samples.py 使用）。
fn capture(dir: &Path, frame: &Value, recv_us: u64) {
    let source = frame["source"].as_str().unwrap_or("unknown");
    let path = dir.join(format!("{source}.jsonl"));
    let line = json!({"daemon_recv_us": recv_us, "frame": frame});
    let r = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| {
            let mut buf = serde_json::to_vec(&line)?;
            buf.push(b'\n');
            f.write_all(&buf)
        });
    if let Err(e) = r {
        log!("写入样本 {} 失败：{e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_dir;
    use crate::title::Found;
    use std::sync::mpsc::Receiver;

    struct Bench {
        core: Core,
        dir: PathBuf,
        next_id: u64,
        /// ExitWatch 的通知；测试里手动交给核心。
        exits: Receiver<Cmd>,
    }

    impl Drop for Bench {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn bench(name: &str) -> Bench {
        bench_with_titles(name, |_| None)
    }

    /// 读标题的函数由测试注入，不读用户主目录下的真实文件。
    fn bench_with_titles(
        name: &str,
        mut read: impl FnMut(&TitleJob) -> Option<String> + Send + 'static,
    ) -> Bench {
        let dir = test_dir(&format!("core-{name}"));
        let store = Store::open(&dir.join("t.db")).unwrap();
        let (tx, exits) = mpsc::channel();
        let mut b = Bench {
            core: Core::new(
                store,
                dir.join("s.sock"),
                None,
                ExitWatch::new(tx.clone()).unwrap(),
                GitWorker::new(crate::git::GitWatch::new(tx.clone()).unwrap(), tx.clone()),
                TitleWorker::with_reader(tx.clone(), move |j| Found { title: read(j), ..Found::default() }),
                RouteWorker::with_resolver(tx.clone(), |_, _| None),
                HookWatch::inert(),
                VersionWorker::with_resolver(tx.clone(), |_| None),
                crate::waitwatch::WaitWatch::inert(),
                dir.display().to_string(),
                crate::pty::Host::new(tx),
                dir.join("retention.json"),
            )
            .unwrap(),
            dir,
            next_id: 0,
            exits,
        };
        b.core.probe = probe(&[], &[], &[]);
        b
    }

    /// 一键恢复用的假探测（不读用户主目录）：前台进程组连着的任务、还活着的后台任务、不存在的文件。
    fn probe(attached: &[(i32, &str)], alive: &[&str], missing: &[&str]) -> Probe {
        let attached: HashMap<i32, String> = attached.iter().map(|(p, j)| (*p, j.to_string())).collect();
        let alive: Vec<String> = alive.iter().map(|s| s.to_string()).collect();
        let missing: Vec<String> = missing.iter().map(|s| s.to_string()).collect();
        Probe {
            attached: Box::new(move |_, pgid| attached.get(&pgid).cloned()),
            is_job: Box::new(move |_, job, _| alive.iter().any(|a| a == job)),
            exists: Box::new(move |path| !missing.iter().any(|m| m == path)),
        }
    }

    fn drain(rx: &Receiver<Out>) -> Vec<Value> {
        let mut v = Vec::new();
        while let Ok(Out::Line(buf)) = rx.try_recv() {
            v.push(serde_json::from_slice(&buf).unwrap());
        }
        v
    }

    impl Bench {
        fn connect(&mut self, role: &str, version: u64) -> (u64, Receiver<Out>, Value) {
            self.next_id += 1;
            let (tx, rx) = mpsc::sync_channel(4096);
            let hello =
                json!({"type":"hello","role":role,"protocol_version":version,"build_id":"t"});
            self.core.handle(Cmd::Hello {
                conn: self.next_id,
                hello,
                out: tx,
            });
            let greeting = drain(&rx).remove(0);
            (self.next_id, rx, greeting)
        }

        /// 发一条请求，返回应答与期间的推送。
        fn req(&mut self, conn: u64, rx: &Receiver<Out>, mut msg: Value) -> (Value, Vec<Value>) {
            msg["id"] = json!(7);
            self.core.handle(Cmd::Request { conn, msg });
            let mut out = drain(rx);
            let i = out
                .iter()
                .position(|m| m["type"] == "reply")
                .expect("应有应答");
            let reply = out.remove(i);
            assert_eq!(reply["id"], 7);
            (reply, out)
        }

        fn app(&mut self, terminals: &[&str]) -> (u64, Receiver<Out>, Value) {
            let (id, rx, g) = self.connect("app", PROTOCOL_VERSION);
            assert_eq!(g["type"], "hello_ok");
            assert!(g["exe"].as_str().is_some_and(|p| p.starts_with('/')), "hello_ok 带后台自身的绝对路径（M4b 块 2）");
            let list: Vec<Value> = terminals
                .iter()
                .map(|t| json!({"terminal_id": t, "project_dir": "/w/p/", "cwd": "/w/p"}))
                .collect();
            let (reply, _) = self.req(id, &rx, json!({"type":"sync","terminals":list}));
            assert_eq!(reply["ok"], true, "{reply}");
            (id, rx, reply["snapshot"].clone())
        }

        fn hook(&mut self, terminal: &str, source: &str, payload: Value) {
            self.hook_frame(terminal, source, false, &payload);
        }

        fn hook_frame(&mut self, terminal: &str, source: &str, dropped: bool, payload: &Value) {
            let frame = json!({"type":"hook","source":source,"dropped":dropped,"pane_id":terminal,
                "hook_t0_us": 1, "payload": payload});
            self.core.handle(Cmd::Hook { frame, recv_us: 2 });
        }

        /// 指定后台收到的时刻（微秒）：Kimi 以收到提交的时刻作轮次标识。
        fn hook_at(&mut self, terminal: &str, source: &str, payload: &Value, recv_us: u64) {
            let frame = json!({"type":"hook","source":source,"dropped":false,"pane_id":terminal,
                "hook_t0_us": 1, "payload": payload});
            self.core.handle(Cmd::Hook { frame, recv_us });
        }

        /// 等到符合条件的后台命令并返回（不处理）；其余命令照常交给核心。
        fn next_cmd(&mut self, want: impl Fn(&Cmd) -> bool, what: &str) -> Cmd {
            loop {
                let cmd = self.exits.recv_timeout(Duration::from_secs(5)).expect(what);
                if want(&cmd) {
                    return cmd;
                }
                self.core.handle(cmd);
            }
        }

        /// 处理后台线程（GitWorker、kqueue）送回的命令，直到 300 ms 内没有新的为止。
        fn settle(&mut self) {
            while let Ok(cmd) = self.exits.recv_timeout(Duration::from_millis(300)) {
                self.core.handle(cmd);
            }
        }

        fn count(&self, sql: &str) -> i64 {
            self.core.store.count(sql)
        }
    }

    fn samples() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../tests/hook-samples")
    }

    /// 六份真实样本各在一个终端里回放（codex-0.161.0-resume 在新终端里恢复同一会话）。
    #[test]
    fn recorded_samples_become_conversations() {
        let files = [
            "claude-2.1.292",
            "codex-0.160.0",
            "codex-0.161.0",
            "codex-0.161.0-resume",
            "kimi-2.1.1",
            "grok-1.0.46",
            "claude-2.1.296",
            "codex-0.162.0",
            "grok-1.0.50",
        ];
        let mut b = bench("samples");
        let (_app, rx, _) = b.app(&files);
        for f in files {
            let text = fs::read_to_string(samples().join(format!("{f}.jsonl"))).unwrap();
            for line in text.lines() {
                let row: Value = serde_json::from_str(line).unwrap();
                let dropped = row["dropped"].as_bool().unwrap();
                b.hook_frame(f, row["source"].as_str().unwrap(), dropped, &row["payload"]);
            }
        }
        // S0 六份：Claude 两段（同一会话退出后恢复 + 以 --session-id 新开），Codex 0.161.0 与其恢复是同一条。
        // 2026-10-10 三份：Claude 两段、Codex 一段、Grok 一段（被 kill -9，没有 SessionEnd，仍未结束）。
        assert_eq!(b.count("SELECT COUNT(*) FROM conversation"), 6 + 4);
        assert_eq!(
            b.count("SELECT COUNT(*) FROM conversation WHERE end_reason = 'session_end'"),
            6 + 3
        );
        assert_eq!(
            b.count("SELECT COUNT(*) FROM conversation WHERE ended_at IS NULL AND tool = 'grok'"),
            1
        );
        // 后台记忆整理会话（7 条）、Grok 重复来源（丢弃）与 SessionEnd 之后的会话级 Stop（2 条）不落库。
        assert_eq!(
            b.count("SELECT COUNT(*) FROM event"),
            14 + 7 + 7 + 4 + 12 + 13 + 9 + 4 + 4
        );
        // 新样本的提示词写入「最新提示词」。
        assert_eq!(
            b.count("SELECT COUNT(*) FROM conversation WHERE native_id LIKE 'e67a6458%' AND last_prompt IS NOT NULL"),
            1
        );
        assert_eq!(
            b.count("SELECT COUNT(*) FROM conversation WHERE native_id LIKE '%f19eac'"),
            0
        );
        let resumed = b.count(
            "SELECT COUNT(*) FROM conversation WHERE native_id LIKE '%8da12f' \
             AND terminal_id = 'codex-0.161.0-resume'",
        );
        assert_eq!(resumed, 1, "恢复后换绑到新终端");
        let pushes = drain(&rx);
        let ended = pushes
            .iter()
            .filter(|m| {
                m["type"] == "conversation.upsert"
                    && m["conversation"]["end_reason"] == "session_end"
            })
            .count();
        assert!(ended >= 9);
        let revs: Vec<u64> = pushes.iter().map(|m| m["rev"].as_u64().unwrap()).collect();
        assert!(revs.windows(2).all(|w| w[0] < w[1]), "rev 单调递增");
    }

    #[test]
    fn clear_replaces_previous_conversation() {
        let mut b = bench("clear");
        let (_app, rx, snap) = b.app(&["t1"]);
        assert_eq!(snap["terminals"][0]["agent"]["state"], "unknown");
        assert_eq!(snap["projects"][0]["dir"], "/w/p", "去掉末尾斜杠");
        b.hook(
            "t1",
            "claude",
            json!({"hook_event_name":"SessionStart","session_id":"a"}),
        );
        b.hook(
            "t1",
            "claude",
            json!({"hook_event_name":"UserPromptSubmit","session_id":"a"}),
        );
        drain(&rx);
        b.hook(
            "t1",
            "claude",
            json!({"hook_event_name":"SessionStart","session_id":"b","source":"clear"}),
        );
        let pushes = drain(&rx);
        let a = pushes
            .iter()
            .find(|m| m["conversation"]["native_id"] == "a")
            .unwrap();
        assert_eq!(a["conversation"]["end_reason"], "replaced");
        let t = pushes
            .iter()
            .find(|m| m["type"] == "terminal.upsert")
            .unwrap();
        assert_eq!(t["terminal"]["agent"]["session_id"], "b");
        assert_eq!(t["trace"]["event"], "SessionStart");
        assert!(t["trace"]["daemon_send_us"].as_u64().is_some());
        let b_id = b.count("SELECT id FROM conversation WHERE native_id = 'b'");
        assert_eq!(t["terminal"]["conversation_id"], b_id);
    }

    #[test]
    fn terminal_exit_and_app_disconnect_close_terminals() {
        let mut b = bench("close");
        let (app, rx, _) = b.app(&["t1", "t2"]);
        b.hook(
            "t1",
            "claude",
            json!({"hook_event_name":"SessionStart","session_id":"a"}),
        );
        b.hook(
            "t2",
            "codex",
            json!({"hook_event_name":"SessionStart","session_id":"c"}),
        );
        drain(&rx);
        let (reply, pushes) = b.req(
            app,
            &rx,
            json!({"type":"terminal.exit","terminal_id":"t1","exit_code":0}),
        );
        assert_eq!(reply["ok"], true);
        assert!(pushes.iter().any(|m| m["terminal"]["status"] == "closed"));
        let (reply, _) = b.req(app, &rx, json!({"type":"terminal.exit","terminal_id":"t1"}));
        assert_eq!(reply["error"]["code"], "not_found");
        b.core.handle(Cmd::Closed { conn: app });
        assert_eq!(
            b.count(
                "SELECT COUNT(*) FROM terminal WHERE status = 'closed' AND close_reason = \
                     CASE id WHEN 't1' THEN 'exit' ELSE 'app_exit' END"
            ),
            2
        );
        assert_eq!(
            b.count("SELECT COUNT(*) FROM conversation WHERE end_reason = 'terminal_closed'"),
            2
        );
        // 终端已关闭：之后的 Hook 丢弃。
        b.hook(
            "t2",
            "codex",
            json!({"hook_event_name":"Stop","session_id":"c"}),
        );
        assert_eq!(b.count("SELECT COUNT(*) FROM event"), 2);
        // App 重新连接即可再接入（单个 App 的名额已释放）。
        let (_, _, snap) = b.app(&[]);
        assert_eq!(snap["terminals"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn ignored_events_are_not_recorded() {
        let mut b = bench("ignored");
        let _app = b.app(&["t1"]);
        b.hook(
            "t1",
            "claude",
            json!({"hook_event_name":"SessionStart","session_id":"a"}),
        );
        b.hook(
            "t1",
            "claude",
            json!({"hook_event_name":"Stop","session_id":"a","agent_id":"sub"}),
        );
        b.hook(
            "t1",
            "codex",
            json!({"hook_event_name":"SessionStart","session_id":"m","transcript_path":null}),
        );
        b.hook(
            "t1",
            "claude",
            json!({"hook_event_name":"Stop","session_id":"other"}),
        );
        b.hook(
            "t-unknown",
            "claude",
            json!({"hook_event_name":"SessionStart","session_id":"z"}),
        );
        b.hook_frame(
            "t1",
            "grok",
            true,
            &json!({"hook_event_name":"SessionStart","sessionId":"g"}),
        );
        assert_eq!(b.count("SELECT COUNT(*) FROM event"), 1);
        assert_eq!(b.count("SELECT COUNT(*) FROM conversation"), 1);
        assert_eq!(
            b.count("SELECT COUNT(*) FROM terminal WHERE status = 'alive'"),
            1
        );
    }

    #[test]
    fn restart_restores_binding_with_unknown_state_and_sync_reconciles() {
        let mut b = bench("restart");
        let _app = b.app(&["t1", "t2"]);
        b.hook(
            "t1",
            "kimi",
            json!({"hook_event_name":"SessionStart","session_id":"a","session_title":"T"}),
        );
        assert!(!b.core.panes["t1"].inherited, "本进程登记的终端不算旧后台留下的");
        b.hook(
            "t1",
            "kimi",
            json!({"hook_event_name":"UserPromptSubmit","session_id":"a"}),
        );
        // 模拟后台崩溃：不经 App 断开，直接用同一个库重建核心。
        let store = Store::open(&b.dir.join("t.db")).unwrap();
        let (tx, exits) = mpsc::channel();
        b.core = Core::new(
            store,
            b.dir.join("s.sock"),
            None,
            ExitWatch::new(tx.clone()).unwrap(),
            GitWorker::new(crate::git::GitWatch::new(tx.clone()).unwrap(), tx.clone()),
            TitleWorker::with_reader(tx.clone(), |_| Found::default()),
            RouteWorker::with_resolver(tx.clone(), |_, _| None),
            HookWatch::inert(),
            VersionWorker::with_resolver(tx.clone(), |_| None),
            crate::waitwatch::WaitWatch::inert(),
            b.dir.display().to_string(),
            crate::pty::Host::new(tx),
            b.dir.join("retention.json"),
        )
        .unwrap();
        b.exits = exits;
        assert!(b.core.panes["t1"].inherited && b.core.panes["t2"].inherited, "重启时从库里接回的终端（决策 21）");
        let (cli, rx, g) = b.connect("cli", PROTOCOL_VERSION);
        assert_eq!(g["type"], "hello_ok");
        let (reply, _) = b.req(cli, &rx, json!({"type":"snapshot.get"}));
        let t1 = &reply["snapshot"]["terminals"][0];
        assert_eq!(
            (
                t1["agent"]["state"].as_str(),
                t1["agent"]["session_id"].as_str(),
                t1["agent"]["title"].as_str()
            ),
            (Some("unknown"), Some("a"), Some("T"))
        );
        assert_eq!(
            reply["snapshot"]["conversations"].as_array().unwrap().len(),
            1
        );
        // 恢复的绑定生效：其他会话的事件忽略，本会话的事件推进状态。
        b.hook(
            "t1",
            "kimi",
            json!({"hook_event_name":"Stop","session_id":"b"}),
        );
        b.hook(
            "t1",
            "kimi",
            json!({"hook_event_name":"PermissionRequest","session_id":"a"}),
        );
        let (_app, _rx, snap) = b.app(&["t1"]);
        let terms = snap["terminals"].as_array().unwrap();
        assert_eq!(terms.len(), 1, "sync 未列出的 t2 被关闭");
        assert_eq!(terms[0]["agent"]["state"], "waiting");
        assert_eq!(
            b.count("SELECT COUNT(*) FROM terminal WHERE id = 't2' AND close_reason = 'missing_on_sync'"),
            1
        );
    }

    #[test]
    fn handshake_and_request_rules() {
        let mut b = bench("rules");
        let (app, app_rx, _) = b.app(&["t1"]);
        let (_, _, g) = b.connect("app", PROTOCOL_VERSION);
        assert_eq!(
            (g["type"].as_str(), g["code"].as_str()),
            (Some("hello_err"), Some("app_already_connected"))
        );
        let (old, old_rx, g) = b.connect("cli", 1);
        assert_eq!(g["code"], "protocol_mismatch");
        b.core.handle(Cmd::Request {
            conn: old,
            msg: json!({"type":"snapshot.get","id":1}),
        });
        assert!(drain(&old_rx).is_empty(), "hello_err 之后只处理 shutdown");
        let (reply, _) = b.req(old, &old_rx, json!({"type":"shutdown"}));
        assert_eq!(
            reply["error"]["code"], "app_connected",
            "已有 App 时拒绝其他连接的 shutdown"
        );
        let (cli, rx, _) = b.connect("cli", PROTOCOL_VERSION);
        let (reply, _) = b.req(
            cli,
            &rx,
            json!({"type":"terminal.open","terminal_id":"x","project_dir":"/a","cwd":"/a"}),
        );
        assert_eq!(reply["error"]["code"], "bad_request");
        let (reply, _) = b.req(cli, &rx, json!({"type":"nope"}));
        assert_eq!(reply["error"]["code"], "unknown_type");
        let (reply, _) = b.req(
            app,
            &app_rx,
            json!({"type":"terminal.open","terminal_id":"x","project_dir":"rel","cwd":"/a"}),
        );
        assert_eq!(reply["error"]["code"], "bad_request");
        let (reply, pushes) = b.req(
            app,
            &app_rx,
            json!({"type":"terminal.rename","terminal_id":"t1","name":"构建"}),
        );
        assert_eq!(reply["ok"], true);
        assert_eq!(pushes[0]["terminal"]["name"], "构建");
        let pid = b.count("SELECT id FROM project");
        let (reply, pushes) = b.req(
            app,
            &app_rx,
            json!({"type":"project.update","project_id":pid,"name":"P","sort_order":5}),
        );
        assert_eq!(reply["ok"], true);
        assert_eq!(
            (
                pushes[0]["project"]["name"].as_str(),
                pushes[0]["project"]["sort_order"].as_i64()
            ),
            (Some("P"), Some(5))
        );
        assert_eq!(pushes[0]["project"]["pinned"], false);
        // 置顶（0.3.5）：只改 pinned，其余不变，推送带布尔；不是布尔为 bad_request。
        let (reply, pushes) = b.req(app, &app_rx, json!({"type":"project.update","project_id":pid,"pinned":true}));
        assert_eq!(reply["ok"], true);
        assert_eq!(
            (pushes[0]["project"]["pinned"].clone(), pushes[0]["project"]["name"].clone(), pushes[0]["project"]["sort_order"].clone()),
            (json!(true), json!("P"), json!(5))
        );
        let (snap, _) = b.req(app, &app_rx, json!({"type": "snapshot.get"}));
        assert_eq!(snap["snapshot"]["projects"][0]["pinned"], true);
        let (reply, _) = b.req(app, &app_rx, json!({"type":"project.update","project_id":pid,"pinned":1}));
        assert_eq!(reply["error"]["code"], "bad_request");
        let (reply, _) = b.req(
            cli,
            &rx,
            json!({"type":"recent.query","before":{"ended_at":1}}),
        );
        assert_eq!(reply["error"]["code"], "bad_request");
        let (reply, _) = b.req(cli, &rx, json!({"type":"recent.query","limit":1000}));
        assert_eq!(
            (
                reply["items"].as_array().map(Vec::len),
                reply["next"].is_null()
            ),
            (Some(0), true)
        );
    }

    #[test]
    fn full_queue_drops_the_app_connection() {
        let mut b = bench("queue");
        b.next_id += 1;
        let (tx, _rx) = mpsc::sync_channel(2);
        let hello = json!({"type":"hello","role":"app","protocol_version":PROTOCOL_VERSION});
        b.core.handle(Cmd::Hello {
            conn: b.next_id,
            hello,
            out: tx,
        });
        b.core.handle(Cmd::Request {
            conn: b.next_id,
            msg: json!({"type":"sync","id":1,"terminals":[
            {"terminal_id":"t1","project_dir":"/w","cwd":"/w"}]}),
        });
        // 队列（2 条）已被 hello_ok 与应答占满，下一次推送即丢弃该 App，并按 App 断开处理。
        b.hook(
            "t1",
            "claude",
            json!({"hook_event_name":"SessionStart","session_id":"a"}),
        );
        assert!(b.core.app.is_none());
        assert_eq!(
            b.count("SELECT COUNT(*) FROM terminal WHERE close_reason = 'app_exit'"),
            1
        );
    }

    /// 回归（2026-11-13 长跑缺陷 B）：CLI 自己投递的提交（Copilot 子代理结束的通知、Kimi 子代理收到的任务）改状态，
    /// 但不写进 last_prompt，也不当任务标题。
    #[test]
    fn injected_prompt_keeps_title_and_last_prompt() {
        let mut b = bench("injected");
        let (_app, _rx, _) = b.app(&["t1", "t2"]);
        b.hook("t1", "copilot", json!({"hook_event_name":"userPromptSubmitted","sessionId":"p",
            "prompt":"<system_notification>\nAgent \"largest-file\" (general-purpose) has finished processing and is now idle.\n</system_notification>"}));
        // Kimi 子代理收到的任务算子代理事件（2026-11-14）：先有主会话，它才有归属。
        b.hook("t2", "kimi", json!({"hook_event_name":"SessionStart","session_id":"k"}));
        b.hook("t2", "kimi", json!({"hook_event_name":"TurnStarted","session_id":"k","turn_id":0,
            "origin_kind":"system_trigger","origin_name":"subagent","prompt":"<git-context>\nBranch: main\n</git-context>"}));
        assert_eq!(b.count("SELECT COUNT(*) FROM conversation WHERE last_prompt IS NULL AND title IS NULL"), 2);
        assert_eq!(b.core.panes["t1"].state.state, AgentState::Running);
        b.hook("t1", "copilot", json!({"hook_event_name":"userPromptSubmitted","sessionId":"p","prompt":"数一下目录"}));
        assert_eq!(b.count("SELECT COUNT(*) FROM conversation WHERE last_prompt = '数一下目录' AND title = '数一下目录'"), 1);
    }

    #[test]
    fn prompt_is_kept_as_last_prompt() {
        let mut b = bench("prompt");
        let (_app, rx, _) = b.app(&["t1"]);
        b.hook(
            "t1",
            "codex",
            json!({"hook_event_name":"SessionStart","session_id":"c"}),
        );
        drain(&rx);
        let long = "改".repeat(450);
        b.hook("t1", "codex", json!({"hook_event_name":"UserPromptSubmit","session_id":"c","prompt":long,"turn_id":"1"}));
        let pushes = drain(&rx);
        let conv = pushes
            .iter()
            .find(|m| m["type"] == "conversation.upsert")
            .expect("提示词变化应推送对话");
        assert_eq!(
            conv["conversation"]["last_prompt"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            400
        );
        // 一轮结束：记下最后一条回复的时间（M1 第 6 块），推送对话。
        b.hook(
            "t1",
            "codex",
            json!({"hook_event_name":"Stop","session_id":"c","turn_id":"1"}),
        );
        let pushes = drain(&rx);
        let conv = pushes.iter().find(|m| m["type"] == "conversation.upsert").expect("一轮结束应推送对话");
        assert!(conv["conversation"]["last_reply_at"].as_i64().is_some());
        b.hook(
            "t1",
            "codex",
            json!({"hook_event_name":"PreToolUse","session_id":"c","turn_id":"2"}),
        );
        assert!(
            drain(&rx)
                .iter()
                .all(|m| m["type"] != "conversation.upsert"),
            "只有 last_event_at 变化时不推送对话"
        );
        assert_eq!(
            b.count("SELECT COUNT(*) FROM event WHERE kind = 'UserPromptSubmit'"),
            1,
            "事件表存原生事件名"
        );
    }

    /// 把休眠程序（procinfo::testbin）放到 Kimi 的安装路径，在独立进程组中运行。
    fn fake_kimi(b: &Bench) -> std::process::Child {
        use std::os::unix::process::CommandExt;
        let exe = b.dir.join(".kimi-code/bin/kimi");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        procinfo::testbin::install(&exe);
        let child = procinfo::testbin::sleep(&exe, 30)
            .process_group(0)
            .spawn()
            .unwrap();
        // 等 exec 完成，识别读到的是伪装路径而不是父进程映像。
        let pid = child.id() as i32;
        for _ in 0..100 {
            if procinfo::pid_path(pid).is_some_and(|p| p.ends_with("/.kimi-code/bin/kimi")) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        child
    }

    fn foreground(b: &mut Bench, app: u64, rx: &Receiver<Out>, pgid: i32) -> Value {
        let (reply, pushes) = b.req(
            app,
            rx,
            json!({"type":"terminal.foreground","terminal_id":"t1","pgid":pgid}),
        );
        assert_eq!(reply["ok"], true, "{reply}");
        pushes
            .last()
            .map(|m| m["terminal"]["foreground"].clone())
            .unwrap_or(Value::Null)
    }

    /// 等进程退出通知；期间到达的 GitWorker 结果照常交给核心。
    fn exit_cmd(b: &mut Bench) -> Cmd {
        b.next_cmd(|c| matches!(c, Cmd::ProcExit { .. }), "应收到进程退出")
    }

    #[test]
    fn foreground_agent_exit_without_session_end_ends_conversation() {
        let mut b = bench("fg-exit");
        let (app, rx, _) = b.app(&["t1"]);
        let mut cli = fake_kimi(&b);
        let pid = cli.id() as i32;
        let fg = foreground(&mut b, app, &rx, pid);
        assert_eq!(
            (fg["kind"].as_str(), fg["tool"].as_str(), fg["pid"].as_i64()),
            (Some("agent"), Some("kimi"), Some(pid as i64))
        );
        b.hook(
            "t1",
            "kimi",
            json!({"hook_event_name":"SessionStart","session_id":"session_k"}),
        );
        cli.kill().unwrap();
        cli.wait().unwrap();
        let cmd = exit_cmd(&mut b);
        let t0 = Instant::now();
        b.core.handle(cmd);
        let pushes = drain(&rx);
        assert_eq!(
            pushes.last().unwrap()["terminal"]["foreground"]["kind"],
            "unknown"
        );
        b.core.expire_exits(t0 + Duration::from_millis(500));
        assert_eq!(
            b.count("SELECT COUNT(*) FROM conversation WHERE ended_at IS NULL"),
            1,
            "宽限期内不结束"
        );
        b.core
            .expire_exits(t0 + EXIT_GRACE + Duration::from_millis(1));
        assert_eq!(
            b.count("SELECT COUNT(*) FROM conversation WHERE end_reason = 'process_exit'"),
            1
        );
        let pushes = drain(&rx);
        assert_eq!(
            pushes.last().unwrap()["terminal"]["agent"]["state"],
            "ended"
        );
        // 之后回到 Shell：App 上报 Shell 的进程组。
        let fg = foreground(&mut b, app, &rx, std::process::id() as i32);
        assert_ne!(fg["kind"], "agent");
    }

    #[test]
    fn session_end_within_grace_wins() {
        let mut b = bench("fg-grace");
        let (app, rx, _) = b.app(&["t1"]);
        let mut cli = fake_kimi(&b);
        foreground(&mut b, app, &rx, cli.id() as i32);
        b.hook(
            "t1",
            "kimi",
            json!({"hook_event_name":"SessionStart","session_id":"session_k"}),
        );
        cli.kill().unwrap();
        cli.wait().unwrap();
        let cmd = exit_cmd(&mut b);
        let t0 = Instant::now();
        b.core.handle(cmd);
        b.hook(
            "t1",
            "kimi",
            json!({"hook_event_name":"SessionEnd","session_id":"session_k"}),
        );
        b.core.expire_exits(t0 + EXIT_GRACE * 2);
        assert_eq!(
            b.count("SELECT COUNT(*) FROM conversation WHERE end_reason = 'session_end'"),
            1
        );
        assert_eq!(
            b.count("SELECT COUNT(*) FROM conversation WHERE end_reason = 'process_exit'"),
            0
        );
    }

    fn conv_title(b: &Bench, native: &str) -> (Option<String>, Option<String>) {
        b.core.store.title_of(native)
    }

    /// 任务标题（docs/design.md 第 5 节）：第一条提示词是标题，之后的提示词不改它；读到原生标题即覆盖并推送。
    #[test]
    fn first_prompt_is_title_until_native_title_arrives() {
        let mut b = bench_with_titles("title-rules", |j| {
            (j.native_id == "c").then(|| "确认当前时区".to_owned())
        });
        let (_app, rx, _) = b.app(&["t1"]);
        b.hook("t1", "codex", json!({"hook_event_name":"SessionStart","session_id":"c"}));
        assert_eq!(conv_title(&b, "c"), (None, None));
        b.hook("t1", "codex", json!({"hook_event_name":"UserPromptSubmit","session_id":"c","prompt":"现在是哪个时区？","turn_id":"1"}));
        assert_eq!(conv_title(&b, "c"), (Some("现在是哪个时区？".into()), Some("prompt".into())));
        b.hook("t1", "codex", json!({"hook_event_name":"UserPromptSubmit","session_id":"c","prompt":"今天星期几？","turn_id":"2"}));
        assert_eq!(conv_title(&b, "c").0.as_deref(), Some("现在是哪个时区？"), "第二条提示词不改标题");
        drain(&rx);
        // 一轮结束：立即读一次，读到原生标题。
        b.hook("t1", "codex", json!({"hook_event_name":"Stop","session_id":"c","turn_id":"2"}));
        let cmd = b.next_cmd(|c| matches!(c, Cmd::TitleRead { .. }), "应读到原生标题");
        b.core.handle(cmd);
        assert_eq!(conv_title(&b, "c"), (Some("确认当前时区".into()), Some("native".into())));
        let pushes = drain(&rx);
        assert!(
            pushes.iter().any(|m| m["type"] == "conversation.upsert"
                && m["conversation"]["title"] == "确认当前时区"
                && m["conversation"]["title_source"] == "native"),
            "原生标题应推送：{pushes:?}"
        );
        // 同一标题再读到一次：不重复推送。
        b.core.handle(Cmd::TitleRead { conversation_id: 1, title: "确认当前时区".into() });
        assert!(drain(&rx).iter().all(|m| m["type"] != "conversation.upsert"));
    }

    /// Kimi 的标题来自每个 Hook 的 session_title（`hook`），不读文件；也不覆盖已有的原生标题。
    #[test]
    fn hook_title_is_kept_and_never_overrides_native() {
        let mut b = bench_with_titles("title-hook", |_| panic!("Kimi 不应读文件"));
        let (_app, _rx, _) = b.app(&["t1"]);
        b.hook("t1", "kimi", json!({"hook_event_name":"SessionStart","session_id":"session_k","session_title":"修复登录"}));
        b.hook("t1", "kimi", json!({"hook_event_name":"UserPromptSubmit","session_id":"session_k","session_title":"修复登录","prompt":"修复登录","agent_id":"main","turn_id":1}));
        b.hook("t1", "kimi", json!({"hook_event_name":"Stop","session_id":"session_k","session_title":"修复登录","agent_id":"main","turn_id":1}));
        assert_eq!(conv_title(&b, "session_k"), (Some("修复登录".into()), Some("hook".into())));
        b.core.store.set_native_title(1, "原生").unwrap();
        b.hook("t1", "kimi", json!({"hook_event_name":"UserPromptSubmit","session_id":"session_k","session_title":"别的","prompt":"别的","agent_id":"main","turn_id":2}));
        assert_eq!(conv_title(&b, "session_k"), (Some("原生".into()), Some("native".into())));
        b.settle();
    }

    /// 读标题在单独线程：读取很慢（模拟授权弹窗）时，Hook 照常处理、状态照常推送，读完再更新标题。
    #[test]
    fn slow_title_read_does_not_block_hooks() {
        let mut b = bench_with_titles("title-slow", |_| {
            std::thread::sleep(Duration::from_secs(2));
            Some("慢慢读到的标题".to_owned())
        });
        let (_app, rx, _) = b.app(&["t1"]);
        let start = Instant::now();
        claude(&mut b, "t1", "SessionStart", "a");
        b.hook("t1", "claude", json!({"hook_event_name":"UserPromptSubmit","session_id":"a","prompt":"第一问"}));
        claude(&mut b, "t1", "Stop", "a");
        claude(&mut b, "t1", "SessionStart", "b");
        claude(&mut b, "t1", "UserPromptSubmit", "b");
        assert!(start.elapsed() < Duration::from_millis(500), "Hook 处理不应等读标题：{:?}", start.elapsed());
        let pushes = drain(&rx);
        assert!(pushes.iter().any(|m| m["type"] == "terminal.upsert" && m["terminal"]["agent"]["state"] == "running"));
        assert_eq!(conv_title(&b, "a").0.as_deref(), Some("第一问"));
        let cmd = b.next_cmd(|c| matches!(c, Cmd::TitleRead { .. }), "慢读完成后应送回标题");
        b.core.handle(cmd);
        assert_eq!(conv_title(&b, "a"), (Some("慢慢读到的标题".into()), Some("native".into())));
    }

    /// 2026-10-12 取证样本回放（docs/s0-hook-compat.md「Claude Code 2.1.296 后台会话」）：守护进程托管的会话按
    /// 「哪个终端连着它」归属，备用会话不归属，另一个终端里接回的 `--bg` 不再把原终端带偏；转入后台的对话接替上一段。
    #[test]
    fn claude_daemon_sessions_follow_the_attached_terminal() {
        use std::sync::{Arc, Mutex};
        let attached: Arc<Mutex<HashMap<i32, String>>> = Arc::default();
        let a = attached.clone();
        let mut b = bench("claude-bg");
        let (tx, rx) = mpsc::channel();
        b.exits = rx;
        b.core.routes = RouteWorker::with_resolver(tx, move |_, pgid| a.lock().unwrap().get(&pgid).cloned());
        let claude_term = "d7d84e47-289e-4e88-bd30-3f83be3335f8";
        let other_term = "f7f0bf0a-0000-0000-0000-000000000000";
        let (_app, _rx, _) = b.app(&[claude_term, other_term]);
        for (id, pgid) in [(claude_term, 100), (other_term, 200)] {
            b.core.panes.get_mut(id).unwrap().foreground =
                Foreground::Agent { pid: pgid, path: "claude.exe".into(), tool: Tool::Claude };
        }
        let text = fs::read_to_string(samples().join("claude-2.1.296-bg.jsonl")).unwrap();
        for (i, line) in text.lines().enumerate() {
            match i {
                // 按左方向键转入后台：原终端的交互进程停靠到 ee4afd71。
                4 => { attached.lock().unwrap().insert(100, "ee4afd71".into()); }
                // 另一个终端里 `claude attach 11375847`。
                9 => { attached.lock().unwrap().insert(200, "11375847".into()); }
                _ => {}
            }
            let row: Value = serde_json::from_str(line).unwrap();
            let pane = row["env"]["SCYLLAUI_PANE_ID"].as_str().unwrap();
            let frame = json!({"type":"hook","source":"claude","dropped":false,"pane_id":pane,
                "hook_t0_us":1,"payload":row["payload"],"env":row["env"]});
            b.core.handle(Cmd::Hook { frame, recv_us: 2 });
            if row["env"].get("CLAUDE_JOB_DIR").is_some() {
                let cmd = b.next_cmd(|c| matches!(c, Cmd::HookRouted { .. }), "后台会话的 Hook 应送回归属结果");
                b.core.handle(cmd);
            }
        }
        let term_of = |b: &Bench, native: &str| {
            b.count(&format!(
                "SELECT COUNT(*) FROM conversation WHERE native_id LIKE '{native}%' AND terminal_id = '{claude_term}'"
            ))
        };
        let p = &b.core.panes[claude_term];
        assert_eq!(p.state.session_id.as_deref().map(|s| &s[..8]), Some("ee4afd71"), "原终端跟着转入后台的对话");
        assert_eq!(p.state.state, AgentState::Ready);
        assert_eq!(b.count("SELECT COUNT(*) FROM conversation WHERE native_id LIKE '05da4a74%'"), 0, "备用会话不进库");
        assert_eq!(term_of(&b, "11375847"), 0, "另一个终端接回的会话不归原终端");
        assert_eq!(
            b.count(&format!(
                "SELECT COUNT(*) FROM conversation WHERE native_id LIKE '11375847%' AND terminal_id = '{other_term}'"
            )),
            1
        );
        assert_eq!(b.core.panes[other_term].state.state, AgentState::Ready);
        // 转入后台的对话接替上一段：上一段 continued（不进「最近任务」），新对话 resumed_from 指向它并沿用标题。
        assert_eq!(
            b.count("SELECT COUNT(*) FROM conversation WHERE native_id LIKE '806715e8%' AND end_reason = 'continued'"),
            1
        );
        let (recent, _) = b.core.store.recent(None, None, None, 50).unwrap();
        assert!(
            recent.iter().all(|c| c["native_id"].as_str().is_some_and(|n| !n.starts_with("806715e8"))),
            "被接替的对话不进「最近任务」：{recent:?}"
        );
        assert_eq!(b.core.store.recent_total(now_ms()).unwrap(), 0);
        assert_eq!(
            b.count("SELECT COUNT(*) FROM conversation f JOIN conversation o ON f.resumed_from = o.id \
                     WHERE f.native_id LIKE 'ee4afd71%' AND o.native_id LIKE '806715e8%'"),
            1
        );
        let full = b.core.store.title_of(&p.state.session_id.clone().unwrap());
        assert_eq!(full, (Some("你是什么时区？".into()), Some("prompt".into())), "标题沿用上一段，第二问不改它");
    }

    /// Codex 共享守护进程取证样本回放（docs/s0-hook-compat.md「Codex 0.162.1 共享守护进程」）：Hook 都带终端 A 的 ID，
    /// 按「已绑定 → 当前目录唯一的 Codex 终端 → 用户正在看的终端」归属，确定不了就不归属。
    #[test]
    fn codex_daemon_hooks_are_attributed_only_when_certain() {
        let mut b = bench("codex-daemon");
        let (app, rx, _) = b.app(&["term-a", "term-b", "term-c"]);
        for id in ["term-a", "term-b", "term-c"] {
            b.core.panes.get_mut(id).unwrap().current_dir = Some("~".into());
        }
        let codex = |b: &mut Bench, id: &str, pgid: i32| {
            b.core.panes.get_mut(id).unwrap().foreground =
                Foreground::Agent { pid: pgid, path: "codex".into(), tool: Tool::Codex };
        };
        let text = fs::read_to_string(samples().join("codex-0.162.1-daemon.jsonl")).unwrap();
        let rows: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(rows.len(), 10);
        let send = |b: &mut Bench, row: &Value| {
            // 样本录于 hosted 字段加入之前；守护进程执行的 Hook 由 scyllaui 标为 hosted。
            let frame = json!({"type":"hook","source":"codex","dropped":false,
                "pane_id": "45530dd7-5713-492e-b10d-c967edb26ea6", "hook_t0_us":1,
                "payload": row["payload"], "env": row["env"], "hosted": true});
            b.core.handle(Cmd::Hook { frame, recv_us: 2 });
            let cmd = b.next_cmd(|c| matches!(c, Cmd::HookRouted { .. }), "应送回归属结果");
            b.core.handle(cmd);
        };
        let term_of = |b: &Bench, native: &str| {
            b.core.panes.iter().find(|(_, p)| p.state.session_id.as_deref().is_some_and(|s| s.starts_with(native)))
                .map(|(id, _)| id.clone())
        };
        // E：只有终端 A 在跑 Codex。
        codex(&mut b, "term-a", 100);
        for row in &rows[0..4] { send(&mut b, row); }
        assert_eq!(term_of(&b, "01a12bf0").as_deref(), Some("term-a"));
        // F1：A 仍在跑，B 也开了 Codex，同一目录。焦点不在候选上：新对话不归属。
        codex(&mut b, "term-b", 200);
        b.core.focused = None;
        send(&mut b, &rows[4]);
        assert_eq!(term_of(&b, "01a12bf4"), None, "两个候选、焦点不明：不归属");
        assert_eq!(b.core.panes["term-a"].state.session_id.as_deref().map(|s| &s[..8]), Some("01a12bf0"), "A 不被带偏");
        // 用户在 B 里按回车：焦点在 B。
        b.core.focused = Some("term-b".into());
        for row in &rows[5..7] { send(&mut b, row); }
        assert_eq!(term_of(&b, "01a12bf4").as_deref(), Some("term-b"));
        // F2：两个 Codex 都退出、关闭 A，C 新开 Codex；守护进程仍带 A 的 ID。
        let (r, _) = b.req(app, &rx, json!({"type":"terminal.exit","terminal_id":"term-a"}));
        assert_eq!(r["ok"], true);
        b.core.panes.get_mut("term-b").unwrap().foreground = Foreground::Shell { pid: 200, path: "/bin/zsh".into() };
        codex(&mut b, "term-c", 300);
        b.core.focused = None;
        for row in &rows[7..10] { send(&mut b, row); }
        assert_eq!(term_of(&b, "01a12bf8").as_deref(), Some("term-c"), "唯一候选，焦点不明也归它");
        assert_eq!(b.core.panes["term-c"].state.state, AgentState::Ready);
    }

    fn claude(b: &mut Bench, terminal: &str, event: &str, session: &str) {
        b.hook(
            terminal,
            "claude",
            json!({"hook_event_name": event, "session_id": session}),
        );
    }

    fn unseen_of(pushes: &[Value], terminal: &str) -> Option<bool> {
        pushes
            .iter()
            .filter(|m| m["type"] == "terminal.upsert" && m["terminal"]["terminal_id"] == terminal)
            .last()
            .map(|m| m["terminal"]["unseen"].as_bool().unwrap())
    }

    #[test]
    fn unseen_marks_turns_finished_while_not_looking() {
        let mut b = bench("unseen");
        let (app, rx, snap) = b.app(&["t1", "t2"]);
        assert_eq!(snap["terminals"][0]["unseen"], false);
        let (r, _) = b.req(app, &rx, json!({"type":"terminal.focus","terminal_id":"t1"}));
        assert_eq!(r["ok"], true);
        // 正在看的终端：一轮结束不置位。
        for e in ["SessionStart", "UserPromptSubmit", "Stop"] {
            claude(&mut b, "t1", e, "a");
        }
        assert_eq!(unseen_of(&drain(&rx), "t1"), Some(false));
        // 没在看的终端：Stop 使 running → ready，置位并推送。
        for e in ["SessionStart", "UserPromptSubmit"] {
            claude(&mut b, "t2", e, "b");
        }
        drain(&rx);
        claude(&mut b, "t2", "Stop", "b");
        assert_eq!(unseen_of(&drain(&rx), "t2"), Some(true));
        // 已是 ready 时再来 Stop：不变，不推送。
        claude(&mut b, "t2", "Stop", "b");
        assert_eq!(unseen_of(&drain(&rx), "t2"), None);
        // 看它即清除并推送；再看一次不推送。
        let (_, pushes) = b.req(app, &rx, json!({"type":"terminal.focus","terminal_id":"t2"}));
        assert_eq!(unseen_of(&pushes, "t2"), Some(false));
        let (_, pushes) = b.req(app, &rx, json!({"type":"terminal.focus","terminal_id":"t2"}));
        assert!(pushes.is_empty());
        // 切到别的 App（focus 为 null）期间结束的轮次也置位；提交提示词即清除。
        let (r, _) = b.req(app, &rx, json!({"type":"terminal.focus","terminal_id":null}));
        assert_eq!(r["ok"], true);
        claude(&mut b, "t2", "UserPromptSubmit", "b");
        claude(&mut b, "t2", "Stop", "b");
        assert_eq!(unseen_of(&drain(&rx), "t2"), Some(true));
        claude(&mut b, "t2", "UserPromptSubmit", "b");
        assert_eq!(unseen_of(&drain(&rx), "t2"), Some(false));
        // 未知终端、错误类型与非 app 角色。
        let (r, _) = b.req(app, &rx, json!({"type":"terminal.focus","terminal_id":"nope"}));
        assert_eq!(r["error"]["code"], "not_found");
        let (r, _) = b.req(app, &rx, json!({"type":"terminal.focus","terminal_id":3}));
        assert_eq!(r["error"]["code"], "bad_request");
        let (cli, cli_rx, _) = b.connect("cli", PROTOCOL_VERSION);
        let (r, _) = b.req(cli, &cli_rx, json!({"type":"terminal.focus","terminal_id":"t1"}));
        assert_eq!(r["error"]["code"], "bad_request");
        // App 断开后 focus 重置：重连后同一终端 ID 上结束的轮次会置位。
        b.req(app, &rx, json!({"type":"terminal.focus","terminal_id":"t1"}));
        b.core.handle(Cmd::Closed { conn: app });
        let (_, rx, _) = b.app(&["t1"]);
        for e in ["SessionStart", "UserPromptSubmit", "Stop"] {
            claude(&mut b, "t1", e, "c");
        }
        assert_eq!(unseen_of(&drain(&rx), "t1"), Some(true));
    }

    /// 「需确认」通知的发与撤（完成通知的 ID 含 `:done:`，另由 `done_notices` 统计）。
    fn notices(pushes: &[Value]) -> (Vec<String>, Vec<String>) {
        let (posts, withdrawn) = all_notices(pushes);
        (posts.into_iter().filter(|i| !i.contains(":done:")).collect(), withdrawn.into_iter().filter(|i| !i.contains(":done:")).collect())
    }

    fn done_notices(pushes: &[Value]) -> (Vec<String>, Vec<String>) {
        let (posts, withdrawn) = all_notices(pushes);
        (posts.into_iter().filter(|i| i.contains(":done:")).collect(), withdrawn.into_iter().filter(|i| i.contains(":done:")).collect())
    }

    fn all_notices(pushes: &[Value]) -> (Vec<String>, Vec<String>) {
        let posts = pushes.iter().filter(|m| m["type"] == "notify.post").map(|m| m["id"].as_str().unwrap().to_owned());
        let withdrawn = pushes
            .iter()
            .filter(|m| m["type"] == "notify.withdraw")
            .flat_map(|m| m["ids"].as_array().unwrap().iter().map(|i| i.as_str().unwrap().to_owned()).collect::<Vec<_>>());
        (posts.collect(), withdrawn.collect())
    }

    /// 完成通知（M4a）：没在看的终端一轮结束发一条（带状态），切过去、再次提问或终端关闭即撤回；正在看的终端不发；
    /// 「需确认」通知带 `kind: waiting`。
    #[test]
    fn done_notice_when_turn_ends_unseen() {
        let mut b = bench("done");
        let (app, rx, _) = b.app(&["t1", "t2"]);
        b.req(app, &rx, json!({"type":"terminal.focus","terminal_id":"t1"}));
        for e in ["SessionStart", "UserPromptSubmit"] {
            claude(&mut b, "t2", e, "a");
        }
        drain(&rx);
        claude(&mut b, "t2", "PermissionRequest", "a");
        let pushes = drain(&rx);
        let waiting = pushes.iter().find(|m| m["type"] == "notify.post").unwrap();
        assert_eq!(waiting["kind"], "waiting");
        claude(&mut b, "t2", "PostToolUse", "a");
        drain(&rx);
        claude(&mut b, "t2", "Stop", "a");
        let pushes = drain(&rx);
        assert_eq!(done_notices(&pushes), (vec!["t2:done:1".to_owned()], vec![]));
        let post = pushes.iter().find(|m| m["type"] == "notify.post").unwrap();
        assert_eq!((post["kind"].as_str(), post["state"].as_str(), post["tool"].as_str()), (Some("done"), Some("ready"), Some("claude")));
        // 再次处理命令不重复发。
        b.req(app, &rx, json!({"type":"terminal.focus","terminal_id":"t1"}));
        assert_eq!(done_notices(&drain(&rx)), (vec![], vec![]));
        // 切过去即撤回；正在看时结束的轮次不发。
        let (_, pushes) = b.req(app, &rx, json!({"type":"terminal.focus","terminal_id":"t2"}));
        assert_eq!(done_notices(&pushes), (vec![], vec!["t2:done:1".to_owned()]));
        for e in ["UserPromptSubmit", "Stop"] {
            claude(&mut b, "t2", e, "a");
        }
        assert_eq!(done_notices(&drain(&rx)), (vec![], vec![]));
        // 切走后出错结束：序号加一，状态 error；在那个终端里再次提问（清除待查看）即撤回。
        b.req(app, &rx, json!({"type":"terminal.focus","terminal_id":"t1"}));
        claude(&mut b, "t2", "UserPromptSubmit", "a");
        drain(&rx);
        b.hook("t2", "claude", json!({"hook_event_name":"StopFailure","session_id":"a"}));
        let pushes = drain(&rx);
        assert_eq!(done_notices(&pushes), (vec!["t2:done:2".to_owned()], vec![]));
        assert_eq!(pushes.iter().find(|m| m["type"] == "notify.post").unwrap()["state"], "error");
        claude(&mut b, "t2", "UserPromptSubmit", "a");
        assert_eq!(done_notices(&drain(&rx)), (vec![], vec!["t2:done:2".to_owned()]));
        // 终端关闭即撤回。
        claude(&mut b, "t2", "Stop", "a");
        assert_eq!(done_notices(&drain(&rx)).0, vec!["t2:done:3".to_owned()]);
        let (_, pushes) = b.req(app, &rx, json!({"type":"terminal.exit","terminal_id":"t2"}));
        assert_eq!(done_notices(&pushes).1, vec!["t2:done:3".to_owned()]);
    }

    /// 缺陷 E（docs/design.md 4.3 节「子代理事件」例外）：OpenCode 主会话这一轮结束后，两个子会话先后要审批，同属一次等待，
    /// 只发一条通知，全部处理完才撤回、回到 Ready。子会话的审批记在主会话的对话上：不建对话、不换绑终端。
    #[test]
    fn subagent_approvals_share_one_notice_and_keep_binding() {
        let mut b = bench("sub-wait");
        let (_app, rx, _) = b.app(&["t1", "t2"]);
        let oc = |b: &mut Bench, v: Value| b.hook("t1", "opencode", v);
        oc(&mut b, json!({"hook_event_name":"session.prompt","sessionID":"ses_p","prompt":"派两个子代理"}));
        oc(&mut b, json!({"hook_event_name":"session.execution.succeeded","sessionID":"ses_p"}));
        drain(&rx);
        let child = |e: &str, c: &str| json!({"hook_event_name": e, "sessionID": c, "scyllaui_subagent": true});
        oc(&mut b, child("permission.asked", "ses_c1"));
        oc(&mut b, child("permission.asked", "ses_c2"));
        oc(&mut b, child("permission.replied", "ses_c1"));
        assert_eq!(notices(&drain(&rx)), (vec!["t1:1".to_owned()], vec![]), "一次等待一条");
        assert_eq!(b.core.panes["t1"].state.state, AgentState::Waiting);
        oc(&mut b, child("permission.replied", "ses_c2"));
        assert_eq!(notices(&drain(&rx)), (vec![], vec!["t1:1".to_owned()]));
        assert_eq!(b.core.panes["t1"].state.state, AgentState::Ready);
        assert_eq!(b.core.panes["t1"].state.session_id.as_deref(), Some("ses_p"));
        assert_eq!(b.count("SELECT COUNT(*) FROM conversation"), 1, "子会话不建对话");
        let parent = b.count("SELECT id FROM conversation WHERE native_id = 'ses_p'");
        assert_eq!(b.core.panes["t1"].conversation, Some(parent));
        assert_eq!(b.count(&format!("SELECT COUNT(*) FROM event WHERE conversation_id = {parent} AND kind LIKE 'permission.%'")), 4);
        assert_eq!(b.count("SELECT COUNT(*) FROM conversation WHERE last_prompt = '派两个子代理'"), 1);
    }

    /// 通知（M2 第 1 块）：同一次等待只发一条（Claude 的 PermissionRequest 与 permission_prompt 算一次），等待结束、
    /// 用户切过来、终端关闭时撤回；开始时正在看的终端不发，之后切走也不补发。
    #[test]
    fn approval_notice_once_per_wait() {
        let mut b = bench("notify");
        let (app, rx, _) = b.app(&["t1", "t2"]);
        b.req(app, &rx, json!({"type":"terminal.rename","terminal_id":"t1","name":"修登录"}));
        for e in ["SessionStart", "UserPromptSubmit"] {
            claude(&mut b, "t1", e, "a");
        }
        drain(&rx);
        claude(&mut b, "t1", "PermissionRequest", "a");
        let pushes = drain(&rx);
        assert_eq!(notices(&pushes), (vec!["t1:1".to_owned()], vec![]));
        let post = pushes.iter().find(|m| m["type"] == "notify.post").unwrap();
        assert_eq!(post["terminal_id"], "t1");
        assert_eq!(post["tool"], "claude");
        assert_eq!(post["project"], "p");
        assert_eq!(post["title"], "修登录");
        assert_eq!(post["trace"]["hook_t0_us"], 1, "附带触发它的 Hook 打点");
        b.hook("t1", "claude", json!({"hook_event_name":"Notification","session_id":"a","notification_type":"permission_prompt"}));
        assert_eq!(notices(&drain(&rx)), (vec![], vec![]), "同一次等待不重复");
        claude(&mut b, "t1", "PostToolUse", "a");
        assert_eq!(notices(&drain(&rx)), (vec![], vec!["t1:1".to_owned()]), "批准后撤回");
        // 第二次等待：序号加一；用户切过来即撤回，之后等待结束不再撤回。
        claude(&mut b, "t1", "PermissionRequest", "a");
        assert_eq!(notices(&drain(&rx)).0, vec!["t1:2".to_owned()]);
        let (_, pushes) = b.req(app, &rx, json!({"type":"terminal.focus","terminal_id":"t1"}));
        assert_eq!(notices(&pushes), (vec![], vec!["t1:2".to_owned()]));
        claude(&mut b, "t1", "Stop", "a");
        assert_eq!(notices(&drain(&rx)), (vec![], vec![]));
        // 正在看的终端进入等待：不发；切走也不补发；等待结束不撤回。
        claude(&mut b, "t1", "PermissionRequest", "a");
        assert_eq!(notices(&drain(&rx)), (vec![], vec![]));
        b.req(app, &rx, json!({"type":"terminal.focus","terminal_id":null}));
        claude(&mut b, "t1", "PostToolUse", "a");
        assert_eq!(notices(&drain(&rx)), (vec![], vec![]));
        // 没在看的终端等待中被关闭：撤回。
        for e in ["SessionStart", "UserPromptSubmit", "PermissionRequest"] {
            claude(&mut b, "t2", e, "b");
        }
        assert_eq!(notices(&drain(&rx)).0, vec!["t2:1".to_owned()]);
        let (_, pushes) = b.req(app, &rx, json!({"type":"terminal.exit","terminal_id":"t2"}));
        assert_eq!(notices(&pushes), (vec![], vec!["t2:1".to_owned()]));
    }

    /// Claude 拒绝审批（M2 第 1 块）：WaitWatch 报告拒绝后按中断处理——回到 Ready、不标待查看、撤回通知、写事件；
    /// 已不在等待或会话不同的报告忽略。
    #[test]
    fn rejection_from_transcript_ends_the_wait() {
        let mut b = bench("rejected");
        let (_app, rx, _) = b.app(&["t1"]);
        for e in ["SessionStart", "UserPromptSubmit", "PermissionRequest"] {
            claude(&mut b, "t1", e, "a");
        }
        drain(&rx);
        b.core.handle(Cmd::SessionRecord { terminal_id: "t1".into(), session_id: Some("other".into()), kind: Kind::TurnInterrupt, turn: None, agent: None });
        assert!(drain(&rx).is_empty(), "会话不同：忽略");
        b.core.handle(Cmd::SessionRecord { terminal_id: "t1".into(), session_id: Some("a".into()), kind: Kind::TurnInterrupt, turn: None, agent: None });
        let pushes = drain(&rx);
        let t = pushes.iter().rev().find(|m| m["type"] == "terminal.upsert").expect("推送终端");
        assert_eq!(t["terminal"]["agent"]["state"], "ready");
        assert_eq!(t["terminal"]["unseen"], false);
        assert_eq!(t["trace"]["event"], "PermissionRejected");
        assert_eq!(notices(&pushes), (vec![], vec!["t1:1".to_owned()]));
        assert_eq!(b.count("SELECT count(*) FROM event WHERE kind = 'PermissionRejected'"), 1);
        b.core.handle(Cmd::SessionRecord { terminal_id: "t1".into(), session_id: Some("a".into()), kind: Kind::TurnInterrupt, turn: None, agent: None });
        assert!(drain(&rx).is_empty(), "已不在等待：忽略");
    }

    /// Copilot（M4b 块 9c）：批准、拒绝与 Esc 中断都不发 Hook，从会话记录补——待审批时批准回到运行中、拒绝回到 Ready；
    /// 运行中认出中断回到 Ready。Claude 运行中的同类报告照旧忽略（只在待审批时看）。
    /// 快照的 `tools`：各款读得到哪些信息，App 据此对读不到的写明「暂不支持」（docs/ui-methodology.md 1.7 节）。
    #[test]
    fn snapshot_tools_capabilities() {
        let tools = tools_json();
        let caps = |name: &str| {
            let t = tools.as_array().unwrap().iter().find(|t| t["tool"] == name).unwrap();
            (t["reply"].as_bool().unwrap(), t["prompts"].as_bool().unwrap(), t["usage"].as_bool().unwrap(), t["model"].as_bool().unwrap())
        };
        assert_eq!(tools.as_array().unwrap().len(), 7);
        assert_eq!(caps("claude"), (true, true, true, true));
        assert_eq!(caps("antigravity"), (true, true, true, true), "Antigravity 读会话记录，模型来自 Hook");
        assert_eq!(caps("copilot"), (true, true, true, true), "Copilot 2026-10-31 起读回复、用量与模型（events.jsonl）");
        assert_eq!(caps("opencode"), (true, true, true, true), "OpenCode 2026-10-31 起读回复与用量（它的库），模型来自插件事件");
    }

    /// Antigravity（M4b 块 9b）：等待审批不发 Hook，由对话库报告需确认；批准、拒绝同 Copilot；Stop Hook 丢了时记录里的
    /// 一轮结束也回到 Ready，Stop Hook 先到时记录的报告不再生效；子代理（`lastUserInput` 为空）的事件不改状态。
    #[test]
    fn antigravity_session_record_signals() {
        let mut b = bench("agy-record");
        let (_app, rx, _) = b.app(&["t1"]);
        let id = "eb669d56-2056-4647-9e87-b3cc31354479";
        let agy = |b: &mut Bench, ev: &str, conv: &str, extra: Value| {
            let mut p = json!({"hook_event_name": ev, "conversationId": conv, "lastUserInput": "跑测试", "workspacePaths": ["/w"]});
            p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            b.hook("t1", "antigravity", p);
        };
        let state = |b: &Bench| b.core.panes["t1"].state.state;
        agy(&mut b, "SessionStart", id, json!({}));
        agy(&mut b, "PreInvocation", id, json!({"invocationNum": 0}));
        assert_eq!(state(&b), AgentState::Running);
        assert!(b.core.watching.contains_key("t1"), "运行中就监听对话库");
        drain(&rx);
        let rec = |b: &mut Bench, kind: Kind| b.core.handle(Cmd::SessionRecord { terminal_id: "t1".into(), session_id: Some(id.into()), kind, turn: None, agent: None });
        rec(&mut b, Kind::PermissionDone);
        assert_eq!(state(&b), AgentState::Running, "开始监听时报告的「不在等」不改状态");
        rec(&mut b, Kind::PermissionWait);
        assert_eq!(state(&b), AgentState::Waiting, "对话库里出现等待用户的步骤");
        rec(&mut b, Kind::PermissionDone);
        assert_eq!(state(&b), AgentState::Running, "批准");
        rec(&mut b, Kind::TurnEnd);
        assert_eq!(state(&b), AgentState::Ready, "Stop Hook 丢了也回到 Ready");
        assert!(!b.core.watching.contains_key("t1"));
        agy(&mut b, "PreInvocation", id, json!({"invocationNum": 0}));
        rec(&mut b, Kind::PermissionWait);
        rec(&mut b, Kind::TurnInterrupt);
        assert_eq!(state(&b), AgentState::Ready, "拒绝审批");
        agy(&mut b, "PreInvocation", id, json!({"invocationNum": 0}));
        agy(&mut b, "PreInvocation", "31195811-4617-4220-8182-921e93302120", json!({"invocationNum": 0, "lastUserInput": ""}));
        assert_eq!(b.core.panes["t1"].state.session_id.as_deref(), Some(id), "子代理不换绑");
        agy(&mut b, "Stop", "31195811-4617-4220-8182-921e93302120", json!({"error": "", "lastUserInput": ""}));
        assert_eq!(state(&b), AgentState::Running, "子代理结束不改状态");
        agy(&mut b, "Stop", id, json!({"error": "", "fullyIdle": false}));
        assert_eq!(state(&b), AgentState::Ready);
        rec(&mut b, Kind::TurnEnd);
        assert_eq!(b.count("SELECT count(*) FROM event WHERE kind = 'PermissionAsked'"), 2);
        assert_eq!(b.count("SELECT count(*) FROM event WHERE kind = 'PermissionApproved'"), 1);
        assert_eq!(b.count("SELECT count(*) FROM event WHERE kind = 'PermissionRejected'"), 1);
        assert_eq!(b.count("SELECT count(*) FROM event WHERE kind = 'ExecutionDone'"), 1, "Stop Hook 已到，记录的一轮结束不再生效");
    }

    #[test]
    fn copilot_session_record_signals() {
        let mut b = bench("copilot-record");
        let (_app, rx, _) = b.app(&["t1", "t2"]);
        let cop = |b: &mut Bench, ev: &str, extra: Value| {
            let mut p = json!({"hook_event_name": ev, "sessionId": "c1", "cwd": "/w"});
            p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            b.hook("t1", "copilot", p);
        };
        cop(&mut b, "sessionStart", json!({}));
        cop(&mut b, "userPromptSubmitted", json!({"prompt": "跑测试"}));
        cop(&mut b, "notification", json!({"notification_type": "permission_prompt"}));
        assert_eq!(b.core.panes["t1"].state.state, AgentState::Waiting);
        assert!(b.core.watching.contains_key("t1"), "待审批时监听");
        drain(&rx);
        let rec = |b: &mut Bench, kind: Kind| b.core.handle(Cmd::SessionRecord { terminal_id: "t1".into(), session_id: Some("c1".into()), kind, turn: None, agent: None });
        rec(&mut b, Kind::PermissionDone);
        assert_eq!(b.core.panes["t1"].state.state, AgentState::Running, "批准回到运行中");
        assert!(b.core.watching.contains_key("t1"), "运行中继续监听");
        rec(&mut b, Kind::TurnInterrupt);
        assert_eq!(b.core.panes["t1"].state.state, AgentState::Ready, "运行中按 Esc 中断");
        assert!(!b.core.watching.contains_key("t1"), "回到 Ready 停止监听");
        assert_eq!(b.count("SELECT count(*) FROM event WHERE kind = 'PermissionApproved'"), 1);
        assert_eq!(b.count("SELECT count(*) FROM event WHERE kind = 'SessionAborted'"), 1);
        cop(&mut b, "userPromptSubmitted", json!({"prompt": "再跑"}));
        cop(&mut b, "notification", json!({"notification_type": "permission_prompt"}));
        rec(&mut b, Kind::TurnInterrupt);
        assert_eq!(b.core.panes["t1"].state.state, AgentState::Ready, "审批框里按 Esc");
        // Claude 只在待审批时看：运行中的报告忽略。
        claude(&mut b, "t2", "SessionStart", "a");
        claude(&mut b, "t2", "UserPromptSubmit", "a");
        drain(&rx);
        b.core.handle(Cmd::SessionRecord { terminal_id: "t2".into(), session_id: Some("a".into()), kind: Kind::TurnInterrupt, turn: None, agent: None });
        assert_eq!(b.core.panes["t2"].state.state, AgentState::Running);
        assert!(!b.core.watching.contains_key("t2"));
    }

    /// 第二轮长跑缺陷 E 的三处补齐（2026-11-14 取样，docs/design.md 4.3 节）：Antigravity 子代理的等待只写在它自己的
    /// 对话库里；Codex 拒绝子代理的审批不发 Hook；Grok 后台子代理的审批通知分不出主、子代理，到的时候主会话已结束。
    #[test]
    fn subagent_approvals_from_child_records() {
        let mut b = bench("child-records");
        let (_app, _rx, _) = b.app(&["t1", "t2", "t3"]);
        let state = |b: &Bench, t: &str| b.core.panes[t].state.state;
        // Antigravity：子代理的 Hook 带来它的对话 ID，另开监听；它的库里出现等待 → 需确认，消失 → 回到等待之前（主会话已结束）。
        let (main, child) = ("eb669d56-2056-4647-9e87-b3cc31354479", "5644b7cc-2056-4647-9e87-b3cc31354479");
        let agy = |b: &mut Bench, ev: &str, conv: &str, input: &str, extra: Value| {
            let mut p = json!({"hook_event_name": ev, "conversationId": conv, "lastUserInput": input, "workspacePaths": ["/w"]});
            p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            b.hook("t1", "antigravity", p);
        };
        agy(&mut b, "SessionStart", main, "跑", json!({}));
        agy(&mut b, "PreInvocation", main, "跑", json!({"invocationNum": 0}));
        agy(&mut b, "SessionStart", child, "", json!({}));
        let key = format!("t1#child:{child}");
        assert!(b.core.watching.contains_key(&key), "子代理的对话库另开监听");
        agy(&mut b, "Stop", main, "跑", json!({"fullyIdle": false}));
        assert_eq!(state(&b, "t1"), AgentState::Ready);
        assert!(b.core.watching.contains_key(&key), "主会话这一轮结束了也继续监听子代理");
        let rec = |b: &mut Bench, kind: Kind| {
            b.core.handle(Cmd::SessionRecord { terminal_id: "t1".into(), session_id: Some(main.into()), kind, turn: None, agent: Some(child.into()) })
        };
        rec(&mut b, Kind::PermissionDone);
        assert_eq!(state(&b, "t1"), AgentState::Ready, "开始监听时报告的「不在等」不改状态");
        rec(&mut b, Kind::PermissionWait);
        assert_eq!(state(&b, "t1"), AgentState::Waiting, "子代理在等审批");
        rec(&mut b, Kind::PermissionDone);
        assert_eq!(state(&b, "t1"), AgentState::Ready, "批准后回到等待之前的状态");
        // 实机的顺序（2026-11-14）：子代理先开始等，主会话这一轮随后才结束——不清掉还在等的审批。
        agy(&mut b, "PreInvocation", main, "再跑", json!({"invocationNum": 0}));
        rec(&mut b, Kind::PermissionWait);
        agy(&mut b, "Stop", main, "再跑", json!({"fullyIdle": false}));
        assert_eq!(state(&b, "t1"), AgentState::Waiting, "主会话一轮结束，子代理还在等");
        rec(&mut b, Kind::PermissionDone);
        assert_eq!(state(&b, "t1"), AgentState::Ready, "批准后回到主会话此时的状态");
        rec(&mut b, Kind::PermissionWait);
        rec(&mut b, Kind::TurnInterrupt);
        assert_eq!(state(&b, "t1"), AgentState::Ready, "拒绝：子代理这一次执行结束");
        agy(&mut b, "SessionStart", "99999999-2056-4647-9e87-b3cc31354479", "新的", json!({}));
        assert!(!b.core.watching.contains_key(&key), "换了会话不再监听上一段的子代理");

        // Codex：子代理的审批指向子线程的记录；拒绝后记录里的 turn_aborted 按审批已处理，主会话回到运行中继续等。
        let codex = |b: &mut Bench, ev: &str, extra: Value| {
            let mut p = json!({"hook_event_name": ev, "session_id": "c1", "cwd": "/w", "transcript_path": "/r/main.jsonl"});
            p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            b.hook("t2", "codex", p);
        };
        codex(&mut b, "SessionStart", json!({}));
        codex(&mut b, "UserPromptSubmit", json!({"prompt": "跑"}));
        codex(&mut b, "PermissionRequest", json!({"agent_id": "a1", "transcript_path": "/r/child.jsonl", "tool_input": {"command": "sleep 9"}}));
        assert_eq!(state(&b, "t2"), AgentState::Waiting);
        assert_eq!(b.core.approval_records["t2"], ("/r/child.jsonl".to_owned(), true), "监听这次审批指向的记录");
        assert!(b.core.watching.contains_key("t2"));
        b.core.handle(Cmd::SessionRecord { terminal_id: "t2".into(), session_id: Some("c1".into()), kind: Kind::PermissionDone, turn: None, agent: None });
        assert_eq!(state(&b, "t2"), AgentState::Running);
        assert!(!b.core.watching.contains_key("t2") && b.core.approval_records.is_empty(), "离开待审批即停");

        // Grok：后台子代理的审批通知用主会话的 ID、不带代理标识，到的时候主会话已结束；批准后回到 Ready。
        let grok = |b: &mut Bench, ev: &str, extra: Value| {
            let mut p = json!({"hook_event_name": ev, "sessionId": "g1", "cwd": "/w"});
            p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            b.hook("t3", "grok", p);
        };
        grok(&mut b, "SessionStart", json!({}));
        grok(&mut b, "UserPromptSubmit", json!({"prompt": "后台跑"}));
        grok(&mut b, "Stop", json!({"reason": "end_turn"}));
        assert_eq!(state(&b, "t3"), AgentState::Ready);
        grok(&mut b, "Notification", json!({"notificationType": "permission_prompt"}));
        assert_eq!(state(&b, "t3"), AgentState::Waiting);
        b.core.handle(Cmd::SessionRecord { terminal_id: "t3".into(), session_id: Some("g1".into()), kind: Kind::PermissionDone, turn: None, agent: None });
        assert_eq!(state(&b, "t3"), AgentState::Ready, "回到等待之前的状态，不是运行中");
        grok(&mut b, "UserPromptSubmit", json!({"prompt": "前台跑"}));
        grok(&mut b, "Notification", json!({"notificationType": "permission_prompt"}));
        b.core.handle(Cmd::SessionRecord { terminal_id: "t3".into(), session_id: Some("g1".into()), kind: Kind::PermissionDone, turn: None, agent: None });
        assert_eq!(state(&b, "t3"), AgentState::Running, "主代理自己的审批照旧回到运行中");
    }

    /// 换版本接管带 Agent 状态（2026-11-14，docs/design.md 第 7 节「换版本」）：前台还是交接时的那个进程、对话也相同
    /// 才还原，连同子代理审批、轮次、通知序号，还原后不重发通知；对不上的维持未知。
    #[test]
    fn takeover_restores_agent_state_only_for_the_same_process() {
        let mut b = bench("takeover-state");
        let (_app, rx, _) = b.app(&["t1"]);
        let me = std::process::id() as i32;
        let kimi = |b: &mut Bench, ev: &str, extra: Value| {
            let mut p = json!({"hook_event_name": ev, "session_id": "session_k1", "cwd": "/w"});
            p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            b.hook("t1", "kimi", p);
        };
        kimi(&mut b, "SessionStart", json!({}));
        let ask = json!({"hook_event_name": "UserPromptSubmit", "session_id": "session_k1", "cwd": "/w", "prompt": [{"type": "text", "text": "跑测试"}]});
        b.hook_at("t1", "kimi", &ask, 1_794_668_002_000_000);
        kimi(&mut b, "PermissionRequest", json!({"id": "approval_1", "agent_id": "agent-0", "turn_id": 0}));
        assert_eq!(b.core.panes["t1"].state.state, AgentState::Waiting);
        let fg = || Foreground::Agent { pid: me, path: "/x/kimi".into(), tool: Tool::Kimi };
        b.core.panes.get_mut("t1").unwrap().foreground = fg();
        let saved = b.core.agent_handoff("t1").expect("有工具的终端带状态");
        assert_eq!(saved["fg_pid"], me);
        assert_eq!(saved["waiting"], true);
        let posted = drain(&rx).iter().filter(|m| m["type"] == "notify.post").count();
        assert_eq!(posted, 1, "进入需确认时发过一次通知");
        // 新进程：状态从库里恢复为未知，内存里的等待与序号都没有了。
        let reset = |b: &mut Bench| {
            let p = b.core.panes.get_mut("t1").unwrap();
            p.state = PaneState::restored(Tool::Kimi, "session_k1".into(), None);
            b.core.waits.clear();
            b.core.wait_seq.clear();
        };
        reset(&mut b);
        let mut other = saved.clone();
        other["fg_pid"] = json!(me + 1);
        b.core.restore_agent("t1", &other);
        assert_eq!(b.core.panes["t1"].state.state, AgentState::Unknown, "进程号对不上不还原");
        let mut other = saved.clone();
        other["fg_start_ms"] = json!(1);
        b.core.restore_agent("t1", &other);
        assert_eq!(b.core.panes["t1"].state.state, AgentState::Unknown, "启动时间对不上不还原");
        let mut other = saved.clone();
        other["conversation_id"] = json!(999);
        b.core.restore_agent("t1", &other);
        assert_eq!(b.core.panes["t1"].state.state, AgentState::Unknown, "对话对不上不还原");
        b.core.panes.get_mut("t1").unwrap().foreground = Foreground::Unknown;
        b.core.restore_agent("t1", &saved);
        assert_eq!(b.core.panes["t1"].state.state, AgentState::Unknown, "前台不是 AI 不还原");
        b.core.panes.get_mut("t1").unwrap().foreground = fg();
        b.core.restore_agent("t1", &saved);
        assert_eq!(b.core.panes["t1"].state.state, AgentState::Waiting);
        assert_eq!(b.core.panes["t1"].state.turn(), Some("1794668002000"));
        assert!(b.core.panes["t1"].resync, "下一次监听会话记录按对账处理");
        drain(&rx);
        // 还原后不重发通知；子代理的审批处理完回到等待之前的状态。
        kimi(&mut b, "PermissionResult", json!({"id": "approval_1", "agent_id": "agent-0", "decision": "approved"}));
        assert_eq!(b.core.panes["t1"].state.state, AgentState::Running);
        let msgs = drain(&rx);
        assert!(msgs.iter().all(|m| m["type"] != "notify.post"), "还原的等待不重发通知：{msgs:?}");
        assert!(msgs.iter().any(|m| m["type"] == "notify.withdraw"), "等待结束照常撤回");
    }

    /// Kimi 的轮次结束以会话记录为准（2026-11-14，docs/design.md 3.1 节）：监听确认已打开时，Stop、StopFailure、
    /// Interrupt 这几个 Hook 不改状态（分不出主、子代理），等记录里的结束；上一轮的记录不算。监听没打开时照 Hook。
    /// 轮次标识是后台收到提交的时刻（2026-11-15 起不装 TurnStarted）。
    #[test]
    fn kimi_turn_end_comes_from_session_record() {
        let mut b = bench("kimi-record");
        let (_app, rx, _) = b.app(&["t1"]);
        let kimi = |b: &mut Bench, ev: &str, extra: Value| {
            let mut p = json!({"hook_event_name": ev, "session_id": "session_k1", "cwd": "/w"});
            p.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            b.hook("t1", "kimi", p);
        };
        let state = |b: &Bench| b.core.panes["t1"].state.state;
        let rec = |b: &mut Bench, kind: Kind, turn: &str| {
            b.core.handle(Cmd::SessionRecord { terminal_id: "t1".into(), session_id: Some("session_k1".into()), kind, turn: Some(turn.into()), agent: None })
        };
        let opened = |b: &mut Bench, ok: bool| {
            b.core.handle(Cmd::WatchOpened { key: "t1".into(), session_id: Some("session_k1".into()), ok })
        };
        // 用户提问：后台在第 ms 毫秒收到，轮次标识就是它。
        let ask = |b: &mut Bench, ms: u64, text: &str| {
            let p = json!({"hook_event_name": "UserPromptSubmit", "session_id": "session_k1", "cwd": "/w", "is_steer": false,
                           "prompt": [{"type": "text", "text": text}]});
            b.hook_at("t1", "kimi", &p, ms * 1000);
        };
        kimi(&mut b, "SessionStart", json!({}));
        ask(&mut b, 1_794_668_003_000, "跑测试");
        assert_eq!(state(&b), AgentState::Running);
        assert_eq!(b.core.watching["t1"].turn.as_deref(), Some("1794668003000"), "运行中监听，按收到提问的时刻");
        // 监听还没确认打开：照 Hook 处理（2.5.x 及以前有 Stop）。
        kimi(&mut b, "Stop", json!({"stop_hook_active": false}));
        assert_eq!(state(&b), AgentState::Ready);
        ask(&mut b, 1_794_668_004_000, "再跑");
        assert_eq!(b.core.watching["t1"].turn.as_deref(), Some("1794668004000"), "又问了一次，重开监听");
        assert!(!b.core.watch_open.contains("t1"));
        opened(&mut b, true);
        assert!(b.core.watch_open.contains("t1"));
        drain(&rx);
        // 还没修复的旧配置发来的 TurnStarted（含子代理收到任务时的）：认得但不改状态，监听不重开。
        kimi(&mut b, "TurnStarted", json!({"turn_id": 0, "origin_kind": "system_trigger", "origin_name": "subagent", "prompt": "<git-context>"}));
        assert_eq!(b.core.watching["t1"].turn.as_deref(), Some("1794668004000"));
        assert!(b.core.watch_open.contains("t1"));
        assert_eq!(state(&b), AgentState::Running);
        // 监听已打开：子代理的 Stop、StopFailure、Interrupt（载荷与主代理的分不出）不改状态。
        kimi(&mut b, "Stop", json!({"stop_hook_active": false}));
        kimi(&mut b, "StopFailure", json!({"error_type": "x"}));
        kimi(&mut b, "Interrupt", json!({"turn_id": 0, "reason": "cancelled"}));
        assert_eq!(state(&b), AgentState::Running);
        rec(&mut b, Kind::TurnEnd, "1794668003000");
        assert_eq!(state(&b), AgentState::Running, "上一轮监听晚到的结束不算");
        rec(&mut b, Kind::TurnEnd, "1794668004000");
        assert_eq!(state(&b), AgentState::Ready);
        assert!(b.core.watching.contains_key("t1"), "回到 Ready 仍监听（记录里可能另开一轮）");
        assert_eq!(b.count("SELECT count(*) FROM event WHERE kind = 'ExecutionDone'"), 1);
        // 出错、取消。
        ask(&mut b, 1_794_668_005_000, "三");
        opened(&mut b, true);
        rec(&mut b, Kind::TurnFail, "1794668005000");
        assert_eq!(state(&b), AgentState::Error);
        ask(&mut b, 1_794_668_006_000, "四");
        opened(&mut b, true);
        rec(&mut b, Kind::TurnInterrupt, "1794668006000");
        assert_eq!(state(&b), AgentState::Ready);
        // 记录打不开：按 Hook。
        ask(&mut b, 1_794_668_007_000, "五");
        opened(&mut b, false);
        kimi(&mut b, "Interrupt", json!({"turn_id": 7, "reason": "cancelled"}));
        assert_eq!(state(&b), AgentState::Ready);
        // 提示词取自内容块（侧栏副标题、历史提示词用）。
        assert_eq!(b.count("SELECT count(*) FROM conversation WHERE native_id = 'session_k1' AND last_prompt = '五'"), 1);
        // 一轮刚结束时的插话另开一轮、不发提交 Hook（2026-11-15 实测）：记录里新开的一轮让 Ready 回到运行中，
        // 以此刻作新的轮次标识，监听按它重开；这一轮的结束照常从记录认出。
        assert!(b.core.watching.contains_key("t1"), "Ready 时也在监听");
        opened(&mut b, true);
        rec(&mut b, Kind::PromptSubmit, "1794668007000");
        assert_eq!(state(&b), AgentState::Running);
        let turn = b.core.panes["t1"].state.turn().unwrap().to_owned();
        assert_ne!(turn, "1794668007000");
        assert_eq!(b.core.watching["t1"].turn.as_deref(), Some(turn.as_str()));
        rec(&mut b, Kind::PromptSubmit, &turn);
        assert_eq!(state(&b), AgentState::Running, "运行中再读到开始不重复处理");
        opened(&mut b, true);
        rec(&mut b, Kind::TurnEnd, &turn);
        assert_eq!(state(&b), AgentState::Ready);
        assert_eq!(b.count("SELECT count(*) FROM event WHERE kind = 'ExecutionStarted'"), 1);
    }

    /// 用量（M2 第 2 块）：Hook 带来的模型与会话记录读到的模型都推送 `usage.upsert`，相同不重复推送；快照带未结束对话的用量；
    /// 对话结束后移除，结束后才读到的不再记。
    #[test]
    fn model_from_hook_and_log() {
        let mut b = bench("usage");
        let (_app, rx, _) = b.app(&["t1"]);
        let codex = |b: &mut Bench, event: &str, model: &str| {
            b.hook("t1", "codex", json!({"hook_event_name": event, "session_id": "x", "model": model, "transcript_path": "/t/x"}))
        };
        codex(&mut b, "SessionStart", "gpt-6.1-sol");
        let ups: Vec<Value> = drain(&rx).into_iter().filter(|m| m["type"] == "usage.upsert").collect();
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0]["usage"], json!({"model": "gpt-6.1-sol", "model_source": "hook"}));
        let cid = ups[0]["conversation_id"].as_i64().unwrap();
        codex(&mut b, "UserPromptSubmit", "gpt-6.1-sol");
        assert!(drain(&rx).iter().all(|m| m["type"] != "usage.upsert"), "相同不推送");
        b.core.handle(Cmd::ModelRead { conversation_id: cid, model: "gpt-6.2".into() });
        let ups: Vec<Value> = drain(&rx).into_iter().filter(|m| m["type"] == "usage.upsert").collect();
        assert_eq!(ups[0]["usage"]["model_source"], "log");
        // 推理强度（维护期 0.3.5）：读到即带上，相同不重复推送。
        b.core.handle(Cmd::EffortRead { conversation_id: cid, effort: "xhigh".into() });
        let ups: Vec<Value> = drain(&rx).into_iter().filter(|m| m["type"] == "usage.upsert").collect();
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0]["usage"], json!({"model": "gpt-6.2", "model_source": "log", "effort": "xhigh"}));
        b.core.handle(Cmd::EffortRead { conversation_id: cid, effort: "xhigh".into() });
        assert!(drain(&rx).iter().all(|m| m["type"] != "usage.upsert"), "相同不推送");
        let (snap, _) = b.req(_app, &rx, json!({"type": "snapshot.get"}));
        assert_eq!(snap["snapshot"]["usage"], json!([{"conversation_id": cid, "usage": {"model": "gpt-6.2", "model_source": "log", "effort": "xhigh"}}]));
        codex(&mut b, "SessionEnd", "gpt-6.1-sol");
        assert!(drain(&rx).iter().all(|m| m["type"] != "usage.upsert"), "结束事件里的模型不记");
        b.core.handle(Cmd::ModelRead { conversation_id: cid, model: "gpt-7".into() });
        b.core.handle(Cmd::EffortRead { conversation_id: cid, effort: "low".into() });
        assert!(drain(&rx).is_empty(), "结束后读到的不记");
        let (snap, _) = b.req(_app, &rx, json!({"type": "snapshot.get"}));
        assert_eq!(snap["snapshot"]["usage"], json!([]));
    }

    /// 搜索（M2 第 4 块）：五类字段（终端名、任务标题、提示词、最后回复、目录与项目名）各一例命中，不区分大小写；
    /// 14 天以前的已结束对话也能搜到；被接替的、从没提问的不算；`%` `_` 按字面匹配；空查询与过长为 bad_request。
    #[test]
    fn search_matches_terminals_and_ended_conversations() {
        let mut b = bench("search");
        let (app, rx, _) = b.app(&["t1", "t2", "t3", "t4"]);
        let claude = |b: &mut Bench, t: &str, sid: &str, payload: Value| {
            let mut p = json!({"session_id": sid, "transcript_path": format!("/t/{sid}")});
            p.as_object_mut().unwrap().extend(payload.as_object().unwrap().clone());
            b.hook(t, "claude", p)
        };
        claude(&mut b, "t1", "a", json!({"hook_event_name": "UserPromptSubmit", "prompt": "修复 Login 页面"}));
        claude(&mut b, "t2", "b", json!({"hook_event_name": "UserPromptSubmit", "prompt": "整理文档"}));
        b.req(app, &rx, json!({"type": "terminal.rename", "terminal_id": "t3", "name": "部署脚本"}));
        // 已结束的对话：一条 20 天前结束、标题含 100%_done；一条最后回复含关键词；一条被接替的不算。
        claude(&mut b, "t4", "c", json!({"hook_event_name": "UserPromptSubmit", "prompt": "旧任务 100%_done"}));
        claude(&mut b, "t4", "c", json!({"hook_event_name": "SessionEnd"}));
        claude(&mut b, "t4", "d", json!({"hook_event_name": "UserPromptSubmit", "prompt": "另一件事"}));
        claude(&mut b, "t4", "d", json!({"hook_event_name": "SessionEnd"}));
        let old = now_ms() - 20 * 86_400_000;
        b.core.store.exec(&format!("UPDATE conversation SET ended_at = {old} WHERE native_id = 'c'"));
        b.core.store.exec("UPDATE conversation SET last_reply = '已生成 Changelog' WHERE native_id = 'd'");
        drain(&rx);
        let search = |b: &mut Bench, q: &str| {
            let (r, _) = b.req(app, &rx, json!({"type": "search", "query": q}));
            assert_eq!(r["ok"], true, "{r}");
            let ts: Vec<String> = r["terminals"].as_array().unwrap().iter().map(|t| t.as_str().unwrap().to_owned()).collect();
            let cs: Vec<String> = r["conversations"].as_array().unwrap().iter().map(|c| c["native_id"].as_str().unwrap().to_owned()).collect();
            (ts, cs)
        };
        assert_eq!(search(&mut b, " login "), (vec!["t1".to_owned()], vec![]), "提示词，不区分大小写、去掉首尾空白");
        assert_eq!(search(&mut b, "部署"), (vec!["t3".to_owned()], vec![]), "终端名");
        assert_eq!(search(&mut b, "100%_DONE"), (vec![], vec!["c".to_owned()]), "20 天前的对话；% 与 _ 按字面");
        assert_eq!(search(&mut b, "100x_done").1, Vec::<String>::new(), "_ 不当通配符");
        assert_eq!(search(&mut b, "changelog").1, vec!["d".to_owned()], "最后回复（ASCII 不区分大小写）");
        let (ts, cs) = search(&mut b, "/w/P");
        assert_eq!((ts.len(), cs.len()), (4, 2), "目录与项目目录：所有终端与对话都在 /w/p 下");
        b.core.store.exec("UPDATE conversation SET title = '标题命中' WHERE native_id = 'b'");
        assert_eq!(search(&mut b, "标题命中").0, vec!["t2".to_owned()], "任务标题");
        b.core.store.exec("UPDATE conversation SET end_reason = 'continued' WHERE native_id = 'd'");
        assert_eq!(search(&mut b, "changelog").1, Vec::<String>::new(), "被接替的不算");
        for q in [json!("   "), json!("长".repeat(201)), Value::Null] {
            let (r, _) = b.req(app, &rx, json!({"type": "search", "query": q}));
            assert_eq!(r["error"]["code"], "bad_request");
        }
    }

    /// 重启后恢复终端（M4a 追加块 10）：同步时没被列出、后台也没有 PTY 的终端关闭前记下断电前的目录与对话，
    /// 随快照的 `restorable` 交给 App；只交一次，关闭后不再记目录。
    #[test]
    fn sync_hands_lost_terminals_back_for_restore() {
        let mut b = bench("restore");
        let (app, rx, _) = b.app(&["t1"]);
        b.hook("t1", "claude", json!({"hook_event_name": "SessionStart", "session_id": "s1", "transcript_path": "/t/s1"}));
        let cid = b.core.panes["t1"].conversation.unwrap();
        let (r, _) = b.req(app, &rx, json!({"type": "terminal.cwd", "terminal_id": "t1", "cwd": "/w/p/src"}));
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(b.core.workspace.get("t1"), Some("/w/p/src"));
        let (r, _) = b.req(app, &rx, json!({"type": "sync", "terminals": []}));
        let lost = r["snapshot"]["restorable"].as_array().expect("应交回丢失的终端").clone();
        assert_eq!(lost.len(), 1);
        assert_eq!((lost[0]["terminal_id"].as_str(), lost[0]["dir"].as_str(), lost[0]["cwd"].as_str()),
                   (Some("t1"), Some("/w/p/src"), Some("/w/p")));
        assert_eq!(lost[0]["conversation_id"].as_i64(), Some(cid));
        assert_eq!(b.core.workspace.get("t1"), None, "关闭后不再记目录");
        let (r, _) = b.req(app, &rx, json!({"type": "sync", "terminals": []}));
        assert!(r["snapshot"]["restorable"].is_null(), "只交一次");
    }

    /// 缺陷 B（2026-11-15，macOS 14.6.1 虚拟机真实重启）：关机时 Agent 先于后台被结束，后台照常把对话记为结束，重启后
    /// 没有未结束的对话可接。终端还开着时结束的对话记为待确认，后台活过确认期才清掉；丢失终端时还在的据此交回。
    #[test]
    fn conversation_ended_just_before_shutdown_is_handed_back() {
        let mut b = bench("shutdown-ended");
        let (app, rx, _) = b.app(&["t1", "t2", "t3"]);
        let kimi = |b: &mut Bench, t: &str, ev: &str, sid: &str| b.hook(t, "kimi", json!({"hook_event_name": ev, "session_id": sid}));
        // t2：SessionEnd 结束了对话，后台活过确认期 → 正常结束，清掉。
        kimi(&mut b, "t2", "SessionStart", "session_b");
        kimi(&mut b, "t2", "SessionEnd", "session_b");
        assert!(b.core.workspace.ended("t2").is_some(), "SessionEnd 结束的记为待确认");
        b.core.expire_ended(Instant::now() + ENDED_CONFIRM + Duration::from_secs(1));
        assert_eq!(b.core.workspace.ended("t2"), None);
        // t3：结束后这个终端又开了新对话 → 之前那条作废。
        kimi(&mut b, "t3", "SessionStart", "session_c");
        kimi(&mut b, "t3", "SessionEnd", "session_c");
        kimi(&mut b, "t3", "SessionStart", "session_d");
        assert_eq!(b.core.workspace.ended("t3"), None, "有了新对话，之前的结束作废");
        // t1：Agent 进程退出，宽限期满按进程退出结束；确认期内后台就丢了（关机）→ 交回这段对话。
        kimi(&mut b, "t1", "SessionStart", "session_a");
        let a = b.core.panes["t1"].conversation.unwrap();
        let t0 = Instant::now();
        b.core.panes.get_mut("t1").unwrap().exit_deadline = Some((t0, Tool::Kimi));
        b.core.expire_exits(t0);
        assert!(b.core.store.open_conversation_on("t1").unwrap().is_none(), "对话已按进程退出结束");
        assert_eq!(b.core.workspace.ended("t1"), Some(a));
        let (r, _) = b.req(app, &rx, json!({"type": "sync", "terminals": []}));
        let lost = r["snapshot"]["restorable"].as_array().expect("应交回丢失的终端").clone();
        let conv = |t: &str| lost.iter().find(|x| x["terminal_id"] == t).map(|x| x["conversation_id"].as_i64());
        assert_eq!(conv("t1"), Some(Some(a)), "关机时结束的对话交回");
        assert_eq!(conv("t2"), Some(None), "确认期满的正常结束不交回");
        assert_eq!(b.core.workspace.ended("t1"), None, "终端关闭后一起清掉");
    }

    /// 2026-11-16（虚拟机第二次真实重启丢了一个终端）：关机时 Shell 与后台几乎同时被结束，Shell 先被处理就当成了
    /// 用户关闭。App 不在时被信号结束（128 + 信号）的留待恢复；App 在时、或退出码不是信号（用户 exit 总是 0）照常关闭。
    #[test]
    fn shell_killed_while_app_away_is_kept_for_restore() {
        let mut b = bench("shell-killed");
        let held = |ids: &[&str]| -> Value {
            json!({"type": "sync", "terminals": ids.iter()
                .map(|t| json!({"terminal_id": t, "project_dir": "/w/p/", "cwd": "/w/p", "held": true}))
                .collect::<Vec<_>>()})
        };
        let (app, rx, _) = b.app(&[]);
        for t in ["t1", "t2", "t3"] {
            let (r, _) = b.req(app, &rx, json!({"type": "terminal.open", "terminal_id": t, "project_dir": "/w/p/", "cwd": "/w/p", "held": true}));
            assert_eq!(r["ok"], true, "{r}");
        }
        b.hook("t1", "claude", json!({"hook_event_name": "SessionStart", "session_id": "s1", "transcript_path": "/t/s1"}));
        let cid = b.core.panes["t1"].conversation.unwrap();
        let (r, _) = b.req(app, &rx, json!({"type": "terminal.cwd", "terminal_id": "t1", "cwd": "/w/p/src"}));
        assert_eq!(r["ok"], true, "{r}");
        let alive = |b: &Bench, t: &str| b.core.store.terminal(t).unwrap().is_some_and(|r| r["status"] == "alive");
        // t3：App 在时被信号结束 → 照常关闭。
        b.core.shell_gone("t3", Some(143));
        assert!(!alive(&b, "t3"), "App 在时照常关闭");
        // App 断开（测试台没有真的 PTY，drop_conn 会把这几个终端当非保活关掉，这里只摘掉 App）。
        b.core.app = None;
        // t1：App 不在时被 SIGTERM 结束 → 留着；t2：退出码 0（用户 exit）→ 关闭。
        b.core.shell_gone("t1", Some(143));
        b.core.shell_gone("t2", Some(0));
        assert!(alive(&b, "t1"), "App 不在时被信号结束的留待恢复");
        assert_eq!(b.core.workspace.get("t1"), Some("/w/p/src"), "目录留着");
        assert!(!alive(&b, "t2"), "退出码 0 照常关闭");
        // 后台还在、App 回来：按「保活终端没有 PTY」交回，带上对话。
        let (app, rx, _) = b.connect("app", PROTOCOL_VERSION);
        let (r, _) = b.req(app, &rx, held(&["t1"]));
        let lost = r["snapshot"]["restorable"].as_array().expect("应交回 t1").clone();
        assert_eq!(lost.len(), 1, "{lost:?}");
        assert_eq!((lost[0]["terminal_id"].as_str(), lost[0]["conversation_id"].as_i64()), (Some("t1"), Some(cid)));
    }

    /// 后台崩溃重启后在原终端重起 Shell（决策 21）：绑着的未结束对话被结束，App 已订阅即推 `terminal.respawned`，
    /// 没订阅时存着、随下一次 `sync` 的快照交出且只交一次；没绑对话的终端不推。
    #[test]
    fn respawned_terminal_hands_conversation_back() {
        let mut b = bench("respawn");
        let (app, rx, _) = b.app(&["t1", "t2"]);
        b.hook("t1", "codex", json!({"hook_event_name": "SessionStart", "session_id": "s1", "transcript_path": "/t/s1"}));
        let cid = b.core.panes["t1"].conversation.unwrap();
        drain(&rx);
        assert!(b.core.respawned_with_conversation("t2").is_ok());
        assert!(drain(&rx).iter().all(|m| m["type"] != "terminal.respawned"), "没绑对话的终端不推");
        assert!(b.core.respawned_with_conversation("t1").is_ok());
        let msgs = drain(&rx);
        let m = msgs.iter().find(|m| m["type"] == "terminal.respawned").expect("应推送");
        assert_eq!((m["terminal_id"].as_str(), m["conversation_id"].as_i64()), (Some("t1"), Some(cid)));
        assert!(b.core.store.open_conversation_on("t1").unwrap().is_none(), "原对话已结束");
        // App 还没订阅：存着，随 sync 交出一次。
        b.hook("t1", "codex", json!({"hook_event_name": "SessionStart", "session_id": "s2", "transcript_path": "/t/s2"}));
        for c in b.core.conns.values_mut() {
            c.subscribed = false;
        }
        assert!(b.core.respawned_with_conversation("t1").is_ok());
        assert_eq!(b.core.respawned.len(), 1);
        let (r, _) = b.req(app, &rx, json!({"type": "sync", "terminals": [{"terminal_id": "t1", "project_dir": "/w/p/", "cwd": "/w/p"}, {"terminal_id": "t2", "project_dir": "/w/p/", "cwd": "/w/p"}]}));
        assert_eq!(r["snapshot"]["respawned"][0]["terminal_id"], "t1", "{r}");
        let (r, _) = b.req(app, &rx, json!({"type": "sync", "terminals": [{"terminal_id": "t1", "project_dir": "/w/p/", "cwd": "/w/p"}, {"terminal_id": "t2", "project_dir": "/w/p/", "cwd": "/w/p"}]}));
        assert!(r["snapshot"]["respawned"].is_null(), "只交一次");
    }

    /// M4b 块 5 P2：前台识别为某款 AI 时记进 workspace，回到普通命令时清除；丢失与重起 Shell 的终端带上原来前台的
    /// AI（`fg_tool`），没有可恢复的对话时 App 据此说明没能接回的原因。
    #[test]
    fn lost_and_respawned_terminals_carry_foreground_agent() {
        let mut b = bench("lost-agent");
        let (app, rx, _) = b.app(&["t1", "t2"]);
        let mut cli = fake_kimi(&b);
        foreground(&mut b, app, &rx, cli.id() as i32);
        assert_eq!(b.core.workspace.agent("t1"), Some("kimi"));
        foreground(&mut b, app, &rx, std::process::id() as i32);
        assert_eq!(b.core.workspace.agent("t1"), None, "回到普通命令即清除");
        cli.kill().unwrap();
        cli.wait().unwrap();
        b.core.workspace.set_agent("t1", Some("codex"));
        drain(&rx);
        assert!(b.core.respawned_with_conversation("t1").is_ok());
        let msgs = drain(&rx);
        let m = msgs.iter().find(|m| m["type"] == "terminal.respawned").expect("没有对话但前台有 AI 也应推送");
        assert_eq!((m["terminal_id"].as_str(), m["fg_tool"].as_str()), (Some("t1"), Some("codex")));
        assert!(m["conversation_id"].is_null());
        assert_eq!(b.core.workspace.agent("t1"), None, "重起 Shell 后清除");
        b.core.workspace.set_agent("t1", Some("codex"));
        let (r, _) = b.req(app, &rx, json!({"type": "sync", "terminals": []}));
        let lost = r["snapshot"]["restorable"].as_array().expect("应交回丢失的终端").clone();
        let tool = |id: &str| lost.iter().find(|t| t["terminal_id"] == id).map(|t| t["fg_tool"].clone());
        assert_eq!((tool("t1"), tool("t2")), (Some(json!("codex")), Some(Value::Null)));
    }

    /// 历史提示词（M4a 追加块 9）：`prompts.list` 交给读取线程，读完才回；恢复链由旧到新；对话不存在立即回错误。
    #[test]
    fn prompts_list_reads_on_worker_and_replies() {
        let mut b = bench("prompts");
        let (ptx, prx) = mpsc::channel();
        b.core.prompts = Some(PromptWorker::new(ptx));
        let (app, rx, _) = b.app(&["t1"]);
        let file = b.dir.join("s1.jsonl");
        fs::write(&file, "{\"type\":\"user\",\"message\":{\"content\":\"第一句\"}}\n{\"type\":\"user\",\"message\":{\"content\":\"第二句\"}}\n").unwrap();
        b.hook("t1", "claude", json!({"hook_event_name": "SessionStart", "session_id": "s1", "transcript_path": file}));
        let cid = b.core.panes["t1"].conversation.unwrap();
        drain(&rx);
        b.core.handle(Cmd::Request { conn: app, msg: json!({"type": "prompts.list", "id": 7, "conversation_id": cid}) });
        assert!(drain(&rx).iter().all(|m| m["type"] != "reply"), "读完才回");
        let cmd = prx.recv_timeout(Duration::from_secs(5)).expect("读取线程应送回结果");
        b.core.handle(cmd);
        let reply = drain(&rx).into_iter().find(|m| m["type"] == "reply").expect("应有应答");
        assert_eq!((reply["id"].clone(), reply["ok"].clone()), (json!(7), json!(true)));
        let texts: Vec<&str> = reply["items"].as_array().unwrap().iter().map(|i| i["text"].as_str().unwrap()).collect();
        assert_eq!(texts, ["第一句", "第二句"]);
        let (r, _) = b.req(app, &rx, json!({"type": "prompts.list", "conversation_id": 999}));
        assert_eq!(r["error"]["code"], "not_found");
    }

    /// Token 与周额度（M2 第 3 块）：token 有变化才推送 `usage.upsert`（带模型）；周额度按工具推送 `quota.upsert`，旧的不要、
    /// 没变化不满 1 分钟不推送；statusline 帧记 Claude 的周额度，`model.id` 按 session_id 记到正在运行的对话；快照带 `quotas`。
    #[test]
    fn tokens_quota_and_statusline() {
        let mut b = bench("quota");
        let (app, rx, _) = b.app(&["t1"]);
        b.hook("t1", "claude", json!({"hook_event_name": "SessionStart", "session_id": "s1", "transcript_path": "/t/s1"}));
        b.hook("t1", "claude", json!({"hook_event_name": "UserPromptSubmit", "session_id": "s1", "prompt": "hi"}));
        let cid = b.core.panes["t1"].conversation.unwrap();
        drain(&rx);
        let t = Tokens { input: 2, cache_read: 100, cache_write: 10, output: 5, total: 117 };
        b.core.handle(Cmd::TokensRead { conversation_id: cid, tokens: t });
        let ups: Vec<Value> = drain(&rx).into_iter().filter(|m| m["type"] == "usage.upsert").collect();
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0]["usage"], json!({"model": null, "model_source": null, "total_tokens": 117,
            "tokens": {"input": 2, "cache_read": 100, "cache_write": 10, "output": 5}}));
        b.core.handle(Cmd::TokensRead { conversation_id: cid, tokens: t });
        assert!(drain(&rx).iter().all(|m| m["type"] != "usage.upsert"), "相同不推送");
        let q = |p: f64, at: i64| Quota { window: Window::SevenDay, used_percent: p, resets_at: 1_792_675_611_000, as_of: at, unlimited: false };
        let quotas = |rx: &Receiver<Out>| drain(rx).into_iter().filter(|m| m["type"] == "quota.upsert").collect::<Vec<_>>();
        let snap = |b: &mut Bench, qs: Vec<Quota>| b.core.handle(Cmd::QuotaSnapshot { tool: Tool::Codex, quotas: qs, source: "log" });
        snap(&mut b, vec![q(3.0, 1_000_000)]);
        let ups = quotas(&rx);
        assert_eq!(ups.len(), 1);
        assert_eq!(ups[0]["tool"], "codex");
        assert_eq!(ups[0]["quota"], json!({"window": "7d", "used_percent": 3.0, "resets_at": 1_792_675_611_000i64, "as_of": 1_000_000, "source": "log"}));
        snap(&mut b, vec![q(2.0, 999_000)]);
        snap(&mut b, vec![q(3.0, 1_030_000)]);
        assert!(quotas(&rx).is_empty(), "更旧的、没变化且不满 1 分钟的不推送");
        snap(&mut b, vec![q(3.0, 1_060_000)]);
        snap(&mut b, vec![q(4.0, 1_061_000)]);
        assert_eq!(quotas(&rx).len(), 2);
        snap(&mut b, Vec::new());
        assert!(drain(&rx).is_empty(), "空快照不算数");
        // 整份替换（2026-11-01）：5 小时另记一份；之后的快照里没有 5 小时，就删掉并推送 `quota.remove`。
        let five = Quota { window: Window::FiveHour, ..q(10.0, 1_062_000) };
        snap(&mut b, vec![five, q(4.0, 1_062_000)]);
        assert_eq!(quotas(&rx).len(), 1, "7 天没变、不满 1 分钟不推送，5 小时是新的");
        snap(&mut b, vec![q(4.0, 1_200_000)]);
        let out = drain(&rx);
        let removed: Vec<&Value> = out.iter().filter(|m| m["type"] == "quota.remove").collect();
        assert_eq!((removed.len(), &removed[0]["tool"], &removed[0]["window"]), (1, &json!("codex"), &json!("5h")));
        // 查不到额度的原因：有变化才推送，读到额度即清掉。
        b.core.handle(Cmd::QuotaNote { tool: Tool::Grok, note: "login_required" });
        b.core.handle(Cmd::QuotaNote { tool: Tool::Grok, note: "login_required" });
        let notes: Vec<Value> = drain(&rx).into_iter().filter(|m| m["type"] == "quota.note").collect();
        assert_eq!((notes.len(), &notes[0]["tool"], &notes[0]["note"]), (1, &json!("grok"), &json!("login_required")));
        let (s, _) = b.req(app, &rx, json!({"type": "snapshot.get"}));
        assert_eq!(s["snapshot"]["quota_notes"], json!([{"tool": "grok", "note": "login_required"}]));
        b.core.handle(Cmd::QuotaSnapshot { tool: Tool::Grok, quotas: vec![q(1.0, 1_300_000)], source: "command" });
        let notes: Vec<Value> = drain(&rx).into_iter().filter(|m| m["type"] == "quota.note").collect();
        assert_eq!(notes[0]["note"], Value::Null);
        // 按量计费：有依据才带 `billing`，有变化才推送。
        b.core.handle(Cmd::BillingRead { conversation_id: cid, api: false });
        assert!(drain(&rx).iter().all(|m| m["type"] != "usage.upsert"), "默认就不是按量计费");
        b.core.handle(Cmd::BillingRead { conversation_id: cid, api: true });
        let ups: Vec<Value> = drain(&rx).into_iter().filter(|m| m["type"] == "usage.upsert").collect();
        assert_eq!(ups[0]["usage"]["billing"], "api");
        b.core.handle(Cmd::BillingRead { conversation_id: cid, api: false });
        let ups: Vec<Value> = drain(&rx).into_iter().filter(|m| m["type"] == "usage.upsert").collect();
        assert!(ups[0]["usage"].get("billing").is_none());
        // statusline 帧：周额度记到 Claude；模型记到运行 s1 的对话；别的会话、别的来源不记模型。
        let frame = |sid: &str, source: &str| json!({"type": "statusline", "source": source, "payload": {"session_id": sid,
            "model": {"id": "claude-opus-5-5", "display_name": "Opus"},
            "rate_limits": {"five_hour": {"used_percentage": 5, "resets_at": 1_792_075_611}, "seven_day": {"used_percentage": 34.5, "resets_at": 1_792_675_611}}}});
        b.core.handle(Cmd::Statusline { frame: frame("s1", "claude") });
        let out = drain(&rx);
        let qs: Vec<&Value> = out.iter().filter(|m| m["type"] == "quota.upsert").collect();
        assert_eq!((qs.len(), &qs[0]["tool"], &qs[0]["quota"]["source"], &qs[0]["quota"]["window"], &qs[0]["quota"]["used_percent"]),
                   (2, &json!("claude"), &json!("statusline"), &json!("5h"), &json!(5.0)));
        assert_eq!((&qs[1]["quota"]["window"], &qs[1]["quota"]["used_percent"]), (&json!("7d"), &json!(34.5)));
        let us: Vec<&Value> = out.iter().filter(|m| m["type"] == "usage.upsert").collect();
        assert_eq!(us[0]["usage"]["model"], "claude-opus-5-5");
        assert_eq!(us[0]["usage"]["model_source"], "statusline");
        assert_eq!(us[0]["usage"]["total_tokens"], 117, "token 保留");
        b.core.handle(Cmd::Statusline { frame: frame("s2", "claude") });
        assert!(drain(&rx).iter().all(|m| m["type"] != "usage.upsert"), "没有终端在运行的会话不记模型");
        b.core.handle(Cmd::Statusline { frame: frame("s1", "codex") });
        assert!(drain(&rx).is_empty(), "只认 claude");
        let (snap, _) = b.req(app, &rx, json!({"type": "snapshot.get"}));
        let mut qs: Vec<String> = snap["snapshot"]["quotas"].as_array().unwrap().iter().map(|q| q["tool"].as_str().unwrap().to_owned()).collect();
        qs.sort();
        assert_eq!(qs, ["claude", "claude", "codex", "grok"], "每款每个窗口一份");
        // 闲置会话的旧帧（2026-11-05）：只带 7d（它那份 5h 已过重置时间被 Claude 删掉）且用量更低——5h 不删、7d 不压低。
        let sl = |rl: Value| json!({"type": "statusline", "source": "claude", "payload": {"session_id": "s9", "rate_limits": rl}});
        b.core.handle(Cmd::Statusline { frame: sl(json!({"seven_day": {"used_percentage": 30, "resets_at": 1_792_675_611}})) });
        assert!(drain(&rx).iter().all(|m| m["type"] != "quota.remove" && m["type"] != "quota.upsert"), "旧帧不删不改");
        // 更早一期的窗口也不收；同一窗口用量涨了、换到新一期都收。
        b.core.handle(Cmd::Statusline { frame: sl(json!({"five_hour": {"used_percentage": 50, "resets_at": 1_792_000_000}})) });
        assert!(drain(&rx).iter().all(|m| m["type"] != "quota.upsert"), "上一期的 5h 不收");
        b.core.handle(Cmd::Statusline { frame: sl(json!({"five_hour": {"used_percentage": 7, "resets_at": 1_792_075_611},
                                                         "seven_day": {"used_percentage": 1, "resets_at": 1_793_280_411}})) });
        let ups: Vec<(Value, Value)> = drain(&rx).into_iter().filter(|m| m["type"] == "quota.upsert")
            .map(|m| (m["quota"]["window"].clone(), m["quota"]["used_percent"].clone())).collect();
        assert_eq!(ups, [(json!("5h"), json!(7.0)), (json!("7d"), json!(1.0))]);
    }

    /// 标题栏的项目（M2 第 2 块）：只有 watch 的项目带 `git`；换项目即丢掉旧结果；不是 watch 的项目的结果不推送；
    /// 结果相同不重复推送；proc.stats 在还没上报前台时三项为空。
    #[test]
    fn status_watch_and_git_field() {
        let mut b = bench("status-watch");
        let (app, rx, snap) = b.app(&["t1"]);
        let pid = snap["projects"][0]["project_id"].as_i64().unwrap();
        let (r, _) = b.req(app, &rx, json!({"type": "status.watch", "project_id": pid}));
        assert_eq!(r["ok"], true);
        let st = GitStatus { dirty: true, added: 3, deleted: 1, untracked: 2 };
        b.core.handle(Cmd::GitStatus { project_id: pid, status: Some(st.clone()) });
        let ps: Vec<Value> = drain(&rx).into_iter().filter(|m| m["type"] == "project.upsert").collect();
        assert_eq!(ps.last().unwrap()["project"]["git"], json!({"dirty": true, "added": 3, "deleted": 1, "untracked": 2}));
        b.core.handle(Cmd::GitStatus { project_id: pid, status: Some(st) });
        assert!(drain(&rx).iter().all(|m| m["type"] != "project.upsert"), "相同不推送");
        b.core.handle(Cmd::GitStatus { project_id: pid, status: None });
        let ps: Vec<Value> = drain(&rx).into_iter().filter(|m| m["type"] == "project.upsert").collect();
        assert_eq!(ps.last().unwrap()["project"]["git"], Value::Null, "读不到为 null");
        b.req(app, &rx, json!({"type": "status.watch", "project_id": null}));
        b.core.handle(Cmd::GitStatus { project_id: pid, status: Some(GitStatus::default()) });
        assert!(drain(&rx).iter().all(|m| m["type"] != "project.upsert"), "不在显示的项目不推送");
        let (snap, _) = b.req(app, &rx, json!({"type": "snapshot.get"}));
        assert!(snap["snapshot"]["projects"][0].get("git").is_none(), "换掉后没有该字段");
        let (r, _) = b.req(app, &rx, json!({"type": "status.watch", "project_id": 999}));
        assert_eq!(r["error"]["code"], "not_found");
        let (r, _) = b.req(app, &rx, json!({"type": "proc.stats", "terminal_id": "t1"}));
        assert_eq!((r["cpu_percent"].clone(), r["mem_bytes"].clone(), r["processes"].clone()), (Value::Null, Value::Null, json!(0)));
        let (r, _) = b.req(app, &rx, json!({"type": "proc.stats", "terminal_id": "nope"}));
        assert_eq!(r["error"]["code"], "not_found");
        let (cli, cli_rx, _) = b.connect("cli", PROTOCOL_VERSION);
        let (r, _) = b.req(cli, &cli_rx, json!({"type": "status.watch", "project_id": pid}));
        assert_eq!(r["error"]["code"], "bad_request", "只有 app 角色可用");
    }

    #[test]
    fn recent_total_follows_end_reopen_and_retention() {
        let mut b = bench("recent-total");
        let (_app, rx, snap) = b.app(&["t1"]);
        assert_eq!(snap["recent_total"], 0);
        let total_of = |pushes: &[Value], native: &str| {
            pushes
                .iter()
                .filter(|m| {
                    m["type"] == "conversation.upsert" && m["conversation"]["native_id"] == native
                })
                .last()
                .map(|m| m["recent_total"].as_i64().unwrap())
        };
        claude(&mut b, "t1", "SessionStart", "a");
        b.hook(
            "t1",
            "claude",
            json!({"hook_event_name": "UserPromptSubmit", "session_id": "a", "prompt": "今天星期几"}),
        );
        claude(&mut b, "t1", "SessionEnd", "a");
        assert_eq!(total_of(&drain(&rx), "a"), Some(1));
        // 恢复同一会话：重新打开，离开「最近任务」。
        claude(&mut b, "t1", "SessionStart", "a");
        assert_eq!(total_of(&drain(&rx), "a"), Some(0));
        // 已有保留上限条已结束对话时再结束一条：推送的是清理后的总数。
        let now = now_ms();
        let mut sql = String::from("BEGIN;");
        for i in 0..crate::store::KEEP_CONVERSATIONS {
            sql += &format!(
                "INSERT INTO conversation (tool, native_id, last_prompt, started_at, last_event_at, ended_at) \
                 VALUES ('codex', 'old{i}', '问', 0, 0, {});",
                now - 100_000 + i
            );
        }
        sql += "COMMIT;";
        b.core.store.exec(&sql);
        claude(&mut b, "t1", "SessionEnd", "a");
        assert_eq!(
            total_of(&drain(&rx), "a"),
            Some(crate::store::KEEP_CONVERSATIONS)
        );
        let (_, _, snap) = {
            b.core.handle(Cmd::Closed { conn: _app });
            b.app(&[])
        };
        assert_eq!(snap["recent_total"], crate::store::KEEP_CONVERSATIONS);
    }

    /// 从没提问过就结束的对话（如 Codex 不提问就退出时只发来 SessionEnd）不进「最近任务」，也不计入总数。
    #[test]
    fn empty_conversation_is_not_recent() {
        let mut b = bench("recent-empty");
        let (_app, rx, _) = b.app(&["t1"]);
        b.hook(
            "t1",
            "codex",
            json!({"hook_event_name": "SessionEnd", "session_id": "01a1303b", "transcript_path": "/x.jsonl"}),
        );
        let pushes = drain(&rx);
        let total = pushes.iter().filter(|m| m["type"] == "conversation.upsert").last();
        assert_eq!(total.map(|m| m["recent_total"].as_i64().unwrap()), Some(0));
        let (items, _) = b.core.store.recent(None, None, None, 10).unwrap();
        assert!(items.is_empty());
    }

    #[test]
    fn project_branch_follows_head() {
        let mut b = bench("branch");
        let repo = b.dir.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("sub")).unwrap();
        fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let (app, rx, _) = b.app(&[]);
        let dir = repo.join("sub").to_string_lossy().into_owned();
        let (r, pushes) = b.req(
            app,
            &rx,
            json!({"type":"terminal.open","terminal_id":"t1","project_dir":dir,"cwd":dir}),
        );
        assert_eq!(r["ok"], true);
        let p = pushes.iter().find(|m| m["type"] == "project.upsert").unwrap();
        assert!(p["project"]["branch"].is_null(), "建项目时不等读分支");
        // 分支由 GitWorker 读到后再推送。
        b.settle();
        let pushes = drain(&rx);
        let p = pushes.iter().find(|m| m["type"] == "project.upsert").unwrap();
        assert_eq!(p["project"]["branch"], "main", "项目目录在仓库子目录里");
        // 不是仓库的项目：branch 为 null。
        let (_, rx2, snap) = {
            b.core.handle(Cmd::Closed { conn: app });
            b.app(&["t2"])
        };
        let plain = snap["projects"].as_array().unwrap().iter().find(|p| p["dir"] == "/w/p").unwrap();
        assert!(plain["branch"].is_null());
        let main = snap["projects"].as_array().unwrap().iter().find(|p| p["dir"] == dir.as_str()).unwrap();
        assert_eq!(main["branch"], "main", "快照中带分支");
        // 切换分支：git 先写 HEAD.lock 再改名覆盖；kqueue 通知交给核心后推送新分支。
        fs::write(repo.join(".git/HEAD.lock"), "ref: refs/heads/dev\n").unwrap();
        fs::rename(repo.join(".git/HEAD.lock"), repo.join(".git/HEAD")).unwrap();
        let cmd = b.next_cmd(|c| matches!(c, Cmd::GitChanged { .. }), "应收到 git 目录变化");
        b.core.handle(cmd);
        b.settle();
        let pushes = drain(&rx2);
        let p = pushes.iter().find(|m| m["type"] == "project.upsert").unwrap();
        assert_eq!(p["project"]["branch"], "dev");
    }

    /// 符号链接路径（2026-10-23 修正）：经符号链接进入仓库按真实路径归组；从这个终端新建（启动目录就是它此刻所在的
    /// 符号链接路径）直接沿用同一项目，不先按符号链接路径建项目；新终端的 Shell 报真实路径后仍是同一项目。
    #[test]
    fn symlinked_paths_share_one_project() {
        let mut b = bench("cwd-link");
        let repo = b.dir.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::os::unix::fs::symlink(&repo, b.dir.join("alias")).unwrap();
        let real = fs::canonicalize(&repo).unwrap().to_string_lossy().into_owned();
        let alias = b.dir.join("alias").to_string_lossy().into_owned();
        let (app, rx, _) = b.app(&["t1"]);
        b.settle();
        b.req(app, &rx, json!({"type":"terminal.cwd","terminal_id":"t1","cwd":alias}));
        b.settle();
        let pid = b.count(&format!("SELECT id FROM project WHERE dir = '{real}'"));
        assert_eq!(b.count("SELECT project_id FROM terminal WHERE id = 't1'"), pid, "按真实路径归组");
        let before = b.count("SELECT COUNT(*) FROM project");
        let (r, _) = b.req(app, &rx, json!({"type":"terminal.open","terminal_id":"t2","project_dir":alias,"cwd":alias}));
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(b.count("SELECT project_id FROM terminal WHERE id = 't2'"), pid, "沿用同目录终端的项目");
        assert_eq!(b.count("SELECT COUNT(*) FROM project"), before, "没有多出按符号链接路径建的项目");
        b.req(app, &rx, json!({"type":"terminal.cwd","terminal_id":"t2","cwd":real}));
        b.settle();
        assert_eq!(b.count("SELECT project_id FROM terminal WHERE id = 't2'"), pid);
        assert_eq!(b.count("SELECT COUNT(*) FROM project"), before);
        // 没有同目录的终端时照旧按登记的目录建项目。
        let (r, _) = b.req(app, &rx, json!({"type":"terminal.open","terminal_id":"t3","project_dir":"/w/q","cwd":"/w/q"}));
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(b.count("SELECT project_id FROM terminal WHERE id = 't3'"), b.count("SELECT id FROM project WHERE dir = '/w/q'"));
    }

    #[test]
    fn terminal_follows_cwd_into_repo_root() {
        let mut b = bench("cwd");
        let repo = b.dir.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("src/app")).unwrap();
        fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let (app, rx, _) = b.app(&["t1"]);
        let home = b.count("SELECT id FROM project WHERE dir = '/w/p'");
        b.settle();
        let sub = repo.join("src/app").to_string_lossy().into_owned();
        let (r, pushes) = b.req(app, &rx, json!({"type":"terminal.cwd","terminal_id":"t1","cwd":sub}));
        assert_eq!(r["ok"], true, "{r}");
        // 先只推送当前目录，所在仓库由 GitWorker 判定。
        assert_eq!(pushes.len(), 1);
        assert_eq!(pushes[0]["terminal"]["current_dir"], sub.as_str());
        assert_eq!(pushes[0]["terminal"]["project_id"], home);
        b.settle();
        let pushes = drain(&rx);
        let p = pushes.iter().find(|m| m["type"] == "project.upsert").expect("新项目先推送");
        // 按工作区根的真实路径归组（测试目录在 `$TMPDIR` 下，即 `/var/folders/…` → `/private/var/folders/…`）。
        let real = fs::canonicalize(&repo).unwrap().to_string_lossy().into_owned();
        assert_eq!(p["project"]["dir"], real.as_str(), "按工作区根归组");
        let t = pushes.iter().find(|m| m["type"] == "terminal.upsert").unwrap();
        assert_eq!(t["terminal"]["project_id"], p["project"]["project_id"]);
        assert_eq!(t["terminal"]["current_dir"], sub.as_str());
        assert_eq!(t["terminal"]["cwd"], "/w/p", "启动目录不变");
        let last = pushes.iter().rev().find(|m| m["type"] == "project.upsert").unwrap();
        assert_eq!(last["project"]["branch"], "main");
        // 同一仓库里换子目录：项目不变，只推送当前目录。
        let src = repo.join("src").to_string_lossy().into_owned();
        let (_, pushes) = b.req(app, &rx, json!({"type":"terminal.cwd","terminal_id":"t1","cwd":src}));
        assert_eq!(pushes.len(), 1);
        b.settle();
        assert!(drain(&rx).is_empty());
        // 同一目录重复上报：不推送，也不再判定。
        let (_, pushes) = b.req(app, &rx, json!({"type":"terminal.cwd","terminal_id":"t1","cwd":src}));
        assert!(pushes.is_empty());
        // 回到非仓库目录：归回该目录本身对应的项目。
        b.req(app, &rx, json!({"type":"terminal.cwd","terminal_id":"t1","cwd":"/w/p/"}));
        b.settle();
        let pushes = drain(&rx);
        let t = pushes.iter().find(|m| m["type"] == "terminal.upsert").unwrap();
        assert_eq!(t["terminal"]["project_id"], home);
        // 新对话归入终端此刻的项目。
        b.req(app, &rx, json!({"type":"terminal.cwd","terminal_id":"t1","cwd":sub}));
        b.settle();
        claude(&mut b, "t1", "SessionStart", "x");
        let repo_id = b.count(&format!("SELECT id FROM project WHERE dir = '{real}'"));
        assert_eq!(b.count("SELECT project_id FROM conversation WHERE native_id = 'x'"), repo_id);
        // 判定未完成时就开始的新对话（如授权弹窗未处理）：判定完成后随终端改归；已结束的 x 不动。
        b.req(app, &rx, json!({"type":"terminal.cwd","terminal_id":"t1","cwd":"/w/p"}));
        claude(&mut b, "t1", "SessionEnd", "x");
        claude(&mut b, "t1", "SessionStart", "y");
        assert_eq!(b.count("SELECT project_id FROM conversation WHERE native_id = 'y'"), repo_id);
        drain(&rx);
        b.settle();
        assert_eq!(b.count("SELECT project_id FROM conversation WHERE native_id = 'y'"), home);
        assert_eq!(b.count("SELECT project_id FROM conversation WHERE native_id = 'x'"), repo_id);
        assert!(drain(&rx).iter().any(|m| m["type"] == "conversation.upsert"));
        // 判定期间又换了目录：旧结果作废，按最后的目录归组。
        b.req(app, &rx, json!({"type":"terminal.cwd","terminal_id":"t1","cwd":sub}));
        b.req(app, &rx, json!({"type":"terminal.cwd","terminal_id":"t1","cwd":"/w/p"}));
        b.settle();
        assert_eq!(b.count("SELECT project_id FROM terminal WHERE id = 't1'"), home);
        // 错误请求。
        let (r, _) = b.req(app, &rx, json!({"type":"terminal.cwd","terminal_id":"t1","cwd":"rel"}));
        assert_eq!(r["error"]["code"], "bad_request");
        let (r, _) = b.req(app, &rx, json!({"type":"terminal.cwd","terminal_id":"nope","cwd":"/"}));
        assert_eq!(r["error"]["code"], "not_found");
    }

    // ---- 一键恢复与冲突（docs/protocol.md 第 7 节） ----

    /// 在 t1 里跑完一段对话（提问后结束），返回对话 ID。
    fn ended_conversation(b: &mut Bench, tool: &str, native: &str, transcript: Option<&str>) -> i64 {
        // 显式 `transcript_path: null` 是 CLI 自己的后台会话（会被忽略），没有记录路径时不带该字段（如 Kimi）。
        let mut base = json!({"session_id": native, "cwd": "/w/p/sub"});
        if let Some(t) = transcript {
            base["transcript_path"] = json!(t);
        }
        for (event, extra) in [
            ("SessionStart", json!({})),
            ("UserPromptSubmit", json!({"prompt": "修导航焦点样式"})),
            ("SessionEnd", json!({})),
        ] {
            let mut payload = base.clone();
            payload["hook_event_name"] = json!(event);
            payload.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            b.hook("t1", tool, payload);
        }
        b.count(&format!("SELECT id FROM conversation WHERE native_id = '{native}'"))
    }

    fn resume(b: &mut Bench, app: u64, rx: &Receiver<Out>, cid: i64) -> Value {
        let (r, _) = b.req(app, rx, json!({"type": "conversation.resume", "conversation_id": cid}));
        assert_eq!(r["ok"], true, "{r}");
        r
    }

    /// App 以 resume_of 登记为恢复而开的终端，返回期间的推送。
    fn open_for_resume(b: &mut Bench, app: u64, rx: &Receiver<Out>, terminal: &str, cid: i64) -> Vec<Value> {
        let (r, pushes) = b.req(app, rx, json!({"type": "terminal.open", "terminal_id": terminal,
            "project_dir": "/w/p", "cwd": "/w/p/sub", "resume_of": cid}));
        assert_eq!(r["ok"], true, "{r}");
        pushes
    }

    fn ended_state(b: &Bench, cid: i64) -> (Option<i64>, Option<String>, Option<String>) {
        let c = b.core.store.conversation(cid).unwrap().unwrap();
        (c["ended_at"].as_i64(), c["end_reason"].as_str().map(str::to_owned), c["terminal_id"].as_str().map(str::to_owned))
    }

    /// 恢复：在对话的启动目录执行恢复命令；登记终端即重开并绑定（移回「当前任务」），之后按普通对话处理。
    #[test]
    fn resume_reopens_on_the_new_terminal() {
        let mut b = bench("resume-open");
        let (app, rx, _) = b.app(&["t1"]);
        let cid = ended_conversation(&mut b, "claude", "s1", Some("/x/s1.jsonl"));
        drain(&rx);
        let r = resume(&mut b, app, &rx, cid);
        assert_eq!((r["action"].as_str(), r["command"].as_str(), r["dir"].as_str()),
            (Some("open"), Some("claude --resume s1"), Some("/w/p/sub")));
        let pushes = open_for_resume(&mut b, app, &rx, "t2", cid);
        let conv = pushes.iter().find(|m| m["type"] == "conversation.upsert").expect("应推送重开的对话");
        assert_eq!((conv["conversation"]["ended_at"].clone(), conv["conversation"]["terminal_id"].as_str()), (Value::Null, Some("t2")));
        let t = pushes.iter().rev().find(|m| m["type"] == "terminal.upsert").unwrap();
        assert_eq!((t["terminal"]["conversation_id"].as_i64(), t["terminal"]["agent"]["session_id"].as_str(),
            t["terminal"]["agent"]["state"].as_str()), (Some(cid), Some("s1"), Some("unknown")));
        // 再点一次（双击、或推送未到）：已在 t2 运行，切过去。
        assert_eq!(resume(&mut b, app, &rx, cid)["terminal_id"], "t2");
        b.hook("t2", "claude", json!({"hook_event_name": "SessionStart", "session_id": "s1", "source": "resume"}));
        assert_eq!(b.core.panes["t2"].state.state, AgentState::Ready);
        b.hook("t2", "claude", json!({"hook_event_name": "SessionEnd", "session_id": "s1"}));
        assert_eq!(ended_state(&b, cid).1.as_deref(), Some("session_end"), "不提问就退出也照常结束（排到最前）");
        // 未打开就退出（进程退出宽限到期）：同样按普通对话结束，不还原。
        let cid2 = ended_conversation(&mut b, "codex", "01a1303b-0000-7000-8000-000000000009", None);
        open_for_resume(&mut b, app, &rx, "t3", cid2);
        b.core.panes.get_mut("t3").unwrap().exit_deadline = Some((Instant::now(), Tool::Codex));
        b.core.expire_exits(Instant::now());
        assert_eq!(ended_state(&b, cid2).1.as_deref(), Some("process_exit"));
    }

    /// 前台进程正连着它（Codex `resume <ID>`、Claude `attach <任务>`，首次提问前没有 SessionStart）→ 切过去；
    /// Claude 后台任务还活着 → `claude attach <任务>`，否则 `claude --resume`。
    #[test]
    fn resume_switches_to_attached_terminal_or_reattaches_live_job() {
        let mut b = bench("resume-attach");
        let (app, rx, _) = b.app(&["t1", "t2"]);
        let session = "ee4afd71-c942-453f-a45b-f3e09a0fc8cf";
        let cid = ended_conversation(&mut b, "claude", session, Some("/x/bg.jsonl"));
        assert_eq!(resume(&mut b, app, &rx, cid)["command"], format!("claude --resume {session}"));
        b.core.probe = probe(&[], &["ee4afd71"], &[]);
        assert_eq!(resume(&mut b, app, &rx, cid)["command"], "claude attach ee4afd71", "后台任务还活着");
        b.core.probe = probe(&[(300, "ee4afd71")], &["ee4afd71"], &[]);
        b.core.panes.get_mut("t2").unwrap().foreground =
            Foreground::Agent { pid: 300, path: "claude.exe".into(), tool: Tool::Claude };
        let r = resume(&mut b, app, &rx, cid);
        assert_eq!((r["action"].as_str(), r["terminal_id"].as_str()), (Some("switch"), Some("t2")));
        let codex = "01a1303b-0000-7000-8000-000000000002";
        let c2 = ended_conversation(&mut b, "codex", codex, None);
        b.core.probe = probe(&[(400, codex)], &[], &[]);
        assert_eq!(resume(&mut b, app, &rx, c2)["action"], "open", "前台不是 Codex 的终端不算");
        b.core.panes.get_mut("t1").unwrap().foreground =
            Foreground::Agent { pid: 400, path: "codex".into(), tool: Tool::Codex };
        assert_eq!(resume(&mut b, app, &rx, c2)["terminal_id"], "t1");
        let (r, _) = b.req(app, &rx, json!({"type": "conversation.resume", "conversation_id": 999}));
        assert_eq!(r["error"]["code"], "not_found");
    }

    /// 会话记录已不存在 → gone；「从最近任务移除」后不进「最近任务」，被手动恢复时照常重开。
    #[test]
    fn missing_record_is_gone() {
        let mut b = bench("resume-gone");
        let (app, rx, _) = b.app(&["t1"]);
        let cid = ended_conversation(&mut b, "grok", "g1", Some("/gone/updates.jsonl"));
        b.core.probe = probe(&[], &[], &["/gone/updates.jsonl"]);
        let r = resume(&mut b, app, &rx, cid);
        assert_eq!((r["action"].as_str(), r["reason"].as_str(), r["path"].as_str()),
            (Some("gone"), Some("transcript_missing"), Some("/gone/updates.jsonl")));
        assert_eq!(b.core.store.recent_total(now_ms()).unwrap(), 1);
        // Hook 不带会话记录路径（Copilot）：按适配器由会话 ID 推出的位置核对（2026-11-13 长跑：不存在的会话也被代输了恢复命令）。
        let id = "5845f27b-42f1-4f01-aef3-ae7e9ad825cf";
        for e in ["sessionStart", "sessionEnd"] {
            b.hook("t1", "copilot", json!({"hook_event_name": e, "sessionId": id, "cwd": "/w/p/sub"}));
        }
        let cid = b.count(&format!("SELECT id FROM conversation WHERE native_id = '{id}'"));
        b.core.probe = Probe::system();
        let record = b.dir.join(format!(".copilot/session-state/{id}/events.jsonl"));
        assert_eq!(resume(&mut b, app, &rx, cid)["action"], "open", "没有 session-state 目录：说不准，照常恢复");
        fs::create_dir_all(b.dir.join(".copilot/session-state")).unwrap();
        let r = resume(&mut b, app, &rx, cid);
        assert_eq!((r["action"].as_str(), r["path"].as_str()), (Some("gone"), record.to_str()));
        fs::create_dir_all(record.parent().unwrap()).unwrap();
        fs::write(&record, "").unwrap();
        assert_eq!(resume(&mut b, app, &rx, cid)["command"], format!("copilot --resume={id}"));
    }

    /// `type_into`（缺陷 C）：前台是 AI 或命令时不代输；终端没有 PTY 时 not_found。真实 zsh 里的执行见
    /// tests/pty_host.rs `respawn_resume_is_typed_not_pasted`。
    #[test]
    fn type_into_refuses_busy_or_missing_terminal() {
        let mut b = bench("resume-type");
        let (app, rx, _) = b.app(&["t1"]);
        let cid = ended_conversation(&mut b, "claude", "s1", None);
        let typed = |b: &mut Bench, t: &str| {
            b.req(app, &rx, json!({"type": "conversation.resume", "conversation_id": cid, "type_into": t})).0["error"]["code"].clone()
        };
        assert_eq!(typed(&mut b, "t1"), "not_found", "没有 PTY");
        assert_eq!(typed(&mut b, "nope"), "not_found");
        b.core.panes.get_mut("t1").unwrap().foreground = Foreground::Command { pid: 1, path: "/usr/bin/vim".into() };
        assert_eq!(typed(&mut b, "t1"), "busy");
    }

    /// 「删除记录」（docs/design.md 第 5 节「移除与删除」）：只删已结束的；删后库文件与 WAL 里都不再有提示词原文，
    /// 绑在终端上的解除绑定、状态回到未知，推送 `conversation.remove`；再删为 not_found。
    #[test]
    fn delete_removes_ended_conversation_and_its_text() {
        let mut b = bench("delete");
        let (app, rx, _) = b.app(&["t1"]);
        let cid = ended_conversation(&mut b, "grok", "g1", None);
        let bound = b.core.panes["t1"].conversation == Some(cid);
        drain(&rx);
        let (r, pushes) = b.req(app, &rx, json!({"type": "conversation.delete", "conversation_id": cid}));
        assert_eq!(r["ok"], true, "{r}");
        let p = pushes.iter().find(|m| m["type"] == "conversation.remove").unwrap();
        assert_eq!((p["conversation_id"].as_i64(), p["recent_total"].as_i64()), (Some(cid), Some(0)));
        assert!(b.core.store.conversation(cid).unwrap().is_none());
        assert_eq!(b.core.panes["t1"].conversation, None);
        if bound {
            let t = pushes.iter().filter(|m| m["type"] == "terminal.upsert" && m["terminal"]["terminal_id"] == "t1").last().unwrap();
            assert!(t["terminal"]["conversation_id"].is_null());
            assert_eq!(t["terminal"]["agent"]["state"], "unknown");
        }
        let needle = "修导航焦点样式".as_bytes();
        for f in ["t.db", "t.db-wal"] {
            if let Ok(bytes) = std::fs::read(b.dir.join(f)) {
                assert!(!bytes.windows(needle.len()).any(|w| w == needle), "{f} 里仍有提示词原文");
            }
        }
        let (r, _) = b.req(app, &rx, json!({"type": "conversation.delete", "conversation_id": cid}));
        assert_eq!(r["error"]["code"], "not_found");
        b.hook("t1", "grok", json!({"hook_event_name": "SessionStart", "session_id": "g2", "cwd": "/w/p"}));
        let live = b.core.panes["t1"].conversation.unwrap();
        let (r, _) = b.req(app, &rx, json!({"type": "conversation.delete", "conversation_id": live}));
        assert_eq!(r["error"]["code"], "bad_request", "未结束的不能删");
        assert!(b.core.store.conversation(live).unwrap().is_some());
    }

    /// 保留设置（M4a 第 3 块）：默认 30 天、100 条；`dry_run` 只数不删；改小后写入 `retention.json` 并立即清理，
    /// 逐条推送 `conversation.remove`；档位外的值、cli 角色的修改为 `bad_request`。
    #[test]
    fn retention_set_prunes_and_persists() {
        let mut b = bench("retention-set");
        let (app, rx, _) = b.app(&["t1"]);
        let (r, _) = b.req(app, &rx, json!({"type": "retention.get"}));
        assert_eq!(r["retention"], json!({"keep_days": 30, "keep_conversations": 200}));
        let ids: Vec<i64> = (0..60).map(|i| ended_conversation(&mut b, "claude", &format!("c{i}"), None)).collect();
        // 最早的 3 条改为 20 天前结束。
        let old = now_ms() - 20 * 86_400_000;
        b.core.store.exec(&format!("UPDATE conversation SET ended_at = {old} WHERE id IN ({}, {}, {})", ids[0], ids[1], ids[2]));
        drain(&rx);
        let (r, _) = b.req(app, &rx, json!({"type": "retention.set", "keep_days": 7, "keep_conversations": 100, "dry_run": true}));
        assert_eq!(r["count"], 3, "{r}");
        let (r, pushes) = b.req(app, &rx, json!({"type": "retention.set", "keep_days": 7, "keep_conversations": 50, "dry_run": true}));
        assert_eq!(r["count"], 10, "超期的 3 条与第 50 条之后的 7 条");
        assert!(pushes.is_empty());
        assert_eq!(b.count("SELECT COUNT(*) FROM conversation WHERE ended_at IS NOT NULL"), 60, "dry_run 不删");
        assert!(!b.dir.join("retention.json").exists(), "dry_run 不保存");
        for bad_req in [
            json!({"type": "retention.set", "keep_days": 8, "keep_conversations": 50}),
            json!({"type": "retention.set", "keep_days": 7, "keep_conversations": 1000}),
            json!({"type": "retention.set", "keep_days": 7}),
        ] {
            let (r, _) = b.req(app, &rx, bad_req.clone());
            assert_eq!(r["error"]["code"], "bad_request", "{bad_req}");
        }
        let (cli, crx, _) = b.connect("cli", PROTOCOL_VERSION);
        let (r, _) = b.req(cli, &crx, json!({"type": "retention.set", "keep_days": 7, "keep_conversations": 50}));
        assert_eq!(r["error"]["code"], "bad_request", "只有 app 能改");
        let (r, pushes) = b.req(app, &rx, json!({"type": "retention.set", "keep_days": 7, "keep_conversations": 50}));
        assert_eq!((r["count"].as_u64(), &r["retention"]), (Some(10), &json!({"keep_days": 7, "keep_conversations": 50})));
        let mut removed: Vec<i64> =
            pushes.iter().filter(|m| m["type"] == "conversation.remove").map(|m| m["conversation_id"].as_i64().unwrap()).collect();
        removed.sort();
        assert_eq!(removed, ids[..10].to_vec(), "删的是最旧的 10 条");
        assert_eq!(b.count("SELECT COUNT(*) FROM conversation WHERE ended_at IS NOT NULL"), 50);
        assert_eq!(Retention::load(&b.dir.join("retention.json")), Retention { keep_days: 7, keep_conversations: 50 });
        let (r, _) = b.req(cli, &crx, json!({"type": "retention.get"}));
        assert_eq!(r["retention"], json!({"keep_days": 7, "keep_conversations": 50}));
        // 之后的清理按新设置：再结束一条，最旧的一条随之清理。
        drain(&rx);
        ended_conversation(&mut b, "claude", "c60", None);
        let removed: Vec<i64> =
            drain(&rx).iter().filter(|m| m["type"] == "conversation.remove").map(|m| m["conversation_id"].as_i64().unwrap()).collect();
        assert_eq!(removed, vec![ids[10]]);
        assert_eq!(b.count("SELECT COUNT(*) FROM conversation WHERE ended_at IS NOT NULL"), 50);
    }

    /// 「清除记录」（M4a 第 3 块）：删除全部已结束的对话与其事件，正在运行的不动；绑在终端上的解除绑定；
    /// 逐条推送 `conversation.remove`；删后库文件与 WAL 里不再有提示词原文。
    #[test]
    fn history_clear_deletes_all_ended_and_their_text() {
        let mut b = bench("history-clear");
        let (app, rx, _) = b.app(&["t1", "t2"]);
        let a = ended_conversation(&mut b, "grok", "g1", None);
        let c = ended_conversation(&mut b, "kimi", "k1", None);
        b.hook("t2", "claude", json!({"hook_event_name": "SessionStart", "session_id": "live", "cwd": "/w/p"}));
        b.hook("t2", "claude", json!({"hook_event_name": "UserPromptSubmit", "session_id": "live", "prompt": "正在运行"}));
        let live = b.core.panes["t2"].conversation.unwrap();
        let bound = b.core.panes["t1"].conversation;
        // 之前保留清理（不开 secure_delete）删掉的对话，原文留在空闲页里，清除时只开 secure_delete 删不掉（2026-10-23 实测
        // 600 条夹具清除后库文件里仍有 95 处提示词原文）。造 90 条长提示词，按 50 条清理掉 40 条，再清除。
        let mut sql = String::from("BEGIN;");
        for i in 0..90 {
            sql += &format!(
                "INSERT INTO conversation (tool, native_id, started_at, last_event_at, last_prompt) \
                 VALUES ('codex', 'x{i}', 0, 0, '第一版提示词 #{i}：{}');",
                "重构侧栏".repeat(20)
            );
        }
        sql += &format!("UPDATE conversation SET ended_at = {} WHERE native_id LIKE 'x%'; COMMIT;", now_ms());
        b.core.store.exec(&sql);
        let (r, _) = b.req(app, &rx, json!({"type": "retention.set", "keep_days": 30, "keep_conversations": 50}));
        assert_eq!(r["count"], 42, "{r}");
        let x: Vec<i64> = b.core.store.ended_ids().unwrap();
        drain(&rx);
        let (r, _) = b.req(app, &rx, json!({"type": "history.clear", "dry_run": true}));
        assert_eq!(r["count"], 50, "{r}");
        assert_eq!(b.count("SELECT COUNT(*) FROM conversation WHERE ended_at IS NOT NULL"), 50, "dry_run 不删");
        let (cli, crx, _) = b.connect("cli", PROTOCOL_VERSION);
        let (r, _) = b.req(cli, &crx, json!({"type": "history.clear"}));
        assert_eq!(r["error"]["code"], "bad_request", "只有 app 能清除");
        let (r, pushes) = b.req(app, &rx, json!({"type": "history.clear"}));
        assert!(!x.contains(&a) && !x.contains(&c), "最早的两条已在保留清理中删掉");
        assert_eq!(r["count"], 50);
        let mut removed: Vec<i64> =
            pushes.iter().filter(|m| m["type"] == "conversation.remove").map(|m| m["conversation_id"].as_i64().unwrap()).collect();
        removed.sort();
        let mut want = x.clone();
        want.sort();
        assert_eq!(removed, want);
        assert!(pushes.iter().filter(|m| m["type"] == "conversation.remove").all(|m| m["recent_total"] == 0));
        assert_eq!(b.count("SELECT COUNT(*) FROM conversation"), 1);
        assert!(b.core.store.conversation(live).unwrap().is_some(), "正在运行的对话不受影响");
        assert_eq!(b.count(&format!("SELECT COUNT(*) FROM event WHERE conversation_id IN ({a}, {c})")), 0);
        if bound.is_some_and(|x| x == a || x == c) {
            assert_eq!(b.core.panes["t1"].conversation, None, "保留清理删掉的对话同样解除绑定");
        }
        for needle in ["修导航焦点样式", "第一版提示词"] {
            let needle = needle.as_bytes();
            for f in ["t.db", "t.db-wal"] {
                if let Ok(bytes) = std::fs::read(b.dir.join(f)) {
                    assert!(!bytes.windows(needle.len()).any(|w| w == needle), "{f} 里仍有提示词原文");
                }
            }
        }
        let (r, _) = b.req(app, &rx, json!({"type": "history.clear", "dry_run": true}));
        assert_eq!(r["count"], 0);
    }

    /// 任务行右键（docs/design.md 第 5 节「任务行右键」）：会话记录优先取仍存在的 transcript 文件；Grok、Kimi 没有时
    /// 按会话 ID 找会话目录；都没有为 null。恢复命令与目录同一键恢复。
    fn conflict_of(pushes: &[Value], terminal: &str) -> Option<Value> {
        pushes
            .iter()
            .filter(|m| m["type"] == "terminal.upsert" && m["terminal"]["terminal_id"] == terminal)
            .last()
            .map(|m| m["terminal"]["conflict_with"].clone())
    }

    /// 同一对话在两个终端同时打开：两边都标冲突；一方结束时冲突消失，对话不结束、改绑到仍在运行的一方。
    #[test]
    fn same_conversation_in_two_terminals_is_a_conflict() {
        let mut b = bench("conflict");
        let (app, rx, _) = b.app(&["t1", "t2"]);
        claude(&mut b, "t1", "SessionStart", "s1");
        b.hook("t1", "claude", json!({"hook_event_name": "UserPromptSubmit", "session_id": "s1", "prompt": "问"}));
        drain(&rx);
        claude(&mut b, "t2", "SessionStart", "s1");
        let pushes = drain(&rx);
        assert_eq!(conflict_of(&pushes, "t1"), Some(json!(["t2"])));
        assert_eq!(conflict_of(&pushes, "t2"), Some(json!(["t1"])));
        let cid = b.count("SELECT id FROM conversation WHERE native_id = 's1'");
        // t1 的 CLI 退出（SessionEnd）：t2 仍在运行，对话不结束、改绑到 t2。
        claude(&mut b, "t1", "SessionEnd", "s1");
        let pushes = drain(&rx);
        assert_eq!(conflict_of(&pushes, "t2"), Some(json!([])));
        assert_eq!(ended_state(&b, cid), (None, None, Some("t2".into())));
        // 再次冲突后 t2 的进程退出（没有 SessionEnd）：改绑到 t1。
        claude(&mut b, "t1", "SessionStart", "s1");
        b.core.panes.get_mut("t2").unwrap().exit_deadline = Some((Instant::now(), Tool::Claude));
        b.core.expire_exits(Instant::now());
        assert_eq!(ended_state(&b, cid), (None, None, Some("t1".into())));
        assert!(b.core.panes["t1"].conflict.is_empty());
        // 再次冲突后关掉 t1：改绑到 t2，不以 terminal_closed 结束。
        claude(&mut b, "t2", "SessionStart", "s1");
        assert_eq!(b.core.panes["t1"].conflict, vec!["t2".to_string()]);
        b.req(app, &rx, json!({"type": "terminal.exit", "terminal_id": "t1"}));
        assert_eq!(ended_state(&b, cid), (None, None, Some("t2".into())));
        assert!(b.core.panes["t2"].conflict.is_empty());
        // 只剩一个终端时照常结束。
        claude(&mut b, "t2", "SessionEnd", "s1");
        assert_eq!(ended_state(&b, cid).1.as_deref(), Some("session_end"));
    }

    /// Agent 一启动就读一次（2026-11-01）：另起命令查的额度不必等第一轮结束；读到的是否按量计费随用量推送。
    #[test]
    fn usage_is_read_at_session_start() {
        let mut b = bench("read-at-start");
        let (tx, rx_cmds) = mpsc::channel();
        b.exits = rx_cmds;
        b.core.titles = TitleWorker::with_reader(tx, |_| Found { api: Some(true), ..Found::default() });
        let (_app, rx, _) = b.app(&["t1"]);
        b.hook("t1", "kimi", json!({"hook_event_name":"SessionStart","session_id":"session_k","source":"startup"}));
        let cmd = b.next_cmd(|c| matches!(c, Cmd::BillingRead { .. }), "SessionStart 就应读一次");
        drain(&rx);
        b.core.handle(cmd);
        let ups: Vec<Value> = drain(&rx).into_iter().filter(|m| m["type"] == "usage.upsert").collect();
        assert_eq!(ups[0]["usage"]["billing"], "api");
    }

    /// 最后一条 AI 回复（M1 第 6 块）：一轮结束即请求读（标题来自 Hook 的 Kimi 也读），读到即写库、推送。
    #[test]
    fn last_reply_is_read_at_turn_end() {
        let mut b = bench("last-reply");
        let (tx, rx_cmds) = mpsc::channel();
        b.exits = rx_cmds;
        b.core.titles = TitleWorker::with_reader(tx, |j| Found { reply: Some(format!("主人，{} 完成", j.native_id)), ..Found::default() });
        let (_app, rx, _) = b.app(&["t1"]);
        b.hook("t1", "kimi", json!({"hook_event_name":"SessionStart","session_id":"session_k","source":"startup"}));
        b.hook("t1", "kimi", json!({"hook_event_name":"UserPromptSubmit","session_id":"session_k","prompt":"查一下","session_title":"查一下"}));
        b.hook("t1", "kimi", json!({"hook_event_name":"Stop","session_id":"session_k"}));
        let cmd = b.next_cmd(|c| matches!(c, Cmd::ReplyRead { .. }), "一轮结束应读最后一条回复");
        drain(&rx);
        b.core.handle(cmd);
        let pushes = drain(&rx);
        let conv = pushes.iter().find(|m| m["type"] == "conversation.upsert").expect("读到回复应推送对话");
        assert_eq!(conv["conversation"]["last_reply"], "主人，session_k 完成");
        assert!(conv["conversation"]["last_reply_at"].as_i64().is_some());
        // 中断也读（OpenCode 2 中断后不再补发空闲事件）。
        b.hook("t1", "opencode", json!({"hook_event_name":"session.created","sessionID":"ses_2","directory":"/w"}));
        b.hook("t1", "opencode", json!({"hook_event_name":"session.prompt","sessionID":"ses_2","prompt":"写一篇文章"}));
        b.hook("t1", "opencode", json!({"hook_event_name":"session.execution.interrupted","sessionID":"ses_2","reason":"user"}));
        // 前面 Kimi 那一轮的延迟重读可能先到，跳过。
        loop {
            if let Cmd::ReplyRead { reply, .. } = b.next_cmd(|c| matches!(c, Cmd::ReplyRead { .. }), "中断后应读最后一条回复") {
                if reply == "主人，ses_2 完成" {
                    break;
                }
            }
        }
    }

    /// 同一终端挂起 Claude、运行 Codex 提问后退出、再 fg 回 Claude 提问：终端认回 Claude 那段对话，
    /// 被 Codex 的 SessionStart 记为已替换的对话重新打开（docs/design.md 4.3 节，2026-10-15）。
    #[test]
    fn prompt_in_resumed_job_rebinds_terminal() {
        let mut b = bench("rebind-prompt");
        let (_app, _rx, _) = b.app(&["t1"]);
        claude(&mut b, "t1", "SessionStart", "c1");
        b.hook("t1", "claude", json!({"hook_event_name":"UserPromptSubmit","session_id":"c1","prompt":"今天星期几"}));
        b.hook("t1", "codex", json!({"hook_event_name":"SessionStart","session_id":"x1","transcript_path":"/x"}));
        b.hook("t1", "codex", json!({"hook_event_name":"UserPromptSubmit","session_id":"x1","prompt":"你好","transcript_path":"/x"}));
        b.hook("t1", "codex", json!({"hook_event_name":"SessionEnd","session_id":"x1","transcript_path":"/x"}));
        let c1 = b.count("SELECT id FROM conversation WHERE native_id = 'c1'");
        assert_eq!(ended_state(&b, c1).1.as_deref(), Some("replaced"));
        b.hook("t1", "claude", json!({"hook_event_name":"UserPromptSubmit","session_id":"c1","prompt":"金价"}));
        assert_eq!(ended_state(&b, c1), (None, None, Some("t1".into())), "认回并重新打开");
        assert_eq!(b.core.panes["t1"].conversation, Some(c1));
        assert_eq!(b.core.panes["t1"].state.state, AgentState::Running);
    }

    /// Hook 配置的状态、生成改动与修复（M1 第 7 块，docs/protocol.md 第 4、5 节）：快照带 `hooks`；指纹不符或文件
    /// 在生成改动后被改过都回 `conflict` 且不写；修复后推送 `hooks.upsert` 并留备份。
    #[test]
    fn hooks_plan_and_repair() {
        let mut b = bench("hooks");
        let settings = b.dir.join(".claude/settings.json");
        fs::create_dir_all(settings.parent().unwrap()).unwrap();
        fs::write(&settings, "{\n  \"theme\": \"dark\"\n}\n").unwrap();
        let (app, rx, snap) = b.app(&[]);
        let of = |v: &Value, tool: &str| v.as_array().unwrap().iter().find(|s| s["tool"] == tool).unwrap().clone();
        assert_eq!(snap["hooks"].as_array().unwrap().len(), 7);
        assert_eq!(of(&snap["hooks"], "claude")["state"], "missing");
        assert_eq!(of(&snap["hooks"], "claude")["fixable"], true);
        assert_eq!(of(&snap["hooks"], "kimi")["state"], "absent", "没装的不提示");
        let (reply, _) = b.req(app, &rx, json!({"type": "hooks.plan"}));
        let changes = reply["plan"]["changes"].as_array().unwrap().clone();
        assert_eq!(changes.len(), 1, "{reply}");
        assert_eq!(changes[0]["tool"], "claude");
        assert!(changes[0]["diff"].as_str().unwrap().contains("hook --config claude"));
        assert_eq!(changes[0]["files"][0]["backup"], true);
        let fp = reply["plan"]["fingerprint"].as_str().unwrap().to_owned();
        // 指纹不符：不写。
        let (r, _) = b.req(app, &rx, json!({"type": "hooks.repair", "tools": ["claude"], "fingerprint": "x"}));
        assert_eq!(r["error"]["code"], "conflict");
        assert_eq!(fs::read_to_string(&settings).unwrap(), "{\n  \"theme\": \"dark\"\n}\n");
        // 生成改动之后文件被改：不写。
        fs::write(&settings, "{\n  \"theme\": \"light\"\n}\n").unwrap();
        let (r, _) = b.req(app, &rx, json!({"type": "hooks.repair", "tools": ["claude"], "fingerprint": fp}));
        assert_eq!(r["error"]["code"], "conflict");
        assert_eq!(fs::read_to_string(&settings).unwrap(), "{\n  \"theme\": \"light\"\n}\n");
        // 重新生成后修复成功。
        let (reply, _) = b.req(app, &rx, json!({"type": "hooks.plan", "tools": ["claude"]}));
        let fp = reply["plan"]["fingerprint"].as_str().unwrap().to_owned();
        let (r, pushes) = b.req(app, &rx, json!({"type": "hooks.repair", "tools": ["claude"], "fingerprint": fp}));
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(of(&r["hooks"], "claude")["state"], "ok");
        assert_eq!(r["backups"].as_array().unwrap().len(), 1);
        let backup = r["backups"][0].as_str().unwrap();
        assert_eq!(fs::read_to_string(backup).unwrap(), "{\n  \"theme\": \"light\"\n}\n");
        let upsert = pushes.iter().find(|m| m["type"] == "hooks.upsert").expect("修复后推送 hooks.upsert");
        assert_eq!(of(&upsert["hooks"], "claude")["state"], "ok");
        assert!(fs::read_to_string(&settings).unwrap().starts_with("{\n  \"theme\": \"light\",\n  \"hooks\""), "保留原有键的顺序");
        // 已正常：没有改动；状态不变时不推送。
        let (reply, _) = b.req(app, &rx, json!({"type": "hooks.plan"}));
        assert_eq!(reply["plan"]["changes"], json!([]));
        b.core.handle(Cmd::HooksChanged { statuses: analyze_all(&b.dir.display().to_string()) });
        assert!(drain(&rx).iter().all(|m| m["type"] != "hooks.upsert"));
        // cli 角色也能查询；未知工具为 bad_request。
        let (cli, crx, _) = b.connect("cli", PROTOCOL_VERSION);
        let (r, _) = b.req(cli, &crx, json!({"type": "hooks.status"}));
        assert_eq!(of(&r["hooks"], "claude")["state"], "ok");
        let (r, _) = b.req(cli, &crx, json!({"type": "hooks.plan", "tools": ["vim"]}));
        assert_eq!(r["error"]["code"], "bad_request");
        let (r, _) = b.req(app, &rx, json!({"type": "hooks.plan", "action": "wipe"}));
        assert_eq!(r["error"]["code"], "bad_request");
        // 撤销（M4a）：缺省四款，没有本项目条目的不在结果里；只删本项目的条目，回到修复前的内容，写入前同样备份。
        let (reply, _) = b.req(app, &rx, json!({"type": "hooks.plan", "action": "uninstall"}));
        let changes = reply["plan"]["changes"].as_array().unwrap().clone();
        assert_eq!(changes.iter().map(|c| c["tool"].clone()).collect::<Vec<_>>(), vec![json!("claude")], "{reply}");
        assert_eq!(changes[0]["steps"][0]["kind"], "uninstall");
        let fp = reply["plan"]["fingerprint"].as_str().unwrap().to_owned();
        let (r, pushes) =
            b.req(app, &rx, json!({"type": "hooks.repair", "tools": ["claude"], "fingerprint": fp, "action": "uninstall"}));
        assert_eq!(r["ok"], true, "{r}");
        assert_eq!(fs::read_to_string(&settings).unwrap(), "{\n  \"theme\": \"light\"\n}\n");
        assert_eq!(r["backups"].as_array().unwrap().len(), 1);
        assert_eq!(of(&r["hooks"], "claude")["state"], "missing");
        assert!(pushes.iter().any(|m| m["type"] == "hooks.upsert"));
        // 测试后台没指定 CLI 配置目录：拒绝修复。
        b.core.real_config_guard = true;
        fs::write(&settings, "{}\n").unwrap();
        let (reply, _) = b.req(app, &rx, json!({"type": "hooks.plan", "tools": ["claude"]}));
        let fp = reply["plan"]["fingerprint"].as_str().unwrap().to_owned();
        let (r, _) = b.req(app, &rx, json!({"type": "hooks.repair", "tools": ["claude"], "fingerprint": fp}));
        assert_eq!(r["error"]["code"], "bad_request");
        assert_eq!(fs::read_to_string(&settings).unwrap(), "{}\n");
    }

    /// 前台 CLI 的版本（M1 第 7 块）：只对读到它的那个进程有效，推送在终端前台信息里，对话绑定时写入 `cli_version`
    /// （只写第一次）。
    #[test]
    fn cli_version_on_terminal_and_conversation() {
        let mut b = bench("version");
        let t = "b0b0b0b0-0000-4000-8000-000000000001";
        let (_app, rx, _) = b.app(&[t]);
        b.core.panes.get_mut(t).unwrap().foreground =
            Foreground::Agent { pid: 300, path: "claude.exe".into(), tool: Tool::Claude };
        b.core.handle(Cmd::VersionRead { terminal_id: t.into(), pid: 999, version: Some("2.1.296".into()) });
        assert!(b.core.panes[t].version.is_none(), "不是当前前台进程的结果丢弃");
        drain(&rx);
        b.core.handle(Cmd::VersionRead { terminal_id: t.into(), pid: 300, version: Some("2.1.296".into()) });
        let fg = drain(&rx).iter().rev().find(|m| m["type"] == "terminal.upsert").unwrap()["terminal"]["foreground"].clone();
        assert_eq!((fg["version"].clone(), fg["version_verified"].clone()), (json!("2.1.296"), json!(true)));
        b.hook(t, "claude", json!({"hook_event_name": "SessionStart", "session_id": "v1", "transcript_path": "/x.jsonl"}));
        assert_eq!(b.count("SELECT COUNT(*) FROM conversation WHERE native_id = 'v1' AND cli_version = '2.1.296'"), 1);
        // 换成未验证的新版本进程：前台显示新版本，对话的版本不改。
        b.core.panes.get_mut(t).unwrap().foreground =
            Foreground::Agent { pid: 301, path: "claude.exe".into(), tool: Tool::Claude };
        drain(&rx);
        b.core.handle(Cmd::VersionRead { terminal_id: t.into(), pid: 301, version: Some("9.9.9".into()) });
        let fg = drain(&rx).iter().rev().find(|m| m["type"] == "terminal.upsert").unwrap()["terminal"]["foreground"].clone();
        assert_eq!((fg["version"].clone(), fg["version_verified"].clone()), (json!("9.9.9"), json!(false)));
        assert_eq!(b.count("SELECT COUNT(*) FROM conversation WHERE native_id = 'v1' AND cli_version = '2.1.296'"), 1);
        // 版本未知（如运行中被升级）：两项都是 null。
        b.core.handle(Cmd::VersionRead { terminal_id: t.into(), pid: 301, version: None });
        let fg = drain(&rx).iter().rev().find(|m| m["type"] == "terminal.upsert").unwrap()["terminal"]["foreground"].clone();
        assert_eq!((fg["version"].clone(), fg["version_verified"].clone()), (Value::Null, Value::Null));
    }

    /// 「状态可能不准」（M4b 块 6）：Hook 载荷异常记到终端、推送在前台信息里，只对那个进程有效；只是版本不在清单里时没有。
    /// `hooks.status` 每款附版本：正在运行的优先，没在运行时取最近一次对话记下的。
    #[test]
    fn anomaly_on_terminal_and_versions_in_hook_status() {
        let mut b = bench("anomaly");
        let t = "b0b0b0b0-0000-4000-8000-000000000002";
        let (_app, rx, _) = b.app(&[t]);
        b.core.panes.get_mut(t).unwrap().foreground =
            Foreground::Agent { pid: 300, path: "codex".into(), tool: Tool::Codex };
        b.core.handle(Cmd::VersionRead { terminal_id: t.into(), pid: 300, version: Some("9.9.9".into()) });
        let fg = |rx: &Receiver<Out>| {
            drain(rx).iter().rev().find(|m| m["type"] == "terminal.upsert").unwrap()["terminal"]["foreground"].clone()
        };
        assert_eq!((fg(&rx)["version_verified"].clone(), fg_anomaly(&b, t)), (json!(false), Value::Null), "只是未验证不算");
        b.hook(t, "codex", json!({"hook_event_name": "SessionStart", "session_id": "a1", "transcript_path": "/x.jsonl"}));
        assert_eq!(fg_anomaly(&b, t), Value::Null, "正常事件不算");
        drain(&rx);
        b.hook(t, "codex", json!({"hook_event_name": "SessionBegin", "session_id": "a1"}));
        assert_eq!(fg(&rx)["anomaly"], "unknown_event:SessionBegin");
        b.hook(t, "codex", Value::Null);
        assert_eq!(fg(&rx)["anomaly"], "unparsable");
        let codex = |b: &Bench| b.core.hooks_json().as_array().unwrap().iter().find(|h| h["tool"] == "codex").unwrap().clone();
        let h = codex(&b);
        assert_eq!((h["version"].clone(), h["version_verified"].clone(), h["version_running"].clone()), (json!("9.9.9"), json!(false), json!(true)));
        // 换了进程：异常不再显示；版本退回最近一次对话记下的。
        b.core.panes.get_mut(t).unwrap().foreground =
            Foreground::Agent { pid: 301, path: "codex".into(), tool: Tool::Codex };
        assert_eq!(fg_anomaly(&b, t), Value::Null);
        let h = codex(&b);
        assert_eq!((h["version"].clone(), h["version_running"].clone()), (json!("9.9.9"), json!(false)));
        // 从没运行过的工具不附版本；每款都附清单里最新的已验证版本。
        let hooks = b.core.hooks_json();
        assert!(hooks.as_array().unwrap().iter().any(|h| h["tool"] == "kimi" && h.get("version").is_none()));
        assert_eq!(codex(&b)["verified_latest"], json!(verified_list(Tool::Codex).last().unwrap()));
    }

    /// `hooks.status` 附主配置文件的修改时间（2026-11-17，引导用户信任时与 Codex 的启动时间比较）：文件不存在时不附，
    /// 写入后是它此刻的修改时间，每次现读。
    #[test]
    fn hook_status_carries_config_change_time() {
        let mut b = bench("hook-changed-at");
        let home = test_dir("hook-changed-at-home");
        b.core.cli_home = home.display().to_string();
        let codex = |b: &Bench| b.core.hooks_json().as_array().unwrap().iter().find(|h| h["tool"] == "codex").unwrap().clone();
        assert!(codex(&b).get("changed_at").is_none(), "没有配置文件时不附");
        let file = home.join(".codex/hooks.json");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "{\"hooks\": {}}").unwrap();
        let mtime = |f: &Path| f.metadata().unwrap().modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
        let h = codex(&b);
        assert_eq!(h["path"], json!(file.display().to_string()));
        assert_eq!(h["changed_at"], json!(mtime(&file)));
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&file, "{\"hooks\": {\"Stop\": []}}").unwrap();
        let later = codex(&b)["changed_at"].as_i64().unwrap();
        assert!(later > h["changed_at"].as_i64().unwrap() && later == mtime(&file), "改动后是新的修改时间");
    }

    fn fg_anomaly(b: &Bench, t: &str) -> Value {
        b.core.terminal_json(t).ok().flatten().unwrap()["foreground"]["anomaly"].clone()
    }

    /// 「会话是否真的在同步」的测试用：一款的配置状态；前台设为本测试进程（启动时间读得到）。
    fn hook_status(tool: Tool, state: HookState) -> Vec<Status> {
        vec![Status { tool, state, path: String::new(), problems: Vec::new(), fixable: false }]
    }

    fn agent_fg(b: &mut Bench, t: &str, tool: Tool) -> i64 {
        let pid = std::process::id() as i32;
        b.core.panes.get_mut(t).unwrap().foreground = Foreground::Agent { pid, path: tool.as_str().into(), tool };
        procinfo::start_ms(pid).expect("读得到本进程的启动时间")
    }

    fn term(b: &Bench, t: &str) -> Value {
        b.core.terminal_json(t).ok().flatten().unwrap()
    }

    /// 运行中重读的工具（Claude）：配置一失效，这个会话就没在同步——状态立即置为「未知」、`sync` 为 `off` 并推送，
    /// 不停在最后一次事件的 Running（2026-11-20，用户撤销后看到状态像卡住）。之后又收到它的事件，按事实算还在同步。
    #[test]
    fn live_tool_goes_unknown_at_once_when_its_hooks_stop_working() {
        let mut b = bench("sync-live");
        let t = "b0b0b0b0-0000-4000-8000-0000000000a1";
        let (_app, rx, _) = b.app(&[t]);
        let started = agent_fg(&mut b, t, Tool::Claude);
        b.core.hook_times.observe(Tool::Claude, HookState::Ok, started - 10_000, false);
        b.core.hooks_changed(hook_status(Tool::Claude, HookState::Ok));
        b.hook(t, "claude", json!({"hook_event_name": "SessionStart", "session_id": "s1", "transcript_path": "/x.jsonl"}));
        b.hook(t, "claude", json!({"hook_event_name": "UserPromptSubmit", "session_id": "s1", "prompt": "hi"}));
        let v = term(&b, t);
        assert_eq!((v["agent"]["state"].clone(), v["foreground"].get("sync").cloned()), (json!("running"), None));
        drain(&rx);
        b.core.hooks_changed(hook_status(Tool::Claude, HookState::Missing));
        let pushed = drain(&rx).iter().rev().find(|m| m["type"] == "terminal.upsert").expect("配置失效要推送终端")["terminal"].clone();
        assert_eq!((pushed["agent"]["state"].clone(), pushed["foreground"]["sync"].clone()), (json!("unknown"), json!("off")));
        assert_eq!(pushed["conversation_id"], term(&b, t)["conversation_id"], "绑定的对话不动");
        assert!(pushed["conversation_id"].is_i64());
        // 失效之后还收到它的事件：它在用旧条目同步，状态照常更新。
        std::thread::sleep(Duration::from_millis(3));
        b.hook(t, "claude", json!({"hook_event_name": "UserPromptSubmit", "session_id": "s1", "prompt": "again"}));
        let v = term(&b, t);
        assert_eq!((v["agent"]["state"].clone(), v["foreground"]["sync"].clone()), (json!("running"), json!("old")));
        // 修复后恢复正常。
        b.core.hooks_changed(hook_status(Tool::Claude, HookState::Ok));
        assert!(term(&b, t)["foreground"].get("sync").is_none());
    }

    /// 会话开始时读、进程启动时读的工具：进程启动时条目还没写入，之后修复——配置正常，但这个进程读不到，
    /// `sync` 为 `new_session`（Codex）或 `restart`（Kimi），并给出 `entries_at_start: false`；收到它的事件后不再提示。
    #[test]
    fn a_session_older_than_the_entries_is_told_to_reopen() {
        let mut b = bench("sync-reopen");
        let (c, k) = ("b0b0b0b0-0000-4000-8000-0000000000b1", "b0b0b0b0-0000-4000-8000-0000000000b2");
        let (_app, rx, _) = b.app(&[c, k]);
        let started = agent_fg(&mut b, c, Tool::Codex);
        agent_fg(&mut b, k, Tool::Kimi);
        for tool in [Tool::Codex, Tool::Kimi] {
            b.core.hook_times.observe(tool, HookState::Missing, started - 10_000, false);
        }
        let mut all = hook_status(Tool::Codex, HookState::Missing);
        all.extend(hook_status(Tool::Kimi, HookState::Missing));
        b.core.hooks_changed(all);
        assert_eq!(term(&b, c)["foreground"]["sync"], "off", "还没修复：没在同步");
        drain(&rx);
        let mut all = hook_status(Tool::Codex, HookState::Ok);
        all.extend(hook_status(Tool::Kimi, HookState::Ok));
        b.core.hooks_changed(all);
        assert_eq!(drain(&rx).iter().filter(|m| m["type"] == "terminal.upsert").count(), 2, "两个终端都推送");
        let (vc, vk) = (term(&b, c), term(&b, k));
        assert_eq!((vc["foreground"]["sync"].clone(), vc["foreground"]["entries_at_start"].clone()), (json!("new_session"), json!(false)));
        assert_eq!(vk["foreground"]["sync"], "restart");
        // Codex 里 /new 之后提交了一条：收到事件，不再提示。
        std::thread::sleep(Duration::from_millis(3));
        b.hook(c, "codex", json!({"hook_event_name": "SessionStart", "session_id": "n1", "transcript_path": "/x.jsonl"}));
        assert!(term(&b, c)["foreground"].get("sync").is_none());
        assert_eq!(term(&b, k)["foreground"]["sync"], "restart", "别的终端不受影响");
    }

    /// 会话开始时读的工具，会话中途条目没了：这个进程还在用启动时读到的条目同步，`sync` 为 `old`，状态不动；
    /// 失效之后才启动的进程是 `off`。
    #[test]
    fn a_session_that_loaded_the_entries_is_still_synced_after_removal() {
        let mut b = bench("sync-old");
        let t = "b0b0b0b0-0000-4000-8000-0000000000c1";
        let (_app, rx, _) = b.app(&[t]);
        let started = agent_fg(&mut b, t, Tool::Codex);
        b.core.hook_times.observe(Tool::Codex, HookState::Ok, started - 10_000, false);
        b.core.hooks_changed(hook_status(Tool::Codex, HookState::Ok));
        b.hook(t, "codex", json!({"hook_event_name": "SessionStart", "session_id": "o1", "transcript_path": "/x.jsonl"}));
        b.hook(t, "codex", json!({"hook_event_name": "UserPromptSubmit", "session_id": "o1", "prompt": "hi"}));
        drain(&rx);
        b.core.hooks_changed(hook_status(Tool::Codex, HookState::Missing));
        let v = term(&b, t);
        assert_eq!((v["agent"]["state"].clone(), v["foreground"]["sync"].clone()), (json!("running"), json!("old")));
        assert!(drain(&rx).iter().any(|m| m["type"] == "terminal.upsert"), "变成 old 也推送");
        // 时间线上这个进程启动时就已失效（失效在前、启动在后）：没在同步。
        let mut b = bench("sync-old-2");
        let (_app, _rx, _) = b.app(&[t]);
        let started = agent_fg(&mut b, t, Tool::Codex);
        b.core.hook_times.observe(Tool::Codex, HookState::Ok, started - 20_000, false);
        b.core.hook_times.observe(Tool::Codex, HookState::Missing, started - 10_000, false);
        b.core.hooks_changed(hook_status(Tool::Codex, HookState::Missing));
        assert_eq!(term(&b, t)["foreground"]["sync"], "off");
    }

    /// 会话记录对账（2026-11-21 正式版实测的缺陷）：撤销后被算作「还在用内存里的条目同步」的会话，会话记录里出现它
    /// 提交的提示词而事件没到，改判没在同步、状态置为未知；修复回去后它仍不发事件，提示开新会话；收到它的事件后恢复。
    #[test]
    fn a_prompt_in_the_record_without_its_event_flips_an_inferred_session() {
        let mut b = bench("sync-reconcile");
        let t = "b0b0b0b0-0000-4000-8000-0000000000d1";
        let (_app, rx, _) = b.app(&[t]);
        let key = format!("{t}#sync");
        let started = agent_fg(&mut b, t, Tool::Codex);
        b.core.hook_times.observe(Tool::Codex, HookState::Ok, started - 10_000, false);
        b.core.hooks_changed(hook_status(Tool::Codex, HookState::Ok));
        b.hook(t, "codex", json!({"hook_event_name": "SessionStart", "session_id": "01a1fbb3-f403-7d81-b0cb-c3db8bff8109", "transcript_path": "/x.jsonl"}));
        b.hook(t, "codex", json!({"hook_event_name": "UserPromptSubmit", "session_id": "01a1fbb3-f403-7d81-b0cb-c3db8bff8109", "prompt": "hi"}));
        b.hook(t, "codex", json!({"hook_event_name": "Stop", "session_id": "01a1fbb3-f403-7d81-b0cb-c3db8bff8109"}));
        b.core.refresh_watches();
        assert!(!b.core.watching.contains_key(&key), "配置没变过的会话不核对");
        let sid = Some("01a1fbb3-f403-7d81-b0cb-c3db8bff8109".to_owned());
        // 撤销：算作还在用内存里的条目同步，开始盯它的会话记录。
        b.core.hooks_changed(hook_status(Tool::Codex, HookState::Missing));
        b.core.refresh_watches();
        assert_eq!(term(&b, t)["foreground"]["sync"], "old");
        assert!(b.core.watching.contains_key(&key), "靠推断的会话要核对");
        // 记录里的提示词，它的事件到了（提交事件早到几毫秒）：照旧。
        b.hook(t, "codex", json!({"hook_event_name": "UserPromptSubmit", "session_id": "01a1fbb3-f403-7d81-b0cb-c3db8bff8109", "prompt": "还在"}));
        b.core.record_submit(t, sid.clone(), Some(now_ms()));
        assert!(b.core.panes[t].submit_check.is_some());
        b.core.expire_submits(Instant::now());
        assert!(b.core.panes[t].submit_check.is_some(), "没到时间不下结论");
        b.core.expire_submits(Instant::now() + Duration::from_secs(6));
        assert_eq!(term(&b, t)["foreground"]["sync"], "old");
        b.hook(t, "codex", json!({"hook_event_name": "Stop", "session_id": "01a1fbb3-f403-7d81-b0cb-c3db8bff8109"}));
        assert_eq!(term(&b, t)["agent"]["state"], "ready");
        // 之后它不再发事件（最近一次事件是很久以前的）：记录里又有一条提示词，事件没到。
        b.core.panes.get_mut(t).unwrap().last_hook = Some((Tool::Codex, now_ms() - 60_000));
        drain(&rx);
        b.core.record_submit(t, Some("别的会话".into()), None);
        assert!(b.core.panes[t].submit_check.is_none(), "不是这个终端当前会话的记录不理会");
        b.core.record_submit(t, sid.clone(), None);
        b.core.expire_submits(Instant::now() + Duration::from_secs(6));
        let pushed = drain(&rx).iter().rev().find(|m| m["type"] == "terminal.upsert").expect("改判要推送终端")["terminal"].clone();
        assert_eq!((pushed["agent"]["state"].clone(), pushed["foreground"]["sync"].clone()), (json!("unknown"), json!("off")));
        assert!(!b.core.watching.contains_key(&key), "已经得出没在同步，不用再盯");
        // 换版本接管：结论随接管文件带到新进程（同一个进程才带）。
        let saved = b.core.agent_handoff(t).expect("带状态");
        assert!(saved["silent"].is_i64());
        b.core.panes.get_mut(t).unwrap().silent = None;
        b.core.restore_agent(t, &saved);
        assert!(b.core.panes[t].silent.is_some(), "同一个进程：带过来");
        // 修复回去，它仍不发事件：配置正常，但要开新会话。
        b.core.hooks_changed(hook_status(Tool::Codex, HookState::Ok));
        assert_eq!(term(&b, t)["foreground"]["sync"], "new_session");
        // /new 之后提交：收到它的事件，恢复同步，不再提示；之后配置没再变，不用盯。
        std::thread::sleep(Duration::from_millis(3));
        b.hook(t, "codex", json!({"hook_event_name": "SessionStart", "session_id": "01a1fbb3-f403-7d81-b0cb-c3db8bff8110", "transcript_path": "/y.jsonl"}));
        assert!(term(&b, t)["foreground"].get("sync").is_none());
        b.core.refresh_watches();
        assert!(!b.core.watching.contains_key(&key));
        // 运行中重读的工具不核对。
        let c = "b0b0b0b0-0000-4000-8000-0000000000d2";
        let mut b = bench("sync-reconcile-live");
        let (_app, _rx, _) = b.app(&[c]);
        let started = agent_fg(&mut b, c, Tool::Claude);
        b.core.hook_times.observe(Tool::Claude, HookState::Ok, started - 10_000, false);
        b.core.hooks_changed(hook_status(Tool::Claude, HookState::Ok));
        b.hook(c, "claude", json!({"hook_event_name": "SessionStart", "session_id": "s1", "transcript_path": "/x.jsonl"}));
        b.core.hooks_changed(hook_status(Tool::Claude, HookState::Missing));
        b.core.refresh_watches();
        assert!(!b.core.watching.contains_key(&format!("{c}#sync")));
    }

    /// `hooks.status` 每款附「何时读取 Hook」与引导里要写的命令；没有的命令不附。
    #[test]
    fn hook_status_carries_load_class_and_guide_commands() {
        let b = bench("hook-load");
        let hooks = b.core.hooks_json();
        let of = |tool: &str| hooks.as_array().unwrap().iter().find(|h| h["tool"] == tool).unwrap().clone();
        for (tool, load) in [("claude", "live"), ("opencode", "live"), ("antigravity", "live"), ("codex", "session"), ("grok", "session"), ("copilot", "session"), ("kimi", "process")] {
            assert_eq!(of(tool)["load"], load, "{tool}");
        }
        let codex = of("codex");
        assert_eq!((codex["new_session"].clone(), codex["quit"].clone(), codex["resume_last"].clone()), (json!("/new"), json!("/quit"), json!("codex resume --last")));
        let kimi = of("kimi");
        assert!(kimi.get("new_session").is_none(), "Kimi 同一进程里开新会话没用，不给这条");
        assert_eq!((kimi["quit"].clone(), kimi["resume_last"].clone()), (json!("/exit"), json!("kimi -c")));
        assert!(of("claude").get("quit").is_none(), "运行中重读的不需要引导");
    }
}
