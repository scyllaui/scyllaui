#!/usr/bin/env python3
"""M1 验收：多会话并行时侧栏状态与真实事件一致（开发用，不进入产品运行时）。

在伪终端里同时运行真实的七款 CLI（交互界面，不是无界面模式；OpenCode、Copilot、Antigravity 为 M4b 块 9a、9c、9b 加入，每款 3 个会话），每个会话的 Hook 环境指向测试后台、
SCYLLAUI_PANE_ID 指向 App 以 SCYLLAUI_FIXTURE_TERMINALS 登记的夹具终端（没有界面的终端，见 README「侧栏测试数据」）。
分阶段触发真实事件，每个阶段取后台快照，与按 docs/design.md 4.3 节推出的期望状态逐个比对；
另在会话并行时删掉 Hook 条目，核对后台发现与修复（hooks.status / plan / repair）。

用法：acceptance-parallel.py <测试目录> [--held]
  测试目录下须有：home/（临时 HOME 副本，已装 Hook）、work/（工作目录）、fixture.json（夹具终端，至少 16 个）、
  data/scyllauid.sock（App 已以 SCYLLAUI_DATA_DIR=<测试目录>/data 启动）。
  Claude 在临时 HOME 下取不到登录，用真实 HOME、在已信任的项目目录里运行；Kimi 同样用真实 HOME 与项目目录（2026-10-16：
  不再把 Kimi 凭据复制进临时 HOME——令牌 15 分钟过期、靠 refresh token 续期，副本续期可能让真实 HOME 的凭据失效）；
  Codex、Grok 用临时 HOME。Grok 已改用 API Key 时，从环境变量 XAI_API_KEY 透传（不写盘、不打印）。
  OpenCode 用临时 HOME：插件由 `scyllaui hooks install --home` 装入，配置写在临时 HOME 的 ~/.config/opencode/opencode.json（bash 设为需审批，
  模型用 Zen 的免费模型，不需要登录；环境里有 DEEPSEEK_API_KEY 时改用 deepseek/deepseek-flash，键只透传）。不用 OPENCODE_CONFIG_DIR：
  OpenCode 2 设了它就只读这一个目录，不再读 ~/.config/opencode，插件不加载（2026-11-05 实测 2.2.5）。
  Copilot 的登录在钥匙串里，换了 HOME 取不到：用真实 HOME，COPILOT_HOME 指向临时 HOME 的 .copilot（测试目录里须先复制
  ~/.copilot/config.json 进去，再 `scyllaui hooks install --home`），在 work/ 里运行；首次进目录的信任提示只写临时目录。
  Antigravity 的登录令牌在 ~/.gemini 下：用真实 HOME，在 agy 已信任的目录（SCYLLAUI_ACCEPT_AGY_WORK，缺省
  $TMPDIR/agy-probe/work）里运行，Hook 由该目录的项目级 .agents/hooks.json 提供（内容同 `scyllaui hooks install` 写的那一项），
  不碰真实的全局 Hook 文件；出现信任提示即中止，不替用户应答（应答会写进真实配置）。测试后台按 SCYLLAUI_CLI_HOME（临时 HOME）
  推出对话库位置，agy 却写在真实 HOME：临时 HOME 里放一个指向真实对话库目录的符号链接，后台只读（2026-11-05 发现）。
结果写 <测试目录>/report.json，并打印每个阶段的比对表。每个会话最多 2—3 条最短提示词。

--held（M3 第 3 块，docs/design.md 10.3 节）：CLI 不由脚本的伪终端运行，而由后台持有的 PTY 运行（保活终端）。
脚本以 `pty` 角色接入夹具终端（docs/protocol.md 第 3.1 节），hello 里的 `spawn.command` 让测试后台直接起 CLI；
脚本扮演 attach：收快照与输出、代答终端查询、以 INPUT 帧输入。另加阶段 PD：全部会话提问后立即断开全部 attach，
并（SCYLLAUI_ACCEPT_QUIT_APP=1 时）退出测试 App，CLI 在无人连接时回答完，从后台核对状态；再重新接回，核对快照里有回答。
结果写 <测试目录>/report-held.json。
"""
import json
import os
import pty
import re
import select
import shutil
import signal
import socket
import struct
import sys
import termios
import fcntl
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PROJECT = str(ROOT)
SCYLLAUI_BIN = str(ROOT / "output/build/Build/Products/Debug/ScyllaUIBeta.app/Contents/Helpers/scyllaui")
NVM_BIN = os.environ.get("NVM_BIN") or os.path.dirname(shutil.which("node") or "")  # nvm 设置的 NVM_BIN，没有时取 PATH 上 node 所在目录
REAL_HOME = os.environ["HOME"]
# ~/.local/bin（agy）放在最后：前面的顺序与之前各轮回归相同，Codex 仍用 npm 的那一份，结果可与基线对比。
PATH = ":".join([NVM_BIN, f"{REAL_HOME}/.kimi-code/bin", f"{REAL_HOME}/.grok/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin",
                 f"{REAL_HOME}/.local/bin"])
