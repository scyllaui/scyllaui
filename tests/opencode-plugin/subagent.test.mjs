// ScyllaUI 的 OpenCode 插件（src/core/proto/src/opencode-plugin.js）在 OpenCode 2 下的子会话标记。运行：
//   node --test tests/opencode-plugin/subagent.test.mjs
// 用假的 OpenCode 上下文驱动插件的 `setup`，`SCYLLAUI_BIN` 换成把载荷追加到文件的小脚本，核对转发出去的每一行。
// 回归（2026-11-13 长跑缺陷 D）：OpenCode 2.2.5 / 2.3.0 建好子会话后立刻提交任务，提示词钩子先于（或根本没有）
// `session.created` 到达插件，子会话没被标记，后台把终端换绑到子会话，主会话的完成事件被当作其他会话丢掉。
// 事件顺序按长跑库里的记录（终端 4a38b72f 事件 12907—12910、终端 6f2c70a3 事件 13220—13227）与 OpenCode 的会话库。
import { test } from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, writeFileSync, readFileSync, existsSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const dir = mkdtempSync(join(tmpdir(), "scyllaui-plugin-"));
const out = join(dir, "frames.jsonl");
const bin = join(dir, "fake-scyllaui");
writeFileSync(bin, `#!/bin/sh\ncat >> "${out}"\necho >> "${out}"\n`);
chmodSync(bin, 0o755);
process.env.SCYLLAUI_SOCK = join(dir, "sock");
process.env.SCYLLAUI_BIN = bin;
const plugin = (await import("../../src/core/proto/src/opencode-plugin.js")).default;

// 假上下文：事件由测试推入；`session.get` 按表回答父会话（`get` 为 null 时模拟没有这个接口）。
function fakeContext(parents, get) {
  const pending = [];
  let wake = null;
  let prompt = null;
  const ctx = {
    location: { directory: "/w" },
    event: {
      async *subscribe({ signal }) {
        while (!signal.aborted) {
          if (pending.length === 0) await new Promise((r) => (wake = r));
          while (pending.length) yield pending.shift();
        }
      },
    },
    session: {
      hook: async (name, fn) => { if (name === "prompt") prompt = fn; },
      get: get === undefined ? async ({ sessionID }) => ({ id: sessionID, parentID: parents[sessionID] }) : get,
    },
  };
  const emit = (type, data) => { pending.push({ type, data }); wake?.(); };
  const submit = (sessionID, text) => prompt({ sessionID, prompt: { text } });
  return { ctx, emit, submit };
}

async function frames(n) {
  for (let i = 0; i < 200; i++) {
    const rows = existsSync(out) ? readFileSync(out, "utf8").split("\n").filter(Boolean).map((l) => JSON.parse(l)) : [];
    if (rows.length >= n) return rows;
    await new Promise((r) => setTimeout(r, 20));
  }
  assert.fail(`等不到 ${n} 条转发`);
}

const tick = () => new Promise((r) => setImmediate(r));

test("子会话不先发 session.created 也标为子代理，主会话照常", async () => {
  writeFileSync(out, "");
  const { ctx, emit, submit } = fakeContext({ ses_child: "ses_parent" });
  const stop = await plugin.setup(ctx);
  submit("ses_parent", "Use a subagent to list the files");
  emit("session.execution.started", { sessionID: "ses_parent" });
  await tick();
  submit("ses_child", "You are a subagent spawned by another session.\nList files");
  emit("session.execution.started", { sessionID: "ses_child" });
  emit("session.execution.succeeded", { sessionID: "ses_child" });
  emit("session.execution.succeeded", { sessionID: "ses_parent" });
  const rows = await frames(6);
  stop();
  // 只保证同一会话内的先后（查父会话时只有那个会话的事件等）。
  const of = (id) => rows.filter((r) => r.sessionID === id).map((r) => [r.hook_event_name, r.scyllaui_subagent === true]);
  assert.deepEqual(of("ses_parent"), [
    ["session.prompt", false], ["session.execution.started", false], ["session.execution.succeeded", false],
  ]);
  assert.deepEqual(of("ses_child"), [
    ["session.prompt", true], ["session.execution.started", true], ["session.execution.succeeded", true],
  ]);
  assert.ok(rows.every((r) => r.directory === "/w"));
});

test("session.created 带 parentID 时直接标记（原有路径）", async () => {
  writeFileSync(out, "");
  const { ctx, emit } = fakeContext({});
  const stop = await plugin.setup(ctx);
  emit("session.created", { sessionID: "ses_c2", parentID: "ses_p2", title: "t" });
  emit("session.execution.started", { sessionID: "ses_c2" });
  const rows = await frames(2);
  stop();
  assert.deepEqual(rows.map((r) => r.scyllaui_subagent === true), [true, true]);
});

test("查父会话出错或超时不卡住转发，按不是子会话处理", async () => {
  writeFileSync(out, "");
  const { ctx, submit } = fakeContext({}, async () => { throw new Error("no such api"); });
  const stop = await plugin.setup(ctx);
  submit("ses_a", "hi");
  const slow = fakeContext({}, () => new Promise(() => {}));
  const stop2 = await plugin.setup(slow.ctx);
  slow.submit("ses_b", "hi");
  const rows = await frames(2);
  stop();
  stop2();
  assert.deepEqual(rows.map((r) => [r.sessionID, r.scyllaui_subagent === true]).sort(), [["ses_a", false], ["ses_b", false]]);
});

test("查父会话超时：先按主会话转发，晚到的结果对之后的事件生效（评审第 3 条）", async () => {
  writeFileSync(out, "");
  let answer;
  const late = new Promise((r) => (answer = r));
  const { ctx, submit, emit } = fakeContext({}, () => late);
  const stop = await plugin.setup(ctx);
  submit("ses_c", "You are a subagent spawned by another session.");
  emit("session.execution.started", { sessionID: "ses_c" });
  await frames(2);
  answer({ id: "ses_c", parentID: "ses_p" });
  await tick();
  emit("permission.asked", { sessionID: "ses_c", id: "per_1" });
  const rows = await frames(3);
  stop();
  assert.deepEqual(rows.map((r) => r.scyllaui_subagent === true), [false, false, true]);
});

test("查父会话出错：下一个事件再查（评审第 3 条）", async () => {
  writeFileSync(out, "");
  let calls = 0;
  const { ctx, submit, emit } = fakeContext({}, async ({ sessionID }) => {
    calls += 1;
    if (calls === 1) throw new Error("busy");
    return { id: sessionID, parentID: "ses_p" };
  });
  const stop = await plugin.setup(ctx);
  submit("ses_c", "task");
  await frames(1);
  emit("session.execution.started", { sessionID: "ses_c" });
  const rows = await frames(2);
  stop();
  assert.deepEqual(rows.map((r) => r.scyllaui_subagent === true), [false, true]);
  assert.equal(calls, 2);
});

test("一个会话查得慢不拖住其他会话（评审第 3 条）", async () => {
  writeFileSync(out, "");
  const { ctx, submit } = fakeContext({}, ({ sessionID }) =>
    sessionID === "ses_slow" ? new Promise(() => {}) : Promise.resolve({ id: sessionID }));
  const stop = await plugin.setup(ctx);
  const t0 = Date.now();
  submit("ses_slow", "a");
  submit("ses_fast", "b");
  const first = await frames(1);
  const firstAt = Date.now() - t0;
  const rows = await frames(2);
  stop();
  assert.equal(first[0].sessionID, "ses_fast");
  assert.ok(firstAt < 450, `快的会话不等慢的：${firstAt} ms`);
  assert.equal(rows[1].sessionID, "ses_slow");
});
