//! Everything that talks to the VPS. One SSH session, reused for every poll.

use crate::config::{Auth, Config, PalworldConfig};
use crate::model::*;
use crate::palworld::PalIni;
use anyhow::{Context, Result, bail};
use async_ssh2_tokio::client::{AuthMethod, Client, ServerCheckMethod};
use std::time::Duration;

pub struct Vps {
    client: Client,
}

/// Wrap a value so the remote shell sees it as one literal argument.
fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

impl Vps {
    pub async fn connect(cfg: &Config) -> Result<Self> {
        let auth = match &cfg.ssh.auth {
            Auth::Key { path, passphrase } => {
                let pass = (!passphrase.is_empty()).then_some(passphrase.as_str());
                AuthMethod::with_key_file(path, pass)
            }
            Auth::Password { password } => AuthMethod::with_password(password),
        };
        let check = if cfg.ssh.strict_host_key {
            ServerCheckMethod::DefaultKnownHostsFile
        } else {
            ServerCheckMethod::NoCheck
        };

        let client = Client::connect(
            (cfg.ssh.host.as_str(), cfg.ssh.port),
            &cfg.ssh.user,
            auth,
            check,
        )
        .await
        .with_context(|| format!("SSH connect to {}@{}", cfg.ssh.user, cfg.ssh.host))?;

        Ok(Self { client })
    }

    /// Run a command; non-zero exit is an error carrying stderr.
    async fn run(&self, cmd: &str) -> Result<String> {
        let out = self
            .client
            .execute(cmd)
            .await
            .with_context(|| format!("exec `{cmd}`"))?;
        if out.exit_status != 0 {
            bail!(
                "command failed (exit {}): {}\n{}",
                out.exit_status,
                cmd,
                out.stderr.trim()
            );
        }
        Ok(out.stdout)
    }

    /// Run a command, tolerating non-zero exit (e.g. `systemctl is-active` on a
    /// stopped unit exits 3).
    async fn run_lenient(&self, cmd: &str) -> Result<String> {
        let out = self
            .client
            .execute(cmd)
            .await
            .with_context(|| format!("exec `{cmd}`"))?;
        Ok(out.stdout)
    }

    /// One round-trip for every host metric. `/proc/stat` is sampled twice
    /// around a short sleep because CPU% is a delta, not an instantaneous value.
    pub async fn metrics(&self) -> Result<Metrics> {
        let script = r#"
echo '@@host';    hostname
echo '@@uptime';  uptime -p 2>/dev/null || uptime
echo '@@nproc';   nproc
echo '@@load';    cat /proc/loadavg
echo '@@stat1';   grep '^cpu ' /proc/stat
sleep 0.4
echo '@@stat2';   grep '^cpu ' /proc/stat
echo '@@mem';     cat /proc/meminfo
echo '@@disk';    df -B1 / | tail -1
"#;
        let out = self.run(script).await?;
        let s = Sections::parse(&out);

        let mut m = Metrics {
            hostname: s.get("host").trim().to_owned(),
            uptime: s.get("uptime").trim().to_owned(),
            cores: s.get("nproc").trim().parse().unwrap_or(0),
            ..Default::default()
        };

        let load: Vec<f32> = s
            .get("load")
            .split_whitespace()
            .take(3)
            .filter_map(|v| v.parse().ok())
            .collect();
        if load.len() == 3 {
            m.load = [load[0], load[1], load[2]];
        }

        m.cpu_percent = cpu_delta(s.get("stat1"), s.get("stat2")).unwrap_or(0.0);

        for line in s.get("mem").lines() {
            let Some((key, rest)) = line.split_once(':') else {
                continue;
            };
            // /proc/meminfo is in kB.
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            match key {
                "MemTotal" => m.mem_total = kb * 1024,
                "MemAvailable" => m.mem_available = kb * 1024,
                "SwapTotal" => m.swap_total = kb * 1024,
                "SwapFree" => m.swap_free = kb * 1024,
                _ => {}
            }
        }

        // `df -B1 /` tail: Filesystem 1B-blocks Used Available Use% Mounted
        let disk: Vec<&str> = s.get("disk").split_whitespace().collect();
        if disk.len() >= 3 {
            m.disk_total = disk[1].parse().unwrap_or(0);
            m.disk_used = disk[2].parse().unwrap_or(0);
        }

        Ok(m)
    }

