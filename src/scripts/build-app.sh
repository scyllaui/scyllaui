#!/bin/sh
# 生成 Xcode 工程并构建 ScyllaUI.app（Xcode 目标同名 ScyllaUI）。
# 用法：src/scripts/build-app.sh [Debug|Release]，默认 Release
# 前置：build-ghosttykit.sh（xcframework）、build-ghostty-app.sh（terminfo、shell 集成脚本）与 build-ghostty-vt.sh（M3 起 scyllauid 链接的
#   libghostty-vt）；项目局部 Rust（with-rust.sh）
# 产物：output/build/Build/Products/Release/ScyllaUI.app（正式版）、Debug/ScyllaUIBeta.app（测试版，显示名 ScyllaUI Beta）。
# 2026-11-02 前包名用旧名，2026-11-05 起内部标识也改名（新旧对照见 docs/design.md 1.1 节「内部标识」）。
set -eu

XCODEGEN_VERSION="2.46.0"
CONFIG="${1:-Release}"
TARGET="ScyllaUI"

PROJECT_DIR=$(cd "$(dirname "$0")/../.." && pwd)
XCODEGEN="$PROJECT_DIR/.tools/xcodegen/$XCODEGEN_VERSION/bin/xcodegen"
APP_DIR="$PROJECT_DIR/src/app"
DERIVED="$PROJECT_DIR/output/build"

fail() { echo "错误：$*" >&2; exit 1; }

# 交付正式版前的两道检查（docs/design.md 10.1 节「版本号」），都在构建之前，不通过就不动现有正式版：
# 一、src 不能有未提交的改动；二、App 源码（src/app、src/core）比现有正式版有改动时，版本号必须变。
# 现有正式版是带改动构建的（build_id 含 -dirty）或读不到它的提交时，按「有改动」处理。
if [ "$CONFIG" = "Release" ]; then
  [ -z "$(git -C "$PROJECT_DIR" status --porcelain -- src)" ] || fail "src/ 有未提交的改动：交付正式版前先提交"
  VERSION=$(awk '/^  ScyllaUI:$/ {t=1; next} /^  [A-Za-z]+:$/ {t=0} t && /MARKETING_VERSION:/ {gsub(/"/, "", $2); print $2; exit}' "$APP_DIR/project.yml")
  [ -n "$VERSION" ] || fail "project.yml 里找不到 ScyllaUI 的 MARKETING_VERSION"
  OLD_APP="$DERIVED/Build/Products/Release/ScyllaUI.app"
  if [ -d "$OLD_APP" ]; then
    OLD_VERSION=$(/usr/libexec/PlistBuddy -c "Print CFBundleShortVersionString" "$OLD_APP/Contents/Info.plist" 2>/dev/null || true)
    OLD_BUILD=$("$OLD_APP/Contents/Helpers/scyllauid" --build-id 2>/dev/null || true)
    if [ "$VERSION" = "$OLD_VERSION" ]; then
      case "$OLD_BUILD" in
        ""|*-dirty*) CHANGED=1 ;;
        *) git -C "$PROJECT_DIR" diff --quiet "$OLD_BUILD" HEAD -- src/app src/core 2>/dev/null && CHANGED=0 || CHANGED=1 ;;
      esac
      [ "$CHANGED" = 0 ] || fail "版本号仍是 ${VERSION}，与现有正式版（build ${OLD_BUILD:-未知}）相同，但 App 源码已变：先把 project.yml 中 ScyllaUI 的修订号加 1"
    fi
  fi
fi

# 第三方许可清单是否过期（M4b 块 4，docs/design.md 10.6 节「块 4 实现」）：交付正式版时过期即拒绝构建，现有正式版不动；
# 测试版只提示，好让开发者先构建出链接映射，再运行 gen-third-party.py 重新生成。
if ! python3 -I "$PROJECT_DIR/src/scripts/gen-third-party.py" --check >/dev/null 2>&1; then
  [ "$CONFIG" != "Release" ] || fail "第三方许可清单已过期：先构建测试版，再运行 src/scripts/gen-third-party.py 并提交 ThirdPartyNotices.txt"
  echo "警告：第三方许可清单已过期，构建完成后运行 src/scripts/gen-third-party.py" >&2
fi

