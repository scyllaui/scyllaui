#!/usr/bin/env python3
"""新版本冒烟测试（M1 第 7 块，docs/design.md 4.7 节「冒烟测试工具」）：开发用，不进入产品运行时。

用法：src/scripts/smoke-cli.py [--new-only] [--record] [claude|codex|kimi|grok|opencode|copilot|antigravity ...]   # 缺省为全部
      src/scripts/smoke-cli.py --checklist                   # 只打印实机清单
  --new-only  只测本机版本不在 verified-versions.json 里的（M4b 块 6 的 6c：一条命令测新版本）；都在清单里就说明并退出。
  --record    通过的版本追加进 verified-versions.json（版本、日期、样本，保持原格式），由开发者提交并重新构建。

每款：
  1. 在临时 HOME（$(getconf DARWIN_USER_TEMP_DIR)scyllaui-smoke/<工具>/home，0700）里只复制该 CLI 必需的配置与认证
     文件，用 `scyllaui hooks install --home` 装 Hook（顺带验证修复代码）；
     Claude 换了 HOME 取不到钥匙串里的登录（docs/s0-hook-compat.md「防失效取证」），改用真实 HOME 加
     `--setting-sources project,local --settings <临时 HOME 的 settings.json>`：只加载临时那份配置里的 Hook。
     Kimi 也用真实 HOME、不复制凭据（2026-10-16，见 COPY 的说明），Hook 用真实配置里已装好的条目（只读检查状态）。
     Antigravity 也用真实 HOME、不复制登录令牌；临时 HOME 里装好的「scyllaui」一项写到工作目录的项目级
     `.agents/hooks.json`（agy 同样加载），不碰真实的全局 Hook 文件（2026-11-01）。
  2. 拉起隔离的 scyllauid（独立 --data-dir，--capture 落盘原始帧），Hook 环境指向它。
  3. 以无界面模式新建并提交「只回复 OK」，再按同一会话 ID 恢复并提交「再回复一次 OK」（每款 2 条最短提示词）。
  4. 自动核对：必有事件、会话 ID 一致（恢复后不变）、环境报告里有 SCYLLAUI 的三个变量、不认识的事件名。
  5. 脱敏后存为 tests/hook-samples/<工具>-<版本>-smoke.jsonl（proto/tests/replay_samples.rs 自动回放），
     判定结果存 tests/smoke-results/<工具>-<版本>.json。
  6. 删除临时 HOME（含复制进去的认证文件）；全程不打印凭据内容。
通过后把版本加入 src/core/proto/verified-versions.json（`--record` 自动追加）并提交。交互界面才有的行为见 --checklist，在测试窗口里实机做。
"""
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import time
import urllib.request
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCYLLAUI = ROOT / "src/core/target/debug/scyllaui"
SCYLLAUID = ROOT / "src/core/target/debug/scyllauid"
REAL_HOME = Path.home()
TMP = Path(subprocess.run(["getconf", "DARWIN_USER_TEMP_DIR"], capture_output=True, text=True).stdout.strip())
NVM_BIN = os.environ.get("NVM_BIN") or os.path.dirname(shutil.which("node") or "")  # nvm 设置的 NVM_BIN，没有时取 PATH 上 node 所在目录
# ~/.local/bin 在最前，与用户登录 Shell 的顺序一致：Codex 官方独立安装包的 codex 在这里，npm 里可能还留着旧版
# （2026-10-31 冒烟测到的是 npm 的 0.165.0，终端里用的是独立安装包的 0.166.0）。
PATH = ":".join([str(REAL_HOME / ".local/bin"), str(REAL_HOME / ".grok/bin"), str(REAL_HOME / ".kimi-code/bin"), NVM_BIN,
                 "/usr/bin", "/bin", "/usr/sbin", "/sbin"])
TIMEOUT = 240

