//! Scenario positions and canonical state/options for differential testing (formats
//! `scenario/v0` and `state/v0`).
//!
//! Testing only: reached from a test harness's `scenario` / `state` / `options` /
//! `submit_canon` commands, never from normal games or agents. Normal games never
//! touch this module (their `State::lineage` stays `None`), so they are byte-identical.
//!
//! Scope (v2): any step at priority (not untap/cleanup, which have none) or at its start
//! (`phase_point: "start"`, before its turn-based actions, CR 703.4), mulligans before the game
//! (`"mulligan"`), combat positions (attackers, planeswalkers attacked, blockers, "blocked",
//! first-strike damage already dealt), a preset stack (`scenario_stack`), creatures that died
//! this turn (`died_this_turn`, as last known information), manifest / manifest dread face-down
//! permanents and tokens with keyword text. Anything else the format can express is refused
//! with a precise reason: never approximated (unimplemented behaviour fails loudly).
//!
//! Construction: objects are created the way `builder::GameBuilder` does it (no events, no
//! triggers: the position is given, not played), with every derived field set consistently:
//! timestamps in battlefield order (CR 613.7), summoning sickness from `summoning_sick` (CR
//! 302.6: `ctrl_since` vs the controller's most recent turn), printed loyalty (CR 306.5b),
//! attachments, counters, damage, transformed faces (as the engine's own transform does),
//! manifests (as `ext6::manifest` does), "exiled with" links (CR 607, 610.3) with their return
//! zone taken from the source card's own exile ability, chosen types/names, library order, mana
//! pool, per-turn counters. After building: zone lists must partition the objects and no
//! state-based action may be pending (CR 704.3), else the load fails.
//!
//! Keys: a scenario `key` names a card and follows it across zone changes (new objects, CR
//! 400.7) through `State::lineage` (enabled only here). Objects created during the game are
//! `new:<Name>:<n>` (n-th created object with that name, creation order).

use crate::basics::*;
use crate::carddb::{AbilityId, CardDb, Kw, KwSet, Layout};
use crate::engine::{Ctx, Decision, DecisionKind, Game, Halt, Opt};
use crate::state::*;
use serde_json::{json, Map, Value};
use smallvec::SmallVec;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// Scenario keys of the initial objects (ids `0..n_init`).
#[derive(Clone, Debug, Default)]
pub struct Keys {
    pub n_init: u32,
    pub keys: Vec<Option<String>>,
    /// `"auto": {"mana": true}` (the default): every `PayMana`
    /// decision is answered with the first source in the engine's own canonical order (every
    /// offered source leaves the rest of the cost payable), so payments never surface.
    pub auto_mana: bool,
}

pub struct Loaded {
    pub game: Game,
    pub keys: Keys,
}

const PREDEFINED_TOKENS: [(&str, &str); 5] = [("Treasure", "c_a_treasure_sac"), ("Food", "c_a_food_sac"), ("Clue", "c_a_clue_draw"), ("Map", "c_a_map_sac_explore"), ("Blood", "c_a_blood_draw")];

/// CR 122.1b keyword counters: the engine has no keyword counters, refused.
const KEYWORD_COUNTERS: [&str; 16] = ["flying", "first strike", "double strike", "deathtouch", "decayed", "exalted", "haste", "hexproof", "indestructible", "lifelink", "menace", "reach", "shadow", "trample", "vigilance", "first-strike"];

/// A game number from the scenario, refused past the engine's bound (`num::NUM_MAX`) instead of
/// wrapping into `i32`.
fn game_num(v: i64, what: &str) -> Result<i32, String> {
    if v.abs() > crate::num::NUM_MAX as i64 {
        return err(format!("{what}: {v} is beyond the engine's game-number bound ±{}", crate::num::NUM_MAX));
    }
    Ok(v as i32)
}

fn err<T>(m: impl Into<String>) -> Result<T, String> {
    Err(m.into())
}

fn check_fields(v: &Value, allowed: &[&str], what: &str) -> Result<(), String> {
    let Some(m) = v.as_object() else { return err(format!("{what}: expected an object")) };
    for k in m.keys() {
        if !allowed.contains(&k.as_str()) {
            return err(format!("{what}: field {k:?} is not supported by the engine's scenario loader (v1)"));
        }
    }
    Ok(())
}

/// Validate a `rules_options` object (scenario/v0; a harness's new game) and return its
/// `mana_abilities_at_priority` (None: absent or null). Unknown keys and non-boolean values are
/// refused (never ignored). Applied with `cast::set_mana_abilities_at_priority`.
pub fn rules_options(ro: &Value) -> Result<Option<bool>, String> {
    let Some(m) = ro.as_object() else { return err("rules_options: expected an object") };
    if let Some(k) = m.keys().find(|k| k.as_str() != "mana_abilities_at_priority") {
        return err(format!("rules_options: field {k:?} is not supported by the engine (known: \"mana_abilities_at_priority\")"));
    }
    match &ro["mana_abilities_at_priority"] {
        Value::Null => Ok(None),
        Value::Bool(b) => Ok(Some(*b)),
        _ => err("rules_options.mana_abilities_at_priority: expected a boolean"),
    }
}

fn player_of(v: &Value, what: &str) -> Result<PlayerId, String> {
    match v.as_u64() {
        Some(p @ 0..=1) => Ok(PlayerId(p as u8)),
        _ => err(format!("{what}: expected player 0 or 1, got {v}")),
    }
}

fn step_of(s: &str) -> Result<Step, String> {
    use Step::*;
    Ok(match s {
        "Untap" => Untap,
        "Upkeep" => Upkeep,
        "Draw" => Draw,
        "Main1" => Main1,
        "BeginCombat" => BeginCombat,
        "DeclareAttackers" => DeclareAttackers,
        "DeclareBlockers" => DeclareBlockers,
        "FirstStrikeDamage" => FirstStrikeDamage,
        "CombatDamage" => CombatDamage,
        "EndCombat" => EndCombat,
        "Main2" => Main2,
        "End" => End,
        "Cleanup" => Cleanup,
        _ => return err(format!("unknown step {s:?}")),
    })
}

pub fn step_name(s: Step) -> &'static str {
    use Step::*;
    match s {
        Untap => "Untap",
        Upkeep => "Upkeep",
        Draw => "Draw",
        Main1 => "Main1",
        BeginCombat => "BeginCombat",
        DeclareAttackers => "DeclareAttackers",
        DeclareBlockers => "DeclareBlockers",
        FirstStrikeDamage => "FirstStrikeDamage",
        CombatDamage => "CombatDamage",
        EndCombat => "EndCombat",
        Main2 => "Main2",
        End => "End",
        Cleanup => "Cleanup",
    }
}

/// A scenario's `phase_point`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum PhasePoint {
    /// The step's turn-based actions happened; `priority` has priority with the given stack.
    Priority,
    /// The beginning of `step`, before its turn-based actions (CR 703.4).
    Start,
    /// Before the game: opening hands are drawn, then mulligans (CR 103.5).
    Mulligan,
}

/// Which (step, phase point) positions exist and can be loaded.
fn check_position(pp: PhasePoint, step: Step, turn: u32, stack: bool) -> Result<(), String> {
    match pp {
        PhasePoint::Mulligan => {
            if turn != 1 || step != Step::Untap {
                return err("phase_point \"mulligan\" is before the game: it needs \"turn\": 1 and \"step\": \"Untap\"");
            }
            if stack {
                return err("phase_point \"mulligan\": the stack is empty before the game");
            }
        }
        PhasePoint::Start => {
            // CR 500.2: a step ends only once the stack is empty, so the next one begins with an
            // empty stack.
            if stack {
                return err("phase_point \"start\" with a non-empty stack: a step begins with an empty stack (CR 500.2)");
            }
        }
        PhasePoint::Priority => match step {
            Step::Untap => return err("no player gets priority in the untap step (CR 502.4); use phase_point \"start\""),
            // CR 514.3: normally no player gets priority in the cleanup step; the exception
            // (514.3a, something happened during it) is not a position the format describes.
            Step::Cleanup => return err("no player gets priority in the cleanup step unless something happened during it (CR 514.3, 514.3a); use phase_point \"start\""),
            _ => {}
        },
    }
    Ok(())
}

/// One creature of `died_this_turn` as it last existed on the battlefield (CR 608.2h).
struct Died {
    name: String,
    /// The engine card standing for it (resolved with the card DB).
    card: Option<CardId>,
    owner: PlayerId,
    controller: PlayerId,
    token: bool,
    def: Value,
}

/// `died_this_turn` (v2) / `creatures_died_this_turn`: the list of last-known creatures that
/// died this turn. A bare count can't be loaded: the engine answers "creatures that died this
/// turn" conditions from the dead objects' last known characteristics.
fn died_this_turn(sc: &Value) -> Result<Vec<Died>, String> {
    let count = match &sc["creatures_died_this_turn"] {
        Value::Null => None,
        v => Some(v.as_u64().ok_or("creatures_died_this_turn: expected a count")?),
    };
    let list: Vec<&Value> = match &sc["died_this_turn"] {
        Value::Null => vec![],
        Value::Array(a) => a.iter().collect(),
        _ => return err("died_this_turn: expected a list"),
    };
    if let Some(n) = count {
        if n as usize != list.len() {
            if list.is_empty() {
                return err("creatures_died_this_turn without died_this_turn is not supported: the engine answers \"creatures that died this turn\" from the dead creatures' last known characteristics (CR 608.2h), which a bare count doesn't give; list them in died_this_turn");
            }
            return err(format!("creatures_died_this_turn = {n} but died_this_turn lists {} creatures", list.len()));
        }
    }
    let mut out = vec![];
    for (i, d) in list.into_iter().enumerate() {
        let what = format!("died_this_turn[{i}]");
        check_fields(d, &["name", "owner", "controller", "types", "subtypes", "colors", "power", "toughness", "token", "supertypes"], &what)?;
        let name = d["name"].as_str().ok_or_else(|| format!("{what}: missing \"name\""))?.to_string();
        let controller = player_of(&d["controller"], &format!("{what}.controller"))?;
        let owner = if d["owner"].is_null() { controller } else { player_of(&d["owner"], &format!("{what}.owner"))? };
        if !d["types"].as_array().is_some_and(|t| t.iter().any(|x| x == "Creature")) {
            return err(format!("{what}: \"types\" must include Creature (it died as a creature)"));
        }
        out.push(Died { name, card: None, owner, controller, token: d["token"].as_bool().unwrap_or(false), def: d.clone() });
    }
    Ok(out)
}

/// Record the creatures of `died_this_turn` as the engine records a creature dying: an old
/// object (`Zone::Gone`, CR 400.7) with its last known characteristics (`lki`, CR 608.2h) and a
/// battlefield → graveyard entry in this turn's zone-change history (`moved_this_turn`).
fn load_died(s: &mut State, db: &CardDb, died: &[Died]) -> Result<(), String> {
    for (i, d) in died.iter().enumerate() {
        let what = format!("died_this_turn[{i}]");
        let card = d.card.ok_or_else(|| format!("{what}: card not resolved"))?;
        let id = new_obj(s, card, d.owner, d.controller, Zone::Battlefield);
        s.obj_mut(id).token = d.token;
        let mut c = crate::query::chars(s, db, id);
        let strs = |k: &str| -> Vec<String> { d.def[k].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect() };
        let mut types = TypeSet::default();
        for t in strs("types") {
            let ty = all_card_types().into_iter().find(|x| x.1 == t).ok_or_else(|| format!("{what}.types: unknown card type {t:?}"))?.0;
            types = types.with(ty);
        }
        c.types = types;
        if !d.def["subtypes"].is_null() {
            c.subtypes = strs("subtypes").iter().map(|n| db.subtype(n).ok_or_else(|| format!("{what}.subtypes: unknown subtype {n:?}"))).collect::<Result<_, _>>()?;
        }
        if !d.def["colors"].is_null() {
            let mut cs = ColorSet::default();
            for x in strs("colors") {
                let col = COLORS.into_iter().find(|k| format!("{k:?}") == x).ok_or_else(|| format!("{what}.colors: unknown color {x:?}"))?;
                cs = cs.with(col);
            }
            c.colors = cs;
        }
        if !d.def["supertypes"].is_null() && !strs("supertypes").is_empty() {
            return err(format!("{what}.supertypes: not supported"));
        }
        for (k, dst) in [("power", &mut c.power), ("toughness", &mut c.toughness)] {
            match &d.def[k] {
                Value::Null => {}
                v => *dst = game_num(v.as_i64().ok_or_else(|| format!("{what}.{k}: expected an integer"))?, &format!("{what}.{k}"))?,
            }
        }
        crate::lki::record(s, id, c);
        s.obj_mut(id).zone = Zone::Gone;
        // "Dies" = put into a graveyard from the battlefield (CR 700.4), tokens too (111.7).
        s.ext.moved_this_turn.push((id, Zone::Battlefield, Zone::Graveyard, id));
    }
    Ok(())
}

/// A reference to a scenario key in a combat field: `"k"` or `"key:k"`.
fn bare_key(v: &str) -> &str {
    v.strip_prefix("key:").unwrap_or(v)
}