[ -x "$XCODEGEN" ] || fail "未找到 $XCODEGEN"
# ScyllaUI 目标用 Developer ID 签名、开硬化运行时（M4b 块 1，docs/design.md 10.6 节「块 1 实现」）；钥匙串里没有这个身份时不退回 ad-hoc。
TEAM_ID="94A284J775"
SIGN_ID=$(security find-identity -v -p codesigning | sed -n "s/.*\"\(Developer ID Application: .*($TEAM_ID)\)\".*/\1/p" | head -1)
[ -n "$SIGN_ID" ] || fail "钥匙串里没有有效的「Developer ID Application …($TEAM_ID)」签名身份"
HELPERS="scyllauid scyllaui"
XCFW="$PROJECT_DIR/vendor/ghostty/macos/GhosttyKit.xcframework"
[ -d "$XCFW" ] || fail "缺少 ${XCFW}，先运行 build-ghosttykit.sh"

# Rust 内核先构建，由 project.yml 的脚本阶段拷入 Contents/Helpers（在签名之前）。
# src 有未提交改动时附上改动内容的哈希：两次带改动的构建 build_id 也不同，App 才能识别并替换常驻的旧后台
# （docs/protocol.md 第 1 节）。哈希覆盖已跟踪文件的 diff 与未跟踪文件的路径和内容（忽略的文件除外）。
src_changes() {
  git -C "$PROJECT_DIR" diff HEAD -- src
  git -C "$PROJECT_DIR" ls-files --others --exclude-standard -- src | while IFS= read -r f; do
    echo "$f"; cat "$PROJECT_DIR/$f"
  done
}
DIRTY=""
[ -z "$(git -C "$PROJECT_DIR" status --porcelain -- src)" ] || DIRTY="-dirty.$(src_changes | shasum | cut -c1-8)"
SCYLLAUI_BUILD_ID="$(git -C "$PROJECT_DIR" rev-parse --short HEAD)$DIRTY"
export SCYLLAUI_BUILD_ID
"$PROJECT_DIR/src/scripts/with-rust.sh" cargo build --release --locked --manifest-path "$PROJECT_DIR/src/core/Cargo.toml"

"$XCODEGEN" generate --spec "$APP_DIR/project.yml" --project "$APP_DIR" --quiet
# 构建号（CFBundleVersion）取 git 提交数，只增不减（docs/design.md 10.1 节「版本号」）。
BUILD_NUMBER=$(git -C "$PROJECT_DIR" rev-list --count HEAD)
# 编译失败时旧的 App 包仍在、后面的检查都能过，所以必须看 xcodebuild 自己的退出码（2026-10-23 发现：管道接 grep 后
# 失败被吞掉，仍以新构建标识报告成功）。完整输出留在日志里，终端只摘错误、警告与结果。
XCODE_LOG="$DERIVED/xcodebuild-$TARGET-$CONFIG.log"
mkdir -p "$DERIVED"
XCODE_STATUS=0
xcodebuild \
  -project "$APP_DIR/ScyllaUI.xcodeproj" \
  -scheme "$TARGET" \
  -configuration "$CONFIG" \
  -derivedDataPath "$DERIVED" \
  CURRENT_PROJECT_VERSION="$BUILD_NUMBER" \
  build > "$XCODE_LOG" 2>&1 || XCODE_STATUS=$?
grep -E "^/.*(error|warning): |BUILD (SUCCEEDED|FAILED)" "$XCODE_LOG" || true
[ "$XCODE_STATUS" -eq 0 ] || fail "xcodebuild 失败（退出码 ${XCODE_STATUS}），这次的 App 不可用；完整日志：$XCODE_LOG"

# ScyllaUI 目标的包名：正式版 ScyllaUI、测试版 ScyllaUIBeta（project.yml 的 PRODUCT_NAME）。
NAME="ScyllaUI"
[ "$CONFIG" = "Debug" ] && NAME="ScyllaUIBeta"
APP="$DERIVED/Build/Products/$CONFIG/$NAME.app"
[ -x "$APP/Contents/MacOS/$NAME" ] || fail "构建失败，未生成 $APP"
[ "$(file -b "$APP/Contents/MacOS/$NAME" | head -c 6)" = "Mach-O" ] && ! cmp -s "$APP/Contents/MacOS/$NAME" "$PROJECT_DIR/src/core/target/release/scyllaui" || fail "$APP 主程序异常"
[ -f "$APP/Contents/Resources/terminfo/78/xterm-ghostty" ] || fail "$APP 缺少 terminfo"
for bin in $HELPERS; do
  [ -x "$APP/Contents/Helpers/$bin" ] || fail "$APP 缺少 $bin"