# 复制进临时 HOME 的文件（相对家目录）：只取运行所必需的。
COPY = {
    "claude": [],
    "codex": [".codex/auth.json", ".codex/config.toml"],
    # Kimi 不复制（2026-10-16）：令牌 15 分钟过期、靠 refresh token 续期，临时副本续期可能让真实 HOME 的凭据失效
    # （同日用户的 Kimi CLI 曾需重新登录，原因未证实）。改为与 Claude 一样用真实 HOME，见 REAL_HOME_TOOLS。
    "kimi": [],
    "grok": [".grok/auth.json", ".grok/config.toml"],
    # OpenCode 用 Zen 的免费模型（OPENCODE_MODEL），不需要登录，什么也不复制（2026-10-29）。
    "opencode": [],
    # Copilot 的登录在钥匙串里，config.json 只记登录过的账号、不含令牌（2026-10-30 实测）：复制它，用真实 HOME 加
    # COPILOT_HOME 指向临时目录（见 COPILOT_HOME_TOOLS）。
    "copilot": [".copilot/config.json"],
    # Antigravity 的登录令牌在 ~/.gemini 下，复制品续期可能让真实的失效（同 Kimi 的顾虑）：不复制，用真实 HOME。
    "antigravity": [],
}
# OpenCode 冒烟用的模型：Zen 的免费模型，不需要账号（2026-10-29 `opencode models` 列出）。
OPENCODE_MODEL = "opencode/deepseek-v4.1-flash-free"
# 免费额度按账号或网络计、用完即失败（2026-11-05 九个免费模型都报「Free usage limit reached」）：环境里有
# DEEPSEEK_API_KEY（用户写在 ~/.zshrc）时改用 DeepSeek 自己的模型，键从本进程环境透传，不写盘、不打印。
if os.environ.get("DEEPSEEK_API_KEY"):
    OPENCODE_MODEL = "deepseek/deepseek-flash"
# 用真实 HOME 运行的工具：Claude 换了 HOME 取不到钥匙串里的登录；Kimi 不复制凭据（见 COPY）。
REAL_HOME_TOOLS = ("claude", "kimi", "copilot", "antigravity")
# 用真实 HOME、但 CLI 自己的目录指向临时 HOME 的工具：换了 HOME 取不到钥匙串，COPILOT_HOME 改目录不影响（2026-10-30 实测）。
COPILOT_HOME_TOOLS = ("copilot",)
# 每款必须出现的事件（原生事件名，任一即可）。
REQUIRED = {
    "claude": [["SessionStart"], ["UserPromptSubmit"], ["Stop"], ["SessionEnd"]],
    "codex": [["SessionStart"], ["UserPromptSubmit"], ["Stop"]],
    # Kimi 2.2.0 的 -p 无界面模式不发 SessionEnd（2026-10-15 实测）；交互界面的退出在实机清单里核对。
    # 2.6.1 起一轮正常结束不发 Stop（docs/design.md 3.1 节）：轮次结束改查会话记录的 turn.ended（kimi_turn_ended）。
    "kimi": [["SessionStart"], ["UserPromptSubmit", "TurnStarted"]],
    "grok": [["SessionStart"], ["UserPromptSubmit"], ["Stop"], ["SessionEnd"]],
    # OpenCode 没有会话结束事件；恢复时不发 session.created。OpenCode 2（2026-10-31）的提示词与一轮结束换了名字。
    "opencode": [["session.created"], ["chat.message", "session.prompt"], ["session.idle", "session.execution.succeeded"]],
    "copilot": [["sessionStart"], ["userPromptSubmitted"], ["agentStop"], ["sessionEnd"]],
    # Antigravity 没有会话结束事件；恢复时不发 SessionStart（2026-11-01 实测 1.4.12）。
    "antigravity": [["SessionStart"], ["PreInvocation"], ["Stop"]],
}
# 适配器认识的事件名（src/core/proto/src/adapters/*.rs 的 map_event）。
KNOWN = {
    "claude": {"SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PostToolUseFailure", "PermissionRequest",
               "Notification", "Stop", "StopFailure", "SessionEnd"},
    "codex": {"SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PermissionRequest", "Notification",
              "Stop", "StopFailure", "Interrupt", "SessionEnd"},
    "kimi": {"SessionStart", "UserPromptSubmit", "TurnStarted", "PreToolUse", "PostToolUse", "PermissionRequest",
             "PermissionResult", "Notification", "Stop", "StopFailure", "Interrupt", "SessionEnd"},
    "grok": {"SessionStart", "UserPromptSubmit", "PreToolUse", "PostToolUse", "PostToolUseFailure", "Notification",
             "Stop", "StopFailure", "StopCancelled", "SessionEnd"},
    "opencode": {"session.created", "session.updated", "session.status", "session.idle", "session.error", "chat.message",
                 "permission.asked", "permission.updated", "permission.replied", "question.asked", "question.replied",
                 "question.rejected",
                 "session.renamed", "session.prompt", "session.execution.started", "session.execution.succeeded",
                 "session.execution.failed", "session.execution.interrupted", "form.created", "form.replied", "form.cancelled"},
    "copilot": {"sessionStart", "userPromptSubmitted", "preToolUse", "postToolUse", "postToolUseFailure", "notification",
                "agentStop", "errorOccurred", "sessionEnd"},
    "antigravity": {"SessionStart", "PreInvocation", "PostInvocation", "PreToolUse", "PostToolUse", "Stop"},
}
SESSION_KEYS = ("session_id", "sessionId", "sessionID", "conversationId")

