// GRIM — Guardian of RAM in Termux
// OOM-killer для процессов Termux: убивает «молчаливые» тяжёлые процессы,
// чтобы они не повесили телефон. Denylist защищает выбранные процессы (ботов).
//
// Подкоманды:
//   grim boot        — запускается Termux:Boot: respects `grim stop`, иначе стартует гипервизор
//   grim start       — снять стоп-файл и запустить гипервизор (вручную)
//   grim stop        — остановить GRIM (гипервизор гасит демон и выходит; стоп переживает ребут)
//   grim hypervisor  — супервизор: держит `grim daemon` живым, перезапускает при падении
//   grim daemon      — сам OOM-killer (цикл сканирования /proc)
//   grim scan        — таблица наших процессов (pid, RSS, тишина, защита), ничего не убивает
//   grim check       — один проход правил; печатает, кого бы убил (dry-run)
//   grim status      — конфиг, pid-файлы, последние строки лога
//   grim deny list|add <glob>|rm <glob>|test <строка|pid>
//
// Env-overrides (удобно для тестов): GRIM_RSS_LIMIT_MB, GRIM_SILENT_SECONDS,
// GRIM_CHECK_INTERVAL, GRIM_TOTAL_LIMIT_MB, GRIM_GRACE_SECONDS, GRIM_DRY_RUN,
// GRIM_ONLY_MATCH (подстрока cmdline — оценивать только совпадающие pid)

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[link(name = "c")]
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn getuid() -> u32;
    fn getppid() -> i32;
    fn setpriority(which: i32, who: i32, prio: i32) -> i32;
}
const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;
const SIG0: i32 = 0;
const PRIO_PROCESS: i32 = 0;

/// Максимальный приоритет для GRIM (nice -20). На этом ядре Android
/// отрицательный nice для своих процессов разрешён; если нет — EPERM, работаем дальше.
fn set_max_priority() -> bool {
    unsafe { setpriority(PRIO_PROCESS, 0, -20) == 0 }
}

/// Утопить жертву в самый низкий приоритет (nice +19) — легально для своего uid.
/// Телефон получает воздух немедленно, ещё до SIGTERM/KILL.
fn renice_lowest(pid: u32) -> bool {
    unsafe { setpriority(PRIO_PROCESS, pid as i32, 19) == 0 }
}

// ---------- пути и конфиг ----------

fn grim_home() -> PathBuf {
    if let Ok(h) = env::var("GRIM_HOME") {
        return PathBuf::from(h);
    }
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| ".".into())).join(".grim")
}
fn cfg_path() -> PathBuf { grim_home().join("config") }
fn denylist_path() -> PathBuf { grim_home().join("denylist") }
fn log_path() -> PathBuf { grim_home().join("grim.log") }
fn run_dir() -> PathBuf { grim_home().join("run") }
fn stop_file() -> PathBuf { run_dir().join("stop") }
fn hyp_pid_file() -> PathBuf { run_dir().join("hypervisor.pid") }
fn daemon_pid_file() -> PathBuf { run_dir().join("daemon.pid") }

#[derive(Clone)]
struct Config {
    rss_limit_mb: u64,
    silent_seconds: u64,
    check_interval: u64,
    total_limit_mb: u64,
    total_floor_mb: u64,
    total_cooldown_seconds: u64,
    grace_seconds: u64,
    dry_run: bool,
    only_match: Option<String>,
}