TOOLS = ["claude", "codex", "kimi", "grok", "opencode", "copilot", "antigravity"]
# SCYLLAUI_ACCEPT_TOOLS（逗号分隔）只跑其中几款，如与 OpenCode 加入前的基线同条件对比：claude,codex,kimi,grok。
TOOLS = [t for t in os.environ.get("SCYLLAUI_ACCEPT_TOOLS", ",".join(TOOLS)).split(",") if t in TOOLS]
N = len(TOOLS) * 3
# 各款的启动命令；其他脚本（notify-check.py）可按需追加参数。
# Codex 在临时 HOME 里没有信任记录：只在本次调用中跳过信任（测试用）。
# Codex 一律关掉启动时的更新检查（临时 HOME 里回车会被当成确认去全局更新）。
# Claude Code 2.1.302 起本账号默认进入 auto 模式（自动批准它判断为安全的命令，2026-10-17 实测），测试需要审批框：
# 只在本次启动中用 manual（逐条询问），不改用户配置。Codex 同理：真实 HOME 的配置为 workspace-write 时，0.163.0 直接执行了
# P2 的 curl、不发 PermissionRequest（2026-10-24 M4a 验收实测），只在本次启动中用只读沙箱（未配置时的默认值）；
# approval_policy = "untrusted" 在 0.163.0 已不支持（配置加载失败、不发任何 Hook）。
CMDS = {"claude": ["claude", "--permission-mode", "manual"], "codex": ["codex", "--no-daemon", "--dangerously-bypass-hook-trust",
                                        "-c", "check_for_update_on_startup=false", "-c", "sandbox_mode=read-only"], "kimi": ["kimi"],
        "grok": ["grok"], "opencode": ["opencode", "--standalone"], "copilot": ["copilot", "--no-auto-update"], "antigravity": ["agy"]}
# OpenCode 2 默认连每个用户共享的后台，插件跑在后台里、拿不到这个会话的 SCYLLAUI_*；ScyllaUI 的终端里由 zsh 入口自动补
# `--standalone`（docs/design.md 3.1 节「OpenCode 2」），这里的伪终端不经过它，直接带上（本机已是 2.x）。
# 用真实路径（/private/var/…）：agy 按启动目录的写法判断是否信任，`$TMPDIR` 的 /var/… 写法不在信任列表里。
AGY_WORK = os.path.realpath(os.environ.get("SCYLLAUI_ACCEPT_AGY_WORK", os.path.join(os.environ.get("TMPDIR", "/tmp"), "agy-probe/work")))
# 用真实 HOME、在项目目录里运行的工具；其他脚本（usage-check.py）可按需改。环境变量 SCYLLAUI_ACCEPT_REAL_HOME
# （逗号分隔）可覆盖：2026-10-17 起临时 HOME 里没有 Codex 登录（凭据已按用户确认删除），M2 验收时 Codex 也用真实 HOME。
REAL_HOME_TOOLS = tuple(t for t in os.environ.get("SCYLLAUI_ACCEPT_REAL_HOME", "claude,kimi").split(",") if t)

