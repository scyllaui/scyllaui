#!/bin/sh
# 清理 output/ 下除正式版外的所有 App 副本（正式版：装进「应用程序」后为 /Applications/ScyllaUI.app，否则为构建目录里的 Release）（ScyllaUI*.app，以及 2026-11-02 改名前的旧包名 LEGACY*.app）：先注销系统登记（LaunchServices），再删除 App 包本身。
# 副本所在目录里的日志、脚本等其他文件保留。默认只列出，加 --yes 才执行。
# 用法：src/scripts/clean-app-copies.sh [--yes]
set -eu

PROJECT_DIR=$(cd "$(dirname "$0")/../.." && pwd)
BUILD_RELEASE="$PROJECT_DIR/output/build/Build/Products/Release/ScyllaUI.app"
# 装进「应用程序」后以那份为正式版（M4b 块 2）；构建目录里的 Release 与 output/release 下的发布产物只注销登记、不删除
# （前者是 install-app.sh 与 release.sh 的来源，后者是已发出去的包）。
KEEP="$BUILD_RELEASE"
[ -d /Applications/ScyllaUI.app ] && KEEP=/Applications/ScyllaUI.app
# 2026-11-02 前的旧包名（Pit + wall），只为认出并清掉当时留下的副本。
LEGACY="Pit""wall"
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister

[ -d "$PROJECT_DIR/output" ] || { echo "没有 output/ 目录，无需清理"; exit 0; }

copies=$(find "$PROJECT_DIR/output" -type d \( -name 'ScyllaUI*.app' -o -name "$LEGACY*.app" \) -prune | sort)
targets=""
for app in $copies; do
    [ "$app" = "$KEEP" ] || targets="$targets $app"
done

if [ -z "$targets" ]; then
    echo "output/ 里只有正式版，无需清理：$KEEP"
    exit 0
fi

echo "保留正式版：$KEEP"
echo "待清理："
for app in $targets; do echo "  $app"; done

if [ "${1:-}" != "--yes" ]; then
    echo "（只列出未执行；确认后加 --yes 运行）"
    exit 0
fi

for app in $targets; do
    if pgrep -f "$app/Contents/MacOS/" >/dev/null; then
        echo "跳过（正在运行）：$app" >&2
        continue
    fi
    # 已注销过的副本 lsregister 会报错（-10814），不算失败；Sparkle 内嵌的 Updater.app 也有自己的登记，一并注销（M4b 块 3）。
    "$LSREGISTER" -u "$app/Contents/Frameworks/Sparkle.framework/Versions/B/Updater.app" 2>/dev/null || true
    "$LSREGISTER" -u "$app" 2>/dev/null || true
    case "$app" in
        "$BUILD_RELEASE"|"$PROJECT_DIR/output/release/"*) echo "已注销（保留文件）：$app"; continue ;;
    esac
    rm -rf "$app"
    echo "已注销并删除：$app"
done

# 正式版包内的登记（如 Sparkle 的 Updater.app）属于正式版本身，不算多余。
left=$("$LSREGISTER" -dump | grep -E "^path:.*/(ScyllaUI|$LEGACY)[A-Za-z]*\.app" | grep -v "$KEEP (" | grep -v "$KEEP/Contents/" || true)
if [ -n "$left" ]; then
    echo "仍有其他 ScyllaUI / ScyllaUI 登记：" >&2
    echo "$left" >&2
    exit 1
fi
echo "完成：系统登记里只剩正式版"
