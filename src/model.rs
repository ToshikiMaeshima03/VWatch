//! Plain data the SSH layer produces and the UI renders.

#[derive(Debug, Clone, Default)]
pub struct Metrics {
    pub hostname: String,
    pub uptime: String,
    pub cores: usize,
    pub cpu_percent: f32,
    pub load: [f32; 3],
    pub mem_total: u64,
    pub mem_available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
    pub disk_total: u64,
    pub disk_used: u64,
}

impl Metrics {
    pub fn mem_used(&self) -> u64 {
        self.mem_total.saturating_sub(self.mem_available)
    }
    pub fn mem_percent(&self) -> f32 {
        pct(self.mem_used(), self.mem_total)
    }
    pub fn swap_used(&self) -> u64 {
        self.swap_total.saturating_sub(self.swap_free)
    }
    pub fn swap_percent(&self) -> f32 {
        pct(self.swap_used(), self.swap_total)
    }
    pub fn disk_percent(&self) -> f32 {
        pct(self.disk_used, self.disk_total)
    }
}

fn pct(used: u64, total: u64) -> f32 {
    if total == 0 {
        0.0
    } else {
        (used as f64 / total as f64 * 100.0) as f32
    }
}

#[derive(Debug, Clone)]
pub struct ServiceStatus {
    pub name: String,
    /// `systemctl is-active` verbatim: active / inactive / failed / activating…
    pub state: String,
}

impl ServiceStatus {
    pub fn is_active(&self) -> bool {
        self.state == "active"
    }
}

#[derive(Debug, Clone, Default)]
pub struct Pm2App {
    pub name: String,
    pub status: String,
    pub cpu: f32,
    pub memory: u64,
    pub restarts: u64,
}

impl Pm2App {
    pub fn is_online(&self) -> bool {
        self.status == "online"
    }
}

#[derive(Debug, Clone)]
pub struct Player {
    pub name: String,
    pub playeruid: String,
    pub steamid: String,
}

/// One process. The command *name* only — never the full argv. This host runs an
/// MCP server whose argv carries a Figma API key, and anything the UI draws can
/// end up in a screenshot in the (public) repo.
#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub user: String,
    /// Instantaneous, from a /proc delta — not `ps` %CPU, which is an average
    /// over the whole lifetime and so barely moves for a long-lived process.
    pub cpu: f32,
    pub rss: u64,
    pub uptime: u64,
    pub comm: String,
}

impl Proc {
    /// Kernel threads (`kworker/*`, `kthreadd`…) are children of pid 2 and hold
    /// no memory. They idle at 0% and would just pad out the table.
    pub fn is_kernel_thread(&self) -> bool {
        self.pid == 2 || self.ppid == 2
    }
}

/// A `claude` process with its whole tree (MCP servers, browsers…) rolled into
/// it. Flat, that tree is ~40 processes per session and drowns out everything else.
#[derive(Debug, Clone)]
pub struct ClaudeSession {
    pub pid: i32,
    /// Working directory — the only thing that tells two sessions apart.
    pub cwd: String,
    pub uptime: u64,
    /// Session + descendants.
    pub cpu: f32,
    pub rss: u64,
    pub descendants: usize,
}

/// A point in the rolling CPU/memory history behind the graphs.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    /// Seconds since the app started.
    pub t: f64,
    pub cpu: f32,
    pub mem: f32,
}

/// Everything one poll cycle produces. Palworld pieces are `None` when the
/// Palworld integration is disabled or the host isn't running it.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub metrics: Metrics,
    pub services: Vec<ServiceStatus>,
    pub pm2: Vec<Pm2App>,
    pub players: Option<Vec<Player>>,
    pub palworld_ini: Option<crate::palworld::PalIni>,
    pub procs: Vec<Proc>,
    pub claude: Vec<ClaudeSession>,
}

impl Snapshot {
    pub fn proc_by_comm(&self, comm: &str) -> Option<&Proc> {
        self.procs.iter().find(|p| p.comm == comm)
    }
}

pub fn human_secs(secs: u64) -> String {
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    match (h, m) {
        (0, 0) => format!("{secs}秒"),
        (0, m) => format!("{m}分"),
        (h, m) => format!("{h}時間{m}分"),
    }
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}