# 终端查询的应答：光标位置、设备属性、前景 / 背景色、Kitty 键盘协议、同步输出模式。
REPLIES = [
    (b"\x1b[6n", b"\x1b[1;1R"),
    (b"\x1b[c", b"\x1b[?62;22c"),
    (b"\x1b[0c", b"\x1b[?62;22c"),
    (b"\x1b[>c", b"\x1b[>1;10;0c"),
    (b"\x1b[>0c", b"\x1b[>1;10;0c"),
    (b"\x1b]10;?", b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\"),
    (b"\x1b]11;?", b"\x1b]11;rgb:1e1e/1e1e/1e1e\x1b\\"),
    (b"\x1b[?u", b"\x1b[?0u"),
    (b"\x1b[?2026$p", b"\x1b[?2026;2$y"),
]


def session_env(tool, pane, home, work):
    real = tool in REAL_HOME_TOOLS
    cwd = PROJECT if real else work
    env = {"HOME": REAL_HOME if real else home, "PATH": PATH, "TERM": "xterm-256color",
           "COLORTERM": "truecolor", "LANG": "en_US.UTF-8", "USER": os.environ.get("USER", ""),
           "TMPDIR": os.environ.get("TMPDIR", "/tmp"), "SCYLLAUI_SOCK": SOCK, "SCYLLAUI_PANE_ID": pane,
           "SCYLLAUI_BIN": SCYLLAUI_BIN}
    if tool == "grok" and os.environ.get("XAI_API_KEY"):
        env["XAI_API_KEY"] = os.environ["XAI_API_KEY"]
    if tool == "opencode":
        if os.environ.get("DEEPSEEK_API_KEY"):
            env["DEEPSEEK_API_KEY"] = os.environ["DEEPSEEK_API_KEY"]
    if tool == "copilot":
        env["HOME"], env["COPILOT_HOME"] = REAL_HOME, str(Path(home) / ".copilot")
    if tool == "antigravity":
        cwd, env["HOME"] = AGY_WORK, REAL_HOME
    return cwd, env


class Session:
    def __init__(self, tool, pane, home, work, log_dir):
        self.tool, self.pane = tool, pane
        cmd = CMDS[tool]
        cwd, env = session_env(tool, pane, home, work)
        self.log = open(log_dir / f"{tool}-{pane[:11]}.out", "wb")
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(cwd)
            os.execvpe(cmd[0], cmd, env)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 140, 0, 0))
        self.alive = True
        self.seen = bytearray()   # 去掉控制序列后的输出（只为识别信任提示）
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        while self.alive:
            r, _, _ = select.select([self.fd], [], [], 0.2)
            if not r:
                continue
            try:
                d = os.read(self.fd, 65536)
            except OSError:
                break
            if not d:
                break
            self.log.write(d)
            self.seen += re.sub(rb"\x1b\[[0-9;?<>=]*[ -/]*[@-~]", b"", d)[-4096:]
            del self.seen[:-65536]
            for q, a in REPLIES:
                if q in d:
                    self.write(a)
        self.alive = False

    def write(self, b):
        try:
            os.write(self.fd, b)
        except OSError:
            pass

    def say(self, text):
        self.write(text.encode())
        time.sleep(0.4)
        self.write(b"\r")

    def exit(self):
        if self.tool == "codex":
            self.write(b"\x03")
            time.sleep(0.5)
            self.write(b"\x03")
        else:
            self.say({"claude": "/exit", "kimi": "/exit", "grok": "/quit", "opencode": "/exit", "copilot": "/exit",
                      "antigravity": "/exit"}[self.tool])

    def running(self):
        try:
            return os.waitpid(self.pid, os.WNOHANG) == (0, 0)
        except ChildProcessError:
            return False

    def kill(self):
        if self.running():
            os.kill(self.pid, signal.SIGTERM)
            time.sleep(1)
        if self.running():
            os.kill(self.pid, signal.SIGKILL)
        self.alive = False


def frame(kind, payload):
    return bytes([kind]) + struct.pack("<I", len(payload)) + payload


