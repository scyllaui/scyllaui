-- scyllauid 数据库第 1 版（docs/design.md 4.6 节，docs/protocol.md 第 8 节）。
-- 只存元数据，不存终端正文。时间一律为 Unix 毫秒。已发布的脚本不再修改，改表结构一律追加新脚本。

CREATE TABLE project (
  id INTEGER PRIMARY KEY,
  dir TEXT NOT NULL UNIQUE,                    -- 规范化后的绝对路径
  name TEXT NOT NULL,
  sort_order INTEGER NOT NULL,
  created_at INTEGER NOT NULL
);

CREATE TABLE terminal (
  id TEXT PRIMARY KEY,                         -- = SCYLLAUI_PANE_ID，App 生成的 UUID
  project_id INTEGER NOT NULL REFERENCES project(id),
  cwd TEXT NOT NULL,                           -- 启动目录
  name TEXT,                                   -- 用户命名，空 = 自动
  status TEXT NOT NULL CHECK (status IN ('alive', 'closed')),
  exit_code INTEGER,
  close_reason TEXT,                           -- exit | app_exit | missing_on_sync
  created_at INTEGER NOT NULL,
  closed_at INTEGER
);

-- 对话是独立记录，不挂在某个终端下面；按「工具 + 原生会话 ID」唯一。
CREATE TABLE conversation (
  id INTEGER PRIMARY KEY,
  tool TEXT NOT NULL,
  native_id TEXT NOT NULL,
  project_id INTEGER REFERENCES project(id) ON DELETE SET NULL,
  terminal_id TEXT REFERENCES terminal(id) ON DELETE SET NULL,      -- 当前或最后所在终端
  resumed_from INTEGER REFERENCES conversation(id) ON DELETE SET NULL,
  title TEXT,
  title_source TEXT,
  last_prompt TEXT,
  cwd TEXT,
  transcript_path TEXT,
  cli_version TEXT,
  started_at INTEGER NOT NULL,
  last_event_at INTEGER NOT NULL,
  ended_at INTEGER,
  end_reason TEXT,                             -- session_end | replaced | terminal_closed
  UNIQUE (tool, native_id)
);
CREATE INDEX conversation_recent ON conversation (project_id, ended_at DESC, id DESC)
  WHERE ended_at IS NOT NULL;
CREATE INDEX conversation_terminal ON conversation (terminal_id);

CREATE TABLE event (
  id INTEGER PRIMARY KEY,
  conversation_id INTEGER REFERENCES conversation(id) ON DELETE CASCADE,
  terminal_id TEXT,
  kind TEXT NOT NULL,                          -- 规范化后的事件名
  detail TEXT,                                 -- 可空 JSON，如 {"notification_type": "permission_prompt"}
  at INTEGER NOT NULL
);
CREATE INDEX event_conversation ON event (conversation_id, id);
