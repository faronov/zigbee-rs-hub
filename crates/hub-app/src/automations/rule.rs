//! Automation rules: the typed model the evaluator runs, parsed and
//! validated from the API's JSON (no serde derives: they cost kilobytes of
//! firmware each). The rule's JSON itself is kept as compact text: it is
//! what the API returns and what the store holds (`auto/<id>`, with the
//! format version `v`).

use hub_devices::hub::ApiError;
use serde_json::{Map, Value as J};

use super::cron::Cron;

/// Rules per hub.
pub const MAX_RULES: usize = 64;
/// Triggers per rule.
pub const MAX_TRIGGERS: usize = 8;
/// Condition nodes per rule (nested ones included).
pub const MAX_CONDITIONS: usize = 16;
/// Nesting of `and`/`or`/`not`.
pub const MAX_DEPTH: usize = 3;
/// Actions per rule.
pub const MAX_ACTIONS: usize = 16;
/// Longest delay, s.
pub const MAX_DELAY_S: f64 = 86_400.0;
/// A rule's JSON text.
pub const MAX_RULE_BYTES: usize = 4096;
/// All rules' JSON together (RAM of a hub without PSRAM).
pub const MAX_TOTAL_BYTES: usize = 16 * 1024;
/// Default of `max_per_min`.
pub const DEFAULT_PER_MIN: u32 = 20;
/// Storage format version (`v`).
pub const FORMAT: u64 = 1;

/// API requests an action may send (`request`): device and group control,
/// never maintenance or the automations themselves.
pub const REQUEST_PREFIXES: &[&str] =
    &["entities/command", "entities/refresh", "groups/", "scenes/", "network/permit_join", "gp/commissioning/"];

/// A comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `equals`.
    Equals,
    /// `not_equals`.
    NotEquals,
    /// `above` (numbers).
    Above,
    /// `below` (numbers).
    Below,
    /// `changed` (triggers only).
    Changed,
}

/// What a state trigger or condition looks at.
#[derive(Debug, Clone, PartialEq)]
pub struct StateRef {
    /// Entity.
    pub uid: String,
    /// An attribute of the state message instead of `state`.
    pub attribute: Option<String>,
    /// Comparison.
    pub op: Op,
    /// Operand (`null` for `changed`).
    pub value: J,
}

/// A trigger.
#[derive(Debug, Clone, PartialEq)]
pub enum Trigger {
    /// An entity's state (`entity_state`).
    State(StateRef),
    /// An `entity_event` (buttons, remotes, GP commands).
    Event {
        /// Entity.
        uid: String,
        /// Only this event type.
        event_type: Option<String>,
    },
    /// A device's availability changed.
    Availability {
        /// Device.
        device: String,
        /// Only to this state (`true` = available).
        to: Option<bool>,
    },
    /// A wall-clock schedule.
    Time(Cron),
    /// The hub started (once its NCP session is up).
    Start,
}

/// A condition.
#[derive(Debug, Clone, PartialEq)]
pub enum Cond {
    /// An entity's current state.
    State(StateRef),
    /// Local time window `[after, before)` in minutes of the day; wraps
    /// midnight when `after > before`.
    Time {
        /// From.
        after: Option<u16>,
        /// Until.
        before: Option<u16>,
    },
    /// ISO weekdays, bit 0 = Monday … bit 6 = Sunday.
    Weekday(u8),
    /// All hold.
    And(Vec<Cond>),
    /// Any holds.
    Or(Vec<Cond>),
    /// None holds.
    Not(Vec<Cond>),
}

/// An action.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// `entities/command`.
    Command {
        /// Entity.
        uid: String,
        /// HA service name.
        command: String,
        /// Parameters, as JSON text (a `Value` object costs a B-tree node).
        params: String,
    },
    /// Any allowed API request ([`REQUEST_PREFIXES`]): group commands and
    /// scene recalls; JSON text.
    Request(String),
    /// Wait, ms.
    Delay(u64),
    /// An `automation_notify` event; `webhook` is a placeholder.
    Notify {
        /// Text.
        message: String,
        /// Extra data, JSON text.
        data: String,
        /// Not sent yet.
        webhook: Option<String>,
    },
    /// Enable or disable a rule.
    Enable {
        /// The rule.
        rule: String,
        /// Enable (`true`) or disable.
        enabled: bool,
    },
}

/// What a trigger does while a run of the rule is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The new trigger is dropped.
    Single,
    /// The active run is cancelled, a new one starts.
    Restart,
}

