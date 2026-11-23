//! Forge's number/condition mini-language (SVars like `Count$Valid Creature.YouCtrl`,
//! `Count$Compare X GE1.4.2`, `TriggeredCard$CardPower`, `Number$3/Minus.X`), compiled to
//! [`Expr`] and evaluated against an [`EvalCtx`].
//!
//! Strict: unknown forms are compile errors (card marked unsupported).

use crate::basics::*;
use crate::carddb::CardDb;
use crate::filter::{CmpOp, Filter};
use crate::query::{chars, obj_matches, FilterCtx};
use crate::state::*;
use forge_script::Face;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ZoneSel {
    Battlefield,
    Graveyard,
    Hand,
    Exile,
    Library,
}

/// An object or player referred to by an expression.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ref {
    /// The source object (`Count$CardPower` etc.).
    SelfObj,
    /// Object that caused the trigger (`TriggeredCard`).
    Triggered,
    /// First target of this ability chain (`Targeted`, `ParentTargeted`).
    Targeted,
    You,
    TargetedController,
    TargetedPlayer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjProp {
    Power,
    Toughness,
    ManaValue,
    NumColors,
    /// Total mana spent to cast it (`CastTotalManaSpent`).
    ManaSpent,
    /// Number of counters of a kind on it (`CardCounters.P1P1`; last known information).
    Counters(CounterKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlayerProp {
    LifeTotal,
    CardsInHand,
    CardsInLibrary,
    CardsInGraveyard,
    LifeLostThisTurn,
    LifeGainedThisTurn,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Expr {
    Num(i32),
    XPaid,
    /// `Count$Kicked.A.B`: A if kicked else B.
    Kicked(i32, i32),
    /// Number of objects in a zone matching a filter (battlefield: all; others: all players').
    CountValid(ZoneSel, Filter),
    Obj(Ref, ObjProp),
    Player(Ref, PlayerProp),
    LifeOppsLostThisTurn,
    YourTurns,
    /// `Count$Compare L <op><n>.A.B`
    Compare(Box<Expr>, CmpOp, Box<Expr>, Box<Expr>, Box<Expr>),
    Plus(Box<Expr>, Box<Expr>),
    Minus(Box<Expr>, Box<Expr>),
    Times(Box<Expr>, Box<Expr>),
    HalfDown(Box<Expr>),
    Min(Box<Expr>, Box<Expr>),
    Max(Box<Expr>, Box<Expr>),
    /// The amount carried by the triggering event (damage dealt, life gained, ...).
    TriggerAmount,
    /// Aggregate over matching objects: (zone, filter, property, max? else sum).
    Aggregate(ZoneSel, Filter, ObjProp, bool),
    /// Expressions over choices/history kept in `State::ext` (see `expr_ext`).
    Ext(Box<crate::expr_ext::ExtExpr>),
    /// How the source was cast (optional costs paid, mana spent, ...): `altcast`.
    Alt(crate::altcast::AltExpr),
}

/// What an expression or condition is evaluated against.
#[derive(Clone, Copy, Debug)]
pub struct EvalCtx<'a> {
    pub s: &'a State,
    pub db: &'a CardDb,
    pub you: PlayerId,
    pub source: ObjId,
    pub x: i32,
    pub kicked: bool,
    pub triggered: Option<ObjId>,
    pub trigger_amount: i32,
    pub targets: &'a [Target],
    /// Objects remembered / imprinted by the resolving ability (`Count$RememberedSize`,
    /// `Imprinted$Amount`, `PlayerCountDefinedRemembered...`).
    pub rem: &'a [ObjId],
    pub imprinted: &'a [ObjId],
    /// Evaluate filters with layer 1–5 characteristics only (inside static-ability application,
    /// to avoid recursion; see query::chars).
    pub base_only: bool,
}

pub fn eval(e: &Expr, c: &EvalCtx) -> i32 {
    let s = c.s;
    match e {
        Expr::Num(n) => *n,
        Expr::XPaid => c.x,
        Expr::Kicked(a, b) => {
            if c.kicked {
                *a
            } else {
                *b
            }
        }
        Expr::CountValid(z, f) => {
            let fcx = FilterCtx { x: 0, ..eval_fcx!(c) };
            let count = |ids: &[ObjId]| {
                ids.iter()
                    .filter(|&&o| if c.base_only { crate::query::obj_matches_base(s, c.db, f, o, fcx) } else { obj_matches(s, c.db, f, o, fcx) })
                    .count() as i32
            };
            let all = || match z {
                ZoneSel::Battlefield => count(&s.battlefield),
                ZoneSel::Exile => count(&s.exile),
                ZoneSel::Graveyard => s.players.iter().map(|p| count(&p.graveyard)).sum(),
                ZoneSel::Hand => s.players.iter().map(|p| count(&p.hand)).sum(),
                ZoneSel::Library => s.players.iter().map(|p| count(&p.library)).sum(),
            };
            // A count that can't depend on the source is shared by every source asking it in a
            // memo scope (exact: `query::source_free`); on big boards only, where it pays for the
            // lookup and the filter clone (normal boards: a direct count is cheaper).
            if s.battlefield.len() >= crate::query::BIG_BOARD && crate::query::source_free(f) {
                crate::query::memo_count(s, c.db, f, *z as u8, c.you, c.base_only, all)
            } else {
                all()
            }
        }
        Expr::Obj(r, prop) => {
            let Some(o) = resolve_obj(*r, c) else { return 0 };
            let ch = chars(s, c.db, o);
            match prop {
                ObjProp::Power => ch.power,
                ObjProp::Toughness => ch.toughness,
                ObjProp::ManaValue => ch.mv as i32,
                ObjProp::NumColors => ch.colors.count() as i32,
                ObjProp::ManaSpent => s.obj(o).cast_mana as i32,
                ObjProp::Counters(k) => s.obj(o).counter(*k),
            }
        }
        Expr::Player(r, prop) => {
            let Some(p) = resolve_player(*r, c) else { return 0 };
            let pl = s.player(p);
            match prop {
                PlayerProp::LifeTotal => pl.life,
                PlayerProp::CardsInHand => pl.hand.len() as i32,
                PlayerProp::CardsInLibrary => pl.library.len() as i32,
                PlayerProp::CardsInGraveyard => pl.graveyard.len() as i32,
                PlayerProp::LifeLostThisTurn => s.turn_stats.life_lost[p.idx()],
                PlayerProp::LifeGainedThisTurn => s.turn_stats.life_gained[p.idx()],
            }
        }
        Expr::LifeOppsLostThisTurn => s.turn_stats.life_lost[c.you.opp().idx()],
        Expr::YourTurns => s.turn_stats.turns_taken[c.you.idx()] as i32,
        Expr::Compare(l, op, r, a, b) => {
            if op.eval(eval(l, c), eval(r, c)) {
                eval(a, c)
            } else {
                eval(b, c)
            }
        }
        // Game-number arithmetic never wraps (`num`: exact in range, saturating and sticky past it).
        Expr::Plus(a, b) => crate::num::add(eval(a, c), eval(b, c)),
        Expr::Minus(a, b) => crate::num::sub(eval(a, c), eval(b, c)),
        Expr::Times(a, b) => crate::num::mul(eval(a, c), eval(b, c)),
        Expr::HalfDown(a) => crate::num::div_floor(eval(a, c), 2),
        Expr::Min(a, b) => eval(a, c).min(eval(b, c)),
        Expr::Max(a, b) => eval(a, c).max(eval(b, c)),
        Expr::TriggerAmount => c.trigger_amount,
        Expr::Ext(x) => crate::expr_ext::eval_ext(x, c),
        Expr::Alt(a) => crate::altcast::eval_expr(a, c),
        Expr::Aggregate(z, f, prop, max) => {
            let fcx = eval_fcx!(c);
            let pool: Vec<ObjId> = match z {
                ZoneSel::Battlefield => s.battlefield.clone(),
                ZoneSel::Exile => s.exile.clone(),
                ZoneSel::Graveyard => s.players.iter().flat_map(|p| p.graveyard.iter().copied()).collect(),
                ZoneSel::Hand => s.players.iter().flat_map(|p| p.hand.iter().copied()).collect(),
                ZoneSel::Library => s.players.iter().flat_map(|p| p.library.iter().copied()).collect(),
            };
            let vals = pool.into_iter().filter(|&o| if c.base_only { crate::query::obj_matches_base(s, c.db, f, o, fcx) } else { obj_matches(s, c.db, f, o, fcx) }).map(|o| {
                let ch = if c.base_only { crate::query::chars_base(s, c.db, o) } else { chars(s, c.db, o) };
                match prop {
                    ObjProp::Power => ch.power,
                    ObjProp::Toughness => ch.toughness,
                    ObjProp::ManaValue => ch.mv as i32,
                    ObjProp::NumColors => ch.colors.count() as i32,
                    ObjProp::ManaSpent => s.obj(o).cast_mana as i32,
                    ObjProp::Counters(k) => s.obj(o).counter(*k),
                }
            });
            if *max {
                vals.max().unwrap_or(0)
            } else {
                crate::num::sum(vals)
            }
        }
    }
}

fn first_target_obj(c: &EvalCtx) -> Option<ObjId> {
    c.targets.iter().find_map(|t| if let Target::Obj(o) = t { Some(*o) } else { None })
}

fn resolve_obj(r: Ref, c: &EvalCtx) -> Option<ObjId> {
    match r {
        Ref::SelfObj => Some(c.source),
        Ref::Triggered => c.triggered,
        Ref::Targeted => first_target_obj(c),
        _ => None,
    }
}

fn resolve_player(r: Ref, c: &EvalCtx) -> Option<PlayerId> {
    match r {
        Ref::You => Some(c.you),
        Ref::TargetedController => first_target_obj(c).map(|o| c.s.obj(o).controller),
        Ref::TargetedPlayer => c.targets.iter().find_map(|t| if let Target::Player(p) = t { Some(*p) } else { None }),
        _ => None,
    }
}

/// Compiles expressions in the context of one card face (for SVar lookup).
pub struct ExprCompiler<'a> {
    pub face: &'a Face,
    pub intern: &'a dyn Fn(&str) -> Option<Subtype>,
    /// Named counter kinds (`CardDb::counter_names`), for `counters_GE4_QUEST` in filters.
    pub counters: &'a [String],
}

impl<'a> ExprCompiler<'a> {
    /// A parameter value: a literal number, an SVar name, or an inline expression.
    pub fn value(&self, v: &str) -> Result<Expr, String> {
        self.value_depth(v, 0)
    }

    pub fn value_depth(&self, v: &str, depth: u32) -> Result<Expr, String> {
        if depth > 8 {
            return Err(format!("SVar recursion at {v}"));
        }
        let v = v.trim();
        if let Ok(n) = v.parse::<i32>() {
            return Ok(Expr::Num(n));
        }
        if let Some(sv) = self.face.svar(v) {
            return self.expr(&sv.value, depth + 1);
        }
        if let Some(neg) = v.strip_prefix('-') {
            if self.face.svar(neg).is_some() {
                return Ok(Expr::Times(Box::new(Expr::Num(-1)), Box::new(self.value_depth(neg, depth + 1)?)));
            }
        }
        self.expr(v, depth + 1)
    }

    pub fn filter(&self, s: &str) -> Result<Filter, String> {
        let mut f = |x: &str| (self.intern)(x);
        let counter = |name: &str| match name {
            "P1P1" => Some(CounterKind::P1P1),
            "M1M1" => Some(CounterKind::M1M1),
            "LOYALTY" => Some(CounterKind::Loyalty),
            "STUN" => Some(CounterKind::Stun),
            n => self.counters.iter().position(|c| c == n).map(|i| CounterKind::Named(i as u16)),
        };
        Filter::parse_with(s, &mut f, &|_| None, &counter)
    }

    /// Compile an expression string such as `Count$Valid Creature.YouCtrl/Plus.2`.
    pub fn expr(&self, src: &str, depth: u32) -> Result<Expr, String> {
        // Trailing math: `/Plus.N`, `/Minus.X`, `/Times.N`, `/HalfDown`.
        if let Some((base, math)) = src.rsplit_once('/') {
            if let Some(arg) = math.strip_prefix("Plus.") {
                return Ok(Expr::Plus(Box::new(self.expr(base, depth)?), Box::new(self.value_depth(arg, depth + 1)?)));
            }
            if let Some(arg) = math.strip_prefix("Minus.") {
                return Ok(Expr::Minus(Box::new(self.expr(base, depth)?), Box::new(self.value_depth(arg, depth + 1)?)));
            }
            if let Some(arg) = math.strip_prefix("Times.") {
                return Ok(Expr::Times(Box::new(self.expr(base, depth)?), Box::new(self.value_depth(arg, depth + 1)?)));
            }
            if let Some(arg) = math.strip_prefix("LimitMax.") {
                return Ok(Expr::Min(Box::new(self.expr(base, depth)?), Box::new(self.value_depth(arg, depth + 1)?)));
            }
            if let Some(arg) = math.strip_prefix("LimitMin.") {
                return Ok(Expr::Max(Box::new(self.expr(base, depth)?), Box::new(self.value_depth(arg, depth + 1)?)));
            }
            if math == "Twice" {
                return Ok(Expr::Times(Box::new(self.expr(base, depth)?), Box::new(Expr::Num(2))));
            }
            if let Some(n) = math.strip_prefix("DivideEvenlyDown.").and_then(|n| n.parse::<i32>().ok()).filter(|&n| n > 0) {
                return Ok(Expr::Ext(Box::new(crate::expr_ext::ExtExpr::X(crate::ext6::XExpr::DivDown(Box::new(self.expr(base, depth)?), n)))));
            }
            if math == "HalfDown" {
                return Ok(Expr::HalfDown(Box::new(self.expr(base, depth)?)));
            }
        }
        if let Some(n) = src.strip_prefix("Number$") {
            return self.value_depth(n, depth + 1);
        }
        // `CastSA>…`: evaluated on the spell that became this permanent; we keep how it was cast
        // on the permanent itself (`altcast::CastInfo`).
        if let Some(rest) = src.strip_prefix("CastSA>") {
            return self.expr(rest, depth + 1);
        }
        if let Some(rest) = src.strip_prefix("SVar$") {
            return self.value_depth(rest, depth + 1);
        }
        if let Ok(n) = src.parse::<i32>() {
            return Ok(Expr::Num(n));
        }
        let (head, body) = src.split_once('$').ok_or_else(|| format!("expr {src:?}"))?;
        match head {
            "Count" => self.count(body, depth),
            // `RememberedLKI`: a target remembered as it last existed (Azog; `ext5`).
            "TriggeredCard" | "Targeted" | "ParentTargeted" | "TriggerRemembered" | "RememberedLKI" if head != "TriggerRemembered" || body == "CardPower" => {
                // `TriggerRemembered`: the object a reflexive/delayed trigger remembered, carried
                // as its trigger object.
                let r = if matches!(head, "TriggeredCard" | "TriggerRemembered") { Ref::Triggered } else { Ref::Targeted };
                let prop = match body {
                    "CardPower" => ObjProp::Power,
                    "CardToughness" => ObjProp::Toughness,
                    "CardManaCost" => ObjProp::ManaValue,
                    "CardNumColors" => ObjProp::NumColors,
                    "CastTotalManaSpent" => ObjProp::ManaSpent,
                    b if b.starts_with("CardCounters.") => {
                        let name = &b["CardCounters.".len()..];
                        ObjProp::Counters(match name {
                            "P1P1" => CounterKind::P1P1,
                            "M1M1" => CounterKind::M1M1,
                            "LOYALTY" => CounterKind::Loyalty,
                            n => CounterKind::Named(self.counters.iter().position(|c| c == n).ok_or_else(|| format!("expr {src:?}"))? as u16),
                        })
                    }
                    _ => return Err(format!("expr {src:?}")),
                };
                Ok(Expr::Obj(r, prop))
            }
            "TargetedController" | "TargetedPlayer" | "PlayerCountPropertyYou" => {
                let r = match head {
                    "TargetedController" => Ref::TargetedController,
                    "TargetedPlayer" => Ref::TargetedPlayer,
                    _ => Ref::You,
                };
                let prop = match body {
                    "CardsInHand" => PlayerProp::CardsInHand,
                    "CardsInLibrary" => PlayerProp::CardsInLibrary,
                    "CardsInGraveyard" => PlayerProp::CardsInGraveyard,
                    "LifeLostThisTurn" => PlayerProp::LifeLostThisTurn,
                    "LifeGainedThisTurn" => PlayerProp::LifeGainedThisTurn,
                    _ => match crate::expr_ext::compile_ext(self, head, body, depth)? {
                        Some(e) => return Ok(e),
                        None => return Err(format!("expr {src:?}")),
                    },
                };
                Ok(Expr::Player(r, prop))
            }
            // Number of players: this engine plays two-player games.
            "PlayerCountPlayers" if body == "Amount" => Ok(Expr::Num(2)),
            // Number of opponents (two-player game: one) with a property.
            "PlayerCountRegisteredOpponents" | "PlayerCountOpponents" => match body {
                "Amount" => Ok(Expr::Num(1)),
                "HasPropertyLostLifeThisTurn" => Ok(Expr::Compare(Box::new(Expr::LifeOppsLostThisTurn), CmpOp::GE, Box::new(Expr::Num(1)), Box::new(Expr::Num(1)), Box::new(Expr::Num(0)))),
                // Highest/lowest among opponents = the one opponent's value (`ext5`).
                b => crate::ext5::compile_opponent_expr(self, b)?.ok_or_else(|| format!("expr {src:?}")),
            },
            _ => match crate::expr_ext::compile_ext(self, head, body, depth)? {
                Some(e) => Ok(e),
                None => Err(format!("expr {src:?}")),
            },
        }
    }

    fn count(&self, body: &str, depth: u32) -> Result<Expr, String> {
        if let Some(e) = crate::ext5::compile_count_pre(self, body)? {
            return Ok(e);
        }
        let zone_valid = [
            ("Valid ", ZoneSel::Battlefield),
            ("ValidGraveyard ", ZoneSel::Graveyard),
            ("ValidHand ", ZoneSel::Hand),
            ("ValidExile ", ZoneSel::Exile),
            ("ValidLibrary ", ZoneSel::Library),
        ];
        for (pre, z) in zone_valid {
            if let Some(f) = body.strip_prefix(pre) {
                if let Some((filt, agg)) = f.split_once('$') {
                    let (prop, max) = match agg {
                        "GreatestCardManaCost" => (ObjProp::ManaValue, true),
                        "GreatestCardPower" => (ObjProp::Power, true),
                        "GreatestCardToughness" => (ObjProp::Toughness, true),
                        "CardPower" => (ObjProp::Power, false),
                        "CardToughness" => (ObjProp::Toughness, false),
                        "CardManaCost" => (ObjProp::ManaValue, false),
                        "CardTypes" => return Ok(Expr::Ext(Box::new(crate::expr_ext::ExtExpr::CardTypesAmong(z, self.filter(filt.trim())?)))),
                        _ => return Err(format!("expr aggregate {agg}")),
                    };
                    return Ok(Expr::Aggregate(z, self.filter(filt.trim())?, prop, max));
                }
                return Ok(Expr::CountValid(z, self.filter(f.trim())?));
            }
        }
        if let Some(rest) = body.strip_prefix("Kicked.") {
            let mut it = rest.split('.');
            let a = it.next().and_then(|x| x.parse().ok()).ok_or("Kicked")?;
            let b = it.next().and_then(|x| x.parse().ok()).ok_or("Kicked")?;
            return Ok(Expr::Kicked(a, b));
        }
        if let Some(e) = crate::altcast::count_expr(body)? {
            return Ok(e);
        }
        if let Some(rest) = body.strip_prefix("Compare ") {
            // `Compare L <OP><R>.A.B`
            let (l, tail) = rest.split_once(' ').ok_or("Compare")?;
            let mut parts = tail.splitn(3, '.');
            let cond = parts.next().ok_or("Compare")?;
            let a = parts.next().ok_or("Compare")?;
            let b = parts.next().ok_or("Compare")?;
            let op = CmpOp::parse(&cond[..2]).ok_or("Compare op")?;
            return Ok(Expr::Compare(
                Box::new(self.value_depth(l, depth + 1)?),
                op,
                Box::new(self.value_depth(&cond[2..], depth + 1)?),
                Box::new(self.value_depth(a, depth + 1)?),
                Box::new(self.value_depth(b, depth + 1)?),
            ));
        }
        Ok(match body {
            "xPaid" => Expr::XPaid,
            "YourLifeTotal" => Expr::Player(Ref::You, PlayerProp::LifeTotal),
            "LifeYouGainedThisTurn" => Expr::Player(Ref::You, PlayerProp::LifeGainedThisTurn),
            "LifeOppsLostThisTurn" => Expr::LifeOppsLostThisTurn,
            "YourTurns" => Expr::YourTurns,
            "CardPower" => Expr::Obj(Ref::SelfObj, ObjProp::Power),
            "CastTotalManaSpent" => Expr::Obj(Ref::SelfObj, ObjProp::ManaSpent),
            "CardToughness" => Expr::Obj(Ref::SelfObj, ObjProp::Toughness),
            "InYourHand" | "CardsInYourHand" => Expr::Player(Ref::You, PlayerProp::CardsInHand),
            _ => match crate::expr_ext::compile_count_ext(self, body, depth)? {
                Some(e) => e,
                None => return Err(format!("expr Count${body}")),
            },
        })
    }

    /// Compile `ConditionPresent`/`ConditionCompare`-style pairs into a condition.
    pub fn present(&self, filter: &str, cmp: Option<&str>) -> Result<Condition, String> {
        let (op, n) = parse_cmp(cmp.unwrap_or("GE1"))?;
        Ok(Condition::Compare(Expr::CountValid(ZoneSel::Battlefield, self.filter(filter)?), op, Expr::Num(n)))
    }

    /// "If there are N or more [kind] counters on [this]" (`Card.Self+counters_<op><n>_<KIND>`,
    /// compared `GE1`/`EQ1`, i.e. "[this] matches"): information about the source, so its last
    /// known information once it left the battlefield (CR 608.2h, 113.7a; 603.4 for an
    /// intervening "if"; Malcolm, Alluring Scoundrel ruling 2023-11-10, Quest for the Nihil Stone
    /// ruling 2010-03-01), not "is [this] on the battlefield with N counters".
    /// The counter count of the source object (`Expr::Obj`: the old object keeps its counters,
    /// `lki`). `None`: not this form.
    pub fn self_counters(&self, filter: &str, cmp: Option<&str>) -> Result<Option<Condition>, String> {
        if !matches!(cmp, None | Some("GE1") | Some("EQ1")) || !filter.starts_with("Card.Self+counters_") {
            return Ok(None);
        }
        let f = self.filter(filter)?;
        let [alt] = f.alts.as_slice() else { return Ok(None) };
        if alt.base != crate::filter::Base::Card || alt.props.len() != 2 || !alt.props.contains(&crate::filter::Prop::SelfCard) {
            return Ok(None);
        }
        let Some((k, op, n)) = alt.props.iter().find_map(|p| match p {
            crate::filter::Prop::Counters(k, op, n) => Some((*k, *op, *n)),
            _ => None,
        }) else {
            return Ok(None);
        };
        Ok(Some(Condition::Compare(Expr::Obj(Ref::SelfObj, ObjProp::Counters(k)), op, Expr::Num(n))))
    }
    pub fn check_svar(&self, svar: &str, cmp: Option<&str>) -> Result<Condition, String> {
        let (op, rhs) = match cmp {
            Some(c) => {
                let op = CmpOp::parse(c.get(..2).ok_or("cmp")?).ok_or_else(|| format!("cmp {c}"))?;
                (op, self.value(&c[2..])?)
            }
            None => (CmpOp::GE, Expr::Num(1)),
        };
        Ok(Condition::Compare(self.value(svar)?, op, rhs))
    }
}

pub fn parse_cmp(s: &str) -> Result<(CmpOp, i32), String> {
    let op = CmpOp::parse(s.get(..2).ok_or("cmp")?).ok_or_else(|| format!("cmp {s}"))?;
    let n = s[2..].parse().map_err(|_| format!("cmp {s}"))?;
    Ok((op, n))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Condition {
    Compare(Expr, CmpOp, Expr),
    /// It's (not) the controller's turn.
    YourTurn(bool),
    /// Conditions on objects only known while resolving (`ConditionDefined$`); evaluated by
    /// `resolve` (always true elsewhere, and only compiled for resolution conditions).
    Defined(crate::carddb::Defined, Filter, CmpOp, i32),
    /// A trigger condition checked only when the ability triggers, not again on resolution
    /// (Forge `NoResolvingCheck$`; e.g. "∞ — [triggered ability]": the ability exists while
    /// the permanent is harnessed, CR 702.186b, but isn't an intervening "if").
    TriggerOnly(Box<Condition>),
}

pub fn holds(cond: &Condition, c: &EvalCtx) -> bool {
    match cond {
        Condition::Compare(a, op, b) => op.eval(eval(a, c), eval(b, c)),
        Condition::YourTurn(want) => (c.s.turn.active == c.you) == *want,
        Condition::Defined(..) => true,
        Condition::TriggerOnly(inner) => holds(inner, c),
    }
}
