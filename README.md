# GRIM — Guardian of RAM in Termux

OOM-killer для процессов Termux на **чистом Rust** (ноль внешних зависимостей).
Убивает процессы, которые **молча жрут память**, пока они не повесили телефон,
и вместо их вывода пишет своё:

```
Process killed by GRIM: python3 leaked_bot.py (pid 1234, RSS 812 MB; RSS 812 MB > 700 MB, silent 95s)
```

Работает без root, только со своими процессами (uid Termux).

## Возможности

- **Два правила убийства**
  1. per-process: RSS выше лимита **+** процесс не тратит CPU не меньше `silent_seconds`
  2. аварийный клапан: суммарный RSS всех процессов Termux выше `total_limit_mb` → убивается самый жирный незащищённый кандидат (приоритет — молчащим), но **только** не легче `total_floor_mb` и не чаще `total_cooldown_seconds`; если все крупные под denylist или легче floor — GRIM пишет в лог и **никого не трогает**
- **Denylist** — выбранные процессы (боты) GRIM не трогает **никогда**; правила — glob по `cmdline` / `cwd` / `exe`
- **Гипервизор** — супервизор, который держит демон живым (рестарт при падении) и переживает ребуты через Termux:Boot
- **Сообщение «вместо вывода»** — строка дописывается в stdout/stderr жертвы *до* kill, плюс лог и уведомление (termux-api)
- **Максимальный приоритет** — GRIM стартует с `nice -20` (где ядро это разрешает — мягкая деградация с записью в лог), а жертва ренайсится в `+19` за мгновение до TERM
- Чистый `/proc`, ни одного крейте, сборка `rustc -O` за ~4 секунды

## Установка

```bash
pkg install rust git
git clone https://github.com/rival-afk/grim.git
cd grim
./install.sh
```

Скрипт: поставит Rust при отсутствии, соберёт `~/.grim/bin/grim`, сделает симлинк в `$PREFIX/bin/grim`,
положит примеры `config`/`denylist` (не перезатирая существующие), поставит boot-хук и запустит GRIM.

### Требования

