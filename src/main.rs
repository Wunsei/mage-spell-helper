//! Spellcasting helper: a small web server that builds and rolls a dice pool.
//!
//! This file is organized in six parts, top to bottom:
//!   1. Rules      which spell factors exist, their colors, and the CSV loading
//!   2. Errors     the error type the endpoints share
//!   3. Pool       turns the player's choices into a dice pool
//!   4. Rolling    rolls dice with random.org
//!   5. Endpoints  the three API calls the web page makes
//!   6. Server     starts everything

use std::collections::HashMap;
use std::time::Duration;

use axum::{
    http::StatusCode,
    response::Html,
    routing::{get, post},
    Json, Router,
};
use rand::{rngs::OsRng, Rng};
use serde::{Deserialize, Serialize};

// =====================================================================
// 1. RULES
// =====================================================================

// ---- Numbers you might want to tweak ----

const DIE_SIDES: u32 = 6; // every die is a d6
const HIT_MIN: u32 = 5; // a rolled 5 or 6 is a hit

const DEFAULT_BASE_DICE: u32 = 5; // starting value of the "Base Dice" field
const MAX_BASE_DICE: u32 = 100; // largest value that field accepts
const MAX_MANA: u32 = 99; // largest amount of any one mana type
const MAX_DICE_PER_ROLL: u32 = 1000;

// Desperate casting: extra dice now, fewer Base Dice after each roll.
const DESPERATE_ID: &str = "desperate";
const DESPERATE_LABEL: &str = "Desperate casting";
const DESPERATE_DICE: i32 = 3;
const DESPERATE_BASE_COST: u32 = 1;
const DESPERATE_COLOR: &str = "#ffb3d1";

// The chosen Arcana decides how many factors from the A&As section you may use
// (the aa_slots column of rules/arcana.csv).
const ARCANA_FACTOR: &str = "arcana";
const AA_SECTION: &str = "A&As";

// ---- The list of spell factors ----

/// A factor is either a row of buttons (pick one option) or a typed-in amount.
#[derive(Serialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Choice, // e.g. Range: Touch / Near / Far
    Count,  // e.g. Matching mana: how many you paid
}

struct FactorDef {
    id: &'static str,      // unique name
    label: &'static str,   // name shown on the page
    section: &'static str, // section header it appears under ("" = none)
    color: &'static str,   // color of its boxes and dice
    table: &'static str,   // CSV file (without .csv) holding its numbers
    kind: Kind,
}

/// A button factor with its own table, rules/<id>.csv
const fn choice_factor(
    id: &'static str,
    label: &'static str,
    section: &'static str,
    color: &'static str,
) -> FactorDef {
    FactorDef { id, label, section, color, table: id, kind: Kind::Choice }
}

/// A typed-in amount. Its dice-per-unit is the row with the same id in rules/mana.csv
const fn mana_factor(
    id: &'static str,
    label: &'static str,
    section: &'static str,
    color: &'static str,
) -> FactorDef {
    FactorDef { id, label, section, color, table: "mana", kind: Kind::Count }
}

// The order here is the order on the page.
//
// Colors: each family runs from light to dark, so factors stay easy to tell
// apart even without seeing color well.
//   bright green  Arcana    cool  SCOPE    warm  A&As    dark greens  MANA
const FACTORS: [FactorDef; 12] = [
    choice_factor("arcana", "Arcana", "CASTING ARCANA", "#39ff14"),
    choice_factor("duration", "Duration", "SCOPE", "#8ad8ff"),
    choice_factor("range", "Range", "SCOPE", "#3f6fd8"),
    choice_factor("size", "Size", "SCOPE", "#9a8cff"),
    choice_factor("intensity", "Intensity", "SCOPE", "#1fa596"),
    choice_factor("casting_time", "Casting Time", "A&As", "#ffd84d"),
    choice_factor("incantation", "Incantation", "A&As", "#ff9440"),
    choice_factor("gesture", "Gesture", "A&As", "#f0505a"),
    choice_factor("reagents", "Reagents", "A&As", "#a8482f"),
    mana_factor("mana_matching", "Matching mana", "MANA", "#1fa652"),
    mana_factor("mana_base", "Base mana", "MANA", "#12813f"),
    mana_factor("mana_nonmatching", "Non-matching mana", "MANA", "#0b5f31"),
];

