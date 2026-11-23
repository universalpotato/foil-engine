//! Long-tail mechanics, second batch:
//! empower (CR 701.71), loyalty abilities at instant speed,
//! exhaust (CR 702.177), divided counters (CR 601.2d), "unless you discard a [type]" discards,
//! casting copies without paying their mana cost (CR 707.12, 608.2g), committing crimes
//! (CR 700.13), granted flashback, and more. Kept in its own module to stay additive.

use crate::basics::*;
use crate::carddb::*;
use crate::engine::*;
use crate::expr::Expr;
use crate::filter::Filter;
use crate::query::*;
use crate::resolve::{objs, Exec};
use crate::state::*;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MoreEffect {
    /// CR 701.71a: "If you don't control a [kind] planeswalker token, create one (`token`); choose
    /// a [kind] planeswalker token you control; put N loyalty counters on it."
    Empower { token: CardId, kind: Subtype, n: Expr },
    /// "Discard N cards unless you discard a [type] card" (Forge `Discard | UnlessType$`): the
    /// player either discards one matching card or N cards.
    DiscardUnless { n: Expr, unless: Filter },
    /// "You may cast any number of spells [with total mana value N or less] from among copies of
    /// the matching cards in `zone` without paying their mana costs" (Forge `Play | CopyCard$ |
    /// WithoutManaCost$ | Amount$ All`; CR 707.12, 608.2g). Permanent spells become tokens.
    CastCopies { valid: Filter, zone: Zone, total_mv: Option<Expr> },
    /// `chooser` looks through `origin` (of `owner`'s, or every player's for a public zone) and
    /// chooses matching cards (`num`, or any number) to put into `dest` ("search its owner's
    /// graveyard, hand, and library for any number of cards with that name", CR 701.19).
    /// `imprint`: also remembered as imprinted (`Imprint$`).
    SearchZone { origin: Zone, dest: Zone, change_type: Filter, num: Option<Expr>, owner: Option<Defined>, chooser: Defined, mandatory: bool, shuffle: bool, imprint: bool },
    /// The permanents the previous `Animate` node affected gain these triggered abilities
    /// (indices into `CardDb::triggers_pool`; Forge `Animate | Triggers$`), layer 6.
    GrantTriggers { triggers: Vec<u32>, duration: Duration },
    /// The targeted cards in a graveyard gain flashback until end of turn, with a flashback cost
    /// equal to their mana cost (Flashback, the card; CR 702.34a).
    GrantFlashback,
    /// Put the counters divided among the targets as the spell was cast (CR 601.2d,
    /// `State::ext.divided`); `total` is what was divided.
    PutCounterDivided { kind: CounterKind, total: Expr },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MoreStatic {
    /// "You may activate loyalty abilities of matching planeswalkers you control on any player's
    /// turn any time you could cast an instant" (Forge `CastWithFlash | ValidSA$ Activated.Loyalty`).
    LoyaltyAtInstantSpeed { valid: Filter },
}

/// Whose zone a `SearchZone` searches: the defined players (none if defined by a remembered
/// object that no longer exists), or both players' graveyards.
pub(crate) fn search_zone_owners(cx: &Ctx, ex: &Exec, owner: Option<Defined>) -> SmallVec<[PlayerId; 2]> {
    match owner {
        Some(d) if d == Defined::RememberedController && ex.remembered.is_empty() => SmallVec::new(),
        Some(d) => crate::effect_ext::defined_players(cx, ex, d),
        None => smallvec::smallvec![ex.you, ex.you.opp()],
    }
}

/// Every object in `o_p`'s `origin` zone (exile: the ones `o_p` owns).
fn search_zone_all(s: &State, origin: Zone, o_p: PlayerId) -> Vec<ObjId> {
    match origin {
        Zone::Graveyard => s.player(o_p).graveyard.clone(),
        Zone::Hand => s.player(o_p).hand.clone(),
        Zone::Library => s.player(o_p).library.clone(),
        Zone::Exile => s.exile.iter().copied().filter(|&o| s.obj(o).owner == o_p).collect(),
        _ => vec![],
    }
}

/// The cards of `o_p`'s `origin` zone a `SearchZone` may choose: matching cards (tokens there,
/// just died and not yet gone, aren't cards: `query::is_card`). Shared by the
/// search and the "may" check (`resolve::optional_choice_possible`).
pub(crate) fn search_zone_pool(cx: &Ctx, ex: &Exec, origin: Zone, change_type: &Filter, o_p: PlayerId) -> Vec<ObjId> {
    let fcx = exec_fcx!(ex);
    search_zone_all(cx.s, origin, o_p).into_iter().filter(|&o| crate::query::is_card(cx.s, o) && obj_matches(cx.s, cx.db, change_type, o, fcx)).collect()
}

pub fn apply(cx: &mut Ctx, ex: &mut Exec, _ad: &AbilityDef, x: &MoreEffect, _tg: &[Target], _who: &[Target]) -> R<()> {
    let num = |cx: &mut Ctx, ex: &Exec, e: &Expr| crate::resolve::amount(cx, ex, e);
    match x {
        MoreEffect::Empower { token, kind, n } => {
            let p = ex.you;
            let k = num(cx, ex, n);
            let is_tok = |s: &State, db: &CardDb, o: ObjId| {
                let ob = s.obj(o);
                if !ob.token || ob.controller != p {
                    return false;
                }
                let c = chars(s, db, o);
                c.types.has(CardType::Planeswalker) && c.subtypes.contains(kind)
            };
            if !cx.s.battlefield.iter().any(|&o| is_tok(cx.s, cx.db, o)) {
                crate::zones::create_tokens(cx, *token, p, 1, false)?;
            }
            let objs: Vec<ObjId> = cx.s.battlefield.iter().copied().filter(|&o| is_tok(cx.s, cx.db, o)).collect();
            let items = crate::query::sig_items(cx.s, cx.db, &objs);
            if items.is_empty() {
                return Ok(()); // e.g. the token was replaced into another zone
            }
            let Some(Opt::Card(t)) = cx.choose_set(p, DecisionKind::ChooseCard, items, 1, 1, Some(ex.source))?.into_iter().next() else { return Ok(()) };
            crate::ext6::put_counters(cx.s, cx.db, t, CounterKind::Loyalty, k);
        }
        MoreEffect::DiscardUnless { n, unless } => {
            let k = num(cx, ex, n).max(0) as usize;
            let mut ps = crate::resolve::players(_who);
            if ps.is_empty() {
                ps.push(ex.you);
            }
            for p in ps {
                let fcx = exec_fcx!(ex);
                let hand = crate::flow::hand_items(cx, p);
                let matching: Vec<(crate::query::ObjSig, Opt)> = hand.iter().copied().filter(|(_, o)| matches!(o, Opt::Card(h) if obj_matches(cx.s, cx.db, unless, *h, fcx))).collect();
                // Discarding one matching card, or N cards. With a single card in hand that
                // matches, both are the same action (not asked).
                let one = !matching.is_empty()
                    && (hand.len() <= 1 || cx.choose(Decision { player: p, kind: DecisionKind::OptionalEffect, options: vec![Opt::Yes, Opt::No], subject: Some(ex.source) })? == Opt::Yes);
                let (items, m) = if one { (matching, 1) } else { (hand, k) };
                let m = m.min(items.len());
                let chosen: Vec<ObjId> = cx.choose_set(p, DecisionKind::Discard, items, m, m, Some(ex.source))?.into_iter().filter_map(|o| if let Opt::Card(h) = o { Some(h) } else { None }).collect();
                for &h in &chosen {
                    let card = cx.s.obj(h).card;
                    cx.s.emit(Event::Discard { player: p, card });
                }
                let moves: Vec<crate::zones::Move> = chosen.iter().map(|&h| crate::zones::Move::new(h, Zone::Graveyard)).collect();
                let new = crate::zones::move_batch(cx, &moves)?;
                if _ad.remember {
                    ex.remembered.extend(new.into_iter().flatten());
                }
            }
        }
        MoreEffect::CastCopies { valid, zone, total_mv } => {
            let p = ex.you;
            let mut budget = total_mv.as_ref().map(|e| num(cx, ex, e));
            let fcx = FilterCtx { you: p, ..exec_fcx!(ex) };
            let pool: &[ObjId] = match zone {
                Zone::Exile => &cx.s.exile,
                Zone::Graveyard => &cx.s.player(p).graveyard,
                _ => return cx.unsupported("casting copies from this zone"),
            };
            let mut left: Vec<ObjId> = pool.iter().copied().filter(|&o| obj_matches(cx.s, cx.db, valid, o, fcx)).collect();
            loop {
                // One spell at a time (each goes on the stack as it is cast); each card's copy
                // at most once. Identical cards are one option (`query::card_sig`).
                let mut opts = vec![Opt::Done];
                let first = crate::query::first_of_each_card(cx.s, cx.db, &left);
                for (&o, _) in left.iter().zip(first).filter(|x| x.1) {
                    let cd = cx.db.get(cx.s.obj(o).card);
                    // A copy of a two-faced card is cast as either face (CR 707.12, 709.3c,
                    // 715.3; `faces`); the budget is the face's mana value (Room ruling:
                    // "consider only the characteristics of the door you're casting").
                    for face in crate::faces::spell_faces(cx.s, cx.db, o, crate::faces::FacePerm::Any) {
                        if budget.is_some_and(|b| cd.face(face).cost.mana_value() as i32 > b) {
                            continue;
                        }
                        // Timing is ignored while an effect resolves (CR 608.2g); restrictions,
                        // targets and mandatory additional costs still apply.
                        if crate::altcast::castable(cx, p, o, face, true, crate::altcast::Method::Free) {
                            opts.push(crate::faces::choice_opt(o, face));
                        }
                    }
                }
                let opt = cx.choose(Decision { player: p, kind: DecisionKind::ChooseCard, options: opts, subject: Some(ex.source) })?;
                let Some((o, face)) = crate::faces::chosen_face(&opt) else { break };
                let card = cx.s.obj(o).card;
                if let Some(b) = budget.as_mut() {
                    *b -= cx.db.get(card).face(face).cost.mana_value() as i32;
                }
                left.retain(|&x| x != o);
                // CR 707.12: the copy is created in the card's zone and cast; it's a token.
                let copy = cx.s.new_token(card, p);
                cx.s.obj_mut(copy).zone = *zone;
                crate::cast::cast_spell_method(cx, p, copy, face, crate::altcast::Method::Free)?;
            }
        }
        MoreEffect::SearchZone { origin, dest, change_type, num: n, owner, chooser, mandatory, shuffle, imprint } => {
            let Some(&ch) = crate::effect_ext::defined_players(cx, ex, *chooser).first() else { return Ok(()) };
            let owners = search_zone_owners(cx, ex, *owner);
            let limit = n.as_ref().map(|e| num(cx, ex, e).max(0) as usize);
            let mut new_ids: SmallVec<[ObjId; 2]> = SmallVec::new();
            for &o_p in &owners {
                if !matches!(origin, Zone::Graveyard | Zone::Hand | Zone::Library | Zone::Exile) {
                    return cx.unsupported("search of this zone");
                }
                // Searching a hidden zone: the chooser looks at those cards.
                if matches!(origin, Zone::Hand | Zone::Library) {
                    for o in search_zone_all(cx.s, *origin, o_p) {
                        cx.s.obj_mut(o).known_to |= 1 << ch.0;
                    }
                }
                let pool = search_zone_pool(cx, ex, *origin, change_type, o_p);
                let items = crate::query::card_items(cx.s, cx.db, &pool);
                let m = limit.unwrap_or(items.len()).min(items.len());
                let min = if *mandatory { m } else { 0 };
                let chosen: Vec<ObjId> = cx.choose_set(ch, DecisionKind::Search, items, min, m, Some(ex.source))?.into_iter().filter_map(|o| if let Opt::Card(x) = o { Some(x) } else { None }).collect();
                let moves: Vec<crate::zones::Move> = chosen.iter().map(|&o| crate::zones::Move::new(o, *dest)).collect();
                new_ids.extend(crate::zones::move_batch(cx, &moves)?.into_iter().flatten());
                if *origin == Zone::Library || *shuffle {
                    crate::flow::shuffle_library(cx.s, o_p);
                }
            }
            if _ad.remember {
                ex.remembered.extend(new_ids.iter().copied());
            }
            if *imprint {
                ex.imprinted.extend(new_ids.iter().copied());
            }
            ex.moved = new_ids;
            crate::effect_ext::link_exiled(cx, ex, *dest);
        }
        MoreEffect::GrantTriggers { triggers, duration } => {
            let (eot, until) = match duration {
                Duration::EndOfTurn => (true, None),
                Duration::Permanent => (false, None),
                Duration::UntilYourNextTurn => (false, Some(ex.you)),
            };
            for o in ex.moved.clone() {
                if cx.s.obj(o).zone == Zone::Battlefield {
                    let ts = cx.s.ts();
                    cx.s.effects.push(ContEffect { ts, affected: o, layer: Layer::L6, until_eot: eot, until_turn_of: until, triggers: triggers.iter().copied().collect(), ..Default::default() });
                }
            }
        }
        MoreEffect::PutCounterDivided { kind, .. } => {
            // 608.2b: an illegal target gets nothing (its share isn't redistributed).
            for &t in _tg {
                let Target::Obj(o) = t else { continue };
                let Some(&(_, _, n)) = cx.s.ext.divided.iter().find(|d| d.0 == ex.source && d.1 == t) else { return cx.unsupported("divided effect without a division") };
                if cx.s.obj(o).zone == Zone::Battlefield {
                    crate::ext6::put_counters(cx.s, cx.db, o, *kind, n);
                }
            }
        }
        MoreEffect::GrantFlashback => {
            for o in objs(_who) {
                if cx.s.obj(o).zone == Zone::Graveyard && !cx.s.ext.granted_flashback.contains(&o) {
                    cx.s.ext.granted_flashback.push(o);
                }
            }
        }
    }
    Ok(())
}

/// CR 700.13: a player commits a crime as they cast a spell, activate an ability or put a
/// triggered ability on the stack that targets an opponent, something an opponent controls
/// (permanent, spell, ability) or a card in an opponent's graveyard. Called right after the
/// spell/ability was put on the stack (it is the top item).
pub fn fire_crime_top(cx: &mut Ctx) {
    let Some(it) = cx.s.stack.last() else { return };
    let p = it.controller;
    let crime = it.targets.iter().any(|&(_, t)| match t {
        Target::Player(q) => q != p,
        Target::Obj(o) => {
            let ob = cx.s.obj(o);
            match ob.zone {
                Zone::Graveyard => ob.owner != p,
                Zone::Battlefield => ob.controller != p,
                // A spell on the stack (waiting or resolving, 608.2n).
                _ => cx.s.stack_items().any(|x| x.source == o && x.controller != p),
            }
        }
        // An ability on the stack an opponent controls (CR 700.13; `ext8`).
        Target::Ability { uid, .. } => cx.s.stack_items().any(|x| x.uid == uid && x.controller != p),
    });
    if crime {
        cx.s.ext.crimes |= 1 << p.0;
        crate::triggers::fire(cx.s, cx.db, &crate::triggers::TrigEvent::Ext(crate::trig_ext::ExtEvent::Crime { player: p }));
    }
}

/// CR 601.2d: as a spell that divides counters among its targets is cast, its controller
/// announces the division (each target gets at least one). Amounts are chosen target by target
/// in the targets' canonical order; among interchangeable targets (same signature, adjacent in
/// that order) amounts are non-increasing, so each division is offered once. Stored in
/// `State::ext.divided` until the spell leaves the stack.
pub fn choose_division(cx: &mut Ctx, p: PlayerId, so: ObjId, ability: AbilityId, modes: &[u8], targets: &[(u8, Target)], x: i32) -> R<()> {
    for (i, &n) in crate::resolve::nodes(cx.db, ability, modes).iter().enumerate() {
        let Effect::Ext(crate::effect_ext::ExtEffect::More(MoreEffect::PutCounterDivided { total, .. })) = &cx.db.ability(n).effect else { continue };
        let ts: Vec<Target> = targets.iter().filter(|t| t.0 as usize == i).map(|t| t.1).collect();
        if ts.is_empty() {
            continue;
        }
        let ecx = crate::expr::EvalCtx { s: cx.s, db: cx.db, you: p, source: so, x, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &[], imprinted: &[], base_only: false };
        let mut left = crate::expr::eval(total, &ecx);
        let k = ts.len();
        if left < k as i32 {
            return cx.unsupported("division with fewer than one per target");
        }
        if left > crate::engine::MAX_NUMBER_OPTIONS {
            // One option per amount below (Ctx::number_options).
            return cx.unsupported(format!("division of {left}: more than {} amounts to choose from", crate::engine::MAX_NUMBER_OPTIONS));
        }
        let sig = |cx: &Ctx, t: Target| match t {
            Target::Obj(o) => Some(obj_sig(cx.s, cx.db, o)),
            Target::Player(_) | Target::Ability { .. } => None,
        };
        let mut prev: Option<i32> = None;
        for j in 0..k {
            let same_as_prev = j > 0 && sig(cx, ts[j]).is_some() && sig(cx, ts[j]) == sig(cx, ts[j - 1]);
            let cap = if same_as_prev { prev.unwrap() } else { left };
            // Later targets in the same run of interchangeable targets are capped by this one.
            let run_after = ts[j + 1..].iter().take_while(|&&t| sig(cx, t).is_some() && sig(cx, t) == sig(cx, ts[j])).count();
            let others_after = k - 1 - j - run_after;
            let amount = if j == k - 1 {
                left
            } else {
                let opts: Vec<Opt> = (1..=cap.min(left - (k - 1 - j) as i32))
                    .filter(|&a| {
                        let rest = left - a;
                        // The rest must be absorbable: unbounded if some later target isn't in
                        // this run, else at most `a` each.
                        others_after > 0 || rest <= run_after as i32 * a
                    })
                    .map(Opt::Number)
                    .collect();
                let subject = if let Target::Obj(o) = ts[j] { Some(o) } else { None };
                let Opt::Number(a) = cx.choose(Decision { player: p, kind: DecisionKind::ChooseX, options: opts, subject })? else { unreachable!() };
                a
            };
            if same_as_prev && amount > prev.unwrap() {
                return cx.unsupported("division bookkeeping");
            }
            cx.s.ext.divided.push((so, ts[j], amount));
            left -= amount;
            prev = Some(amount);
        }
    }
    Ok(())
}

/// For a spell whose charm chooses more modes if its optional additional cost was paid
/// (`CardDb::charm_optcost`): is that cost paid with this selection of modes?
pub fn charm_optcost_paid(db: &CardDb, a: AbilityId, modes: &[u8]) -> Option<bool> {
    if db.charm_optcost.is_empty() {
        return None;
    }
    crate::resolve::nodes(db, a, &[]).into_iter().find_map(|n| db.charm_optcost.iter().find(|c| c.0 == n).map(|&(_, paid, _)| modes.len() == paid as usize))
}

/// Total mana value of the cards in `p`'s graveyard (collect evidence, CR 701.59).
pub fn graveyard_mv(cx: &Ctx, p: PlayerId) -> i32 {
    cx.s.player(p).graveyard.iter().filter(|&&o| crate::query::is_card(cx.s, o)).map(|&o| chars(cx.s, cx.db, o).mv as i32).sum()
}

/// Collect evidence N (CR 701.59a): exile cards from your graveyard with total mana value N or
/// more. Cards are picked one at a time in canonical order (card, signature, id) until the total
/// reaches N; a pick is offered only if N stays reachable with later cards, and identical cards
/// (`query::card_sig`: the same card nothing refers to, with the same designations) are
/// interchangeable. Exiling more than needed is not offered.
pub fn collect_evidence(cx: &mut Ctx, p: PlayerId, so: ObjId, n: i32) -> R<()> {
    let gy: Vec<ObjId> = cx.s.player(p).graveyard.iter().copied().filter(|&o| crate::query::is_card(cx.s, o)).collect();
    let sigs = crate::query::card_sigs(cx.s, cx.db, &gy);
    // (card + signature as one key, object, mana value): merged by card alone, a card
    // targeted by a spell on the stack (or linked, ...) would be one option with its twin.
    let mut cands: Vec<((CardId, crate::query::ObjSig), ObjId, i32)> = gy.iter().zip(sigs).map(|(&o, k)| ((cx.s.obj(o).card, k), o, chars(cx.s, cx.db, o).mv as i32)).collect();
    cands.sort_by_key(|c| (c.0, c.1));
    let (mut total, mut next) = (0, 0usize);
    let mut chosen: Vec<ObjId> = vec![];
    // CR 701.59a: "exile any number of cards ... with total mana value N or greater": once the
    // total is reached, `Done` is offered beside further cards (exiling more is legal, as
    // crew). Cards are picked in canonical order, so each set has one sequence.
    loop {
        let mut opts = vec![];
        let mut pos = vec![];
        let mut last: Option<(CardId, crate::query::ObjSig)> = None;
        for j in next..cands.len() {
            let (card, o, mv) = cands[j];
            let rest: i32 = cands[j + 1..].iter().map(|c| c.2).sum();
            if last == Some(card) || total + mv + rest < n {
                continue;
            }
            last = Some(card);
            opts.push(Opt::Card(o));
            pos.push(j);
        }
        if opts.is_empty() {
            if total >= n {
                break;
            }
            return cx.unsupported("collect evidence: not enough mana value");
        }
        if total >= n {
            opts.push(Opt::Done);
        }
        let c = cx.choose(Decision { player: p, kind: DecisionKind::PayCost, options: opts.clone(), subject: Some(so) })?;
        if c == Opt::Done {
            break;
        }
        let j = pos[cx.offered_ix(c, &opts)];
        cx.swap_alt(c, j, &mut cands, |k| Opt::Card(k.1) == c)?;
        total += cands[j].2;
        chosen.push(cands[j].1);
        next = j + 1;
    }
    let moves: Vec<crate::zones::Move> = chosen.into_iter().map(|o| crate::zones::Move::new(o, Zone::Exile)).collect();
    crate::zones::move_batch(cx, &moves)?;
    Ok(())
}

/// A target-only part of an ability (Forge `Pump` with no effect): it only names targets for
/// later parts (`Defined$ Targeted`).
pub fn target_holder(ad: &AbilityDef) -> bool {
    matches!(&ad.effect, Effect::Pump { power: Expr::Num(0), toughness: Expr::Num(0), kws, .. } if kws.0 == 0) && ad.target.is_some()
}

/// Effects whose own choice includes declining (so `Optional$` asks nothing more).
pub fn optional_in_effect(e: &Effect) -> bool {
    matches!(e, Effect::Ext(crate::effect_ext::ExtEffect::More(MoreEffect::CastCopies { .. })) | Effect::Ext(crate::effect_ext::ExtEffect::Mr(crate::ext4::MrEffect::DiscardUpTo { .. }))
        // "You may cast the discarded card ...": choosing none is the "may" (`ext9`).
        | Effect::Ext(crate::effect_ext::ExtEffect::X9(crate::ext9::X9Effect::MayCastDefinedFree))
        // "You may put a card exiled this way onto ...": choosing none is the "may" (`ext9`).
        | Effect::Ext(crate::effect_ext::ExtEffect::More(MoreEffect::SearchZone { origin: Zone::Exile, mandatory: false, .. }))
        | Effect::Ext(crate::effect_ext::ExtEffect::Mr(crate::ext4::MrEffect::MayCastImprinted { .. }))
        // "Sacrifice any number of ...": sacrificing none is the "may".
        | Effect::Sacrifice { up_to: true, .. })
}


/// May `p` activate loyalty abilities of `o` at instant speed (an effect object's
/// `LoyaltyAtInstantSpeed` static, Jace's Machinations)?
pub fn loyalty_instant_ok(s: &State, db: &CardDb, p: PlayerId, o: ObjId) -> bool {
    s.effect_objs.iter().any(|eo| {
        eo.controller == p
            // effect-object static reader: LoyaltyAtInstantSpeed (`static_ext::applies_from_effect_object`)
            && eo.statics.iter().any(|&si| match &db.statics_pool[si as usize].kind {
                StaticKind::Ext(crate::static_ext::StaticExt::More(MoreStatic::LoyaltyAtInstantSpeed { valid })) => obj_matches(s, db, valid, o, FilterCtx { you: p, ..eo_fcx!(eo) }),
                _ => false,
            })
    })
}

// ---------------------------------------------------------------- compiling

impl CardDb {
    /// Effect APIs of this batch (`None`: not one of them).
    pub(crate) fn more_api(&mut self, api: &str, p: &crate::compile::P, f: &forge_script::Face) -> Result<Option<Effect>, String> {
        Ok(Some(Effect::Ext(crate::effect_ext::ExtEffect::More(match api {
            "Empower" => {
                // CR 701.71a: only "Empower Jace" exists; Forge's token is `u_empower_<type>`.
                let t = p.get("Type").ok_or("Empower without Type")?;
                let kind = self.subtype(t).ok_or_else(|| format!("Empower Type {t}"))?;
                let token = self.token(&format!("u_empower_{}", t.to_ascii_lowercase()))?;
                if !self.get(token).playable() {
                    return Err(format!("Empower token: {:?}", self.get(token).unsupported));
                }
                let n = match p.get("Num") {
                    Some(v) => self.value(f, v)?,
                    None => Expr::Num(1),
                };
                MoreEffect::Empower { token, kind, n }
            }
            "Play" if p.flag("CopyCard") => {
                // Only "cast any number of copies without paying their mana costs".
                if p.get("Amount") != Some("All") || !p.flag("WithoutManaCost") || p.get("ValidSA") != Some("Spell") {
                    return Err("Play: other forms than casting copies for free".into());
                }
                p.ack("Optional", &["True"], "the choice includes casting none (`optional_in_effect`)");
                let zone = match p.get("ValidZone") {
                    Some("Exile") => Zone::Exile,
                    Some("Graveyard") => Zone::Graveyard,
                    z => return Err(format!("Play ValidZone {z:?}")),
                };
                let valid = self.filter_f(f, p.get("Valid").ok_or("Play without Valid")?)?;
                let total_mv = p.get("WithTotalCMC").map(|v| self.value(f, v)).transpose()?;
                MoreEffect::CastCopies { valid, zone, total_mv }
            }
            _ => return Ok(None),
        }))))
    }

    /// `ChangeZone` forms of this batch. Returns an effect replacing the plain `ChangeZone`, or
    /// None. (`WithCountersType$` is `compile`'s `enter_counters`: the objects enter with them,
    /// CR 122.6.)
    pub(crate) fn more_change_zone(&mut self, p: &crate::compile::P, _f: &forge_script::Face, _origin: ZoneSet, dest: Zone, _extra: &mut Vec<AbilityDef>) -> Result<Option<Effect>, String> {
        // Searches of one zone by a chooser (`SearchZone`): a public graveyard search ("exile a
        // card from an opponent's graveyard", all players' graveyards without `DefinedPlayer`,
        // Forge ChangeZoneEffect), any player's zone with `Chooser$`, "any number" searches whose
        // `ChangeNum$` is the zone's size, and `Imprint$`.
        // Exile: only the `Imprint$` form ("you may put a creature card exiled this way onto the
        // battlefield", Anzrag's Rampage; `ext9`), the owner's exiled cards as in Forge.
        let single = [Zone::Graveyard, Zone::Hand, Zone::Library, Zone::Exile].into_iter().find(|&z| _origin == ZoneSet::of(z)).filter(|&z| z != Zone::Exile || p.peek("Imprint").is_some());
        if let Some(origin) = single {
            let any_number = p.peek("ChangeNum").and_then(|v| _f.svar(v)).is_some_and(|sv| {
                let v = sv.value.trim();
                let zone_word = match origin {
                    Zone::Graveyard => "CardsInGraveyard",
                    Zone::Hand => "CardsInHand",
                    _ => "CardsInLibrary",
                };
                v.split_once('$').is_some_and(|(head, body)| body == zone_word && (head.starts_with("PlayerCountDefined") || head == "TargetedController"))
            });
            let public_all = origin == Zone::Graveyard && p.peek("DefinedPlayer").is_none() && p.peek("Hidden").is_some_and(|h| h.eq_ignore_ascii_case("true"));
            // (`WithTotalCMC$` searches are the plain hidden ChangeZone's, see compile.rs.)
            let takes = p.peek("ValidTgts").is_none() && p.peek("Defined").is_none() && p.peek("WithTotalCMC").is_none() && (any_number || p.peek("Imprint").is_some() || public_all || (origin == Zone::Graveyard && p.peek("Chooser").is_some()));
            if takes {
                p.ack("Hidden", &["True"], "a chooser's search of one zone");
                let change_type = self.filter_f(_f, p.get("ChangeType").unwrap_or("Card"))?;
                let num = if any_number {
                    p.matched("ChangeNum", |v| _f.svar(v).is_some(), "the zone's size (checked above)");
                    None
                } else {
                    Some(match p.get("ChangeNum") {
                        Some(v) => self.value(_f, v)?,
                        None => Expr::Num(1),
                    })
                };
                let owner = match p.get("DefinedPlayer") {
                    Some(d) => Some(self.defined(d)?),
                    None if origin == Zone::Graveyard => None,
                    None => Some(Defined::You),
                };
                let chooser = self.defined(p.get("Chooser").unwrap_or("You"))?;
                if chooser != Defined::You {
                    return Err(format!("ChangeZone Chooser {:?}", p.peek("Chooser")));
                }
                let mandatory = p.flag("Mandatory");
                let shuffle = p.flag("Shuffle");
                let imprint = p.flag("Imprint");
                return Ok(Some(Effect::Ext(crate::effect_ext::ExtEffect::More(MoreEffect::SearchZone { origin, dest, change_type, num, owner, chooser, mandatory, shuffle, imprint }))));
            }
        }
        Ok(None)
    }

    /// Trigger modes of this batch (`None`: not one of them; keys untouched).
    pub(crate) fn more_trigger_mode(&self, m: &str, p: &crate::compile::P) -> Result<Option<TrigMode>, String> {
        Ok(Some(match m {
            // "Whenever a creature you control or a creature spell you control becomes the target
            // of a spell or ability an opponent controls" (Surrak, Elusive Hunter).
            "BecomesTarget" => {
                let by = match p.get("ValidSource") {
                    Some("SpellAbility.OppCtrl") | Some("Card.OppCtrl,Emblem.OppCtrl") => PlayerSel::Opponent,
                    Some("SpellAbility.YouCtrl") => PlayerSel::You,
                    None | Some("SpellAbility") => PlayerSel::Any,
                    Some(v) => return Err(format!("BecomesTarget ValidSource {v}")),
                };
                // `Spell.X` (a spell on the stack) = a card on the stack with X.
                let vt = p.get("ValidTarget").ok_or("BecomesTarget without ValidTarget")?;
                let vt = vt.split(',').map(|a| a.strip_prefix("Spell.").map(|r| format!("Card.inZoneStack+{r}")).unwrap_or_else(|| a.to_string())).collect::<Vec<_>>().join(",");
                if p.has("FirstTime") {
                    return Err("BecomesTarget FirstTime".into());
                }
                TrigMode::BecomesTarget { valid: self.filter(&vt)?, by }
            }
            _ => return Ok(None),
        }))
    }

    /// A characteristic-defining static setting power and toughness (CR 604.3), e.g. "Sandman's
    /// power and toughness are each equal to the number of lands you control."
    pub(crate) fn cda_pt(&mut self, f: &forge_script::Face) -> Result<Option<(Expr, Expr)>, String> {
        let mut out = None;
        for s in f.statics.iter().filter(|s| s.get("CharacteristicDefining").is_some()) {
            let p = crate::compile::P::new(s);
            if p.get("Mode") != Some("Continuous") || !p.flag("CharacteristicDefining") || out.is_some() {
                return Err("characteristic-defining static form".into());
            }
            // Only power defined ("This Vehicle's power is equal to ...", printed `*/4`): the
            // printed toughness stays.
            let printed_t = f.pt.as_deref().and_then(|pt| pt.strip_prefix("*/")).and_then(|t| t.parse::<i32>().ok());
            out = match (p.get("SetPower"), p.get("SetToughness"), printed_t) {
                (Some(sp), Some(st), _) => Some((self.value(f, sp)?, self.value(f, st)?)),
                (Some(sp), None, Some(t)) => Some((self.value(f, sp)?, Expr::Num(t))),
                _ => return Err("characteristic-defining static without P/T".into()),
            };
            let unused = p.unused();
            if !unused.is_empty() {
                return Err(format!("characteristic-defining static param {}", unused.join(",")));
            }
        }
        Ok(out)
    }

    /// `Animate | Triggers$ A,B`: compile the triggered abilities into the pool and add a node
    /// granting them to what the `Animate` affected.
    pub(crate) fn animate_triggers(&mut self, names: &str, f: &forge_script::Face, uns: &mut Vec<String>, duration: Option<&str>, extra: &mut Vec<AbilityDef>) -> Result<(), String> {
        let duration = match duration {
            None => Duration::EndOfTurn,
            Some("Permanent") => Duration::Permanent,
            Some("UntilYourNextTurn") => Duration::UntilYourNextTurn,
            Some(d) => return Err(format!("Duration {d}")),
        };
        let mut triggers = vec![];
        for name in names.split(',') {
            let sv = f.svar(name.trim()).ok_or_else(|| format!("Animate trigger {name} missing"))?;
            let tp = forge_script::Params::parse(&sv.value);
            let td = self.compile_trigger(&tp, f, uns)?.ok_or("Animate trigger")?;
            if td.per_turn.is_some() || td.optional || !td.intervening.is_empty() {
                return Err("gained trigger with a per-turn limit / optional".into());
            }
            triggers.push(self.triggers_pool.len() as u32);
            self.triggers_pool.push(td);
        }
        extra.push(crate::compile_ext::node(Effect::Ext(crate::effect_ext::ExtEffect::More(MoreEffect::GrantTriggers { triggers, duration })), None));
        Ok(())
    }

    /// Static modes of this batch (`None`: not one of them; keys untouched).
    pub(crate) fn more_static(&mut self, mode: &str, p: &crate::compile::P, f: &forge_script::Face) -> Result<Option<MoreStatic>, String> {
        Ok(Some(match mode {
            "CastWithFlash" if p.peek("ValidSA") == Some("Activated.Loyalty") => {
                p.ack("ValidSA", &["Activated.Loyalty"], "checked above");
                if p.get("Caster").is_some_and(|c| c != "You") {
                    return Err("CastWithFlash for other players".into());
                }
                let valid = self.filter_f(f, p.get("ValidCard").unwrap_or("Card"))?;
                MoreStatic::LoyaltyAtInstantSpeed { valid }
            }
            _ => return Ok(None),
        }))
    }
}
