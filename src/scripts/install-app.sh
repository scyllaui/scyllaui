#!/bin/sh
# 把构建目录里的正式版装进「应用程序」（M4b 块 2，docs/design.md 10.6 节「块 2 实现」）：/Applications/ScyllaUI.app。
# 用法：src/scripts/install-app.sh
# 装之前核对：源是 build-app.sh Release 产出的、签名完整、本团队 Developer ID；装好后核对与源的构建标识一致。
# 已有的 /Applications/ScyllaUI.app 先改名为 .old 备份，装好、核对通过后才删掉备份；失败时恢复备份。
# 正在运行的 App 不受影响：它在内存里，下次从「应用程序」打开时，后台按路径不同原地换新（docs/protocol.md 第 1 节）。
set -eu

PROJECT_DIR=$(cd "$(dirname "$0")/../.." && pwd)
SRC="$PROJECT_DIR/output/build/Build/Products/Release/ScyllaUI.app"
DEST=/Applications/ScyllaUI.app
OLD=/Applications/ScyllaUI.app.old
TEAM_ID="94A284J775"
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister

fail() { echo "错误：$*" >&2; exit 1; }

[ -d "$SRC" ] || fail "没有正式版：先运行 src/scripts/build-app.sh Release"
codesign --verify --deep --strict "$SRC" || fail "$SRC 签名校验失败"
codesign -dvv "$SRC" 2>&1 | grep -q "^TeamIdentifier=$TEAM_ID$" || fail "$SRC 不是本团队的 Developer ID 签名"
SRC_BUILD=$("$SRC/Contents/Helpers/scyllauid" --build-id)
VERSION=$(/usr/libexec/PlistBuddy -c "Print CFBundleShortVersionString" "$SRC/Contents/Info.plist")
[ ! -e "$OLD" ] || fail "$OLD 已存在（上次安装没有收尾），先确认后手动处理"

if [ -d "$DEST" ]; then
  mv "$DEST" "$OLD"
fi
if ! ditto "$SRC" "$DEST"; then
  rm -rf "$DEST"
  [ -d "$OLD" ] && mv "$OLD" "$DEST"
  fail "拷贝失败，已恢复原来的 $DEST"
fi
DEST_BUILD=$("$DEST/Contents/Helpers/scyllauid" --build-id 2>/dev/null || true)
if [ "$DEST_BUILD" != "$SRC_BUILD" ] || ! codesign --verify --deep --strict "$DEST"; then
  rm -rf "$DEST"
  [ -d "$OLD" ] && mv "$OLD" "$DEST"
  fail "装好后核对不通过（构建标识 ${DEST_BUILD:-无}，应为 ${SRC_BUILD}），已恢复原来的 $DEST"
fi
# 图标换了没有：程序坞缓存着旧图标，重新登记不会让它重读（2026-11-14：包内已是新图标，程序坞自 10-18 起没重启，
# 显示的一直是旧图）。只在图标文件确实变了时才刷新，平时不动程序坞。
ICON_CHANGED=0
if [ -d "$OLD" ]; then
  for f in Assets.car Scylla.icns; do
    cmp -s "$OLD/Contents/Resources/$f" "$DEST/Contents/Resources/$f" || ICON_CHANGED=1
  done
  "$LSREGISTER" -u "$OLD" 2>/dev/null || true
  rm -rf "$OLD"
fi
"$LSREGISTER" -f "$DEST"
if [ "$ICON_CHANGED" = 1 ]; then
  # 改包目录的修改时间让系统重读图标（不动包里的文件，签名不受影响），再重启程序坞：它会闪一下重载，不影响运行中的程序。
  # 程序坞另有自己的图标缓存，只重启不会重读（2026-11-15 实测：两次换图标后程序坞仍显示 11-08 的旧图），所以先删掉它，
  # 重启时按当前 App 重建（缓存文件，删后自动生成，用户 2026-11-15 同意）。
  touch "$DEST"
  rm -f "$(getconf DARWIN_USER_CACHE_DIR)com.apple.dock.iconcache"
  killall Dock 2>/dev/null || true
  echo "图标有变化：已刷新程序坞"
fi
echo "已装好：${DEST}（${VERSION}，构建标识 ${DEST_BUILD}）"