/// A validated rule.
#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    /// ID (`[a-z0-9_-]`, 1–32).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Enabled.
    pub enabled: bool,
    /// Mode.
    pub mode: Mode,
    /// Runs per minute at most.
    pub max_per_min: u32,
    /// Triggers (any fires).
    pub triggers: Vec<Trigger>,
    /// The rule's JSON (compact, with `v`); conditions and actions are
    /// parsed from it when the rule fires ([`Rule::body`]), so a rule is
    /// kept about once in RAM.
    pub src: String,
}

/// What a rule does once triggered.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Body {
    /// Conditions (all must hold).
    pub conditions: Vec<Cond>,
    /// Actions, in order.
    pub actions: Vec<Action>,
}

fn bad(path: &str, msg: impl std::fmt::Display) -> ApiError {
    ApiError::new("invalid_value", format!("{path}: {msg}"))
}

fn obj<'a>(v: &'a J, path: &str) -> Result<&'a Map<String, J>, ApiError> {
    v.as_object().ok_or_else(|| bad(path, "must be an object"))
}

fn text(o: &Map<String, J>, key: &str, path: &str) -> Result<String, ApiError> {
    match o.get(key) {
        Some(J::String(s)) if !s.is_empty() && s.len() <= 128 => Ok(s.clone()),
        _ => Err(bad(path, format!("`{key}` must be a non-empty string"))),
    }
}

fn opt_text(o: &Map<String, J>, key: &str, path: &str) -> Result<Option<String>, ApiError> {
    match o.get(key) {
        None | Some(J::Null) => Ok(None),
        Some(_) => text(o, key, path).map(Some),
    }
}

fn list<'a>(o: &'a Map<String, J>, key: &str, path: &str, max: usize) -> Result<&'a [J], ApiError> {
    match o.get(key) {
        None | Some(J::Null) => Ok(&[]),
        Some(J::Array(a)) if a.len() <= max => Ok(a),
        Some(J::Array(_)) => Err(bad(path, format!("at most {max} {key}"))),
        Some(_) => Err(bad(path, format!("`{key}` must be a list"))),
    }
}