    pub async fn services(&self, units: &[String]) -> Result<Vec<ServiceStatus>> {
        if units.is_empty() {
            return Ok(vec![]);
        }
        let args: Vec<String> = units.iter().map(|u| shq(u)).collect();
        // `is-active` exits non-zero when any unit is inactive — that's data, not failure.
        let out = self
            .run_lenient(&format!("systemctl is-active {}", args.join(" ")))
            .await?;
        let states: Vec<&str> = out.lines().map(str::trim).collect();

        Ok(units
            .iter()
            .enumerate()
            .map(|(i, name)| ServiceStatus {
                name: name.clone(),
                state: states.get(i).copied().unwrap_or("unknown").to_owned(),
            })
            .collect())
    }

    pub async fn pm2(&self) -> Result<Vec<Pm2App>> {
        // Absent pm2 is normal, not an error — report an empty list.
        let out = self
            .run_lenient("command -v pm2 >/dev/null 2>&1 && pm2 jlist 2>/dev/null || echo '[]'")
            .await?;
        let json = out.trim();
        let start = json.find('[').unwrap_or(0);
        let parsed: serde_json::Value =
            serde_json::from_str(&json[start..]).unwrap_or(serde_json::Value::Array(vec![]));

        let mut apps = Vec::new();
        for item in parsed.as_array().into_iter().flatten() {
            let monit = &item["monit"];
            apps.push(Pm2App {
                name: item["name"].as_str().unwrap_or("?").to_owned(),
                status: item["pm2_env"]["status"].as_str().unwrap_or("?").to_owned(),
                cpu: monit["cpu"].as_f64().unwrap_or(0.0) as f32,
                memory: monit["memory"].as_u64().unwrap_or(0),
                restarts: item["pm2_env"]["restart_time"].as_u64().unwrap_or(0),
            });
        }
        Ok(apps)
    }

    pub async fn palworld_ini(&self, pal: &PalworldConfig) -> Result<PalIni> {
        let sudo = if pal.sudo { "sudo -n " } else { "" };
        let text = self
            .run(&format!("{sudo}cat {}", shq(&pal.ini_path)))
            .await?;
        PalIni::parse(&text)
    }

    pub async fn players(&self, pal: &PalworldConfig) -> Result<Vec<Player>> {
        if pal.players_command.trim().is_empty() {
            return Ok(vec![]);
        }
        let out = self.run_lenient(&pal.players_command).await?;
        Ok(parse_players(&out))
    }

    pub async fn service_action(&self, unit: &str, action: &str, sudo: bool) -> Result<()> {
        let sudo = if sudo { "sudo -n " } else { "" };
        self.run(&format!("{sudo}systemctl {} {}", shq(action), shq(unit)))
            .await?;
        Ok(())
    }

