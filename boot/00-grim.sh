#!/data/data/com.termux/files/usr/bin/bash
# Termux:Boot — запуск GRIM (Guardian of RAM in Termux) при загрузке телефона.
# GRIM следит за процессами Termux и убивает тяжёлые «молчащие», чтобы
# они не повесили телефон. Защищённые процессы — ~/.grim/denylist.
export HOME="${HOME:-/data/data/com.termux/files/home}"
export PATH="/data/data/com.termux/files/usr/bin:$PATH"
export PREFIX="${PREFIX:-/data/data/com.termux/files/usr}"

GRIM="$HOME/.grim/bin/grim"
LOG="$HOME/.grim/grim.log"
[ -x "$GRIM" ] || exit 0

termux-wake-lock 2>/dev/null

mkdir -p "$HOME/.grim/run"

# grim stop переживает ребут: если GRIM остановлен пользователем — не поднимать
if [ -f "$HOME/.grim/run/stop" ]; then
    echo "$(date): GRIM disabled via 'grim stop' — skipping boot" >> "$LOG"
    exit 0
fi

"$GRIM" boot >> "$LOG" 2>&1
exit 0
