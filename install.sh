#!/data/data/com.termux/files/usr/bin/bash
# GRIM — Guardian of RAM in Termux · установка одной командой.
# Собирает grim из src/main.rs, ставит в ~/.grim, подключает Termux:Boot.
set -euo pipefail

PREFIX="${PREFIX:-/data/data/com.termux/files/usr}"
GRIM_HOME="${GRIM_HOME:-$HOME/.grim}"
SRC_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

log()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mWARN\033[0m %s\n' "$*"; }

termux-wake-lock 2>/dev/null || true

if ! command -v rustc >/dev/null 2>&1; then
    log "rust не найден — ставлю (pkg install rust)..."
    pkg update -y && pkg install -y rust
fi

mkdir -p "$GRIM_HOME/bin" "$GRIM_HOME/src" "$GRIM_HOME/run"

log "собираю grim (rustc -O)..."
rustc --edition 2021 -C opt-level=2 -o "$GRIM_HOME/bin/grim" "$SRC_DIR/src/main.rs"
log "установлен: $GRIM_HOME/bin/grim"

ln -sf "$GRIM_HOME/bin/grim" "$PREFIX/bin/grim"
log "симлинк: $PREFIX/bin/grim -> $GRIM_HOME/bin/grim"

# примеры конфига — только если нет своих (не перезатираем!)
if [ ! -f "$GRIM_HOME/config" ];  then cp "$SRC_DIR/config.example"  "$GRIM_HOME/config";  log "создан $GRIM_HOME/config"; fi
if [ ! -f "$GRIM_HOME/denylist" ]; then cp "$SRC_DIR/denylist.example" "$GRIM_HOME/denylist"; log "создан $GRIM_HOME/denylist"; fi

# Termux:Boot
if [ -d "$HOME/.termux/boot" ]; then
    cp "$SRC_DIR/boot/00-grim.sh" "$HOME/.termux/boot/00-grim.sh"
    chmod 700 "$HOME/.termux/boot/00-grim.sh"
    log "boot-хук: ~/.termux/boot/00-grim.sh"
    warn "не забудь: Settings → Apps → Termux и Termux:Boot → Battery → Unrestricted"
else
    warn "~/.termux/boot не найден — установи Termux:Boot (F-Droid) и перезапусти install.sh"
fi

log "запускаю GRIM..."
"$GRIM_HOME/bin/grim" start
log "готово! Проверь: grim status"
