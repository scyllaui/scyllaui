#!/bin/sh
# 对外发布包（M4b 块 1，docs/design.md 10.6 节「块 1 实现」）：把 build-app.sh Release 产出的正式版公证、盖票据、打成 DMG。
# 用法：src/scripts/release.sh [--prepare-only]
#   --prepare-only：只做公证前的检查与打包（拷贝、zip），不向 Apple 提交，用来演练；产物在 output/release/<版本>-prepare/。
# 前置：src/scripts/build-app.sh Release 已在干净的提交上构建（构建标识不带 -dirty）；钥匙串里有 Developer ID 身份与
#   notarytool 配置 scyllaui-notary（xcrun notarytool store-credentials 建立）。脚本只引用配置名，不接触密码。
# 产物：output/release/<版本>/ScyllaUI.app（已盖票据的拷贝）、ScyllaUI-<版本>.dmg、公证结果 notary-*.json 与失败时的日志、
#   appcast-item.xml（更新签名与下载地址，M4b 块 3）。
# 不动构建目录里的正式版（正在运行的那份）；同一版本的产物目录已存在时拒绝，避免覆盖已发出去的包。
set -eu

PREPARE_ONLY=0
[ "${1:-}" = "--prepare-only" ] && PREPARE_ONLY=1

PROJECT_DIR=$(cd "$(dirname "$0")/../.." && pwd)
SRC_APP="$PROJECT_DIR/output/build/Build/Products/Release/ScyllaUI.app"
TEAM_ID="94A284J775"
PROFILE="scyllaui-notary"

fail() { echo "错误：$*" >&2; exit 1; }

# 1. 只接受干净构建的、签名正确的正式版。
[ -d "$SRC_APP" ] || fail "没有正式版：先运行 src/scripts/build-app.sh Release"
BUILD_ID=$("$SRC_APP/Contents/Helpers/scyllauid" --build-id)
case "$BUILD_ID" in *-dirty*) fail "正式版是带未提交改动构建的（${BUILD_ID}），先提交再用 build-app.sh Release 重建" ;; esac
# GPL 要求发出去的程序有对应的公开源码：维护者本机有发布工具时，核对公开分支的内容等于这次构建所在提交的公开部分。
if [ "$PREPARE_ONLY" = 0 ] && [ -f "$PROJECT_DIR/publish/publish.py" ]; then
  python3 -I "$PROJECT_DIR/publish/publish.py" --check "$BUILD_ID" || fail "公开源码还没发布到这次构建的提交（${BUILD_ID}）"
fi
VERSION=$(/usr/libexec/PlistBuddy -c "Print CFBundleShortVersionString" "$SRC_APP/Contents/Info.plist")
BUILD_NUMBER=$(/usr/libexec/PlistBuddy -c "Print CFBundleVersion" "$SRC_APP/Contents/Info.plist")
SIGN_ID=$(security find-identity -v -p codesigning | sed -n "s/.*\"\(Developer ID Application: .*($TEAM_ID)\)\".*/\1/p" | head -1)
[ -n "$SIGN_ID" ] || fail "钥匙串里没有有效的 Developer ID 身份（${TEAM_ID}）"
codesign --verify --deep --strict "$SRC_APP" || fail "正式版签名校验失败"
for f in "$SRC_APP" "$SRC_APP/Contents/Helpers/scyllauid" "$SRC_APP/Contents/Helpers/scyllaui"; do
  info=$(codesign -dvv "$f" 2>&1)
  echo "$info" | grep -q "^TeamIdentifier=$TEAM_ID$" || fail "$f 不是本团队签名"
  echo "$info" | grep -Eq "flags=.*runtime" || fail "$f 没有开硬化运行时"
  echo "$info" | grep -q "^Timestamp=" || fail "$f 没有安全时间戳（公证要求），用 build-app.sh Release 重建"
done
if codesign -d --entitlements - --xml "$SRC_APP" 2>/dev/null | grep -q "get-task-allow"; then
  fail "正式版带 get-task-allow，不能公证"
fi

OUT="$PROJECT_DIR/output/release/$VERSION"
# 演练写到单独的目录，不占正式产物的位置（正式产物目录已存在即拒绝）。
[ "$PREPARE_ONLY" = 1 ] && OUT="$OUT-prepare" && rm -rf "$OUT"
[ ! -e "$OUT" ] || fail "$OUT 已存在：同一版本只发一次，先升版本号（或确认旧产物作废后自行移走）"
mkdir -p "$OUT"
echo "ScyllaUI ${VERSION}（构建号 ${BUILD_NUMBER}，构建标识 ${BUILD_ID}）→ $OUT"