class HeldSession(Session):
    """--held：CLI 由后台持有的 PTY 运行，脚本以 `pty` 角色接入（帧格式见 docs/protocol.md 第 3.1 节）。"""
    INPUT, SNAPSHOT, OUTPUT, EXIT = 2, 10, 11, 12

    def __init__(self, tool, pane, home, work, log_dir):
        self.tool, self.pane = tool, pane
        self.cwd, self.env = session_env(tool, pane, home, work)
        # 后台在终端登记的目录里起 CLI（不是 hello 里的目录）：登记的目录必须就是这款工具应在的目录，
        # 否则用真实 HOME 的工具会进到未信任的目录（见 accept_trust）。夹具用 `fixture-held.json`（make_held_fixture）。
        t = states()[0].get(pane) or {}
        if t.get("cwd") != self.cwd:
            raise RuntimeError(f"{tool} {pane[:11]} 登记的目录 {t.get('cwd')} 不是 {self.cwd}：请用 fixture-held.json 启动测试 App")
        self.log = open(log_dir / f"held-{tool}-{pane[:11]}.out", "wb", buffering=0)
        self.seen = bytearray()
        self.exited = False
        self.snapshot = b""
        self.lock = threading.Lock()
        self.sock = None
        self.attach(spawn=True)

    def attach(self, spawn=False):
        s = socket.socket(socket.AF_UNIX)
        s.connect(SOCK)
        hello = {"type": "hello", "role": "pty", "protocol_version": 3, "build_id": "acceptance",
                 "terminal_id": self.pane, "cols": 140, "rows": 40, "cell_w": 0, "cell_h": 0}
        if spawn:
            # 后台以 execve 起 `spawn.command`，不查 PATH：先按会话的 PATH 解析出绝对路径。
            exe = shutil.which(CMDS[self.tool][0], path=self.env["PATH"])
            if not exe:
                raise RuntimeError(f"PATH 里找不到 {CMDS[self.tool][0]}")
            hello["spawn"] = {"cwd": self.cwd, "env": self.env, "command": [exe] + CMDS[self.tool][1:]}
        s.sendall((json.dumps(hello) + "\n").encode())
        f = s.makefile("rb")
        reply = json.loads(f.readline())
        if reply.get("type") != "hello_ok":
            raise RuntimeError(f"{self.tool} {self.pane[:11]} 接入被拒：{reply}")
        self.sock, self.reader, self.alive = s, f, True
        threading.Thread(target=self._pump, args=(s, f), daemon=True).start()
        return reply

    def _read(self, f, n):
        b = f.read(n)
        if len(b) < n:
            raise EOFError
        return b

    def _pump(self, s, f):
        try:
            while True:
                kind = self._read(f, 1)[0]
                (n,) = struct.unpack("<I", self._read(f, 4))
                d = self._read(f, n)
                if kind == self.EXIT:
                    self.exited = True
                    break
                if kind == self.SNAPSHOT:
                    self.snapshot = d
                if kind in (self.SNAPSHOT, self.OUTPUT):
                    self.log.write(d)
                    self.seen += re.sub(rb"\x1b\[[0-9;?<>=]*[ -/]*[@-~]", b"", d)[-4096:]
                    del self.seen[:-65536]
                    if kind == self.OUTPUT:
                        for q, a in REPLIES:
                            if q in d:
                                self.write(a)
        except (EOFError, OSError):
            pass
        if s is self.sock:
            self.alive = False

    def write(self, b):
        with self.lock:
            if self.sock is None:
                return
            try:
                self.sock.sendall(frame(self.INPUT, b))
            except OSError:
                pass

    def detach(self):
        with self.lock:
            s, self.sock = self.sock, None
        if s:
            s.close()

    def running(self):
        return not self.exited

    def kill(self):
        if not self.exited:
            request({"type": "terminal.close", "terminal_id": self.pane, "confirmed": True})
        self.detach()


def make_held_fixture(base):
    """--held 用的夹具：用真实 HOME 的工具所在的终端登记在项目目录，其余在 work/（工具按 TOOLS 轮流分配，第 N + 1 个为 Codex）。"""
    fixture = json.loads((base / "fixture.json").read_text())
    tools = [TOOLS[i % len(TOOLS)] for i in range(N)] + ["codex"]
    for t, tool in zip(fixture, tools):
        if tool in REAL_HOME_TOOLS:
            t["cwd"] = PROJECT
    out = base / "fixture-held.json"
    out.write_text(json.dumps(fixture, indent=2) + "\n")
    return out


def open_held_as_app(pane, cwd):
    """测试 App 已退出时，以 app 角色登记一个保活终端（连接保持到 attach 接入后再关，见调用处）。"""
    s = socket.socket(socket.AF_UNIX)
    s.connect(SOCK)
    f = s.makefile("rwb")
    for m in ({"type": "hello", "role": "app", "protocol_version": 3, "build_id": "acceptance"},
              {"type": "sync", "id": 1, "terminals": []},
              {"type": "terminal.open", "id": 2, "terminal_id": pane, "project_dir": cwd, "cwd": cwd, "held": True}):
        f.write((json.dumps(m) + "\n").encode())
        f.flush()
    f.readline()
    return s


