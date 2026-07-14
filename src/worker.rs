//! The SSH session lives on a background tokio runtime; the UI thread never
//! blocks on the network. They share state through a mutex and a command channel.

use crate::config::Config;
use crate::model::{Sample, Snapshot};
use crate::palworld::PalIni;
use crate::ssh::Vps;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

const HISTORY_LEN: usize = 240;
const LOG_LEN: usize = 200;

#[derive(Debug)]
pub enum Cmd {
    Connect(Box<Config>),
    Disconnect,
    Refresh,
    Service { unit: String, action: String },
    ApplyPalworld(Vec<(String, String)>),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Conn {
    #[default]
    Disconnected,
    Connecting,
    Connected,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Level {
    Info,
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub at: String,
    pub level: Level,
    pub text: String,
}

#[derive(Default)]
pub struct Shared {
    pub conn: Conn,
    pub snap: Snapshot,
    pub history: VecDeque<Sample>,
    /// Set while a mutating operation is in flight; the UI disables its buttons.
    pub busy: Option<String>,
    pub log: VecDeque<LogEntry>,
    /// Bumped whenever a fresh Palworld ini lands, so the UI knows to reseed the
    /// editable copy instead of clobbering what the user is currently dragging.
    pub pal_revision: u64,
}

impl Shared {
    fn push_log(&mut self, level: Level, text: impl Into<String>) {
        self.log.push_back(LogEntry {
            at: chrono::Local::now().format("%H:%M:%S").to_string(),
            level,
            text: text.into(),
        });
        while self.log.len() > LOG_LEN {
            self.pop_oldest_log();
        }
    }

    fn pop_oldest_log(&mut self) {
        self.log.pop_front();
    }
}

#[derive(Clone)]
pub struct Handle {
    tx: UnboundedSender<Cmd>,
    pub shared: Arc<Mutex<Shared>>,
}

impl Handle {
    pub fn send(&self, cmd: Cmd) {
        // The worker outliving the UI is impossible; a closed channel means we're
        // shutting down, so dropping the command is correct.
        let _ = self.tx.send(cmd);
    }
}

/// Spawn the worker thread. `ctx` is used to wake the UI when state changes.
pub fn spawn(ctx: egui::Context) -> Handle {
    let (tx, rx) = unbounded_channel();
    let shared = Arc::new(Mutex::new(Shared::default()));
    let handle = Handle {
        tx,
        shared: shared.clone(),
    };

    std::thread::Builder::new()
        .name("vwatch-ssh".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(run(rx, shared, ctx));
        })
        .expect("spawn ssh thread");

    handle
}

struct Worker {
    shared: Arc<Mutex<Shared>>,
    ctx: egui::Context,
    vps: Option<Vps>,
    cfg: Option<Config>,
    started: Instant,
}

impl Worker {
    fn set<R>(&self, f: impl FnOnce(&mut Shared) -> R) -> R {
        let r = {
            let mut s = self.shared.lock().unwrap();
            f(&mut s)
        };
        self.ctx.request_repaint();
        r
    }

    fn log(&self, level: Level, text: impl Into<String>) {
        self.set(|s| s.push_log(level, text));
    }

    async fn connect(&mut self, cfg: Config) {
        self.set(|s| s.conn = Conn::Connecting);
        self.log(
            Level::Info,
            format!("{}@{} に接続しています…", cfg.ssh.user, cfg.ssh.host),
        );

        match Vps::connect(&cfg).await {
            Ok(vps) => {
                self.vps = Some(vps);
                self.cfg = Some(cfg);
                self.set(|s| s.conn = Conn::Connected);
                self.log(Level::Ok, "接続しました");
                self.poll().await;
            }
            Err(e) => {
                let msg = format!("{e:#}");
                self.vps = None;
                self.set(|s| s.conn = Conn::Failed(msg.clone()));
                self.log(Level::Error, format!("接続に失敗しました: {msg}"));
            }
        }
    }

