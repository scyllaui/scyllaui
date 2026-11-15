#!/bin/sh
# 把 Ghostty 固定 commit 拉取到 vendor/ghostty（不入库），并校验 HEAD 与固定值完全一致。
# 用法：src/scripts/fetch-ghostty.sh
# 已存在且干净的 vendor/ghostty 会就地切换到固定 commit；有本地改动或不是 Git 仓库时报错退出，不做删除。
set -eu

GHOSTTY_REPO="https://github.com/ghostty-org/ghostty.git"
GHOSTTY_COMMIT="34f39002c6e3974b54e6a6d400bd83777c5ea558"

PROJECT_DIR=$(cd "$(dirname "$0")/../.." && pwd)
DEST="$PROJECT_DIR/vendor/ghostty"

if [ -e "$DEST" ]; then
  if [ ! -d "$DEST/.git" ]; then
    echo "错误：$DEST 已存在但不是 Git 仓库，请人工处理后重试。" >&2
    exit 1
  fi
  if [ -n "$(git -C "$DEST" status --porcelain)" ]; then
    echo "错误：$DEST 有未提交的改动，请人工处理后重试。" >&2
    exit 1
  fi
else
  mkdir -p "$DEST"
  git -C "$DEST" init -q
  git -C "$DEST" remote add origin "$GHOSTTY_REPO"
fi

if ! git -C "$DEST" cat-file -e "$GHOSTTY_COMMIT^{commit}" 2>/dev/null; then
  git -C "$DEST" fetch --depth 1 origin "$GHOSTTY_COMMIT"
fi
git -C "$DEST" -c advice.detachedHead=false checkout -q --detach "$GHOSTTY_COMMIT"

HEAD=$(git -C "$DEST" rev-parse HEAD)
if [ "$HEAD" != "$GHOSTTY_COMMIT" ]; then
  echo "错误：HEAD 为 ${HEAD}，与固定 commit $GHOSTTY_COMMIT 不一致。" >&2
  exit 1
fi
echo "Ghostty 已就位：$DEST @ $HEAD"