CHECKLIST = """实机清单（测试窗口，后台带 --capture；结果追加到同一份样本，见 docs/s0-hook-compat.md 的做法）：
  1. 交互界面启动：Claude、Grok、Kimi 恢复时启动即有 SessionStart；Codex、Kimi 新会话首次提交才有。
  2. 审批：同意与拒绝各一次（Grok 用 rm 一类危险命令才会询问，默认选项为 always-approve，不能直接回车）。
  3. 中断：轮次进行中按 Esc / Ctrl+C。
  4. Ctrl+Z 挂起与 fg 恢复（Claude、Codex 支持；Kimi、Grok 不挂起）。
  5. 交互界面恢复命令：claude --resume、codex resume --no-daemon、kimi -r、grok --resume，会话 ID 不变。
  6. Codex 升级后是否要求重新信任 Hook（侧栏应为 ok；未信任时为「状态同步未生效」）。
  7. 侧栏：未验证版本提示出现；加入 verified-versions.json 并重建后消失。
"""


# 命令名与工具名不同的（Antigravity 的命令是 agy）。
COMMAND = {"antigravity": "agy"}


def version_of(tool):
    out = subprocess.run([COMMAND.get(tool, tool), "--version"], capture_output=True, text=True, env={"HOME": str(REAL_HOME), "PATH": PATH}, timeout=20).stdout
    m = re.search(r"\bv?(\d+\.\d+\.\d+[\w.-]*)", out)
    return m.group(1).rstrip(".") if m else None   # Copilot 输出「GitHub Copilot CLI 1.0.105.」，OpenCode 2 输出「opencode v2.2.4」


def copy_auth(tool, home):
    for rel in COPY[tool]:
        src, dst = REAL_HOME / rel, home / rel
        if not src.exists():
            continue
        dst.parent.mkdir(parents=True, exist_ok=True)
        if src.is_dir():
            shutil.copytree(src, dst)
        else:
            shutil.copy2(src, dst)


def start_daemon(data, capture):
    data.mkdir(mode=0o700, parents=True, exist_ok=True)
    capture.mkdir(parents=True, exist_ok=True)
    log = open(data / "scyllauid.log", "a")
    proc = subprocess.Popen([str(SCYLLAUID), "--data-dir", str(data), "--capture", str(capture)],
                            stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                            env={"HOME": str(REAL_HOME), "PATH": "/usr/bin:/bin", "SCYLLAUI_CLI_HOME": str(data / "no-cli-home")})
    sock = data / "scyllauid.sock"
    for _ in range(50):
        if sock.exists():
            return proc, sock
        time.sleep(0.1)
    proc.kill()
    raise RuntimeError("scyllauid 没有起来")


