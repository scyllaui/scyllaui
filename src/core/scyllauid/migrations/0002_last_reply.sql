-- M1 第 6 块（2026-10-14 用户确认）：「最近任务」以最后一条 AI 回复为副标题，右侧显示回复时间（docs/design.md 第 5 节）。
-- last_reply：后台从会话记录读到的最后一条 AI 回复，截前 200 个字符；last_reply_at：最后一轮结束（Hook）的时间。
ALTER TABLE conversation ADD COLUMN last_reply TEXT;
ALTER TABLE conversation ADD COLUMN last_reply_at INTEGER;