impl Config {
    fn load() -> Config {
        let mut kv: HashMap<String, String> = HashMap::new();
        if let Ok(s) = fs::read_to_string(cfg_path()) {
            for line in s.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') { continue }
                if let Some((k, v)) = line.split_once('=') {
                    kv.insert(k.trim().to_lowercase(), v.trim().to_string());
                }
            }
        }
        let get_u64 = |k: &str, default: u64| -> u64 {
            kv.get(k).and_then(|v| v.parse().ok()).unwrap_or(default)
        };
        let get_bool = |k: &str, default: bool| -> bool {
            kv.get(k).map(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"))
                .unwrap_or(default)
        };
        let mut c = Config {
            rss_limit_mb: get_u64("rss_limit_mb", 700),
            silent_seconds: get_u64("silent_seconds", 90),
            check_interval: get_u64("check_interval", 5),
            total_limit_mb: get_u64("total_limit_mb", 4096),
            total_floor_mb: get_u64("total_floor_mb", 150),
            total_cooldown_seconds: get_u64("total_cooldown_seconds", 60),
            grace_seconds: get_u64("grace_seconds", 3),
            dry_run: get_bool("dry_run", false),
            only_match: None,
        };
        // env-overrides
        let eu = |k: &str| env::var(k).ok().and_then(|v| v.parse::<u64>().ok());
        if let Some(v) = eu("GRIM_RSS_LIMIT_MB") { c.rss_limit_mb = v }
        if let Some(v) = eu("GRIM_SILENT_SECONDS") { c.silent_seconds = v }
        if let Some(v) = eu("GRIM_CHECK_INTERVAL") { c.check_interval = v }
        if let Some(v) = eu("GRIM_TOTAL_LIMIT_MB") { c.total_limit_mb = v }
        if let Some(v) = eu("GRIM_TOTAL_FLOOR_MB") { c.total_floor_mb = v }
        if let Some(v) = eu("GRIM_TOTAL_COOLDOWN_SECONDS") { c.total_cooldown_seconds = v }
        if let Some(v) = eu("GRIM_GRACE_SECONDS") { c.grace_seconds = v }
        if let Ok(v) = env::var("GRIM_DRY_RUN") {
            c.dry_run = matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on");
        }
        c.only_match = env::var("GRIM_ONLY_MATCH").ok().filter(|s| !s.is_empty());
        c
    }
}

// ---------- denylist: glob с * и ?, иначе — подстрока, всё регистронезависимо ----------

fn load_denylist() -> Vec<String> {
    fs::read_to_string(denylist_path())
        .unwrap_or_default()
        .lines()
        // отрезаем inline-комментарий (#) — правило не должно его содержать
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.to_lowercase())
        .collect()
}

fn glob_ci(pat: &str, text: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star_pi = usize::MAX;
    let mut star_ti = 0usize;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1; ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star_pi = pi; star_ti = ti; pi += 1;
        } else if star_pi != usize::MAX {
            pi = star_pi + 1; star_ti += 1; ti = star_ti;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' { pi += 1 }
    pi == p.len()
}

fn pattern_matches(pat: &str, text: &str) -> bool {
    if pat.contains(['*', '?']) {
        glob_ci(pat, &text.to_lowercase())
    } else {
        text.to_lowercase().contains(pat)
    }
}

fn denylist_hit(denylist: &[String], cmdline: &str, cwd: &str, exe: &str) -> Option<String> {
    for pat in denylist {
        if pattern_matches(pat, cmdline) || pattern_matches(pat, cwd) || pattern_matches(pat, exe) {
            return Some(pat.clone());
        }
    }
    None
}

// ---------- /proc ----------

#[derive(Clone)]
struct ProcInfo {
    pid: u32,
    #[allow(dead_code)]
    name: String,
    cmdline: String,
    cwd: String,
    exe: String,
    rss_kb: u64,
    ticks: u64,
    state: char,
}

fn read_proc(pid: u32) -> Option<ProcInfo> {
    let base = format!("/proc/{pid}");
    let status = fs::read_to_string(format!("{base}/status")).ok()?;
    let mut name = String::new();
    let mut rss_kb = 0u64;
    let mut uid = u32::MAX;
    for line in status.lines() {
        if let Some(v) = line.strip_prefix("Name:") {
            name = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("VmRSS:") {
            rss_kb = v.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("Uid:") {
            uid = v.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(u32::MAX);
        }
    }
    if uid != unsafe { getuid() } { return None; } // только свои процессы

    // /proc/pid/stat: pid (comm) state ... utime(14) stime(15)
    let stat = fs::read_to_string(format!("{base}/stat")).ok()?;
    let rparen = stat.rfind(')')?;
    let rest: Vec<&str> = stat[rparen + 1..].split_whitespace().collect();
    let state = rest.first().and_then(|s| s.chars().next()).unwrap_or('?');
    let utime: u64 = rest.get(11).and_then(|s| s.parse().ok()).unwrap_or(0);
    let stime: u64 = rest.get(12).and_then(|s| s.parse().ok()).unwrap_or(0);

    let raw_cmd = fs::read(format!("{base}/cmdline")).unwrap_or_default();
    let mut cmdline = String::from_utf8_lossy(&raw_cmd).replace('\0', " ").trim().to_string();
    if cmdline.is_empty() { cmdline = name.clone() }

    let cwd = fs::read_link(format!("{base}/cwd")).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
    let exe = fs::read_link(format!("{base}/exe"))
        .ok()
        .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
        .unwrap_or_default();

    Some(ProcInfo { pid, name, cmdline, cwd, exe, rss_kb, ticks: utime + stime, state })
}

fn our_pids() -> Vec<u32> {
    let mut v = Vec::new();
    if let Ok(rd) = fs::read_dir("/proc") {
        for e in rd.flatten() {
            if let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) {
                v.push(pid);
            }
        }
    }
    v.sort_unstable();
    v
}

// ---------- лог / уведомления / kill ----------

fn ts() -> String {
    Command::new("date").arg("+%Y-%m-%d %H:%M:%S").output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| {
            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| format!("epoch:{}", d.as_secs())).unwrap_or_default()
        })
}