def stop_daemon(proc, sock):
    subprocess.run([str(SCYLLAUI), "ctl", "shutdown"], env={"SCYLLAUI_SOCK": str(sock), "PATH": "/usr/bin:/bin"},
                   capture_output=True, timeout=10)
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()


def opencode_major():
    """本机 OpenCode 的主版本：1.x 输出 `1.19.5`，2.x 输出 `opencode v2.2.4`（2026-10-31）。"""
    out = subprocess.run(["opencode", "--version"], capture_output=True, text=True).stdout
    m = re.search(r"(\d+)\.\d+\.\d+", out)
    return int(m.group(1)) if m else 1


def start_opencode(env, cwd):
    """OpenCode 的 `run` 答完即退出，插件排队转发的最后几个事件来不及发出（2026-10-29 实测 1.19.5）；交互界面常驻，
    不受影响。冒烟改为起一个常驻的 `opencode serve`（插件在它里面），再用 `run --attach`（2.x 为 `--server`）提问。"""
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    proc = subprocess.Popen(["opencode", "serve", "--hostname", "127.0.0.1", "--port", str(port)], env=env, cwd=cwd,
                            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    url = f"http://127.0.0.1:{port}"
    for _ in range(150):
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=1):
                time.sleep(0.5)
                return proc, url
        except OSError:
            time.sleep(0.2)
    proc.kill()
    raise RuntimeError("opencode serve 没有起来")


def commands(tool, home, sid, attach=None):
    """(新建命令, 恢复命令生成函数)。sid 为预分配的会话 ID（只有 Claude、Grok 用得上）；attach 为 OpenCode 服务端地址。"""
    first, second = "只回复 OK", "再回复一次 OK"
    if tool == "claude":
        extra = ["--setting-sources", "project,local", "--settings", str(home / ".claude/settings.json")]
        return (["claude", "-p", first, "--session-id", sid, *extra],
                lambda s: ["claude", "-p", second, "--resume", s, *extra])
    if tool == "codex":
        flags = ["--dangerously-bypass-hook-trust", "--skip-git-repo-check"]
        return (["codex", "exec", *flags, first], lambda s: ["codex", "exec", "resume", *flags, s, second])
    if tool == "kimi":
        return (["kimi", "-p", first], lambda s: ["kimi", "-r", s, "-p", second])
    if tool == "opencode":
        flags = ["--server" if opencode_major() >= 2 else "--attach", attach, "-m", OPENCODE_MODEL]
        return (["opencode", "run", *flags, first], lambda s: ["opencode", "run", *flags, "-s", s, second])
    if tool == "copilot":
        return (["copilot", "-p", first, "--no-auto-update"], lambda s: ["copilot", "-p", second, f"--resume={s}", "--no-auto-update"])
    if tool == "antigravity":
        return (["agy", "-p", first, "--print-timeout", "180s"], lambda s: ["agy", f"--conversation={s}", "-p", second, "--print-timeout", "180s"])
    return (["grok", "-p", first, "--session-id", sid], lambda s: ["grok", "-p", second, "--resume", s])


def project_hooks(home, work):
    """Antigravity：把临时 HOME 里装好的「scyllaui」一项写进工作目录的 `.agents/hooks.json`（项目级，agy 与全局的一并加载）。
    真实的全局 Hook 文件里已经有这一项时不写，免得每个事件触发两次。"""
    real = REAL_HOME / ".gemini/config/hooks.json"
    if real.exists() and "scyllaui" in json.loads(real.read_text() or "{}"):
        return
    entry = json.loads((home / ".gemini/config/hooks.json").read_text())["scyllaui"]
    (work / ".agents").mkdir(parents=True, exist_ok=True)
    (work / ".agents/hooks.json").write_text(json.dumps({"scyllaui": entry}, ensure_ascii=False, indent=2) + "\n")


