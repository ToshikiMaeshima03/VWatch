//! Parse and patch `PalWorldSettings.ini`.
//!
//! The whole file matters only for one line:
//!
//! ```text
//! OptionSettings=(Difficulty=None,DayTimeSpeedRate=1.000000,ServerName="アルカトラズ島",...)
//! ```
//!
//! We keep every other line byte-identical and rewrite only that one, preserving
//! key order and any keys we don't know about — a future Palworld update can add
//! settings without VWatch silently dropping them.

use anyhow::{Context, Result};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PalIni {
    lines: Vec<String>,
    option_line: usize,
    /// Insertion-ordered; Palworld tolerates any order but a stable diff is nicer.
    options: Vec<(String, String)>,
}

impl PalIni {
    pub fn parse(text: &str) -> Result<Self> {
        let lines: Vec<String> = text.lines().map(str::to_owned).collect();
        let option_line = lines
            .iter()
            .position(|l| l.trim_start().starts_with("OptionSettings="))
            .context("no `OptionSettings=(...)` line found in PalWorldSettings.ini")?;
        let options = parse_option_list(&lines[option_line])?;
        Ok(Self {
            lines,
            option_line,
            options,
        })
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.options
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Numeric read that tolerates Palworld's `1.000000` formatting.
    pub fn get_f32(&self, key: &str) -> Option<f32> {
        self.get(key)?.parse().ok()
    }

    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.get(key)?.parse().ok()
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        match self.get(key)? {
            "True" | "true" => Some(true),
            "False" | "false" => Some(false),
            _ => None,
        }
    }

    /// Value of a quoted string setting, without the surrounding quotes.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        Some(self.get(key)?.trim_matches('"'))
    }

    /// Overwrite an existing key, or append it if Palworld didn't write it out.
    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        let value = value.into();
        match self.options.iter_mut().find(|(k, _)| k == key) {
            Some((_, v)) => *v = value,
            None => self.options.push((key.to_owned(), value)),
        }
    }

    pub fn options(&self) -> &[(String, String)] {
        &self.options
    }

    /// Rebuild the full file with the `OptionSettings` line regenerated.
    pub fn render(&self) -> String {
        let body: Vec<String> = self
            .options
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        let rebuilt = format!("OptionSettings=({})", body.join(","));

        let mut out = self.lines.clone();
        out[self.option_line] = rebuilt;
        let mut text = out.join("\n");
        text.push('\n');
        text
    }
}

/// Split `OptionSettings=(a=1,b="x,y",c=(d=2))` into its top-level `key=value` pairs.
///
/// Naive `split(',')` breaks on `ServerName="ある,島"` and on any nested
/// parenthesised value, so track quote and paren state.
fn parse_option_list(line: &str) -> Result<Vec<(String, String)>> {
    let open = line
        .find('(')
        .context("OptionSettings line has no opening `(`")?;
    let close = line
        .rfind(')')
        .context("OptionSettings line has no closing `)`")?;
    anyhow::ensure!(close > open, "OptionSettings line has `)` before `(`");
    let inner = &line[open + 1..close];

    let mut out = Vec::new();
    for field in split_top_level(inner) {
        let field = field.trim();
        if field.is_empty() {
            continue;
        }
        // `split_once` — a value may itself contain `=` (e.g. a base64 blob).
        let (k, v) = field
            .split_once('=')
            .with_context(|| format!("malformed OptionSettings entry: `{field}`"))?;
        out.push((k.trim().to_owned(), v.trim().to_owned()));
    }
    Ok(out)
}

fn split_top_level(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut depth = 0usize;

    for c in s.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                cur.push(c);
            }
            '(' if !in_quotes => {
                depth += 1;
                cur.push(c);
            }
            ')' if !in_quotes => {
                depth = depth.saturating_sub(1);
                cur.push(c);
            }
            ',' if !in_quotes && depth == 0 => {
                parts.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

/// Palworld writes floats as `5.000000`; match that so our diffs stay clean.
pub fn fmt_f32(v: f32) -> String {
    format!("{v:.6}")
}

// ---------------------------------------------------------------------------
// Which settings the UI exposes, and how to render each one.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub enum Kind {
    Float { min: f32, max: f32 },
    Int { min: i64, max: i64 },
    Bool,
    Choice(&'static [&'static str]),
    Text,
}