def quit_app():
    """退出测试 App（只认 Debug 构建的测试实例，正式版不碰）。"""
    out = os.popen("pgrep -f 'Debug/ScyllaUIBeta.app/Contents/MacOS/ScyllaUIBeta'").read().split()
    for pid in out:
        os.kill(int(pid), signal.SIGTERM)
    return out


def prepare_home(home):
    """临时 HOME 里关掉 Codex 启动时的更新检查：临时 HOME 没有「已跳过」的记录，Codex 会弹更新提示，
    输入的回车会被当成确认而去全局更新（2026-10-16 实际发生过：0.162.1 被更新到 0.163.0）。"""
    cfg = Path(home) / ".codex/config.toml"
    text = cfg.read_text() if cfg.exists() else ""
    if not re.search(r"(?m)^check_for_update_on_startup\s*=", text):
        cfg.write_text("check_for_update_on_startup = false\n" + text)


# OpenCode 的模型：Zen 的免费模型不需要账号，但额度用完即失败（2026-11-05 九个免费模型都报「Free usage limit reached」）；
# 环境里有 DEEPSEEK_API_KEY（用户写在 ~/.zshrc）时改用 DeepSeek 自己的模型，与 smoke-cli.py 相同，键从本进程环境透传，不写盘、不打印。
OPENCODE_MODEL = "deepseek/deepseek-flash" if os.environ.get("DEEPSEEK_API_KEY") else "opencode/deepseek-v4.1-flash-free"


def prepare_opencode(base):
    """OpenCode 的配置：bash 需审批、模型见 OPENCODE_MODEL，写在临时 HOME 的 `~/.config/opencode/`，插件由
    `scyllaui hooks install --home` 装在同一目录的 `plugins/`（与用户机器上的位置相同）。"""
    d = base / "home/.config/opencode"
    d.mkdir(parents=True, exist_ok=True)
    (d / "opencode.json").write_text(json.dumps({"$schema": "https://opencode.ai/config.json",
                                                 "model": OPENCODE_MODEL,
                                                 "permission": {"bash": "ask"}}, indent=2) + "\n")


def prepare_agy(home):
    """测试后台在临时 HOME 下找 Antigravity 对话库，agy 用真实 HOME：链到真实的对话库目录（后台只读）。"""
    link = Path(home) / ".gemini/antigravity-cli/conversations"
    if not link.exists():
        link.parent.mkdir(parents=True, exist_ok=True)
        link.symlink_to(Path(REAL_HOME) / ".gemini/antigravity-cli/conversations")


def accept_trust(sessions):
    """Kimi 在新的临时 HOME 里第一次进目录会先问是否信任（只写临时 HOME），默认选项即信任，回车确认。
    用真实 HOME 的工具出现信任提示时中止，不替用户应答：应答会写进用户真实的配置（2026-10-17 --held 曾因
    夹具目录不对、在真实 HOME 的 Kimi 里信任了测试目录）。"""
    for s in sessions:
        if s.tool == "copilot" and b"Do you trust the files in this folder" in bytes(s.seen):
            # 选第 2 项「信任并记住」：记在 COPILOT_HOME（临时目录）里。
            s.write(b"\x1b[B")
            time.sleep(0.3)
            s.write(b"\r")
            time.sleep(0.5)
            continue
        if s.tool == "grok" and b"Do you trust the contents of this directory" in bytes(s.seen):
            # 新的临时 HOME 里 Grok 1.0.70 启动先问是否信任（2026-11-01 实测，答之前不发 SessionStart）；信任记录写在
            # 临时 HOME 的 .grok/trusted_folders.toml，按「y」确认。
            if s.tool in REAL_HOME_TOOLS:
                raise RuntimeError(f"grok {s.pane[:11]} 用真实 HOME 却在问是否信任目录：中止，不替用户应答")
            s.write(b"y")
            time.sleep(0.5)
            continue
        if s.tool == "antigravity" and b"Do you trust the contents of this project" in bytes(s.seen):
            raise RuntimeError(f"antigravity {s.pane[:11]} 在问是否信任目录：{AGY_WORK} 未被 agy 信任，中止，不替用户应答")
        if s.tool == "kimi" and b"Trust this folder" in bytes(s.seen):
            if s.tool in REAL_HOME_TOOLS:
                raise RuntimeError(f"{s.tool} {s.pane[:11]} 用真实 HOME 却在问是否信任目录：目录不对，中止，不替用户应答")
            s.write(b"\r")
            time.sleep(0.5)


