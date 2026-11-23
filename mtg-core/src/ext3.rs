//! Long-tail mechanics, third batch: cast-from-
//! graveyard conditions, bargain (CR 702.166), finality counters (CR 122.1h), "if it would die
//! this turn, exile it instead" effects, transformed returns, Dig putting the rest on top, and
//! more. Kept in its own module to stay additive.

use crate::basics::*;
use crate::carddb::*;
use crate::engine::*;
use crate::query::*;
use crate::resolve::{objs, Exec};
use crate::state::*;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// Card-database side tables of this batch (`CardDb::lt`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LtTables {
    /// `Dig | LibraryPosition2$ 0`: the cards not taken stay on top of the library.
    pub dig_rest_top: Vec<AbilityId>,
    /// The FINALITY counter kind, if some loaded card uses it (CR 122.1h).
    pub finality: Option<CounterKind>,
    /// Power-up abilities (CR 702.193a).
    pub power_up: Vec<AbilityId>,
    /// A spell's mandatory additional cost found while compiling its ability, for the face.
    #[serde(skip)]
    pub pending_spell_cost: Option<crate::altcast::AddCost>,
    /// Cards that can bring cards from outside the game into it (their sideboards are
    /// materialized as objects only in games with one of these, see `flow::initial_state`).
    pub wish_cards: Vec<CardId>,
    #[serde(skip)]
    pub compiling_wish: bool,
}

/// Per-game state of this batch (`Ext::lt`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LtState {
    /// Permanents that are exiled instead if they would die this turn (Torch the Tower,
    /// `ReplaceDyingDefined$`); cleared as each turn begins.
    pub dies_exile: SmallVec<[ObjId; 1]>,
    /// Set while `exile_face_down` moves cards: objects entering exile arrive face down.
    pub face_down_next: bool,
    /// Players whose next spell this turn can't be countered (bitmask; cleared as turns begin).
    pub next_uncounterable: u8,
    /// Spells cast for their sneak cost: (spell, defending player, attacked planeswalker) of the
    /// returned creature (CR 702.190b); cleared as turns begin.
    pub sneak: SmallVec<[(ObjId, PlayerId, Option<ObjId>); 1]>,
}

