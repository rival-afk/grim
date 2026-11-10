#!/data/data/com.termux/files/usr/bin/bash
# GRIM — деинсталлятор.
set -uo pipefail
GRIM_HOME="${GRIM_HOME:-$HOME/.grim}"
PREFIX="${PREFIX:-/data/data/com.termux/files/usr}"

log() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }

command -v grim >/dev/null 2>&1 && grim stop || true
rm -f "$HOME/.termux/boot/00-grim.sh" && log "boot-хук удалён"
rm -f "$PREFIX/bin/grim" && log "симлинк удалён"
rm -f "$GRIM_HOME/bin/grim" && log "бинарь удалён"

read -r -p "Удалить $GRIM_HOME целиком (config, denylist, лог)? [y/N] " ans
if [ "${ans,,}" = "y" ]; then
    rm -rf "$GRIM_HOME"
    log "$GRIM_HOME удалён"
else
    log "$GRIM_HOME оставлен (там конфиг, denylist и лог)"
fi
log "GRIM удалён. wakelock снимется при остановке Termux:Settings → Apps → Termux → Force stop, либо adb shell dumpsys battery"
