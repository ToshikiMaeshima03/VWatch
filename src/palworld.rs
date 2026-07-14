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
    Float {
        min: f32,
        max: f32,
    },
    Int {
        min: i64,
        max: i64,
    },
    Bool,
    Choice(&'static [&'static str]),
    /// Quoted string: the ini holds `ServerName="…"`, the UI edits what's inside.
    Text,
    /// Quoted string that must not be shown by default (passwords).
    Secret,
    /// Written back exactly as typed — for values that are neither quoted nor
    /// scalar, like `CrossplayPlatforms=(Steam,Xbox,PS5,Mac)`.
    Raw,
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

/// Every key Palworld writes into `OptionSettings`, grouped the way you'd go
/// looking for them. Anything the running server didn't write out is skipped by
/// the UI, so a key a future update drops just disappears from the list rather
/// than being resurrected with a wrong default.
///
/// Ranges are deliberately wider than the in-game world-settings UI: the
/// dedicated server does *not* clamp what it reads from the ini — verified by
/// writing 5.0 to the rates, restarting, and reading back the server's own
/// shutdown write-back (still 5.0).
const TIME: &[Spec] = &[
    Spec {
        key: "DayTimeSpeedRate",
        label: "昼の長さ",
        kind: Kind::Float {
            min: 0.1,
            max: 10.0,
        },
        note: Some("大きいほど昼が速く過ぎる（=昼が短い）。0.5 で昼が2倍長い"),
    },
    Spec {
        key: "NightTimeSpeedRate",
        label: "夜の長さ",
        kind: Kind::Float {
            min: 0.1,
            max: 10.0,
        },
        note: Some("大きいほど夜が速く過ぎる（=夜が短い）。夜を飛ばしたいなら上げる"),
    },
    Spec {
        key: "AutoSaveSpan",
        label: "自動セーブ間隔(秒)",
        kind: Kind::Float {
            min: 10.0,
            max: 600.0,
        },
        note: None,
    },
    Spec {
        key: "bIsUseBackupSaveData",
        label: "セーブのバックアップを取る",
        kind: Kind::Bool,
        note: None,
    },
];

