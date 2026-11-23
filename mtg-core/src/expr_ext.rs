//! Forge `Count$` forms that read per-turn history or choices kept in `State::ext`
//! (`ThisTurnCast_…`, `ThisTurnEntered_…`, `Converge`, card types among objects, whose turn).

use crate::basics::*;
use crate::expr::{EvalCtx, Expr, ExprCompiler, ZoneSel};
use crate::filter::Filter;
use crate::query::{chars, obj_matches, FilterCtx};
use crate::state::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExtExpr {
    /// 1 if it's the evaluating player's turn, else 0 (`PlayerCountPropertyYou$HasPropertyActive`).
    YourTurn,
    /// Number of card types among matching objects in a zone (`$CardTypes`, CR 205.2a).
    CardTypesAmong(ZoneSel, Filter),
    /// Spells cast this turn matching the filter (`Count$ThisTurnCast_<filter>`).
    ThisTurnCast(Filter),
    /// Spells cast this turn matching the filter by the player who cast the spell that caused
    /// this "whenever a player casts" trigger (`..+ControlledBy TriggeredActivator`; Magebane
    /// Lizard, `ext7`): the caster of the triggering spell (its controller as it was cast).
    ThisTurnCastByTriggeredCaster(Filter),
    /// Objects that moved to `to` (from `from`, if given) this turn matching the filter
    /// (`Count$ThisTurnEntered_<To>[_from_<From>]_<filter>`).
    ThisTurnEntered(Zone, Option<Zone>, Filter),
    /// Number of colors of mana spent to cast the source spell (`Count$Converge`, CR 702.?/207.2c).
    Converge,
    /// Permanents matching the filter the evaluating player sacrificed this turn.
    SacrificedThisTurn(Filter),
    /// Mana value of the permanent sacrificed to pay the evaluating ability's cost
    /// (`Sacrificed$CardManaCost`), as it last existed.
    SacrificedManaValue,
    /// Objects remembered by the resolving ability matching the filter
    /// (`Remembered$Valid <filter>`; `Count$RememberedSize` with `Card`).
    RememberedValid(Filter),
    /// The evaluating player's speed (CR 702.179f: 0 if they have none).
    Speed,
    /// 1 if the evaluating player has an enduring story (CR 702.195b).
    EnduringStory,
    /// Equip abilities the evaluating player activated this turn
    /// (`Count$ThisTurnActivated_Activated.Equip+YouCtrl`).
    EquipsThisTurn,
    /// Number of objects remembered / imprinted by the resolving ability
    /// (`Count$RememberedSize`, `Imprinted$Amount`).
    RememberedSize,
    ImprintedAmount,
    /// Expressions of the third long-tail batch (`ext3`).
    Lt(crate::ext3::LtExpr),
    /// Expressions of the fourth long-tail batch (`ext4`).
    Mr(crate::ext4::MrExpr),
    /// Expressions of the fifth long-tail batch (`ext5`).
    Le(crate::ext5::LeExpr),
    /// Expressions of the sixth long-tail batch (`ext6`).
    X(crate::ext6::XExpr),
    /// Expressions of the ninth batch (`ext9`).
    X9(crate::ext9::X9Expr),
}

fn zone_word(w: &str) -> Option<Zone> {
    Some(match w {
        "Battlefield" => Zone::Battlefield,
        "Graveyard" => Zone::Graveyard,
        "Hand" => Zone::Hand,
        "Exile" => Zone::Exile,
        "Library" => Zone::Library,
        "Stack" => Zone::Stack,
        _ => return None,
    })
}

