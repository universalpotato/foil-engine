//! Compile-time audits of the compiled IR: shapes that would quietly match nothing at run time
//! refuse the card instead.
//!
//! The audits walk the serialised IR (`serde_json::Value`), so every filter, `Defined`, condition
//! and expression of every effect variant is seen without a hand-written visitor that new effect
//! variants could bypass.
//!
//! - **Memory reads without memory** (~260 Forge chains). Forge's
//!   `Remembered` / `IsRemembered` is the host card's remembered list; the engine has only the
//!   memory of the current resolution (`Exec::remembered`), filled by the nodes before it in the
//!   same chain (`RememberX$`, `RememberObjects$`, `RepeatEach`, a delayed trigger's objects). A
//!   node that reads it with nothing before it in its chain remembering reads Forge's card-level
//!   memory (set by another ability of the card): it would read an empty set. Likewise trigger
//!   conditions, statics of permanents and replacements are evaluated without any memory.
//! - **Player alternatives in object filters**. Only a target's filter is matched
//!   against players (`cast::legal_targets_in`, `query::player_matches`); everywhere else
//!   `query::obj_matches` never matches a `Player` / `Opponent` / `You` alternative, so such an
//!   alternative ("Player,Planeswalker") silently matches nothing.
use crate::carddb::{AbilityDef, CardDb, ReplacementDef, StaticDef, TriggerDef};
use serde::Serialize;
use serde_json::Value;

/// IR names that read per-resolution memory: `Prop::IsRemembered`,
/// `ExtProp::SameNameAsRemembered`, `Defined::Remembered` / `RememberedController`,
/// `ExtExpr::RememberedSize` / `RememberedValid`.
const MEMORY_READERS: &[&str] = &["IsRemembered", "SameNameAsRemembered", "Remembered", "RememberedController", "RememberedSize", "RememberedValid"];

fn ir<T: Serialize + ?Sized>(x: &T) -> Result<Value, String> {
    serde_json::to_value(x).map_err(|e| format!("compile audit: IR not serialisable ({e})"))
}

fn mentions_memory(v: &Value) -> bool {
    match v {
        Value::String(s) => MEMORY_READERS.contains(&s.as_str()),
        Value::Array(a) => a.iter().any(mentions_memory),
        Value::Object(m) => m.iter().any(|(k, x)| MEMORY_READERS.contains(&k.as_str()) || mentions_memory(x)),
        _ => false,
    }
}

/// Whether a Forge node writes per-resolution memory for the nodes after it (and for the bodies
/// it creates: delayed / reflexive triggers, effect objects): any `Remember…$` key that isn't
/// `False` (`RememberChanged$`, `RememberObjects$`, `RememberTargets$`, ...; a key the compiler
/// doesn't honour makes the card unsupported on its own), or `RepeatEach` (each repetition's
/// player or card is "Remembered", Forge `RepeatEachEffect`).
pub(crate) fn writes_memory(p: &forge_script::Params) -> bool {
    p.pairs.iter().any(|(k, v)| k.starts_with("Remember") && !v.eq_ignore_ascii_case("false")) || p.pairs.iter().any(|(k, v)| matches!(k.as_str(), "DB" | "AB" | "SP") && v == "RepeatEach")
}

/// The parts of an ability node evaluated by its own resolution (its sub-ability is a node of
/// its own; its target's filter is checked by `node_player_alts`' caller rules).
#[derive(Serialize)]
struct NodeParts<'a> {
    effect: &'a crate::carddb::Effect,
    defined: &'a Option<crate::carddb::Defined>,
    target: Option<TargetParts<'a>>,
    cost: &'a Option<crate::carddb::Cost>,
    activate_if: &'a Vec<crate::expr::Condition>,
    conditions: &'a Vec<crate::expr::Condition>,
    unless: &'a Option<(crate::carddb::UnlessCost, crate::carddb::Defined)>,
}
#[derive(Serialize)]
struct TargetParts<'a> {
    filter: Option<&'a crate::filter::Filter>,
    kind: &'a crate::carddb::TargetKind,
    max_expr: &'a Option<crate::expr::Expr>,
}
fn node_parts(a: &AbilityDef, with_target_filter: bool) -> NodeParts<'_> {
    NodeParts {
        effect: &a.effect,
        defined: &a.defined,
        target: a.target.as_ref().map(|t| TargetParts { filter: with_target_filter.then_some(&t.filter), kind: &t.kind, max_expr: &t.max_expr }),
        cost: &a.cost,
        activate_if: &a.activate_if,
        conditions: &a.conditions,
        unless: &a.unless,
    }
}

/// Whether an ability node reads per-resolution memory (its own parts, not its sub-ability).
pub(crate) fn node_reads_memory(a: &AbilityDef) -> Result<bool, String> {
    Ok(mentions_memory(&ir(&node_parts(a, true))?))
}

