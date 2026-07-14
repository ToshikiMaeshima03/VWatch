// Release builds are a GUI app: no console window should flash up behind it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod fonts;
mod model;
mod palworld;
mod ssh;
mod worker;

use anyhow::Result;

fn main() -> Result<()> {
    if std::env::args().any(|a| a == "--probe") {
        return probe();
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 760.0])
            .with_min_inner_size([820.0, 560.0])
            .with_title("VWatch — VPS モニター"),
        ..Default::default()
    };

    eframe::run_native(
        "VWatch",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Headless end-to-end check: connect over SSH with the saved config and print
/// what the GUI would render. Lets the backend be exercised on a box with no
/// display — and doubles as the first thing to run when something looks wrong.
fn probe() -> Result<()> {
    use model::human_bytes;

    let cfg = config::Config::load()?;
    if !cfg.is_connectable() {
        anyhow::bail!(
            "no host/user configured. Edit {}",
            config::Config::path()?.display()
        );
    }

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        eprintln!("connecting to {}@{}…", cfg.ssh.user, cfg.ssh.host);
        let vps = ssh::Vps::connect(&cfg).await?;

        let m = vps.metrics().await?;
        println!("host      {} ({})", m.hostname, m.uptime);
        println!(
            "cpu       {:.1}%  ({} cores, load {:.2})",
            m.cpu_percent, m.cores, m.load[0]
        );
        println!(
            "memory    {} / {}  ({:.0}%)",
            human_bytes(m.mem_used()),
            human_bytes(m.mem_total),
            m.mem_percent()
        );
        println!(
            "disk      {} / {}  ({:.0}%)",
            human_bytes(m.disk_used),
            human_bytes(m.disk_total),
            m.disk_percent()
        );

        println!("\nservices");
        for s in vps.services(&cfg.services).await? {
            println!("  {:<14} {}", s.name, s.state);
        }

        let pm2 = vps.pm2().await?;
        if !pm2.is_empty() {
            println!("\npm2");
            for a in pm2 {
                println!("  {:<26} {:<8} {}", a.name, a.status, human_bytes(a.memory));
            }
        }

        if cfg.palworld.enabled {
            println!("\npalworld");
            match vps.palworld_ini(&cfg.palworld).await {
                Ok(ini) => {
                    println!("  {} settings parsed", ini.options().len());
                    for key in [
                        "ExpRate",
                        "CollectionDropRate",
                        "EnemyDropItemRate",
                        "ServerName",
                    ] {
                        println!("    {:<20} {}", key, ini.get(key).unwrap_or("(absent)"));
                    }
                }
                Err(e) => println!("  ini unreadable: {e:#}"),
            }
            match vps.players(&cfg.palworld).await {
                Ok(players) if players.is_empty() => println!("  no players online"),
                Ok(players) => {
                    for p in players {
                        println!("    {} ({})", p.name, p.steamid);
                    }
                }
                Err(e) => println!("  players unavailable: {e:#}"),
            }
        }

        anyhow::Ok(())
    })
}