/// `Head$Body` forms not handled by `ExprCompiler::expr`.
pub fn compile_ext(_ec: &ExprCompiler, head: &str, body: &str, _depth: u32) -> Result<Option<Expr>, String> {
    Ok(match (head, body) {
        ("PlayerCountPropertyYou", "HasPropertyActive") => Some(Expr::Ext(Box::new(ExtExpr::YourTurn))),
        ("Sacrificed", "CardManaCost") => Some(Expr::Ext(Box::new(ExtExpr::SacrificedManaValue))),
        ("Remembered", b) if b.starts_with("Valid ") => Some(Expr::Ext(Box::new(ExtExpr::RememberedValid(_ec.filter(b.trim_start_matches("Valid ").trim())?)))),
        // "Up to that many": the reflexive trigger's amount, computed when it was created as the
        // number of remembered objects matching this same filter (`ImmediateTrigger` compile).
        ("TriggerRemembered", b) if b.starts_with("Valid ") => Some(Expr::TriggerAmount),
        ("Remembered", "Amount") => Some(Expr::Ext(Box::new(ExtExpr::RememberedSize))),
        // "For each creature that attacked this turn" (summed over players).
        ("PlayerCountPlayers", "AttackersDeclared") => Some(Expr::Ext(Box::new(ExtExpr::Mr(crate::ext4::MrExpr::AttackedThisTurn)))),
        ("Imprinted", "Amount") => Some(Expr::Ext(Box::new(ExtExpr::ImprintedAmount))),
        // The amount of the triggering damage / life-gain event.
        ("TriggerCount", "DamageAmount") | ("TriggerCount", "LifeAmount") | ("TriggerCount", "Amount") => Some(Expr::TriggerAmount),
        // The mana value of the spell that caused the trigger (CR 202.3e: with X on the stack).
        ("TriggeredSpellAbility", "CardManaCostLKI") | ("TriggeredSpellAbility", "CardManaCost") => Some(Expr::Ext(Box::new(ExtExpr::Lt(crate::ext3::LtExpr::TriggeredSpellMv)))),
        ("PlayerCountPropertyYou", b) if b.starts_with("SacrificedThisTurn ") => Some(Expr::Ext(Box::new(ExtExpr::SacrificedThisTurn(sacrificed_filter(_ec.filter(b.trim_start_matches("SacrificedThisTurn ").trim())?))))),
        _ => None,
    })
}

/// The filter of "[permanents] you've sacrificed this turn", matched against each sacrificed
/// object as it last existed on the battlefield (`Ext::sacrificed_this_turn` keeps the old object,
/// now `Zone::Gone`, whose characteristics are its last known information, CR 608.2h). Each was a
/// permanent when sacrificed (CR 701.21a), so `Permanent` holds for all of them; `Base::Permanent`
/// itself requires the object to be on the battlefield (or a permanent card elsewhere) and so
/// would match none (Obsessive Pursuit's X would always be 0).
fn sacrificed_filter(mut f: crate::filter::Filter) -> crate::filter::Filter {
    for a in &mut f.alts {
        if matches!(a.base, crate::filter::Base::Permanent) {
            a.base = crate::filter::Base::Card;
        }
    }
    f
}

/// `Count$<body>` forms not handled by `ExprCompiler::count`.
pub fn compile_count_ext(ec: &ExprCompiler, body: &str, _depth: u32) -> Result<Option<Expr>, String> {
    // CR 103.4: each player's starting life total is 20 (two-player Standard).
    if body == "YourStartingLife" {
        return Ok(Some(Expr::Num(20)));
    }
    // CR 700.11: "you descended this turn" (`ext9`).
    if body == "YouDescendedThisTurn" {
        return Ok(Some(Expr::Ext(Box::new(ExtExpr::X9(crate::ext9::X9Expr::DescendedThisTurn)))));
    }
    if let Some(e) = crate::ext4::compile_count(body) {
        return Ok(Some(e));
    }
    if let Some(e) = crate::ext5::compile_count(ec, body)? {
        return Ok(Some(e));
    }
    if let Some(e) = crate::ext6::compile_count(body) {
        return Ok(Some(e));
    }
    if body == "RememberedSize" {
        return Ok(Some(Expr::Ext(Box::new(ExtExpr::RememberedSize))));
    }
    if body == "Converge" {
        return Ok(Some(Expr::Ext(Box::new(ExtExpr::Converge))));
    }
    if body == "ThisTurnActivated_Activated.Equip+YouCtrl" {
        return Ok(Some(Expr::Ext(Box::new(ExtExpr::EquipsThisTurn))));
    }
    if let Some(name) = body.strip_prefix("CardCounters.") {
        // Counters of a kind on the source (Chainsaw's rev counters).
        let k = match name {
            "P1P1" => CounterKind::P1P1,
            "M1M1" => CounterKind::M1M1,
            "LOYALTY" => CounterKind::Loyalty,
            n => CounterKind::Named(ec.counters.iter().position(|c| c == n).ok_or_else(|| format!("expr Count${body}"))? as u16),
        };
        return Ok(Some(Expr::Alt(crate::altcast::AltExpr::SelfCounters(k))));
    }
    if let Some(f) = body.strip_prefix("ThisTurnCast_") {
        if let Some(f) = f.strip_suffix("+ControlledBy TriggeredActivator") {
            return Ok(Some(Expr::Ext(Box::new(ExtExpr::ThisTurnCastByTriggeredCaster(ec.filter(f)?)))));
        }
        return Ok(Some(Expr::Ext(Box::new(ExtExpr::ThisTurnCast(ec.filter(f)?)))));
    }
    if let Some(rest) = body.strip_prefix("ThisTurnEntered_") {
        // <To>[_from_<From>]_<filter>
        let (to_s, rest) = rest.split_once('_').ok_or_else(|| format!("expr Count${body}"))?;
        let to = zone_word(to_s).ok_or_else(|| format!("expr Count${body}"))?;
        let (from, filt) = match rest.strip_prefix("from_") {
            Some(r) => {
                let (fz, f) = r.split_once('_').ok_or_else(|| format!("expr Count${body}"))?;
                (Some(zone_word(fz).ok_or_else(|| format!("expr Count${body}"))?), f)
            }
            None => (None, rest),
        };
        return Ok(Some(Expr::Ext(Box::new(ExtExpr::ThisTurnEntered(to, from, ec.filter(filt)?)))));
    }
    Ok(None)
}