def run(cmd, env, cwd, real_home=False):
    """`real_home`：用真实 HOME 运行时 CLI 会读到用户自己的指令文件（如 AGENTS.md），思考摘要可能带出其中内容，
    判定结果要入库，所以 stderr 只记长度、不记原文。"""
    t = time.time()
    p = subprocess.run(cmd, env=env, cwd=cwd, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=TIMEOUT)
    err = p.stderr.strip()
    return {"cmd": " ".join(cmd[:3]) + " …", "exit": p.returncode, "seconds": round(time.time() - t, 1),
            "stdout_tail": p.stdout.strip()[-200:],
            "stderr_tail": f"（真实 HOME 运行，不记原文，{len(err)} 字符）" if real_home else err[-300:]}


def frames(capture, tool):
    f = capture / f"{tool}.jsonl"
    if not f.exists():
        return []
    return [json.loads(l)["frame"] for l in f.read_text().splitlines() if l.strip()]


def session_of(frame):
    """主会话的 ID；CLI 自己起的后台会话（transcript_path 显式为 null，如 Codex 的记忆整理）不算。"""
    p = frame.get("payload") or {}
    if any(k in p and p[k] is None for k in ("transcript_path", "transcriptPath")):
        return None
    return next((p[k] for k in SESSION_KEYS if p.get(k)), None)


def kimi_turn_ended(sid):
    """Kimi 会话记录里主代理的 turn.ended 条数（适配器靠它判断一轮结束；格式非官方承诺，每次升级回归）。"""
    try:
        index = (REAL_HOME / ".kimi-code/session_index.jsonl").read_text().splitlines()
        dirs = [json.loads(l).get("sessionDir") for l in index if f'"{sid}"' in l]
        wire = Path(dirs[-1].replace("~", str(REAL_HOME), 1)) / "agents/main/wire.jsonl"
        lines = [json.loads(l) for l in wire.read_text().splitlines() if '"turn.ended"' in l]
        return sum(1 for l in lines if l.get("type") == "turn.ended" and l.get("agentId") == "main"
                   and l.get("reason") in ("completed", "cancelled", "failed") and "turnId" in l)
    except (OSError, ValueError, IndexError, AttributeError):
        return 0


