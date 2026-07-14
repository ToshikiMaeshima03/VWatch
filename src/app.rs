//! The egui front-end. Reads the shared snapshot, sends commands; never blocks.

use crate::config::{Auth, Config};
use crate::model::{Metrics, human_bytes};
use crate::palworld::{self, Kind, PalIni};
use crate::worker::{self, Cmd, Conn, Handle, Level};
use egui::{Color32, RichText, Ui};
use egui_plot::{Legend, Line, Plot, PlotPoints};

const GREEN: Color32 = Color32::from_rgb(0x3f, 0xb9, 0x50);
const RED: Color32 = Color32::from_rgb(0xe5, 0x53, 0x4b);
const AMBER: Color32 = Color32::from_rgb(0xd9, 0x9e, 0x22);
const DIM: Color32 = Color32::from_rgb(0x8b, 0x94, 0x9e);

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Overview,
    Services,
    Palworld,
    Settings,
    Log,
}

pub struct App {
    handle: Handle,
    cfg: Config,
    tab: Tab,
    font: Option<String>,
    config_notice: Option<String>,

    /// Editable copy of the server's ini, reseeded whenever the server's copy
    /// changes underneath us.
    pal_edit: Option<PalIni>,
    pal_seen_revision: u64,
    confirm_apply: Option<Vec<(String, String)>>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let font = crate::fonts::install(&cc.egui_ctx);
        cc.egui_ctx.set_visuals(egui::Visuals::dark());

        let (cfg, config_notice) = match Config::load() {
            Ok(cfg) => (cfg, None),
            Err(e) => (
                Config::default(),
                Some(format!("設定の読み込みに失敗: {e:#}")),
            ),
        };

        let handle = worker::spawn(cc.egui_ctx.clone());
        if cfg.is_connectable() {
            handle.send(Cmd::Connect(Box::new(cfg.clone())));
        }

        Self {
            handle,
            tab: if cfg.is_connectable() {
                Tab::Overview
            } else {
                Tab::Settings
            },
            cfg,
            font,
            config_notice,
            pal_edit: None,
            pal_seen_revision: 0,
            confirm_apply: None,
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Snapshot the shared state once per frame; hold the lock as briefly as possible.
        let (conn, snap, history, busy, log, pal_revision) = {
            let s = self.handle.shared.lock().unwrap();
            (
                s.conn.clone(),
                s.snap.clone(),
                s.history.iter().copied().collect::<Vec<_>>(),
                s.busy.clone(),
                s.log.iter().cloned().collect::<Vec<_>>(),
                s.pal_revision,
            )
        };

        // Reseed the editable ini only when the server's copy actually changed,
        // so a poll landing mid-drag doesn't yank the slider out of the user's hand.
        if pal_revision != self.pal_seen_revision {
            self.pal_edit = snap.palworld_ini.clone();
            self.pal_seen_revision = pal_revision;
        }

        self.top_bar(ctx, &conn, &busy);

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Overview => self.overview(ui, &snap.metrics, &history, &conn),
            Tab::Services => self.services(ui, &snap, busy.is_some()),
            Tab::Palworld => self.palworld(ui, &snap, busy.is_some()),
            Tab::Settings => self.settings(ui),
            Tab::Log => self.log(ui, &log),
        });

        self.confirm_dialog(ctx, &snap);
    }
}