/// `HH:MM` → minutes of the day.
pub fn hhmm(s: &str) -> Option<u16> {
    let (h, m) = s.split_once(':')?;
    let (h, m) = (h.parse::<u16>().ok()?, m.parse::<u16>().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

fn state_ref(o: &Map<String, J>, path: &str, trigger: bool) -> Result<StateRef, ApiError> {
    let op = match o.get("op").and_then(J::as_str) {
        Some("equals") => Op::Equals,
        Some("not_equals") => Op::NotEquals,
        Some("above") => Op::Above,
        Some("below") => Op::Below,
        Some("changed") if trigger => Op::Changed,
        _ => {
            return Err(bad(
                path,
                if trigger {
                    "`op` must be equals, not_equals, above, below or changed"
                } else {
                    "`op` must be equals, not_equals, above or below"
                },
            ));
        }
    };
    let value = o.get("value").cloned().unwrap_or(J::Null);
    if matches!(op, Op::Above | Op::Below) && !value.is_number() {
        return Err(bad(path, "`value` must be a number for above/below"));
    }
    if matches!(op, Op::Equals | Op::NotEquals) && (value.is_object() || value.is_array()) {
        return Err(bad(path, "`value` must be a string, number, bool or null"));
    }
    Ok(StateRef {
        uid: text(o, "uid", path)?,
        attribute: opt_text(o, "attribute", path)?,
        op,
        value: if op == Op::Changed { J::Null } else { value },
    })
}

fn trigger(v: &J, path: &str) -> Result<Trigger, ApiError> {
    let o = obj(v, path)?;
    Ok(match o.get("type").and_then(J::as_str) {
        Some("state") => Trigger::State(state_ref(o, path, true)?),
        Some("event") => Trigger::Event { uid: text(o, "uid", path)?, event_type: opt_text(o, "event_type", path)? },
        Some("availability") => Trigger::Availability {
            device: text(o, "device", path)?,
            to: match o.get("to").and_then(J::as_str) {
                None => None,
                Some("available") => Some(true),
                Some("unavailable") => Some(false),
                Some(_) => return Err(bad(path, "`to` must be available or unavailable")),
            },
        },
        Some("time") => Trigger::Time(Cron::parse(&text(o, "cron", path)?).map_err(|e| bad(path, e))?),
        Some("start") => Trigger::Start,
        _ => return Err(bad(path, "`type` must be state, event, availability, time or start")),
    })
}

fn cond(v: &J, path: &str, depth: usize, nodes: &mut usize) -> Result<Cond, ApiError> {
    *nodes += 1;
    if *nodes > MAX_CONDITIONS {
        return Err(bad(path, format!("at most {MAX_CONDITIONS} conditions in a rule")));
    }
    let o = obj(v, path)?;
    let ty = o.get("type").and_then(J::as_str);
    if let Some(t @ ("and" | "or" | "not")) = ty {
        if depth >= MAX_DEPTH {
            return Err(bad(path, format!("conditions nest at most {MAX_DEPTH} deep")));
        }
        let items = list(o, "conditions", path, MAX_CONDITIONS)?;
        let mut subs = Vec::with_capacity(items.len());
        for (i, c) in items.iter().enumerate() {
            subs.push(cond(c, &format!("{path}.conditions[{i}]"), depth + 1, nodes)?);
        }
        if subs.is_empty() {
            return Err(bad(path, "`conditions` must not be empty"));
        }
        return Ok(match t {
            "and" => Cond::And(subs),
            "or" => Cond::Or(subs),
            _ => Cond::Not(subs),
        });
    }
    Ok(match ty {
        Some("state") => Cond::State(state_ref(o, path, false)?),
        Some("time") => {
            let at = |k: &str| -> Result<Option<u16>, ApiError> {
                match o.get(k) {
                    None | Some(J::Null) => Ok(None),
                    Some(J::String(s)) => hhmm(s).map(Some).ok_or_else(|| bad(path, format!("`{k}` must be HH:MM"))),
                    Some(_) => Err(bad(path, format!("`{k}` must be HH:MM"))),
                }
            };
            let (after, before) = (at("after")?, at("before")?);
            if after.is_none() && before.is_none() {
                return Err(bad(path, "`after` or `before` is needed"));
            }
            Cond::Time { after, before }
        }
        Some("weekday") => {
            let mut days = 0u8;
            for d in list(o, "days", path, 7)? {
                let i = match d {
                    J::String(s) => WEEKDAYS.iter().position(|w| w.eq_ignore_ascii_case(s)),
                    J::Number(n) => n.as_u64().filter(|n| (1..=7).contains(n)).map(|n| n as usize - 1),
                    _ => None,
                }
                .ok_or_else(|| bad(path, "`days` are mon…sun or 1–7"))?;
                days |= 1 << i;
            }
            if days == 0 {
                return Err(bad(path, "`days` must not be empty"));
            }
            Cond::Weekday(days)
        }
        _ => return Err(bad(path, "`type` must be state, time, weekday, and, or or not")),
    })
}

fn action(v: &J, path: &str) -> Result<Action, ApiError> {
    let o = obj(v, path)?;
    Ok(match o.get("type").and_then(J::as_str) {
        Some("command") => Action::Command {
            uid: text(o, "uid", path)?,
            command: text(o, "command", path)?,
            params: match o.get("params") {
                None | Some(J::Null) => "{}".into(),
                Some(p @ J::Object(_)) => p.to_string(),
                Some(_) => return Err(bad(path, "`params` must be an object")),
            },
        },
        Some("request") => {
            let r = o.get("request").filter(|r| r.is_object()).ok_or_else(|| bad(path, "`request` must be an object"))?;
            let ty = r.get("type").and_then(J::as_str).unwrap_or("");
            if !REQUEST_PREFIXES.iter().any(|p| ty.starts_with(p)) {
                return Err(bad(path, format!("request type {ty:?} is not allowed (only {})", REQUEST_PREFIXES.join(", "))));
            }
            let mut r = r.clone();
            if let Some(m) = r.as_object_mut() {
                m.remove("id");
            }
            Action::Request(r.to_string())
        }
        Some("delay") => {
            let s = o.get("s").and_then(J::as_f64).filter(|s| *s >= 0.0 && *s <= MAX_DELAY_S);
            Action::Delay((s.ok_or_else(|| bad(path, format!("`s` must be 0–{MAX_DELAY_S} seconds")))? * 1000.0) as u64)
        }
        Some("notify") => Action::Notify {
            message: o.get("message").and_then(J::as_str).unwrap_or("").chars().take(256).collect(),
            data: o.get("data").map_or_else(|| "null".into(), J::to_string),
            webhook: opt_text(o, "webhook", path)?,
        },
        Some(t @ ("enable" | "disable")) => Action::Enable { rule: text(o, "automation", path)?, enabled: t == "enable" },
        _ => return Err(bad(path, "`type` must be command, request, delay, notify, enable or disable")),
    })
}

/// A valid rule ID.
pub fn valid_id(id: &str) -> bool {
    (1..=32).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Parses and validates a rule; `id` is the rule's ID (the caller picks
/// one for a new rule without). Unknown top-level fields are dropped.
pub fn parse(v: &J, id: &str) -> Result<Rule, ApiError> {
    parse_full(v, id).map(|(r, _)| r)
}

fn parse_full(v: &J, id: &str) -> Result<(Rule, Body), ApiError> {
    let o = obj(v, "rule")?;
    if !valid_id(id) {
        return Err(bad("rule.id", "1–32 characters a-z, 0-9, _ or -"));
    }
    if let Some(f) = o.get("v").and_then(J::as_u64).filter(|f| *f > FORMAT) {
        return Err(ApiError::new("unsupported", format!("rule format {f} is newer than {FORMAT}")));
    }
    let name = match o.get("name") {
        None | Some(J::Null) => id.to_string(),
        Some(J::String(s)) if s.len() <= 64 => s.clone(),
        Some(_) => return Err(bad("rule.name", "a string of at most 64 bytes")),
    };
    let enabled = match o.get("enabled") {
        None => true,
        Some(J::Bool(b)) => *b,
        Some(_) => return Err(bad("rule.enabled", "must be a bool")),
    };
    let mode = match o.get("mode").and_then(J::as_str) {
        None | Some("single") => Mode::Single,
        Some("restart") => Mode::Restart,
        Some(_) => return Err(bad("rule.mode", "single or restart")),
    };
    let max_per_min = match o.get("max_per_min") {
        None => DEFAULT_PER_MIN,
        Some(n) => n.as_u64().filter(|n| (1..=60).contains(n)).ok_or_else(|| bad("rule.max_per_min", "1–60"))? as u32,
    };
    // Exact capacities: a `Vec` grows to 4 elements at least.
    let items = list(o, "triggers", "rule", MAX_TRIGGERS)?;
    let mut triggers = Vec::with_capacity(items.len());
    for (i, t) in items.iter().enumerate() {
        triggers.push(trigger(t, &format!("rule.triggers[{i}]"))?);
    }
    let mut nodes = 0;
    let items = list(o, "conditions", "rule", MAX_CONDITIONS)?;
    let mut conditions = Vec::with_capacity(items.len());
    for (i, c) in items.iter().enumerate() {
        conditions.push(cond(c, &format!("rule.conditions[{i}]"), 0, &mut nodes)?);
    }
    let items = list(o, "actions", "rule", MAX_ACTIONS)?;
    let mut actions = Vec::with_capacity(items.len());
    for (i, a) in items.iter().enumerate() {
        actions.push(action(a, &format!("rule.actions[{i}]"))?);
    }
    if actions.is_empty() {
        return Err(bad("rule", "at least one action is needed"));
    }
    // The canonical JSON: known fields only, in a fixed order.
    let mut c = Map::new();
    c.insert("v".into(), FORMAT.into());
    c.insert("id".into(), id.into());
    c.insert("name".into(), name.clone().into());
    c.insert("enabled".into(), enabled.into());
    c.insert("mode".into(), if mode == Mode::Restart { "restart" } else { "single" }.into());
    c.insert("max_per_min".into(), max_per_min.into());
    for k in ["triggers", "conditions", "actions"] {
        c.insert(k.into(), o.get(k).cloned().unwrap_or_else(|| J::Array(Vec::new())));
    }
    let mut src = J::Object(c).to_string();
    src.shrink_to_fit();
    if src.len() > MAX_RULE_BYTES {
        return Err(ApiError::new("no_space", format!("rule is {} B, at most {MAX_RULE_BYTES}", src.len())));
    }
    Ok((Rule { id: id.into(), name, enabled, mode, max_per_min, triggers, src }, Body { conditions, actions }))
}

impl Rule {
    /// The rule's JSON.
    pub fn json(&self) -> J {
        serde_json::from_str(&self.src).unwrap_or(J::Null)
    }

    /// Sets `enabled`, in the JSON too.
    pub fn set_enabled(&mut self, on: bool) {
        self.enabled = on;
        let mut j = self.json();
        j["enabled"] = on.into();
        self.src = j.to_string();
        self.src.shrink_to_fit();
    }

    /// Conditions and actions (empty if the JSON no longer parses, which a
    /// validated rule does).
    pub fn body(&self) -> Body {
        parse_full(&self.json(), &self.id).map(|(_, b)| b).unwrap_or_default()
    }

    /// Whether a trigger depends on the wall clock.
    pub fn scheduled(&self) -> bool {
        self.triggers.iter().any(|t| matches!(t, Trigger::Time(_)))
    }
}
