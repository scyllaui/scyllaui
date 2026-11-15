#!/usr/bin/env python3
"""用 S0 的安装脚本 s0-hooks.py 为 tests/hook-config/<用例>/before 生成期望结果（测试辅助，不进产品）。

s0-hooks.py 已于 2026-10-16 删除（由 `scyllaui hooks` 取代），这里从 Git 历史里它最后一次修改的提交 459fc2a 取出，
放进临时目录运行，所以仍能为新增的用例生成期望结果。

每个用例在临时 HOME 中执行：
  install/      ← before 上执行 install 的结果
  uninstall/    ← before 上执行 uninstall 的结果
  roundtrip/    ← install 结果上再执行 uninstall 的结果
备份文件（*.scyllaui-bak-*）不收进期望结果。Rust 的 hookcfg 兼容测试（proto/tests/hookcfg_compat.rs）逐字节比对。
用法：tests/hook-config/make-goldens.py
"""
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parents[1]
SCRIPT_REV = "459fc2a"
SCRIPT_PATH = "src/scripts/s0-hooks.py"


def run(script, home, action):
    env = {"HOME": str(home), "PATH": "/usr/bin:/bin"}
    subprocess.run([sys.executable, "-I", str(script), action], env=env, check=True, stdout=subprocess.DEVNULL)


def collect(home, dest):
    if dest.exists():
        shutil.rmtree(dest)
    for p in sorted(home.rglob("*")):
        if p.is_file() and ".scyllaui-bak-" not in p.name:
            q = dest / p.relative_to(home)
            q.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(p, q)
    dest.mkdir(parents=True, exist_ok=True)


def main():
    with tempfile.TemporaryDirectory() as tools:
        script = Path(tools) / "s0-hooks.py"
        src = subprocess.run(["git", "-C", str(REPO), "show", f"{SCRIPT_REV}:{SCRIPT_PATH}"],
                             check=True, capture_output=True).stdout
        script.write_bytes(src)
        for case in sorted(d for d in ROOT.iterdir() if (d / "before").is_dir()):
            with tempfile.TemporaryDirectory() as t:
                for action, start in (("install", "before"), ("uninstall", "before"), ("roundtrip", "install")):
                    home = Path(t) / action
                    shutil.copytree(case / start, home)
                    run(script, home, "uninstall" if action == "roundtrip" else action)
                    collect(home, case / action)
            print(f"{case.name}: ok")


if __name__ == "__main__":
    main()