#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: Kind,
    pub note: Option<&'static str>,
}

pub struct Group {
    pub title: &'static str,
    pub specs: &'static [Spec],
}

/// The in-game world-settings UI caps rate sliders at 3.0, but the dedicated
/// server does *not* clamp what it reads from the ini — verified by writing 5.0,
/// restarting, and reading back the server's own shutdown write-back (still 5.0).
/// So the sliders here go well past 3.0 on purpose.
const RATES: &[Spec] = &[
    Spec {
        key: "ExpRate",
        label: "経験値倍率",
        kind: Kind::Float {
            min: 0.1,
            max: 20.0,
        },
        note: Some("ゲーム内UIの上限は3.0だが、ini経由なら丸められない"),
    },
    Spec {
        key: "CollectionDropRate",
        label: "採集アイテム量",
        kind: Kind::Float {
            min: 0.1,
            max: 20.0,
        },
        note: None,
    },
    Spec {
        key: "EnemyDropItemRate",
        label: "パルからのドロップ量",
        kind: Kind::Float {
            min: 0.1,
            max: 20.0,
        },
        note: None,
    },
    Spec {
        key: "PalCaptureRate",
        label: "パル捕獲率",
        kind: Kind::Float {
            min: 0.1,
            max: 10.0,
        },
        note: None,
    },
    Spec {
        key: "PalSpawnNumRate",
        label: "パル出現数",
        kind: Kind::Float { min: 0.1, max: 5.0 },
        note: Some("上げすぎるとサーバーのCPU負荷が上がる"),
    },
    Spec {
        key: "WorkSpeedRate",
        label: "作業速度",
        kind: Kind::Float {
            min: 0.1,
            max: 10.0,
        },
        note: None,
    },
    Spec {
        key: "CollectionObjectRespawnSpeedRate",
        label: "採集物のリスポップ速度",
        kind: Kind::Float { min: 0.1, max: 5.0 },
        note: None,
    },
    Spec {
        key: "ItemWeightRate",
        label: "アイテム重量倍率",
        kind: Kind::Float { min: 0.0, max: 5.0 },
        note: Some("0 で重量無制限"),
    },
];

const DROPS: &[Spec] = &[
    Spec {
        key: "DropItemMaxNum",
        label: "ドロップ品の同時存在上限",
        kind: Kind::Int {
            min: 100,
            max: 20000,
        },
        note: Some("ドロップ率を上げるならここも上げないと古い物から消える"),
    },
    Spec {
        key: "DropItemAliveMaxHours",
        label: "ドロップ品の消滅時間(時)",
        kind: Kind::Float {
            min: 0.1,
            max: 24.0,
        },
        note: None,
    },
    Spec {
        key: "DeathPenalty",
        label: "デスペナルティ",
        kind: Kind::Choice(&["None", "Item", "ItemAndEquipment", "All"]),
        note: None,
    },
];

const COMBAT: &[Spec] = &[
    Spec {
        key: "PalDamageRateAttack",
        label: "パルの与ダメージ",
        kind: Kind::Float { min: 0.1, max: 5.0 },
        note: None,
    },
    Spec {
        key: "PalDamageRateDefense",
        label: "パルの被ダメージ",
        kind: Kind::Float { min: 0.1, max: 5.0 },
        note: None,
    },
    Spec {
        key: "PlayerDamageRateAttack",
        label: "プレイヤーの与ダメージ",
        kind: Kind::Float { min: 0.1, max: 5.0 },
        note: None,
    },
    Spec {
        key: "PlayerDamageRateDefense",
        label: "プレイヤーの被ダメージ",
        kind: Kind::Float { min: 0.1, max: 5.0 },
        note: None,
    },
    Spec {
        key: "PalAutoHPRegeneRate",
        label: "パルのHP自動回復",
        kind: Kind::Float { min: 0.1, max: 5.0 },
        note: None,
    },
    Spec {
        key: "bIsPvP",
        label: "PvPを有効にする",
        kind: Kind::Bool,
        note: None,
    },
];