fn log_line(msg: &str) {
    let path = log_path();
    if let Ok(md) = fs::metadata(&path) {
        if md.len() > 1_000_000 {
            let _ = fs::rename(&path, path.with_extension("log.1"));
        }
    }
    if let Some(parent) = path.parent() { let _ = fs::create_dir_all(parent); }
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "[{}] {}", ts(), msg);
    }
}

fn notify(msg: &str, id: u32) {
    let _ = Command::new("termux-notification")
        .args(["--id", &id.to_string(), "--title", "GRIM — Guardian of RAM", "--content", msg])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .spawn();
}

fn state_of(pid: u32) -> Option<char> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rparen = stat.rfind(')')?;
    stat[rparen + 1..].split_whitespace().next().and_then(|s| s.chars().next())
}

fn alive(pid: u32) -> bool {
    if unsafe { kill(pid as i32, SIG0) } == 0 {
        return state_of(pid) != Some('Z');
    }
    false
}

fn slay(p: &ProcInfo, cfg: &Config, reason: String) {
    let rss_mb = p.rss_kb / 1024;
    let short: String = p.cmdline.chars().take(140).collect();
    let msg = format!("Process killed by GRIM: {short} (pid {}, RSS {} MB; {reason})", p.pid, rss_mb);

    if cfg.dry_run {
        println!("[DRY-RUN] {msg}");
        log_line(&format!("DRY-RUN would kill: {msg}"));
        return;
    }

    // 1) сначала дописываем своё сообщение туда, куда процесс выводил данные
    let line = format!("{msg}\n");
    let fd1 = fs::read_link(format!("/proc/{}/fd/1", p.pid)).ok();
    let fd2 = fs::read_link(format!("/proc/{}/fd/2", p.pid)).ok();
    let write_fd = |fd: &str| {
        let _ = fs::OpenOptions::new().append(true)
            .open(format!("/proc/{}/fd/{fd}", p.pid))
            .and_then(|mut f| f.write_all(line.as_bytes()));
    };
    if fd1.is_some() && fd1 == fd2 {
        write_fd("1"); // тот же файл/tty/пайп — не дублируем строку
    } else {
        if fd1.is_some() { write_fd("1"); }
        if fd2.is_some() { write_fd("2"); }
    }
    // 1.5) утопить жертву на дно приоритета — CPU сразу уходит процессу
    if !renice_lowest(p.pid) {
        log_line(&format!("warn: renice +19 pid {} failed", p.pid));
    }
    // 2) TERM, ждём grace, потом KILL
    unsafe { kill(p.pid as i32, SIGTERM); }
    let deadline = Instant::now() + Duration::from_secs(cfg.grace_seconds.max(1));
    while Instant::now() < deadline {
        if !alive(p.pid) { break }
        std::thread::sleep(Duration::from_millis(100));
    }
    if alive(p.pid) {
        unsafe { kill(p.pid as i32, SIGKILL); }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !alive(p.pid) { break }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    log_line(&msg);
    notify(&msg, p.pid); // daemon's stdout уходит в тот же лог — без println!
}

// ---------- ядро: один проход правил ----------

#[derive(Clone, Copy, PartialEq)]
enum Mode { Scan, Check, Daemon }

struct PassState {
    ticks: HashMap<u32, (u64, u64)>, // pid -> (prev_ticks, silent_since)
    last_pressure: u64,              // epoch сек, когда клапан последний раз убил/предупредил
}
impl PassState {
    fn new() -> Self { PassState { ticks: HashMap::new(), last_pressure: 0 } }
}

fn pass(cfg: &Config, mode: Mode, state: &mut PassState) {
    let ticks_map = &mut state.ticks;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let denylist = load_denylist();
    let self_pid = std::process::id();
    let parent_pid = unsafe { getppid() } as u32;

    let mut procs: Vec<ProcInfo> = Vec::new();
    let mut total_kb = 0u64;
    for pid in our_pids() {
        let Some(info) = read_proc(pid) else { continue };
        if info.state == 'Z' { continue }
        if let Some(m) = &cfg.only_match {
            if !info.cmdline.to_lowercase().contains(&m.to_lowercase()) { continue }
        }
        procs.push(info);
    }
    for p in &procs { total_kb += p.rss_kb }

    let mut killed: Vec<u32> = Vec::new();

    // правило 1: RSS > лимит + процесс молчит (CPU не идёт) silent_seconds
    for p in &procs {
        let prev = ticks_map.get(&p.pid).copied();
        let prev_ticks = prev.map(|(t, _)| t).unwrap_or(0);
        let prev_silent = prev.map(|(_, s)| s).unwrap_or(0);
        let delta = p.ticks.saturating_sub(prev_ticks);
        let silent_since = if delta > 2 { 0 } else if prev_silent != 0 { prev_silent } else { now };
        ticks_map.insert(p.pid, (p.ticks, silent_since));
        if mode == Mode::Scan { continue }

        if p.pid == self_pid || p.pid == parent_pid { continue }
        if denylist_hit(&denylist, &p.cmdline, &p.cwd, &p.exe).is_some() {
            continue; // denylist: процесс живёт
        }
        let rss_mb = p.rss_kb / 1024;
        if rss_mb > cfg.rss_limit_mb && silent_since != 0 {
            let silent_for = now - silent_since;
            if silent_for >= cfg.silent_seconds {
                slay(p, cfg, format!("RSS {} MB > {} MB, silent {}s", rss_mb, cfg.rss_limit_mb, silent_for));
                killed.push(p.pid);
                continue;
            }
        }
    }

    // правило 2 (аварийный клапан): суммарный RSS Termux превышен.
    // Важно: убиваем ТОЛЬКО если есть крупный кандидат (>= total_floor_mb),
    // в первую очередь молчащий, не чаще раза в total_cooldown_seconds.
    // Убийство 2-16 MB fish ради галочки ничего не освобождает — не делаем.
    if mode != Mode::Scan && cfg.total_limit_mb > 0 {
        let total_mb = total_kb / 1024;
        if total_mb > cfg.total_limit_mb && now.saturating_sub(state.last_pressure) >= cfg.total_cooldown_seconds {
            let mut cands: Vec<&ProcInfo> = procs.iter()
                .filter(|p| !killed.contains(&p.pid))
                .filter(|p| p.pid != self_pid && p.pid != parent_pid)
                .filter(|p| p.rss_kb / 1024 >= cfg.total_floor_mb)
                .filter(|p| denylist_hit(&denylist, &p.cmdline, &p.cwd, &p.exe).is_none())
                .collect();
            // молчащие — в первую очередь, затем по RSS
            cands.sort_by(|a, b| {
                let sa = ticks_map.get(&a.pid).map(|(_, s)| if *s != 0 { 1 } else { 0 }).unwrap_or(0);
                let sb = ticks_map.get(&b.pid).map(|(_, s)| if *s != 0 { 1 } else { 0 }).unwrap_or(0);
                sb.cmp(&sa).then(b.rss_kb.cmp(&a.rss_kb))
            });
            state.last_pressure = now; // и kill, и предупреждение — под один cooldown
            match cands.first() {
                Some(v) => {
                    slay(v, cfg, format!("total Termux RSS {total_mb} MB > {} MB (floor {} MB)", cfg.total_limit_mb, cfg.total_floor_mb));
                    killed.push(v.pid);
                }
                None => {
                    log_line(&format!(
                        "total Termux RSS {total_mb} MB > {} MB, но все крупные процессы защищены denylist или легче floor {} MB — ничего не трогаю",
                        cfg.total_limit_mb, cfg.total_floor_mb
                    ));
                }
            }
        }
    }

    if mode == Mode::Scan {
        procs.sort_by_key(|p| std::cmp::Reverse(p.rss_kb));
        println!("{:>6} {:>8} {:>10} {:>8}  {:<9} {}", "PID", "RSS MB", "CPU TICKS", "SILENT s", "PROTECT", "COMMAND");
        for p in &procs {
            let (_, silent_since) = ticks_map.get(&p.pid).copied().unwrap_or((0, 0));
            let silent_for = if silent_since == 0 { 0 } else { now.saturating_sub(silent_since) };
            let prot = if p.pid == self_pid { "self".to_string() } else {
                match denylist_hit(&denylist, &p.cmdline, &p.cwd, &p.exe) {
                    Some(pat) => format!("deny:{pat}"),
                    None => "killable".to_string(),
                }
            };
            let cmd: String = p.cmdline.chars().take(70).collect();
            println!("{:>6} {:>8} {:>10} {:>8}  {:<9} {}", p.pid, p.rss_kb / 1024, p.ticks, silent_for, prot, cmd);
        }
        println!("\nВсего: {} процессов, суммарно {} MB RSS", procs.len(), total_kb / 1024);
    }

    // чистим карту от умерших pid
    let live: Vec<u32> = procs.iter().map(|p| p.pid).collect();
    ticks_map.retain(|pid, _| live.contains(pid));
}

// ---------- демон и гипервизор ----------

fn pid_alive_named(pidfile: &Path, needle: &str) -> bool {
    let Ok(s) = fs::read_to_string(pidfile) else { return false };
    let Ok(pid) = s.trim().parse::<i32>() else { return false };
    if unsafe { kill(pid, SIG0) } != 0 { return false }
    let cmd = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    String::from_utf8_lossy(&cmd).contains(needle)
}

fn write_pid_file(path: &Path) {
    let _ = fs::create_dir_all(run_dir());
    let _ = fs::write(path, format!("{}\n", std::process::id()));
}

fn terminate_child(child: &mut std::process::Child) {
    let pid = child.id() as i32;
    unsafe { kill(pid, SIGTERM); }
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            _ => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn cmd_daemon() {
    let cfg = Config::load();
    let _ = fs::create_dir_all(run_dir());
    write_pid_file(&daemon_pid_file());
    let prio = if set_max_priority() { "nice=-20" } else { "nice=-20 rejected (EPERM), staying at current" };
    log_line(&format!("daemon started (rss_limit={}MB silent={}s total={}MB dry_run={}, {prio})",
        cfg.rss_limit_mb, cfg.silent_seconds, cfg.total_limit_mb, cfg.dry_run));
    let mut state = PassState::new();
    loop {
        if stop_file().exists() {
            log_line("daemon: stop file found, exiting");
            break;
        }
        pass(&cfg, Mode::Daemon, &mut state);
        std::thread::sleep(Duration::from_secs(cfg.check_interval.max(1)));
    }
    let _ = fs::remove_file(daemon_pid_file());
}

fn cmd_hypervisor() {
    if stop_file().exists() {
        println!("GRIM: stop file present — not arming (run `grim start` to re-arm)");
        return;
    }
    let _ = fs::create_dir_all(run_dir());
    if pid_alive_named(&hyp_pid_file(), "grim") {
        println!("GRIM hypervisor already running");
        return;
    }
    let _ = Command::new("termux-wake-lock").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    write_pid_file(&hyp_pid_file());
    let prio = if set_max_priority() { "nice=-20" } else { "nice=-20 rejected (EPERM), staying at current" };
    log_line(&format!("hypervisor armed (pid {}, {prio})", std::process::id()));

    let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("grim"));
    loop {
        if stop_file().exists() {
            log_line("hypervisor: stop file — shutting down");
            break;
        }
        let log = fs::OpenOptions::new().create(true).append(true).open(log_path()).ok();
        let log2 = log.as_ref().map(|f| f.try_clone().ok()).flatten();
        let mut cmd = Command::new(&exe);
        cmd.arg("daemon").stdin(Stdio::null());
        match log {
            Some(f) => { cmd.stdout(Stdio::from(f)); }
            None => { cmd.stdout(Stdio::null()); }
        }
        match log2 {
            Some(f) => { cmd.stderr(Stdio::from(f)); }
            None => { cmd.stderr(Stdio::null()); }
        }
        match cmd.spawn() {
            Ok(mut child) => {
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            log_line(&format!("hypervisor: daemon exited ({status}); restart in 3s"));
                            break;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            log_line(&format!("hypervisor: try_wait error: {e}; restarting"));
                            break;
                        }
                    }
                    if stop_file().exists() {
                        log_line("hypervisor: stop file — terminating daemon");
                        terminate_child(&mut child);
                        let _ = fs::remove_file(daemon_pid_file());
                        let _ = fs::remove_file(hyp_pid_file());
                        log_line("hypervisor: stopped by user");
                        return;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
                // backoff, но смотрим стоп-файл каждую секунду
                for _ in 0..3 {
                    if stop_file().exists() { break }
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
            Err(e) => {
                log_line(&format!("hypervisor: failed to spawn daemon: {e}; retry in 5s"));
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    }
    let _ = fs::remove_file(hyp_pid_file());
}

fn spawn_detached_hypervisor() -> std::io::Result<u32> {
    let exe = env::current_exe()?;
    let log = fs::OpenOptions::new().create(true).append(true).open(log_path())?;
    let log2 = log.try_clone()?;
    // setsid есть в toybox; если нет — nohup
    let use_setsid = Path::new("/system/bin/setsid").exists();
    let mut cmd = if use_setsid { Command::new("/system/bin/setsid") } else { Command::new("setsid") };
    cmd.arg(&exe).arg("hypervisor")
        .stdin(Stdio::null()).stdout(Stdio::from(log)).stderr(Stdio::from(log2));
    if !use_setsid {
        // fallback: прямой спавн, stdout/stderr уже в лог
        let mut c2 = Command::new(&exe);
        c2.arg("hypervisor").stdin(Stdio::null());
        return c2.spawn().map(|ch| ch.id());
    }
    cmd.spawn().map(|ch| ch.id())
}

// ---------- CLI ----------

fn cmd_start(respect_stop: bool) {
    if respect_stop && stop_file().exists() {
        println!("GRIM: остановлен (`grim stop`), пропускаю автозапуск. `grim start` — включить.");
        return;
    }
    let _ = fs::create_dir_all(run_dir());
    if !respect_stop {
        let _ = fs::remove_file(stop_file());
    }
    let _ = Command::new("termux-wake-lock").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    match spawn_detached_hypervisor() {
        Ok(pid) => println!("GRIM armed (hypervisor pid {pid}). Лог: {}", log_path().display()),
        Err(e) => eprintln!("GRIM: не удалось запустить гипервизор: {e}"),
    }
}

fn cmd_stop() {
    let _ = fs::create_dir_all(run_dir());
    let _ = fs::write(stop_file(), format!("{}\n", std::process::id()));
    for _ in 0..60 {
        if !hyp_pid_file().exists() && !pid_alive_named(&daemon_pid_file(), "grim") { break }
        std::thread::sleep(Duration::from_millis(100));
    }
    // подстраховка: если демон всё ещё жив — TERM ему
    if pid_alive_named(&daemon_pid_file(), "grim") {
        if let Ok(s) = fs::read_to_string(daemon_pid_file()) {
            if let Ok(pid) = s.trim().parse::<i32>() {
                unsafe { kill(pid, SIGTERM); }
            }
        }
    }
    println!("GRIM остановлен. Переживёт ребут. Включить обратно: grim start");
}

fn cmd_status() {
    let cfg = Config::load();
    println!("GRIM — Guardian of RAM in Termux");
    println!("  конфиг:      {}", cfg_path().display());
    println!("  rss_limit:   {} MB   silent: {}s   interval: {}s   total_limit: {}MB   floor: {}MB   cooldown: {}s   dry_run: {}",
        cfg.rss_limit_mb, cfg.silent_seconds, cfg.check_interval, cfg.total_limit_mb, cfg.total_floor_mb, cfg.total_cooldown_seconds, cfg.dry_run);
    println!("  denylist:    {} ({} правил)", denylist_path().display(), load_denylist().len());
    println!("  стоп-файл:   {}", if stop_file().exists() { "ЕСТЬ (GRIM выключен)" } else { "нет" });
    println!("  гипервизор:  {}", if pid_alive_named(&hyp_pid_file(), "grim") { "работает" } else { "НЕ работает" });
    println!("  демон:       {}", if pid_alive_named(&daemon_pid_file(), "grim") { "работает" } else { "НЕ работает" });
    println!("  лог:         {}", log_path().display());
    if let Ok(s) = fs::read_to_string(log_path()) {
        let lines: Vec<&str> = s.lines().collect();
        let start = lines.len().saturating_sub(8);
        println!("  --- последние строки лога ---");
        for l in &lines[start..] { println!("  {l}") }
    }
}

fn cmd_deny(args: &[String]) {
    match args.first().map(|s| s.as_str()) {
        Some("list") => {
            match fs::read_to_string(denylist_path()) {
                Ok(s) => {
                    for (i, l) in s.lines().enumerate() { println!("{:>3}  {}", i + 1, l) }
                }
                Err(_) => println!("(пусто — {})", denylist_path().display()),
            }
        }
        Some("add") => {
            let Some(pat) = args.get(1) else { eprintln!("usage: grim deny add <glob>"); return };
            let mut content = fs::read_to_string(denylist_path()).unwrap_or_default();
            if content.lines().any(|l| l.trim() == pat.trim()) { println!("уже есть: {pat}"); return }
            if !content.ends_with('\n') && !content.is_empty() { content.push('\n') }
            content.push_str(pat); content.push('\n');
            let _ = fs::write(denylist_path(), content);
            println!("добавлено в denylist: {pat}");
        }
        Some("rm") => {
            let Some(pat) = args.get(1) else { eprintln!("usage: grim deny rm <glob>"); return };
            match fs::read_to_string(denylist_path()) {
                Ok(s) => {
                    let kept: Vec<&str> = s.lines().filter(|l| l.trim() != pat.trim()).collect();
                    let _ = fs::write(denylist_path(), kept.join("\n") + "\n");
                    println!("удалено: {pat}");
                }
                Err(_) => eprintln!("denylist не найден"),
            }
        }
        Some("test") => {
            let Some(target) = args.get(1) else { eprintln!("usage: grim deny test <строка|pid>"); return };
            let dl = load_denylist();
            if let Ok(pid) = target.parse::<u32>() {
                match read_proc(pid) {
                    Some(p) => {
                        println!("pid {pid}: cmdline={}\n  cwd={}\n  exe={}", p.cmdline, p.cwd, p.exe);
                        match denylist_hit(&dl, &p.cmdline, &p.cwd, &p.exe) {
                            Some(pat) => println!("  -> ЗАЩИЩЁН правилом: {pat}"),
                            None => println!("  -> НЕ защищён (killable)"),
                        }
                    }
                    None => println!("pid {pid}: не наш процесс / нет в /proc"),
                }
            } else {
                match denylist_hit(&dl, target, target, target) {
                    Some(pat) => println!("совпадение с правилом: {pat}"),
                    None => println!("нет совпадений в denylist"),
                }
            }
        }
        _ => eprintln!("usage: grim deny list|add <glob>|rm <glob>|test <строка|pid>"),
    }
}

fn usage() {
    eprintln!("GRIM — Guardian of RAM in Termux. OOM-killer для Termux.

USAGE:
  grim boot          автозапуск (Termux:Boot): respects `grim stop`
  grim start         включить (снять стоп-файл, запустить гипервизор)
  grim stop          выключить до `grim start` (переживает ребут)
  grim status        конфиг, здоровье, последние строки лога
  grim scan          таблица процессов (read-only)
  grim check         один проход правил, печатает кандидатов на kill (dry-run)
  grim daemon        OOM-killer цикл (обычно под гипервизором)
  grim hypervisor    супервизор демона (перезапуск при падении)
  grim deny list|add|rm|test    управление denylist

Конфиг: ~/.grim/config   Правила защиты: ~/.grim/denylist   Лог: ~/.grim/grim.log");
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("boot") => cmd_start(true),
        Some("start") => cmd_start(false),
        Some("stop") => cmd_stop(),
        Some("status") => cmd_status(),
        Some("scan") => { let cfg = Config::load(); let mut st = PassState::new(); pass(&cfg, Mode::Scan, &mut st) }
        Some("check") => { let mut cfg = Config::load(); cfg.dry_run = true; let mut st = PassState::new(); pass(&cfg, Mode::Check, &mut st) }
        Some("daemon") => cmd_daemon(),
        Some("hypervisor") => cmd_hypervisor(),
        Some("deny") => cmd_deny(&args[1..]),
        _ => usage(),
    }
}
