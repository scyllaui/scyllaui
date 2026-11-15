#!/usr/bin/env python3
"""M4b 块 7：升级 Ghostty 前对比新旧默认快捷键与 ScyllaUI 菜单快捷键（开发用，不进入产品运行时；docs/design.md 10.1 节「终端内核升级的流程」）。

新版新增或改了动作、又与 ScyllaUI 菜单快捷键重叠的绑定会抢走菜单的按键（如某版新绑定的 super+f），要在
src/app/ScyllaUI/Resources/scyllaui.conf 里解除；已解除的不算。旧版本里已有的重叠由菜单优先处理，不报。
用法：ghostty-keybind-diff.py [旧] 新
  参数可以是 Ghostty.app、ghostty 可执行文件，或事先存下的 `ghostty +list-keybinds --default` 输出；
  只给一个参数时，旧的用 vendor/ghostty/zig-out/Ghostty.app（固定的 commit）。
退出码：0 没有新冲突，1 有新冲突，2 用法或读取错误。
"""
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PINNED = ROOT / "vendor/ghostty/zig-out/Ghostty.app"
DELEGATE = ROOT / "src/app/ScyllaUI/ScyllaUIAppDelegate.swift"
CONF = ROOT / "src/app/ScyllaUI/Resources/scyllaui.conf"

# 触发键里也可能有 `=`（super+==…、super++=…），动作名不会有，所以非贪婪地取到第一个「=动作名」。
BIND = re.compile(r"^(.+?)=([a-z_][a-z0-9_]*(?::.*)?)$")
MODS = {"super": "super", "cmd": "super", "command": "super", "shift": "shift", "ctrl": "ctrl", "control": "ctrl",
        "alt": "alt", "opt": "alt", "option": "alt"}
KEYS = {"left_bracket": "[", "bracket_left": "[", "right_bracket": "]", "bracket_right": "]", "comma": ",",
        "period": ".", "slash": "/", "backslash": "\\", "semicolon": ";", "quote": "'", "backquote": "`",
        "grave_accent": "`", "minus": "-", "equal": "=", "plus": "+", "space": " "}
# ScyllaUI 菜单里带 Shift 的字符：AppKit 的 keyEquivalent 写成上档字符，Ghostty 写成 shift+原键。
SHIFTED = {"{": "[", "}": "]", "<": ",", ">": ".", "?": "/", "|": "\\", ":": ";", '"': "'", "~": "`", "_": "-"}


def norm(trigger):
    """触发键规范化为「修饰键（排好序）+键」，去掉 global:、all: 等前缀；连按的序列只取第一步。"""
    t = trigger.split(">")[0]
    while re.match(r"^(global|all|unconsumed|performable):", t):
        t = t.split(":", 1)[1]
    parts = t.split("+")
    # 键本身可能是「+」或「=」：super++ 拆出来末尾是两个空串。
    if t.endswith("++") or t == "+":
        parts, key = parts[:-2], "+"
    else:
        parts, key = parts[:-1], parts[-1]
    mods = sorted({MODS.get(p.lower(), p.lower()) for p in parts if p})
    key = key.lower()
    key = KEYS.get(key, key)
    if key.startswith("digit_"):
        key = key[len("digit_"):]
    return "+".join(mods + [key])


def parse(text):
    """`keybind = 触发键=动作` 的列表 → {规范化触发键: 动作}。"""
    out = {}
    for line in text.splitlines():
        line = line.strip()
        if not line.startswith("keybind"):
            continue
        body = line.split("=", 1)[1].strip()
        m = BIND.match(body)
        if not m:
            raise ValueError(f"认不出的绑定：{line}")
        out[norm(m.group(1))] = m.group(2)
    return out


def load(arg):
    p = Path(arg)
    if p.suffix == ".app":
        p = p / "Contents/MacOS/ghostty"
    if not p.exists():
        raise ValueError(f"不存在：{p}")
    data = p.read_bytes()
    if not data.startswith(b"\xcf\xfa\xed\xfe") and not data.startswith(b"\xca\xfe\xba\xbe"):
        return parse(data.decode("utf-8"))
    r = subprocess.run([str(p), "+list-keybinds", "--default"], capture_output=True, text=True, timeout=30)
    if r.returncode != 0:
        raise ValueError(f"{p} +list-keybinds 失败：{r.stderr.strip()}")
    return parse(r.stdout)


