#!/usr/bin/env python3
"""本机更新测试源（M4b 块 3，docs/design.md 10.6 节「块 3 实现」）：开发用，不进入产品运行时。

把给出的 DMG 用 sign_update 签名（私钥在用户的登录钥匙串里，系统可能弹授权框），拷进服务目录，写出 appcast.xml，
再在 127.0.0.1:8766 提供下载（只监听本机），每个请求打一行日志，用来确认「第一次打开不联网」。

用法：appcast-local.py <服务目录> <DMG> <版本号> <构建号> [--serve] [--tamper]
  --serve   写完后开始提供服务（Ctrl+C 结束）
  --tamper  故意把签名改错一个字符，验证 App 拒绝安装
测试版的更新源已指向 http://127.0.0.1:8766/appcast.xml；正式版测试时用
  defaults write dev.scyllaui.app ScyllaUIUpdateFeedOverride http://127.0.0.1:8766/appcast.xml
测完 defaults delete dev.scyllaui.app ScyllaUIUpdateFeedOverride。
"""
import email.utils
import http.server
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SIGN_UPDATE = ROOT / ".tools/sparkle/2.10.1/bin/sign_update"
PORT = 8766


def main() -> int:
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if len(args) != 4:
        print(__doc__)
        return 2
    serve_dir, dmg, version, build = Path(args[0]), Path(args[1]), args[2], args[3]
    serve_dir.mkdir(parents=True, exist_ok=True)
    target = serve_dir / dmg.name
    if dmg.resolve() != target.resolve():
        shutil.copyfile(dmg, target)
    out = subprocess.run([str(SIGN_UPDATE), str(target)], capture_output=True, text=True)
    if out.returncode != 0:
        print(f"sign_update 失败：{out.stderr.strip()}", file=sys.stderr)
        return 1
    attrs = out.stdout.strip()
    if not re.search(r'sparkle:edSignature="[^"]+" length="\d+"', attrs):
        print(f"sign_update 输出不认识：{attrs}", file=sys.stderr)
        return 1
    if "--tamper" in sys.argv:
        attrs = re.sub(r'edSignature="(.)', lambda m: 'edSignature="' + ("A" if m.group(1) != "A" else "B"), attrs, count=1)
    url = f"http://127.0.0.1:{PORT}/{target.name}"
    (serve_dir / "appcast.xml").write_text(f"""<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">
<channel>
<title>ScyllaUI 本机测试源</title>
<item>
    <title>ScyllaUI {version}</title>
    <pubDate>{email.utils.formatdate(usegmt=True)}</pubDate>
    <sparkle:version>{build}</sparkle:version>
    <sparkle:shortVersionString>{version}</sparkle:shortVersionString>
    <sparkle:minimumSystemVersion>14.0</sparkle:minimumSystemVersion>
    <enclosure url="{url}" {attrs} type="application/octet-stream"/>
</item>
</channel>
</rss>
""", encoding="utf-8")
    print(f"已写 {serve_dir / 'appcast.xml'}：{version}（构建号 {build}）→ {url}" + ("（签名已故意改错）" if "--tamper" in sys.argv else ""))
    if "--serve" in sys.argv:
        handler = lambda *a, **k: http.server.SimpleHTTPRequestHandler(*a, directory=str(serve_dir), **k)
        server = http.server.ThreadingHTTPServer(("127.0.0.1", PORT), handler)
        print(f"提供服务：http://127.0.0.1:{PORT}/appcast.xml", flush=True)
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