impl App {
    fn top_bar(&mut self, ctx: &egui::Context, conn: &Conn, busy: &Option<String>) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("VWatch");
                ui.separator();
                for (tab, label) in [
                    (Tab::Overview, "概要"),
                    (Tab::Services, "サービス"),
                    (Tab::Palworld, "Palworld"),
                    (Tab::Settings, "設定"),
                    (Tab::Log, "ログ"),
                ] {
                    ui.selectable_value(&mut self.tab, tab, label);
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (dot, text, color) = match conn {
                        Conn::Connected => ("●", "接続中".to_owned(), GREEN),
                        Conn::Connecting => ("◐", "接続しています…".to_owned(), AMBER),
                        Conn::Disconnected => ("○", "未接続".to_owned(), DIM),
                        Conn::Failed(e) => ("●", format!("失敗: {}", first_line(e)), RED),
                    };
                    ui.colored_label(color, format!("{dot} {text}"));

                    if let Some(msg) = busy {
                        ui.spinner();
                        ui.colored_label(AMBER, msg);
                    } else if matches!(conn, Conn::Connected) {
                        if ui.button("更新").clicked() {
                            self.handle.send(Cmd::Refresh);
                        }
                    } else if self.cfg.is_connectable() && ui.button("接続").clicked() {
                        self.handle.send(Cmd::Connect(Box::new(self.cfg.clone())));
                    }
                });
            });
        });
    }

    fn overview(
        &mut self,
        ui: &mut Ui,
        m: &Metrics,
        history: &[crate::model::Sample],
        conn: &Conn,
    ) {
        if !matches!(conn, Conn::Connected) {
            ui.vertical_centered(|ui| {
                ui.add_space(60.0);
                ui.label(
                    RichText::new("VPS に接続していません")
                        .size(18.0)
                        .color(DIM),
                );
                if let Conn::Failed(e) = conn {
                    ui.add_space(8.0);
                    ui.colored_label(RED, e);
                }
                ui.add_space(12.0);
                if ui.button("設定を開く").clicked() {
                    self.tab = Tab::Settings;
                }
            });
            return;
        }

        ui.horizontal(|ui| {
            ui.label(RichText::new(&m.hostname).size(20.0).strong());
            ui.label(RichText::new(&m.uptime).color(DIM));
            ui.label(
                RichText::new(format!(
                    "load {:.2} / {:.2} / {:.2}  ({} コア)",
                    m.load[0], m.load[1], m.load[2], m.cores
                ))
                .color(DIM),
            );
        });
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            gauge(ui, "CPU", m.cpu_percent, &format!("{:.0}%", m.cpu_percent));
            gauge(
                ui,
                "メモリ",
                m.mem_percent(),
                &format!(
                    "{} / {}",
                    human_bytes(m.mem_used()),
                    human_bytes(m.mem_total)
                ),
            );
            gauge(
                ui,
                "ディスク",
                m.disk_percent(),
                &format!(
                    "{} / {}",
                    human_bytes(m.disk_used),
                    human_bytes(m.disk_total)
                ),
            );
            if m.swap_total > 0 {
                gauge(
                    ui,
                    "スワップ",
                    m.swap_percent(),
                    &format!(
                        "{} / {}",
                        human_bytes(m.swap_used()),
                        human_bytes(m.swap_total)
                    ),
                );
            }
        });

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        let cpu: PlotPoints = history.iter().map(|s| [s.t, s.cpu as f64]).collect();
        let mem: PlotPoints = history.iter().map(|s| [s.t, s.mem as f64]).collect();

        Plot::new("history")
            .height(260.0)
            .include_y(0.0)
            .include_y(100.0)
            .legend(Legend::default())
            .x_axis_formatter(|mark, _| format!("{:.0}s", mark.value))
            .y_axis_formatter(|mark, _| format!("{:.0}%", mark.value))
            .show(ui, |plot| {
                // `width` takes `impl Into<f32>`; an unsuffixed literal infers as
                // f64 and leans on a fallback that newer rustc rejects.
                plot.line(Line::new("CPU", cpu).color(GREEN).width(1.5_f32));
                plot.line(Line::new("メモリ", mem).color(AMBER).width(1.5_f32));
            });
    }

    fn services(&mut self, ui: &mut Ui, snap: &crate::model::Snapshot, busy: bool) {
        ui.heading("systemd");
        ui.add_space(4.0);

        egui::Grid::new("services")
            .num_columns(3)
            .spacing([16.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                for svc in &snap.services {
                    let (color, mark) = if svc.is_active() {
                        (GREEN, "●")
                    } else {
                        (RED, "○")
                    };
                    ui.colored_label(color, mark);
                    ui.label(&svc.name);
                    ui.horizontal(|ui| {
                        ui.add_enabled_ui(!busy, |ui| {
                            ui.label(RichText::new(&svc.state).color(DIM));
                            for action in ["start", "stop", "restart"] {
                                if ui.small_button(action).clicked() {
                                    self.handle.send(Cmd::Service {
                                        unit: svc.name.clone(),
                                        action: action.to_owned(),
                                    });
                                }
                            }
                        });
                    });
                    ui.end_row();
                }
            });

        if !snap.pm2.is_empty() {
            ui.add_space(16.0);
            ui.heading("PM2");
            ui.add_space(4.0);
            egui::Grid::new("pm2")
                .num_columns(5)
                .spacing([16.0, 6.0])
                .striped(true)
                .show(ui, |ui| {
                    ui.label(RichText::new("").color(DIM));
                    ui.label(RichText::new("アプリ").color(DIM));
                    ui.label(RichText::new("CPU").color(DIM));
                    ui.label(RichText::new("メモリ").color(DIM));
                    ui.label(RichText::new("再起動回数").color(DIM));
                    ui.end_row();

                    for app in &snap.pm2 {
                        let (color, mark) = if app.is_online() {
                            (GREEN, "●")
                        } else {
                            (RED, "○")
                        };
                        ui.colored_label(color, mark);
                        ui.label(&app.name);
                        ui.label(format!("{:.0}%", app.cpu));
                        ui.label(human_bytes(app.memory));
                        ui.label(app.restarts.to_string());
                        ui.end_row();
                    }
                });
        }
    }

    fn palworld(&mut self, ui: &mut Ui, snap: &crate::model::Snapshot, busy: bool) {
        if !self.cfg.palworld.enabled {
            ui.label("Palworld 連携は設定で無効になっています。");
            return;
        }

        let running = snap
            .services
            .iter()
            .any(|s| s.name == self.cfg.palworld.service && s.is_active());

        ui.horizontal(|ui| {
            ui.heading("Palworld");
            if running {
                ui.colored_label(GREEN, "● 稼働中");
            } else {
                ui.colored_label(RED, "○ 停止中");
            }
        });

        // Players ---------------------------------------------------------
        ui.add_space(6.0);
        match &snap.players {
            Some(players) if players.is_empty() => {
                ui.colored_label(
                    DIM,
                    "オンラインのプレイヤーはいません（今なら誰も切断せずに再起動できます）",
                );
            }
            Some(players) => {
                ui.colored_label(AMBER, format!("{} 人がプレイ中", players.len()));
                egui::Grid::new("players")
                    .num_columns(3)
                    .spacing([16.0, 4.0])
                    .striped(true)
                    .show(ui, |ui| {
                        for p in players {
                            ui.label(&p.name);
                            ui.label(RichText::new(&p.steamid).monospace().size(11.0).color(DIM));
                            ui.label(
                                RichText::new(&p.playeruid)
                                    .monospace()
                                    .size(11.0)
                                    .color(DIM),
                            );
                            ui.end_row();
                        }
                    });
            }
            None => {
                ui.colored_label(
                    DIM,
                    "プレイヤー情報なし（サーバー停止中、または RCON コマンド未設定）",
                );
            }
        }

        ui.add_space(10.0);
        ui.separator();

        let Some(original) = snap.palworld_ini.as_ref() else {
            ui.add_space(20.0);
            ui.colored_label(RED, "PalWorldSettings.ini を読み込めませんでした。");
            ui.label(
                RichText::new("「設定」タブでパスと sudo の設定を確認してください。").color(DIM),
            );
            return;
        };
        let Some(edit) = self.pal_edit.as_mut() else {
            return;
        };

        let changes = worker::diff(original, edit);

        egui::ScrollArea::vertical().show(ui, |ui| {
            for group in palworld::groups() {
                ui.add_space(10.0);
                ui.label(RichText::new(group.title).strong().size(15.0));
                ui.add_space(4.0);

                egui::Grid::new(group.title)
                    .num_columns(2)
                    .spacing([20.0, 8.0])
                    .show(ui, |ui| {
                        for spec in group.specs {
                            // A key Palworld didn't write out isn't editable here.
                            if edit.get(spec.key).is_none() {
                                continue;
                            }
                            let dirty = changes.iter().any(|(k, _)| k == spec.key);

                            ui.horizontal(|ui| {
                                let mut label = RichText::new(spec.label);
                                if dirty {
                                    label = label.color(AMBER).strong();
                                }
                                ui.label(label);
                                if dirty {
                                    ui.colored_label(AMBER, "●");
                                }
                            });

                            ui.vertical(|ui| {
                                widget(ui, edit, spec, busy);
                                if let Some(note) = spec.note {
                                    ui.label(RichText::new(note).size(11.0).color(DIM));
                                }
                            });
                            ui.end_row();
                        }
                    });
            }
            ui.add_space(16.0);
        });

        // Apply bar -------------------------------------------------------
        ui.separator();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!busy && !changes.is_empty(), |ui| {
                if ui
                    .button(RichText::new("適用してサーバーを再起動").strong())
                    .clicked()
                {
                    self.confirm_apply = Some(changes.clone());
                }
            });
            if !changes.is_empty() {
                if ui.button("変更を破棄").clicked() {
                    self.pal_edit = snap.palworld_ini.clone();
                }
                ui.colored_label(AMBER, format!("{} 件の変更", changes.len()));
            } else {
                ui.colored_label(DIM, "変更はありません");
            }
        });
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context, snap: &crate::model::Snapshot) {
        let Some(changes) = self.confirm_apply.clone() else {
            return;
        };
        let online = snap.players.as_ref().map(Vec::len).unwrap_or(0);

        let mut open = true;
        egui::Window::new("設定を適用しますか？")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label("適用にはサーバーの停止と起動が必要です:");
                ui.add_space(6.0);
                for (key, value) in &changes {
                    ui.label(RichText::new(format!("  {key} = {value}")).monospace());
                }
                ui.add_space(10.0);

                if online > 0 {
                    ui.colored_label(
                        RED,
                        format!("⚠ {online} 人がプレイ中です。再起動すると全員が切断されます。",),
                    );
                } else {
                    ui.colored_label(
                        GREEN,
                        "オンラインのプレイヤーはいません。誰も切断されません。",
                    );
                }
                ui.label(
                    RichText::new(
                        "ワールドはセーブされ、ini は .vwatch.bak にバックアップされます。",
                    )
                    .size(11.0)
                    .color(DIM),
                );

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(RichText::new("適用する").color(RED).strong())
                        .clicked()
                    {
                        self.handle.send(Cmd::ApplyPalworld(changes.clone()));
                        open = false;
                    }
                    if ui.button("キャンセル").clicked() {
                        open = false;
                    }
                });
            });

        if !open {
            self.confirm_apply = None;
        }
    }

    fn settings(&mut self, ui: &mut Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("接続設定");
            ui.label(
                RichText::new(
                    "この内容はローカルにのみ保存され、リポジトリには入りません。",
                )
                .size(11.0)
                .color(DIM),
            );
            ui.add_space(8.0);

            egui::Grid::new("ssh").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                ui.label("ホスト");
                ui.text_edit_singleline(&mut self.cfg.ssh.host);
                ui.end_row();

                ui.label("ポート");
                ui.add(egui::DragValue::new(&mut self.cfg.ssh.port).range(1..=65535));
                ui.end_row();

                ui.label("ユーザー");
                ui.text_edit_singleline(&mut self.cfg.ssh.user);
                ui.end_row();

                ui.label("認証方式");
                ui.horizontal(|ui| {
                    let is_key = matches!(self.cfg.ssh.auth, Auth::Key { .. });
                    if ui.selectable_label(is_key, "SSH鍵").clicked() && !is_key {
                        self.cfg.ssh.auth = Auth::default();
                    }
                    if ui.selectable_label(!is_key, "パスワード").clicked() && is_key {
                        self.cfg.ssh.auth = Auth::Password { password: String::new() };
                    }
                });
                ui.end_row();

                match &mut self.cfg.ssh.auth {
                    Auth::Key { path, passphrase } => {
                        ui.label("鍵ファイル");
                        ui.text_edit_singleline(path);
                        ui.end_row();
                        ui.label("パスフレーズ");
                        ui.add(egui::TextEdit::singleline(passphrase).password(true));
                        ui.end_row();
                    }
                    Auth::Password { password } => {
                        ui.label("パスワード");
                        ui.add(egui::TextEdit::singleline(password).password(true));
                        ui.end_row();
                    }
                }

                ui.label("ホスト鍵の検証");
                ui.checkbox(
                    &mut self.cfg.ssh.strict_host_key,
                    "known_hosts で検証する（初回接続では失敗します）",
                );
                ui.end_row();

                ui.label("更新間隔（秒）");
                ui.add(egui::DragValue::new(&mut self.cfg.poll_interval_secs).range(1..=300));
                ui.end_row();
            });

            ui.add_space(16.0);
            ui.heading("監視するサービス");
            ui.add_space(4.0);
            let mut remove = None;
            for (i, svc) in self.cfg.services.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(svc);
                    if ui.small_button("削除").clicked() {
                        remove = Some(i);
                    }
                });
            }
            if let Some(i) = remove {
                self.cfg.services.remove(i);
            }
            if ui.button("＋ 追加").clicked() {
                self.cfg.services.push(String::new());
            }
            ui.checkbox(&mut self.cfg.show_pm2, "PM2 のアプリも表示する");

            ui.add_space(16.0);
            ui.heading("Palworld");
            ui.add_space(4.0);
            ui.checkbox(&mut self.cfg.palworld.enabled, "Palworld 連携を有効にする");
            egui::Grid::new("pal").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                ui.label("ini のパス");
                ui.text_edit_singleline(&mut self.cfg.palworld.ini_path);
                ui.end_row();

                ui.label("systemd ユニット名");
                ui.text_edit_singleline(&mut self.cfg.palworld.service);
                ui.end_row();

                ui.label("プレイヤー取得コマンド");
                ui.vertical(|ui| {
                    ui.text_edit_singleline(&mut self.cfg.palworld.players_command);
                    ui.label(
                        RichText::new(
                            "RCON ShowPlayers の CSV を標準出力に出すリモートコマンド。空なら人数を取得しません。",
                        )
                        .size(11.0)
                        .color(DIM),
                    );
                });
                ui.end_row();

                ui.label("sudo");
                ui.checkbox(&mut self.cfg.palworld.sudo, "特権コマンドに sudo -n を付ける");
                ui.end_row();
            });

            ui.add_space(20.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("保存して再接続").strong()).clicked() {
                    match self.cfg.save() {
                        Ok(path) => {
                            self.config_notice = Some(format!("保存しました: {}", path.display()));
                            if self.cfg.is_connectable() {
                                self.handle.send(Cmd::Connect(Box::new(self.cfg.clone())));
                                self.tab = Tab::Overview;
                            }
                        }
                        Err(e) => self.config_notice = Some(format!("保存に失敗: {e:#}")),
                    }
                }
                if ui.button("切断").clicked() {
                    self.handle.send(Cmd::Disconnect);
                }
            });

            if let Some(notice) = &self.config_notice {
                ui.add_space(6.0);
                ui.colored_label(DIM, notice);
            }

            ui.add_space(16.0);
            ui.separator();
            ui.label(
                RichText::new(match &self.font {
                    Some(path) => format!("日本語フォント: {path}"),
                    None => "日本語フォントが見つかりません（日本語が □ で表示されます）".to_owned(),
                })
                .size(11.0)
                .color(DIM),
            );
        });
    }

    fn log(&mut self, ui: &mut Ui, log: &[worker::LogEntry]) {
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for entry in log {
                    let color = match entry.level {
                        Level::Info => DIM,
                        Level::Ok => GREEN,
                        Level::Warn => AMBER,
                        Level::Error => RED,
                    };
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(&entry.at).monospace().color(DIM));
                        ui.colored_label(color, &entry.text);
                    });
                }
            });
    }
}