// ---- Loading the CSV tables ----

/// One row of a CSV table. Required columns: id,label,dice
/// Optional column: aa_slots (only used in arcana.csv).
#[derive(Serialize, Deserialize)]
struct Choice {
    id: String,
    label: String,
    dice: i32, // dice added (may be negative). For mana: dice per mana paid
    #[serde(default)]
    aa_slots: u32,
}

/// A factor together with its options, ready to send to the web page.
#[derive(Serialize)]
struct Factor {
    id: String,
    label: String,
    section: String,
    color: String,
    kind: Kind,
    choices: Vec<Choice>,
}

/// Reads every table from disk. This runs on each request, so you can edit
/// a CSV and just refresh the page (no restart needed).
fn load_rules() -> Result<Vec<Factor>, String> {
    FACTORS.iter().map(load_factor).collect()
}

fn load_factor(def: &FactorDef) -> Result<Factor, String> {
    let path = format!("rules/{}.csv", def.table);
    let mut choices = read_table(&path)?;
    let mut label = def.label.to_string();

    if def.kind == Kind::Count {
        // A mana factor uses just its own row of the shared mana table
        choices.retain(|row| row.id == def.id);
        match choices.first() {
            Some(row) => label = row.label.clone(),
            None => return Err(format!("{path} has no row with id '{}'", def.id)),
        }
    }

    Ok(Factor {
        id: def.id.to_string(),
        label,
        section: def.section.to_string(),
        color: def.color.to_string(),
        kind: def.kind,
        choices,
    })
}

fn read_table(path: &str) -> Result<Vec<Choice>, String> {
    let mut reader =
        csv::Reader::from_path(path).map_err(|e| format!("Could not open {path}: {e}"))?;
    let mut rows = Vec::new();
    for row in reader.deserialize() {
        rows.push(row.map_err(|e| format!("Bad row in {path}: {e}"))?);
    }
    Ok(rows)
}

// =====================================================================
// 2. ERRORS
// =====================================================================

/// An HTTP status plus a message that the web page shows to the player.
type ApiError = (StatusCode, String);

/// The player asked for something that isn't allowed.
fn bad_request(message: String) -> ApiError {
    (StatusCode::BAD_REQUEST, message)
}

/// Something is wrong on our side (for example a broken CSV file).
fn server_error(message: String) -> ApiError {
    (StatusCode::INTERNAL_SERVER_ERROR, message)
}

// =====================================================================
// 3. POOL: turning the player's choices into dice
// =====================================================================

/// Everything the player has set on the page.
#[derive(Deserialize)]
struct PoolRequest {
    base_dice: u32,
    #[serde(default)]
    desperate: bool,
    /// factor id -> chosen option id, e.g. { "range": "far" }
    selections: HashMap<String, String>,
    /// factor id -> amount paid, e.g. { "mana_matching": 2 }
    #[serde(default)]
    mana_paid: HashMap<String, u32>,
}

/// Dice added (or removed, if negative) by one factor.
#[derive(Serialize)]
struct Addition {
    factor_id: String,
    factor: String, // e.g. "Range"
    choice: String, // e.g. "Far", or "×2" for mana, or "" for Desperate casting
    dice: i32,
    color: String,
}

#[derive(Serialize)]
struct PoolResponse {
    base_dice: u32,
    additions: Vec<Addition>,
    total_dice: i32,  // may be below zero
    notation: String, // e.g. "8d6"
}

fn compute_pool(req: &PoolRequest) -> Result<PoolResponse, ApiError> {
    if req.base_dice > MAX_BASE_DICE {
        return Err(bad_request(format!("Base dice can be at most {}", MAX_BASE_DICE)));
    }
    let factors = load_rules().map_err(server_error)?;

    let mut additions = Vec::new();
    if req.desperate {
        additions.push(desperate_addition());
    }
    for factor in &factors {
        let addition = match factor.kind {
            Kind::Choice => choice_addition(factor, req)?,
            Kind::Count => mana_addition(factor, req)?,
        };
        additions.extend(addition); // adds nothing if the factor isn't used
    }
    check_aa_limit(&factors, req)?;

    // No clamping: the pool is allowed to go below zero.
    let total_dice = req.base_dice as i32 + additions.iter().map(|a| a.dice).sum::<i32>();

    Ok(PoolResponse {
        base_dice: req.base_dice,
        additions,
        total_dice,
        notation: format!("{}d{}", total_dice, DIE_SIDES),
    })
}

