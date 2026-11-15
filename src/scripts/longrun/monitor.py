#!/usr/bin/env python3
"""24 小时长跑：采样与判定（开发用，docs/design.md 10.6 节「24 小时长跑方案」）。只读观察，不连 ScyllaUI 的后台。

用法：caffeinate -i python3 -I <本文件> <结果目录> [时长小时，默认 24]
每 10 分钟：正式版 App 与 scyllauid 的进程号、内存（RSS）与 CPU（换了进程号即记一条事件）；两份日志里新增的出错行；
新出现的 ScyllaUI / scyllauid 崩溃报告；新的 unexpected_exit 行；长跑目录里 Grok 会话的累计花费（超过 3 美元写 stop-grok）；
各 CLI 新写的会话文件里的限流字样。每小时：App 日志 hook_latency 行（窗口可见时）的 P50 / P95 / 最大值。
第二轮（docs/design.md 10.6 节「第二轮长跑方案」）另记：scyllauid 打开的文件数；长跑目录里七款 CLI 进程（及 OpenCode
常驻服务）按款合计的进程数、内存与 CPU；Copilot events.jsonl 与 agy 对话库的大小（后台不记读取耗时，只能记大小）。
写 <结果目录>/samples.csv、clis.csv、records.csv、events.log、latency.csv；output/longrun/stop-all 存在或到时即结束。
"""
import csv
import json
import os
import re
import statistics
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
CTRL = ROOT / "output/longrun"
APP = "/Applications/ScyllaUI.app/Contents/MacOS/ScyllaUI"
DAEMON = "/Applications/ScyllaUI.app/Contents/Helpers/scyllauid"
LONGRUN = str(Path.home() / "scyllaui-longrun")
# 按可执行文件路径或参数认 CLI 进程（只算工作目录在长跑目录里的；OpenCode 的常驻服务不在长跑目录，单独记为 opencode-service）。
# 按前两段命令行的程序名（去掉目录）认：从 PATH 起的进程命令行是不带路径的 `claude`、`grok`、`agy`（2026-11-13 试跑发现
# 只认带斜杠的写法会漏掉），node 起的是脚本路径。Grok 的可执行文件带版本号（grok-1.0.77-macos-aarch64）。
CLI_NAMES = {
    "claude": ("claude",),
    "codex": ("codex",),
    "kimi": ("kimi", "kimi-code"),
    "grok": ("grok",),
    "opencode": ("opencode", "opencode.exe"),
    "copilot": ("copilot", "npm-loader.js"),
    "antigravity": ("agy",),
}


def cli_of(cmd):
    for part in cmd.split(" ")[:2]:
        name = part.rsplit("/", 1)[-1]
        for tool, names in CLI_NAMES.items():
            if name in names or (tool == "grok" and name.startswith("grok-")) or ("@github/copilot" in part and tool == "copilot"):
                return tool
    return None
LOGS = Path.home() / "Library/Logs/ScyllaUI"
CRASH = Path.home() / "Library/Logs/DiagnosticReports"
GROK_LIMIT_TICKS = 3 * 10**10  # 3 美元（1 美元 = 1e10 ticks，2026-10-28 按官网价格核对）
LIMIT_WORDS = re.compile(rb"rate limit|usage limit|429 |too many requests|quota exceeded|limit reached", re.I)
# 日志级别 ERROR 只认大写（App 日志的 notify_latency 行带 error=- 字段，不区分大小写会每条都误报，2026-11-13 第二轮试跑发现）。
ERROR_WORDS = re.compile(r"\bERROR\b(?!=)|(?i:panic|fatal)|错误|失败")


def now():
    return time.strftime("%Y-%m-%d %H:%M:%S")


