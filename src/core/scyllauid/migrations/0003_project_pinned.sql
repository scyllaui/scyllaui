-- 维护期 0.3.5（2026-10-23 用户确认）：「当前任务」里的项目组可以置顶，置顶的组排在最前，其余顺序不变（docs/design.md 第 5 节「项目置顶」）。
-- pinned：1 = 置顶，0 = 未置顶。只影响「当前任务」的分组顺序，不改 sort_order，不影响「最近任务」。
ALTER TABLE project ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1));