/// A permanent spell cast with sneak became `nid` (CR 702.190b): it enters tapped and
/// attacking what the returned creature attacked (if it's still combat).
pub fn sneak_entered(cx: &mut Ctx, spell: ObjId, nid: ObjId) {
    let Some(i) = cx.s.ext.lt.sneak.iter().position(|x| x.0 == spell) else { return };
    let (_, def, pw) = cx.s.ext.lt.sneak.remove(i);
    if cx.s.obj(nid).zone != Zone::Battlefield {
        return;
    }
    cx.s.obj_mut(nid).tapped = true;
    if (Step::DeclareAttackers..=Step::EndCombat).contains(&cx.s.turn.step) && cx.s.obj(nid).controller == cx.s.turn.active {
        cx.s.combat.attackers.push((nid, def));
        if let Some(pw) = pw.filter(|&pw| cx.s.obj(pw).zone == Zone::Battlefield) {
            cx.s.combat.attacking_pw.push((nid, pw));
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LtExpr {
    /// Mana value of the spell that caused this trigger, X included while it is on the stack
    /// (CR 202.3e); as it last existed if it left (its X is then not known: counted as 0).
    TriggeredSpellMv,
    /// 1 if the trigger object (an attacker) attacks a player (not a planeswalker) who has the
    /// most life or is tied for it.
    AttacksPlayerWithMostLife,
    /// 1 if the evaluating player has the most life or is tied for it.
    YouHaveMostLife,
    /// Spells the evaluating player cast this turn.
    SpellsCastThisTurn,
}

pub fn eval_expr(e: &LtExpr, c: &crate::expr::EvalCtx) -> i32 {
    let most = |p: PlayerId| c.s.player(p).life >= c.s.player(p.opp()).life;
    match e {
        LtExpr::AttacksPlayerWithMostLife => {
            let Some(a) = c.triggered else { return 0 };
            let attacking_pw = c.s.combat.attacking_pw.iter().any(|x| x.0 == a);
            c.s.combat.attackers.iter().find(|x| x.0 == a).is_some_and(|&(_, d)| !attacking_pw && most(d)) as i32
        }
        LtExpr::YouHaveMostLife => most(c.you) as i32,
        LtExpr::SpellsCastThisTurn => c.s.turn_stats.spells_cast[c.you.idx()] as i32,
        // The spell's current mana value: `chars` already counts X on the stack (CR 202.3e,
        // from the face it was cast as). Adding X here again would count it twice (Thor, God of
        // Thunder / Dancing from Dark to Dawn with an X spell).
        LtExpr::TriggeredSpellMv => c.triggered.map_or(0, |o| chars(c.s, c.db, o).mv as i32),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LtStatic {
    /// Matching creatures can't be blocked by more than one creature (Forge `MinMaxBlocker |
    /// Max$ 1`; CR 509.1b restriction).
    MaxOneBlocker { valid: crate::filter::Filter },
    /// "If one or more tokens would be created under your control, twice that many of those
    /// tokens are created instead." (CR 614.1a; several apply in turn, 616.1).
    DoubleTokens,
    /// "[Matching permanents] can be the targets of spells and abilities [you control] as though
    /// they didn't have hexproof" (CR 702.11; Forge `IgnoreHexproof`).
    IgnoreHexproof { valid: crate::filter::Filter },
    /// "Ward abilities of [matching permanents] don't trigger" (Forge `DisableTriggers |
    /// ValidTrigger$ Triggered.Ward`).
    NoWard { valid: crate::filter::Filter },
}

/// May `by` target `id` as though it didn't have hexproof?
pub fn ignores_hexproof(s: &State, db: &CardDb, id: ObjId, by: PlayerId) -> bool {
    crate::static_ext::active_lt(s, db).any(|(src, you, _, x)| matches!(x, LtStatic::IgnoreHexproof { valid } if you == by && obj_matches(s, db, valid, id, FilterCtx { you, source: Some(src), x: 0, rem: &[] })))
}

/// Don't the ward abilities of `id` trigger?
pub fn ward_disabled(s: &State, db: &CardDb, id: ObjId) -> bool {
    crate::static_ext::active_lt(s, db).any(|(src, you, _, x)| matches!(x, LtStatic::NoWard { valid } if obj_matches(s, db, valid, id, FilterCtx { you, source: Some(src), x: 0, rem: &[] })))
}

/// Can attacker `a` be blocked by at most one creature?
pub fn max_one_blocker(s: &State, db: &CardDb, a: ObjId) -> bool {
    crate::static_ext::active_lt(s, db).any(|(src, you, _, x)| match x {
        LtStatic::MaxOneBlocker { valid } => obj_matches(s, db, valid, a, FilterCtx { you, source: Some(src), x: 0, rem: &[] }),
        _ => false,
    })
}

/// How many tokens are created when `n` would be created under `p`'s control (token doublers).
pub fn token_count(s: &State, db: &CardDb, p: PlayerId, n: i32) -> i32 {
    let k = crate::static_ext::active_lt(s, db).filter(|(_, you, _, x)| *you == p && matches!(x, LtStatic::DoubleTokens)).count();
    // Never wraps (`num`); `zones::create_tokens_with` fails loudly past `MAX_TOKENS_AT_ONCE`.
    (0..k.min(32)).fold(n.max(0), |m, _| crate::num::mul(m, 2))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LtEffect {
    /// "If [the affected permanents] would die this turn, exile [them] instead" (CR 614.1a).
    ExileIfDiesThisTurn,
    /// Put the defined cards onto the battlefield transformed under their owner's control
    /// (CR 712.14, Forge `ChangeZone | Transformed$ True`); `WithCountersType$` counters are applied as it
    /// enters (`resolve::apply_node`, CR 122.6).
    PutTransformed,
    /// Put one counter of each kind on the affected permanents (`PutCounter | CounterTypes$`).
    PutCounters { kinds: Vec<CounterKind> },
    /// Put the counters that are on `from` (as it last existed) on the affected permanents
    /// (`PutCounter | CounterType$ EachFromSource`), each kind and number.
    PutCountersFrom { from: Defined },
    /// The affected players exile the top N cards of their library face down without looking
    /// at them (Doomsday Excruciator; Forge `Dig | ExileFaceDown$ True | ChangeNum$ All`).
    ExileTopFaceDown { n: crate::expr::Expr },
    /// "The next spell you cast this turn can't be countered." (CR 101.2).
    NextSpellUncounterable,
    /// "You may put a card you own from outside the game into your hand" (North Wind Avatar):
    /// choose up to `n` cards of your sideboard (CR 400.11a), put them into your hand unrevealed.
    WishToHand { n: u8 },
    /// "You may reveal exactly `n` cards you own with different names from outside the game"
    /// (Extrapolate the Impossible): they are revealed and remembered.
    RevealFromOutside { n: u8 },
    /// "An opponent chooses one of them. You put that card into your hand." (the remembered
    /// revealed cards).
    OpponentChoosesToHand,
    /// Create an Aura token (a Role) attached to the defined/targeted creature (Forge `Token |
    /// AttachedTo$ Targeted`).
    TokenAttached { token: CardId },
    /// Transform the source permanent (CR 701.28; Forge `SetState | Defined$ Self | Mode$
    /// Transform`).
    TransformSelf,
    /// "Each player creates N [token]" (`Token | TokenOwner$ Player`), in APNAP order.
    TokenEachPlayer { token: CardId, n: crate::expr::Expr },
}

/// CR 701.28a: turn a transforming double-faced permanent over. `ability_card`: the card whose
/// ability does it; per 701.28f an ability of the permanent transforms it only if it hasn't
/// transformed since (approximated: the face with that ability is still up).
pub fn transform(cx: &mut Ctx, o: ObjId, ability_card: CardId) {
    if cx.s.obj(o).zone != Zone::Battlefield || cx.s.obj(o).card != ability_card {
        return;
    }
    if let Some(i) = cx.s.copy_effects.iter().position(|c| c.obj == o && c.transformed) {
        // Back to the front face.
        let printed = cx.s.copy_effects.remove(i).printed;
        let back = cx.s.obj(o).card;
        cx.s.obj_mut(o).card = printed;
        cx.s.emit(Event::Transformed { obj: o, card: printed, face_card: printed });
        let _ = back;
        crate::ext6::transformed(cx, o);
    } else if let Some(back) = cx.db.back_of(cx.s.obj(o).card) {
        if cx.s.copy_effects.iter().any(|c| c.obj == o) {
            return; // a copy of a DFC isn't transformed here (not in the pool)
        }
        let printed = cx.s.obj(o).card;
        cx.s.copy_effects.push(CopyEffect { obj: o, printed, name: None, add_types: TypeSet::default(), add_subtypes: SmallVec::new(), kws: crate::carddb::KwSet::default(), set_pt: None, transformed: true, add_super: 0 });
        cx.s.obj_mut(o).card = back;
        cx.s.emit(Event::Transformed { obj: o, card: printed, face_card: back });
        crate::ext6::transformed(cx, o);
    }
}

/// CR 303.7a: if a permanent has more than one Role controlled by the same player attached to
/// it, each of those Roles except the one with the most recent timestamp is put into its
/// owner's graveyard (a state-based action). Returns those Roles.
pub fn roles_to_graveyard(s: &State, db: &CardDb) -> SmallVec<[ObjId; 1]> {
    let mut out = SmallVec::new();
    let Some(role) = db.subtype("Role") else { return out };
    let roles: SmallVec<[ObjId; 4]> = s.battlefield.iter().copied().filter(|&o| s.obj(o).attached_to.is_some() && chars(s, db, o).subtypes.contains(&role)).collect();
    for &r in &roles {
        let (t, c, ts) = (s.obj(r).attached_to, s.obj(r).controller, s.obj(r).ts);
        if roles.iter().any(|&q| q != r && s.obj(q).attached_to == t && s.obj(q).controller == c && s.obj(q).ts > ts) {
            out.push(r);
        }
    }
    out
}

/// A spell `so` was just cast by `p` (put on the stack, CR 601.2i): effects waiting for "the next
/// spell you cast this turn" apply to it.
pub fn spell_cast(cx: &mut Ctx, p: PlayerId, so: ObjId) {
    let bit = 1u8 << p.0;
    if cx.s.ext.lt.next_uncounterable & bit != 0 {
        cx.s.ext.lt.next_uncounterable &= !bit;
        if !cx.s.ext.uncounterable.contains(&so) {
            cx.s.ext.uncounterable.push(so);
        }
    }
}

pub fn apply(cx: &mut Ctx, ex: &mut Exec, ad: &AbilityDef, x: &LtEffect, _tg: &[Target], who: &[Target]) -> R<()> {
    match x {
        LtEffect::ExileIfDiesThisTurn => {
            for o in objs(who) {
                if cx.s.obj(o).zone == Zone::Battlefield && !cx.s.ext.lt.dies_exile.contains(&o) {
                    cx.s.ext.lt.dies_exile.push(o);
                }
            }
        }
        LtEffect::PutTransformed => {
            let mut new_ids: SmallVec<[ObjId; 2]> = SmallVec::new();
            for o in objs(who) {
                let z = cx.s.obj(o).zone;
                if !matches!(z, Zone::Exile | Zone::Graveyard | Zone::Hand) {
                    continue;
                }
                let owner = cx.s.obj(o).owner;
                if let Some(nid) = crate::zones::move_special(cx, o, owner, None, true)? {
                    let (card, front) = (cx.s.obj(nid).card, cx.s.obj(o).card);
                    cx.s.emit(Event::Transformed { obj: nid, card: front, face_card: card });
                    new_ids.push(nid);
                }
            }
            ex.moved = new_ids.clone();
            if ad.remember {
                ex.remembered.extend(new_ids);
            }
        }
        LtEffect::TokenAttached { token } => {
            // `AttachedTo$ Targeted`: the target of the previous targeted part.
            let _ = who;
            ex.moved.clear();
            for o in objs(&ex.last_targets.clone()) {
                // CR 303.4g-like: no legal object to enchant, no token.
                let enchant = cx.db.get(*token).front().enchant.clone();
                let ok = cx.s.obj(o).zone == Zone::Battlefield && enchant.is_some_and(|f| obj_matches(cx.s, cx.db, &f, o, FilterCtx { source: None, x: 0, ..exec_fcx!(ex) }));
                if ok {
                    // Every token created, doubled ones included (CR 614.1a).
                    let ids = crate::zones::create_token_attached(cx, *token, ex.you, o)?;
                    ex.moved.extend(ids.iter().copied());
                    if ad.remember {
                        ex.remembered.extend(ids);
                    }
                }
            }
        }
        LtEffect::NextSpellUncounterable => {
            cx.s.ext.lt.next_uncounterable |= 1 << ex.you.0;
        }
        LtEffect::TransformSelf => transform(cx, ex.source, ex.source_card),
        LtEffect::TokenEachPlayer { token, n } => {
            let k = crate::resolve::amount(cx, ex, n);
            let a = cx.s.turn.active;
            for p in [a, a.opp()] {
                crate::zones::create_tokens(cx, *token, p, k, false)?;
            }
        }
        LtEffect::WishToHand { n } => {
            let p = ex.you;
            // One option per distinct card (identical cards are interchangeable).
            let items = crate::query::card_items(cx.s, cx.db, &cx.s.player(p).sideboard);
            let chosen = cx.choose_set(p, DecisionKind::Search, items, 0, *n as usize, Some(ex.source))?;
            let moves: Vec<crate::zones::Move> = chosen.iter().filter_map(|c| if let Opt::Card(o) = c { Some(crate::zones::Move::new(*o, Zone::Hand)) } else { None }).collect();
            let new = crate::zones::move_batch(cx, &moves)?;
            if ad.remember {
                ex.remembered.extend(new.into_iter().flatten());
            }
        }
        LtEffect::RevealFromOutside { n } => {
            let p = ex.you;
            // Different names: one candidate per card name (CardId).
            let mut cands: Vec<ObjId> = vec![];
            for &o in &cx.s.player(p).sideboard {
                if !cands.iter().any(|&c| cx.s.obj(c).card == cx.s.obj(o).card) {
                    cands.push(o);
                }
            }
            // "You may reveal exactly N": if that many names exist, choose whether to.
            if cands.len() < *n as usize || cx.choose(Decision { player: p, kind: DecisionKind::OptionalEffect, options: vec![Opt::Yes, Opt::No], subject: Some(ex.source) })? == Opt::No {
                return Ok(());
            }
            let items: Vec<(u32, Opt)> = cands.iter().map(|&o| (cx.s.obj(o).card.0, Opt::Card(o))).collect();
            for c in cx.choose_set(p, DecisionKind::Search, items, *n as usize, *n as usize, Some(ex.source))? {
                if let Opt::Card(o) = c {
                    cx.s.obj_mut(o).known_to = 0b11; // revealed
                    ex.remembered.push(o);
                }
            }
        }
        LtEffect::OpponentChoosesToHand => {
            let opts: Vec<Opt> = ex.remembered.iter().copied().filter(|&o| cx.s.obj(o).zone == Zone::Sideboard).map(Opt::Card).collect();
            if opts.is_empty() {
                return Ok(());
            }
            let Opt::Card(o) = cx.choose(Decision { player: ex.you.opp(), kind: DecisionKind::ChooseCard, options: opts, subject: Some(ex.source) })? else { unreachable!() };
            if let Some(nid) = crate::zones::move_to(cx, o, Zone::Hand)? {
                cx.s.obj_mut(nid).known_to = 0b11; // it was revealed
            }
        }
        LtEffect::ExileTopFaceDown { n } => {
            for p in crate::resolve::players(who) {
                let k = crate::resolve::amount(cx, ex, n).max(0) as usize;
                let top: Vec<crate::zones::Move> = cx.s.player(p).library.iter().rev().take(k).map(|&o| crate::zones::Move::new(o, Zone::Exile)).collect();
                exile_face_down(cx, &top)?;
            }
        }
        LtEffect::PutCounters { kinds } => {
            for o in objs(who) {
                if cx.s.obj(o).zone == Zone::Battlefield {
                    for &k in kinds {
                        crate::ext6::put_counters(cx.s, cx.db, o, k, 1);
                    }
                }
            }
        }
        LtEffect::PutCountersFrom { from } => {
            // Last known information: the trigger object as it last existed (CR 608.2h).
            let src = if *from == Defined::Triggered { ex.trig.obj } else { crate::effect_ext::defined_objs(cx, ex, *from).first().copied() };
            let Some(src) = src else { return Ok(()) };
            let counters = cx.s.obj(src).counters.clone();
            for o in objs(who) {
                if cx.s.obj(o).zone == Zone::Battlefield && o != src {
                    for &(k, n) in &counters {
                        crate::ext6::put_counters(cx.s, cx.db, o, k, n);
                    }
                }
            }
        }
    }
    Ok(())
}

/// CR 702.193a–b: a power-up ability's cost is reduced by the permanent's mana cost if it entered
/// this turn: generic by generic, colored/colorless by the same type, any excess by generic.
pub fn power_up_reduce(cx: &Ctx, o: ObjId, mut req: crate::cast::Req) -> crate::cast::Req {
    let ob = cx.s.obj(o);
    if ob.zone != Zone::Battlefield || ob.entered != cx.s.turn.number {
        return req;
    }
    let mut excess = 0u32;
    for sym in &cx.db.get(ob.card).face(ob.face).cost.syms {
        let slot = match *sym {
            ManaSym::Generic(n) => {
                excess += n as u32;
                continue;
            }
            ManaSym::Colored(c) => c as usize,
            ManaSym::Colorless => 5,
            // Checked at compile time (`lt_power_up_ok`): other symbols don't occur.
            _ => continue,
        };
        if req.exact[slot] > 0 {
            req.exact[slot] -= 1;
        } else {
            excess += 1;
        }
    }
    req.generic = req.generic.saturating_sub(excess);
    req
}

/// Move cards to exile face down (CR 406.3): nobody may look at them (`known_to` 0 from the
/// moment they arrive, so triggers and observations never see their characteristics, 406.3a).
/// A redirected move (to another zone) is unaffected.
pub fn exile_face_down(cx: &mut Ctx, moves: &[crate::zones::Move]) -> R<Vec<Option<ObjId>>> {
    cx.s.ext.lt.face_down_next = true;
    let r = crate::zones::move_batch(cx, moves);
    cx.s.ext.lt.face_down_next = false;
    r
}

/// Replacement effects of this batch on a move from the battlefield to `dest` (CR 614): a
/// finality counter (CR 122.1h) or a "dies this turn" exile effect sends it to exile instead.
pub fn redirect(s: &State, db: &CardDb, obj: ObjId, from: Zone, dest: Zone) -> Zone {
    if from != Zone::Battlefield || dest != Zone::Graveyard {
        return dest;
    }
    if db.lt.finality.is_some_and(|k| s.obj(obj).counter(k) > 0) {
        return Zone::Exile;
    }
    // "Would die": a creature (or planeswalker) put into a graveyard from the battlefield
    // (CR 700.4); the effects here are only created for creatures/planeswalkers dealt damage.
    if s.ext.lt.dies_exile.contains(&obj) {
        let c = chars(s, db, obj);
        if c.types.has(CardType::Creature) || c.types.has(CardType::Planeswalker) {
            return Zone::Exile;
        }
    }
    dest
}

/// Dig: the cards not taken (`others`) stay on top of `p`'s library (`LibraryPosition2$ 0`).
/// More than one would need an ordering decision, which isn't implemented. `reveal`: the card
/// was revealed ("Reveal the top card of your library. If it's a ..."), so it stays known.
pub fn dig_rest_on_top(cx: &mut Ctx, p: PlayerId, others: &[ObjId], reveal: bool) -> R<()> {
    if others.len() > 1 {
        return cx.unsupported("Dig: ordering several cards put back on top");
    }
    if reveal {
        for &o in others {
            cx.s.obj_mut(o).known_to = 0b11;
        }
    }
    let lib = &mut cx.s.player_mut(p).library;
    lib.retain(|o| !others.contains(o));
    lib.extend(others.iter().copied());
    Ok(())
}

// ---------------------------------------------------------------- compiling

impl CardDb {
    /// `ChangeZone` forms of this batch (`None`: not one of them; keys untouched).
    pub(crate) fn lt_change_zone(&mut self, p: &crate::compile::P, _f: &forge_script::Face, origin: ZoneSet, dest: Zone) -> Result<Option<Effect>, String> {
        if p.peek("Transformed").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
            p.matched("Transformed", |v| v.eq_ignore_ascii_case("true"), "checked above");
            if dest != Zone::Battlefield || p.peek("Defined").is_none() || p.peek("ValidTgts").is_some() || origin.has(Zone::Library) || origin.has(Zone::Battlefield) {
                return Err("ChangeZone Transformed form".into());
            }
            // `WithCountersType$` is the node's `enter_counters` (`compile`): applied as it enters.
            if p.has("GainControl") || p.has("Tapped") {
                return Err("ChangeZone Transformed form".into());
            }
            return Ok(Some(Effect::Ext(crate::effect_ext::ExtEffect::Lt(LtEffect::PutTransformed))));
        }
        Ok(None)
    }

    /// `ChangeZone | Origin$ Sideboard` ("wishes", CR 400.11): North Wind Avatar's "put a card
    /// you own from outside the game into your hand" and Extrapolate the Impossible's reveal /
    /// opponent's choice. Other forms are unsupported.
    pub(crate) fn lt_wish(&mut self, p: &crate::compile::P, _f: &forge_script::Face) -> Result<Effect, String> {
        if p.get("Origin") != Some("Sideboard") || !p.flag("Hidden") {
            return Err("ChangeZone from outside the game: form".into());
        }
        self.lt.compiling_wish = true;
        let n = p.get("ChangeNum").unwrap_or("1").parse::<u8>().map_err(|_| "ChangeZone Sideboard ChangeNum")?;
        let e = match (p.get("Destination"), p.get("Chooser"), p.get("ChangeType")) {
            (Some("Hand"), None, Some("Card.YouOwn")) if !p.flag("Mandatory") => LtEffect::WishToHand { n },
            (None, None, Some("Card.YouOwn")) if p.flag("Exactly") && p.flag("DifferentNames") && p.flag("Reveal") && p.flag("RememberChanged") => LtEffect::RevealFromOutside { n },
            (Some("Hand"), Some("Opponent"), Some("Card.IsRemembered")) if n == 1 && p.flag("Mandatory") => LtEffect::OpponentChoosesToHand,
            _ => return Err("ChangeZone from outside the game: form".into()),
        };
        Ok(Effect::Ext(crate::effect_ext::ExtEffect::Lt(e)))
    }

    /// Conditions on the trigger event, checked only as it triggers (`Condition::TriggerOnly`):
    /// `Attacks | Attacked$ Player.withMostLife` ("attacks the player with the most life or tied
    /// for most life") and `CheckDefinedPlayer$ You.withMostLife` ("while you have the most life
    /// or are tied for most life").
    pub(crate) fn lt_trigger_conditions(&self, mode: &str, p: &crate::compile::P) -> Result<Vec<crate::expr::Condition>, String> {
        use crate::expr::{Condition, Expr};
        let mut out = vec![];
        let flag = |e: LtExpr| Condition::TriggerOnly(Box::new(Condition::Compare(Expr::Ext(Box::new(crate::expr_ext::ExtExpr::Lt(e))), crate::filter::CmpOp::EQ, Expr::Num(1))));
        if mode == "Attacks" {
            match p.get("Attacked") {
                None => {}
                Some("Player.withMostLife") => out.push(flag(LtExpr::AttacksPlayerWithMostLife)),
                Some(a) => return Err(format!("Attacks Attacked {a}")),
            }
        }
        if mode == "SpellCast" {
            // "Whenever you cast your second spell each turn" (`ActivatorThisTurnCast$ EQ2`): the
            // caster's spells this turn, this one included.
            if let Some(c) = p.get("ActivatorThisTurnCast") {
                let (op, n) = crate::expr::parse_cmp(c)?;
                if p.peek("ValidActivatingPlayer") != Some("You") {
                    // "Whenever an opponent casts their first noncreature spell each turn" (The
                    // Queen of Dale; `ext7`): that player's spells this turn matching `ValidCard`
                    // (Forge `TriggerSpellAbilityCastOrCopy`), this one included.
                    let valid = self.filter(p.peek("ValidCard").unwrap_or("Card"))?;
                    out.push(Condition::TriggerOnly(Box::new(Condition::Compare(Expr::Ext(Box::new(crate::expr_ext::ExtExpr::ThisTurnCastByTriggeredCaster(valid))), op, Expr::Num(n)))));
                    return Ok(out);
                }
                out.push(Condition::TriggerOnly(Box::new(Condition::Compare(Expr::Ext(Box::new(crate::expr_ext::ExtExpr::Lt(LtExpr::SpellsCastThisTurn))), op, Expr::Num(n)))));
            }
        }
        match p.get("CheckDefinedPlayer") {
            None => {}
            // With `NoResolvingCheck$` (required): compile_trigger makes it trigger-time only.
            Some("You.withMostLife") if p.peek("NoResolvingCheck").is_some() => out.push(Condition::Compare(Expr::Ext(Box::new(crate::expr_ext::ExtExpr::Lt(LtExpr::YouHaveMostLife))), crate::filter::CmpOp::EQ, Expr::Num(1))),
            Some(c) => return Err(format!("CheckDefinedPlayer {c}")),
        }
        Ok(out)
    }

    /// `Token | TokenScript$ X | TokenOwner$ Player`: each player creates the tokens.
    pub(crate) fn lt_token_each_player(&mut self, p: &crate::compile::P, f: &forge_script::Face) -> Result<Effect, String> {
        let script = p.get("TokenScript").ok_or("Token without TokenScript")?;
        let token = self.token(script)?;
        if !self.get(token).playable() {
            return Err(format!("token {script}: {:?}", self.get(token).unsupported));
        }
        p.ack("TokenOwner", &["Player"], "each player (checked by the caller)");
        let n = match p.get("TokenAmount") {
            Some(v) => self.value(f, v)?,
            None => crate::expr::Expr::Num(1),
        };
        Ok(Effect::Ext(crate::effect_ext::ExtEffect::Lt(LtEffect::TokenEachPlayer { token, n })))
    }

    /// `Token | TokenScript$ <aura> | AttachedTo$ Targeted`: an Aura token attached to the target.
    pub(crate) fn lt_token_attached(&mut self, p: &crate::compile::P) -> Result<Effect, String> {
        let script = p.get("TokenScript").ok_or("Token without TokenScript")?;
        let token = self.token(script)?;
        if !self.get(token).playable() {
            return Err(format!("token {script}: {:?}", self.get(token).unsupported));
        }
        if p.get("AttachedTo") != Some("Targeted") || p.get("TokenOwner").is_some_and(|o| o != "You") || p.get("TokenAmount").is_some_and(|n| n != "1") || self.get(token).front().enchant.is_none() {
            return Err("Token AttachedTo form".into());
        }
        Ok(Effect::Ext(crate::effect_ext::ExtEffect::Lt(LtEffect::TokenAttached { token })))
    }

    /// Effect APIs of this batch (`None`: not one of them; keys untouched).
    pub(crate) fn lt_api(&mut self, api: &str, p: &crate::compile::P, f: &forge_script::Face) -> Result<Option<Effect>, String> {
        Ok(Some(Effect::Ext(crate::effect_ext::ExtEffect::Lt(match api {
            // "The next spell you cast this turn can't be countered." (Mistrise Village): Forge's
            // delayed SpellCast trigger creating a `CantHappen` counter replacement on that spell.
            "SetState" if p.peek("Mode") == Some("Transform") && p.peek("Defined") == Some("Self") => {
                p.ack("Mode", &["Transform"], "checked above");
                LtEffect::TransformSelf
            }
            "DelayedTrigger" if p.peek("Mode") == Some("SpellCast") && p.peek("ThisTurn").is_some() => {
                let exec = p.get("Execute").ok_or("DelayedTrigger without Execute")?;
                let body = f.svar(exec).and_then(|s| s.as_ability()).ok_or("DelayedTrigger Execute missing")?;
                let repl = body.get("ReplacementEffects").and_then(|r| f.svar(r)).map(|sv| forge_script::Params::parse(&sv.value));
                let anti_magic = body.api() == Some(("DB", "Effect"))
                    && body.get("RememberObjects") == Some("TriggeredCard")
                    && repl.as_ref().is_some_and(|r| r.get("Event") == Some("Counter") && r.get("ValidCard") == Some("Card.IsRemembered") && r.get("Layer") == Some("CantHappen"));
                if !anti_magic || p.get("ValidCard") != Some("Card") || p.get("ValidActivatingPlayer") != Some("You") || !p.flag("ThisTurn") {
                    return Err("DelayedTrigger form".into());
                }
                p.ack("Mode", &["SpellCast"], "checked above");
                // `Static$ True`: the delayed trigger acts without using the stack, as this does.
                p.ack("Static", &["True"], "applied as the spell is cast");
                LtEffect::NextSpellUncounterable
            }
            _ => return Ok(None),
        }))))
    }

    /// `Dig | DigNum$ X | ChangeNum$ All | DestinationZone$ Exile | ExileFaceDown$ True |
    /// NoReveal$ True`: exile the top X cards face down without looking (Doomsday Excruciator).
    pub(crate) fn lt_dig_face_down(&mut self, p: &crate::compile::P, f: &forge_script::Face) -> Result<Effect, String> {
        if !p.flag("ExileFaceDown") || p.get("ChangeNum") != Some("All") || p.get("DestinationZone") != Some("Exile") || !p.flag("NoReveal") {
            return Err("Dig ExileFaceDown form".into());
        }
        let n = self.value(f, p.get("DigNum").ok_or("Dig without DigNum")?)?;
        Ok(Effect::Ext(crate::effect_ext::ExtEffect::Lt(LtEffect::ExileTopFaceDown { n })))
    }

    /// Static modes of this batch (`None`: not one of them; keys untouched).
    pub(crate) fn lt_static(&mut self, mode: &str, p: &crate::compile::P, f: &forge_script::Face) -> Result<Option<LtStatic>, String> {
        Ok(Some(match mode {
            "MinMaxBlocker" => {
                if p.get("Max") != Some("1") || p.peek("Min").is_some() {
                    return Err("MinMaxBlocker other than Max 1".into());
                }
                LtStatic::MaxOneBlocker { valid: self.filter_f(f, p.get("ValidCard").unwrap_or("Card"))? }
            }
            "IgnoreHexproof" => {
                if p.get("Activator").is_some_and(|a| a != "You") {
                    return Err("IgnoreHexproof for other players".into());
                }
                LtStatic::IgnoreHexproof { valid: self.filter_f(f, p.get("ValidEntity").ok_or("IgnoreHexproof without ValidEntity")?)? }
            }
            "DisableTriggers" if p.peek("ValidTrigger") == Some("Triggered.Ward") => {
                p.ack("ValidTrigger", &["Triggered.Ward"], "checked above");
                LtStatic::NoWard { valid: self.filter_f(f, p.get("ValidCard").unwrap_or("Card"))? }
            }
            _ => return Ok(None),
        }))
    }

    /// `PutCounter | CounterTypes$ A,B` and `PutCounter | CounterType$ EachFromSource`.
    pub(crate) fn lt_put_counter(&mut self, p: &crate::compile::P, _f: &forge_script::Face) -> Result<Effect, String> {
        let e = if let Some(ts) = p.get("CounterTypes") {
            if p.get("CounterNum").is_some_and(|n| n != "1") {
                return Err("PutCounter CounterTypes with CounterNum".into());
            }
            let kinds = ts.split(',').map(|t| self.counter_kind(t.trim())).collect();
            LtEffect::PutCounters { kinds }
        } else {
            p.ack("CounterType", &["EachFromSource"], "checked by the caller");
            let from = self.defined(p.get("EachFromSource").ok_or("EachFromSource without source")?)?;
            LtEffect::PutCountersFrom { from }
        };
        Ok(Effect::Ext(crate::effect_ext::ExtEffect::Lt(e)))
    }

    /// Record that a counter kind with rules meaning in this batch was interned.
    pub(crate) fn lt_note_counter(&mut self, k: CounterKind, name: &str) {
        if name == "FINALITY" {
            self.lt.finality = Some(k);
        }
    }
}