class Run:
    def __init__(self, out, hours):
        self.out = out
        self.end = time.time() + hours * 3600
        self.start = time.time()
        out.mkdir(parents=True, exist_ok=True)
        self.events = out / "events.log"
        # 开跑时已有的日志只看之后新写的；之后新出现的文件（如跨天后的 app-日期.log）从头看。
        self.offsets = {p: p.stat().st_size for p in LOGS.glob("*.log")} if LOGS.exists() else {}
        self.seen_crash = {p.name for p in CRASH.glob("*")} if CRASH.exists() else set()
        self.pids = {}
        self.limit_seen = set()
        # 长跑目录里的 Grok 会话会跨轮累积（第三轮开跑时已有第二轮的 3.139 美元），只算开跑后新增的花费（2026-11-14）。
        self.grok_base = self.grok_total()

    def event(self, text):
        line = f"{now()} {text}"
        with open(self.events, "a", encoding="utf-8") as f:
            f.write(line + "\n")
        print(line, flush=True)

    def procs(self):
        out = subprocess.run(["ps", "-ww", "-axo", "pid=,rss=,%cpu=,command="], capture_output=True, text=True).stdout
        found = {}
        for line in out.splitlines():
            parts = line.split(None, 3)
            if len(parts) == 4 and parts[3].split(" ")[0] == APP:
                found["app"] = parts
            elif len(parts) == 4 and parts[3].split(" ")[0] == DAEMON:
                found["daemon"] = parts
        return found, out

    @staticmethod
    def cwd_of(pid):
        out = subprocess.run(["lsof", "-a", "-d", "cwd", "-p", pid, "-Fn"], capture_output=True, text=True).stdout
        return next((l[1:] for l in out.splitlines() if l.startswith("n")), "")

    def sample_clis(self, ps_out):
        totals = {}
        for line in ps_out.splitlines():
            parts = line.split(None, 3)
            if len(parts) != 4:
                continue
            cmd = parts[3]
            tool = cli_of(cmd)
            if not tool or "/ScyllaUI.app/" in cmd:
                continue
            # 常驻服务（serve --service）不在长跑目录里，单独记；每个界面另起的 serve --stdio 子进程跟着界面的目录算进 opencode。
            if tool == "opencode" and "--service" in cmd:
                key = "opencode-service"
            elif self.cwd_of(parts[0]).startswith(LONGRUN):
                key = tool
            else:
                continue
            n, rss, cpu = totals.get(key, (0, 0, 0.0))
            totals[key] = (n + 1, rss + int(parts[1]), cpu + float(parts[2]))
        new = not (self.out / "clis.csv").exists()
        with open(self.out / "clis.csv", "a", newline="") as f:
            w = csv.writer(f)
            if new:
                w.writerow(["time", "tool", "procs", "rss_mb", "cpu"])
            for key in sorted(totals):
                n, rss, cpu = totals[key]
                w.writerow([now(), key, n, rss // 1024, round(cpu, 1)])

    def sample_records(self):
        # Copilot：长跑开始后写过的 events.jsonl；agy：会话摘要库与各对话库（按修改时间取开跑后的）。
        rows = []
        cop = [p for p in (Path.home() / ".copilot/session-state").glob("*/events.jsonl") if p.stat().st_mtime >= self.start]
        rows.append(("copilot-events", len(cop), sum(p.stat().st_size for p in cop), max((p.stat().st_size for p in cop), default=0)))
        agy = Path.home() / ".gemini/antigravity-cli"
        convs = [p for p in (agy / "conversations").glob("*.db") if p.stat().st_mtime >= self.start]
        rows.append(("agy-conversations", len(convs), sum(p.stat().st_size for p in convs), max((p.stat().st_size for p in convs), default=0)))
        summ = agy / "conversation_summaries.db"
        if summ.exists():
            rows.append(("agy-summaries", 1, summ.stat().st_size, summ.stat().st_size))
        new = not (self.out / "records.csv").exists()
        with open(self.out / "records.csv", "a", newline="") as f:
            w = csv.writer(f)
            if new:
                w.writerow(["time", "kind", "files", "total_kb", "max_kb"])
            for kind, n, total, biggest in rows:
                w.writerow([now(), kind, n, total // 1024, biggest // 1024])

    def sample(self):
        found, ps_out = self.procs()
        row = [now()]
        for k in ("app", "daemon"):
            p = found.get(k)
            pid = p[0] if p else ""
            if self.pids.get(k) != pid:
                self.event(f"{k} pid {self.pids.get(k) or '-'} -> {pid or '-'}")
                self.pids[k] = pid
            row += [pid, int(p[1]) // 1024 if p else "", p[2] if p else ""]
        d = found.get("daemon")
        fds = len(subprocess.run(["lsof", "-p", d[0]], capture_output=True, text=True).stdout.splitlines()[1:]) if d else ""
        row.append(fds)
        new = not (self.out / "samples.csv").exists()
        with open(self.out / "samples.csv", "a", newline="") as f:
            w = csv.writer(f)
            if new:
                w.writerow(["time", "app_pid", "app_rss_mb", "app_cpu", "daemon_pid", "daemon_rss_mb", "daemon_cpu", "daemon_fds"])
            w.writerow(row)
        self.sample_clis(ps_out)
        self.sample_records()

    def new_lines(self, path):
        if not path.exists():
            return []
        size = path.stat().st_size
        off = self.offsets.get(path, 0)
        if size < off:
            off = 0
        with open(path, "rb") as f:
            f.seek(off)
            data = f.read()
        self.offsets[path] = off + len(data)
        return data.decode("utf-8", "replace").splitlines()

    def scan_logs(self):
        for path in [LOGS / "scyllauid.log", LOGS / time.strftime("app-%Y-%m-%d.log")]:
            for line in self.new_lines(path):
                if "unexpected_exit" in line:
                    self.event(f"[unexpected_exit] {line.strip()[:300]}")
                elif ERROR_WORDS.search(line):
                    self.event(f"[log {path.name}] {line.strip()[:300]}")
        if CRASH.exists():
            for p in CRASH.glob("*"):
                if p.name not in self.seen_crash:
                    self.seen_crash.add(p.name)
                    if p.name.lower().startswith(("scyllaui", "scyllauid")):
                        self.event(f"[crash] {p.name}")

    def grok_total(self):
        total = 0
        for u in (Path.home() / ".grok/sessions").glob("*scyllaui-longrun*/*/usage.json"):
            try:
                total += json.loads(u.read_text())["session"].get("costUsdTicks", 0)
            except (OSError, ValueError, KeyError):
                pass
        return total

    def grok_cost(self):
        total = self.grok_total() - self.grok_base
        if total > GROK_LIMIT_TICKS and not (CTRL / "stop-grok").exists():
            (CTRL / "stop-grok").touch()
            self.event(f"[grok] 累计 {total / 1e10:.3f} 美元，超过 3 美元，已写 stop-grok")
        return total / 1e10

    def scan_limits(self, since):
        roots = [Path.home() / ".claude/projects", Path.home() / ".codex/sessions", Path.home() / ".kimi-code/sessions",
                 Path.home() / ".grok/sessions"]
        for root in roots:
            if not root.exists():
                continue
            for p in root.rglob("*.json*"):
                try:
                    if p.stat().st_mtime < since:
                        continue
                    with open(p, "rb") as f:
                        f.seek(max(0, p.stat().st_size - 200_000))
                        data = f.read()
                except OSError:
                    continue
                # Claude 的会话按目录分文件夹，只看长跑目录的；否则本仓库里谈到长跑的会话（如值守的会话）会误报。
                if root.name == "projects" and "scyllaui-longrun" not in str(p):
                    continue
                if b"scyllaui-longrun" not in data and "scyllaui-longrun" not in str(p):
                    continue
                m = LIMIT_WORDS.search(data)
                if m and (p, m.group(0)) not in self.limit_seen:
                    self.limit_seen.add((p, m.group(0)))
                    self.event(f"[limit?] {root.parent.name} {p.name}: {m.group(0).decode(errors='replace')}")

    def latency(self):
        # 打点事件不进系统日志的持久存储（2026-10-28 实测 log show --signpost 取不到），改读 App 日志里同一处写的 hook_latency 行。
        # 只统计窗口可见时的样本：status_timer stop visible=false 到下一个 status_timer start 之间算不可见，单独计数。
        since = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(time.time() - 3600))
        vals, hidden, visible = [], 0, True
        days = sorted({since[:10], now()[:10]})
        for day in days:
            path = LOGS / f"app-{day}.log"
            if not path.exists():
                continue
            with open(path, encoding="utf-8", errors="replace") as f:
                for line in f:
                    if "status_timer stop visible=false" in line:
                        visible = False
                    elif "status_timer start" in line:
                        visible = True
                    elif "hook_latency" in line and line[:19] >= since:
                        m = re.search(r"total_us=(\d+)", line)
                        if not m:
                            continue
                        if visible:
                            vals.append(int(m.group(1)) / 1000)
                        else:
                            hidden += 1
        new = not (self.out / "latency.csv").exists()
        with open(self.out / "latency.csv", "a", newline="") as f:
            w = csv.writer(f)
            if new:
                w.writerow(["time", "n", "p50_ms", "p95_ms", "max_ms", "hidden_n"])
            if vals:
                vals.sort()
                p95 = vals[min(len(vals) - 1, max(0, -(-len(vals) * 95 // 100) - 1))]
                w.writerow([now(), len(vals), round(statistics.median(vals), 1), round(p95, 1), round(vals[-1], 1), hidden])
            else:
                w.writerow([now(), 0, "", "", "", hidden])

    def loop(self):
        self.event(f"monitor start pid={os.getpid()} until={time.strftime('%m-%d %H:%M', time.localtime(self.end))}")
        last_hour = 0
        last_scan = time.time()
        while time.time() < self.end and not (CTRL / "stop-all").exists():
            try:
                self.sample()
                self.scan_logs()
                cost = self.grok_cost()
                self.scan_limits(last_scan - 60)
                last_scan = time.time()
                if time.time() - last_hour >= 3600:
                    self.latency()
                    last_hour = time.time()
                    self.event(f"hourly: grok_cost={cost:.3f} USD")
            except Exception as e:  # 采样本身出错只记一笔，不中断长跑
                self.event(f"[monitor error] {type(e).__name__}: {e}")
            for _ in range(60):
                if (CTRL / "stop-all").exists():
                    break
                time.sleep(10)
        self.event("monitor end")


if __name__ == "__main__":
    if len(sys.argv) not in (2, 3):
        print(__doc__.strip(), file=sys.stderr)
        sys.exit(2)
    Run(Path(sys.argv[1]).resolve(), float(sys.argv[2]) if len(sys.argv) == 3 else 24).loop()