def scyllaui_shortcuts():
    """从 ScyllaUIAppDelegate.swift 读菜单快捷键 → {规范化触发键: 菜单项}。修饰键默认 Cmd，紧跟的
    `keyEquivalentModifierMask = [...]` 改写上一项的修饰键；`"\\(n)"` 按 `for n in 1...9` 展开。"""
    src = DELEGATE.read_text(encoding="utf-8")
    item = re.compile(r'(?:add\(|NSMenuItem\(title: |addItem\(withTitle: )String\(localized: "([^"]*)"\).*?(?:keyEquivalent: )?"((?:\\.|[^"\\])*)"(?:, tag: -?\w+)?\)')
    mask = re.compile(r"keyEquivalentModifierMask = \[([^\]]*)\]")
    found, last = {}, None
    for line in src.splitlines():
        m = item.search(line)
        if m and m.group(2):
            title, key = m.group(1), m.group(2).replace("\\\\", "\\")
            keys = [str(n) for n in range(1, 10)] if key == "\\(n)" else [key]
            last = [(title.replace("\\(n)", k), k, ["super"]) for k in keys]
            for t, k, mods in last:
                found[(t, k)] = mods
            continue
        mm = mask.search(line)
        if mm and last:
            mods = [MODS[x.strip().lstrip(".")] for x in mm.group(1).split(",") if x.strip()]
            for t, k, _ in last:
                found[(t, k)] = mods
    out = {}
    for (title, key), mods in found.items():
        mods = set(mods)
        if key in SHIFTED:
            key = SHIFTED[key]
            mods.add("shift")
        elif key.isalpha() and key.isupper():
            key = key.lower()
            mods.add("shift")
        out["+".join(sorted(mods) + [KEYS.get(key, key)])] = title
    return out


def unbound():
    """scyllaui.conf 里已解除的触发键。"""
    out = set()
    for line in CONF.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line.startswith("keybind"):
            m = BIND.match(line.split("=", 1)[1].strip())
            if m and m.group(2) == "unbind":
                out.add(norm(m.group(1)))
    return out


def main(argv):
    if len(argv) not in (1, 2) or argv[0] in ("-h", "--help"):
        print(__doc__.strip(), file=sys.stderr)
        return 2
    old_arg, new_arg = (str(PINNED), argv[0]) if len(argv) == 1 else argv
    try:
        old, new = load(old_arg), load(new_arg)
    except (ValueError, OSError, subprocess.TimeoutExpired) as e:
        print(f"错误：{e}", file=sys.stderr)
        return 2
    added = {k: v for k, v in new.items() if k not in old}
    removed = {k: v for k, v in old.items() if k not in new}
    changed = {k: (old[k], v) for k, v in new.items() if k in old and old[k] != v}
    print(f"旧 {len(old)} 条，新 {len(new)} 条（按触发键合并，digit_1 与 1 算一条）：新增 {len(added)}，删除 {len(removed)}，改了动作 {len(changed)}")
    for k, v in sorted(added.items()):
        print(f"  + {k}={v}")
    for k, v in sorted(removed.items()):
        print(f"  - {k}={v}")
    for k, (a, b) in sorted(changed.items()):
        print(f"  ~ {k}：{a} → {b}")
    menu, off = scyllaui_shortcuts(), unbound()
    conflicts = [(k, new[k], menu[k]) for k in sorted({**added, **changed}) if k in menu and k not in off]
    if not conflicts:
        print(f"没有新冲突（ScyllaUI 菜单快捷键 {len(menu)} 个，scyllaui.conf 已解除 {len(off)} 个）。")
        return 0
    print("新冲突（在 scyllaui.conf 里加 `keybind = <触发键>=unbind`）：")
    for k, action, title in conflicts:
        print(f"  {k}={action}  ←→  菜单「{title}」")
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