/// Load the combat fields (`attacking`, `blocking`, `blocked`) into the engine's combat state,
/// as `combat::declare_attackers` / `declare_blockers` / `damage_step` leave it (CR 506–511).
/// `ids[i]` is the object of `bf[i]`.
fn load_combat(s: &mut State, db: &CardDb, bf: &[&Value], ids: &[ObjId], find: &dyn Fn(&str) -> Option<ObjId>, pp: PhasePoint, step: Step) -> Result<(), String> {
    use Step::*;
    let active = s.turn.active;
    let def = active.opp();
    // Which combat state exists at this position: attackers are declared by the declare
    // attackers step's turn-based action (CR 508.1), blockers by the declare blockers step's
    // (509.1); creatures are removed from combat as the end of combat step ends (511.3).
    let declared = |tba_step: Step| step > tba_step && step <= EndCombat || (step == tba_step && pp == PhasePoint::Priority);
    let (attackers_ok, blockers_ok) = (declared(DeclareAttackers), declared(DeclareBlockers));
    let mut attackers: Vec<(ObjId, PlayerId)> = vec![];
    let mut attacking_pw: Vec<(ObjId, ObjId)> = vec![];
    for (i, b) in bf.iter().enumerate() {
        let what = format!("battlefield[{i}]");
        let id = ids[i];
        let Some(a) = b["attacking"].as_str() else {
            if !b["attacking"].is_null() {
                return err(format!("{what}.attacking: expected \"p0\", \"p1\" or a planeswalker key"));
            }
            continue;
        };
        if !attackers_ok {
            return err(format!("{what}: \"attacking\" at {} ({:?}): attackers are declared in the declare attackers step's turn-based action (CR 508.1) and leave combat as the end of combat step ends (CR 511.3)", step_name(step), pp));
        }
        if s.obj(id).controller != active {
            return err(format!("{what}: an attacking creature is controlled by the active player (CR 508.1a)"));
        }
        if !crate::query::chars(s, db, id).types.has(CardType::Creature) {
            return err(format!("{what}: attacking but not a creature (CR 506.4: it would be removed from combat)"));
        }
        match a {
            "p0" | "p1" => {
                if a != format!("p{}", def.0) {
                    return err(format!("{what}: attacks {a}, but the defending player is p{} (CR 506.2)", def.0));
                }
            }
            k => {
                let pw = find(bare_key(k)).ok_or_else(|| format!("{what}.attacking: unknown key {k:?}"))?;
                let pc = crate::query::chars(s, db, pw);
                if s.obj(pw).zone != Zone::Battlefield || s.obj(pw).controller != def {
                    return err(format!("{what}.attacking: {k:?} is not a permanent the defending player controls (CR 508.1b)"));
                }
                if pc.types.has(CardType::Battle) {
                    return err(format!("{what}: attacking a battle is not supported (the engine's combat has no battles, CR 310.8)"));
                }
                if !pc.types.has(CardType::Planeswalker) {
                    return err(format!("{what}.attacking: {k:?} is not a planeswalker (CR 508.1b)"));
                }
                attacking_pw.push((id, pw));
            }
        }
        attackers.push((id, def));
    }
    if matches!(step, DeclareBlockers | FirstStrikeDamage | CombatDamage) && attackers.is_empty() {
        return err(format!("step {} with no attacking creature: the combat damage steps and the declare blockers step are skipped without attackers (CR 508.8)", step_name(step)));
    }
    let mut blocks: Vec<(ObjId, ObjId)> = vec![];
    let mut blocked: Vec<ObjId> = vec![];
    for (i, b) in bf.iter().enumerate() {
        let what = format!("battlefield[{i}]");
        let id = ids[i];
        let list: Vec<&str> = match &b["blocking"] {
            Value::Null => vec![],
            Value::Array(a) => a.iter().map(|x| x.as_str().ok_or_else(|| format!("{what}.blocking: expected attacker keys"))).collect::<Result<_, _>>()?,
            _ => return err(format!("{what}.blocking: expected a list of attacker keys")),
        };
        if list.is_empty() {
            continue;
        }
        if !blockers_ok {
            return err(format!("{what}: \"blocking\" at {} ({:?}): blockers are declared in the declare blockers step's turn-based action (CR 509.1)", step_name(step), pp));
        }
        if list.len() > 1 {
            return err(format!("{what}: blocking several attackers (CR 509.1a, \"can block additional creatures\") is not supported: the engine's combat damage step deals a blocker's damage to one attacker"));
        }
        if s.obj(id).controller != def {
            return err(format!("{what}: a blocking creature is controlled by the defending player (CR 509.1a)"));
        }
        if !crate::query::chars(s, db, id).types.has(CardType::Creature) {
            return err(format!("{what}: blocking but not a creature (CR 506.4)"));
        }
        let k = list[0];
        let a = find(bare_key(k)).ok_or_else(|| format!("{what}.blocking: unknown key {k:?}"))?;
        if !attackers.iter().any(|x| x.0 == a) {
            return err(format!("{what}.blocking: {k:?} is not an attacking creature"));
        }
        blocks.push((id, a));
        if !blocked.contains(&a) {
            blocked.push(a);
        }
    }
    for (i, b) in bf.iter().enumerate() {
        let what = format!("battlefield[{i}]");
        let id = ids[i];
        match &b["blocked"] {
            Value::Null => {}
            Value::Bool(v) => {
                if !attackers.iter().any(|x| x.0 == id) {
                    if *v {
                        return err(format!("{what}: \"blocked\" on a creature that isn't attacking"));
                    }
                    continue;
                }
                if *v && !blockers_ok {
                    return err(format!("{what}: \"blocked\" before blockers are declared (CR 509.1h)"));
                }
                if !*v && blocked.contains(&id) {
                    return err(format!("{what}: \"blocked\": false, but creatures block it (CR 509.1h)"));
                }
                // CR 509.1h: it remains blocked even if all its blockers leave combat.
                if *v && !blocked.contains(&id) {
                    blocked.push(id);
                }
            }
            v => return err(format!("{what}.blocked: expected true/false, got {v}")),
        }
    }
    // CR 510.4: there is a first-strike combat damage step if a combatant had first or double
    // strike as the combat damage step began (`flow::end_step`); its turn-based action records
    // those combatants (`combat::damage_step`). After it, the combatants that have first or
    // double strike now are taken to be those (the format has no field for a keyword lost or
    // gained since).
    let fs = |o: ObjId| {
        let k = crate::query::chars(s, db, o).kws;
        k.has(crate::carddb::Kw::FirstStrike) || k.has(crate::carddb::Kw::DoubleStrike)
    };
    let combatants: Vec<ObjId> = attackers.iter().map(|a| a.0).chain(blocks.iter().map(|b| b.0)).collect();
    let any_fs = combatants.iter().any(|&o| fs(o));
    if step == FirstStrikeDamage && !any_fs {
        return err("step FirstStrikeDamage without a first or double strike combatant: that step doesn't happen (CR 510.4)");
    }
    let fs_step_done = (step == FirstStrikeDamage && pp == PhasePoint::Priority) || (matches!(step, CombatDamage | EndCombat) && any_fs);
    let mut first_strike_dealt: SmallVec<[ObjId; 4]> = SmallVec::new();
    if fs_step_done {
        for &o in &combatants {
            if fs(o) && !first_strike_dealt.contains(&o) {
                first_strike_dealt.push(o);
            }
        }
    }
    s.combat = Combat {
        attackers: attackers.iter().copied().collect(),
        blocks: blocks.iter().copied().collect(),
        blocked: blocked.iter().copied().collect(),
        first_strike_dealt,
        attacking_pw: attacking_pw.iter().copied().collect(),
        declaring: SmallVec::new(),
    };
    // "Creatures that attacked this turn" (`ext4::note_attackers`): the attacking creatures.
    if attackers_ok {
        crate::ext4::note_attackers(s, attackers.len());
    }
    Ok(())
}

/// A card entry of a player zone: a name or `{name, key, ...}`.
struct CardEntry {
    name: String,
    key: Option<String>,
    exiled_with: Option<String>,
}

fn card_entry(v: &Value, what: &str) -> Result<CardEntry, String> {
    if let Some(n) = v.as_str() {
        return Ok(CardEntry { name: n.to_string(), key: None, exiled_with: None });
    }
    check_fields(v, &["name", "key", "face_down", "exiled_with", "counters"], what)?;
    let name = v["name"].as_str().ok_or_else(|| format!("{what}: missing \"name\""))?.to_string();
    if v["face_down"].as_bool().unwrap_or(false) {
        return err(format!("{what}: face-down cards outside the battlefield (CR 406.3) are not supported (v1)"));
    }
    if v.get("counters").is_some_and(|c| c.as_object().is_some_and(|m| m.values().any(|n| n.as_i64() != Some(0)))) {
        return err(format!("{what}: counters on a card outside the battlefield are not supported (v1)"));
    }
    Ok(CardEntry { name, key: v["key"].as_str().map(String::from), exiled_with: v["exiled_with"].as_str().map(String::from) })
}

fn zone_list<'a>(p: &'a Value, z: &str) -> Result<Vec<&'a Value>, String> {
    match &p[z] {
        Value::Null => Ok(vec![]),
        Value::Array(a) => Ok(a.iter().collect()),
        _ => err(format!("player.{z}: expected a list")),
    }
}

/// Card names (Forge scripts) a scenario needs loaded into the CardDb: cards in every zone and
/// non-predefined tokens named after real cards. Predefined tokens and `token_def` tokens come
/// from Forge's token scripts (loaded by `load`).
pub fn card_names(sc: &Value) -> Vec<String> {
    let mut out = vec![];
    let mut push = |n: &str| out.push(n.split(" // ").next().unwrap_or(n).trim().to_string());
    if let Some(ps) = sc["players"].as_array() {
        for p in ps {
            for z in ["library", "hand", "graveyard", "exile"] {
                for c in p[z].as_array().into_iter().flatten() {
                    if let Some(n) = c.as_str().or_else(|| c["name"].as_str()) {
                        push(n);
                    }
                }
            }
        }
    }
    for b in sc["battlefield"].as_array().into_iter().flatten() {
        let Some(n) = b["name"].as_str() else { continue };
        let token = b["token"].as_bool().unwrap_or(false);
        if token && (b.get("token_def").is_some() || predefined_token(n).is_some()) {
            continue;
        }
        push(n);
    }
    for it in sc["stack"].as_array().into_iter().flatten() {
        if let Some(n) = it["name"].as_str() {
            push(n);
        }
    }
    for d in sc["died_this_turn"].as_array().into_iter().flatten() {
        if let (Some(n), false) = (d["name"].as_str(), d["token"].as_bool().unwrap_or(false)) {
            push(n);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn predefined_token(name: &str) -> Option<&'static str> {
    let base = name.strip_suffix(" Token").unwrap_or(name);
    PREDEFINED_TOKENS.iter().find(|t| t.0 == base).map(|t| t.1)
}

fn resolve_card(db: &CardDb, name: &str, what: &str) -> Result<CardId, String> {
    let (front, back) = match name.split_once(" // ") {
        Some((a, b)) => (a.trim(), Some(b.trim())),
        None => (name.trim(), None),
    };
    let c = db.by_name(front).ok_or_else(|| format!("{what}: card {name:?} not loaded"))?;
    let cd = db.get(c);
    if !cd.playable() {
        return err(format!("{what}: card {name:?} can't be played faithfully by the engine yet: {}", cd.unsupported.join("; ")));
    }
    if let Some(b) = back {
        let ok = cd.faces.iter().skip(1).any(|f| crate::carddb::name_key_eq(&f.name, b)) || db.back_of(c).is_some_and(|bc| crate::carddb::name_key_eq(&db.get(bc).name, b));
        if !ok {
            return err(format!("{what}: {name:?}: the engine's card {:?} has no face named {b:?}", cd.name));
        }
    }
    Ok(c)
}

/// Counter kind for a scenario counter name (`+1/+1`, `loyalty`, `stun`, `lore`, ...).
fn counter_kind(db: &mut CardDb, name: &str, what: &str) -> Result<CounterKind, String> {
    let n = name.trim();
    Ok(match n {
        "+1/+1" => CounterKind::P1P1,
        "-1/-1" => CounterKind::M1M1,
        "loyalty" => CounterKind::Loyalty,
        "stun" => CounterKind::Stun,
        _ => {
            let lower = n.to_ascii_lowercase();
            if KEYWORD_COUNTERS.contains(&lower.as_str()) {
                return err(format!("{what}: keyword counter {n:?} (CR 122.1b) is not implemented in the engine"));
            }
            if n.starts_with('+') || n.starts_with('-') {
                return err(format!("{what}: counter {n:?} (+X/+Y other than +1/+1 and -1/-1, CR 122.1a) is not implemented in the engine"));
            }
            // Shield counters: CR 122.1c (`resolve::deal_damage_in`, `resolve::shield_replaces_destroy`).
            if lower == "shield" {
                return Ok(db.counter_kind("SHIELD"));
            }
            if matches!(lower.as_str(), "defense" | "rad" | "poison" | "energy") {
                return err(format!("{what}: {n:?} counters on a permanent are not implemented in the engine"));
            }
            if !n.chars().all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '-') || n.is_empty() {
                return err(format!("{what}: bad counter name {n:?}"));
            }
            db.counter_kind(&n.to_ascii_uppercase().replace(' ', "_"))
        }
    })
}

/// The scenario name of a counter kind (inverse of `counter_kind`).
pub fn counter_name(db: &CardDb, k: CounterKind) -> String {
    match k {
        CounterKind::P1P1 => "+1/+1".into(),
        CounterKind::M1M1 => "-1/-1".into(),
        CounterKind::Loyalty => "loyalty".into(),
        CounterKind::Stun => "stun".into(),
        CounterKind::Named(i) => db.counter_names.get(i as usize).map(|n| n.to_ascii_lowercase().replace('_', " ")).unwrap_or_else(|| format!("counter#{i}")),
    }
}

/// The zone an object exiled by `source` returns to when `source` leaves the battlefield
/// (CR 610.3a), from the source card's own "exile ... until ~ leaves the battlefield" ability;
/// `None` for a plain "exiled with" link (CR 607.2a). Errors if the card has both kinds.
fn link_return_zone(db: &CardDb, card: CardId) -> Result<Option<Zone>, String> {
    let mut seen: Vec<AbilityId> = vec![];
    let mut found: Vec<Option<Zone>> = vec![];
    let mut roots: Vec<AbilityId> = vec![];
    for f in &db.get(card).faces {
        roots.extend(f.spell);
        roots.extend(f.activated.iter().copied());
        roots.extend(f.hand_abilities.iter().copied());
        roots.extend(f.grave_abilities.iter().copied());
        roots.extend(f.triggers.iter().map(|t| t.execute));
        for r in &f.replacements {
            if let crate::carddb::ReplAction::Modify(a) = r.action {
                roots.push(a);
            }
        }
    }
    while let Some(a) = roots.pop() {
        if seen.contains(&a) {
            continue;
        }
        seen.push(a);
        let ad = db.ability(a);
        if let crate::carddb::Effect::Ext(crate::effect_ext::ExtEffect::LinkMoved { until }) = &ad.effect {
            if !found.contains(until) {
                found.push(*until);
            }
        }
        roots.extend(ad.sub);
    }
    match found.as_slice() {
        [] => Ok(None),
        [z] => Ok(*z),
        _ => err(format!("{}: has several exile-until abilities with different return zones; which one exiled the card is ambiguous", db.get(card).name)),
    }
}

/// Find a Forge token script for a `token_def` (vanilla tokens only).
fn token_from_def(db: &mut CardDb, name: &str, def: &Value, what: &str) -> Result<CardId, String> {
    check_fields(def, &["types", "subtypes", "colors", "power", "toughness", "text", "supertypes"], &format!("{what}.token_def"))?;
    // Rules text: keyword lines only ("Flying", "Flying, vigilance"), found in the matching Forge
    // token script; any other text is refused (the engine compiles no free text).
    let mut text_kws: Vec<Kw> = vec![];
    if let Some(t) = def["text"].as_str().filter(|t| !t.trim().is_empty()) {
        for part in t.split(['\n', ',']).map(|x| x.trim()).filter(|x| !x.is_empty()) {
            let k = ALL_KWS.iter().copied().find(|&k| kw_name(k).eq_ignore_ascii_case(part)).ok_or_else(|| format!("{what}: token_def text {t:?}: only keyword lines (CR 702) are supported, and {part:?} isn't one the engine knows"))?;
            if text_kws.contains(&k) {
                return err(format!("{what}: token_def text {t:?} repeats a keyword"));
            }
            text_kws.push(k);
        }
    } else if !def["text"].is_null() && !def["text"].is_string() {
        return err(format!("{what}.token_def.text: expected a string"));
    }
    if def["supertypes"].as_array().is_some_and(|a| !a.is_empty()) {
        return err(format!("{what}: token_def supertypes are not supported (v1)"));
    }
    let strs = |k: &str| -> Vec<String> { def[k].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).collect() };
    let (types, subtypes, colors) = (strs("types"), strs("subtypes"), strs("colors"));
    let color_letters: String = colors.iter().map(|c| c.to_ascii_lowercase()).collect();
    let mut script = if color_letters.is_empty() { "c".to_string() } else { color_letters };
    if types.iter().any(|t| t == "Artifact") {
        script.push_str("_a");
    }
    if types.iter().any(|t| t == "Creature") {
        let (p, t) = (def["power"].as_i64().ok_or_else(|| format!("{what}: token_def creature without power"))?, def["toughness"].as_i64().ok_or_else(|| format!("{what}: token_def creature without toughness"))?);
        script.push_str(&format!("_{p}_{t}"));
    }
    for st in &subtypes {
        script.push('_');
        script.push_str(&st.to_ascii_lowercase().replace(' ', "_"));
    }
    // Keyword tokens: Forge names them `<base>_<keyword>...` (text order, else alphabetical).
    let id = if text_kws.is_empty() {
        db.token(&script).map_err(|e| format!("{what}: no engine token matches token_def (tried Forge token script {script:?}: {e})"))?
    } else {
        let suffix = |ks: &[Kw]| ks.iter().map(|&k| kw_name(k).replace(' ', "_")).collect::<Vec<_>>().join("_");
        let mut sorted = text_kws.clone();
        sorted.sort_by_key(|&k| kw_name(k));
        let tries = [format!("{script}_{}", suffix(&text_kws)), format!("{script}_{}", suffix(&sorted))];
        let mut found = None;
        for t in &tries {
            if let Ok(id) = db.token(t) {
                found = Some(id);
                break;
            }
        }
        found.ok_or_else(|| format!("{what}: no engine token matches token_def (tried Forge token scripts {tries:?})"))?
    };
    let mut want_kws = KwSet::default();
    for &k in &text_kws {
        want_kws = want_kws.with(k);
    }
    // Verify the script's characteristics are exactly the definition's.
    let f = db.get(id).front();
    let mut ok = f.activated.is_empty() && f.triggers.is_empty() && f.statics.is_empty() && f.kws == want_kws;
    let tnames: Vec<String> = all_card_types().into_iter().filter(|(t, _)| f.types.has(*t)).map(|(_, n)| n.to_string()).collect();
    let mut want_t = types.clone();
    want_t.sort();
    let mut have_t = tnames;
    have_t.sort();
    ok &= want_t == have_t;
    let have_st: Vec<String> = f.subtypes.iter().map(|&s| db.subtypes[s as usize].clone()).collect();
    ok &= have_st == subtypes;
    let have_c: Vec<String> = f.colors.iter().map(|c| format!("{c:?}")).collect();
    ok &= have_c == colors;
    ok &= f.power.map(|x| x as i64) == def["power"].as_i64() && f.toughness.map(|x| x as i64) == def["toughness"].as_i64();
    if !ok {
        return err(format!("{what}: the engine's token script {script:?} doesn't match token_def exactly"));
    }
    if !crate::carddb::name_key_eq(&db.get(id).name, name) {
        return err(format!("{what}: token name {name:?} differs from the engine token's {:?} (named tokens are not supported, v1)", db.get(id).name));
    }
    Ok(id)
}