done
# Xcode 不删除这次构建没产出的文件：同一路径的包里，改名前的程序会一直留着并被签进新包（2026-11-05 0.4.24 首次构建，
# Helpers 里还有 11 月 2 日的旧后台与命令行）。Helpers 里只能有 HELPERS 列出的程序。
extra_helpers() {
  for f in "$APP/Contents/Helpers"/*; do
    case " $HELPERS " in
      *" ${f##*/} "*) ;;
      *) printf ' %s' "${f##*/}" ;;
    esac
  done
}
EXTRA=$(extra_helpers)
[ -z "$EXTRA" ] || fail "$APP/Contents/Helpers 里有不该有的文件：${EXTRA}（多半是改名前的构建留下的）；删掉这个 App 包后重新构建"
# 只有 Rust 二进制变化时 Xcode 不会重新签名，外层签名清单仍是旧哈希。按 project.yml 的身份重新封装：
# ScyllaUI 用 Developer ID、硬化运行时（Release 加时间戳）。
if ! codesign --verify --deep --strict "$APP" 2>/dev/null; then
  TS="--timestamp=none"; [ "$CONFIG" = "Release" ] && TS="--timestamp"
  for bin in $HELPERS; do codesign --force --sign "$SIGN_ID" --options runtime $TS "$APP/Contents/Helpers/$bin"; done
  codesign --force --sign "$SIGN_ID" --options runtime $TS --preserve-metadata=entitlements,requirements,flags "$APP"
fi
codesign --verify --deep --strict "$APP" || fail "$APP 签名校验失败"
# 签名核对（M4b 块 1）：App 与两个 Helper 都是本团队的 Developer ID、带硬化运行时；Release 不带 get-task-allow。
# Sparkle 的内层组件也要本团队签名、带硬化运行时（M4b 块 3，project.yml「Sign Sparkle」）；XPC 服务应已删除。
FW="$APP/Contents/Frameworks/Sparkle.framework"
[ -d "$FW" ] || fail "$APP 缺少 Sparkle.framework（先运行 src/scripts/fetch-sparkle.sh）"
[ ! -e "$FW/Versions/B/XPCServices" ] || fail "$FW 里还有 XPCServices（App 不在沙盒里，应在「Sign Sparkle」里删掉）"
for f in "$APP" $(for bin in $HELPERS; do printf '%s ' "$APP/Contents/Helpers/$bin"; done) "$FW/Versions/B/Autoupdate" "$FW/Versions/B/Updater.app" "$FW"; do
  info=$(codesign -dvv "$f" 2>&1)
  echo "$info" | grep -q "^Authority=Developer ID Application: .*($TEAM_ID)$" || fail "$f 不是本团队的 Developer ID 签名"
  echo "$info" | grep -q "^TeamIdentifier=$TEAM_ID$" || fail "$f 的团队标识不是 $TEAM_ID"
  echo "$info" | grep -Eq "flags=.*runtime" || fail "$f 没有开硬化运行时"
done
if codesign -d --entitlements - --xml "$APP" 2>/dev/null | grep -q "get-task-allow"; then
  [ "$CONFIG" = "Debug" ] || fail "$APP 是 Release 却带 get-task-allow"
else
  [ "$CONFIG" = "Release" ] || fail "$APP 是 Debug 却没有 get-task-allow"
fi
# 正式版的调试符号按版本存档（2026-11-02，docs/design.md 10.6 节「问题反馈」）：Developer ID 发布后 Apple 不转交崩溃报告，
# 用户发来的报告要靠同一构建的符号才看得出崩在哪里。App 存 dSYM；Rust 的两个程序没 strip、release 不带调试信息，
# dsymutil 生成不了有用的 dSYM，直接存这两个可执行文件本身。同一构建只存一次。
if [ "$CONFIG" = "Release" ]; then
  VERSION=$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' "$APP/Contents/Info.plist")
  SYMBOLS="$PROJECT_DIR/output/symbols/$VERSION-$BUILD_NUMBER-$SCYLLAUI_BUILD_ID"
  if [ ! -d "$SYMBOLS" ]; then
    mkdir -p "$SYMBOLS"
    cp -R "$DERIVED/Build/Products/$CONFIG/$NAME.app.dSYM" "$SYMBOLS/" || fail "$NAME.app.dSYM 存档失败"
    for bin in $HELPERS; do cp "$APP/Contents/Helpers/$bin" "$SYMBOLS/$bin" || fail "$bin 存档失败"; done
    echo "调试符号：$SYMBOLS"
  fi
fi
echo "${NAME}.app：${APP}（build ${SCYLLAUI_BUILD_ID}）"
