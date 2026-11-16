#!/usr/bin/env python3
"""Kimi 自己重写 config.toml 之后，本项目的 Hook 条目仍被正确识别（开发用，不进入产品运行时）。

用法：python3 -I src/scripts/kimi-rewrite-check.py [--keep]     # --keep 保留临时目录供排查
退出码：0 通过（没装 Kimi 时跳过）；1 有一项不符；2 用法错误或缺少构建产物。

起因（2026-11-16）：Kimi 登录、登出时把 config.toml 解析后整份重新写出，注释连同本项目的标记全丢、命令改成
双引号。当时只靠标记认条目，于是状态误报缺失、撤销删不掉、修复再追加一组。现在按命令特征认条目；这个脚本在
本机已装的 Kimi 上实测一遍，Kimi 升级后的维护冒烟（smoke-cli.py）会跑它，重写的写法变了能及时发现。

做法：全在临时 HOME 里，不碰真实配置、不需要登录（依据见 docs/design.md「外部文件清单」）。
  1. 写一份带注释与一条用户自己 Hook 的配置，用 `scyllaui hooks install --home` 装上本项目的条目；
  2. 加一段托管 provider 的残留，用伪终端驱动 Kimi 的 `/logout`：它不论是否登录过都会整份重写配置；
  3. 核对：状态为 ok、修复无改动、撤销后没有本项目的任何字样且用户的条目还在、重装后为 ok，
     每一步 `kimi doctor` 都不报错。
"""
import os
import pty
import re
import select
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SCYLLAUI = ROOT / "src/core/target/debug/scyllaui"
USER_HOOK = '[[hooks]]\nevent = "Stop"\ncommand = "~/bin/my-hook.sh stop"\ntimeout = 5\n'
REMNANT = '\n[providers."managed:kimi-code"]\ntype = "kimi"\nbase_url = "https://api.kimi.com/coding/v1"\napi_key = ""\n'
ANSI = re.compile(rb"\x1b\[[0-9;?<>=]*[A-Za-z]|\x1b\][^\x07]*\x07|\x1b[()][0-9A-B]")


def drive(kimi, env, cwd, inputs, done=None, timeout=90):
    """在伪终端里打开 Kimi 的交互界面，依次输入 inputs（每条后回车），返回去掉控制序列的输出。"""
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        os.execve(kimi, ["kimi"], env)
    out = b""

    def pump(seconds, until=None):
        nonlocal out
        end = time.time() + seconds
        while time.time() < end:
            if select.select([fd], [], [], 0.2)[0]:
                try:
                    data = os.read(fd, 65536)
                except OSError:
                    return
                if not data:
                    return
                out += data
            if until and until in ANSI.sub(b"", out):
                return

    try:
        # 输入框出现前先等：全新 HOME 首次启动要十几秒，新目录还会先问是否信任。
        pump(timeout, b"context:")
        pump(2)
        for text in inputs:
            if text:
                os.write(fd, text.encode())
                pump(1.5)
            os.write(fd, b"\r")
            pump(8, done)
        os.write(fd, b"\x03")
        pump(1)
        os.write(fd, b"\x03")
        pump(3)
    except OSError:
        pass
    finally:
        try:
            os.kill(pid, 9)
        except ProcessLookupError:
            pass
        try:
            os.waitpid(pid, 0)
        except ChildProcessError:
            pass
        os.close(fd)
    return ANSI.sub(b"", out).decode("utf-8", "replace")


def main():
    args = sys.argv[1:]
    if [a for a in args if a != "--keep"]:
        print(__doc__)
        return 2
    kimi = shutil.which("kimi") or str(Path.home() / ".kimi-code/bin/kimi")
    if not Path(kimi).exists():
        print("== kimi：没装，跳过")
        return 0
    if not SCYLLAUI.exists():
        print(f"缺少 {SCYLLAUI.relative_to(ROOT)}（先 src/scripts/with-rust.sh cargo build --workspace）")
        return 2
    base = Path(tempfile.mkdtemp(prefix="scyllaui-kimi-rewrite-"))
    home, work = base / "home", base / "work"
    config = home / ".kimi-code/config.toml"
    config.parent.mkdir(parents=True)
    work.mkdir()
    env = {"HOME": str(home), "PATH": "/usr/bin:/bin", "TERM": "xterm-256color", "LANG": "en_US.UTF-8"}
    problems = []

    def hooks(action):
        return subprocess.run([str(SCYLLAUI), "hooks", action, "--home", str(home)], capture_output=True, text=True).stdout

    def state():
        return next((line.split()[1] for line in hooks("status").splitlines() if line.startswith("kimi ")), "?")

    def check(step, ok, detail=""):
        print(f"   {'通过' if ok else '不符'}  {step}{'：' + detail if detail and not ok else ''}")
        if not ok:
            problems.append(step)

    def doctor(step):
        r = subprocess.run([kimi, "doctor"], env=env, capture_output=True, text=True, timeout=60)
        bad = [line for line in (r.stdout + r.stderr).splitlines() if "ERROR" in line or "Invalid" in line]
        check(f"{step}后 kimi doctor 不报错", not bad, "；".join(bad)[:200])

    version = subprocess.run([kimi, "--version"], env=env, capture_output=True, text=True, timeout=30).stdout.strip()
    print(f"== kimi {version}：登出重写配置后的回归（临时目录 {base}）")
    try:
        config.write_text("# my settings\n\n" + USER_HOOK, encoding="utf-8")
        hooks("install")
        installed = config.read_text(encoding="utf-8")
        ours = installed.count("hook --config kimi")
        check("安装后状态为 ok", state() == "ok" and ours > 0, state())
        config.write_text(installed + REMNANT, encoding="utf-8")
        doctor("安装")

        drive(kimi, env, work, [""], timeout=90)  # 新目录：信任
        logged_out = False
        for _ in range(2):
            if "Logged out" in drive(kimi, env, work, ["/logout", ""], done=b"Logged out"):
                logged_out = True
                break
        check("Kimi 执行了 /logout", logged_out, "界面没有出现 Logged out：交互方式可能变了，脚本要跟着改")
        after = config.read_text(encoding="utf-8")
        rewritten = "整份重写了配置（注释与标记都没了）" if "#" not in after else "注释还在：这个版本没有整份重写"
        quotes = "改成了双引号" if 'command = "if' in after else "引号没变"
        print(f"   说明  登出后{rewritten}，命令{quotes}")
        check("登出后本项目的条目都在", after.count("hook --config kimi") == ours, str(after.count("hook --config kimi")))
        check("登出后状态为 ok", state() == "ok", state())
        check("登出后修复无改动", "kimi：无改动" in hooks("diff"))
        doctor("登出")

        out = hooks("uninstall")
        left = config.read_text(encoding="utf-8")
        check("撤销预览列出了要删的条目", out.count("-command = ") == ours, str(out.count("-command = ")))
        check("撤销后没有本项目的字样", "scyllaui" not in left.lower())
        check("撤销后用户自己的条目还在", left.count("my-hook.sh") == 1)
        check("撤销后状态为 missing", state() == "missing", state())
        doctor("撤销")

        hooks("install")
        check("重装后状态为 ok、不重复", state() == "ok" and config.read_text(encoding="utf-8").count("hook --config kimi") == ours)
        doctor("重装")
    finally:
        if "--keep" in args:
            print(f"   临时目录保留：{base}")
        else:
            shutil.rmtree(base, ignore_errors=True)
    print(f"== kimi {version}：{'通过' if not problems else '未通过（' + '、'.join(problems) + '）'}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