fn desperate_addition() -> Addition {
    Addition {
        factor_id: DESPERATE_ID.to_string(),
        factor: DESPERATE_LABEL.to_string(),
        choice: String::new(),
        dice: DESPERATE_DICE,
        color: DESPERATE_COLOR.to_string(),
    }
}

/// The option the player picked for a button factor, if any.
fn selected_choice<'a>(
    factor: &'a Factor,
    req: &PoolRequest,
) -> Result<Option<&'a Choice>, ApiError> {
    let Some(choice_id) = req.selections.get(&factor.id) else {
        return Ok(None);
    };
    factor
        .choices
        .iter()
        .find(|choice| &choice.id == choice_id)
        .map(Some)
        .ok_or_else(|| bad_request(format!("Unknown option '{}' for {}", choice_id, factor.label)))
}

fn choice_addition(factor: &Factor, req: &PoolRequest) -> Result<Option<Addition>, ApiError> {
    Ok(selected_choice(factor, req)?.map(|choice| Addition {
        factor_id: factor.id.clone(),
        factor: factor.label.clone(),
        choice: choice.label.clone(),
        dice: choice.dice,
        color: factor.color.clone(),
    }))
}

fn mana_addition(factor: &Factor, req: &PoolRequest) -> Result<Option<Addition>, ApiError> {
    let paid = req.mana_paid.get(&factor.id).copied().unwrap_or(0);
    if paid == 0 {
        return Ok(None);
    }
    if paid > MAX_MANA {
        return Err(bad_request(format!("{} can be at most {}", factor.label, MAX_MANA)));
    }
    let Some(dice_per_mana) = factor.choices.first() else {
        return Ok(None);
    };
    Ok(Some(Addition {
        factor_id: factor.id.clone(),
        factor: factor.label.clone(),
        choice: format!("×{}", paid),
        dice: dice_per_mana.dice * paid as i32,
        color: factor.color.clone(),
    }))
}

/// The chosen Arcana limits how many A&A factors may be used.
fn check_aa_limit(factors: &[Factor], req: &PoolRequest) -> Result<(), ApiError> {
    let slots_available = match factors.iter().find(|f| f.id == ARCANA_FACTOR) {
        Some(arcana) => selected_choice(arcana, req)?.map_or(0, |choice| choice.aa_slots),
        None => 0,
    };
    let slots_used = factors
        .iter()
        .filter(|f| f.section == AA_SECTION && req.selections.contains_key(&f.id))
        .count() as u32;

    if slots_used > slots_available {
        return Err(bad_request(format!(
            "{} {} chosen, but only {} available",
            slots_used, AA_SECTION, slots_available
        )));
    }
    Ok(())
}

// =====================================================================
// 4. ROLLING: true random numbers from random.org
// =====================================================================
//
// If random.org can't be reached, the roll falls back to the operating
// system's random number generator, and the result says so.

#[derive(Serialize)]
struct RolledDice {
    source: &'static str, // "random.org" or "local"
    rolls: Vec<u32>,
    note: Option<String>, // why random.org wasn't used, if it wasn't
}

/// Rolls `count` dice with `sides` sides. This waits on the network, so call
/// it from a blocking thread.
fn roll_dice(count: u32, sides: u32) -> RolledDice {
    match fetch_from_random_org(count, sides) {
        Ok(rolls) => RolledDice { source: "random.org", rolls, note: None },
        Err(reason) => RolledDice {
            source: "local",
            rolls: roll_with_os_entropy(count, sides),
            note: Some(reason),
        },
    }
}

