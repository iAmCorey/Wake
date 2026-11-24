#!/usr/bin/env bash
# 构建 Linux 发行产物:release 二进制 + .desktop + SVG 图标,打成
# tar.gz(通用)与 .deb(Debian/Ubuntu)。在 Linux 上跑(CI 或容器);
# macOS 打包走 scripts/make-app.sh,两边互不认识对方的产物。
# 用法: scripts/make-linux.sh
set -euo pipefail
cd "$(dirname "$0")/.."

ASSETS=crates/wake/assets
TARGET_DIR=${CARGO_TARGET_DIR:-target}
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH=$(dpkg --print-architecture 2>/dev/null || uname -m)

# 1. release 构建(二进制名 Wake 来自 [[bin]],macOS 菜单栏需要;
#    Linux 装成小写 wake,CLI 惯例)
cargo build --release -p wake
cargo build --release -p wake-core --bin wake-mcp --bin wake-cli

STAGE=$(mktemp -d)/wake-${VERSION}-linux-${ARCH}
mkdir -p "$STAGE"
cp "$TARGET_DIR/release/Wake" "$STAGE/wake"
# 只读 MCP server,装在主程序旁边(Settings → Connect 按同目录找它)
cp "$TARGET_DIR/release/wake-mcp" "$STAGE/wake-mcp"
# 会话查询 CLI,同样并排
cp "$TARGET_DIR/release/wake-cli" "$STAGE/wake-cli"
cp "$ASSETS/icon.svg" "$STAGE/wake.svg"

# 2. .desktop(StartupWMClass 对齐 main.rs 的 app_id = "wake")。MimeType 把 wake:// 链接
# 登记给 Wake(#82),系统打开链接时以 `wake <链接>` 起它(%u),已在跑就由它转给那个窗口
cat > "$STAGE/wake.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=Wake
Comment=Browse, search and resume your coding agent sessions
Exec=wake %u
Icon=wake
Terminal=false
Categories=Development;Utility;
MimeType=x-scheme-handler/wake;
StartupWMClass=wake
DESKTOP

cat > "$STAGE/install.sh" <<'INSTALL'
#!/usr/bin/env bash
# 装进用户目录(不需要 root):~/.local/bin + .desktop + 图标
set -euo pipefail
cd "$(dirname "$0")"
install -Dm755 wake "$HOME/.local/bin/wake"
install -Dm755 wake-mcp "$HOME/.local/bin/wake-mcp"
install -Dm755 wake-cli "$HOME/.local/bin/wake-cli"
# 桌面项写绝对路径:~/.local/bin 若是本次新建的,图形会话的 PATH 里还没有它,
# Exec=wake 会找不到程序直到重新登录(deb 装 /usr/bin 不受此累,保持裸名)
sed "s|^Exec=wake %u$|Exec=\"$HOME/.local/bin/wake\" %u|" wake.desktop \
  | install -Dm644 /dev/stdin "$HOME/.local/share/applications/wake.desktop"
install -Dm644 wake.svg "$HOME/.local/share/icons/hicolor/scalable/apps/wake.svg"
command -v update-desktop-database >/dev/null && update-desktop-database "$HOME/.local/share/applications" || true
# wake:// 链接交给 Wake(deb 装的由系统按 .desktop 的 MimeType 自己认)
command -v xdg-mime >/dev/null && xdg-mime default wake.desktop x-scheme-handler/wake || true
echo "Installed. Make sure ~/.local/bin is on your PATH, then run: wake (or wake-cli --help)"
INSTALL
chmod +x "$STAGE/install.sh"

# 3. tar.gz
mkdir -p dist
tar -C "$(dirname "$STAGE")" -czf "dist/wake-${VERSION}-linux-${ARCH}.tar.gz" "$(basename "$STAGE")"

# 4. .deb(dpkg-deb 在则打;Depends 按 ldd 实测的动态链接面,
#    rusqlite/zstd 是 bundled 静态、不进清单)
if command -v dpkg-deb >/dev/null; then
  DEB=$(mktemp -d)/wake_deb
  mkdir -p "$DEB/DEBIAN"
  install -Dm755 "$STAGE/wake"         "$DEB/usr/bin/wake"
  install -Dm755 "$STAGE/wake-mcp"     "$DEB/usr/bin/wake-mcp"
  install -Dm755 "$STAGE/wake-cli"     "$DEB/usr/bin/wake-cli"
  install -Dm644 "$STAGE/wake.desktop" "$DEB/usr/share/applications/wake.desktop"
  install -Dm644 "$STAGE/wake.svg"     "$DEB/usr/share/icons/hicolor/scalable/apps/wake.svg"
  cat > "$DEB/DEBIAN/control" <<CONTROL
Package: wake
Version: ${VERSION}
Section: devel
Priority: optional
Architecture: ${ARCH}
Depends: libc6, libgcc-s1, libfreetype6, libfontconfig1, zlib1g, libxcb1, libxcb-xkb1, libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libvulkan1, mesa-vulkan-drivers, xdg-utils
Maintainer: Corey Chiu <iamcoreychiu@gmail.com>
Homepage: https://github.com/iAmCorey/Wake
Description: Coding agent session manager
 Browse, search and resume sessions from Claude Code, Codex and
 other local coding agents in one place.
CONTROL
  dpkg-deb --build --root-owner-group "$DEB" "dist/wake_${VERSION}_${ARCH}.deb" >/dev/null
fi

ls -lh dist/wake*${VERSION}* | awk '{print $NF, "("$5")"}'