const DIFFICULTY: &[Spec] = &[
    Spec {
        key: "Difficulty",
        label: "難易度プリセット",
        kind: Kind::Choice(&["None", "Casual", "Normal", "Hard"]),
        note: Some("None 以外にすると個別のレート設定より優先される"),
    },
    Spec {
        key: "RandomizerType",
        label: "ランダマイザ",
        kind: Kind::Choice(&["None", "Region", "All"]),
        note: Some("パルの出現をランダム化する。変更後は新規ワールド推奨"),
    },
    Spec {
        key: "RandomizerSeed",
        label: "ランダマイザのシード",
        kind: Kind::Text,
        note: None,
    },
    Spec {
        key: "bIsRandomizerPalLevelRandom",
        label: "パルのレベルもランダムにする",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bHardcore",
        label: "ハードコア",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bPalLost",
        label: "死亡時にパルをロストする",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bCharacterRecreateInHardcore",
        label: "ハードコアで死亡したらキャラ再作成",
        kind: Kind::Bool,
        note: None,
    },
];

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
        key: "CollectionObjectHpRate",
        label: "採集オブジェクトの耐久",
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
        key: "ItemWeightRate",
        label: "アイテム重量倍率",
        kind: Kind::Float { min: 0.0, max: 5.0 },
        note: Some("0 で重量無制限"),
    },
    Spec {
        key: "EquipmentDurabilityDamageRate",
        label: "装備の消耗速度",
        kind: Kind::Float {
            min: 0.0,
            max: 10.0,
        },
        note: Some("0 で装備が壊れない"),
    },
    Spec {
        key: "ItemCorruptionMultiplier",
        label: "アイテムの腐敗速度",
        kind: Kind::Float {
            min: 0.0,
            max: 10.0,
        },
        note: Some("0 で腐らない"),
    },
    Spec {
        key: "MonsterFarmActionSpeedRate",
        label: "牧場の生産速度",
        kind: Kind::Float {
            min: 0.1,
            max: 10.0,
        },
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

const CONDITION: &[Spec] = &[
    Spec {
        key: "PlayerStomachDecreaceRate",
        label: "プレイヤーの空腹速度",
        kind: Kind::Float { min: 0.0, max: 5.0 },
        note: None,
    },
    Spec {
        key: "PlayerStaminaDecreaceRate",
        label: "プレイヤーのスタミナ消費",
        kind: Kind::Float { min: 0.0, max: 5.0 },
        note: None,
    },
    Spec {
        key: "PlayerAutoHPRegeneRate",
        label: "プレイヤーのHP自動回復",
        kind: Kind::Float {
            min: 0.1,
            max: 10.0,
        },
        note: None,
    },
    Spec {
        key: "PlayerAutoHpRegeneRateInSleep",
        label: "プレイヤーの睡眠時HP回復",
        kind: Kind::Float {
            min: 0.1,
            max: 10.0,
        },
        note: None,
    },
    Spec {
        key: "PalStomachDecreaceRate",
        label: "パルの空腹速度",
        kind: Kind::Float { min: 0.0, max: 5.0 },
        note: None,
    },
    Spec {
        key: "PalStaminaDecreaceRate",
        label: "パルのスタミナ消費",
        kind: Kind::Float { min: 0.0, max: 5.0 },
        note: None,
    },
    Spec {
        key: "PalAutoHPRegeneRate",
        label: "パルのHP自動回復",
        kind: Kind::Float {
            min: 0.1,
            max: 10.0,
        },
        note: None,
    },
    Spec {
        key: "PalAutoHpRegeneRateInSleep",
        label: "パルの拠点でのHP回復",
        kind: Kind::Float {
            min: 0.1,
            max: 10.0,
        },
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
        key: "bEnableInvaderEnemy",
        label: "拠点への襲撃を有効にする",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "EnablePredatorBossPal",
        label: "捕食者ボスパルを出現させる",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bIsPvP",
        label: "PvPを有効にする",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bEnablePlayerToPlayerDamage",
        label: "プレイヤー間のダメージ",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bEnableFriendlyFire",
        label: "フレンドリーファイア",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bAdditionalDropItemWhenPlayerKillingInPvPMode",
        label: "PvPキル時に追加ドロップ",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "AdditionalDropItemNumWhenPlayerKillingInPvPMode",
        label: "PvPキル時の追加ドロップ数",
        kind: Kind::Int { min: 0, max: 10 },
        note: None,
    },
    Spec {
        key: "bEnableAimAssistPad",
        label: "エイムアシスト(パッド)",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bEnableAimAssistKeyboard",
        label: "エイムアシスト(キーボード)",
        kind: Kind::Bool,
        note: None,
    },
];

/// `BaseCampWorkerMaxNum` is not clamped in the ini either — 30 survives the
/// shutdown write-back. But the binary has a `GetMaxWorkerMaxNum`, which the
/// rate settings have no equivalent of, so the game may still refuse to *assign*
/// more than 20 workers at runtime even though it stores the larger number.
const BASE: &[Spec] = &[
    Spec {
        key: "BaseCampWorkerMaxNum",
        label: "拠点で働けるパルの数",
        kind: Kind::Int { min: 1, max: 30 },
        note: Some(
            "ゲーム内UIの上限は20。20超も設定は通るが実際に配置できるかは未検証。1体ごとにAIが動くのでCPU負荷に直結する",
        ),
    },
    Spec {
        key: "BaseCampMaxNum",
        label: "拠点の総数上限(サーバー全体)",
        kind: Kind::Int { min: 1, max: 256 },
        note: None,
    },
    Spec {
        key: "BaseCampMaxNumInGuild",
        label: "ギルドが持てる拠点の数",
        kind: Kind::Int { min: 1, max: 10 },
        note: None,
    },
    Spec {
        key: "BuildObjectHpRate",
        label: "建築物の耐久",
        kind: Kind::Float {
            min: 0.1,
            max: 10.0,
        },
        note: None,
    },
    Spec {
        key: "BuildObjectDamageRate",
        label: "建築物への被ダメージ",
        kind: Kind::Float {
            min: 0.0,
            max: 10.0,
        },
        note: None,
    },
    Spec {
        key: "BuildObjectDeteriorationDamageRate",
        label: "建築物の劣化速度",
        kind: Kind::Float {
            min: 0.0,
            max: 10.0,
        },
        note: Some("0 で劣化しない"),
    },
    Spec {
        key: "MaxBuildingLimitNum",
        label: "建築数の上限",
        kind: Kind::Int { min: 0, max: 50000 },
        note: Some("0 で無制限"),
    },
    Spec {
        key: "bBuildAreaLimit",
        label: "建築範囲の制限",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bEnableDefenseOtherGuildPlayer",
        label: "他ギルドの拠点を攻撃可能にする",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bInvisibleOtherGuildBaseCampAreaFX",
        label: "他ギルドの拠点範囲を非表示",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bEnableBuildingPlayerUIdDisplay",
        label: "建築物に設置者IDを表示",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "BuildingNameDisplayCacheTTLSeconds",
        label: "建築物名の表示キャッシュ(秒)",
        kind: Kind::Int { min: 0, max: 3600 },
        note: None,
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
        key: "PhysicsActiveDropItemMaxNum",
        label: "物理演算するドロップ品の上限",
        kind: Kind::Int { min: -1, max: 1000 },
        note: Some("-1 で無制限"),
    },
    Spec {
        key: "DropItemMaxNum_UNKO",
        label: "フンの同時存在上限",
        kind: Kind::Int { min: 0, max: 1000 },
        note: None,
    },
    Spec {
        key: "DeathPenalty",
        label: "デスペナルティ",
        kind: Kind::Choice(&["None", "Item", "ItemAndEquipment", "All"]),
        note: None,
    },
    Spec {
        key: "bCanPickupOtherGuildDeathPenaltyDrop",
        label: "他ギルドの死亡ドロップを拾える",
        kind: Kind::Bool,
        note: None,
    },
];

const GUILD: &[Spec] = &[
    Spec {
        key: "GuildPlayerMaxNum",
        label: "ギルドの人数上限",
        kind: Kind::Int { min: 1, max: 100 },
        note: None,
    },
    Spec {
        key: "bAutoResetGuildNoOnlinePlayers",
        label: "無人ギルドを自動解散する",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "AutoResetGuildTimeNoOnlinePlayers",
        label: "自動解散までの時間(時)",
        kind: Kind::Float {
            min: 1.0,
            max: 720.0,
        },
        note: None,
    },
    Spec {
        key: "GuildRejoinCooldownMinutes",
        label: "ギルド再加入のクールダウン(分)",
        kind: Kind::Int { min: 0, max: 1440 },
        note: None,
    },
];

const RULES: &[Spec] = &[
    Spec {
        key: "bEnableFastTravel",
        label: "ファストトラベル",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bEnableFastTravelOnlyBaseCamp",
        label: "ファストトラベルは拠点のみ",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bIsStartLocationSelectByMap",
        label: "開始地点をマップから選ぶ",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bExistPlayerAfterLogout",
        label: "ログアウト後もキャラが残る",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bEnableNonLoginPenalty",
        label: "未ログインペナルティ",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bAllowGlobalPalboxExport",
        label: "グローバルパルボックスへの書き出し",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bAllowGlobalPalboxImport",
        label: "グローバルパルボックスからの取り込み",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bAllowEnhanceStat_Health",
        label: "ステ振り: HP",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bAllowEnhanceStat_Attack",
        label: "ステ振り: 攻撃",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bAllowEnhanceStat_Stamina",
        label: "ステ振り: スタミナ",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bAllowEnhanceStat_Weight",
        label: "ステ振り: 重量",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bAllowEnhanceStat_WorkSpeed",
        label: "ステ振り: 作業速度",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bActiveUNKO",
        label: "UNKO を有効にする",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "DenyTechnologyList",
        label: "禁止する技術のリスト",
        kind: Kind::Raw,
        note: Some("技術IDをカンマ区切りで。空なら制限なし"),
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
        key: "CoopPlayerMaxNum",
        label: "同一ギルドの同時参加上限",
        kind: Kind::Int { min: 1, max: 32 },
        note: None,
    },
    Spec {
        key: "bIsMultiplay",
        label: "マルチプレイ",
        kind: Kind::Bool,
        note: Some("専用サーバーでは通常 False のままでよい"),
    },
    Spec {
        key: "bShowPlayerList",
        label: "プレイヤー一覧を公開する",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bIsShowJoinLeftMessage",
        label: "参加・退出メッセージを出す",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "ChatPostLimitPerMinute",
        label: "チャットの投稿上限(毎分)",
        kind: Kind::Int { min: 1, max: 300 },
        note: None,
    },
    Spec {
        key: "SupplyDropSpan",
        label: "補給物資の間隔(分)",
        kind: Kind::Int { min: 0, max: 1440 },
        note: None,
    },
    Spec {
        key: "bAllowClientMod",
        label: "クライアントMODを許可",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "bUseAuth",
        label: "認証を要求する",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "CrossplayPlatforms",
        label: "クロスプレイ対象",
        kind: Kind::Raw,
        note: Some("(Steam,Xbox,PS5,Mac) の形式。括弧ごと編集する"),
    },
    Spec {
        key: "Region",
        label: "リージョン",
        kind: Kind::Text,
        note: None,
    },
    Spec {
        key: "BanListURL",
        label: "BANリストのURL",
        kind: Kind::Text,
        note: None,
    },
    Spec {
        key: "LogFormatType",
        label: "ログ形式",
        kind: Kind::Choice(&["Text", "Json"]),
        note: None,
    },
    Spec {
        key: "ServerReplicatePawnCullDistance",
        label: "同期する距離",
        kind: Kind::Float {
            min: 5000.0,
            max: 30000.0,
        },
        note: Some("下げるとCPU負荷が減るが、遠くのパルが動かなくなる"),
    },
    Spec {
        key: "ItemContainerForceMarkDirtyInterval",
        label: "コンテナ同期の間隔(秒)",
        kind: Kind::Float {
            min: 0.1,
            max: 10.0,
        },
        note: None,
    },
    Spec {
        key: "PlayerDataPalStorageUpdateCheckTickInterval",
        label: "パルボックス同期の間隔(秒)",
        kind: Kind::Float {
            min: 0.1,
            max: 10.0,
        },
        note: None,
    },
    Spec {
        key: "AutoTransferMasterCheckIntervalSeconds",
        label: "ギルド主の自動移譲チェック間隔(秒)",
        kind: Kind::Float {
            min: 60.0,
            max: 86400.0,
        },
        note: None,
    },
    Spec {
        key: "AutoTransferMasterThresholdDays",
        label: "ギルド主の自動移譲までの日数",
        kind: Kind::Int { min: 1, max: 365 },
        note: None,
    },
];

const VOICE: &[Spec] = &[
    Spec {
        key: "bEnableVoiceChat",
        label: "ボイスチャット",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "VoiceChatMaxVolumeDistance",
        label: "最大音量の距離",
        kind: Kind::Float {
            min: 100.0,
            max: 30000.0,
        },
        note: None,
    },
    Spec {
        key: "VoiceChatZeroVolumeDistance",
        label: "音量ゼロになる距離",
        kind: Kind::Float {
            min: 100.0,
            max: 30000.0,
        },
        note: None,
    },
];

/// Ports and passwords. `AdminPassword` / `ServerPassword` render masked —
/// screenshots of this tab end up in `docs/` in a public repo.
///
/// `PublicIP` is the same hazard in reverse: filling it in publishes the real
/// address the playit tunnel exists to hide.
const ACCESS: &[Spec] = &[
    Spec {
        key: "ServerPassword",
        label: "サーバーパスワード",
        kind: Kind::Secret,
        note: Some("空にすると誰でも入れる"),
    },
    Spec {
        key: "AdminPassword",
        label: "管理者パスワード",
        kind: Kind::Secret,
        note: Some("RCON のパスワードでもある。変えると palbot の config.json も直す必要がある"),
    },
    Spec {
        key: "PublicPort",
        label: "公開ポート",
        kind: Kind::Int { min: 1, max: 65535 },
        note: None,
    },
    Spec {
        key: "PublicIP",
        label: "公開IP",
        kind: Kind::Text,
        note: Some("空のままにする。書くと playit トンネルで隠している実IPを晒すことになる"),
    },
    Spec {
        key: "RCONEnabled",
        label: "RCON を有効にする",
        kind: Kind::Bool,
        note: Some("切ると VWatch のプレイヤー一覧と palbot が動かなくなる"),
    },
    Spec {
        key: "RCONPort",
        label: "RCON ポート",
        kind: Kind::Int { min: 1, max: 65535 },
        note: None,
    },
    Spec {
        key: "RESTAPIEnabled",
        label: "REST API を有効にする",
        kind: Kind::Bool,
        note: None,
    },
    Spec {
        key: "RESTAPIPort",
        label: "REST API ポート",
        kind: Kind::Int { min: 1, max: 65535 },
        note: None,
    },
];

pub fn groups() -> Vec<Group> {
    vec![
        Group {
            title: "時間・セーブ",
            specs: TIME,
        },
        Group {
            title: "難易度",
            specs: DIFFICULTY,
        },
        Group {
            title: "レート",
            specs: RATES,
        },
        Group {
            title: "プレイヤー・パルの状態",
            specs: CONDITION,
        },
        Group {
            title: "戦闘",
            specs: COMBAT,
        },
        Group {
            title: "拠点・建築",
            specs: BASE,
        },
        Group {
            title: "ドロップ",
            specs: DROPS,
        },
        Group {
            title: "ギルド",
            specs: GUILD,
        },
        Group {
            title: "ゲームルール",
            specs: RULES,
        },
        Group {
            title: "サーバー",
            specs: SERVER,
        },
        Group {
            title: "ボイスチャット",
            specs: VOICE,
        },
        Group {
            title: "接続・パスワード",
            specs: ACCESS,
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

    /// Every key the live server (v1.0.0.100427) writes into `OptionSettings`,
    /// in file order. The UI is meant to cover all of them; a key here with no
    /// spec is a setting the user can't reach, and a spec whose key isn't here
    /// is a typo that would silently never render.
    const LIVE_KEYS: &[&str] = &[
        "Difficulty",
        "RandomizerType",
        "RandomizerSeed",
        "bIsRandomizerPalLevelRandom",
        "DayTimeSpeedRate",
        "NightTimeSpeedRate",
        "ExpRate",
        "PalCaptureRate",
        "PalSpawnNumRate",
        "PalDamageRateAttack",
        "PalDamageRateDefense",
        "PlayerDamageRateAttack",
        "PlayerDamageRateDefense",
        "PlayerStomachDecreaceRate",
        "PlayerStaminaDecreaceRate",
        "PlayerAutoHPRegeneRate",
        "PlayerAutoHpRegeneRateInSleep",
        "PalStomachDecreaceRate",
        "PalStaminaDecreaceRate",
        "PalAutoHPRegeneRate",
        "PalAutoHpRegeneRateInSleep",
        "BuildObjectHpRate",
        "BuildObjectDamageRate",
        "BuildObjectDeteriorationDamageRate",
        "CollectionDropRate",
        "CollectionObjectHpRate",
        "CollectionObjectRespawnSpeedRate",
        "EnemyDropItemRate",
        "DeathPenalty",
        "bEnablePlayerToPlayerDamage",
        "bEnableFriendlyFire",
        "bEnableInvaderEnemy",
        "bActiveUNKO",
        "bEnableAimAssistPad",
        "bEnableAimAssistKeyboard",
        "DropItemMaxNum",
        "PhysicsActiveDropItemMaxNum",
        "DropItemMaxNum_UNKO",
        "BaseCampMaxNum",
        "BaseCampWorkerMaxNum",
        "DropItemAliveMaxHours",
        "bAutoResetGuildNoOnlinePlayers",
        "AutoResetGuildTimeNoOnlinePlayers",
        "GuildPlayerMaxNum",
        "BaseCampMaxNumInGuild",
        "PalEggDefaultHatchingTime",
        "WorkSpeedRate",
        "AutoSaveSpan",
        "bIsMultiplay",
        "bIsPvP",
        "bHardcore",
        "bPalLost",
        "bCharacterRecreateInHardcore",
        "bCanPickupOtherGuildDeathPenaltyDrop",
        "bEnableNonLoginPenalty",
        "bEnableFastTravel",
        "bEnableFastTravelOnlyBaseCamp",
        "bIsStartLocationSelectByMap",
        "bExistPlayerAfterLogout",
        "bEnableDefenseOtherGuildPlayer",
        "bInvisibleOtherGuildBaseCampAreaFX",
        "bBuildAreaLimit",
        "ItemWeightRate",
        "CoopPlayerMaxNum",
        "ServerPlayerMaxNum",
        "ServerName",
        "ServerDescription",
        "AdminPassword",
        "ServerPassword",
        "bAllowClientMod",
        "PublicPort",
        "PublicIP",
        "RCONEnabled",
        "RCONPort",
        "Region",
        "bUseAuth",
        "BanListURL",
        "RESTAPIEnabled",
        "RESTAPIPort",
        "bShowPlayerList",
        "ChatPostLimitPerMinute",
        "CrossplayPlatforms",
        "bIsUseBackupSaveData",
        "LogFormatType",
        "bIsShowJoinLeftMessage",
        "SupplyDropSpan",
        "EnablePredatorBossPal",
        "MaxBuildingLimitNum",
        "ServerReplicatePawnCullDistance",
        "bAllowGlobalPalboxExport",
        "bAllowGlobalPalboxImport",
        "EquipmentDurabilityDamageRate",
        "ItemContainerForceMarkDirtyInterval",
        "PlayerDataPalStorageUpdateCheckTickInterval",
        "ItemCorruptionMultiplier",
        "MonsterFarmActionSpeedRate",
        "DenyTechnologyList",
        "GuildRejoinCooldownMinutes",
        "AutoTransferMasterCheckIntervalSeconds",
        "AutoTransferMasterThresholdDays",
        "AdditionalDropItemNumWhenPlayerKillingInPvPMode",
        "bAdditionalDropItemWhenPlayerKillingInPvPMode",
        "bEnableVoiceChat",
        "VoiceChatMaxVolumeDistance",
        "VoiceChatZeroVolumeDistance",
        "bAllowEnhanceStat_Health",
        "bAllowEnhanceStat_Attack",
        "bAllowEnhanceStat_Stamina",
        "bAllowEnhanceStat_Weight",
        "bAllowEnhanceStat_WorkSpeed",
        "bEnableBuildingPlayerUIdDisplay",
        "BuildingNameDisplayCacheTTLSeconds",
    ];

    fn spec_keys() -> Vec<&'static str> {
        groups()
            .iter()
            .flat_map(|g| g.specs.iter().map(|s| s.key))
            .collect()
    }

    #[test]
    fn every_live_setting_is_reachable_from_the_ui() {
        let keys = spec_keys();
        let missing: Vec<_> = LIVE_KEYS
            .iter()
            .filter(|k| !keys.contains(k))
            .copied()
            .collect();
        assert!(missing.is_empty(), "settings with no UI spec: {missing:?}");
    }

    #[test]
    fn no_spec_points_at_a_key_the_server_never_writes() {
        let unknown: Vec<_> = spec_keys()
            .into_iter()
            .filter(|k| !LIVE_KEYS.contains(k))
            .collect();
        assert!(unknown.is_empty(), "spec keys not in the ini: {unknown:?}");
    }

    #[test]
    fn no_setting_is_listed_twice() {
        let mut keys = spec_keys();
        let before = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(before, keys.len(), "a key appears in two groups");
    }
}