/// random.org's free plain-text interface: no account needed, but each IP
/// address has a daily allowance.
fn fetch_from_random_org(count: u32, sides: u32) -> Result<Vec<u32>, String> {
    let url = format!(
        "https://www.random.org/integers/?num={count}&min=1&max={sides}&col=1&base=10&format=plain&rnd=new"
    );
    let body = ureq::get(&url)
        .set("User-Agent", "spell-calc (personal dice roller)")
        .timeout(Duration::from_secs(6))
        .call()
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())?;

    let rolls = body
        .split_whitespace()
        .map(|word| word.parse::<u32>().map_err(|e| format!("unexpected reply: {e}")))
        .collect::<Result<Vec<u32>, String>>()?;

    let all_valid = rolls.iter().all(|&r| (1..=sides).contains(&r));
    if rolls.len() != count as usize || !all_valid {
        return Err("random.org sent back the wrong numbers".to_string());
    }
    Ok(rolls)
}

fn roll_with_os_entropy(count: u32, sides: u32) -> Vec<u32> {
    let mut rng = OsRng;
    (0..count).map(|_| rng.gen_range(1..=sides)).collect()
}

// =====================================================================
// 5. ENDPOINTS: what the web page asks for
// =====================================================================

// ---- GET /api/rules: everything the page needs to draw itself ----

#[derive(Serialize)]
struct DesperateInfo {
    label: &'static str,
    dice: i32,
    base_cost: u32,
    color: &'static str,
}

#[derive(Serialize)]
struct RulesResponse {
    default_base_dice: u32,
    max_base_dice: u32,
    max_mana: u32,
    sides: u32,
    hit_min: u32,
    arcana_factor: &'static str,
    aa_section: &'static str,
    desperate: DesperateInfo,
    factors: Vec<Factor>,
}

async fn get_rules() -> Result<Json<RulesResponse>, ApiError> {
    Ok(Json(RulesResponse {
        default_base_dice: DEFAULT_BASE_DICE,
        max_base_dice: MAX_BASE_DICE,
        max_mana: MAX_MANA,
        sides: DIE_SIDES,
        hit_min: HIT_MIN,
        arcana_factor: ARCANA_FACTOR,
        aa_section: AA_SECTION,
        desperate: DesperateInfo {
            label: DESPERATE_LABEL,
            dice: DESPERATE_DICE,
            base_cost: DESPERATE_BASE_COST,
            color: DESPERATE_COLOR,
        },
        factors: load_rules().map_err(server_error)?,
    }))
}

// ---- POST /api/pool: build the dice pool from the player's choices ----

async fn build_pool(Json(req): Json<PoolRequest>) -> Result<Json<PoolResponse>, ApiError> {
    Ok(Json(compute_pool(&req)?))
}

// ---- POST /api/roll: roll that pool ----

#[derive(Serialize)]
struct RollResponse {
    #[serde(flatten)]
    dice: RolledDice,
    /// Base Dice after this roll (Desperate casting lowers it).
    base_dice_after: u32,
}

async fn roll(Json(req): Json<PoolRequest>) -> Result<Json<RollResponse>, ApiError> {
    let pool = compute_pool(&req)?;
    if pool.total_dice <= 0 {
        return Err(bad_request(format!(
            "Nothing to roll: the pool is {} dice",
            pool.total_dice
        )));
    }
    let count = pool.total_dice as u32;
    if count > MAX_DICE_PER_ROLL {
        return Err(bad_request(format!(
            "Too many dice to roll at once (limit {})",
            MAX_DICE_PER_ROLL
        )));
    }

    // The random.org request waits on the network, so keep it off the async threads
    let dice = tokio::task::spawn_blocking(move || roll_dice(count, DIE_SIDES))
        .await
        .map_err(|e| server_error(e.to_string()))?;

    let base_dice_after = if req.desperate {
        req.base_dice.saturating_sub(DESPERATE_BASE_COST)
    } else {
        req.base_dice
    };
    Ok(Json(RollResponse { dice, base_dice_after }))
}

// =====================================================================
// 6. SERVER
// =====================================================================

async fn index() -> Html<&'static str> {
    // include_str! bakes the HTML file into the program at compile time
    Html(include_str!("../static/index.html"))
}

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(index))
        .route("/api/rules", get(get_rules))
        .route("/api/pool", post(build_pool))
        .route("/api/roll", post(roll));

    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string());

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port))
        .await
        .expect("could not bind to port");

    println!("Spellcasting helper running on port {}", port);

    axum::serve(listener, app).await.unwrap();
}