def request(msg):
    s = socket.socket(socket.AF_UNIX)
    s.connect(SOCK)
    f = s.makefile("rwb")
    f.write((json.dumps({"type": "hello", "role": "cli", "protocol_version": 3, "build_id": "acceptance"}) + "\n").encode())
    f.write((json.dumps({**msg, "id": 1}) + "\n").encode())
    f.flush()
    f.readline()
    reply = json.loads(f.readline())
    s.close()
    return reply


def states():
    snap = request({"type": "snapshot.get"})["snapshot"]
    return {t["terminal_id"]: t for t in snap["terminals"]}, snap


def check(phase, sessions, expect, report):
    terms, snap = states()
    rows, bad = [], 0
    for s in sessions:
        t = terms.get(s.pane) or {}
        got = (t.get("agent") or {}).get("state", "absent")
        want = expect(s)
        ok = got in want if isinstance(want, (set, tuple, list)) else got == want
        bad += not ok
        rows.append({"pane": s.pane[:11], "tool": s.tool, "state": got, "want": sorted(want) if isinstance(want, set) else want,
                     "ok": ok, "unseen": t.get("unseen"), "session": (t.get("agent") or {}).get("session_id")})
    report["phases"].append({"phase": phase, "at": time.strftime("%H:%M:%S"), "rows": rows, "mismatch": bad,
                             "recent_total": snap.get("recent_total")})
    print(f"\n== {phase}  不一致 {bad} / {len(rows)}  recent_total={snap.get('recent_total')}")
    for r in rows:
        print(f"  {'✓' if r['ok'] else '✗'} {r['tool']:6} {r['pane']}  {r['state']:8} 期望 {r['want']}  unseen={r['unseen']}")
    return bad