    /// The process table, plus `claude` processes resolved to their working
    /// directory. CPU is sampled as a delta over a short sleep, like the host
    /// metrics: `ps` %CPU is a lifetime average, so a Claude session that has
    /// been idle for an hour would still show whatever it burned at startup.
    ///
    /// `comm` only — never `args`. An MCP server here is launched with an API
    /// key in its argv.
    pub async fn processes(&self) -> Result<(Vec<Proc>, Vec<ClaudeSession>)> {
        // /proc/<pid>/stat field 2 is the comm in parens and may itself contain
        // spaces, so everything is indexed from after the last ") ".
        let ticks = r#"awk '{i=index($0,") ");s=substr($0,i+2);split(s,a," ");split(FILENAME,f,"/");print f[3], a[12]+a[13]}' /proc/[0-9]*/stat 2>/dev/null"#;
        let script = format!(
            r#"
echo '@@hz';   getconf CLK_TCK
echo '@@e1';   date +%s.%N
echo '@@t1';   {ticks}
sleep 0.4
echo '@@e2';   date +%s.%N
echo '@@t2';   {ticks}
echo '@@ps';   ps -eo pid=,ppid=,user:24=,rss=,etimes=,comm=
echo '@@cwd';  for p in $(pgrep -x claude 2>/dev/null); do printf '%s %s\n' "$p" "$(readlink /proc/$p/cwd 2>/dev/null || echo -)"; done
"#
        );
        let out = self.run(&script).await?;
        let s = Sections::parse(&out);

        let hz: f64 = s.get("hz").trim().parse().unwrap_or(100.0);
        let t1 = tick_map(s.get("t1"));
        let t2 = tick_map(s.get("t2"));

        // The window is *not* the `sleep 0.4` — the awk that reads ~250 /proc
        // files takes real time too, and assuming 0.4s inflated every reading by
        // ~8% (a busy-loop that should read 100% came out at 108%). Each awk pass
        // reads a given pid at the same offset into its run, so the interval
        // between the two clock stamps is the true per-process sampling window.
        let stamp = |name: &str| s.get(name).trim().parse::<f64>().ok();
        let elapsed = stamp("e1")
            .zip(stamp("e2"))
            .map(|(a, b)| b - a)
            // A clock that jumped (NTP step) would otherwise divide by ~0 and
            // print a five-digit CPU%.
            .filter(|d| (0.05..=5.0).contains(d))
            .unwrap_or(0.4);
        let window = elapsed * hz;

        let mut procs = Vec::new();
        for line in s.get("ps").lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 6 {
                continue;
            }
            let Ok(pid) = f[0].parse::<i32>() else {
                continue;
            };
            let busy = t2
                .get(&pid)
                .zip(t1.get(&pid))
                .map(|(b, a)| b.saturating_sub(*a))
                .unwrap_or(0);
            procs.push(Proc {
                pid,
                ppid: f[1].parse().unwrap_or(0),
                user: f[2].to_owned(),
                cpu: (busy as f64 / window * 100.0) as f32,
                rss: f[3].parse::<u64>().unwrap_or(0) * 1024,
                uptime: f[4].parse().unwrap_or(0),
                comm: f[5..].join(" "),
            });
        }