fn widget(ui: &mut Ui, ini: &mut PalIni, spec: &palworld::Spec, busy: bool) {
    ui.add_enabled_ui(!busy, |ui| match spec.kind {
        Kind::Float { min, max } => {
            let mut v = ini.get_f32(spec.key).unwrap_or(min);
            if ui
                .add(egui::Slider::new(&mut v, min..=max).fixed_decimals(2))
                .changed()
            {
                ini.set(spec.key, palworld::fmt_f32(v));
            }
        }
        Kind::Int { min, max } => {
            let mut v = ini.get_i64(spec.key).unwrap_or(min);
            if ui.add(egui::Slider::new(&mut v, min..=max)).changed() {
                ini.set(spec.key, v.to_string());
            }
        }
        Kind::Bool => {
            let mut v = ini.get_bool(spec.key).unwrap_or(false);
            if ui.checkbox(&mut v, "").changed() {
                ini.set(spec.key, if v { "True" } else { "False" });
            }
        }
        Kind::Choice(options) => {
            let current = ini.get(spec.key).unwrap_or("").to_owned();
            let mut selected = current.clone();
            egui::ComboBox::from_id_salt(spec.key)
                .selected_text(&current)
                .show_ui(ui, |ui| {
                    for option in options {
                        ui.selectable_value(&mut selected, (*option).to_owned(), *option);
                    }
                });
            if selected != current {
                ini.set(spec.key, selected);
            }
        }
        Kind::Text => {
            let mut v = ini.get_str(spec.key).unwrap_or("").to_owned();
            if ui.text_edit_singleline(&mut v).changed() {
                // Palworld stores these quoted; keep it that way.
                ini.set(spec.key, format!("\"{}\"", v.replace('"', "")));
            }
        }
    });
}

fn gauge(ui: &mut Ui, label: &str, percent: f32, detail: &str) {
    let color = if percent >= 90.0 {
        RED
    } else if percent >= 70.0 {
        AMBER
    } else {
        GREEN
    };
    ui.group(|ui| {
        ui.set_width(210.0);
        ui.vertical(|ui| {
            ui.label(RichText::new(label).color(DIM).size(12.0));
            ui.label(
                RichText::new(format!("{percent:.0}%"))
                    .size(26.0)
                    .strong()
                    .color(color),
            );
            ui.add(
                egui::ProgressBar::new(percent / 100.0)
                    .desired_height(6.0)
                    .fill(color),
            );
            ui.label(RichText::new(detail).size(11.0).color(DIM));
        });
    });
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}