pub fn all_card_types() -> [(CardType, &'static str); 9] {
    [
        (CardType::Artifact, "Artifact"),
        (CardType::Battle, "Battle"),
        (CardType::Creature, "Creature"),
        (CardType::Enchantment, "Enchantment"),
        (CardType::Instant, "Instant"),
        (CardType::Kindred, "Kindred"),
        (CardType::Land, "Land"),
        (CardType::Planeswalker, "Planeswalker"),
        (CardType::Sorcery, "Sorcery"),
    ]
}

struct PermSpec {
    id: ObjId,
    attached_to: Option<String>,
    exiled_cards: Vec<String>,
    chosen: Option<Value>,
    face: u8,
    face_down: bool,
    counters: Vec<(CounterKind, i32)>,
    loyalty_given: bool,
}

fn new_obj(s: &mut State, card: CardId, owner: PlayerId, controller: PlayerId, zone: Zone) -> ObjId {
    let id = ObjId(s.objs.len() as u32);
    let ts = s.ts();
    s.objs.push(Obj {
        card,
        owner,
        controller,
        zone,
        tapped: false,
        ctrl_since: 0,
        damage: 0,
        deathtouched: false,
        counters: SmallVec::new(),
        ts,
        token: false,
        known_to: match zone {
            Zone::Hand | Zone::Sideboard => 1 << owner.0,
            Zone::Library => 0,
            _ => 0b11,
        },
        entered: 0,
        cast_by: None,
        face: 0,
        on_adventure: false,
        attached_to: None,
        prepared: false,
        cast_mana: 0,
    });
    id
}

/// Load a scenario/v0 position. `base` must have every card of `card_names(sc)` loaded.
pub fn load(base: &CardDb, sc: &Value) -> Result<Loaded, String> {
    check_fields(
        sc,
        &[
            "format", "name", "rules", "rulings", "why", "status", "needs", "seed", "turn", "active", "step", "priority", "phase_point", "players", "battlefield", "stack", "choices", "expect",
            "report", "ref", "eut", "auto", "rules_options", "creatures_died_this_turn", "died_this_turn", "descended_this_turn", "notes",
        ],
        "scenario",
    )?;
    match sc["format"].as_str() {
        Some("scenario/v0") => {}
        f => return err(format!("unsupported format {f:?} (expected \"scenario/v0\")")),
    }
    // Rules options.
    // Validated here; applied to the built state below (`cast::set_mana_abilities_at_priority`).
    let mana_at_priority = match sc.get("rules_options") {
        Some(ro) => rules_options(ro)?,
        None => None,
    };
    // Position.
    let phase_point = match sc["phase_point"].as_str().unwrap_or("priority") {
        "priority" => PhasePoint::Priority,
        "start" => PhasePoint::Start,
        "mulligan" => PhasePoint::Mulligan,
        p => return err(format!("unknown phase_point {p:?}")),
    };
    let step = step_of(sc["step"].as_str().ok_or("missing \"step\"")?)?;
    let stack_entries: Vec<&Value> = match &sc["stack"] {
        Value::Null => vec![],
        Value::Array(a) => a.iter().collect(),
        _ => return err("\"stack\": expected a list"),
    };
    let turn = sc["turn"].as_u64().ok_or("missing \"turn\"")?;
    if turn < 1 || turn > crate::flow::TURN_LIMIT as u64 {
        return err(format!("turn {turn} out of range"));
    }
    let turn = turn as u32;
    let active = player_of(&sc["active"], "active")?;
    let priority = player_of(&sc["priority"], "priority")?;
    check_position(phase_point, step, turn, !stack_entries.is_empty())?;
    let died = died_this_turn(sc)?;
    if !matches!(&sc["descended_this_turn"], Value::Null | Value::Bool(false)) && sc["descended_this_turn"].as_u64() != Some(0) {
        return err("descended_this_turn is not supported (v1)");
    }
    let seed = sc["seed"].as_u64().unwrap_or(0);
    let players: Vec<&Value> = sc["players"].as_array().ok_or("missing \"players\"")?.iter().collect();
    if players.len() != 2 {
        return err("\"players\" must have exactly 2 entries");
    }
    let bf: Vec<&Value> = match &sc["battlefield"] {
        Value::Null => vec![],
        Value::Array(a) => a.iter().collect(),
        _ => return err("\"battlefield\": expected a list"),
    };

    // ---- Card DB (all mutations before it is shared) ----
    let mut db = base.clone();
    let mut perm_cards: Vec<(CardId, bool)> = vec![];
    for (i, b) in bf.iter().enumerate() {
        let what = format!("battlefield[{i}]");
        check_fields(
            b,
            &["name", "key", "controller", "owner", "tapped", "summoning_sick", "counters", "damage", "attached_to", "face", "face_down", "face_down_as", "token", "token_def", "chosen", "attacking", "blocking", "blocked", "exiled_cards"],
            &what,
        )?;
        let name = b["name"].as_str().ok_or_else(|| format!("{what}: missing \"name\""))?;
        let token = b["token"].as_bool().unwrap_or(false);
        if b.get("token_def").is_some() && !token {
            return err(format!("{what}: token_def without \"token\": true"));
        }
        let card = if token {
            if let Some(def) = b.get("token_def") {
                token_from_def(&mut db, name, def, &what)?
            } else if let Some(script) = predefined_token(name) {
                db.token(script).map_err(|e| format!("{what}: predefined token {name:?}: {e}"))?
            } else {
                resolve_card(&db, name, &what)? // a token copy of a real card
            }
        } else {
            resolve_card(&db, name, &what)?
        };
        perm_cards.push((card, token));
    }
    // Counter kinds and the face-down card need the DB mutable.
    let mut perm_counters: Vec<Vec<(CounterKind, i32)>> = vec![];
    // `counters` names `loyalty` (even 0): the planeswalker doesn't get its printed loyalty
    // (`{"loyalty": 0}` is loyalty 0, CR 704.5i).
    let mut perm_loyalty_given: Vec<bool> = vec![];
    for (i, b) in bf.iter().enumerate() {
        let what = format!("battlefield[{i}]");
        let mut cs = vec![];
        let mut loyalty_given = false;
        if let Some(m) = b.get("counters") {
            let m = m.as_object().ok_or_else(|| format!("{what}.counters: expected an object"))?;
            for (k, n) in m {
                let n = n.as_i64().ok_or_else(|| format!("{what}.counters.{k}: expected an integer"))?;
                if n < 0 {
                    return err(format!("{what}.counters.{k}: negative count"));
                }
                // (a count of 0 of another kind is ignored)
                loyalty_given |= k.trim() == "loyalty";
                if n > 0 {
                    let n = game_num(n, &format!("{what}.counters.{k}"))?;
                    cs.push((counter_kind(&mut db, k, &what)?, n));
                }
            }
        }
        perm_counters.push(cs);
        perm_loyalty_given.push(loyalty_given);
        if b["face_down"].as_bool().unwrap_or(false) {
            match b["face_down_as"].as_str() {
                // Manifest dread manifests one of the cards (CR 701.62a): the same face-down
                // permanent as a manifest (CR 701.40).
                Some("manifest" | "manifest_dread") => {
                    db.ensure_face_down_card();
                }
                Some(k @ ("cloak" | "disguise")) => {
                    return err(format!("{what}: face-down as {k:?} is not supported: the engine's face-down permanents are manifests, which have no ward {{2}} (CR 701.58a, 702.168a)"))
                }
                Some(k @ ("morph" | "megamorph")) => return err(format!("{what}: face-down as {k:?} is not supported: the engine implements no morph (CR 702.37); its face-down permanents are manifests")),
                None => return err(format!("{what}: face_down without \"face_down_as\" means a morph (the format's default), which the engine doesn't implement; give \"face_down_as\": \"manifest\" or \"manifest_dread\"")),
                Some(other) => return err(format!("{what}: unknown face_down_as {other:?}")),
            }
        }
    }
    // Cards standing for the creatures that died this turn (a token that isn't a copy of a
    // card: the engine's token for its definition).
    let mut died = died;
    for (i, d) in died.iter_mut().enumerate() {
        let what = format!("died_this_turn[{i}]");
        let real = db.by_name(d.name.split(" // ").next().unwrap_or(&d.name).trim());
        d.card = Some(match (d.token, real) {
            (false, _) => resolve_card(&db, &d.name, &what)?,
            (true, Some(c)) => c,
            (true, None) => {
                let mut def = json!({});
                for k in ["types", "subtypes", "colors", "power", "toughness"] {
                    if !d.def[k].is_null() {
                        def[k] = d.def[k].clone();
                    }
                }
                token_from_def(&mut db, &d.name, &def, &what)?
            }
        });
    }
    // Player zone cards.
    struct PZone {
        lib: Vec<(CardId, CardEntry)>,
        hand: Vec<(CardId, CardEntry)>,
        gy: Vec<(CardId, CardEntry)>,
        ex: Vec<(CardId, CardEntry)>,
    }
    let mut pzones = vec![];
    for (pi, p) in players.iter().enumerate() {
        let what = format!("players[{pi}]");
        check_fields(
            p,
            &["life", "poison", "library", "hand", "graveyard", "exile", "mana_pool", "lands_played_this_turn", "counters", "speed", "mulligans", "spells_cast_this_turn", "life_lost_this_turn", "life_gained_this_turn"],
            &what,
        )?;
        let mut z = PZone { lib: vec![], hand: vec![], gy: vec![], ex: vec![] };
        for (zn, dst) in [("library", &mut z.lib), ("hand", &mut z.hand), ("graveyard", &mut z.gy), ("exile", &mut z.ex)] {
            for (j, c) in zone_list(p, zn)?.into_iter().enumerate() {
                let w = format!("{what}.{zn}[{j}]");
                let e = card_entry(c, &w)?;
                if e.exiled_with.is_some() && zn != "exile" {
                    return err(format!("{w}: exiled_with outside exile"));
                }
                let card = resolve_card(&db, &e.name, &w)?;
                dst.push((card, e));
            }
        }
        pzones.push(z);
    }
    // Link return zones (needs only the DB).
    let db = Arc::new(db);

    // ---- State ----
    let empty = crate::engine::Deck { main: vec![], side: vec![] };
    let mut s = crate::flow::initial_state(&db, [&empty, &empty], seed);
    s.events.clear();
    s.stage = Stage::Playing;
    for p in &mut s.players {
        p.kept = true;
        p.drew_from_empty = false;
    }
    s.rng = Rng::new(seed);
    s.lineage = Some(vec![]);
    let mut keys: Vec<Option<String>> = vec![];
    let mut key_ids: HashMap<String, ObjId> = HashMap::new();
    let mut add_key = |keys: &mut Vec<Option<String>>, id: ObjId, k: Option<String>, what: &str| -> Result<(), String> {
        while keys.len() <= id.idx() {
            keys.push(None);
        }
        if let Some(k) = &k {
            if k.is_empty() || k.contains(char::is_whitespace) {
                return err(format!("{what}: bad key {k:?}"));
            }
            if key_ids.insert(k.clone(), id).is_some() {
                return err(format!("{what}: duplicate key {k:?}"));
            }
        }
        keys[id.idx()] = k;
        Ok(())
    };
    // Permanents, in timestamp order (earlier entries are older, CR 613.7).
    let mut perms: Vec<PermSpec> = vec![];
    for (i, b) in bf.iter().enumerate() {
        let what = format!("battlefield[{i}]");
        let controller = player_of(&b["controller"], &format!("{what}.controller"))?;
        let owner = if b["owner"].is_null() { controller } else { player_of(&b["owner"], &format!("{what}.owner"))? };
        let (card, token) = perm_cards[i];
        let id = new_obj(&mut s, card, owner, controller, Zone::Battlefield);
        s.battlefield.push(id);
        add_key(&mut keys, id, b["key"].as_str().map(String::from), &what)?;
        let sick = b["summoning_sick"].as_bool().unwrap_or(false);
        if turn == 1 && controller != active && !sick {
            return err(format!("{what}: on turn 1 the non-active player hasn't begun a turn, so their permanents are summoning sick (CR 302.6); give \"summoning_sick\": true"));
        }
        let damage = b["damage"].as_i64().unwrap_or(0);
        if damage < 0 {
            return err(format!("{what}: negative damage"));
        }
        let damage = game_num(damage, &format!("{what}.damage"))?;
        let face = b["face"].as_u64().unwrap_or(0);
        if face > 1 {
            return err(format!("{what}: face {face} is not supported (0 or 1)"));
        }
        {
            let o = s.obj_mut(id);
            o.token = token;
            o.tapped = b["tapped"].as_bool().unwrap_or(false);
            o.damage = damage;
            // CR 302.6: summoning sick = not controlled continuously since its controller's
            // most recent turn began (`State::is_sick`); a sick permanent entered this turn.
            if sick {
                o.ctrl_since = turn;
                o.entered = turn;
            }
        }
        let exiled_cards: Vec<String> = b["exiled_cards"].as_array().into_iter().flatten().map(|v| v.as_str().map(String::from).ok_or_else(|| format!("{what}.exiled_cards: expected keys"))).collect::<Result<_, _>>()?;
        let loyalty_given = perm_loyalty_given[i];
        perms.push(PermSpec {
            id,
            attached_to: match &b["attached_to"] {
                Value::Null => None,
                Value::String(k) => Some(k.clone()),
                v => return err(format!("{what}.attached_to: expected a key, got {v}")),
            },
            exiled_cards,
            chosen: b.get("chosen").cloned(),
            face: face as u8,
            face_down: b["face_down"].as_bool().unwrap_or(false),
            counters: perm_counters[i].clone(),
            loyalty_given,
        });
    }
    // Player zones.
    let mut exile_links: Vec<(ObjId, String, String)> = vec![];
    for (pi, z) in pzones.into_iter().enumerate() {
        let p = PlayerId(pi as u8);
        let what = format!("players[{pi}]");
        // Library: listed top first; the top is the end of the vector.
        for (j, (card, e)) in z.lib.into_iter().enumerate().collect::<Vec<_>>().into_iter().rev() {
            let id = new_obj(&mut s, card, p, p, Zone::Library);
            s.player_mut(p).library.push(id);
            add_key(&mut keys, id, e.key, &format!("{what}.library[{j}]"))?;
        }
        for (j, (card, e)) in z.hand.into_iter().enumerate() {
            let id = new_obj(&mut s, card, p, p, Zone::Hand);
            s.player_mut(p).hand.push(id);
            add_key(&mut keys, id, e.key, &format!("{what}.hand[{j}]"))?;
        }
        // Graveyard: bottom first (the order cards were put there); the vector's end is the top.
        for (j, (card, e)) in z.gy.into_iter().enumerate() {
            let id = new_obj(&mut s, card, p, p, Zone::Graveyard);
            s.player_mut(p).graveyard.push(id);
            add_key(&mut keys, id, e.key, &format!("{what}.graveyard[{j}]"))?;
        }
        for (j, (card, e)) in z.ex.into_iter().enumerate() {
            let id = new_obj(&mut s, card, p, p, Zone::Exile);
            s.exile.push(id);
            let w = format!("{what}.exile[{j}]");
            if let Some(src) = &e.exiled_with {
                exile_links.push((id, src.clone(), w.clone()));
            }
            add_key(&mut keys, id, e.key, &w)?;
        }
    }
    // Spells on the stack (bottom first): their card objects, keyed like the other cards; the
    // stack items are made by `scenario_stack::load_stack` once the position is complete.
    let mut stack_objs: Vec<Option<ObjId>> = vec![];
    for (i, e) in stack_entries.iter().enumerate() {
        let what = format!("stack[{i}]");
        if e["kind"].as_str() != Some("spell") {
            stack_objs.push(None);
            continue;
        }
        let name = e["name"].as_str().ok_or_else(|| format!("{what}: a spell needs \"name\""))?;
        if name.contains(" // ") {
            return err(format!("{what}: {name:?}: which face was cast can't be given (spells of multi-face cards are not supported on a preset stack)"));
        }
        let card = resolve_card(&db, name, &what)?;
        let controller = player_of(&e["controller"], &format!("{what}.controller"))?;
        let id = new_obj(&mut s, card, controller, controller, Zone::Stack);
        add_key(&mut keys, id, e["key"].as_str().map(String::from), &what)?;
        stack_objs.push(Some(id));
    }
    // Creatures that died this turn (last known information, CR 608.2h): old objects, not in any
    // zone, so they have no reference and never appear in the state.
    load_died(&mut s, &db, &died)?;
    let n_init = s.objs.len() as u32;
    keys.resize(n_init as usize, None);
    let find = |k: &str| key_ids.get(k).copied();

    // Permanent details that refer to other objects or need the DB.
    for (i, ps) in perms.iter().enumerate() {
        let what = format!("battlefield[{i}]");
        let id = ps.id;
        let card = s.obj(id).card;
        let cd = db.get(card);
        // Counters; a planeswalker without loyalty gets its printed loyalty (CR 306.5b).
        for &(k, n) in &ps.counters {
            s.obj_mut(id).add_counter(k, n);
        }
        if !ps.loyalty_given {
            if let Some(l) = cd.front().loyalty {
                if ps.face == 0 && !ps.face_down {
                    s.obj_mut(id).add_counter(CounterKind::Loyalty, l);
                }
            }
        }
        // Transformed (CR 712): the back face is up, as `ext3::transform` represents it.
        if ps.face == 1 {
            if ps.face_down {
                return err(format!("{what}: face 1 and face_down together"));
            }
            match cd.layout {
                Layout::Transform => {
                    let back = db.back_of(card).ok_or_else(|| format!("{what}: {} has no back face in the engine", cd.name))?;
                    s.copy_effects.push(CopyEffect { obj: id, printed: card, name: None, add_types: TypeSet::default(), add_subtypes: SmallVec::new(), kws: Default::default(), set_pt: None, transformed: true, add_super: 0 });
                    s.obj_mut(id).card = back;
                    if !ps.loyalty_given {
                        if let Some(l) = db.get(back).front().loyalty {
                            s.obj_mut(id).add_counter(CounterKind::Loyalty, l);
                        }
                    }
                }
                l => return err(format!("{what}: face 1 is supported for transforming double-faced cards only (layout {l:?})")),
            }
        }
        // Face down (CR 708), as `ext6::manifest` represents it.
        if ps.face_down {
            let fd = db.x6.face_down_card.ok_or("face-down card missing")?;
            s.ext.x6.face_down.push((id, card));
            let ctrl = s.obj(id).controller;
            let o = s.obj_mut(id);
            o.card = fd;
            o.known_to = 1 << ctrl.0;
        }
        // Chosen values (CR 614.12, 607): type and name.
        if let Some(ch) = &ps.chosen {
            check_fields(ch, &["type", "name", "color", "mode"], &format!("{what}.chosen"))?;
            if let Some(t) = ch["type"].as_str() {
                let st = db.subtype(t).ok_or_else(|| format!("{what}.chosen.type: unknown subtype {t:?}"))?;
                s.ext.chosen_type.push((id, st));
            }
            if let Some(n) = ch["name"].as_str() {
                let i = crate::name_pool::names().iter().position(|p| crate::carddb::name_key_eq(&p.name, n)).ok_or_else(|| format!("{what}.chosen.name: {n:?} is not in the engine's name pool"))?;
                s.ext.named.push((id, i as u16));
            }
            for k in ["color", "mode"] {
                if !ch[k].is_null() {
                    return err(format!("{what}.chosen.{k} is not supported (the engine records no chosen {k} for permanents)"));
                }
            }
        }
        // Attachments.
        if let Some(k) = &ps.attached_to {
            if k == "p0" || k == "p1" {
                return err(format!("{what}: attached to a player is not supported (no such Auras in the engine)"));
            }
            let host = find(k).ok_or_else(|| format!("{what}.attached_to: unknown key {k:?}"))?;
            if s.obj(host).zone != Zone::Battlefield {
                return err(format!("{what}.attached_to: {k:?} is not on the battlefield"));
            }
            if host == id {
                return err(format!("{what}: attached to itself"));
            }
            let c = crate::query::chars(&s, &db, id);
            let sub = |n: &str| db.subtype(n).is_some_and(|st| c.subtypes.contains(&st));
            if !(sub("Aura") || sub("Equipment") || sub("Fortification")) {
                return err(format!("{what}: attached_to on a permanent that is not an Aura, Equipment or Fortification is not supported"));
            }
            s.obj_mut(id).attached_to = Some(host);
        }
    }
    // "Exiled with" links (CR 607.2a, 610.3).
    let mut links: Vec<(ObjId, ObjId, String)> = vec![];
    for (id, src, w) in &exile_links {
        let sid = find(src).ok_or_else(|| format!("{w}.exiled_with: unknown key {src:?}"))?;
        links.push((*id, sid, w.clone()));
    }
    for (i, ps) in perms.iter().enumerate() {
        for k in &ps.exiled_cards {
            let what = format!("battlefield[{i}].exiled_cards");
            let cid = find(k).ok_or_else(|| format!("{what}: unknown key {k:?}"))?;
            if s.obj(cid).zone != Zone::Exile {
                return err(format!("{what}: {k:?} is not in exile"));
            }
            match links.iter().find(|l| l.0 == cid) {
                Some(l) if l.1 != ps.id => return err(format!("{what}: {k:?} is exiled with another object")),
                Some(_) => {}
                None => links.push((cid, ps.id, what.clone())),
            }
        }
    }
    for (cid, sid, w) in &links {
        if s.obj(*sid).zone != Zone::Battlefield {
            return err(format!("{w}: \"exiled with\" source must be on the battlefield (v1)"));
        }
        let until = link_return_zone(&db, crate::ext6::identity(&s, *sid)).map_err(|e| format!("{w}: {e}"))?;
        s.ext.exiled_with.link(*cid, *sid, until);
    }
    // Players.
    for (pi, p) in players.iter().enumerate() {
        let pid = PlayerId(pi as u8);
        let what = format!("players[{pi}]");
        let int = |k: &str, d: i64| -> Result<i64, String> {
            match &p[k] {
                Value::Null => Ok(d),
                v => v.as_i64().ok_or_else(|| format!("{what}.{k}: expected an integer")),
            }
        };
        let life = int("life", 20)?;
        let poison = int("poison", 0)?;
        let lands = int("lands_played_this_turn", 0)?;
        let speed = int("speed", 0)?;
        let mulls = int("mulligans", 0)?;
        let gained = int("life_gained_this_turn", 0)?;
        let lost = int("life_lost_this_turn", 0)?;
        if !(0..=255).contains(&poison) || !(0..=255).contains(&lands) || !(0..=4).contains(&speed) || gained < 0 || lost < 0 {
            return err(format!("{what}: value out of range"));
        }
        let (life, gained, lost) = (game_num(life, &format!("{what}.life"))?, game_num(gained, &format!("{what}.life_gained_this_turn"))?, game_num(lost, &format!("{what}.life_lost_this_turn"))?);
        if mulls != 0 {
            return err(format!("{what}.mulligans: mulligans already taken are not supported in a loaded position (start at phase_point \"mulligan\" and take them as choices)"));
        }
        if phase_point != PhasePoint::Priority && p["mana_pool"].as_array().is_some_and(|a| !a.is_empty()) {
            return err(format!("{what}.mana_pool: mana in a pool at the start of a step (mana empties between steps, CR 500.4)"));
        }
        if p["counters"].as_object().is_some_and(|m| m.values().any(|v| v.as_i64() != Some(0))) {
            return err(format!("{what}.counters: player counters (energy, rad, ...) are not implemented in the engine"));
        }
        if p["spells_cast_this_turn"].as_array().is_some_and(|a| !a.is_empty()) {
            return err(format!("{what}.spells_cast_this_turn is not supported: the engine keeps the spells themselves (as cast) for this-turn conditions, which a loaded position can't carry (v1)"));
        }
        let pl = s.player_mut(pid);
        pl.life = life;
        pl.poison = poison as u8;
        pl.lands_played = lands as u8;
        s.ext.speed[pi] = speed as u8;
        if speed > 0 {
            s.ext.speed_turn[pi] = 0;
        }
        s.turn_stats.life_gained[pi] = gained;
        s.turn_stats.life_lost[pi] = lost;
        for (j, m) in p["mana_pool"].as_array().into_iter().flatten().enumerate() {
            let sym = m.as_str().unwrap_or("");
            let t = ManaType::all().into_iter().find(|t| t.sym().to_string() == sym).ok_or_else(|| format!("{what}.mana_pool[{j}]: unknown mana symbol {m}"))?;
            s.ext.pool[pi][t as usize] += 1;
        }
    }
    let auto_mana = match sc.get("auto") {
        None => true,
        Some(a) => {
            check_fields(a, &["mana"], "auto")?;
            a["mana"].as_bool().unwrap_or(true)
        }
    };
    if phase_point == PhasePoint::Mulligan {
        // CR 103.5: before the game, each player draws an opening hand from the top of the
        // given library (no shuffle: the order is explicit); then mulligan decisions
        // (`flow::mulligan_unit`), and the starting player's first turn begins (103.8).
        if !bf.is_empty() || !died.is_empty() {
            return err("phase_point \"mulligan\": the battlefield is empty and nothing has died before the game");
        }
        for p in [PlayerId(0), PlayerId(1)] {
            let pl = s.player(p);
            if pl.library.len() < crate::flow::HAND_SIZE {
                return err(format!("phase_point \"mulligan\": p{}'s library has fewer than {} cards, too few to draw an opening hand (CR 103.5; a deck has at least 60 cards, 100.2a)", p.0, crate::flow::HAND_SIZE));
            }
            if !pl.hand.is_empty() || !pl.graveyard.is_empty() || s.exile.iter().any(|&o| s.obj(o).owner == p) {
                return err("phase_point \"mulligan\": hands, graveyards and exile are empty before the game (the opening hand is drawn from the library, CR 103.5)");
            }
        }
        if priority != active {
            return err("phase_point \"mulligan\": the starting player (\"active\") declares first (CR 103.5); give \"priority\" = \"active\"");
        }
        s.stage = Stage::Mulligan;
        s.starting_player = active;
        s.turn = Turn { number: 0, active, step: Step::Untap, tba_done: false, priority: active, passes: 0 };
        for p in &mut s.players {
            p.kept = false;
            p.lands_played = 0;
        }
        for p in [PlayerId(0), PlayerId(1)] {
            for _ in 0..crate::flow::HAND_SIZE {
                crate::flow::draw(&mut s, p);
            }
            s.player_mut(p).drew_from_empty = false;
        }
        s.events.clear();
        check_invariants(&s, &db)?;
        let mut g = Game::from_state(db, s);
        auto_pay(&mut g, auto_mana)?;
        if let Some(e) = g.error() {
            return err(format!("engine error right after loading: {e}"));
        }
        return Ok(Loaded { game: g, keys: Keys { n_init, keys, auto_mana } });
    }
    // Turn (CR 500; 103.8a: turn 1 is the starting player's first turn).
    let starting = if turn % 2 == 1 { active } else { active.opp() };
    s.starting_player = starting;
    // `phase_point: "start"`: the step's turn-based actions (`flow::turn_based_actions`) haven't
    // happened; the engine performs them first, then the active player gets priority.
    let start = phase_point == PhasePoint::Start;
    s.turn = Turn { number: turn, active, step, tba_done: !start, priority: if start { active } else { priority }, passes: 0 };
    s.player_mut(active).last_turn_started = turn;
    s.player_mut(active.opp()).last_turn_started = turn - 1;
    s.turn_stats.turns_taken[starting.idx()] = turn.div_ceil(2) as u16;
    s.turn_stats.turns_taken[starting.opp().idx()] = (turn / 2) as u16;
    crate::hist::on_event(&mut s, &Event::TurnBegan { turn, active });
    s.events.clear();
    // Combat (CR 506–511).
    let perm_ids: Vec<ObjId> = perms.iter().map(|p| p.id).collect();
    load_combat(&mut s, &db, &bf, &perm_ids, &find, phase_point, step)?;
    // The stack (bottom first).
    crate::scenario_stack::load_stack(&mut s, &db, &stack_entries, &stack_objs, &find)?;

    // ---- Invariants ----
    check_invariants(&s, &db)?;
    // No state-based action may be pending (CR 704.3): the position is one where a player has
    // priority, which SBAs always precede (or, at the start of a step, one that followed the
    // last priority of the previous step).
    {
        let mut probe = s.clone();
        let mut cx = Ctx::scratch(&mut probe, &db);
        match crate::sba::check(&mut cx) {
            Ok(false) => {}
            Ok(true) => return err(format!("state-based actions would apply in this position (CR 704.3), so it isn't a priority position: {}", sba_diff(&s, &probe, &db, &keys))),
            Err(Halt::GameOver) => return err("the game is already over in this position (CR 704.5a-c)"),
            Err(Halt::Need(d)) => return err(format!("state-based actions would apply in this position (CR 704.3; {:?} choice pending)", d.kind)),
            Err(e) => return err(format!("state-based action check failed: {e:?}")),
        }
        if !probe.pending_triggers.is_empty() {
            return err("triggered abilities would be pending in this position");
        }
    }
    // Absent: the format's default, no mana abilities at priority.
    crate::cast::set_mana_abilities_at_priority(&mut s, mana_at_priority == Some(true));
    let mut g = Game::from_state(db, s);
    auto_pay(&mut g, auto_mana)?;
    if let Some(e) = g.error() {
        return err(format!("engine error right after loading: {e}"));
    }
    Ok(Loaded { game: g, keys: Keys { n_init, keys, auto_mana } })
}

/// Answer pending `PayMana` decisions with the first offered source (`Keys::auto_mana`).
pub fn auto_pay(g: &mut Game, auto_mana: bool) -> Result<(), String> {
    while auto_mana && g.pending().is_some_and(|d| d.kind == DecisionKind::PayMana) {
        g.submit(0)?;
    }
    Ok(())
}

fn sba_diff(before: &State, after: &State, db: &CardDb, keys: &[Option<String>]) -> String {
    let mut out = vec![];
    for i in 0..before.objs.len() {
        let (a, b) = (&before.objs[i], &after.objs[i]);
        if a.zone != b.zone || a.counters != b.counters || a.attached_to != b.attached_to {
            let k = keys.get(i).cloned().flatten().unwrap_or_else(|| db.get(a.card).name.clone());
            out.push(format!("{k} would change ({:?} -> {:?})", a.zone, b.zone));
        }
    }
    if before.players != after.players {
        out.push("a player would change".into());
    }
    if out.is_empty() {
        "some object would change".into()
    } else {
        out.join(", ")
    }
}

/// Zone lists partition the objects; attachments and links point to existing objects.
pub fn check_invariants(s: &State, db: &CardDb) -> Result<(), String> {
    check_zones(s, db, true)
}

/// `check_invariants`; `attachments`: also that every live Aura/Equipment is attached to a
/// permanent on the battlefield, which holds once state-based actions are done (CR 704.5m–n,
/// 117.5: at priority), not mid-way (e.g. between SBA passes, a creature has died and its Aura
/// waits for the next pass, CR 704.3).
pub fn check_zones(s: &State, db: &CardDb, attachments: bool) -> Result<(), String> {
    let mut seen = vec![0u8; s.objs.len()];
    let mut mark = |id: ObjId, z: Zone, owner: Option<PlayerId>| -> Result<(), String> {
        let o = s.objs.get(id.idx()).ok_or_else(|| format!("zone list has unknown object #{}", id.0))?;
        if o.zone != z || owner.is_some_and(|p| o.owner != p) {
            return err(format!("object #{} ({}) listed in {z:?} but is in {:?}", id.0, db.get(o.card).name, o.zone));
        }
        seen[id.idx()] += 1;
        Ok(())
    };
    for p in [PlayerId(0), PlayerId(1)] {
        let pl = s.player(p);
        for &o in &pl.library {
            mark(o, Zone::Library, Some(p))?;
        }
        for &o in &pl.hand {
            mark(o, Zone::Hand, Some(p))?;
        }
        for &o in &pl.graveyard {
            mark(o, Zone::Graveyard, Some(p))?;
        }
        for &o in &pl.sideboard {
            mark(o, Zone::Sideboard, Some(p))?;
        }
    }
    for &o in &s.battlefield {
        mark(o, Zone::Battlefield, None)?;
        // A permanent's current face is its card's front face: a transformed permanent and a
        // Room's door state are their own cards (`ext3::transform`, `ext5::set_room_state`), so
        // rules code may read a permanent's printed abilities as `db.get(card).front()`.
        // A permanent with another face index would have them read from the wrong face.
        if s.obj(o).face != 0 {
            return err(format!("object #{} ({}) on the battlefield with face {} (a permanent's face is its card's front)", o.0, db.get(s.obj(o).card).name, s.obj(o).face));
        }
    }
    for &o in &s.exile {
        mark(o, Zone::Exile, None)?;
    }
    for it in &s.stack {
        if it.kind == StackKind::Spell {
            mark(it.source, Zone::Stack, None)?;
        }
    }
    for (i, o) in s.objs.iter().enumerate() {
        let want = if o.zone == Zone::Gone { 0 } else { 1 };
        if seen[i] != want {
            return err(format!("object #{i} ({}) in {:?} appears {} times in zone lists", db.get(o.card).name, o.zone, seen[i]));
        }
        // A `Gone` object keeps `attached_to` as last known information (CR 603.10a, 608.2h:
        // "enchanted creature" leaves-the-battlefield abilities); only live objects are checked.
        if let Some(h) = o.attached_to.filter(|_| attachments && o.zone != Zone::Gone) {
            if o.zone != Zone::Battlefield || s.obj(h).zone != Zone::Battlefield {
                return err(format!("object #{i} attached to #{} off the battlefield", h.0));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// References, canonical state (state/v0) and canonical options.

/// Object references for one state (`ref_of`): lineage walked back to the scenario object.
pub struct Refs<'a> {
    s: &'a State,
    db: &'a CardDb,
    keys: &'a Keys,
    prev: Vec<u32>,
    next: Vec<u32>,
    new_names: HashMap<u32, String>,
}

const NONE: u32 = u32::MAX;

impl<'a> Refs<'a> {
    pub fn new(s: &'a State, db: &'a CardDb, keys: &'a Keys) -> Refs<'a> {
        let n = s.objs.len();
        let mut prev = vec![NONE; n];
        let mut next = vec![NONE; n];
        for &(nw, old) in s.lineage.iter().flatten() {
            prev[nw.idx()] = old.0;
            next[old.idx()] = nw.0;
        }
        let mut new_names = HashMap::new();
        let mut counts: HashMap<String, u32> = HashMap::new();
        for id in keys.n_init as usize..n {
            if prev[id] == NONE {
                let name = obj_name(s, db, ObjId(id as u32));
                let c = counts.entry(name.clone()).or_insert(0);
                *c += 1;
                new_names.insert(id as u32, format!("new:{name}:{c}"));
            }
        }
        Refs { s, db, keys, prev, next, new_names }
    }
    fn root(&self, o: ObjId) -> u32 {
        let mut r = o.0;
        while (r as usize) < self.prev.len() && self.prev[r as usize] != NONE {
            r = self.prev[r as usize];
        }
        r
    }
    /// The current object of a root (the end of its lineage chain).
    fn latest(&self, root: u32) -> u32 {
        let mut r = root;
        while self.next[r as usize] != NONE {
            r = self.next[r as usize];
        }
        r
    }
    /// `key:<k>`, `new:<Name>:<n>`, or `name:<Name>` for an unkeyed scenario card.
    pub fn ref_of(&self, o: ObjId) -> String {
        let r = self.root(o);
        if r < self.keys.n_init {
            match &self.keys.keys[r as usize] {
                Some(k) => format!("key:{k}"),
                None => format!("name:{}", obj_name(self.s, self.db, o)),
            }
        } else {
            self.new_names.get(&r).cloned().unwrap_or_else(|| format!("new:?:{}", o.0))
        }
    }
    /// The `key` field of a state object: the bare scenario key, a `new:` key, or null.
    fn key_field(&self, o: ObjId) -> Value {
        let r = self.root(o);
        if r < self.keys.n_init {
            json!(self.keys.keys[r as usize])
        } else {
            json!(self.new_names.get(&r))
        }
    }
    fn target(&self, t: Target) -> String {
        match t {
            Target::Player(p) => format!("p{}", p.0),
            Target::Obj(o) => self.ref_of(o),
            // An ability on the stack (`ext8`): its 1-based position from the bottom of the stack
            // (state/v0 lists the stack top first, after spells being cast); "stack:gone" once
            // it left.
            Target::Ability { uid, .. } => match self.s.stack_items().position(|it| it.uid == uid) {
                Some(i) => format!("stack:{}", i + 1),
                None => "stack:gone".into(),
            },
        }
    }
}

/// An object's name as the canonical state shows it: the characteristics it has now (a copy's
/// copied name, a transformed card's back face, an Adventure spell's Adventure name); a
/// face-down permanent has no name (CR 708.2a). A copy effect's name exception ("except his name
/// is Superior Spider-Man") is part of the copiable values (CR 707.9b).
pub fn obj_name(s: &State, db: &CardDb, o: ObjId) -> String {
    let ob = s.obj(o);
    if ob.zone == Zone::Battlefield && crate::ext6::face_down_card(s, o).is_some() {
        return String::new();
    }
    if let Some(n) = crate::query::name_override(s, o) {
        return db.get(n).name.clone();
    }
    let cd = db.get(ob.card);
    if ob.face != 0 && (ob.zone == Zone::Stack || ob.zone == Zone::Battlefield) {
        return cd.face(ob.face).name.clone();
    }
    cd.name.clone()
}

fn zone_name(z: Zone) -> &'static str {
    match z {
        Zone::Library => "library",
        Zone::Hand => "hand",
        Zone::Battlefield => "battlefield",
        Zone::Graveyard => "graveyard",
        Zone::Stack => "stack",
        Zone::Exile => "exile",
        Zone::Gone => "none",
        Zone::Sideboard => "sideboard",
    }
}

const ALL_KWS: [Kw; 15] = [Kw::Flying, Kw::Reach, Kw::Haste, Kw::Vigilance, Kw::Trample, Kw::Lifelink, Kw::Deathtouch, Kw::FirstStrike, Kw::DoubleStrike, Kw::Menace, Kw::Defender, Kw::Flash, Kw::Hexproof, Kw::Indestructible, Kw::Prowess];

fn kw_name(k: Kw) -> &'static str {
    match k {
        Kw::Flying => "flying",
        Kw::Reach => "reach",
        Kw::Haste => "haste",
        Kw::Vigilance => "vigilance",
        Kw::Trample => "trample",
        Kw::Lifelink => "lifelink",
        Kw::Deathtouch => "deathtouch",
        Kw::FirstStrike => "first strike",
        Kw::DoubleStrike => "double strike",
        Kw::Menace => "menace",
        Kw::Defender => "defender",
        Kw::Flash => "flash",
        Kw::Hexproof => "hexproof",
        Kw::Indestructible => "indestructible",
        Kw::Prowess => "prowess",
    }
}

/// The scenario/v0 kind of decision `d`: `kind_name`, except that the color choice of a mana
/// ability or "add mana of any one color" (`ChooseMode` with `Opt::PoolMana` options) is
/// `ChooseColor`.
pub fn decision_kind_name(d: &Decision) -> String {
    if d.kind == DecisionKind::ChooseMode && !d.options.is_empty() && d.options.iter().all(|o| matches!(o, Opt::PoolMana(_))) {
        return "ChooseColor".into();
    }
    // "Remove a counter from [this]": the kind of counter (`ext5::remove_any_counter`).
    if d.kind == DecisionKind::PayCost && !d.options.is_empty() && d.options.iter().all(|o| matches!(o, Opt::Mode(_))) {
        return "RemoveCounters".into();
    }
    kind_name(d.kind)
}

pub fn kind_name(k: DecisionKind) -> String {
    match k {
        DecisionKind::Kicker => "OptionalCost".into(),
        // CR 704.5j: the legend rule's choice of the one to keep is a ChooseCard in scenario/v0.
        DecisionKind::LegendRule => "ChooseCard".into(),
        k => format!("{k:?}"),
    }
}

fn state_object(r: &Refs, o: ObjId) -> Value {
    let (s, db) = (r.s, r.db);
    let ob = s.obj(o);
    let c = crate::query::chars(s, db, o);
    let creature = c.types.has(CardType::Creature);
    let mut counters = Map::new();
    for &(k, n) in &ob.counters {
        counters.insert(counter_name(db, k), json!(n));
    }
    let mut types: Vec<&str> = all_card_types().into_iter().filter(|(t, _)| c.types.has(*t)).map(|(_, n)| n).collect();
    types.sort();
    let mut subtypes: Vec<String> = c.subtypes.iter().map(|&st| db.subtypes[st as usize].clone()).collect();
    subtypes.sort();
    subtypes.dedup();
    let mut supertypes = vec![];
    if c.supertypes.legendary() {
        supertypes.push("Legendary");
    }
    if c.supertypes.basic() {
        supertypes.push("Basic");
    }
    if c.supertypes.0 & crate::basics::Supertypes::SNOW != 0 {
        supertypes.push("Snow");
    }
    // The type line (CR 205.1a) holds supertypes too: listed in `types` after the card types.
    types.extend(supertypes.iter().copied());
    let keywords: Vec<&str> = c.kws.iter().map(kw_name).collect();
    let colors: Vec<String> = c.colors.iter().map(|x| format!("{x:?}")).collect();
    let on_bf = ob.zone == Zone::Battlefield;
    let attacking = s.combat.attackers.iter().find(|a| a.0 == o).map(|a| match s.combat.attacking_pw.iter().find(|x| x.0 == o) {
        Some(pw) => r.ref_of(pw.1),
        None => format!("p{}", a.1 .0),
    });
    let blocking: Vec<String> = s.combat.blocks.iter().filter(|b| b.0 == o).map(|b| r.ref_of(b.1)).collect();
    let exiled_with = s.ext.exiled_with.source_of(&s.objs, o).map(|src| r.ref_of(src));
    json!({
        "zone": zone_name(ob.zone),
        "name": obj_name(s, db, o),
        "controller": ob.controller.0,
        "owner": ob.owner.0,
        "tapped": ob.tapped,
        "sick": on_bf && s.is_sick(o),
        "damage": ob.damage,
        "counters": counters,
        "power": if creature { json!(c.power) } else { Value::Null },
        "toughness": if creature { json!(c.toughness) } else { Value::Null },
        "types": types,
        "subtypes": subtypes,
        "supertypes": supertypes,
        "keywords": keywords,
        "colors": colors,
        "attached_to": ob.attached_to.map(|h| r.ref_of(h)),
        // A face-down permanent (CR 708) or a card exiled face down (CR 406.3: Outrageous
        // Robbery; known to nobody or only to the player allowed to look at it).
        "face_down": (on_bf && crate::ext6::face_down_card(s, o).is_some()) || (ob.zone == Zone::Exile && ob.face_down()),
        "token": ob.token,
        "exiled_with": exiled_with,
        "attacking": attacking,
        "blocking": blocking,
        "key": r.key_field(o),
        "ref": r.ref_of(o),
    })
}

/// God-view canonical state (`state/v0`), at the pending decision (the paused view).
pub fn state_json(g: &Game, keys: &Keys) -> Result<Value, String> {
    let s = g.view_state();
    let db = &*g.db;
    let r = Refs::new(s, db, keys);
    let mut players = vec![];
    for p in [PlayerId(0), PlayerId(1)] {
        let pl = s.player(p);
        let pool = crate::ext6::pool_total(s, p);
        let mut mana = vec![];
        for t in ManaType::all() {
            for _ in 0..pool[t as usize] {
                mana.push(t.sym().to_string());
            }
        }
        let names = |v: &[ObjId]| -> Vec<String> { v.iter().map(|&o| obj_name(s, db, o)).collect() };
        let refs = |v: &[ObjId]| -> Vec<String> { v.iter().map(|&o| r.ref_of(o)).collect() };
        let exile: Vec<ObjId> = s.exile.iter().copied().filter(|&o| s.obj(o).owner == p).collect();
        players.push(json!({
            "life": pl.life,
            "poison": pl.poison,
            "hand": names(&pl.hand),
            "hand_refs": refs(&pl.hand),
            "library_count": pl.library.len(),
            "graveyard": names(&pl.graveyard),
            "exile": names(&exile),
            "mana_pool": mana,
            "counters": {},
            "speed": s.ext.speed[p.idx()],
            "lands_played_this_turn": pl.lands_played,
        }));
    }
    let mut objects: Vec<Value> = vec![];
    // Battlefield, exile and graveyards, plus spells on the stack (zone "stack": a key's
    // zone is looked up among the objects).
    // Spells being cast (CR 601.2a: on the stack from the start of casting; not stack items yet).
    let casting: Vec<ObjId> = (0..s.objs.len()).map(|i| ObjId(i as u32)).filter(|&o| s.obj(o).zone == Zone::Stack && !s.stack_items().any(|it| it.source == o)).collect();
    let spells = s.stack_items().filter(|it| it.kind == StackKind::Spell && s.obj(it.source).zone == Zone::Stack).map(|it| it.source);
    for o in s.battlefield.iter().chain(s.exile.iter()).chain(s.players[0].graveyard.iter()).chain(s.players[1].graveyard.iter()).copied().chain(spells).chain(casting.iter().copied()) {
        objects.push(state_object(&r, o));
    }
    objects.sort_by_cached_key(|v| (v["zone"].as_str().unwrap_or("").to_string(), v["controller"].as_u64(), v["name"].as_str().unwrap_or("").to_string(), v.to_string()));
    let casting_items = casting.iter().rev().map(|&o| {
        json!({"source": r.ref_of(o), "kind": "spell", "controller": s.obj(o).controller.0, "targets": [], "name": obj_name(s, db, o), "casting": true})
    });
    // A trigger being put on the stack (CR 603.3: the topmost object; `State::being_put`)
    // with its targets chosen so far, flagged like a spell being cast.
    let item = |it: &StackItem| {
        json!({
            "source": r.ref_of(it.source),
            "kind": match it.kind { StackKind::Spell => "spell", StackKind::Activated => "activated", StackKind::Triggered => "triggered" },
            "controller": it.controller.0,
            "targets": it.targets.iter().map(|&(_, t)| r.target(t)).collect::<Vec<_>>(),
            "name": db.get(it.source_card).name,
        })
    };
    let being_put = s.being_put.iter().map(|it| {
        let mut v = item(it);
        v["being_put"] = json!(true);
        v
    });
    let stack: Vec<Value> = being_put.chain(casting_items).chain(s.stack_items().rev().map(item)).collect();
    // Where every keyed or created card is now (CR 400.7: the key follows the card).
    let mut zones = BTreeMap::new();
    for (i, k) in keys.keys.iter().enumerate() {
        if let Some(k) = k {
            let cur = r.latest(i as u32);
            zones.insert(format!("key:{k}"), json!(zone_name(s.objs[cur as usize].zone)));
        }
    }
    for (root, name) in &r.new_names {
        let cur = r.latest(*root);
        zones.insert(name.clone(), json!(zone_name(s.objs[cur as usize].zone)));
    }
    let pending = match g.pending() {
        Some(d) => {
            let opts = canon_options(g, keys, d)?;
            json!({"player": d.player.0, "kind": decision_kind_name(d), "options": opts.iter().map(|o| o.0.clone()).collect::<Vec<_>>()})
        }
        None => Value::Null,
    };
    let game_over = match g.result() {
        None => json!(false),
        Some(GameResult::Win(p)) => json!({"winner": p.0}),
        Some(GameResult::Draw) => json!({"draw": true}),
    };
    Ok(json!({
        "format": "state/v0",
        "turn": s.turn.number,
        "active": s.turn.active.0,
        "step": step_name(s.turn.step),
        "priority": s.turn.priority.0,
        "players": players,
        "objects": objects,
        "stack": stack,
        "zones": zones,
        "pending": pending,
        "game_over": game_over,
        "result": if g.result().is_some() { game_over.clone() } else { Value::Null },
        "engine_error": g.error(),
    }))
}

/// The objects an option refers to, in order (for interchangeable alternatives).
fn opt_objs(o: &Opt) -> SmallVec<[ObjId; 2]> {
    let mut v = SmallVec::new();
    match *o {
        Opt::Card(x) | Opt::PlayLand(x) | Opt::Cast(x) | Opt::CastFace(x, _) | Opt::Flashback(x) | Opt::CastPrepared(x) | Opt::Activate(x, _) | Opt::Mana(x, _, _) | Opt::Alt(x, _) => v.push(x),
        Opt::Target(Target::Obj(x)) => v.push(x),
        Opt::Attack(x, _) => v.push(x),
        Opt::AttackPw(x, pw) => {
            v.push(x);
            v.push(pw)
        }
        Opt::Block(b, a) => {
            v.push(b);
            v.push(a)
        }
        _ => {}
    }
    v
}

fn subst(o: &Opt, slot: usize, y: ObjId) -> Opt {
    match *o {
        Opt::Card(_) => Opt::Card(y),
        Opt::PlayLand(_) => Opt::PlayLand(y),
        Opt::Cast(_) => Opt::Cast(y),
        Opt::CastFace(_, f) => Opt::CastFace(y, f),
        Opt::Flashback(_) => Opt::Flashback(y),
        Opt::CastPrepared(_) => Opt::CastPrepared(y),
        Opt::Activate(_, a) => Opt::Activate(y, a),
        Opt::Mana(_, a, t) => Opt::Mana(y, a, t),
        Opt::Alt(_, m) => Opt::Alt(y, m),
        Opt::Target(Target::Obj(_)) => Opt::Target(Target::Obj(y)),
        Opt::Attack(_, p) => Opt::Attack(y, p),
        Opt::AttackPw(x, pw) => {
            if slot == 0 {
                Opt::AttackPw(y, pw)
            } else {
                Opt::AttackPw(x, y)
            }
        }
        Opt::Block(b, a) => {
            if slot == 0 {
                Opt::Block(y, a)
            } else {
                Opt::Block(b, y)
            }
        }
        other => other,
    }
}

/// Objects interchangeable with `x` (CR-identical as the engine dedups them): the same object
/// signature in the same zone (`query::obj_sig`; in hand `query::card_sig`, without hand
/// knowledge). A card something refers to (a stack target, the source being paid for) has
/// none.
fn interchangeable(s: &State, db: &CardDb, x: ObjId) -> Vec<ObjId> {
    let ox = s.obj(x);
    let pool: Vec<ObjId> = match ox.zone {
        Zone::Hand => s.player(ox.owner).hand.clone(),
        Zone::Battlefield => s.battlefield.clone(),
        Zone::Graveyard => s.player(ox.owner).graveyard.clone(),
        Zone::Exile => s.exile.clone(),
        Zone::Library => s.player(ox.owner).library.clone(),
        _ => return vec![],
    };
    if ox.zone == Zone::Hand {
        let sx = crate::query::card_sig(s, db, x);
        return pool.into_iter().filter(|&y| y != x && s.obj(y).card == ox.card && crate::query::card_sig(s, db, y) == sx).collect();
    }
    // One `obj_sigs` scan for the pool (same values as an `obj_sig`, a whole-state scan, each).
    let sx = crate::query::obj_sig(s, db, x);
    let sigs = crate::query::frozen_if_big(s, db, || crate::query::obj_sigs(s, db, &pool));
    pool.into_iter().zip(sigs).filter(|&(y, k)| y != x && k == sx).map(|(y, _)| y).collect()
}

/// Every option `o` of decision `d` stands for besides itself: `o` with one of its objects
/// replaced by an object interchangeable with it (`interchangeable`), and for `OrderTriggers` the
/// identical triggers (`sba::trigger_key`). In order, without
/// duplicates; may include options `d` also offers.
fn alternatives(s: &State, db: &CardDb, d: &Decision, o: &Opt) -> Vec<Opt> {
    let mut alts: Vec<Opt> = vec![];
    let objs = opt_objs(o);
    for (slot, &x) in objs.iter().enumerate() {
        if s.objs.get(x.idx()).is_none() {
            continue;
        }
        for y in interchangeable(s, db, x) {
            let a = subst(o, slot, y);
            if a != *o && !alts.contains(&a) {
                alts.push(a);
            }
        }
    }
    if let (DecisionKind::OrderTriggers, Opt::Trigger(i)) = (d.kind, o) {
        let all = &s.ext.ordering;
        if (*i as usize) < all.len() {
            let keys = crate::sba::trigger_keys(s, db, all);
            let k = &keys[*i as usize];
            for j in 0..all.len() {
                let a = Opt::Trigger(j as u8);
                if j != *i as usize && keys[j] == *k && !alts.contains(&a) {
                    alts.push(a);
                }
            }
        }
    }
    alts
}

/// The concrete choices option `o` of decision `d` merged (canonical options: identical
/// objects are offered once): its `alternatives` that `d` doesn't offer itself. Each must lead to
/// the same game as `o` up to object identities (checked by tests, via
/// `Game::submit_alt`). `s`: the state the decision is posed in (`Game::view_state`).
pub fn merged_alternatives(s: &State, db: &CardDb, d: &Decision, o: &Opt) -> Vec<Opt> {
    alternatives(s, db, d, o).into_iter().filter(|a| !d.options.contains(a)).collect()
}

/// Objects of the same card as `x` in its zone (its owner's, for a player's zone) that are not
/// `interchangeable` with it: candidates for a merge by a coarser key than the object signature
/// (the canonicalisation soundness test tries them where the decision's merge key is checked).
pub fn same_card_others(s: &State, db: &CardDb, x: ObjId) -> Vec<ObjId> {
    let ox = s.obj(x);
    let pool: &[ObjId] = match ox.zone {
        Zone::Hand => &s.player(ox.owner).hand,
        Zone::Battlefield => &s.battlefield,
        Zone::Graveyard => &s.player(ox.owner).graveyard,
        Zone::Exile => &s.exile,
        Zone::Library => &s.player(ox.owner).library,
        _ => return vec![],
    };
    let twins = interchangeable(s, db, x);
    pool.iter().copied().filter(|&y| y != x && s.obj(y).card == ox.card && !twins.contains(&y)).collect()
}

/// `o` with its object in `slot` (`opt_objs` order) replaced by `y`.
pub fn subst_obj(o: &Opt, slot: usize, y: ObjId) -> Opt {
    subst(o, slot, y)
}

/// The objects option `o` refers to, in slot order (`subst_obj`).
pub fn option_objects(o: &Opt) -> Vec<ObjId> {
    opt_objs(o).to_vec()
}

/// The canonical string of an `Opt::Alt` option casting
/// object `rx` (its reference) by method `m`; `face1` is the card's second face's name (an
/// airbent Adventure / Omen / split half / Room door). `None`: `Normal`, never an `Opt::Alt`.
/// Exhaustive: a new `Method` gets its string here or the crate doesn't compile; tests check
/// they are distinct.
pub fn alt_option(rx: &str, m: crate::altcast::Method, face1: &str) -> Option<String> {
    use crate::altcast::Method::*;
    Some(match m {
        Normal => return None,
        Flashback => format!("cast:{rx}#flashback"),
        Warp => format!("cast:{rx}#warp"),
        Evoke => format!("cast:{rx}#evoke"),
        Impending => format!("cast:{rx}#impending"),
        Harmonize => format!("cast:{rx}#harmonize"),
        Plot => format!("special:{rx}:plot"),
        Plotted => format!("cast:{rx}#plot"),
        Free => format!("cast:{rx}#free"),
        Mayhem => format!("cast:{rx}#mayhem"),
        Sneak => format!("cast:{rx}#sneak"),
        WebSlinging => format!("cast:{rx}#web-slinging"),
        Airbend => format!("cast:{rx}#airbend"),
        AirbendFace => format!("cast:{rx}@{face1}#airbend"),
        FlashbackGranted => format!("cast:{rx}#flashback:2"),
        TurnFaceUp => format!("special:{rx}:turn-face-up"),
        UnlockLeft => format!("special:{rx}:unlock-left"),
        UnlockRight => format!("special:{rx}:unlock-right"),
    })
}

/// The canonical string of option `o` at decision `d`.
fn canon(g: &Game, r: &Refs, d: &Decision, o: &Opt) -> Result<String, String> {
    let s = r.s;
    let db = &*g.db;
    let unmapped = || Err(format!("option {o:?} at a {:?} decision has no canonical form in the engine's scenario support yet", d.kind));
    Ok(match (d.kind, *o) {
        (_, Opt::Pass) => "pass".into(),
        (_, Opt::Done) => "done".into(),
        (_, Opt::Keep) => "keep".into(),
        (_, Opt::Mulligan) => "mulligan".into(),
        (DecisionKind::Kicker, Opt::Yes) => "cost:kicker".into(),
        (DecisionKind::Kicker, Opt::No) => "no".into(),
        (_, Opt::Yes) => "yes".into(),
        (_, Opt::No) => "no".into(),
        (DecisionKind::Surveil, Opt::ScryTop) => "top".into(),
        (_, Opt::ScryTop) => "top".into(),
        (_, Opt::ScryBottom) => "bottom".into(),
        (_, Opt::ToGraveyard) => "graveyard".into(),
        (_, Opt::PlayLand(x)) => format!("play:{}", r.ref_of(x)),
        (_, Opt::Cast(x)) => format!("cast:{}", r.ref_of(x)),
        (_, Opt::CastFace(x, f)) => format!("cast:{}@{}", r.ref_of(x), db.get(s.obj(x).card).face(f).name),
        (_, Opt::Flashback(x)) => format!("cast:{}#flashback", r.ref_of(x)),
        (_, Opt::CastPrepared(x)) => format!("cast:{}#prepared", r.ref_of(x)),
        (_, Opt::Alt(x, m)) => match crate::altcast::Method::from_u8(m).and_then(|m| alt_option(&r.ref_of(x), m, &db.get(s.obj(x).card).face(1).name)) {
            Some(c) => c,
            None => return unmapped(),
        },
        (_, Opt::Activate(x, a)) => format!("activate:{}:{}", r.ref_of(x), ability_index(s, db, x, a).ok_or_else(|| format!("activated ability {a:?} not found on {}", r.ref_of(x)))?),
        // PayCost: web-slinging's "tap this creature for mana, then return it" (`ext8`).
        (DecisionKind::PayMana | DecisionKind::PayCost, Opt::Mana(x, _, t)) => format!("mana:{}:{}", r.ref_of(x), t.sym()),
        (DecisionKind::PayMana, Opt::PoolMana(t)) => format!("pool:{}", t.sym()),
        (DecisionKind::ChooseMode, Opt::PoolMana(t)) => format!("color:{}", t.sym()),
        (DecisionKind::Priority, Opt::Mana(x, a, _)) => format!("mana:{}:{}", r.ref_of(x), mana_ability_index(s, db, x, a).ok_or_else(|| format!("mana ability {a:?} not found on {}", r.ref_of(x)))?),
        (_, Opt::Target(t)) => format!("target:{}", r.target(t)),
        (DecisionKind::ChooseX, Opt::Number(n)) => format!("x:{n}"),
        (DecisionKind::AssignDamage, Opt::Number(n)) => format!("assign:{}:{n}", r.ref_of(d.subject.ok_or("AssignDamage without a recipient")?)),
        (DecisionKind::ChooseMode, Opt::Mode(m)) => format!("mode:{}", m as u32 + 1),
        // "Remove a counter from [this]": which kind (`ext5::remove_any_counter`;
        // `RemoveCounters`).
        (DecisionKind::PayCost, Opt::Mode(m)) => {
            let o = d.subject.ok_or("remove-a-counter choice without its object")?;
            let k = *crate::ext5::remove_any_kinds(&s.obj(o).counters).get(m as usize).ok_or("remove-a-counter option out of range")?;
            format!("counter:{}", counter_name(db, k))
        }
        (_, Opt::Card(x)) => format!("choose:{}", r.ref_of(x)),
        (DecisionKind::DeclareAttackers, Opt::Attack(x, p)) => format!("attack:{}->p{}", r.ref_of(x), p.0),
        (DecisionKind::DeclareAttackers, Opt::AttackPw(x, pw)) => format!("attack:{}->{}", r.ref_of(x), r.ref_of(pw)),
        (DecisionKind::DeclareBlockers, Opt::Block(b, a)) => format!("block:{}->{}", r.ref_of(b), r.ref_of(a)),
        // Not canonical in scenario/v0 (two blockers in one pick for menace): v2.
        (DecisionKind::DeclareBlockers, Opt::Block2(b, c, a)) => format!("block:{}+{}->{}", r.ref_of(b), r.ref_of(c), r.ref_of(a)),
        (DecisionKind::OrderTriggers, Opt::Trigger(i)) => {
            let t = s.ext.ordering.get(i as usize).ok_or("trigger option out of range")?;
            format!("trigger:{}:{}", r.ref_of(t.source), trigger_index(s, db, t))
        }
        (DecisionKind::ChooseType, Opt::Subtype(st)) => format!("type:{}", db.subtypes[st as usize]),
        (DecisionKind::NameCard, Opt::Name(n)) => format!("name:{}", crate::name_pool::name(n)),
        (DecisionKind::AdditionalCost, Opt::CostOption(k)) => cost_option(db, s.obj(d.subject.ok_or("AdditionalCost without a spell")?).card, k).ok_or_else(|| format!("cost option {k} has no canonical form"))?,
        _ => return unmapped(),
    })
}

/// 1-based index of activated ability `a` among the object's non-mana activated abilities
/// (printed in order, then gained), as scenario/v0 numbers them.
fn ability_index(s: &State, db: &CardDb, x: ObjId, a: AbilityId) -> Option<usize> {
    let ob = s.obj(x);
    let f = db.get(ob.card).face(ob.face);
    // Every printed activated ability of the card whatever zone it works from,
    // in Oracle order, then the ones it has from elsewhere (granted) in timestamp order.
    let mut list = printed_ability_order(db, f);
    if !matches!(ob.zone, Zone::Hand | Zone::Graveyard) {
        for b in crate::query::chars(s, db, x).abilities.iter().copied() {
            if !db.ability(b).is_mana && !list.contains(&b) {
                list.push(b);
            }
        }
    }
    list.into_iter().position(|b| b == a).map(|i| i + 1)
}

/// Keyword abilities that are activated abilities (CR 702: equip, crew, station, craft,
/// cycling, ninjutsu) as `compile` names them (`AbilityDef::text`): each is a paragraph of the
/// Oracle text starting with the keyword.
const ACTIVATED_KEYWORDS: [&str; 6] = ["Equip", "Crew", "Station", "Craft", "Cycling", "Ninjutsu"];

/// A face's printed non-mana activated abilities (those working on the battlefield, from the hand
/// and from the graveyard) in Oracle order. `compile` lists
/// keyword abilities (Equip, Crew, ...) before the script's `A:` abilities, whatever their place
/// in the Oracle text (Improvised Arsenal: "{4}{R}: Create a token ..." is printed before
/// "Equip {R}"). Each keyword ability is placed at its keyword's paragraph and the others, in
/// script order (Forge scripts list them in Oracle order), at the remaining activated-ability
/// paragraphs (a cost, ":", an effect that doesn't add mana; loyalty abilities "[+1]: ...";
/// Class levels "{2}{W}: Level 2"). If the Oracle text doesn't give one paragraph per ability,
/// the compiled order is kept (tests check the card pool for that). Only the canonical
/// option strings use this order; the engine's option order is unchanged.
pub fn printed_ability_order(db: &CardDb, f: &crate::carddb::FaceDef) -> Vec<AbilityId> {
    let mut all: Vec<AbilityId> = vec![];
    for &a in f.activated.iter().chain(&f.hand_abilities).chain(&f.grave_abilities) {
        if !db.ability(a).is_mana && !all.contains(&a) {
            all.push(a);
        }
    }
    if all.len() < 2 {
        return all;
    }
    match oracle_positions(db, f, &all) {
        Some(pos) => {
            let mut v: Vec<(usize, AbilityId)> = pos.into_iter().zip(all).collect();
            v.sort_by_key(|x| x.0); // stable: ties (none expected) keep the compiled order
            v.into_iter().map(|x| x.1).collect()
        }
        None => all,
    }
}

/// The Oracle paragraph of each of `abilities` (see `printed_ability_order`), or None if the
/// text doesn't give one paragraph per ability.
pub fn oracle_positions(db: &CardDb, f: &crate::carddb::FaceDef, abilities: &[AbilityId]) -> Option<Vec<usize>> {
    let paras: Vec<&str> = f.oracle.lines().map(str::trim).filter(|p| !p.is_empty()).collect();
    // Activated-ability paragraphs: (index, effect text after the cost, a mana ability: "Add
    // ..." without a loyalty cost, CR 605.1a).
    let mut act: Vec<(usize, String, bool)> = vec![];
    for (i, p) in paras.iter().enumerate() {
        let q = strip_reminder(p);
        let Some((pre, post)) = q.split_once(':') else { continue };
        // An ability word ("Channel — {1}{G}, Discard this card: ...").
        let pre = pre.rsplit_once('—').map_or(pre, |x| x.1).trim();
        if cost_like(pre) {
            let post = post.trim().to_string();
            let mana = post.starts_with("Add ") && !pre.starts_with('[');
            act.push((i, post, mana));
        }
    }
    let mut pos: Vec<Option<usize>> = vec![None; abilities.len()];
    let mut claimed: Vec<usize> = vec![];
    for (k, &a) in abilities.iter().enumerate() {
        let text = &db.ability(a).text;
        let first = text.split_whitespace().next().unwrap_or("");
        let hit = if ACTIVATED_KEYWORDS.contains(&first) {
            // "Equip {1}", "Equip—Pay 3 life", a second "Equip legendary creature {1}".
            // Not "Equipped creature gets ...".
            (0..paras.len()).find(|i| paras[*i].strip_prefix(first).is_some_and(|r| !r.starts_with(char::is_alphabetic)) && !claimed.contains(i))
        } else if let Some(n) = text.strip_prefix("Level ") {
            // A Class's level-up ability (CR 716.2a): "{cost}: Level N".
            act.iter().find(|(_, post, _)| post.strip_prefix("Level ").is_some_and(|r| r.trim_end_matches('.') == n)).map(|x| x.0)
        } else {
            continue;
        };
        let i = hit?;
        if claimed.contains(&i) {
            return None;
        }
        claimed.push(i);
        pos[k] = Some(i);
    }
    let rest: Vec<usize> = act.iter().filter(|(i, _, mana)| !claimed.contains(i) && !mana).map(|x| x.0).collect();
    if rest.len() != pos.iter().filter(|p| p.is_none()).count() {
        return None;
    }
    let mut it = rest.into_iter();
    pos.into_iter().map(|p| p.or_else(|| it.next())).collect()
}

/// Oracle text without reminder text (parenthesised).
fn strip_reminder(p: &str) -> String {
    let mut out = String::new();
    let mut depth = 0u32;
    for ch in p.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// Does `pre` (an Oracle paragraph's text before its first ":") look like an activation cost?
/// Mana symbols, a loyalty cost ("[+1]", "[−2]", "[0]") or a non-mana cost ("Sacrifice ...",
/// "Remove a charge counter ...", "Tap an untapped ..."); text in quotes is a granted ability.
fn cost_like(pre: &str) -> bool {
    if pre.is_empty() || pre.contains('"') {
        return false;
    }
    if pre.starts_with('[') && pre.ends_with(']') {
        return true;
    }
    pre.contains('{') || ["Sacrifice", "Tap", "Exile", "Discard", "Remove", "Pay", "Return", "Put", "Collect", "Blight"].iter().any(|w| pre.starts_with(w))
}

/// 1-based index of mana ability `a` among the object's mana abilities (printed in order, then
/// gained): `mana:<ref>:<n>` at priority.
fn mana_ability_index(s: &State, db: &CardDb, x: ObjId, a: AbilityId) -> Option<usize> {
    crate::query::chars(s, db, x).abilities.iter().filter(|&&b| db.ability(b).is_mana).position(|&b| b == a).map(|i| i + 1)
}

/// n of `trigger:<ref>:<n>`: the number of the trigger's triggered ability among the source's
/// triggered abilities in Oracle order (`printed_trigger_numbers`); other triggers (delayed,
/// reflexive) are numbered after them (the source's triggered abilities + 1).
fn trigger_index(s: &State, db: &CardDb, t: &PendingTrigger) -> usize {
    let f = db.get(t.card).face(s.obj(t.source).face);
    let nums = printed_trigger_numbers(f);
    match t.adhoc {
        None => nums.get(t.trigger_idx as usize).copied().unwrap_or(t.trigger_idx as usize + 1),
        Some(_) => nums.iter().copied().max().unwrap_or(0) + 1,
    }
}

/// For each of a face's triggers (index into `FaceDef::triggers`), the 1-based number of its
/// triggered ability in `trigger:<ref>:<n>`. Abilities are numbered in
/// Oracle order (`printed_trigger_order`), and the engine triggers compiled from one Oracle
/// paragraph with two trigger events ("When this enters or dies", "enters or attacks", two
/// consecutive script triggers with the same text) are one triggered ability (CR 113.2c: each
/// paragraph break marks a separate ability), so they share a number whichever event fired.
/// Not merged: keyword triggers strung on one line (CR 113.2c, 702.1), chapters sharing an
/// "I, II —" paragraph (CR 714.2c: two chapter abilities) and granted triggers.
pub fn printed_trigger_numbers(f: &crate::carddb::FaceDef) -> Vec<usize> {
    let n = f.triggers.len();
    let script = |k: usize| {
        let t = &f.triggers[k].text;
        let first = t.split(|c: char| c.is_whitespace() || c == ':').next().unwrap_or("");
        !TRIGGERED_KEYWORDS.contains(&first) && !t.ends_with("(granted)")
    };
    // Same rule as `trigger_positions`' `same_para`: a script trigger continuing the previous
    // one's paragraph (same text). Holds whether or not the Oracle text gave positions. Not for
    // text-less triggers (Forge's hidden `Static$ True` bookkeeping triggers have no text).
    let continues = |k: usize| k > 0 && script(k) && script(k - 1) && !f.triggers[k].text.is_empty() && same_trigger_text(f, k);
    let mut num = vec![0; n];
    let mut cur = 0;
    let mut prev: Option<usize> = None;
    for k in printed_trigger_order(f) {
        if !(continues(k) && prev == Some(k - 1)) {
            cur += 1;
        }
        num[k] = cur;
        prev = Some(k);
    }
    num
}

/// Do triggers k - 1 and k of a face have the same text (one paragraph, two trigger events)?
/// Forge's descriptions may name the card differently in the two halves (Dragonhawk, Fate's
/// Tempest: "NICKNAME deals" / "Dragonhawk deals"), so CARDNAME, NICKNAME, the name and its
/// short form (before the comma) count as the same word.
fn same_trigger_text(f: &crate::carddb::FaceDef, k: usize) -> bool {
    let short = f.name.split(", ").next().unwrap_or(&f.name);
    let norm = |t: &str| t.replace(f.name.as_str(), "CARDNAME").replace(short, "CARDNAME").replace("NICKNAME", "CARDNAME");
    let (a, b) = (&f.triggers[k - 1].text, &f.triggers[k].text);
    a == b || norm(a) == norm(b)
}

/// Keyword abilities that are triggered abilities and that `compile` synthesizes (CR 702: ward,
/// prowess, evoke's sacrifice, warp, impending, offspring, mobilize, firebending; a saga's
/// chapter abilities, 714.2b; gift's "the chosen opponent gets the gift", 702.174b), named by the
/// first word of `TriggerDef::text`.
const TRIGGERED_KEYWORDS: [&str; 10] = ["Ward", "Prowess", "Evoke", "Warp", "Impending", "Offspring", "Mobilize", "Firebending", "Chapter", "Gift"];

/// A face's triggered abilities (indices into `FaceDef::triggers`) in Oracle order
/// (`OrderTriggers`). `compile` lists synthesized keyword triggers (evoke's
/// sacrifice, ward, ...) before the script's `T:` triggers and puts triggers of granting statics
/// ("other creatures you control have ward", text "... (granted)") last. Each keyword trigger is
/// placed at its keyword's paragraph (a keyword line may list several: "Flying, ward {2}"; a
/// chapter at its "I, II — " paragraph), the script triggers, in script order (Forge scripts list
/// them in Oracle order), at the remaining triggered-ability paragraphs ("When", "Whenever", "At",
/// after an ability word; a Case's "To solve —", CR 719.3c), the granted ones after all of them.
/// One paragraph with two trigger events ("When this enters or dies, ...") is two consecutive
/// script triggers with the same text: they share the paragraph, keep their order and share one
/// number (`printed_trigger_numbers`). If the Oracle text doesn't give one paragraph per trigger, the
/// compiled order is kept (tests check the card pool). Only the canonical option strings
/// use this order; the engine's option order, the observation and the encoding don't.
pub fn printed_trigger_order(f: &crate::carddb::FaceDef) -> Vec<usize> {
    let n = f.triggers.len();
    let compiled: Vec<usize> = (0..n).collect();
    if n < 2 {
        return compiled;
    }
    match trigger_positions(f) {
        Some(pos) => {
            let mut v: Vec<(usize, usize)> = pos.into_iter().zip(compiled).collect();
            v.sort_by_key(|x| x.0); // stable: chapters sharing a paragraph keep chapter order
            v.into_iter().map(|x| x.1).collect()
        }
        None => compiled,
    }
}

/// The Oracle paragraph of each of a face's triggers (see `printed_trigger_order`; granted ones
/// get `usize::MAX`), or None if the text doesn't give one paragraph per printed trigger.
pub fn trigger_positions(f: &crate::carddb::FaceDef) -> Option<Vec<usize>> {
    let paras: Vec<String> = f.oracle.lines().map(|p| strip_reminder(p).trim().to_string()).filter(|p| !p.is_empty()).collect();
    // Text after an ability word or a chapter numeral ("Landfall — Whenever ...", "I, II — ...").
    let body = |p: &str| -> (Option<String>, String) {
        match p.split_once('—') {
            Some((pre, post)) if !pre.contains(['{', ':', '"', ',']) || pre.split(',').all(|x| roman(x.trim()).is_some()) => (Some(pre.trim().to_string()), post.trim().to_string()),
            _ => (None, p.to_string()),
        }
    };
    let is_trigger_para = |p: &str| {
        let b = body(p).1;
        ["When ", "Whenever ", "At "].iter().any(|w| b.starts_with(w)) || p.starts_with("To solve")
    };
    let mut pos: Vec<Option<usize>> = vec![None; f.triggers.len()];
    let mut claimed: Vec<usize> = vec![];
    for (k, t) in f.triggers.iter().enumerate() {
        if t.text.ends_with("(granted)") {
            pos[k] = Some(usize::MAX);
            continue;
        }
        let first = t.text.split(|c: char| c.is_whitespace() || c == ':').next().unwrap_or("");
        if !TRIGGERED_KEYWORDS.contains(&first) {
            continue;
        }
        let hit = match first {
            "Chapter" => {
                let ch: usize = t.text.split_whitespace().nth(1)?.parse().ok()?;
                // Several chapters may share a paragraph ("I, II — ..."): not claimed.
                let i = (0..paras.len()).find(|&i| body(&paras[i]).0.is_some_and(|pre| pre.split(',').any(|x| roman(x.trim()) == Some(ch))))?;
                pos[k] = Some(i);
                continue;
            }
            kw => (0..paras.len()).find(|&i| {
                // "Ward {2}", "Flying, ward {2}", "Mobilize 2", "Evoke {R/G}{R/G}"; not "Wardens ...".
                !claimed.contains(&i)
                    && !is_trigger_para(&paras[i])
                    && paras[i].split(", ").any(|item| item.get(..kw.len()).is_some_and(|h| h.eq_ignore_ascii_case(kw)) && !item[kw.len()..].starts_with(char::is_alphabetic))
            }),
        };
        let i = hit?;
        claimed.push(i);
        pos[k] = Some(i);
    }
    let rest: Vec<usize> = (0..paras.len()).filter(|i| !claimed.contains(i) && is_trigger_para(&paras[*i])).collect();
    // Script triggers continuing the previous one's paragraph (same text: two trigger events).
    let same_para = |k: usize| k > 0 && pos[k].is_none() && pos[k - 1].is_none() && same_trigger_text(f, k);
    let groups = (0..pos.len()).filter(|&k| pos[k].is_none() && !same_para(k)).count();
    if rest.len() != groups {
        return None;
    }
    let mut it = rest.into_iter();
    let mut out: Vec<usize> = vec![];
    for k in 0..pos.len() {
        out.push(match pos[k] {
            Some(i) => i,
            None if same_para(k) => out[k - 1],
            None => it.next()?,
        });
    }
    Some(out)
}

/// A chapter numeral (CR 714.2a: "I", "II", ..., up to "X").
fn roman(s: &str) -> Option<usize> {
    ["I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX", "X"].iter().position(|r| *r == s).map(|i| i + 1)
}

fn add_cost_kind(c: &crate::altcast::AddCost) -> &'static str {
    use crate::altcast::AddCost::*;
    match c {
        Mana(_) => "mana",
        Life(_) => "life",
        Discard(..) => "discard",
        Sac(..) => "sacrifice",
        Blight(_) => "blight",
        Behold(_) => "behold",
        CollectEvidence(_) => "exile",
        ExileFromGrave(..) => "exile",
    }
}

fn cost_option(db: &CardDb, card: CardId, k: u8) -> Option<String> {
    use crate::altcast::code;
    let f = db.get(card).front();
    Some(match k {
        code::DECLINE => "no".into(),
        code::EITHER_A => format!("cost:{}", add_cost_kind(&f.alt.either.as_ref()?.0)),
        code::EITHER_B => format!("cost:{}", add_cost_kind(&f.alt.either.as_ref()?.1)),
        code::OPTIONAL => {
            let st = f.statics.iter().find(|st| matches!(st.kind, crate::carddb::StaticKind::Alt(crate::altcast::AltStatic::OptionalCost(_))))?;
            let crate::carddb::StaticKind::Alt(crate::altcast::AltStatic::OptionalCost(c)) = &st.kind else { return None };
            match st.text.to_ascii_lowercase().as_str() {
                "bargain" => "cost:bargain".into(),
                "gift" => "cost:gift".into(),
                _ => format!("cost:{}", add_cost_kind(c)),
            }
        }
        code::OFFSPRING => "cost:offspring".into(),
        code::TEAMWORK => "cost:teamwork".into(),
        _ => return None,
    })
}

/// Canonical strings of every option of `d`, in the engine's option order, each with its
/// interchangeable alternatives (identical objects are offered once).
pub fn canon_options(g: &Game, keys: &Keys, d: &Decision) -> Result<Vec<(String, Vec<String>)>, String> {
    let s = g.view_state();
    let db = &*g.db;
    let r = Refs::new(s, db, keys);
    let mut out = vec![];
    for o in &d.options {
        let c = canon(g, &r, d, o)?;
        let mut alts: Vec<String> = vec![];
        // Identical objects and identical triggers (same card and ability) are one option.
        for alt in alternatives(s, db, d, o) {
            let a = canon(g, &r, d, &alt)?;
            if a != c && !alts.contains(&a) {
                alts.push(a);
            }
        }
        out.push((c, alts));
    }
    if d.kind == DecisionKind::OrderTriggers {
        disambiguate_triggers(s, &r, d, &mut out)?;
    }
    // Two options with the same canonical string would make submission ambiguous.
    for i in 0..out.len() {
        for j in i + 1..out.len() {
            if out[i].0 == out[j].0 {
                return err(format!("two options of the {:?} decision have the same canonical string {:?}", d.kind, out[i].0));
            }
        }
    }
    Ok(out)
}

/// Two instances of one triggered ability of one source that aren't identical (`sba::trigger_key`:
/// Enduring Curiosity's draw for each of two attackers) share `trigger:<ref>:<n>`.
/// Only where two options share it, each becomes
/// `trigger:<ref>:<n>@<event ref>` (the object the trigger event names, `TrigCtx::obj`, or the
/// source of a life gain, `TrigCtx::gain_source`, CR 119.9; none: no `@`); where that still doesn't tell them apart (the same event object, or none), each gets `/k`,
/// its rank by amount ascending, then in the order the events happened (option order). The plain
/// string stays an alternative of the first instance, so a script naming it selects what it did.
fn disambiguate_triggers(s: &State, r: &Refs, d: &Decision, out: &mut [(String, Vec<String>)]) -> Result<(), String> {
    let trig = |i: usize| -> Result<&crate::state::PendingTrigger, String> {
        let Opt::Trigger(t) = d.options[i] else { return err("OrderTriggers option that isn't a trigger") };
        s.ext.ordering.get(t as usize).ok_or_else(|| "trigger option out of range".to_string())
    };
    let n = out.len();
    let base: Vec<String> = out.iter().map(|o| o.0.clone()).collect();
    let mut with_event = base.clone();
    for i in 0..n {
        if base.iter().filter(|b| **b == base[i]).count() > 1 {
            if let Some(o) = trig(i)?.trig.obj.or(trig(i)?.trig.gain_source) {
                with_event[i] = format!("{}@{}", base[i], r.ref_of(o));
            }
        }
    }
    let mut fin = with_event.clone();
    for i in 0..n {
        let same: Vec<usize> = (0..n).filter(|&j| with_event[j] == with_event[i]).collect();
        if same.len() > 1 {
            let mut order = same.clone();
            let amounts: Vec<i32> = order.iter().map(|&j| trig(j).map(|t| t.trig.amount)).collect::<Result<_, _>>()?;
            let mut keyed: Vec<(i32, usize)> = amounts.into_iter().zip(order.drain(..)).collect();
            keyed.sort();
            let k = keyed.iter().position(|x| x.1 == i).unwrap() + 1;
            fin[i] = format!("{}/{k}", with_event[i]);
        }
    }
    for i in 0..n {
        if fin[i] != base[i] {
            let first = (0..n).find(|&j| base[j] == base[i]).unwrap();
            // identical alternatives render as the plain string: drop it, it names the first
            out[i].1.retain(|a| *a != base[i]);
            if i == first {
                out[i].1.insert(0, base[i].clone());
            }
            out[i].0 = fin[i].clone();
        }
    }
    Ok(())
}

/// `{"cmd":"options"}`: the pending decision with canonical strings next to engine indices.
pub fn options_json(g: &Game, keys: &Keys) -> Result<Value, String> {
    let Some(d) = g.pending() else {
        return Ok(json!({"decision": null, "game_over": g.is_over(), "engine_error": g.error()}));
    };
    let opts = canon_options(g, keys, d)?;
    let list: Vec<Value> = opts.iter().enumerate().map(|(i, (c, alts))| json!({"i": i, "canon": c, "alts": alts})).collect();
    Ok(json!({"decision": {"player": d.player.0, "kind": decision_kind_name(d), "options": list}, "game_over": false}))
}

/// The index of the pending option named by canonical string `c` (or one of its
/// interchangeable alternatives).
pub fn find_canon(g: &Game, keys: &Keys, c: &str) -> Result<usize, String> {
    let d = g.pending().ok_or("no pending decision")?;
    let opts = canon_options(g, keys, d)?;
    let c = c.trim();
    opts.iter().position(|(o, alts)| o == c || alts.iter().any(|a| a == c)).ok_or_else(|| {
        format!("{c:?} is not a legal option of the pending {} decision of P{}; options: {:?}", decision_kind_name(d), d.player.0, opts.iter().map(|o| o.0.as_str()).collect::<Vec<_>>())
    })
}
