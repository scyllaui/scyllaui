// 由 ScyllaUI 生成，把 OpenCode 的事件交给 ScyllaUI 侧栏；改动会在 ScyllaUI「修复」时被覆盖，「撤销」时删除本文件。
// 不在 ScyllaUI 的终端里（没有 SCYLLAUI_SOCK）时什么也不做。只转发事件、不判断状态，规则在 ScyllaUI 的 OpenCode 适配器里
// （ScyllaUI 源码 docs/design.md 3.1 节「OpenCode 插件」）。同一个文件兼容两版：OpenCode 2 调 `setup`，OpenCode 1
// （1.18.29 起认对象形式的入口）调 `server`。
import { spawn } from "node:child_process";

// OpenCode 1 的事件。
const FORWARD_V1 = new Set([
  "session.created", "session.updated", "session.status", "session.idle", "session.error",
  "permission.asked", "permission.updated", "permission.replied",
  "question.asked", "question.replied", "question.rejected",
]);

// OpenCode 2 的事件（提示词另由 `prompt` 钩子转为 `session.prompt`）。
const FORWARD_V2 = [
  "session.created", "session.renamed",
  "session.execution.started", "session.execution.succeeded", "session.execution.failed", "session.execution.interrupted",
  "permission.asked", "permission.replied", "form.created", "form.replied", "form.cancelled",
];

// 查父会话最多等这么久（毫秒）：超时先按不是子会话转发，查询照常进行，晚到的结果对这个会话之后的事件生效；
// 同一会话只等一次。出错时下一个事件再查，最多查这么多次。
const PARENT_TIMEOUT_MS = 500;
const PARENT_TRIES = 3;

// 一个事件起一次 `scyllaui hook`；排队逐个起，后台收到的顺序与事件顺序一致。OpenCode 不等它。不在 ScyllaUI 的终端里为 null。
// `parentOf`（OpenCode 2）按会话 ID 查父会话：子代理的会话不一定有 `session.created` 先到——OpenCode 2 建好子会话后立刻
// 提交任务，提示词钩子比事件订阅先到；续用已有子会话时不再创建（2026-11-13 长跑，2.2.5、2.3.0）。所以每个会话第一次出现时
// 先查一次，查完再发它和之后的事件；只有这个会话的事件等，其他会话照常发，各会话内部保持先后。
const forwarder = (directory, parentOf) => {
  const sock = process.env.SCYLLAUI_SOCK;
  const bin = process.env.SCYLLAUI_BIN;
  if (!sock || !bin) return null;
  const children = new Set();
  const settled = new Set();
  const waited = new Set();
  const pending = new Map();
  const failures = new Map();
  const chains = new Map();
  let queue = Promise.resolve();
  const lookup = (id) => {
    if (!parentOf || !id || settled.has(id) || children.has(id) || (failures.get(id) ?? 0) >= PARENT_TRIES) {
      return Promise.resolve();
    }
    if (!pending.has(id)) {
      const ask = Promise.resolve()
        .then(() => parentOf(id))
        .then(
          (parent) => {
            if (parent) children.add(id);
            settled.add(id);
          },
          () => failures.set(id, (failures.get(id) ?? 0) + 1),
        )
        .finally(() => pending.delete(id));
      pending.set(id, ask);
    }
    if (waited.has(id)) return Promise.resolve();
    waited.add(id);
    let timer;
    return Promise.race([pending.get(id), new Promise((done) => { timer = setTimeout(done, PARENT_TIMEOUT_MS); })])
      .finally(() => clearTimeout(timer));
  };
  const forward = (payload) => {
    queue = queue.then(() => new Promise((done) => {
      try {
        const p = spawn(bin, ["hook", "--config", "opencode"], { stdio: ["pipe", "ignore", "ignore"] });
        p.on("error", done);
        p.on("close", done);
        p.stdin.on("error", () => {});
        p.stdin.end(JSON.stringify(payload));
      } catch {
        done();
      }
    }));
  };
  const send = (payload) => {
    payload.directory = directory;
    const id = payload.sessionID;
    const next = (chains.get(id) ?? Promise.resolve())
      .then(() => {
        // 上次查询出错的会话：这个事件再查一次。
        if (failures.has(id) && !pending.has(id)) waited.delete(id);
        return lookup(id);
      })
      .then(() => {
        if (children.has(id)) payload.scyllaui_subagent = true;
        forward(payload);
      })
      .catch(() => {});
    chains.set(id, next);
    next.then(() => { if (chains.get(id) === next) chains.delete(id); });
  };
  return { send, children };
};

export default {
  id: "scyllaui",

  // OpenCode 2：订阅事件；事件内容在 `data` 里，原样摊平转发，只有提问（`form.created`）取出其中的会话 ID。
  // 父会话从 `ctx.session.get` 取（会话信息的 `parentID`）。
  async setup(ctx) {
    const parentOf = async (sessionID) => {
      const info = await ctx.session.get({ sessionID });
      return info?.parentID ?? info?.data?.parentID;
    };
    const f = forwarder(ctx.location?.directory, ctx.session?.get ? parentOf : null);
    if (!f) return;
    const controller = new AbortController();
    (async () => {
      try {
        for await (const event of ctx.event.subscribe({ types: FORWARD_V2, signal: controller.signal })) {
          try {
            let data = { ...(event.data ?? {}) };
            if (event.type === "form.created") data = { sessionID: data.form?.sessionID, id: data.form?.id };
            if (event.type === "session.created") {
              // 会话信息只转标题、目录与父会话。
              data = { sessionID: data.sessionID, title: data.title, parentID: data.parentID };
              if (data.parentID) f.children.add(data.sessionID);
            }
            f.send({ hook_event_name: event.type, ...data });
          } catch {}
        }
      } catch {}
    })();
    try {
      await ctx.session.hook("prompt", (input) => {
        try {
          f.send({ hook_event_name: "session.prompt", sessionID: input.sessionID, prompt: input.prompt?.text ?? "" });
        } catch {}
      });
    } catch {}
    return () => controller.abort();
  },

  // OpenCode 1。
  async server({ directory }) {
    const f = forwarder(directory);
    if (!f) return {};
    const titles = new Map();
    const statuses = new Map();
    return {
      event: async ({ event }) => {
        try {
          const type = event.type;
          if (!FORWARD_V1.has(type)) return;
          const props = { ...(event.properties ?? {}) };
          if (type === "session.status") {
            // 每一步都会重发 busy：只在变化时转发；idle 与随后的 session.idle 重复，不转发。
            const now = props.status?.type;
            if (statuses.get(props.sessionID) === now) return;
            statuses.set(props.sessionID, now);
            if (now === "idle") return;
          }
          const info = props.info;
          if (info) {
            // 会话信息只转标题、目录与父会话。
            props.info = { id: info.id, title: info.title, directory: info.directory, parentID: info.parentID };
            props.sessionID ??= info.id;
            if (type === "session.created" && info.parentID) f.children.add(info.id);
            if (type === "session.updated") {
              if (titles.get(info.id) === info.title) return;
              titles.set(info.id, info.title);
            }
          }
          f.send({ hook_event_name: type, ...props });
        } catch {}
      },
      "chat.message": async (input, output) => {
        try {
          const prompt = (output.parts ?? []).filter((p) => p.type === "text" && !p.synthetic).map((p) => p.text).join("\n");
          f.send({ hook_event_name: "chat.message", sessionID: input.sessionID, model: input.model, prompt });
        } catch {}
      },
    };
  },
};
