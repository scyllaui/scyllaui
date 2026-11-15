#!/bin/sh
# 按固定版本与 SHA-256 拉取 Sparkle（M4b 块 3，docs/design.md 10.6 节「块 3 实现」）：官方 Release 的 Sparkle-<版本>.tar.xz
# 解到 .tools/sparkle/<版本>/（不入库），含随 App 嵌入的 Sparkle.framework 与 bin/generate_keys、bin/sign_update。
# 升级 Sparkle 时同时改这里的版本、SHA-256（取自 GitHub Release 附件的 digest）与 project.yml。
set -eu

VERSION=2.10.1
SHA256=1a856d2a9edb8e628de88121d7be04cf0e2167f8641650b78fca00b14244cf27
PROJECT_DIR=$(cd "$(dirname "$0")/../.." && pwd)
DEST="$PROJECT_DIR/.tools/sparkle/$VERSION"

if [ -d "$DEST/Sparkle.framework" ] && [ -x "$DEST/bin/sign_update" ]; then
  echo "已有 Sparkle ${VERSION}：$DEST"
  exit 0
fi

TMP=$(mktemp -d "${TMPDIR:-/tmp}/sparkle-dl.XXXXXX")
trap 'rm -rf "$TMP"' EXIT
curl -fsSL -o "$TMP/sparkle.tar.xz" "https://github.com/sparkle-project/Sparkle/releases/download/$VERSION/Sparkle-$VERSION.tar.xz"
GOT=$(shasum -a 256 "$TMP/sparkle.tar.xz" | awk '{print $1}')
if [ "$GOT" != "$SHA256" ]; then
  echo "错误：Sparkle-$VERSION.tar.xz 的 SHA-256 不符（得到 ${GOT}，应为 ${SHA256}），不解压" >&2
  exit 1
fi
mkdir -p "$DEST"
tar -xJf "$TMP/sparkle.tar.xz" -C "$DEST"
echo "已拉取 Sparkle ${VERSION}：$DEST"