def main():
    global SOCK
    base = Path(sys.argv[1])
    held = "--held" in sys.argv[2:]
    if "--make-held-fixture" in sys.argv[2:]:
        print(make_held_fixture(base))
        return 0
    Kind = HeldSession if held else Session
    SOCK = str(base / "data/scyllauid.sock")
    home, work = str(base / "home"), str(base / "work")
    fixture = json.loads((base / "fixture.json").read_text())
    log_dir = base / "pty"
    log_dir.mkdir(exist_ok=True)
    prepare_home(home)
    prepare_opencode(base)
    prepare_agy(home)
    report = {"started": time.strftime("%Y-%m-%d %H:%M:%S"), "phases": [], "hooks": []}
    panes = [t["terminal_id"] for t in fixture]
    sessions = []
    try:
        # P0：N 个会话陆续启动。Claude、Grok 启动即有 SessionStart；Codex、Kimi、OpenCode、Copilot 新会话首次提交才有 Hook。
        for i in range(N):
            sessions.append(Kind(TOOLS[i % len(TOOLS)], panes[i], home, work, log_dir))
            time.sleep(1.0)
        time.sleep(15)
        # CLI 启动时自己出错退出（如 Codex 偶发的 account/read failed）不发任何 Hook，侧栏停在「未知」与事件一致；
        # 为了让后续阶段可比，在同一个终端里重启一次并记下原因。
        report["restarts"] = []
        for i, s in enumerate(sessions):
            if not s.running():
                s.log.flush()
                tail = re.sub(rb"\x1b\[[0-9;?<>=]*[ -/]*[@-~]", b" ", open(s.log.name, "rb").read())[-300:].decode("utf-8", "replace")
                report["restarts"].append({"tool": s.tool, "pane": s.pane[:11], "tail": " ".join(tail.split())})
                print(f"  重启 {s.tool} {s.pane[:11]}：启动后已退出（{' '.join(tail.split())[-120:]}）")
                if held:
                    request({"type": "terminal.close", "terminal_id": s.pane, "confirmed": True})
                    raise RuntimeError(f"{s.tool} 启动后已退出，保活模式下不能在同一终端重启：{' '.join(tail.split())[-120:]}")
                sessions[i] = Session(s.tool, s.pane, home, work, log_dir)
        if report["restarts"]:
            time.sleep(15)
        accept_trust(sessions)
        time.sleep(3)
        check("P0 启动后、提问前", sessions,
              lambda s: "ready" if s.tool in ("claude", "grok") else "unknown", report)

        # P1：N 个会话几乎同时提问（每个间隔 0.2 s）。
        for s in sessions:
            s.say("只回复 OK")
            time.sleep(0.2)
        time.sleep(45)
        check(f"P1 {N} 个会话同时提问、回答完毕", sessions, lambda s: "ready", report)

        # P2：每款第 1 个会话请求执行需要审批的无害命令。
        asks = {
            "claude": "请用 Bash 工具执行这条命令，不要做别的：touch /tmp/scyllaui-b8-claude.txt",
            # 只读沙箱里写 /tmp 必须审批（2026-11-05 实测 0.165.0）。原先用 curl：真实 HOME 下 Codex 直接在沙箱里执行、
            # 联网被拦后失败，不请求审批（2026-11-05 并行回归）。
            "codex": "请执行这条 shell 命令，不要做别的：touch /tmp/scyllaui-b8-codex.txt",
            "kimi": "请执行这条 shell 命令，不要做别的：touch /tmp/scyllaui-b8-kimi.txt",
            "grok": "请执行这条 shell 命令，不要做别的：rm -f ~/scyllaui-b8-nonexistent.txt",
            "opencode": "请用 bash 执行这条命令，不要做别的：touch /tmp/scyllaui-b9-opencode.txt",
            # Copilot 默认逐条审批（Manual Approval），work/ 里 touch 也要问（2026-10-30 实测）。
            "copilot": "请用 shell 执行这条命令，不要做别的：touch scyllaui-b9-copilot.txt",
            # Antigravity 默认逐条审批；等待审批不发 Hook，后台从对话库认出（M4b 块 9b）。
            "antigravity": "请执行这条 shell 命令，不要做别的：touch scyllaui-b9-agy.txt",
        }
        approvers = sessions[0:len(TOOLS)]
        for s in approvers:
            s.say(asks[s.tool])
        time.sleep(40)
        check("P2 每款 1 个会话等待审批", sessions,
              lambda s: "waiting" if s in approvers else "ready", report)

        # P3：拒绝审批。Claude、Codex、Copilot、Antigravity 按 Esc（Copilot、Antigravity 不发 Hook，后台从会话记录认出拒绝），Kimi 选「3」；Grok 的 Esc 不关闭审批框，选第 4 项「No, reject」再回车（1.0.52 实测）。
        # Claude 拒绝后不发任何事件（design.md 风险 13），M2 第 1 块起由后台读会话记录认出拒绝，回到 ready。
        for s in approvers:
            if s.tool == "grok":
                s.write(b"4")
                time.sleep(0.5)
                s.write(b"\r")
            elif s.tool == "opencode":
                # 审批框选项为 Allow once / Allow always / Reject，右移两格到 Reject 再回车（1.19.5 实测）。
                s.write(b"\x1b[C")
                time.sleep(0.3)
                s.write(b"\x1b[C")
                time.sleep(0.3)
                s.write(b"\r")
            elif s.tool == "kimi":
                # Kimi 2.3.1 的审批框 Esc 不再拒绝，按「3. Reject」直接选（2026-10-25 实测）。
                s.write(b"3")
            else:
                s.write(b"\x1b")
        time.sleep(12)
        check("P3 拒绝审批后", sessions,
              lambda s: {"ready", "error"} if s in approvers else "ready", report)

        if held:
            # PD：提问后立即断开全部 attach（并退出测试 App），CLI 在无人连接时回答完；从后台核对状态，再接回核对快照。
            for s in sessions:
                s.say("只回复 SCYLLAUI")
                time.sleep(0.2)
            for s in sessions:
                s.detach()
            quitted = quit_app() if os.environ.get("SCYLLAUI_ACCEPT_QUIT_APP") == "1" else []
            report["detached"] = {"at": time.strftime("%H:%M:%S"), "quit_app": quitted}
            print(f"\n== PD 已断开全部 attach，退出测试 App：{quitted or '否'}")
            time.sleep(45)
            check("PD 断开期间回答完毕", sessions,
                  lambda s: {"ready", "error"} if s in approvers else "ready", report)
            terms, _ = states()
            unseen = sum(bool((terms.get(s.pane) or {}).get("unseen")) for s in sessions)
            missing = []
            for s in sessions:
                s.snapshot = b""
                if not s.exited:
                    s.attach()
            time.sleep(2)
            for s in sessions:
                if s.exited:
                    continue
                text = re.sub(rb"\x1b\[[0-9;?<>=]*[ -/]*[@-~]", b"", s.snapshot)
                if b"SCYLLAUI" not in text:
                    missing.append(f"{s.tool} {s.pane[:11]}")
            report["reattach"] = {"unseen": unseen, "snapshot_without_answer": missing}
            report["phases"][-1]["mismatch"] += len(missing)
            print(f"== PD 接回：待查看 {unseen} / {len(sessions)}；快照里没有回答的 {missing or '无'}")

        # P4：会话并行时删掉 Codex 的一个条目与 Kimi 的标记块，后台应发现；再生成改动并修复。
        cfg = base / "home/.codex/hooks.json"
        d = json.loads(cfg.read_text())
        d["hooks"].pop("SessionStart")
        cfg.write_text(json.dumps(d, indent=2) + "\n")
        kimi = base / "home/.kimi-code/config.toml"
        k = kimi.read_text()
        b, e = k.index("# >>> scyllaui hooks >>>"), k.index("# <<< scyllaui hooks <<<") + len("# <<< scyllaui hooks <<<")
        kimi.write_text(k[:b].rstrip("\n") + "\n" + k[e:].lstrip("\n"))
        time.sleep(2)
        st = {h["tool"]: h["state"] for h in request({"type": "hooks.status"})["hooks"]}
        plan = request({"type": "hooks.plan"})["plan"]
        tools = [c["tool"] for c in plan["changes"] if c["files"]]
        rep = request({"type": "hooks.repair", "tools": tools, "fingerprint": plan["fingerprint"]})
        after = {h["tool"]: h["state"] for h in rep.get("hooks", [])}
        report["hooks"] = {"after_delete": st, "repair_tools": tools, "repair_ok": rep.get("ok"), "after_repair": after}
        print(f"\n== P4 删除后 {st}；修复 {tools} ok={rep.get('ok')}；修复后 {after}")
        # 修复后新开一个 Codex（第 N + 1 个夹具终端），首次提问时 Hook 应到达。
        app_conn = None
        if held and report.get("detached", {}).get("quit_app"):
            app_conn = open_held_as_app(panes[N], session_env("codex", panes[N], home, work)[0])
            time.sleep(0.5)
        extra = Kind("codex", panes[N], home, work, log_dir)
        if app_conn:
            app_conn.close()
        sessions.append(extra)
        time.sleep(10)
        extra.say("只回复 OK")
        time.sleep(30)
        check("P4 修复后新开的 Codex 提问", [extra], lambda s: "ready", report)

        # P5：每款第 2 个会话退出：对话结束、进「最近任务」。
        leavers = sessions[len(TOOLS):2 * len(TOOLS)]
        for s in leavers:
            s.exit()
        time.sleep(25)
        # 保活模式下 CLI 就是终端的进程：CLI 退出即终端关闭，快照里不再有它（absent），对话进入最近任务。
        # OpenCode、Antigravity 没有会话结束事件，只能由后台看到终端里的进程退出；原模式下 CLI 跑在本脚本的伪终端里、夹具终端
        # 没有进程，后台看不到退出，停在最后的 ready（真实窗口的退出另行在块 9a、9b 验收时实测）。
        def leaver_state(s):
            if held:
                return "absent"
            return "ready" if s.tool in ("opencode", "antigravity") else "ended"
        check("P5 每款 1 个会话退出", sessions,
              lambda s: leaver_state(s) if s in leavers else
              ({"ready", "error"} if s in approvers else "ready"), report)
    finally:
        for s in sessions:
            if s.running():
                s.exit()
        time.sleep(15)
        for s in sessions:
            s.kill()
        report["finished"] = time.strftime("%Y-%m-%d %H:%M:%S")
        (base / ("report-held.json" if held else "report.json")).write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    bad = sum(p["mismatch"] for p in report["phases"])
    h = report["hooks"]
    hooks_ok = bool(h) and h.get("repair_ok") and set(h.get("repair_tools", [])) == {"codex", "kimi"} \
        and h["after_delete"].get("codex") == "missing" and h["after_delete"].get("kimi") == "missing" \
        and all(v in ("ok", "absent") for v in h.get("after_repair", {}).values())
    print(f"\n状态比对不一致 {bad} 处；Hook 删除与修复 {'通过' if hooks_ok else '未通过'}")
    return 0 if bad == 0 and hooks_ok else 1


SOCK = ""
if __name__ == "__main__":
    sys.exit(main())