def smoke(tool):
    ver = version_of(tool)
    if not ver:
        return {"tool": tool, "ok": False, "error": "读不到版本"}
    base = TMP / f"scyllaui-smoke/{tool}"
    shutil.rmtree(base, ignore_errors=True)
    home, capture = base / "home", base / "capture"
    home.mkdir(mode=0o700, parents=True)
    os.chmod(base.parent, 0o700)
    data = TMP / f"pw-smoke-{tool}"            # socket 路径不能超过 103 字节
    shutil.rmtree(data, ignore_errors=True)
    work = ROOT / f"output/smoke/{tool}-{ver}/work"
    shutil.rmtree(work, ignore_errors=True)
    work.mkdir(parents=True)
    result = {"tool": tool, "version": ver, "date": time.strftime("%Y-%m-%d"), "runs": [], "checks": {}}
    proc = server = None
    try:
        for rel in [".claude", ".codex", ".kimi-code", ".grok", ".config/opencode", ".copilot", ".gemini/antigravity-cli"]:
            (home / rel).mkdir(parents=True, exist_ok=True)
        copy_auth(tool, home)
        inst = subprocess.run([str(SCYLLAUI), "hooks", "install", "--home", str(home)], capture_output=True, text=True)
        status = subprocess.run([str(SCYLLAUI), "hooks", "status", "--home", str(home)], capture_output=True, text=True).stdout
        if tool == "kimi":
            # 真实 HOME 运行：用真实配置里已装好的 ScyllaUI 条目，只读检查状态，不改真实配置。
            status = subprocess.run([str(SCYLLAUI), "hooks", "status"], capture_output=True, text=True).stdout
        if tool == "antigravity":
            project_hooks(home, work)
        result["hooks_install_exit"] = inst.returncode
        proc, sock = start_daemon(data, capture)
        env = {"HOME": str(REAL_HOME if tool in REAL_HOME_TOOLS else home), "USER": os.environ.get("USER", ""), "PATH": PATH,
               "TERM": "xterm-256color", "LANG": "en_US.UTF-8", "TMPDIR": os.environ.get("TMPDIR", "/tmp"),
               "SCYLLAUI_SOCK": str(sock), "SCYLLAUI_PANE_ID": str(uuid.uuid4()), "SCYLLAUI_BIN": str(SCYLLAUI)}
        # Grok 改用 API Key 认证时（2026-10-16 用户 `grok logout`，键在 ~/.zshrc）：从本进程环境透传，不写盘、不打印。
        # 需在交互式 zsh 里运行本脚本（`zsh -ic 'python3 -I src/scripts/smoke-cli.py grok'`）才拿得到。
        if tool in COPILOT_HOME_TOOLS:
            env["COPILOT_HOME"] = str(home / ".copilot")
        if tool == "grok" and os.environ.get("XAI_API_KEY"):
            env["XAI_API_KEY"] = os.environ["XAI_API_KEY"]
        if tool == "opencode" and os.environ.get("DEEPSEEK_API_KEY"):
            env["DEEPSEEK_API_KEY"] = os.environ["DEEPSEEK_API_KEY"]
        sid = str(uuid.uuid4())
        attach = None
        if tool == "opencode":
            # OpenCode 2 的 serve 要求连接口令（2026-10-31 实测 2.2.4）：每次随机生成，只给这次的服务与 run 用。
            env["OPENCODE_PASSWORD"] = uuid.uuid4().hex
            server, attach = start_opencode(env, work)
        new, resume = commands(tool, home, sid, attach)
        result["runs"].append(run(new, env, work, tool in REAL_HOME_TOOLS))
        time.sleep(1)
        got = frames(capture, tool)
        first_sid = next((session_of(f) for f in got if session_of(f) and not f.get("dropped")), None)
        if first_sid:
            result["runs"].append(run(resume(first_sid), env, work, tool in REAL_HOME_TOOLS))
            time.sleep(1)
        got = [f for f in frames(capture, tool) if not f.get("dropped")]
        names = [(f.get("payload") or {}).get("hook_event_name") for f in got]
        sessions = {session_of(f) for f in got if session_of(f)}
        env_ok = all({"SCYLLAUI_PANE_ID", "SCYLLAUI_SOCK", "SCYLLAUI_BIN"} <= set((f.get("env") or {}).keys()) for f in got)
        c = result["checks"]
        c["hooks_installed"] = inst.returncode == 0 and re.search(rf"^{tool}\s+(ok|untrusted)\b", status, re.M) is not None
        c["frames"] = len(got)
        required = REQUIRED[tool]
        if tool == "opencode" and opencode_major() >= 2:
            # OpenCode 2 的 run 在插件开始订阅之前就建好会话，收不到 session.created（2026-10-31 实测 2.2.4）；
            # 交互界面在第一次提问时才建会话，收得到（实机清单核对）。
            required = [alts for alts in required if alts != ["session.created"]]
        c["missing_events"] = [" / ".join(alts) for alts in required if not any(a in names for a in alts)]
        c["unknown_events"] = sorted({n for n in names if n and n not in KNOWN[tool]})
        c["sessions"] = sorted(s for s in sessions if s)
        c["resume_keeps_id"] = first_sid is not None and len(c["sessions"]) == 1
        c["preassigned_id_used"] = tool not in ("claude", "grok") or first_sid == sid
        c["env_report_ok"] = bool(got) and env_ok
        if tool == "kimi":
            # 每次提问都应在会话记录里留下一条主代理的 turn.ended。
            c["turn_ended_records"] = kimi_turn_ended(first_sid) if first_sid else 0
            if c["turn_ended_records"] < len(result["runs"]):
                c["missing_events"].append("会话记录的 turn.ended")
        result["ok"] = (c["hooks_installed"] and not c["missing_events"] and c["resume_keeps_id"]
                        and c["preassigned_id_used"] and c["env_report_ok"]
                        and all(r["exit"] == 0 for r in result["runs"]) and len(result["runs"]) == 2)
    except Exception as e:  # 记录失败原因，继续清理
        result["ok"] = False
        result["error"] = str(e)
    finally:
        if server:
            server.terminate()
            server.wait(timeout=10)
        if proc:
            stop_daemon(proc, data / "scyllauid.sock")
        if (capture / f"{tool}.jsonl").exists():
            out = subprocess.run([sys.executable, "-I", str(ROOT / "src/scripts/s0-curate-samples.py"), str(capture), f"{tool}={ver}-smoke"],
                                 capture_output=True, text=True)
            result["sample"] = f"tests/hook-samples/{tool}-{ver}-smoke.jsonl" if out.returncode == 0 else None
        shutil.rmtree(base, ignore_errors=True)   # 含复制进去的认证文件
        shutil.rmtree(data, ignore_errors=True)
    dest = ROOT / f"tests/smoke-results/{tool}-{ver}.json"
    dest.parent.mkdir(parents=True, exist_ok=True)
    dest.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    return result