    /// One refresh cycle. A failure here drops the session so the next tick
    /// reconnects rather than hammering a dead socket.
    async fn poll(&mut self) {
        let (Some(vps), Some(cfg)) = (&self.vps, &self.cfg) else {
            return;
        };

        let mut snap = Snapshot::default();
        let mut failure = None;

        match vps.metrics().await {
            Ok(m) => snap.metrics = m,
            Err(e) => failure = Some(format!("{e:#}")),
        }

        if failure.is_none() {
            snap.services = vps.services(&cfg.services).await.unwrap_or_default();
            if cfg.show_pm2 {
                snap.pm2 = vps.pm2().await.unwrap_or_default();
            }
            if cfg.palworld.enabled {
                // A stopped Palworld makes these fail; that's expected, not an error.
                snap.palworld_ini = vps.palworld_ini(&cfg.palworld).await.ok();
                let pal_up = snap
                    .services
                    .iter()
                    .any(|s| s.name == cfg.palworld.service && s.is_active());
                snap.players = if pal_up {
                    vps.players(&cfg.palworld).await.ok()
                } else {
                    None
                };
            }
        }

        if let Some(msg) = failure {
            self.vps = None;
            self.set(|s| s.conn = Conn::Failed(msg.clone()));
            self.log(Level::Error, format!("取得に失敗しました: {msg}"));
            return;
        }

        let t = self.started.elapsed().as_secs_f64();
        let sample = Sample {
            t,
            cpu: snap.metrics.cpu_percent,
            mem: snap.metrics.mem_percent(),
        };

        self.set(|s| {
            let new_ini = snap.palworld_ini.clone();
            let changed = new_ini != s.snap.palworld_ini;
            s.snap = snap;
            s.history.push_back(sample);
            while s.history.len() > HISTORY_LEN {
                s.history.pop_front();
            }
            if changed {
                s.pal_revision += 1;
            }
        });
    }

    async fn service_action(&mut self, unit: String, action: String) {
        let (Some(vps), Some(cfg)) = (&self.vps, &self.cfg) else {
            return;
        };
        let sudo = cfg.palworld.sudo;

        self.set(|s| s.busy = Some(format!("{unit} を {action} しています…")));
        let result = vps.service_action(&unit, &action, sudo).await;
        self.set(|s| s.busy = None);

        match result {
            Ok(()) => self.log(Level::Ok, format!("{unit}: {action} を実行しました")),
            Err(e) => self.log(Level::Error, format!("{unit}: {action} に失敗 — {e:#}")),
        }
        self.poll().await;
    }

    async fn apply_palworld(&mut self, changes: Vec<(String, String)>) {
        let (Some(vps), Some(cfg)) = (&self.vps, &self.cfg) else {
            return;
        };

        let summary: Vec<String> = changes.iter().map(|(k, v)| format!("{k}={v}")).collect();
        self.log(
            Level::Warn,
            format!("Palworld 設定を適用します: {}", summary.join(", ")),
        );

        let shared = self.shared.clone();
        let ctx = self.ctx.clone();
        let progress = move |msg: &str| {
            shared.lock().unwrap().busy = Some(msg.to_owned());
            ctx.request_repaint();
        };

        let result = vps.apply_palworld(&cfg.palworld, &changes, &progress).await;
        self.set(|s| s.busy = None);

        match result {
            Ok(ini) => {
                self.log(Level::Ok, "適用してサーバーを再起動しました");
                self.set(|s| {
                    s.snap.palworld_ini = Some(ini);
                    s.pal_revision += 1;
                });
            }
            Err(e) => self.log(Level::Error, format!("適用に失敗しました: {e:#}")),
        }
        self.poll().await;
    }
}

async fn run(mut rx: UnboundedReceiver<Cmd>, shared: Arc<Mutex<Shared>>, ctx: egui::Context) {
    let mut w = Worker {
        shared,
        ctx,
        vps: None,
        cfg: None,
        started: Instant::now(),
    };
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    Cmd::Connect(cfg) => {
                        tick = tokio::time::interval(
                            Duration::from_secs(cfg.poll_interval_secs.clamp(1, 300)),
                        );
                        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                        w.connect(*cfg).await;
                    }
                    Cmd::Disconnect => {
                        w.vps = None;
                        w.set(|s| { s.conn = Conn::Disconnected; s.snap = Snapshot::default(); });
                        w.log(Level::Info, "切断しました");
                    }
                    Cmd::Refresh => w.poll().await,
                    Cmd::Service { unit, action } => w.service_action(unit, action).await,
                    Cmd::ApplyPalworld(changes) => w.apply_palworld(changes).await,
                }
            }
            _ = tick.tick() => {
                // Don't poll on top of a stop→edit→start that's mid-flight.
                let idle = w.shared.lock().unwrap().busy.is_none();
                if idle && w.vps.is_some() {
                    w.poll().await;
                }
            }
        }
    }
}

/// Keys the UI may write, diffed against what the server currently has.
pub fn diff(original: &PalIni, edited: &PalIni) -> Vec<(String, String)> {
    let mut changes = Vec::new();
    for (key, value) in edited.options() {
        if original.get(key) != Some(value.as_str()) {
            changes.push((key.clone(), value.clone()));
        }
    }
    changes
}