        let claude = claude_sessions(&procs, s.get("cwd"));
        procs.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
        Ok((procs, claude))
    }

    /// TERM (or KILL) a process. Falls back to sudo for processes owned by
    /// another user — Palworld runs as `steam`, the tunnels as root.
    pub async fn kill(&self, pid: i32, force: bool) -> Result<()> {
        if pid <= 1 {
            bail!("pid {pid} は終了できません");
        }
        let sig = if force { "KILL" } else { "TERM" };
        self.run(&format!(
            "kill -{sig} {pid} 2>/dev/null || sudo -n kill -{sig} {pid}"
        ))
        .await?;
        Ok(())
    }

    /// Write Palworld settings — the *only* safe order.
    ///
    /// A running Palworld rewrites `PalWorldSettings.ini` from its in-memory
    /// config when it shuts down, so anything written while it's up is silently
    /// reverted on the next restart. That also means the on-disk file is stale
    /// until the server has stopped, so we re-read it *after* the stop and patch
    /// that, rather than the copy the UI was showing.
    pub async fn apply_palworld(
        &self,
        pal: &PalworldConfig,
        changes: &[(String, String)],
        progress: &(dyn Fn(&str) + Send + Sync),
    ) -> Result<PalIni> {
        let sudo = if pal.sudo { "sudo -n " } else { "" };
        let unit = shq(&pal.service);
        let path = shq(&pal.ini_path);

        progress("ワールドをセーブしています…");
        // Best-effort: no RCON configured, or the server is already down.
        let _ = self
            .run_lenient(&pal.players_command.replace("ShowPlayers", "Save"))
            .await;

        progress("Palworld を停止しています…");
        self.run(&format!("{sudo}systemctl stop {unit}")).await?;

        // Wait for it to actually exit — the ini write-back happens during shutdown.
        let mut stopped = false;
        for _ in 0..60 {
            let state = self
                .run_lenient(&format!("systemctl is-active {unit}"))
                .await?;
            if state.trim() != "active" {
                stopped = true;
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        if !stopped {
            bail!("Palworld が 60 秒以内に停止しませんでした。設定は変更していません。");
        }

        progress("設定ファイルを読み直しています…");
        let text = self.run(&format!("{sudo}cat {path}")).await?;
        let mut ini = PalIni::parse(&text)?;

        progress("バックアップを取得しています…");
        self.run(&format!("{sudo}cp {path} {path}.vwatch.bak"))
            .await?;

        progress("設定を書き込んでいます…");
        for (key, value) in changes {
            ini.set(key, value.clone());
        }
        let body = ini.render();
        let body = body.trim_end_matches('\n');
        // `tee` truncates the existing file in place, so ownership is preserved.
        self.run(&format!(
            "{sudo}tee {path} > /dev/null <<'VWATCH_EOF'\n{body}\nVWATCH_EOF"
        ))
        .await
        .context("設定ファイルの書き込みに失敗しました（バックアップ: .vwatch.bak）")?;

        progress("Palworld を起動しています…");
        self.run(&format!("{sudo}systemctl start {unit}")).await?;

        Ok(ini)
    }
}

/// Split marker-delimited output (`@@name` … ) into named sections.
struct Sections<'a> {
    map: std::collections::HashMap<&'a str, String>,
}

impl<'a> Sections<'a> {
    fn parse(text: &'a str) -> Self {
        let mut map = std::collections::HashMap::new();
        let mut current: Option<&str> = None;
        let mut buf = String::new();
        for line in text.lines() {
            if let Some(name) = line.trim().strip_prefix("@@") {
                if let Some(prev) = current.take() {
                    map.insert(prev, std::mem::take(&mut buf));
                }
                current = Some(name);
            } else if current.is_some() {
                buf.push_str(line);
                buf.push('\n');
            }
        }
        if let Some(prev) = current {
            map.insert(prev, buf);
        }
        Self { map }
    }

    fn get(&self, name: &str) -> &str {
        self.map.get(name).map(String::as_str).unwrap_or("")
    }
}

/// CPU busy% between two `/proc/stat` `cpu` lines.
fn cpu_delta(a: &str, b: &str) -> Option<f32> {
    let nums = |line: &str| -> Vec<u64> {
        line.split_whitespace()
            .skip(1)
            .filter_map(|v| v.parse().ok())
            .collect()
    };
    let (a, b) = (nums(a), nums(b));
    if a.len() < 5 || b.len() < 5 {
        return None;
    }
    let total_a: u64 = a.iter().sum();
    let total_b: u64 = b.iter().sum();
    // idle + iowait
    let idle_a = a[3] + a[4];
    let idle_b = b[3] + b[4];

    let d_total = total_b.checked_sub(total_a)?;
    let d_idle = idle_b.checked_sub(idle_a)?;
    if d_total == 0 {
        return None;
    }
    Some(((d_total - d_idle) as f64 / d_total as f64 * 100.0) as f32)
}

/// `pid busy_ticks` per line, from the awk over /proc/<pid>/stat.
fn tick_map(text: &str) -> std::collections::HashMap<i32, u64> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let pid = f.next()?.parse().ok()?;
            let ticks = f.next()?.parse().ok()?;
            Some((pid, ticks))
        })
        .collect()
}