VERIFIED = ROOT / "src/core/proto/verified-versions.json"


def verified_versions():
    return json.loads(VERIFIED.read_text(encoding="utf-8"))


def record(r):
    """通过的版本追加进清单（与 proto 测试 verified_versions_have_samples 的要求一致：样本存在、日期 10 位）。"""
    data = verified_versions()
    entries = data.setdefault(r["tool"], [])   # 新接入的工具第一次记录时还没有这一项
    if any(e["version"] == r["version"] for e in entries):
        return
    entries.append({"version": r["version"], "verified": r["date"], "sample": Path(r["sample"]).name})
    VERIFIED.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")


def main():
    args = sys.argv[1:]
    if args == ["--checklist"]:
        print(CHECKLIST)
        return 0
    new_only, keep = "--new-only" in args, "--record" in args
    tools = [a for a in args if a not in ("--new-only", "--record")] or list(COPY)
    if any(t not in COPY for t in tools) or not SCYLLAUI.exists() or not SCYLLAUID.exists():
        print(__doc__)
        print("（先 src/scripts/with-rust.sh cargo build --workspace）")
        return 2
    if new_only:
        listed = verified_versions()
        found = {t: version_of(t) for t in tools}
        for t, v in found.items():
            if v and v in [e["version"] for e in listed[t]]:
                print(f"== {t} {v}：已在清单里，跳过")
            elif not v:
                print(f"== {t}：读不到版本（没装或不在 PATH 里），跳过")
        tools = [t for t, v in found.items() if v and v not in [e["version"] for e in listed[t]]]
        if not tools:
            print("本机版本都已在清单里，不用测。")
            return 0
        print(f"要测：{'、'.join(f'{t} {found[t]}' for t in tools)}")
    failed = 0
    for t in tools:
        r = smoke(t)
        mark = "通过" if r.get("ok") else "未通过"
        print(f"== {t} {r.get('version')}：{mark}  {json.dumps(r.get('checks', {}), ensure_ascii=False)}")
        for run_ in r.get("runs", []):
            print(f"   {run_['cmd']} exit={run_['exit']} {run_['seconds']}s")
        if r.get("error"):
            print(f"   错误：{r['error']}")
        if keep and r.get("ok") and r.get("sample"):
            record(r)
            print(f"   已加入清单：{VERIFIED.relative_to(ROOT)}（提交并重新构建后生效）")
        failed += not r.get("ok")
    print()
    print(CHECKLIST)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