const SERVER: &[Spec] = &[
    Spec {
        key: "ServerName",
        label: "サーバー名",
        kind: Kind::Text,
        note: None,
    },
    Spec {
        key: "ServerDescription",
        label: "サーバー説明",
        kind: Kind::Text,
        note: None,
    },
    Spec {
        key: "ServerPlayerMaxNum",
        label: "最大プレイヤー数",
        kind: Kind::Int { min: 1, max: 32 },
        note: None,
    },
    Spec {
        key: "PalEggDefaultHatchingTime",
        label: "卵の孵化時間(時)",
        kind: Kind::Float {
            min: 0.0,
            max: 72.0,
        },
        note: Some("0 で即孵化"),
    },
];

pub fn groups() -> Vec<Group> {
    vec![
        Group {
            title: "レート",
            specs: RATES,
        },
        Group {
            title: "ドロップ",
            specs: DROPS,
        },
        Group {
            title: "戦闘",
            specs: COMBAT,
        },
        Group {
            title: "サーバー",
            specs: SERVER,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like the real file on the VPS, including the Japanese server name
    /// and a comma inside a quoted value — the case naive splitting gets wrong.
    const SAMPLE: &str = concat!(
        "[/Script/Pal.PalGameWorldSettings]\n",
        "OptionSettings=(Difficulty=None,DayTimeSpeedRate=1.000000,ExpRate=5.000000,",
        "ServerName=\"アルカトラズ島, 第2区画\",CollectionDropRate=5.000000,",
        "EnemyDropItemRate=5.000000,DropItemMaxNum=3000,bIsPvP=False,",
        "DeathPenalty=All,Nested=(A=1,B=2))\n",
    );

    #[test]
    fn parses_quoted_comma_without_splitting_it() {
        let ini = PalIni::parse(SAMPLE).unwrap();
        assert_eq!(ini.get_str("ServerName"), Some("アルカトラズ島, 第2区画"));
    }

    #[test]
    fn parses_nested_parens_as_one_value() {
        let ini = PalIni::parse(SAMPLE).unwrap();
        assert_eq!(ini.get("Nested"), Some("(A=1,B=2)"));
    }

    #[test]
    fn reads_typed_values() {
        let ini = PalIni::parse(SAMPLE).unwrap();
        assert_eq!(ini.get_f32("ExpRate"), Some(5.0));
        assert_eq!(ini.get_i64("DropItemMaxNum"), Some(3000));
        assert_eq!(ini.get_bool("bIsPvP"), Some(false));
        assert_eq!(ini.get("DeathPenalty"), Some("All"));
    }

    #[test]
    fn set_then_render_round_trips_and_preserves_unknown_keys() {
        let mut ini = PalIni::parse(SAMPLE).unwrap();
        ini.set("ExpRate", fmt_f32(2.5));
        let rendered = ini.render();

        let reparsed = PalIni::parse(&rendered).unwrap();
        assert_eq!(reparsed.get_f32("ExpRate"), Some(2.5));
        // Everything we never touched must survive the round-trip verbatim.
        assert_eq!(
            reparsed.get_str("ServerName"),
            Some("アルカトラズ島, 第2区画")
        );
        assert_eq!(reparsed.get("Nested"), Some("(A=1,B=2)"));
        assert_eq!(reparsed.get("Difficulty"), Some("None"));
        assert_eq!(reparsed.options().len(), ini.options().len());
    }

    #[test]
    fn render_keeps_the_section_header_line() {
        let ini = PalIni::parse(SAMPLE).unwrap();
        assert!(
            ini.render()
                .starts_with("[/Script/Pal.PalGameWorldSettings]\n")
        );
    }

    #[test]
    fn set_appends_a_key_palworld_omitted() {
        let mut ini = PalIni::parse(SAMPLE).unwrap();
        assert_eq!(ini.get("BrandNewSetting"), None);
        ini.set("BrandNewSetting", "7");
        let reparsed = PalIni::parse(&ini.render()).unwrap();
        assert_eq!(reparsed.get("BrandNewSetting"), Some("7"));
    }

    #[test]
    fn missing_option_line_is_an_error_not_a_panic() {
        assert!(PalIni::parse("[/Script/Pal.PalGameWorldSettings]\n").is_err());
    }
}