pub fn eval_ext(e: &ExtExpr, c: &EvalCtx) -> i32 {
    let s = c.s;
    let fcx = eval_fcx!(c);
    // Inside characteristic computation (`base_only`), layer 1–5 matching (no recursion).
    let om = |s: &State, db: &crate::carddb::CardDb, f: &Filter, o: ObjId, fcx: FilterCtx| if c.base_only { crate::query::obj_matches_base(s, db, f, o, fcx) } else { obj_matches(s, db, f, o, fcx) };
    match e {
        ExtExpr::YourTurn => (s.turn.active == c.you) as i32,
        ExtExpr::CardTypesAmong(z, f) => {
            let pool: Vec<ObjId> = match z {
                ZoneSel::Battlefield => s.battlefield.clone(),
                ZoneSel::Exile => s.exile.clone(),
                ZoneSel::Graveyard => s.players.iter().flat_map(|p| p.graveyard.iter().copied()).collect(),
                ZoneSel::Hand => s.players.iter().flat_map(|p| p.hand.iter().copied()).collect(),
                ZoneSel::Library => s.players.iter().flat_map(|p| p.library.iter().copied()).collect(),
            };
            let mut types = TypeSet::default();
            for o in pool {
                if om(s, c.db, f, o, fcx) {
                    types = TypeSet(types.0 | (if c.base_only { crate::query::chars_base(s, c.db, o) } else { chars(s, c.db, o) }).types.0);
                }
            }
            types.0.count_ones() as i32
        }
        ExtExpr::ThisTurnCast(f) => s.ext.cast_this_turn.iter().filter(|(_, o)| om(s, c.db, f, *o, fcx)).count() as i32,
        ExtExpr::ThisTurnCastByTriggeredCaster(f) => {
            // The triggering spell is in `cast_this_turn` with its caster (CR 601.2i).
            let Some(by) = c.triggered.and_then(|t| s.ext.cast_this_turn.iter().find(|x| x.1 == t)).map(|x| x.0) else { return 0 };
            s.ext.cast_this_turn.iter().filter(|(p, o)| *p == by && om(s, c.db, f, *o, fcx)).count() as i32
        }
        ExtExpr::ThisTurnEntered(to, from, f) => s
            .ext
            .moved_this_turn
            .iter()
            .filter(|(old, fz, tz, new)| tz == to && from.is_none_or(|z| *fz == z) && om(s, c.db, f, if *fz == Zone::Battlefield { *old } else { *new }, fcx))
            .count() as i32,
        ExtExpr::SacrificedManaValue => s.ext.cost_sacrificed.iter().rev().find(|x| x.0 == c.source).map(|x| chars(s, c.db, x.1).mv as i32).unwrap_or(0),
        ExtExpr::RememberedValid(f) => c.rem.iter().filter(|&&o| om(s, c.db, f, o, fcx)).count() as i32,
        ExtExpr::Speed => s.ext.speed[c.you.idx()] as i32,
        ExtExpr::EnduringStory => (s.ext.enduring_story >> c.you.0 & 1) as i32,
        ExtExpr::EquipsThisTurn => s.ext.equips_this_turn[c.you.idx()] as i32,
        ExtExpr::SacrificedThisTurn(f) => s.ext.sacrificed_this_turn.iter().filter(|(p, o)| *p == c.you && om(s, c.db, f, *o, fcx)).count() as i32,
        ExtExpr::RememberedSize => c.rem.len() as i32,
        ExtExpr::ImprintedAmount => c.imprinted.len() as i32,
        ExtExpr::Converge => s.ext.spent_colors.iter().find(|x| x.0 == c.source).map(|x| (x.1 & 0b11111).count_ones() as i32).unwrap_or(0),
        ExtExpr::Lt(l) => crate::ext3::eval_expr(l, c),
        ExtExpr::Mr(m) => crate::ext4::eval_expr(m, c),
        ExtExpr::Le(m) => crate::ext5::eval_expr(m, c),
        ExtExpr::X(m) => crate::ext6::eval_expr(m, c),
        ExtExpr::X9(m) => crate::ext9::eval(m, c),
    }
}