/// Roll each `claude` process up with everything below it in the process tree.
fn claude_sessions(procs: &[Proc], cwds: &str) -> Vec<ClaudeSession> {
    let cwd_of: std::collections::HashMap<i32, &str> = cwds
        .lines()
        .filter_map(|line| {
            let (pid, cwd) = line.trim().split_once(' ')?;
            Some((pid.parse().ok()?, cwd))
        })
        .collect();

    let mut children: std::collections::HashMap<i32, Vec<&Proc>> = std::collections::HashMap::new();
    for p in procs {
        children.entry(p.ppid).or_default().push(p);
    }

    let mut sessions: Vec<ClaudeSession> = procs
        .iter()
        .filter(|p| p.comm == "claude")
        .map(|root| {
            let (mut cpu, mut rss, mut n) = (root.cpu, root.rss, 0usize);
            let mut stack = vec![root.pid];
            while let Some(pid) = stack.pop() {
                for kid in children.get(&pid).into_iter().flatten() {
                    cpu += kid.cpu;
                    rss += kid.rss;
                    n += 1;
                    stack.push(kid.pid);
                }
            }
            ClaudeSession {
                pid: root.pid,
                cwd: cwd_of.get(&root.pid).unwrap_or(&"-").to_string(),
                uptime: root.uptime,
                cpu,
                rss,
                descendants: n,
            }
        })
        .collect();

    sessions.sort_by_key(|s| s.pid);
    sessions
}

/// RCON `ShowPlayers` prints `name,playeruid,steamid` then one row per player.
fn parse_players(out: &str) -> Vec<Player> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .skip_while(|l| l.starts_with("name,"))
        .filter_map(|line| {
            let mut f = line.split(',');
            Some(Player {
                name: f.next()?.to_owned(),
                playeruid: f.next().unwrap_or_default().to_owned(),
                steamid: f.next().unwrap_or_default().to_owned(),
            })
        })
        .filter(|p| !p.name.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_delta_from_two_proc_stat_lines() {
        // 100 ticks elapsed, 25 of them idle -> 75% busy.
        let a = "cpu  100 0 100 700 100 0 0 0 0 0";
        let b = "cpu  150 0 125 725 100 0 0 0 0 0";
        let pct = cpu_delta(a, b).unwrap();
        assert!((pct - 75.0).abs() < 0.01, "got {pct}");
    }

    #[test]
    fn cpu_delta_survives_a_counter_that_did_not_move() {
        let line = "cpu  1 2 3 4 5 0 0 0 0 0";
        assert_eq!(cpu_delta(line, line), None);
    }

    #[test]
    fn players_skips_the_csv_header() {
        // A multibyte name must survive: Palworld players routinely use them.
        let out = "name,playeruid,steamid\nplayer1,AAAA1111,76561190000000001\nプレイヤー2,BBBB2222,76561190000000002\n";
        let players = parse_players(out);
        assert_eq!(players.len(), 2);
        assert_eq!(players[0].name, "player1");
        assert_eq!(players[1].name, "プレイヤー2");
        assert_eq!(players[1].steamid, "76561190000000002");
    }

    #[test]
    fn empty_player_list_is_not_a_phantom_player() {
        assert!(parse_players("name,playeruid,steamid\n").is_empty());
        assert!(parse_players("").is_empty());
    }

    #[test]
    fn sections_split_on_markers() {
        let s = Sections::parse("@@a\n1\n2\n@@b\n3\n");
        assert_eq!(s.get("a"), "1\n2\n");
        assert_eq!(s.get("b"), "3\n");
        assert_eq!(s.get("missing"), "");
    }

    #[test]
    fn shell_quoting_neutralises_a_quote_in_the_path() {
        assert_eq!(shq("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(shq("it's"), r"'it'\''s'");
    }
}