| Что | Зачем |
|---|---|
| [Termux](https://github.com/termux/termux-app) | очевидно |
| `pkg install rust` | сборка |
| [Termux:Boot](https://f-droid.org/packages/com.termux.boot/) | автозапуск после ребута |
| [Termux:API](https://f-droid.org/packages/com.termux.api/) (`pkg install termux-api`) | уведомления (опционально) |
| wakelock | `termux-wake-lock` вызывается сам, но его должен поддерживать Termux:Boot |

**Важно после установки:** Settings → Apps → Termux и Termux:Boot → Battery → **Unrestricted** (иначе Android убьёт/заморозит).

## Как это работает

GRIM сканирует `/proc` каждые `check_interval` секунд. Для каждого нашего процесса:

| Признак | Откуда |
|---|---|
| RSS | `VmRSS` из `/proc/<pid>/status` |
| Тик CPU | `utime + stime` из `/proc/<pid>/stat` |
| «Молчание» | delta тиков ≤ 2 за интервал, непрерывно ≥ `silent_seconds` |
| Команда/cwd/exe | `cmdline`, `readlink /proc/<pid>/cwd`, `readlink /proc/<pid>/exe` |

«Молчит» здесь = **не тратит CPU**, а не «нет сетевых запросов» — глухой busy-loop тоже считается.
Первый цикл после (пере)старта демона — накопление: таймер тишины начинается с 0.

Denylist важнее лимитов: совпавший процесс не будет убит ни при каких условиях.

### Сообщение «Process killed by GRIM …»

Перед `SIGTERM` GRIM дописывает строку в `/proc/<pid>/fd/1` и `fd/2`
(один раз, если оба fd указывают на один файл/tty/пайп), затем лог и
`termux-notification`. Если stdout процесса — tty, строка появится в терминале.
Далее: TERM → `grace_seconds` → KILL.

## Конфигурация: `~/.grim/config`

| Ключ | По умолчанию | Смысл |
|---|---|---|
| `rss_limit_mb` | 700 | лимит RSS для одного процесса |
| `silent_seconds` | 90 | сколько процесс обязан молчать перед kill |
| `check_interval` | 5 | период сканирования, сек |
| `total_limit_mb` | 4096 | клапан суммарного RSS Termux; `0` — выкл |
| `total_floor_mb` | 150 | клапан не трогает процессы легче этого |
| `total_cooldown_seconds` | 60 | минимум секунд между срабатываниями клапана |
| `grace_seconds` | 3 | ожидание между TERM и KILL |
| `dry_run` | false | `true` — только логировать «кого бы убил» |

Env-overrides (удобно для тестов): `GRIM_RSS_LIMIT_MB`, `GRIM_SILENT_SECONDS`,
`GRIM_CHECK_INTERVAL`, `GRIM_TOTAL_LIMIT_MB`, `GRIM_TOTAL_FLOOR_MB`, `GRIM_TOTAL_COOLDOWN_SECONDS`, `GRIM_GRACE_SECONDS`, `GRIM_DRY_RUN`,
`GRIM_ONLY_MATCH` (подстрока cmdline — оценивать только совпадающие pid).

## Denylist: `~/.grim/denylist`

Одно правило на строку, `#` — комментарий, регистр не важен. Правило проверяется по `cmdline`, `cwd`, `exe`:

```
# подстрока (без спецсимволов) — найдёт «python3 run_logged.py» по cwd:
astralis
stos
# glob с * или ? — должен совпасть ЦЕЛИКОМ:
python3 *bot*
*run_logged.py
```

Обычно достаточно добавить имя проекта из `~/projects` — cwd бота совпадёт.

Управление из терминала:

```bash
grim deny list          # правила с номерами
grim deny add my-bot    # добавить
grim deny rm my-bot     # удалить
grim deny test 28515    # защищён ли конкретный pid (или строка)
```

## Команды

```bash
grim scan       # таблица процессов: RSS / тики / тишина / защита (read-only)
grim check      # один проход правил — кто пойдёт под kill (dry-run)
grim status     # конфиг, здоровье демона/гипервизора, хвост лога
grim start      # включить (снять стоп-файл, поднять гипервизор)
grim stop       # выключить до `grim start`, переживает ребут
grim boot       # то же, что вызывается из Termux:Boot (уважает stop-файл)
grim deny ...   # denylist
```

## Автозапуск (Termux:Boot)

`install.sh` кладёт `~/.termux/boot/00-grim.sh`, который берёт `termux-wake-lock`,
уважает `grim stop` и поднимает гипервизор. Порядок по имени важен — `00-*` стартует первым.

Остановить/включить:

```bash
grim stop   # выключено — в т.ч. после ребута
grim start  # снова включено
```

## Приоритет CPU

- GRIM (гипервизор и демон) стартует с `setpriority(nice=-20)`. На ядрах, где
  приложению разрешён отрицательный nice для своих процессов (проверено на
  Android), GRIM получает максимальный приоритет. Если ядро отказывает —
  GRIM мягко работает на своём current nice и пишет это в лог.
- При kill жертва предварительно ренайсится в `+19` — CPU немедленно уходит
  делу, ещё до TERM.
- RT-класс (`SCHED_FIFO`) без root не выдаётся — это честно и не требуется:
  демон спит 5 секунд между проходами.

## Ограничения

- Только свои процессы — чужие приложения GRIM не видит и не трогает.
- Android `lmkd` — первая линия обороны; GRIM — вторая, внутри Termux.
- «Молчание» оценивается по CPU; процесс, который «молчит» внешне, но жрёт CPU
  (сложная компиляция), не будет убит правилом 1 — только клапаном 2 (если включён).
- Сразу после старта демона счётчик тишины пуст — первому kill нужно `silent_seconds`.

## Удаление

```bash
./uninstall.sh
```

## Лицензия

MIT © rival-afk