/// Whether a trigger's event filter or intervening "if" reads memory (evaluated without any:
/// `triggers::matches`, `trig_ext::matches`, `triggers::fire`'s `EvalCtx`).
pub(crate) fn trigger_reads_memory(t: &TriggerDef) -> Result<bool, String> {
    Ok(mentions_memory(&ir(&t.mode)?) || mentions_memory(&ir(&t.intervening)?))
}

/// Whether a permanent's static reads memory (permanents' statics are evaluated without any;
/// effect objects' statics have the effect's remembered objects and are not checked here).
pub(crate) fn static_reads_memory(s: &StaticDef) -> Result<bool, String> {
    Ok(mentions_memory(&ir(&s.kind)?) || mentions_memory(&ir(&s.conditions)?))
}

/// Whether a replacement's event filter reads memory (`zones` evaluates it without any).
pub(crate) fn replacement_reads_memory(r: &ReplacementDef) -> Result<bool, String> {
    Ok(mentions_memory(&ir(&r.valid)?))
}

/// The Forge spelling of the player alternatives of every filter in `v` (a filter is an object
/// with exactly the field `alts`).
fn player_alts(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Array(a) => a.iter().for_each(|x| player_alts(x, out)),
        Value::Object(m) => {
            if m.len() == 1 {
                if let Some(Value::Array(alts)) = m.get("alts") {
                    for a in alts {
                        if let Some(Value::String(b)) = a.get("base") {
                            if matches!(b.as_str(), "Player" | "Opponent" | "You") {
                                out.push(b.clone());
                            }
                        }
                    }
                }
            }
            m.values().for_each(|x| player_alts(x, out));
        }
        _ => {}
    }
}

/// Player alternatives in object-only filters of everything compiled for one face: abilities
/// `abilities` (their targets' filters excluded: `legal_targets_in` matches players there),
/// the face's statics, triggers and replacements, and the effect-object statics and triggers in
/// `statics_pool[pool.0..]` / `triggers_pool[pool.1..]`.
pub(crate) fn face_player_alts(db: &CardDb, abilities: std::ops::Range<usize>, triggers: &[TriggerDef], statics: &[StaticDef], replacements: &[ReplacementDef], pool: (usize, usize)) -> Result<Vec<String>, String> {
    let mut out = vec![];
    let mut push = |what: String, v: Value| {
        let mut alts = vec![];
        player_alts(&v, &mut alts);
        if !alts.is_empty() {
            alts.dedup();
            out.push(format!("{what}: players ({}) in an object filter", alts.join(",")));
        }
    };
    for a in &db.abilities[abilities] {
        push(format!("ability {:?}", short(&a.text)), ir(&node_parts(a, false))?);
    }
    for t in triggers.iter().chain(&db.triggers_pool[pool.1..]) {
        push(format!("trigger {:?}", short(&t.text)), ir(&(&t.mode, &t.intervening))?);
    }
    for s in statics.iter().chain(&db.statics_pool[pool.0..]) {
        push(format!("static {:?}", short(&s.text)), ir(&(&s.kind, &s.conditions))?);
    }
    for r in replacements {
        push(format!("replacement {:?}", short(&r.text)), ir(&r.valid)?);
    }
    Ok(out)
}

fn short(t: &str) -> String {
    t.chars().take(60).collect()
}

/// Keywords that let a spell be cast in another way or for an alternative cost (CR 113.2c: an
/// object may have several instances, each its own choice; 118.9; 702.x). Granting one needs a
/// casting instance for the granted copy (as `Method::FlashbackGranted` for flashback granted to
/// a card with its own flashback). No grant path supports any (flashback only as
/// `Pump | KW$ Flashback | PumpZone$ Graveyard`, compiled to `GrantFlashback`, not through a
/// keyword set), so a grant of one is refused by name.
pub(crate) const ALT_COST_KEYWORDS: &[&str] = &[
    "Flashback", "Escape", "Warp", "Plot", "Harmonize", "Mayhem", "Sneak", "Web-slinging", "Evoke", "Impending", "Disturb", "Jump-start", "Retrace", "Madness", "Dash", "Blitz", "Overload",
    "Spectacle", "Surge", "Emerge", "Bestow", "Foretell", "Prowl", "Freerunning", "Miracle", "Aftermath", "Suspend", "Morph", "Megamorph", "Disguise", "Awaken",
];

/// A keyword granted by an effect or static (`KW$`, `AddKeyword$`, `KWChoice$`,
/// `PumpKeywords$`); `what` names the parameter in the error. Alternative-cost keywords are
/// refused as such ([`ALT_COST_KEYWORDS`]), other unknown keywords as unknown.
pub(crate) fn granted_kw(w: &str, what: &str) -> Result<crate::carddb::Kw, String> {
    let w = w.trim();
    let head = w.split([':', ' ']).next().unwrap_or(w);
    if ALT_COST_KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(head)) {
        return Err(format!("{what} {w}: granted alternative-cost keyword without a casting instance (CR 113.2c, 118.9)"));
    }
    crate::carddb::Kw::parse(w).ok_or_else(|| format!("{what} {w}"))
}