# 2. 拷一份再处理，盖票据会改动包，不碰正在运行的正式版。
APP="$OUT/ScyllaUI.app"
ditto "$SRC_APP" "$APP"
ZIP="$OUT/ScyllaUI-$VERSION-app.zip"
ditto -c -k --keepParent "$APP" "$ZIP"

if [ "$PREPARE_ONLY" = 1 ]; then
  echo "演练完成（未提交公证）：$ZIP"
  exit 0
fi

# 提交公证并等结果；不通过时拉回日志。
notarize() {
  what="$1" file="$2" tag="$3"
  echo "提交公证：$what"
  xcrun notarytool submit "$file" --keychain-profile "$PROFILE" --wait --output-format json > "$OUT/notary-$tag.json" \
    || true
  status=$(plutil -extract status raw -o - "$OUT/notary-$tag.json" 2>/dev/null || echo "")
  id=$(plutil -extract id raw -o - "$OUT/notary-$tag.json" 2>/dev/null || echo "")
  if [ "$status" != "Accepted" ]; then
    [ -n "$id" ] && xcrun notarytool log "$id" --keychain-profile "$PROFILE" "$OUT/notary-$tag-log.json" || true
    fail "$what 公证未通过（状态 ${status:-未知}，提交 ${id:-无}）；结果见 $OUT/notary-$tag.json"
  fi
  echo "公证通过：${what}（提交 ${id}）"
}

# 3. App：公证、盖票据。
notarize "ScyllaUI.app" "$ZIP" app
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"

# 4. DMG：只放 App 与指向「应用程序」的链接；签名后再公证、盖票据，离线第一次打开也不被拦。
STAGE="$OUT/dmg-stage"
mkdir -p "$STAGE"
ditto "$APP" "$STAGE/ScyllaUI.app"
ln -s /Applications "$STAGE/Applications"
DMG="$OUT/ScyllaUI-$VERSION.dmg"
# macOS 27 起 hdiutil create 标为弃用，改用 diskutil image（APFS 卷，macOS 10.13 起都能挂载，覆盖最低支持的 14）。
diskutil image create from --format UDZO --volumeName "ScyllaUI $VERSION" "$STAGE" "$DMG" >/dev/null
rm -rf "$STAGE"
codesign --force --sign "$SIGN_ID" --timestamp "$DMG"
notarize "ScyllaUI-$VERSION.dmg" "$DMG" dmg
xcrun stapler staple "$DMG"
xcrun stapler validate "$DMG"

# 5. 校验：Gatekeeper 对 App 与 DMG 的评估。
spctl -a -vv -t exec "$APP" 2>&1 | tee "$OUT/spctl-app.txt"
grep -q "source=Notarized Developer ID" "$OUT/spctl-app.txt" || fail "App 的 Gatekeeper 评估不是 Notarized Developer ID"
spctl -a -vv -t open --context context:primary-signature "$DMG" 2>&1 | tee "$OUT/spctl-dmg.txt"
grep -q "accepted" "$OUT/spctl-dmg.txt" || fail "DMG 的 Gatekeeper 评估未通过"
shasum -a 256 "$DMG" | tee "$OUT/SHA256.txt"

# 6. 更新签名与 appcast 条目（M4b 块 3）：盖完票据之后再签（盖票据会改动 DMG）。私钥在用户的登录钥匙串里，
#    sign_update 读取时系统可能弹授权框；脚本只拿到签名与长度，不接触私钥。只写文件，不上传（上线 appcast 是公开操作）。
SIGN_UPDATE="$PROJECT_DIR/.tools/sparkle/2.10.1/bin/sign_update"
[ -x "$SIGN_UPDATE" ] || fail "缺少 ${SIGN_UPDATE}，先运行 src/scripts/fetch-sparkle.sh"
ED=$("$SIGN_UPDATE" "$DMG") || fail "sign_update 签名失败（钥匙串里有没有 generate_keys 生成的私钥？）"
case "$ED" in *'sparkle:edSignature="'*'length="'*) ;; *) fail "sign_update 输出不认识：$ED" ;; esac
PUBDATE=$(LC_ALL=C date -u "+%a, %d %b %Y %H:%M:%S +0000")
cat > "$OUT/appcast-item.xml" <<ITEM
<item>
    <title>ScyllaUI $VERSION</title>
    <pubDate>$PUBDATE</pubDate>
    <sparkle:version>$BUILD_NUMBER</sparkle:version>
    <sparkle:shortVersionString>$VERSION</sparkle:shortVersionString>
    <sparkle:minimumSystemVersion>14.0</sparkle:minimumSystemVersion>
    <enclosure url="https://github.com/scyllaui/scyllaui/releases/download/v$VERSION/ScyllaUI-$VERSION.dmg" $ED type="application/octet-stream"/>
</item>
ITEM
echo "appcast 条目：$OUT/appcast-item.xml"
echo "完成：$DMG"
