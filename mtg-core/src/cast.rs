//! Legal actions at priority, casting spells (CR 601), activating abilities (CR 602), targets
//! (CR 115) and mana payment.
//!
//! Every option offered must be completable: castability checks targets and mana payability up
//! front, and during payment only sources that keep the rest of the cost payable are offered.
//! Mana abilities are activated during payment (CR 601.2g) and, as priority options, whenever the
//! player has priority (CR 605.3a; filtered for agents, `priority_mana_options`); mana a
//! source makes beyond a payment or at priority stays in the mana pool (CR 106.4), and pool
//! mana is spent automatically for agent decisions (`auto_spend_pool`; asked with full mana
//! fidelity, `full_mana`).

use crate::basics::*;
use crate::carddb::*;
use crate::engine::*;
use crate::query::*;
use crate::state::*;
use smallvec::SmallVec;

pub fn legal_actions(cx: &Ctx, p: PlayerId) -> Vec<Opt> {
    crate::query::frozen(cx.s, cx.db, || legal_actions_frozen(cx, p))
}

fn legal_actions_frozen(cx: &Ctx, p: PlayerId) -> Vec<Opt> {
    let s = &*cx.s;
    let mut out = vec![Opt::Pass];
    let sorcery_ok = s.turn.active == p && s.turn.step.is_main() && s.stack.is_empty();
    // Identical cards are one option unless something refers to one of them specifically
    // (`query::card_sig`): a card targeted by a spell or ability on the
    // stack and its twin are different choices (CR 400.7, 608.2b).
    let hand = &s.player(p).hand;
    let hand_first = first_of_each_card(s, cx.db, hand);
    for (i, &h) in hand.iter().enumerate() {
        if !hand_first[i] {
            continue;
        }
        let faces = crate::faces::permitted(s, cx.db, h, crate::faces::FacePerm::Any);
        push_plays(cx, p, h, sorcery_ok, faces, &mut out);
    }
    // CR 702.34a: flashback from the graveyard; CR 702.29a: cycling from the hand.
    // Graveyard cards that can be cast or activated from there; identical ones merged as above.
    let gy: Vec<ObjId> = s
        .player(p)
        .graveyard
        .iter()
        .copied()
        .filter(|&o| {
            let f = cx.db.get(s.obj(o).card).front();
            f.flashback.is_some() || !f.grave_abilities.is_empty() || s.ext.granted_flashback.contains(&o)
        })
        .collect();
    let gy_first = first_of_each_card(s, cx.db, &gy);
    for (i, &o) in gy.iter().enumerate() {
        let f = cx.db.get(s.obj(o).card).front();
        if gy_first[i] && (f.flashback.is_some() || s.ext.granted_flashback.contains(&o)) && castable_with(cx, p, o, 0, sorcery_ok, &f.cost) {
            out.push(Opt::Flashback(o));
        }
        // A card with its own flashback that gained flashback has two flashback abilities with
        // different costs: either may be paid (CR 113.2c, 702.34a, 118.9a; Flashback ruling
        // 2026-03-20 "you may choose any of its flashback costs").
        if gy_first[i] && crate::altcast::has_second_flashback(s, o, f) && crate::altcast::castable(cx, p, o, 0, sorcery_ok, crate::altcast::Method::FlashbackGranted) {
            out.push(Opt::Alt(o, crate::altcast::Method::FlashbackGranted as u8));
        }
    }
    for (i, &h) in hand.iter().enumerate() {
        if !hand_first[i] {
            continue;
        }
        for &a in &cx.db.get(s.obj(h).card).front().hand_abilities {
            if can_activate(cx, p, h, a, cx.db.ability(a), sorcery_ok) {
                out.push(Opt::Activate(h, a));
            }
        }
    }
    // Activated abilities that function from the graveyard (CR 602.1, 113.6b): each card's
    // cost exiles that card (602.1a, 601.2h), so a card something refers to (Scavenging Ooze's
    // target on the stack) and its twin are two options; unreferenced identical
    // cards are one.
    for (i, &o) in gy.iter().enumerate() {
        let f = cx.db.get(s.obj(o).card).front();
        if f.grave_abilities.is_empty() || !gy_first[i] {
            continue;
        }
        for &a in &f.grave_abilities {
            if can_activate(cx, p, o, a, cx.db.ability(a), sorcery_ok) {
                out.push(Opt::Activate(o, a));
            }
        }
    }
    // CR 715.3d: a card exiled on an adventure may be played by the player who controlled the
    // Adventure spell: a land card as a land (Midgar, City of Mako; 305.9), any
    // other card cast normally, not as an Adventure again.
    for &o in &s.exile {
        if s.obj(o).on_adventure && adventure_player(s, o) == p {
            let faces = crate::faces::permitted(s, cx.db, o, crate::faces::FacePerm::NormalOnly);
            push_plays(cx, p, o, sorcery_ok, faces, &mut out);
        }
    }
    // Cards an effect lets this player play from other zones (CR 601.2a / impulse draw).
    crate::altcast::legal_actions(cx, p, sorcery_ok, &mut out);
    for (o, faces) in may_play_faces(cx, p) {
        push_plays(cx, p, o, sorcery_ok, faces, &mut out);
    }
    // CR 722.3c: while a permanent is prepared, its controller may cast a copy of its prepare
    // spell. Identical prepared permanents are interchangeable.
    let mut seen_prep: SmallVec<[crate::query::ObjSig; 4]> = SmallVec::new();
    for &o in &s.battlefield {
        let ob = s.obj(o);
        if ob.prepared && ob.controller == p && cx.db.get(ob.card).layout == Layout::Prepare && !seen_prep.contains(&obj_sig(s, cx.db, o)) && castable(cx, p, o, 1, sorcery_ok) {
            seen_prep.push(obj_sig(s, cx.db, o));
            out.push(Opt::CastPrepared(o));
        }
    }
    if sorcery_ok {
        crate::ext5::unlock_actions(cx, p, &mut out);
    }
    // Turning a manifested permanent face up: any time you have priority (CR 701.40b).
    crate::ext6::face_up_actions(cx, p, &mut out);
    for &o in &s.battlefield {
        if s.obj(o).controller != p {
            continue;
        }
        for a in chars(s, cx.db, o).abilities {
            let ad = cx.db.ability(a);
            if !ad.is_mana && can_activate(cx, p, o, a, ad, sorcery_ok) {
                out.push(Opt::Activate(o, a));
            }
        }
    }
    // Mana abilities (CR 605.3a), filtered for agents (`priority_mana_options`); none in
    // scenario positions without `rules_options.mana_abilities_at_priority` (`no_priority_mana`).
    if s.ext.x6.no_priority_mana & (1 << p.0) == 0 {
        out.extend(priority_mana_options(cx, p, !mana_unfiltered(s, p)));
    }
    out
}

pub use crate::faces::{FACE_ALT, FACE_NORMAL};

/// The options to play `o` (in a hand, or elsewhere with a permission to play it) as the `faces`
/// allowed, like a card in hand: a land card is played as a land (one per turn, main phase, stack
/// empty: CR 305.1-305.2, 305.9), any other card is cast; an adventurer / omen / split card / Room
/// may instead be cast as its other face (715.3: "As a player plays an adventurer card, the player
/// chooses whether they play the card normally or as an Adventure"; 715.3d: "other effects that
/// allow a player to cast it may allow a player to cast it as an Adventure").
/// Options already in `out` (one card with two permissions) aren't added twice.
fn push_plays(cx: &Ctx, p: PlayerId, o: ObjId, sorcery_ok: bool, faces: u8, out: &mut Vec<Opt>) {
    let s = &*cx.s;
    let cd = cx.db.get(s.obj(o).card);
    let mut push = |opt: Opt| {
        if !out.contains(&opt) {
            out.push(opt);
        }
    };
    if faces & FACE_NORMAL != 0 {
        if cd.front().types.has(CardType::Land) {
            if sorcery_ok && s.player(p).lands_played < crate::static_ext::land_plays(s, cx.db, p) {
                push(Opt::PlayLand(o));
            }
        } else if castable(cx, p, o, 0, sorcery_ok) {
            push(Opt::Cast(o));
        }
    }
    // `faces` comes from `faces::permitted`: FACE_ALT only for a card with a second face.
    if faces & FACE_ALT != 0 && castable(cx, p, o, 1, sorcery_ok) {
        push(Opt::CastFace(o, 1));
    }
}

/// The player who may play `o`, a card exiled on an adventure (CR 715.3d: the controller of the
/// Adventure spell, which isn't its owner when it was cast with a permission to cast another
/// player's card; `Ext::adventure_player`).
pub fn adventure_player(s: &State, o: ObjId) -> PlayerId {
    s.ext.adventure_player.iter().find(|e| e.0 == o).map_or(s.obj(o).owner, |e| e.1)
}

/// Objects outside the hand that `p` may play because of `MayPlay` effects (any face).
pub fn may_play(cx: &Ctx, p: PlayerId) -> Vec<ObjId> {
    may_play_faces(cx, p).into_iter().map(|e| e.0).collect()
}

/// Objects outside the hand that `p` may play because of `MayPlay` effects, with the faces
/// (`FACE_NORMAL` / `FACE_ALT`) the permissions allow (`faces::permitted`, `FacePerm::Filtered`).
pub fn may_play_faces(cx: &Ctx, p: PlayerId) -> Vec<(ObjId, u8)> {
    let s = &*cx.s;
    let mut out: Vec<(ObjId, u8)> = vec![];
    let add = |out: &mut Vec<(ObjId, u8)>, o: ObjId, faces: u8| {
        if faces == 0 {
            return;
        }
        match out.iter_mut().find(|e| e.0 == o) {
            Some(e) => e.1 |= faces,
            None => out.push((o, faces)),
        }
    };
    for eo in &s.effect_objs {
        if eo.controller != p {
            continue;
        }
        for &si in &eo.statics {
            // effect-object static reader: MayPlay (`static_ext::applies_from_effect_object`)
            let st = &cx.db.statics_pool[si as usize];
            if let StaticKind::MayPlay { affected, zones, .. } = &st.kind {
                if !st.conditions.is_empty() {
                    // "... you may play them if you control a Wizard" (Flameshape).
                    let ecx = crate::expr::EvalCtx { s, db: cx.db, you: p, source: eo.source, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &eo.remembered, imprinted: &[], base_only: false };
                    if !st.conditions.iter().all(|c| crate::expr::holds(c, &ecx)) {
                        continue;
                    }
                }
                let fcx = FilterCtx { you: p, ..eo_fcx!(eo) };
                for &o in &eo.remembered {
                    if zones.has(s.obj(o).zone) {
                        add(&mut out, o, crate::faces::permitted(s, cx.db, o, crate::faces::FacePerm::Filtered(affected, fcx)));
                    }
                }
            }
        }
    }
    // Permanents' "you may play X from your graveyard" (Icetill Explorer): own cards only.
    for &src in &s.battlefield {
        let so = s.obj(src);
        if so.controller != p {
            continue;
        }
        for st in &cx.db.get(so.card).face(so.face).statics {
            let StaticKind::MayPlay { affected, zones, .. } = &st.kind else { continue };
            if !st.zones.has(Zone::Battlefield) || (crate::query::may_lose_abilities(s, cx.db) && lost_all_abilities(s, cx.db, src)) || !crate::ext7::static_conditions_hold(s, cx.db, st, src) {
                continue;
            }
            let fcx = FilterCtx { you: p, source: Some(src), x: 0, rem: &[] };
            let mut pool: Vec<ObjId> = vec![];
            if zones.has(Zone::Graveyard) {
                pool.extend(s.player(p).graveyard.iter().copied());
            }
            if zones.has(Zone::Exile) {
                // Cards exiled with this source may be another player's (Null Summoner, `ext7`).
                let any_owner = crate::ext7::exiled_with_source_only(affected);
                pool.extend(s.exile.iter().copied().filter(|&o| any_owner || s.obj(o).owner == p));
            }
            // Identical cards in the same zone are one option unless something refers to one of
            // them (`query::card_sig`).
            for o in pool {
                // A twin already offered (same zone, same card, nothing refers to either): skip.
                let twin = |x: ObjId| x != o && s.obj(x).zone == s.obj(o).zone && s.obj(x).card == s.obj(o).card && card_sig(s, cx.db, x) == card_sig(s, cx.db, o);
                if out.iter().any(|&(x, _)| twin(x)) {
                    continue;
                }
                add(&mut out, o, crate::faces::permitted(s, cx.db, o, crate::faces::FacePerm::Filtered(affected, fcx)));
            }
        }
    }
    out
}

/// Generic cost reduction from static abilities (CR 601.2f), e.g. "costs {1} less for each...",
/// for `spell` cast as face `face`. The total cost is determined with the spell on the stack
/// (601.2a) as the face being cast: an Adventure, Omen, Room door or prepare spell has only
/// those characteristics there (715.3a-b, 720.3a-b, 709.3a-b, 722.3c; 601.3e), so a reduction's
/// spell filter ("noncreature spells", "red spells") is matched against them.
pub fn cost_reduction(cx: &Ctx, p: PlayerId, spell: ObjId, face: u8) -> u32 {
    let mut total = 0u32;
    let s = &*cx.s;
    let mut view = crate::altcast::SpellView::new(s, cx.db, spell, face, p);
    // The battlefield's permanents, then the spell's own "this spell costs less" statics (a
    // prepared permanent stays a source too: its spell is a copy, CR 722.3c).
    let mut srcs: SmallVec<[(ObjId, bool); 8]> = s.battlefield.iter().map(|&b| (b, false)).collect();
    srcs.push((spell, true));
    for (src, is_spell) in srcs {
        let so = s.obj(src);
        let f = cx.db.get(so.card).face(if is_spell { face } else { so.face });
        for st in &f.statics {
            let StaticKind::ReduceCost { valid, caster, amount } = &st.kind else { continue };
            if !st.zones.has(so.zone) && !(is_spell && st.zones == ZoneSet::ANY) {
                continue;
            }
            // CR 613.1f / 113.6: a permanent that lost all abilities reduces nothing.
            if !is_spell && so.zone == Zone::Battlefield && crate::query::may_lose_abilities(s, cx.db) && lost_all_abilities(s, cx.db, src) {
                continue;
            }
            // The spell's own statics speak for its would-be controller, the caster (CR 109.5,
            // 601.2a, 112.2: Hearth Elemental cast from an opponent's card counts
            // the caster's graveyard); a permanent's for its controller.
            let you = if is_spell { p } else { so.controller };
            if !caster.matches(p, you) {
                continue;
            }
            let ecx = crate::expr::EvalCtx { s, db: cx.db, you, source: src, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &[], imprinted: &[], base_only: false };
            if !st.conditions.iter().all(|c| crate::expr::holds(c, &ecx)) {
                continue;
            }
            if !view.matches(valid, you, if is_spell { None } else { Some(src) }) {
                continue;
            }
            // Reductions only lower the generic part: a saturated sum is exact in effect.
            total = total.saturating_add(crate::expr::eval(amount, &ecx).max(0) as u32);
        }
    }
    total.saturating_add(crate::altcast::affinity_reduction(cx, p, spell, face))
}

fn reduce(mut r: Req, by: u32) -> Req {
    r.generic = r.generic.saturating_sub(by);
    r.settle_any_type();
    r
}

fn castable(cx: &Ctx, p: PlayerId, h: ObjId, face: u8, sorcery_ok: bool) -> bool {
    crate::altcast::castable(cx, p, h, face, sorcery_ok, crate::altcast::Method::Normal)
}

/// Castability with flashback (an alternative cost, CR 702.34a).
fn castable_with(cx: &Ctx, p: PlayerId, h: ObjId, face: u8, sorcery_ok: bool, _cost: &ManaCost) -> bool {
    crate::altcast::castable(cx, p, h, face, sorcery_ok, crate::altcast::Method::Flashback)
}

/// Legal mode selections for an ability (just `[]` if it isn't modal): each selection must have
/// legal targets for every targeted node (CR 601.2c, 700.2).
pub fn mode_sets(cx: &Ctx, p: PlayerId, a: AbilityId, source: Option<ObjId>) -> Vec<SmallVec<[u8; 3]>> {
    let charm = crate::resolve::nodes(cx.db, a, &[]).into_iter().find_map(|n| match &cx.db.ability(n).effect {
        // A maximum given by an expression (`ext9`): evaluated now, for the player choosing.
        Effect::Charm { modes, min, max } => Some((modes.len() as u8, *min, crate::ext9::charm_max(cx, p, n, source).map_or(*max, |m| m.min(*max)))),
        _ => None,
    });
    let mut out = vec![];
    // A spell card about to be cast will be on the stack, not where it is now: it can't be its
    // own target (CR 601.2a, 601.2c; e.g. Flashback cast from a graveyard).
    let excl: SmallVec<[Target; 1]> = match source {
        Some(s) if cx.db.ability(a).kind == AbilityKind::Spell && cx.s.obj(s).zone != Zone::Stack => smallvec::smallvec![Target::Obj(s)],
        _ => SmallVec::new(),
    };
    let ok = |modes: &[u8]| {
        crate::resolve::nodes(cx.db, a, modes).iter().all(|&n| match &cx.db.ability(n).target {
            // Counts by the gift's promise: checked together below (`ext8`).
            Some(t) if t.by_promise.is_some() => true,
            Some(t) => legal_targets(cx, p, t, source, &excl).len() >= t.min as usize,
            None => true,
        }) && source.is_none_or(|src| {
            let g = crate::ext8::promise_gate(cx, p, a, src, !excl.is_empty());
            g.0 || g.1
        })
    };
    // "Choose one that hasn't been chosen this turn" (`ext7`): modes this source already chose.
    let used = crate::ext7::modes_used_this_turn(cx.s, cx.db, a, source);
    match charm {
        None => {
            if ok(&[]) {
                out.push(SmallVec::new());
            }
        }
        Some((n, min, max)) => {
            // Enumerate subsets in increasing order (modes are chosen without repetition).
            for mask in 1u32..(1 << n) {
                let k = mask.count_ones() as u8;
                if k < min || k > max {
                    continue;
                }
                let sel: SmallVec<[u8; 3]> = (0..n).filter(|i| mask & (1 << i) != 0).collect();
                if used & mask == 0 && ok(&sel) {
                    out.push(sel);
                }
            }
        }
    }
    out
}

/// Candidates for the object-choosing parts of a cost. `self_ok`: the source itself qualifies
/// when it matches. A cost's "Sacrifice a token" / "Tap an untapped creature you control" can be
/// paid with the source (CR 601.2h, 701.21a, 118.3; rulings: Nantuko Husk 2015-06-22, Warden of
/// the Inner Sky 2023-11-10, Boiling Rock Rioter 2025-10-02); "another" is in the
/// filter (`Other`). It doesn't when another part of the same cost already uses the source
/// ({T} with "tap an untapped ...", "sacrifice this" with "sacrifice a ...": 118.3), nor for
/// craft (702.167a: other permanents).
fn cost_candidates(cx: &Ctx, p: PlayerId, src: ObjId, f: &crate::filter::Filter, zone: Zone, untapped: bool, self_ok: bool) -> Vec<ObjId> {
    let fcx = FilterCtx { you: p, source: Some(src), x: 0, rem: &[] };
    let pool: Vec<ObjId> = match zone {
        Zone::Battlefield => cx.s.battlefield.iter().copied().filter(|&o| cx.s.obj(o).controller == p).collect(),
        // Cards only (a token that just left the battlefield isn't one: `query::is_card`).
        Zone::Hand => cx.s.player(p).hand.iter().copied().filter(|&o| crate::query::is_card(cx.s, o)).collect(),
        Zone::Graveyard => cx.s.player(p).graveyard.iter().copied().filter(|&o| crate::query::is_card(cx.s, o)).collect(),
        _ => vec![],
    };
    crate::query::frozen_if_big(cx.s, cx.db, || pool.into_iter().filter(|&o| (self_ok || o != src) && (!untapped || !cx.s.obj(o).tapped) && obj_matches(cx.s, cx.db, f, o, fcx)).collect())
}

/// Can the non-mana parts of a cost be paid?
pub fn other_costs_payable(cx: &Ctx, p: PlayerId, src: ObjId, c: &Cost) -> bool {
    let ok = |x: &Option<(u8, crate::filter::Filter)>, z: Zone, untapped: bool, self_ok: bool| x.as_ref().is_none_or(|(n, f)| cost_candidates(cx, p, src, f, z, untapped, self_ok).len() >= *n as usize);
    if c.exile_ctrl_or_grave.as_ref().is_some_and(|(n, f)| craft_candidates(cx, p, src, f).len() < *n as usize) {
        return false;
    }
    if c.crew.is_some_and(|n| crew_candidates(cx, p, src).iter().map(|x| x.1.max(0)).sum::<i32>() < n as i32) {
        return false;
    }
    c.pay_life <= cx.s.player(p).life && (!c.exile_self || matches!(cx.s.obj(src).zone, Zone::Battlefield | Zone::Graveyard)) && ok(&c.sac, Zone::Battlefield, false, !c.sac_self) && ok(&c.discard, Zone::Hand, false, !c.discard_self) && ok(&c.exile_from_grave, Zone::Graveyard, false, !c.exile_self) && ok(&c.tap_x, Zone::Battlefield, true, !c.tap)
}

/// Pay the non-mana parts of a cost (CR 601.2h), choosing objects as needed.
pub fn pay_other_costs(cx: &mut Ctx, p: PlayerId, src: ObjId, c: &Cost) -> R<()> {
    pay_other_costs_src(cx, p, src, c).map(|_| ())
}

/// Craft materials (CR 702.167a–b): other permanents you control and cards in your graveyard.
fn craft_candidates(cx: &Ctx, p: PlayerId, src: ObjId, f: &crate::filter::Filter) -> Vec<ObjId> {
    let mut v = cost_candidates(cx, p, src, f, Zone::Battlefield, false, false);
    v.extend(cost_candidates(cx, p, src, f, Zone::Graveyard, false, false));
    v
}

/// Other untapped creatures `p` controls, with power, for crew (CR 702.122a: "any number of
/// other untapped creatures", a 0-power one too).
fn crew_candidates(cx: &Ctx, p: PlayerId, src: ObjId) -> Vec<(ObjId, i32)> {
    let s = &*cx.s;
    s.battlefield
        .iter()
        .copied()
        .filter(|&o| o != src && s.obj(o).controller == p && !s.obj(o).tapped)
        .filter_map(|o| {
            let c = chars(s, cx.db, o);
            // "Crews Vehicles as though its power were N greater" (CR 702.122a; `ext5`).
            let pw = c.power + crate::ext5::crew_power_bonus(s, cx.db, o, &c);
            c.types.has(CardType::Creature).then_some((o, pw))
        })
        .collect()
}

/// Crew N (CR 702.122a: "tap any number of other untapped creatures you control with total
/// power N or greater"): creatures are tapped one at a time in canonical (signature, id) order,
/// each offered only if N stays reachable with creatures later in the order; once the total is
/// N or more, `Done` is offered beside further creatures (tapping more than needed is legal and
/// each of them crews the Vehicle, 702.122b-c: "becomes tapped" triggers). Every
/// set is reachable by exactly one sequence (`set_choice_identity` style).
fn pay_crew(cx: &mut Ctx, p: PlayerId, src: ObjId, n: u8) -> R<()> {
    // Read-only queries in memo scopes on big boards (`frozen_if_big`) and one `obj_sigs` scan per
    // choice instead of an `obj_sig` (a whole-state scan) per candidate per comparison: same
    // values, but a board of N vehicles cost O(N² log N) state scans per crew choice
    // (Phoenix Fleet Airship copies, seconds per decision).
    let mut cands = crate::query::frozen_if_big(cx.s, cx.db, || {
        let c = crew_candidates(cx, p, src);
        let objs: Vec<ObjId> = c.iter().map(|x| x.0).collect();
        let sigs = crate::query::obj_sigs(cx.s, cx.db, &objs);
        let mut keyed: Vec<_> = sigs.into_iter().zip(c).collect();
        keyed.sort_by_key(|(k, (o, _))| (*k, *o));
        keyed.into_iter().map(|x| x.1).collect::<Vec<_>>()
    });
    let mut total = 0;
    let mut next = 0;
    loop {
        let mut opts = vec![];
        let mut picks = vec![];
        let mut seen: SmallVec<[crate::query::ObjSig; 8]> = SmallVec::new();
        let objs: Vec<ObjId> = cands[next..].iter().map(|x| x.0).collect();
        let sigs = crate::query::frozen_if_big(cx.s, cx.db, || crate::query::obj_sigs(cx.s, cx.db, &objs));
        for j in next..cands.len() {
            let (o, pw) = cands[j];
            let rest: i32 = cands[j + 1..].iter().map(|c| c.1.max(0)).sum();
            let sig = sigs[j - next];
            if total + pw + rest < n as i32 || seen.contains(&sig) {
                continue;
            }
            seen.push(sig);
            opts.push(Opt::Card(o));
            picks.push(j);
        }
        let complete = total >= n as i32;
        if opts.is_empty() {
            if complete {
                return Ok(());
            }
            return cx.unsupported("crew cost dead end");
        }
        if complete {
            opts.push(Opt::Done);
        }
        let c = cx.choose(Decision { player: p, kind: DecisionKind::PayCost, options: opts.clone(), subject: Some(src) })?;
        if c == Opt::Done {
            return Ok(());
        }
        let j = picks[cx.offered_ix(c, &opts)];
        cx.swap_alt(c, j, &mut cands, |k| Opt::Card(k.0) == c)?;
        cx.s.obj_mut(cands[j].0).tapped = true;
        total += cands[j].1;
        next = j + 1;
    }
}

/// `pay_other_costs`, returning the new object of the source if the cost exiled it (craft).
pub fn pay_other_costs_src(cx: &mut Ctx, p: PlayerId, src: ObjId, c: &Cost) -> R<Option<ObjId>> {
    if let Some(n) = c.crew {
        pay_crew(cx, p, src, n)?;
    }
    let mut crafted: Vec<ObjId> = vec![];
    if let Some((n, f)) = &c.exile_ctrl_or_grave {
        // Where a material comes from matters (battlefield vs graveyard): zone is part of the key.
        let objs = craft_candidates(cx, p, src, f);
        let sigs = crate::query::frozen_if_big(cx.s, cx.db, || crate::query::obj_sigs(cx.s, cx.db, &objs));
        let items: Vec<((u8, crate::query::ObjSig), Opt)> = objs.into_iter().zip(sigs).map(|(o, k)| ((cx.s.obj(o).zone as u8, k), Opt::Card(o))).collect();
        // "Craft with N or more" (`craft_more`): any number from N up.
        let max = if c.craft_more { items.len() } else { *n as usize };
        crafted = cx.choose_set(p, DecisionKind::PayCost, items, *n as usize, max, Some(src))?.into_iter().filter_map(|o| if let Opt::Card(x) = o { Some(x) } else { None }).collect();
    }
    if c.exile_self || !crafted.is_empty() {
        let mut moves: Vec<crate::zones::Move> = crafted.iter().map(|&o| crate::zones::Move::new(o, Zone::Exile)).collect();
        if c.exile_self {
            moves.push(crate::zones::Move::new(src, Zone::Exile));
        }
        let new = crate::zones::move_batch(cx, &moves)?;
        pay_other_costs_rest(cx, p, src, c)?;
        let returned = if c.exile_self { new.last().copied().flatten() } else { None };
        // The cards used to craft it, for "the exiled cards used to craft it" (`ext6`).
        if let (Some(r), false) = (returned, crafted.is_empty()) {
            let mats: SmallVec<[ObjId; 4]> = new[..crafted.len()].iter().flatten().copied().collect();
            cx.s.ext.x6.craft_pending.push((r, mats));
        }
        return Ok(returned);
    }
    pay_other_costs_rest(cx, p, src, c)?;
    Ok(None)
}

fn pay_other_costs_rest(cx: &mut Ctx, p: PlayerId, src: ObjId, c: &Cost) -> R<()> {
    let pick = |cx: &mut Ctx, x: &Option<(u8, crate::filter::Filter)>, z: Zone, untapped: bool, self_ok: bool| -> R<Vec<ObjId>> {
        let Some((n, f)) = x else { return Ok(vec![]) };
        let items = crate::query::sig_items(cx.s, cx.db, &cost_candidates(cx, p, src, f, z, untapped, self_ok));
        Ok(cx.choose_set(p, DecisionKind::PayCost, items, *n as usize, *n as usize, Some(src))?.into_iter().filter_map(|o| if let Opt::Card(x) = o { Some(x) } else { None }).collect())
    };
    for o in pick(cx, &c.tap_x, Zone::Battlefield, true, !c.tap)? {
        cx.s.obj_mut(o).tapped = true;
        // The tapped creature, for station (`ext5`, CR 702.184a).
        cx.s.ext.le.cost_tapped.push((src, o));
    }
    if c.pay_life > 0 {
        crate::resolve::lose_life(cx.s, cx.db, p, c.pay_life);
    }
    let sac = pick(cx, &c.sac, Zone::Battlefield, false, !c.sac_self)?;
    // Remember what was sacrificed for `Sacrificed$...` until the ability resolves.
    if let Some(&o) = sac.first() {
        cx.s.ext.cost_sacrificed.push((src, o));
    }
    let disc = pick(cx, &c.discard, Zone::Hand, false, !c.discard_self)?;
    let ex = pick(cx, &c.exile_from_grave, Zone::Graveyard, false, !c.exile_self)?;
    let mut moves: Vec<crate::zones::Move> = vec![];
    crate::trig_ext::fire_sacrificed(cx, &sac, p);
    if c.sac_self {
        crate::trig_ext::fire_sacrificed(cx, &[src], p);
    }
    moves.extend(sac.iter().map(|&o| crate::zones::Move::new(o, Zone::Graveyard)));
    for &o in &disc {
        let card = cx.s.obj(o).card;
        cx.s.emit(Event::Discard { player: p, card });
        moves.push(crate::zones::Move::new(o, Zone::Graveyard));
    }
    moves.extend(ex.iter().map(|&o| crate::zones::Move::new(o, Zone::Exile)));
    if c.discard_self {
        let card = cx.s.obj(src).card;
        cx.s.emit(Event::Discard { player: p, card });
    }
    if c.sac_self || c.discard_self {
        moves.push(crate::zones::Move::new(src, Zone::Graveyard));
    }
    crate::zones::move_batch(cx, &moves)?;
    Ok(())
}

fn can_activate(cx: &Ctx, p: PlayerId, o: ObjId, id: AbilityId, ad: &AbilityDef, sorcery_ok: bool) -> bool {
    if ad.sorcery_speed && !sorcery_ok {
        return false;
    }
    let Some(cost) = &ad.cost else { return false };
    if let Some((k, n)) = cost.counter {
        // CR 606.3: loyalty abilities at sorcery speed, once per turn per permanent; 606.6 / 602.
        // Jace's Machinations: at instant speed on any player's turn (still once per turn).
        if cost.is_loyalty() && ((!sorcery_ok && !crate::ext2::loyalty_instant_ok(cx.s, cx.db, p, o)) || cx.s.turn_stats.loyalty_activated.contains(&o)) {
            return false;
        }
        if n < 0 && cx.s.obj(o).counter(k) < -n {
            return false;
        }
    }
    if cost.remove_any > 0 && cx.s.obj(o).counters.iter().map(|c| c.1.max(0)).sum::<i32>() < cost.remove_any as i32 {
        return false;
    }
    if cost.return_unblocked && unblocked_attackers(cx, p).is_empty() {
        return false;
    }
    if cost.tap && (cx.s.obj(o).tapped || (has_type(cx, o, CardType::Creature) && cx.s.is_sick(o) && !has_kw(cx.s, cx.db, o, Kw::Haste))) {
        return false;
    }
    if !other_costs_payable(cx, p, o, cost) || crate::static_ext::activation_forbidden(cx.s, cx.db, o, ad) || crate::effect_ext::activation_limit_reached(cx.s, cx.db, o, id) {
        return false;
    }
    let ecx = crate::expr::EvalCtx { s: cx.s, db: cx.db, you: p, source: o, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &[], imprinted: &[], base_only: false };
    if !ad.activate_if.iter().all(|c| crate::expr::holds(c, &ecx)) {
        return false;
    }
    if mode_sets(cx, p, id, Some(o)).is_empty() {
        return false;
    }
    let srcs = mana_sources_for(cx, p, if cost.tap { Some(o) } else { None }, Some(crate::ext6::ABILITY_PAYMENT));
    // Sacrifice / tap parts mana payment mustn't consume (CR 118.3, 601.2g–h).
    let reserve = Reserve::of(cx, p, o, cost);
    match target_dependent_cost(cx, id) {
        // The cost depends on the target (CR 601.2f after 601.2c): some target must leave it payable.
        // A free activation past the loop cap isn't offered (`loops`, CR 732 / MTR 4.4).
        Some(spec) => legal_targets(cx, p, &spec, Some(o), &[]).into_iter().any(|t| {
            let req = ability_mana(cx, p, o, id, &[t]);
            payable_reserving(&req, &srcs, reserve.as_ref()) && !crate::loops::blocked(cx.s, o, id, cost, &req)
        }),
        None => {
            let req = ability_mana(cx, p, o, id, &[]);
            payable_reserving(&req, &srcs, reserve.as_ref()) && !crate::loops::blocked(cx.s, o, id, cost, &req)
        }
    }
}

/// Is `ad` an equip ability (CR 702.6a)?
pub fn is_equip(ad: &AbilityDef) -> bool {
    ad.kind == AbilityKind::Activated && matches!(ad.effect, Effect::Attach)
}

/// The target spec an activated ability's cost depends on, if any (its only targeted node, when
/// a cost reduction may refer to the target).
fn target_dependent_cost(cx: &Ctx, a: AbilityId) -> Option<TargetSpec> {
    let ad = cx.db.ability(a);
    if !(cx.db.ability_reduce.iter().any(|x| x.0 == a) || (cx.db.any_ability_reduce && is_equip(ad))) {
        return None;
    }
    ad.target.clone()
}

/// Mana an activated ability `a` of `o` costs `p` with these targets: its mana cost, or {0}
/// for a free equip (CR 118.9), minus generic cost reductions (CR 601.2f).
pub fn ability_mana(cx: &Ctx, p: PlayerId, o: ObjId, a: AbilityId, targets: &[Target]) -> Req {
    let ad = cx.db.ability(a);
    let mut req = requirement(&ad.cost.as_ref().map(|c| c.mana.clone()).unwrap_or_default(), 0);
    if !cx.db.lt.power_up.is_empty() && cx.db.lt.power_up.contains(&a) {
        req = crate::ext3::power_up_reduce(cx, o, req);
    }
    if req.is_empty() || (cx.db.ability_reduce.is_empty() && !cx.db.any_ability_reduce) {
        return req;
    }
    let s = &*cx.s;
    let mut total = 0u32;
    if let Some((_, e)) = cx.db.ability_reduce.iter().find(|x| x.0 == a) {
        let ecx = crate::expr::EvalCtx { s, db: cx.db, you: p, source: o, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets, rem: &[], imprinted: &[], base_only: false };
        total = total.saturating_add(crate::expr::eval(e, &ecx).max(0) as u32);
    }
    if cx.db.any_ability_reduce {
        let equip = is_equip(ad);
        for &src in &s.battlefield {
            let so = s.obj(src);
            for st in &cx.db.get(so.card).face(so.face).statics {
                let StaticKind::Ext(x) = &st.kind else { continue };
                if !st.zones.has(Zone::Battlefield) {
                    continue;
                }
                let ecx = crate::expr::EvalCtx { s, db: cx.db, you: so.controller, source: src, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets, rem: &[], imprinted: &[], base_only: false };
                match x {
                    crate::static_ext::StaticExt::FreeEquip { player } if equip && player.matches(p, so.controller) => {
                        if st.conditions.iter().all(|c| crate::expr::holds(c, &ecx)) && (!crate::query::may_lose_abilities(s, cx.db) || !lost_all_abilities(s, cx.db, src)) {
                            return Req::default();
                        }
                    }
                    crate::static_ext::StaticExt::AbilityCostReduce { valid, equip_only, target_self, player, amount } => {
                        if !player.matches(p, so.controller) || (*equip_only && !equip) || (*target_self && !targets.contains(&Target::Obj(src))) {
                            continue;
                        }
                        if valid.as_ref().is_some_and(|f| !obj_matches(s, cx.db, f, o, FilterCtx { you: so.controller, source: Some(src), x: 0, rem: &[] })) {
                            continue;
                        }
                        if !st.conditions.iter().all(|c| crate::expr::holds(c, &ecx)) || (crate::query::may_lose_abilities(s, cx.db) && lost_all_abilities(s, cx.db, src)) {
                            continue;
                        }
                        total = total.saturating_add(crate::expr::eval(amount, &ecx).max(0) as u32);
                    }
                    _ => {}
                }
            }
        }
    }
    reduce(req, total)
}

/// Unblocked attacking creatures `p` controls (CR 509.1h: only after blockers are declared).
pub fn unblocked_attackers(cx: &Ctx, p: PlayerId) -> Vec<ObjId> {
    let s = &*cx.s;
    if !(Step::DeclareBlockers..=Step::EndCombat).contains(&s.turn.step) || (s.turn.step == Step::DeclareBlockers && !s.turn.tba_done) {
        return vec![];
    }
    s.combat.attackers.iter().map(|a| a.0).filter(|&a| s.obj(a).controller == p && !s.combat.blocked.contains(&a)).collect()
}

fn has_type(cx: &Ctx, o: ObjId, t: CardType) -> bool {
    chars(cx.s, cx.db, o).types.has(t)
}

/// All legal targets for a target spec, in deterministic order. `source`: the spell or the
/// ability's source choosing them (the filter's "this"; a spell can't target itself, CR 115.5),
/// also the source a target's "hexproof from" quality is checked against (702.11d).
pub fn legal_targets(cx: &Ctx, p: PlayerId, t: &TargetSpec, source: Option<ObjId>, exclude: &[Target]) -> Vec<Target> {
    legal_targets_from(cx, p, t, source, source, exclude)
}

/// `legal_targets` with the source of the spell or ability (`quality`: whose characteristics a
/// "hexproof from" quality looks at) given apart from the filter's `source`: an ability's targets
/// rechecked on resolution have no filter source but are still checked against its source's
/// colours, as when they were chosen (CR 608.2b, 702.11d). The one place that
/// decides whether a permanent or player can be targeted (hexproof 702.11b-c, hexproof from
/// 702.11d, "as though it didn't have hexproof" 702.11e), for every target kind.
pub fn legal_targets_from(cx: &Ctx, p: PlayerId, t: &TargetSpec, source: Option<ObjId>, quality: Option<ObjId>, exclude: &[Target]) -> Vec<Target> {
    crate::query::frozen(cx.s, cx.db, || legal_targets_in_from(cx.s, cx.db, p, t, source, quality, exclude))
}

/// `legal_targets` on a plain state (no unit context): e.g. `observe` counting where a spell
/// could legally go (encoding v7 option features). Callers memoize with `query::frozen`.
pub fn legal_targets_in(s: &State, db: &CardDb, p: PlayerId, t: &TargetSpec, source: Option<ObjId>, exclude: &[Target]) -> Vec<Target> {
    legal_targets_in_from(s, db, p, t, source, source, exclude)
}

/// CR 608.2b: is `t` still a legal target of `item`'s node with spec `spec` as it resolves? The
/// same check as when it was chosen (`legal_targets_from`: a spell is its own filter source; an
/// ability's "hexproof from" check uses its source, last known information if it left).
pub fn target_still_legal(cx: &Ctx, item: &StackItem, spec: &TargetSpec, t: Target) -> bool {
    let this = if item.kind == StackKind::Spell { Some(item.source) } else { None };
    legal_targets_from(cx, item.controller, spec, this, Some(item.source), &[]).contains(&t)
}

fn legal_targets_in_from(s: &State, db: &CardDb, p: PlayerId, t: &TargetSpec, source: Option<ObjId>, quality: Option<ObjId>, exclude: &[Target]) -> Vec<Target> {
    let fcx = FilterCtx { you: p, source, x: 0, rem: &[] };
    let mut out = vec![];
    match t.kind {
        TargetKind::Normal => {
            for pl in [p, p.opp()] {
                // A player with hexproof can't be the target of spells or abilities their
                // opponents control (CR 702.11c; `ext7`).
                if player_matches(&t.filter, pl, fcx) && !(pl != p && crate::static_ext::player_has_hexproof(s, db, pl)) {
                    out.push(Target::Player(pl));
                }
            }
            for &o in &s.battlefield {
                if obj_matches(s, db, &t.filter, o, fcx) && targetable_by(s, db, o, p) && !hexproof_from_source(s, db, o, p, quality) {
                    out.push(Target::Obj(o));
                }
            }
        }
        TargetKind::SpellOrPermanent => {
            for &o in &s.battlefield {
                // The same hexproof checks as `Normal` targets (CR 702.11b, 702.11d).
                if obj_matches(s, db, &t.filter, o, fcx) && targetable_by(s, db, o, p) && !hexproof_from_source(s, db, o, p, quality) {
                    out.push(Target::Obj(o));
                }
            }
            for it in &s.stack {
                if it.kind == StackKind::Spell && Some(it.source) != source && obj_matches(s, db, &t.filter, it.source, fcx) {
                    out.push(Target::Obj(it.source));
                }
            }
        }
        TargetKind::Spell => {
            for it in &s.stack {
                if it.kind == StackKind::Spell && Some(it.source) != source && obj_matches(s, db, &t.filter, it.source, fcx) {
                    out.push(Target::Obj(it.source));
                }
            }
        }
        TargetKind::Ability => {
            // Activated/triggered abilities on the stack, each its own target (CR 113.7a,
            // 115.1b-d); the filter is about the source (`ValidTgts$ Card`).
            let sel = StackSel { activated: true, triggered: true, ..Default::default() };
            crate::ext8::stack_targets(s, db, &t.filter, sel, source, fcx, &mut out);
        }
        TargetKind::Stack(sel) => crate::ext8::stack_targets(s, db, &t.filter, sel, source, fcx, &mut out),
        TargetKind::Graveyard => {
            for pl in [p, p.opp()] {
                for &o in &s.player(pl).graveyard {
                    if obj_matches(s, db, &t.filter, o, fcx) {
                        out.push(Target::Obj(o));
                    }
                }
            }
        }
    }
    out.retain(|x| !exclude.contains(x));
    out
}

/// "Hexproof from monocolored" (CR 702.11d) granted by an attached Equipment: `o` can't be the
/// target of monocolored spells or abilities from monocolored sources its controller's
/// opponents control. `source` is the spell or the ability's source. An effect that lets `p`
/// target it as though it didn't have hexproof (Nowhere to Run) covers "hexproof from" too (CR
/// 702.11e).
pub(crate) fn hexproof_from_source(s: &State, db: &CardDb, o: ObjId, p: PlayerId, source: Option<ObjId>) -> bool {
    let Some(src) = source else { return false };
    if s.obj(o).controller == p || !db.any_hexproof_from {
        return false;
    }
    has_hexproof_from_mono(s, db, o) && chars(s, db, src).colors.count() == 1 && !crate::ext3::ignores_hexproof(s, db, o, p)
}

/// `o` has "hexproof from monocolored" (CR 702.11d) from an attached Equipment (the only
/// "hexproof from" the engine compiles; any other fails loudly as unsupported).
pub fn has_hexproof_from_mono(s: &State, db: &CardDb, o: ObjId) -> bool {
    db.any_hexproof_from
        && s.battlefield.iter().any(|&e| {
            let eo = s.obj(e);
            eo.attached_to == Some(o)
                && db.get(eo.card).face(eo.face).statics.iter().any(|st| matches!(&st.kind, StaticKind::DefinedContinuous(dc) if dc.hexproof_from_mono && dc.who == StaticWho::Attached))
                && (!crate::query::may_lose_abilities(s, db) || !lost_all_abilities(s, db, e))
                // granted with the Equipment's timestamp (CR 613.7e): a later loss removes it
                && !crate::query::grant_removed(s, db, o, eo.ts)
        })
}

/// Choose modes (CR 601.2b, 700.2) if modal; returns the selection.
pub fn choose_modes(cx: &mut Ctx, p: PlayerId, a: AbilityId, source: ObjId) -> R<SmallVec<[u8; 3]>> {
    let sets = mode_sets(cx, p, a, Some(source));
    let modes = choose_modes_among(cx, p, sets, source)?;
    crate::ext7::note_modes(cx.s, cx.db, a, source, &modes); // "not chosen this turn" (`ext7`)
    Ok(modes)
}

/// Choose one of the legal mode selections `sets`, one mode at a time.
pub fn choose_modes_among(cx: &mut Ctx, p: PlayerId, sets: Vec<SmallVec<[u8; 3]>>, source: ObjId) -> R<SmallVec<[u8; 3]>> {
    if sets.len() <= 1 {
        return Ok(sets.into_iter().next().unwrap_or_default());
    }
    // Factored: pick modes one at a time in increasing order, then Done when allowed.
    let mut chosen: SmallVec<[u8; 3]> = SmallVec::new();
    loop {
        let mut opts = vec![];
        if sets.iter().any(|s| *s == chosen) {
            opts.push(Opt::Done);
        }
        let mut next: SmallVec<[u8; 4]> = SmallVec::new();
        for s in &sets {
            if s.len() > chosen.len() && s[..chosen.len()] == chosen[..] && !next.contains(&s[chosen.len()]) {
                next.push(s[chosen.len()]);
            }
        }
        opts.extend(next.into_iter().map(Opt::Mode));
        match cx.choose(Decision { player: p, kind: DecisionKind::ChooseMode, options: opts, subject: Some(source) })? {
            Opt::Mode(m) => chosen.push(m),
            _ => return Ok(chosen),
        }
    }
}

/// Choose targets for each node of the (mode-expanded) chain (CR 601.2c).
pub fn choose_targets(cx: &mut Ctx, p: PlayerId, a: AbilityId, modes: &[u8], source: ObjId) -> R<SmallVec<[(u8, Target); 2]>> {
    choose_targets_ctx(cx, p, a, modes, source, 0, TrigCtx::default())
}

/// `choose_targets` with the spell's X and the trigger event, for variable target counts
/// ("up to X target ...", CR 601.2c; reflexive "up to that many", CR 603.12).
pub fn choose_targets_ctx(cx: &mut Ctx, p: PlayerId, a: AbilityId, modes: &[u8], source: ObjId, x: i32, trig: TrigCtx) -> R<SmallVec<[(u8, Target); 2]>> {
    choose_targets_where(cx, p, a, modes, source, x, trig, &|_, _| true)
}

/// `choose_targets_ctx`, offering only targets satisfying `ok` (e.g. those for which a
/// target-dependent cost stays payable).
#[allow(clippy::too_many_arguments)]
pub fn choose_targets_where(cx: &mut Ctx, p: PlayerId, a: AbilityId, modes: &[u8], source: ObjId, x: i32, trig: TrigCtx, ok: &dyn Fn(&Ctx, Target) -> bool) -> R<SmallVec<[(u8, Target); 2]>> {
    let mut out: SmallVec<[(u8, Target); 2]> = SmallVec::new();
    let nodes = crate::resolve::nodes(cx.db, a, modes);
    // Observation context (`ext6::Pick`): which targeted part this is, and the earlier parts'
    // targets (CR 601.2c). Cleared below once every target is chosen.
    let parts = nodes.iter().filter(|&&n| cx.db.ability(n).target.is_some()).count() as u8;
    let mut part = 0u8;
    for (i, &n) in nodes.iter().enumerate() {
        let Some(t) = cx.db.ability(n).target.clone() else { continue };
        part += 1;
        if parts > 0 {
            let pk = &mut cx.s.ext.x6.pick;
            pk.part = part;
            pk.parts = parts;
            pk.earlier.clear();
            pk.earlier.extend(out.iter().map(|x| x.1));
        }
        // A trigger being put on the stack (`State::being_put`, CR 603.3d): its view shows the
        // targets of the targeted parts chosen so far (a view only).
        if let Some(bp) = cx.s.being_put.as_mut().filter(|b| b.source == source && b.ability == a) {
            bp.targets.clone_from(&out);
        }
        // `TargetUnique$`: an object already targeted by another part can't be chosen again.
        let mut exclude: SmallVec<[Target; 2]> = if t.unique { out.iter().map(|x| x.1).collect() } else { SmallVec::new() };
        // "Other than that creature" (`ext9`): not the object that caused the trigger.
        if let (Some(o), true) = (trig.target, !cx.db.x9.exclude_triggered.is_empty() && cx.db.x9.exclude_triggered.contains(&n)) {
            exclude.push(Target::Obj(o));
        }
        let max = match &t.max_expr {
            Some(e) => {
                let ecx = crate::expr::EvalCtx { s: cx.s, db: cx.db, you: p, source, x, kicked: false, triggered: trig.obj, trigger_amount: trig.amount, targets: &[], rem: &[], imprinted: &[], base_only: false };
                crate::expr::eval(e, &ecx).clamp(0, t.max as i32) as u8
            }
            None => t.max,
        };
        // Exactly as many as the gift's promise says (`ext8`; none: no target for this word).
        let (min, max) = match t.by_promise {
            Some(by) => {
                let n = crate::ext8::promise_count(cx.s, source, by);
                (n, n)
            }
            None => (t.min, max),
        };
        if max == 0 && t.by_promise.is_some() {
            continue;
        }
        if max < min {
            return cx.unsupported("variable target maximum below the minimum");
        }
        // A target set: canonical order, identical permanents interchangeable.
        let items: Vec<((u8, crate::query::ObjSig), Opt)> = legal_targets(cx, p, &t, Some(source), &exclude)
            .into_iter()
            .filter(|&x| ok(cx, x) && crate::ext9::attached_to_parent_ok(cx, n, i, &out, x))
            .map(|x| {
                let k = match x {
                    Target::Player(pl) => (0, (pl.0 as u32, 0, 0, 0, 0, 0)),
                    Target::Obj(o) => {
                        (1, obj_sig(cx.s, cx.db, o))
                    }
                    // Each ability on the stack is its own object (CR 113.7a): never merged.
                    Target::Ability { uid, .. } => (2, (uid, 0, 0, 0, 0, 0)),
                };
                (k, Opt::Target(x))
            })
            .collect();
        if items.len() < min as usize {
            return cx.unsupported(format!("ran out of targets ({})", cx.db.get(cx.s.obj(source).card).name));
        }
        // "From a single graveyard" (`ext9`): every target after the first has its owner.
        let same_owner = !cx.db.x9.same_owner_targets.is_empty() && cx.db.x9.same_owner_targets.contains(&n);
        let picked = if same_owner {
            cx.choose_set_compat(p, DecisionKind::Target, items, min as usize, max as usize, Some(source), &|s, a, b| match (a, b) {
                (Opt::Target(Target::Obj(x)), Opt::Target(Target::Obj(y))) => s.obj(*x).owner == s.obj(*y).owner,
                _ => false,
            })?
        } else {
            cx.choose_set(p, DecisionKind::Target, items, min as usize, max as usize, Some(source))?
        };
        for c in picked {
            if let Opt::Target(x) = c {
                out.push((i as u8, x));
            }
        }
    }
    if parts > 0 {
        cx.s.ext.x6.pick = Default::default();
    }
    Ok(out)
}

pub fn play_land(cx: &mut Ctx, p: PlayerId, o: ObjId) -> R<()> {
    let card = cx.s.obj(o).card;
    cx.s.player_mut(p).lands_played += 1;
    cx.s.emit(Event::PlayLand { player: p, card });
    crate::zones::move_batch(cx, &[crate::zones::Move { obj: o, to: Zone::Battlefield, to_top: false, tapped: false, controller: Some(p) }])?;
    Ok(())
}

pub fn cast_spell(cx: &mut Ctx, p: PlayerId, h: ObjId, face: u8) -> R<()> {
    cast_spell_alt(cx, p, h, face, None)
}

/// Cast, optionally paying an alternative cost (flashback) instead of the mana cost.
pub fn cast_spell_alt(cx: &mut Ctx, p: PlayerId, h: ObjId, face: u8, alt: Option<ManaCost>) -> R<()> {
    let m = if alt.is_some() { crate::altcast::Method::Flashback } else { crate::altcast::Method::Normal };
    cast_spell_method(cx, p, h, face, m)
}

/// Cast `h` (as face `face`) by casting method `method` (CR 601.2, alternative costs 118.9).
pub fn cast_spell_method(cx: &mut Ctx, p: PlayerId, h: ObjId, face: u8, method: crate::altcast::Method) -> R<()> {
    use crate::altcast::Method;
    // Flashback/harmonize: exiled instead of going anywhere else when it leaves the stack.
    let flashback = matches!(method, Method::Flashback | Method::FlashbackGranted | Method::Harmonize);
    let card = cx.s.obj(h).card;
    let f = cx.db.get(card).face(face);
    let Some(base_cost) = crate::altcast::base_cost_of(cx.s, h, f, method) else { return cx.unsupported("cast method not available") };
    let ability = f.spell.unwrap_or(permanent_spell_ability(cx.db));
    let reduction = cost_reduction(cx, p, h, face);
    let mode_choices = crate::altcast::affordable_mode_sets(cx, p, h, face, method);
    // CR 609.4b: a permission to cast it "and spend mana as though it were mana of any type"
    // (where the card is now, before it moves; `ext7`).
    let any_type = crate::ext7::any_type_permission(cx, p, h);
    // 601.2a: move to stack (as the chosen face, CR 715.3b).
    let from_grave = cx.s.obj(h).zone == Zone::Graveyard;
    let from_exile = cx.db.x9.any_from_exile && cx.s.obj(h).zone == Zone::Exile;
    let so = crate::altcast::put_on_stack_as_spell(cx.s, h, face, p);
    if from_grave {
        cx.s.cast_mut(so).flags |= crate::altcast::flag::FROM_GRAVE;
    }
    if from_exile {
        cx.s.cast_mut(so).flags |= crate::altcast::flag::FROM_EXILE;
    }
    // 601.2b: modes, alternative/additional costs (harmonize, spree, offspring, kicker...), X.
    let modes = if f.spell.is_some() { choose_modes_among(cx, p, mode_choices, so)? } else { SmallVec::new() };
    let mut req = requirement(&base_cost, 0);
    req.any_type = any_type;
    if f.spell.is_some() {
        crate::altcast::add_req(&mut req, &crate::altcast::mode_cost(cx.db, ability, &modes));
    }
    // "Choose one; if the additional cost was paid, choose both instead": the number of modes
    // chosen is the announcement of the optional cost (CR 601.2b).
    let forced_optional = if f.spell.is_some() { crate::ext2::charm_optcost_paid(cx.db, ability, &modes) } else { None };
    let mut ann = crate::altcast::announce_with(cx, p, so, face, method, &mut req, reduction, forced_optional)?;
    let reduction = reduction + ann.reduction;
    let mut kicked = false;
    let srcs = mana_sources_for(cx, p, None, Some(so));
    if let Some(k) = &f.kicker {
        let mut with = req.clone();
        let kr = requirement(k, 0);
        for (i, n) in kr.exact.iter().enumerate() {
            with.exact[i] += n;
        }
        with.hybrid.extend(kr.hybrid.iter().copied());
        with.generic += kr.generic;
        if payable(&reduce(with, reduction), &srcs) {
            kicked = cx.choose(Decision { player: p, kind: DecisionKind::Kicker, options: vec![Opt::No, Opt::Yes], subject: Some(so) })? == Opt::Yes;
        }
    }
    if kicked {
        let kr = requirement(f.kicker.as_ref().unwrap(), 0);
        for (i, n) in kr.exact.iter().enumerate() {
            req.exact[i] += n;
        }
        req.hybrid.extend(kr.hybrid.iter().copied());
        req.generic += kr.generic;
    }
    let mut x = 0;
    let life_x = f.spell.is_some_and(|a| cx.db.spell_life_x.contains(&a));
    if life_x {
        // "As an additional cost, pay X life": X is announced (CR 601.2b, 107.1b); a player can
        // pay life only up to their life total (CR 119.4).
        if base_cost.has_x() {
            return cx.unsupported("two X announcements");
        }
        let life = cx.s.player(p).life.max(0);
        let opts = cx.number_options(0, life, "pay X life")?;
        let Opt::Number(v) = cx.choose(Decision { player: p, kind: DecisionKind::ChooseX, options: opts, subject: Some(so) })? else { unreachable!() };
        x = v;
    }
    if base_cost.has_x() {
        let max = srcs.iter().map(|s| s.amount as i32).sum::<i32>();
        let opts: Vec<Opt> = (0..=max)
            .filter(|&xv| {
                let mut r = req.clone();
                r.generic += xv as u32;
                payable(&reduce(r, reduction), &srcs)
            })
            .map(Opt::Number)
            .collect();
        let Opt::Number(v) = cx.choose(Decision { player: p, kind: DecisionKind::ChooseX, options: opts, subject: Some(so) })? else { unreachable!() };
        x = v;
        req.generic += x as u32;
    }
    let targets = if f.spell.is_some() { choose_targets_ctx(cx, p, ability, &modes, so, x, TrigCtx::default())? } else { SmallVec::new() };
    if f.spell.is_some() && !targets.is_empty() {
        crate::ext2::choose_division(cx, p, so, ability, &modes, &targets, x)?; // CR 601.2d
    }
    // From here on (costs, 601.2f–h) its targets aren't interchangeable with identical objects;
    // among themselves for the division just made, they still are.
    set_paying_for(cx, &targets, None);
    // "You may sacrifice any number of ...; this spell costs {1} less for each" (601.2b, 601.2f).
    let sac_any = match &f.extra.sac_any_reduce {
        Some(fl) => crate::altcast::choose_sac_any(cx, p, so, fl, &req, reduction)?,
        None => vec![],
    };
    let reduction = reduction + sac_any.len() as u32;
    let req = reduce(req, reduction);
    cx.s.obj_mut(so).cast_mana = req.total().min(255) as u8;
    // Permanents chosen now for sacrifice / blight costs are paid with after the mana; until then
    // mana abilities may tap them, as sources or as fodder, but not sacrifice them (601.2g–h).
    let picked = crate::altcast::choose_before_mana(cx, p, so, &mut ann, &req)?;
    pay_mana_reserving(cx, p, req, None, so, true, Some(so), Some(ReserveFor::Picked(&picked)))?;
    crate::altcast::pay_announced(cx, p, so, &ann)?;
    if !sac_any.is_empty() {
        // 601.2h: the chosen permanents are sacrificed as the costs are paid.
        crate::trig_ext::fire_sacrificed(cx, &sac_any, p);
        let moves: Vec<crate::zones::Move> = sac_any.iter().filter(|&&o| cx.s.obj(o).zone == Zone::Battlefield).map(|&o| crate::zones::Move::new(o, Zone::Graveyard)).collect();
        crate::zones::move_batch(cx, &moves)?;
    }
    if life_x && x > 0 {
        crate::resolve::lose_life(cx.s, cx.db, p, x); // CR 119.4, 601.2h
    }
    cx.s.ext.x6.paying_for.clear();
    let tlist = targets.iter().map(|t| t.1).collect();
    let tobjs: SmallVec<[ObjId; 2]> = targets.iter().filter_map(|t| if let Target::Obj(o) = t.1 { Some(o) } else { None }).collect();
    let item = cx.s.push_stack(StackItem { kind: StackKind::Spell, controller: p, source: so, source_card: card, ability, targets, x, kicked, modes, trig: TrigCtx::default(), trigger_idx: None, flashback, uid: 0 });
    cx.s.turn_stats.spells_cast[p.idx()] += 1;
    cx.s.ext.cast_this_turn.push((p, so));
    crate::ext3::spell_cast(cx, p, so);
    crate::ext4::spell_cast(cx, p, so);
    fire_targeted(cx, &tobjs, so, item, p);
    crate::ext2::fire_crime_top(cx);
    cx.s.emit(Event::Cast { player: p, card, targets: tlist });
    if face != 0 {
        cx.s.emit(Event::CastFace { player: p, card, face });
    }
    crate::triggers::fire(cx.s, cx.db, &crate::triggers::TrigEvent::SpellCast { spell: so, caster: p });
    crate::ext6::spell_cast(cx, p, so);
    Ok(())
}

/// Cast a copy of a prepared permanent's prepare spell (CR 722.3c); the permanent becomes
/// unprepared as the spell becomes cast (601.2i). The copy is a spell, not a card: it ceases to
/// exist when it leaves the stack (704.5e).
pub fn cast_prepared(cx: &mut Ctx, p: PlayerId, perm: ObjId) -> R<()> {
    let card = cx.s.obj(perm).card;
    let copy = cx.s.new_token(card, p);
    cx.s.obj_mut(copy).zone = Zone::Exile; // the copy "in exile" (not listed: it only exists to be cast)
    cast_spell_alt(cx, p, copy, 1, None)?;
    cx.s.obj_mut(perm).prepared = false;
    Ok(())
}

/// Permanent spells have no spell ability; use a sentinel id that resolution special-cases.
pub fn permanent_spell_ability(_db: &CardDb) -> AbilityId {
    AbilityId(u32::MAX)
}

pub fn activate(cx: &mut Ctx, p: PlayerId, o: ObjId, a: AbilityId) -> R<()> {
    let ad = cx.db.ability(a).clone();
    let cost = ad.cost.clone().unwrap_or_default();
    let card = cx.s.obj(o).card;
    // CR 602.2a: the ability exists from its announcement, with its source: from its modes and
    // targets on, the source is not interchangeable with an identical object (targeting the
    // source itself or its twin differs; canonicalisation soundness test).
    set_paying_for(cx, &[], Some(o));
    let modes = choose_modes(cx, p, a, o)?;
    // "[-X]": X is announced before targets are chosen (CR 601.2b-c via 602.2b); at most the
    // loyalty it has (606.6).
    let x = if cost.x_loyalty { crate::ext6::announce_x_loyalty(cx, p, o)? } else { 0 };
    let targets = if target_dependent_cost(cx, a).is_some() {
        // Only targets that leave the (target-dependent) cost payable (CR 601.2c, 601.2f).
        let excl = if cost.tap { Some(o) } else { None };
        choose_targets_where(cx, p, a, &modes, o, 0, TrigCtx::default(), &|cx, t| {
            let req = ability_mana(cx, p, o, a, &[t]);
            payable_reserving(&req, &mana_sources_for(cx, p, excl, Some(crate::ext6::ABILITY_PAYMENT)), Reserve::of(cx, p, o, &cost).as_ref()) && !crate::loops::blocked(cx.s, o, a, &cost, &req)
        })?
    } else {
        choose_targets(cx, p, a, &modes, o)?
    };
    let tl: SmallVec<[Target; 2]> = targets.iter().map(|t| t.1).collect();
    set_paying_for(cx, &targets, Some(o));
    let mana_req = ability_mana(cx, p, o, a, &tl);
    if is_equip(&ad) {
        cx.s.ext.equips_this_turn[p.idx()] = cx.s.ext.equips_this_turn[p.idx()].saturating_add(1);
    }
    crate::effect_ext::count_activation(cx.s, cx.db, o, a);
    if crate::loops::is_free(&cost, &mana_req) {
        if crate::loops::free_capped(cx.s, o, a) {
            return cx.unsupported("free activation past the loop cap was offered");
        }
        crate::loops::record(cx.s, o, a);
    }
    if cost.tap {
        cx.s.obj_mut(o).tapped = true;
    }
    if cost.remove_any > 0 {
        crate::ext5::remove_any_counter(cx, p, o)?;
    }
    if let Some((k, n)) = cost.counter {
        crate::ext6::put_counters(cx.s, cx.db, o, k, n - x); // CR 606.4
        if cost.is_loyalty() {
            cx.s.turn_stats.loyalty_activated.push(o);
        }
    }
    let mut trig = TrigCtx::default();
    // CR 601.2g–h: mana abilities are activated before the costs are paid, so a creature-land
    // attacker returned by ninjutsu may still tap for mana first.
    pay_mana_reserving(cx, p, mana_req, if cost.tap { Some(o) } else { None }, o, false, Some(crate::ext6::ABILITY_PAYMENT), Some(ReserveFor::Cost(&cost)))?;
    if cost.return_unblocked {
        // Ninjutsu (CR 702.49a): reveal this card; return an unblocked attacker. The new
        // creature will attack what the returned one attacked (702.49c), kept in `trig`.
        cx.s.obj_mut(o).known_to = 0b11;
        let items = crate::query::sig_items(cx.s, cx.db, &unblocked_attackers(cx, p));
        let Some(Opt::Card(a)) = cx.choose_set(p, DecisionKind::PayCost, items, 1, 1, Some(o))?.into_iter().next() else { return cx.unsupported("ninjutsu without unblocked attacker") };
        trig.player = cx.s.combat.attackers.iter().find(|x| x.0 == a).map(|x| x.1);
        trig.obj = cx.s.combat.attacking_pw.iter().find(|x| x.0 == a).map(|x| x.1);
        crate::zones::move_batch(cx, &[crate::zones::Move::new(a, Zone::Hand)])?;
    }
    // The ability keeps referring to a sacrificed source (last known information).
    if let Some(exiled) = pay_other_costs_src(cx, p, o, &cost)? {
        trig.new_obj = Some(exiled); // craft: the card to return (CR 702.167a)
    }
    cx.s.ext.x6.paying_for.clear();
    let tobjs: SmallVec<[ObjId; 2]> = targets.iter().filter_map(|t| if let Target::Obj(o) = t.1 { Some(o) } else { None }).collect();
    let item = cx.s.push_stack(StackItem { kind: StackKind::Activated, controller: p, source: o, source_card: card, ability: a, targets, x, kicked: false, modes, trig, trigger_idx: None, flashback: false, uid: 0 });
    cx.s.emit(Event::Activate { player: p, card });
    // "Whenever you activate a loyalty ability" (CR 602.2: activated once on the stack).
    if !ad.is_mana {
        crate::ext6::ability_activated(cx, p);
    }
    if cost.is_loyalty() && cx.db.le.any_loyalty_trigger {
        crate::triggers::fire(cx.s, cx.db, &crate::triggers::TrigEvent::Ext(crate::trig_ext::ExtEvent::LoyaltyActivated { player: p, source: o }));
    }
    fire_targeted(cx, &tobjs, o, item, p);
    crate::ext2::fire_crime_top(cx);
    Ok(())
}

/// The targets just chosen for the spell / ability whose costs are paid next, and the source of
/// an ability being activated: its cost choices keep them apart from identical objects
/// (`XState::paying_for` targets, source: tapping the activated Resplendent
/// Angel for Springleaf Drum is not tapping its twin). A spell's source is on the stack already.
fn set_paying_for(cx: &mut Ctx, targets: &[(u8, Target)], source: Option<ObjId>) {
    let pf = &mut cx.s.ext.x6.paying_for;
    pf.clear();
    pf.extend(source);
    pf.extend(targets.iter().filter_map(|t| if let Target::Obj(o) = t.1 { Some(o) } else { None }));
}

/// "Becomes the target" triggers (ward) for a spell/ability just put on the stack (CR 702.21a):
/// stack object `item` (`StackItem::uid`) with source `by_source`.
pub fn fire_targeted(cx: &mut Ctx, targets: &[ObjId], by_source: ObjId, item: u32, controller: PlayerId) {
    // An object targeted more than once by the same spell/ability becomes its target once.
    for (i, &t) in targets.iter().enumerate() {
        if targets[..i].contains(&t) {
            continue;
        }
        crate::triggers::fire(cx.s, cx.db, &crate::triggers::TrigEvent::BecomesTarget { target: t, by_source, item, controller });
    }
    crate::ext4::note_targeted(cx.s, targets, controller);
}

// ---------------------------------------------------------------- mana payment

/// A permanent that can produce mana via a `{T}`-only mana ability.
#[derive(Clone, Debug)]
pub struct Src {
    pub obj: ObjId,
    pub ability: AbilityId,
    pub types: SmallVec<[ManaType; 5]>,
    pub amount: u8,
    /// Equivalence class: identical permanents are interchangeable for payment.
    pub sig: (CardId, AbilityId),
    /// Using it sacrifices it (Treasure) / costs life (Starting Town).
    pub sac: bool,
    pub life: i32,
    /// Mana types this source provides only through a restricted ability whose mana makes the
    /// spell uncounterable (Cavern of Souls), as a `ManaType` bitmask.
    pub nocounter_types: u8,
    /// Mana types whose production costs `life` (`ManaType` bitmask); 0 = per `life` alone.
    pub life_types: u8,
    /// Mana already in the player's mana pool (CR 106.4), not a permanent (`obj` is a sentinel).
    pub pool: bool,
    /// Mana types (`ManaType` bitmask) this source makes only through an ability that also
    /// costs tapping another untapped permanent (Springleaf Drum, Gene Pollinator, Scene of the
    /// Crime's second ability: `tapXType<1/..>`, CR 118.3): such a type needs one of `fodder`.
    pub fodder_types: u8,
    /// That ability (`Opt::Mana` names it for those types).
    pub fodder_ability: AbilityId,
    /// Untapped permanents it may tap for that cost (not itself, not excluded from the payment):
    /// any matching permanent, mana sources and summoning-sick creatures included (CR 302.6
    /// restricts only a creature's own {T} abilities). A mana source tapped this way
    /// makes no mana itself (`payable` accounts for it).
    pub fodder: SmallVec<[ObjId; 4]>,
    /// Spending restriction (CR 106.6) of the mana this source makes beyond what a payment
    /// uses, which goes to the mana pool (CR 106.4): that of the ability `amount` comes
    /// from. None: unrestricted.
    pub left: Option<crate::ext6::Restr>,
}

/// Sentinel object id of mana-pool pseudo-sources.
pub const POOL_OBJ: ObjId = ObjId(u32::MAX);

/// Remaining mana requirement of a cost.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Req {
    /// Colored + colorless-specific ({C}) needs, by ManaType index.
    pub exact: [u8; 6],
    pub hybrid: SmallVec<[(Color, Color); 2]>,
    pub generic: u32,
    /// Mana may be spent as though it were mana of any type (CR 609.4b; a cast permission
    /// with `MayPlayIgnoreType$`, `ext7`): once cost reductions are applied (`reduce`), every
    /// remaining symbol can be paid with any one mana.
    pub any_type: bool,
}
impl Req {
    /// With `any_type`: every remaining colored / colorless / hybrid symbol becomes one generic
    /// mana. Called after cost reductions, which only reduce the generic part (CR 601.2f).
    pub fn settle_any_type(&mut self) {
        if self.any_type {
            self.generic += self.exact.iter().map(|&n| n as u32).sum::<u32>() + self.hybrid.len() as u32;
            self.exact = [0; 6];
            self.hybrid.clear();
        }
    }
    pub fn is_empty(&self) -> bool {
        self.exact.iter().all(|&n| n == 0) && self.hybrid.is_empty() && self.generic == 0
    }
    pub fn total(&self) -> u32 {
        self.exact.iter().map(|&n| n as u32).sum::<u32>() + self.hybrid.len() as u32 + self.generic
    }
    /// Pay one mana of type `t` toward the most constrained matching part (exact > hybrid >
    /// generic). Returns false if it can't help.
    pub fn apply(&mut self, t: ManaType) -> bool {
        if self.exact[t as usize] > 0 {
            self.exact[t as usize] -= 1;
            return true;
        }
        if let Some(c) = t.color() {
            if let Some(i) = self.hybrid.iter().position(|&(a, b)| a == c || b == c) {
                self.hybrid.remove(i);
                return true;
            }
        }
        if self.generic > 0 {
            self.generic -= 1;
            return true;
        }
        false
    }
}

pub fn requirement(cost: &ManaCost, x: i32) -> Req {
    let mut r = Req::default();
    for s in &cost.syms {
        match *s {
            ManaSym::Generic(n) => r.generic += n as u32,
            ManaSym::Colored(c) => r.exact[c as usize] += 1,
            ManaSym::Colorless => r.exact[5] += 1,
            ManaSym::Hybrid(a, b) => r.hybrid.push((a, b)),
            ManaSym::X => r.generic += x.max(0) as u32,
            // Not in the current meta; payable as generic would be wrong, so treat as colored.
            ManaSym::TwoBrid(c) | ManaSym::Phyrexian(c) => r.exact[c as usize] += 1,
            ManaSym::Snow => r.generic += 1,
        }
    }
    r
}

pub fn mana_sources(cx: &Ctx, p: PlayerId, exclude: Option<ObjId>) -> Vec<Src> {
    mana_sources_for(cx, p, exclude, None)
}

/// Mana sources usable to pay for `spell` (None: an ability or other cost). Mana abilities with
/// a spending restriction (CR 106.6, "spend this mana only to cast ...") count only when the
/// restriction allows the spell; mana a payment doesn't use goes to the pool with its
/// restriction (`Src::left`).
pub fn mana_sources_for(cx: &Ctx, p: PlayerId, exclude: Option<ObjId>, spell: Option<ObjId>) -> Vec<Src> {
    let ex: SmallVec<[ObjId; 1]> = exclude.into_iter().collect();
    mana_sources_excluding(cx, p, &ex, spell)
}

/// `mana_sources_for` without the permanents in `exclude` (being tapped or sacrificed for other
/// parts of the cost): they are neither sources nor tapped for `tapXType` mana costs.
pub fn mana_sources_excluding(cx: &Ctx, p: PlayerId, exclude: &[ObjId], spell: Option<ObjId>) -> Vec<Src> {
    crate::query::frozen(cx.s, cx.db, || mana_sources_excluding_frozen(cx, p, exclude, spell))
}

fn mana_sources_excluding_frozen(cx: &Ctx, p: PlayerId, exclude: &[ObjId], spell: Option<ObjId>) -> Vec<Src> {
    let s = &*cx.s;
    let mut out = vec![];
    for &o in &s.battlefield {
        let ob = s.obj(o);
        if ob.controller != p || ob.tapped || exclude.contains(&o) {
            continue;
        }
        let ch = chars(s, cx.db, o);
        let sick = ch.types.has(CardType::Creature) && s.is_sick(o) && !ch.kws.has(Kw::Haste);
        if sick {
            continue;
        }
        // One source per permanent; if it has several mana abilities, union their types.
        let mut types: SmallVec<[ManaType; 5]> = SmallVec::new();
        let mut first = None;
        let (mut free_mask, mut nc_mask, mut life_mask, mut life_amt) = (0u8, 0u8, 0u8, 0i32);
        let mut amount = 1;
        // The ability the source's `amount` comes from (the largest; unrestricted on a tie) and
        // its spending restriction, kept by mana beyond a payment.
        let mut amt_of: Option<Option<crate::ext6::Restr>> = None;
        // "{T}, Tap an untapped [permanent] you control: Add ..." on this permanent.
        let mut tapx: Option<AbilityId> = None;
        for &a in &ch.abilities {
            let ad = cx.db.ability(a);
            if !ad.is_mana || crate::static_ext::activation_forbidden(s, cx.db, o, ad) || crate::ext6::gained_limit_reached(s, cx.db, o, a) {
                continue;
            }
            // "Spend this mana only to activate abilities" (CR 106.6).
            if !cx.db.x6.ability_mana.is_empty() && cx.db.x6.ability_mana.contains(&a) && spell != Some(crate::ext6::ABILITY_PAYMENT) {
                continue;
            }
            if ad.cost.as_ref().is_some_and(|c| c.tap_x.is_some()) {
                tapx.get_or_insert(a);
                continue;
            }
            // Memoized per read-only scope: a function of (state, source, ability, player).
            let ok = ad.activate_if.is_empty()
                || crate::query::memo_bool(s, cx.db, (1, o.0, a.0, p.0), || {
                    let ecx = crate::expr::EvalCtx { s, db: cx.db, you: p, source: o, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &[], imprinted: &[], base_only: false };
                    ad.activate_if.iter().all(|c| crate::expr::holds(c, &ecx))
                });
            if !ok {
                continue;
            }
            let mut nocounter = false;
            if let Some(r) = cx.db.mana_restrict.iter().find(|r| r.0 == a) {
                let fcx = FilterCtx { you: p, source: Some(o), x: 0, rem: &[] };
                match spell {
                    Some(sp) if sp != crate::ext6::ABILITY_PAYMENT && obj_matches(s, cx.db, &r.1, sp, fcx) => nocounter = r.2,
                    _ => continue,
                }
            }
            let prod: Option<(u8, u8)> = match &ad.effect {
                Effect::Mana { produced, amount: amt } => {
                    let mut mask = 0u8;
                    match produced {
                        Produced::One(t) => mask |= 1 << *t as u8,
                        Produced::AnyOf(v) => v.iter().for_each(|t| mask |= 1 << *t as u8),
                    }
                    Some((mask, *amt))
                }
                Effect::Ext(crate::effect_ext::ExtEffect::Mr(m)) => crate::ext4::mana_count(s, cx.db, o, p, m),
                Effect::Ext(x) => crate::effect_ext::dynamic_mana(s, cx.db, o, x).map(|m| (m, 1)),
                _ => None,
            };
            if let Some((mask, amt)) = prod {
                let c = ad.cost.as_ref().unwrap();
                // CR 106.5: an ability that would produce mana of an undefined type produces none.
                if c.pay_life > s.player(p).life || mask == 0 {
                    continue;
                }
                first.get_or_insert(a);
                let restr = crate::ext6::ability_restr(cx.db, a, o);
                match amt_of {
                    Some(r) if amt < amount || (amt == amount && (restr.is_some() || r.is_none())) => {}
                    _ => {
                        amount = amt;
                        amt_of = Some(restr);
                    }
                }
                for t in ManaType::all() {
                    if mask & (1 << t as u8) != 0 {
                        types.push(t);
                    }
                }
                if nocounter {
                    nc_mask |= mask;
                } else if c.pay_life > 0 {
                    life_mask |= mask;
                    life_amt = c.pay_life;
                } else {
                    free_mask |= mask;
                }
            }
        }
        // Both abilities of a land like Scene of the Crime ({T}: Add {C}; {T}, Tap an untapped
        // creature you control: Add one mana of any color) are one source: either makes one
        // mana, since both cost {T} (CR 118.3).
        let mut fodder_types = 0u8;
        let mut fodder: SmallVec<[ObjId; 4]> = SmallVec::new();
        let mut tapx_amount = None;
        if let Some(a) = tapx {
            let ad = cx.db.ability(a);
            if let Effect::Mana { produced, amount: amt } = &ad.effect {
                let f = &ad.cost.as_ref().unwrap().tap_x.as_ref().unwrap().1;
                let fcx = FilterCtx { you: p, source: Some(o), x: 0, rem: &[] };
                fodder = s.battlefield.iter().copied().filter(|&x| x != o && s.obj(x).controller == p && !s.obj(x).tapped && !exclude.contains(&x) && obj_matches(s, cx.db, f, x, fcx)).collect();
                if !fodder.is_empty() {
                    let mut mask = 0u8;
                    match produced {
                        Produced::One(t) => mask |= 1 << *t as u8,
                        Produced::AnyOf(v) => v.iter().for_each(|t| mask |= 1 << *t as u8),
                    }
                    let regular = types.iter().fold(0u8, |m, t| m | (1 << *t as u8));
                    fodder_types = mask & !regular;
                    for t in ManaType::all() {
                        if fodder_types & (1 << t as u8) != 0 {
                            types.push(t);
                        }
                    }
                    tapx_amount = Some(*amt);
                }
            }
        }
        if fodder_types == 0 {
            fodder.clear();
        }
        let tapx = tapx.filter(|_| fodder_types != 0);
        if let Some(a) = first.or(tapx) {
            types.sort();
            types.dedup();
            let c = cx.db.ability(a).cost.clone().unwrap_or_default();
            if first.is_none() {
                amount = tapx_amount.unwrap_or(1);
                amt_of = Some(crate::ext6::ability_restr(cx.db, a, o));
            }
            // Per mana type, the cheapest ability producing it: free, else uncounterable-
            // restricted, else paying life (Starting Town).
            let life_types = life_mask & !free_mask & !nc_mask;
            out.push(Src {
                obj: o,
                ability: a,
                types,
                amount,
                sig: (ob.card, a),
                sac: c.sac_self,
                life: if life_types != 0 { life_amt } else { c.pay_life },
                nocounter_types: nc_mask & !free_mask,
                life_types,
                pool: false,
                fodder_types,
                fodder_ability: tapx.unwrap_or(a),
                fodder,
                left: amt_of.flatten(),
            });
        }
    }
    // Mana in the pool (CR 106.4) is spent like a source of one fixed type per unit (mana with
    // a spending restriction only on a spell it allows, CR 106.6).
    out.extend(pool_srcs(crate::ext6::pool_avail(s, cx.db, p, spell)));
    out
}

/// Pool mana as pseudo-sources, one per mana type present (`Src::pool`).
fn pool_srcs(pool: [u8; 6]) -> impl Iterator<Item = Src> {
    ManaType::all().into_iter().filter(move |&t| pool[t as usize] > 0).map(move |t| Src {
        obj: POOL_OBJ,
        ability: AbilityId(u32::MAX),
        types: smallvec::smallvec![t],
        amount: pool[t as usize],
        sig: (CardId(u32::MAX), AbilityId(t as u32)),
        sac: false,
        life: 0,
        nocounter_types: 0,
        life_types: 0,
        pool: true,
        fodder_types: 0,
        fodder_ability: AbilityId(u32::MAX),
        fodder: SmallVec::new(),
        left: None,
    })
}

/// Does `p` make every mana decision with full fidelity (`XState::full_mana`): spending pool
/// mana is asked (CR 601.2h) instead of done automatically, and every activatable mana
/// ability is a priority option (no agent mana filter)?
pub fn full_mana(s: &State, p: PlayerId) -> bool {
    s.ext.x6.full_mana & (1 << p.0) != 0
}

/// Set `full_mana` for `p` (scenario `rules_options.mana_abilities_at_priority`, a human seat).
/// Set it before the game advances (it changes which decisions exist).
pub fn set_full_mana(s: &mut State, p: PlayerId, on: bool) {
    if on {
        s.ext.x6.full_mana |= 1 << p.0;
    } else {
        s.ext.x6.full_mana &= !(1 << p.0);
    }
}

/// `rules_options.mana_abilities_at_priority` for both players (scenario/v0 and a front end's
/// new game): true = full fidelity (`set_full_mana`: every activatable mana ability is a priority
/// option, CR 605.3a, and pool spending is asked, CR 601.2h); false = no mana ability at priority
/// (`XState::no_priority_mana`; pool spending stays automatic). Without the option agents get
/// the agent mana filter's selection. Set it before the game advances.
pub fn set_mana_abilities_at_priority(s: &mut State, on: bool) {
    for p in [PlayerId(0), PlayerId(1)] {
        set_full_mana(s, p, on);
    }
    s.ext.x6.no_priority_mana = if on { 0 } else { 0b11 };
}

/// Book one mana of type `t` spent from the pool on `subject` (converge, "spent to cast").
fn note_pool_spent(cx: &mut Ctx, subject: ObjId, spell: bool, t: ManaType) {
    crate::altcast::note_spent(cx.s, subject, t, 1);
    if spell {
        let tbit = 1u8 << t as u8;
        match cx.s.ext.spent_colors.iter_mut().find(|x| x.0 == subject) {
            Some(x) => x.1 |= tbit,
            None => cx.s.ext.spent_colors.push((subject, tbit)),
        }
    }
}

/// Spend pool mana on `req` automatically (agent decisions): as long as some pool mana
/// helps and the rest of the cost stays payable from the remaining pool and `srcs`, spend one
/// unit, preferring (1) a type the cost needs exactly, (2) restricted mana the payment may use
/// (`ext6::spend_pool` takes restricted units of a type first), (3) {C}, (4) the type the pool
/// holds most of (keeping the most flexible, varied colours), (5) WUBRGC order. Pool mana is
/// spent before any permanent is tapped: it empties at the end of the step, permanents don't.
/// Fidelity: which pool mana pays is never asked here, for agents and the full-fidelity human
/// seat alike (a known approximation: floating mana is spent automatically).
fn auto_spend_pool(cx: &mut Ctx, p: PlayerId, req: &mut Req, subject: ObjId, spell: bool, sel: Option<ObjId>, srcs: &[Src]) -> R<()> {
    while !req.is_empty() {
        let Some(t) = auto_pool_pick(cx, p, req, sel, srcs) else { break };
        crate::ext6::spend_pool(cx, p, t, sel)?;
        req.apply(t);
        note_pool_spent(cx, subject, spell, t);
    }
    Ok(())
}

/// The pool unit `auto_spend_pool` spends next on `req`, if any (see there).
fn auto_pool_pick(cx: &Ctx, p: PlayerId, req: &Req, sel: Option<ObjId>, srcs: &[Src]) -> Option<ManaType> {
    let pool = crate::ext6::pool_avail(cx.s, cx.db, p, sel);
    if pool == [0; 6] {
        return None;
    }
    let mut best: Option<((bool, bool, bool, u8, u8), ManaType)> = None;
    for t in ManaType::all() {
        let n = pool[t as usize];
        if n == 0 {
            continue;
        }
        let mut r2 = req.clone();
        if !r2.apply(t) {
            continue;
        }
        let mut left = pool;
        left[t as usize] -= 1;
        let mut rest: Vec<Src> = pool_srcs(left).collect();
        rest.extend(srcs.iter().cloned());
        if !payable(&r2, &rest) {
            continue;
        }
        let restricted = crate::ext6::restricted_avail(cx.s, cx.db, p, t, sel) > 0;
        let key = (req.exact[t as usize] == 0, !restricted, t != ManaType::C, u8::MAX - n, t as u8);
        if best.as_ref().is_none_or(|b| key < b.0) {
            best = Some((key, t));
        }
    }
    best.map(|b| b.1)
}

/// Non-mana parts of a cost paid with permanents after the mana abilities (CR 601.2g before
/// 601.2h) that a mana payment could consume: "Sacrifice [an artifact]" / "Sacrifice this" (a
/// mana ability that sacrifices its permanent, e.g. Treasure, removes it) and "Tap an untapped
/// [creature] you control" (a permanent tapped for mana, or tapped as another source's fodder,
/// is no longer untapped). One object can't pay two parts (CR 118.3; Junk Jet ruling
/// 2024-03-08: "You can't sacrifice a Junk token to pay multiple costs").
#[derive(Clone, Debug, Default)]
pub struct Reserve {
    /// (how many, candidates with their interchangeability class, consumed by tapping?)
    parts: SmallVec<[(u8, Vec<(ObjId, ObjSig)>, bool); 2]>,
}

impl Reserve {
    /// The parts of `c` (paid for `src`'s ability by `p`) that mana payment can consume, with
    /// their candidates in the current state. None when there are none.
    pub fn of(cx: &Ctx, p: PlayerId, src: ObjId, c: &Cost) -> Option<Reserve> {
        if c.sac.is_none() && c.tap_x.is_none() && !c.sac_self {
            return None;
        }
        // Read-only: a memo scope on big boards and one `obj_sigs` scan per part (same values as an
        // `obj_sig` per candidate, which scans the whole state each time).
        crate::query::frozen_if_big(cx.s, cx.db, || {
            let mut r = Reserve::default();
            let classes = |objs: Vec<ObjId>| crate::query::obj_sigs(cx.s, cx.db, &objs).into_iter().zip(objs).map(|(k, o)| (o, k)).collect::<Vec<_>>();
            if c.sac_self && cx.s.obj(src).zone == Zone::Battlefield {
                r.parts.push((1, classes(vec![src]), false));
            }
            if let Some((n, f)) = &c.sac {
                r.parts.push((*n, classes(cost_candidates(cx, p, src, f, Zone::Battlefield, false, !c.sac_self)), false));
            }
            if let Some((n, f)) = &c.tap_x {
                r.parts.push((*n, classes(cost_candidates(cx, p, src, f, Zone::Battlefield, true, !c.tap)), true));
            }
            Some(r)
        })
    }

    /// Permanents already chosen for a spell's sacrifice / blight additional costs (chosen with
    /// the costs before the mana abilities, `altcast::choose_before_mana`; paid at CR 601.2h):
    /// each is a one-candidate part consumed only by sacrificing it. A mana ability that
    /// sacrifices it (Treasure) is unusable; tapping it, for its own mana or as another source's
    /// fodder (Gene Pollinator), is fine: mana abilities are activated before costs are paid
    /// (601.2g) and a tapped permanent can still be sacrificed or get counters (601.2h). None
    /// when `picked` is empty.
    pub fn picked(cx: &Ctx, picked: &[ObjId]) -> Option<Reserve> {
        if picked.is_empty() {
            return None;
        }
        let sigs = crate::query::frozen_if_big(cx.s, cx.db, || crate::query::obj_sigs(cx.s, cx.db, picked));
        Some(Reserve { parts: picked.iter().zip(sigs).map(|(&o, k)| (1, vec![(o, k)], false)).collect() })
    }

    /// After a mana pick consumed `o` (`sacrificed`: its mana ability sacrificed it; otherwise
    /// it was tapped, as a source or as fodder).
    fn without(&self, o: ObjId, sacrificed: bool) -> Reserve {
        let mut r = self.clone();
        for (_, cands, tap) in r.parts.iter_mut() {
            if sacrificed || *tap {
                cands.retain(|c| c.0 != o);
            }
        }
        r
    }
}

/// What a mana payment (`pay_mana_reserving`) must leave payable besides the mana (`Reserve`).
#[derive(Clone, Copy, Debug)]
pub enum ReserveFor<'a> {
    /// The other parts of the cost being paid (an activated ability's, a resolution-time
    /// cost's): `Reserve::of`.
    Cost(&'a Cost),
    /// Permanents already chosen for a spell's additional costs: `Reserve::picked`.
    Picked(&'a [ObjId]),
}

impl ReserveFor<'_> {
    /// The reserve in the current state (`subject`: the object whose cost is paid).
    fn reserve(&self, cx: &Ctx, p: PlayerId, subject: ObjId) -> Option<Reserve> {
        match self {
            ReserveFor::Cost(c) => Reserve::of(cx, p, subject, c),
            ReserveFor::Picked(objs) => Reserve::picked(cx, objs),
        }
    }
}

/// Can `req` be paid from `srcs` while every part of `r` keeps enough candidates (CR 118.3,
/// 601.2g–h)? Exact: some set of objects is reserved for the parts (each part's candidates that
/// no source could consume first, then every distinct choice among the rest), and the mana is
/// paid without consuming any of them.
pub fn payable_reserving(req: &Req, srcs: &[Src], r: Option<&Reserve>) -> bool {
    let Some(r) = r else { return payable(req, srcs) };
    // Could paying mana from `srcs` consume `o` for a part (sacrifice / tap)?
    let conflicts = |o: ObjId, tap: bool| srcs.iter().any(|x| !x.pool && ((x.obj == o && (tap || x.sac)) || (tap && x.fodder.contains(&o))));
    // Per part: how many conflicting candidates it must use, grouped by class.
    let mut needs: SmallVec<[(usize, Vec<Vec<ObjId>>, bool); 2]> = SmallVec::new();
    for (n, cands, tap) in &r.parts {
        let free = cands.iter().filter(|c| !conflicts(c.0, *tap)).count();
        if free >= *n as usize {
            continue;
        }
        let mut groups: Vec<(ObjSig, Vec<ObjId>)> = vec![];
        for c in cands.iter().filter(|c| conflicts(c.0, *tap)) {
            match groups.iter_mut().find(|g| g.0 == c.1) {
                Some(g) => g.1.push(c.0),
                None => groups.push((c.1, vec![c.0])),
            }
        }
        let k = *n as usize - free;
        if groups.iter().map(|g| g.1.len()).sum::<usize>() < k {
            return false;
        }
        needs.push((k, groups.into_iter().map(|g| g.1).collect(), *tap));
    }
    if needs.is_empty() {
        return payable(req, srcs);
    }
    // Per part: every distinct way to take `k` of its conflicting candidates (by class).
    fn combos(k: usize, groups: &[Vec<ObjId>], cur: &mut Vec<ObjId>, out: &mut Vec<Vec<ObjId>>) {
        if k == 0 {
            out.push(cur.clone());
            return;
        }
        let Some((g, rest)) = groups.split_first() else { return };
        for take in (0..=k.min(g.len())).rev() {
            cur.extend_from_slice(&g[..take]);
            combos(k - take, rest, cur, out);
            cur.truncate(cur.len() - take);
        }
    }
    let per_part: Vec<(Vec<Vec<ObjId>>, bool)> = needs
        .iter()
        .map(|(k, groups, tap)| {
            let mut out = vec![];
            combos(*k, groups, &mut vec![], &mut out);
            (out, *tap)
        })
        .collect();
    fn go(i: usize, per_part: &[(Vec<Vec<ObjId>>, bool)], sac: &mut Vec<ObjId>, tap: &mut Vec<ObjId>, req: &Req, srcs: &[Src]) -> bool {
        if i == per_part.len() {
            // Reserved for a sacrifice: its self-sacrificing mana ability is unusable (tapping it
            // as fodder is fine). Reserved for tapping: not usable at all.
            let rest: Vec<Src> = rest_without(srcs, tap).iter().filter(|x| x.pool || !(x.sac && sac.contains(&x.obj))).cloned().collect();
            return payable(req, &rest);
        }
        let (choices, is_tap) = &per_part[i];
        for ch in choices {
            if ch.iter().any(|o| sac.contains(o) || tap.contains(o)) {
                continue; // one object pays one part
            }
            let before = if *is_tap { tap.len() } else { sac.len() };
            if *is_tap {
                tap.extend_from_slice(ch);
            } else {
                sac.extend_from_slice(ch);
            }
            let ok = go(i + 1, per_part, sac, tap, req, srcs);
            if *is_tap {
                tap.truncate(before);
            } else {
                sac.truncate(before);
            }
            if ok {
                return true;
            }
        }
        false
    }
    go(0, &per_part, &mut vec![], &mut vec![], req, srcs)
}

/// Can `req` be paid using each source at most once? A source with `fodder_types` makes those
/// types only by tapping one of its `fodder` permanents (distinct per source); a fodder
/// permanent that is itself one of `srcs` then makes no mana. Fodder not among `srcs`
/// is free (not a source, or not usable for this payment any more).
pub fn payable(req: &Req, srcs: &[Src]) -> bool {
    if srcs.iter().all(|x| x.fodder_types == 0) {
        return payable_plain(req, srcs);
    }
    let tap: SmallVec<[usize; 4]> = (0..srcs.len()).filter(|&i| srcs[i].fodder_types != 0).collect();
    let mut st = FodderSearch { srcs, tap, with: SmallVec::new(), consumed: SmallVec::new(), producing: SmallVec::new() };
    st.with = smallvec::smallvec![false; st.tap.len()];
    st.go(0, req)
}

/// Depth-first assignment of fodder to the `tap` sources of `payable`.
struct FodderSearch<'a> {
    srcs: &'a [Src],
    tap: SmallVec<[usize; 4]>,
    /// Per `tap` source: uses its fodder ability (all its types) or not (only its other types).
    with: SmallVec<[bool; 4]>,
    /// Permanents tapped as fodder.
    consumed: SmallVec<[ObjId; 4]>,
    /// `tap` sources that tap themselves for their fodder ability (can't be fodder).
    producing: SmallVec<[ObjId; 4]>,
}

impl FodderSearch<'_> {
    fn go(&mut self, k: usize, req: &Req) -> bool {
        let srcs = self.srcs;
        if k == self.tap.len() {
            let list: SmallVec<[Src; 16]> = srcs
                .iter()
                .enumerate()
                .filter(|(_, x)| x.pool || !self.consumed.contains(&x.obj))
                .filter_map(|(i, x)| match self.tap.iter().position(|&t| t == i) {
                    Some(j) if !self.with[j] => {
                        let types: SmallVec<[ManaType; 5]> = x.types.iter().copied().filter(|t| x.fodder_types & (1 << *t as u8) == 0).collect();
                        (!types.is_empty()).then(|| Src { types, fodder_types: 0, fodder: SmallVec::new(), ..x.clone() })
                    }
                    _ => Some(x.clone()),
                })
                .collect();
            return payable_plain(req, &list);
        }
        let i = self.tap[k];
        let o = srcs[i].obj;
        self.with[k] = false;
        if self.go(k + 1, req) {
            return true;
        }
        if self.consumed.contains(&o) {
            return false;
        }
        self.with[k] = true;
        self.producing.push(o);
        // Interchangeable fodder is tried once: a source by its (card, ability, types, amount),
        // anything else by which of the `tap` sources may tap it.
        let mut tried: SmallVec<[(bool, (CardId, AbilityId), u8, u32); 8]> = SmallVec::new();
        for &f in &srcs[i].fodder {
            if f == o || self.consumed.contains(&f) || self.producing.contains(&f) {
                continue;
            }
            let key = match srcs.iter().find(|x| x.obj == f && !x.pool) {
                Some(x) => (true, x.sig, x.types.iter().fold(0u8, |m, t| m | (1 << *t as u8)) | x.fodder_types.rotate_left(6), x.amount as u32),
                None => (false, (CardId(0), AbilityId(0)), 0, self.tap.iter().enumerate().filter(|(_, &t)| srcs[t].fodder.contains(&f)).fold(0u32, |m, (j, _)| m | (1 << j))),
            };
            if tried.contains(&key) {
                continue;
            }
            tried.push(key);
            self.consumed.push(f);
            let ok = self.go(k + 1, req);
            self.consumed.pop();
            if ok {
                self.producing.pop();
                self.with[k] = false;
                return true;
            }
        }
        self.producing.pop();
        self.with[k] = false;
        false
    }
}

/// `payable` for sources without fodder costs: a small backtracking matcher.
fn payable_plain(req: &Req, srcs: &[Src]) -> bool {
    let units: u32 = srcs.iter().map(|s| s.amount as u32).sum();
    if units < req.total() {
        return false;
    }
    // Specific needs: (allowed colors mask over ManaType indices)
    let mut needs: SmallVec<[u8; 8]> = SmallVec::new();
    for (i, &n) in req.exact.iter().enumerate() {
        for _ in 0..n {
            needs.push(1 << i);
        }
    }
    for &(a, b) in &req.hybrid {
        needs.push((1 << a as u8) | (1 << b as u8));
    }
    // Most constrained first.
    needs.sort_by_key(|m| m.count_ones());
    let masks: SmallVec<[(u8, u8); 16]> = srcs.iter().map(|s| (s.types.iter().fold(0u8, |m, t| m | (1 << *t as u8)), s.amount)).collect();
    let mut left: SmallVec<[u8; 16]> = masks.iter().map(|m| m.1).collect();
    fn go(i: usize, needs: &[u8], masks: &[(u8, u8)], left: &mut [u8]) -> bool {
        if i == needs.len() {
            return true;
        }
        for j in 0..masks.len() {
            if left[j] > 0 && masks[j].0 & needs[i] != 0 {
                left[j] -= 1;
                if go(i + 1, needs, masks, left) {
                    return true;
                }
                left[j] += 1;
            }
        }
        false
    }
    if !go(0, &needs, &masks, &mut left) {
        return false;
    }
    // Generic: any leftover units. (Matching above is existence-only; leftover count is the same
    // for every successful matching since each need consumes exactly one unit.)
    units - needs.len() as u32 >= req.generic
}

/// `srcs` without the sources whose permanent is in `gone` (tapped for this payment), and with
/// `gone` removed from every fodder list.
fn rest_without<'a>(srcs: &'a [Src], gone: &[ObjId]) -> std::borrow::Cow<'a, [Src]> {
    if !srcs.iter().any(|x| !x.fodder.is_empty() || (!x.pool && gone.contains(&x.obj))) {
        return std::borrow::Cow::Borrowed(srcs);
    }
    srcs.iter()
        .filter(|x| x.pool || !gone.contains(&x.obj))
        .map(|x| {
            let mut x = x.clone();
            if !x.fodder.is_empty() {
                x.fodder.retain(|f| !gone.contains(f));
                if x.fodder.is_empty() && x.fodder_types != 0 {
                    let ft = x.fodder_types;
                    x.types.retain(|t| ft & (1 << *t as u8) == 0);
                    x.fodder_types = 0;
                }
            }
            x
        })
        .filter(|x| !x.types.is_empty())
        .collect::<Vec<_>>()
        .into()
}

/// Pay a mana cost by activating mana abilities one at a time (CR 601.2g–h). Only the *set* of
/// sources (and the mana type each produces) matters, not the order, so sources are taken in a
/// canonical order (signature, id): each pick must come after the previous one, identical
/// permanents are one option, and a pick is offered only if the rest stays payable from later
/// sources. A payment with no real choice is forced.
pub fn pay_mana(cx: &mut Ctx, p: PlayerId, req: Req, exclude: Option<ObjId>, subject: ObjId) -> R<()> {
    pay_mana_for(cx, p, req, exclude, subject, false)
}

/// `pay_mana`; `spell`: the cost is the total cost of casting the spell `subject` (so mana with
/// spending restrictions may be used, and converge/uncounterable are tracked).
pub fn pay_mana_for(cx: &mut Ctx, p: PlayerId, req: Req, exclude: Option<ObjId>, subject: ObjId, spell: bool) -> R<()> {
    pay_mana_sel(cx, p, req, exclude, subject, spell, if spell { Some(subject) } else { None })
}

/// `pay_mana_for` with the mana-source selector given (`ext6::ABILITY_PAYMENT` for an
/// activated ability's cost).
pub fn pay_mana_sel(cx: &mut Ctx, p: PlayerId, req: Req, exclude: Option<ObjId>, subject: ObjId, spell: bool, sel: Option<ObjId>) -> R<()> {
    pay_mana_reserving(cx, p, req, exclude, subject, spell, sel, None)
}

/// `pay_mana_sel` for a cost whose other parts (`reserve_for`, paid after the mana for
/// `subject`) take permanents: every pick leaves them payable (`Reserve`, CR 118.3).
#[allow(clippy::too_many_arguments)]
pub fn pay_mana_reserving(cx: &mut Ctx, p: PlayerId, mut req: Req, exclude: Option<ObjId>, subject: ObjId, spell: bool, sel: Option<ObjId>, reserve_for: Option<ReserveFor>) -> R<()> {
    let (pool_srcs, srcs): (Vec<Src>, Vec<Src>) = mana_sources_for(cx, p, exclude, sel).into_iter().partition(|s| s.pool);
    // Interchangeable sources: same card and ability, and the same object signature (a land
    // with counters, an aura, a link, ... differs from a plain copy: `query::obj_sig`).
    let osigs = crate::query::obj_sigs(cx.s, cx.db, &srcs.iter().map(|x| x.obj).collect::<Vec<_>>());
    let mut keyed: Vec<((CardId, AbilityId), crate::query::ObjSig, Src)> = srcs.into_iter().zip(osigs).map(|(x, o)| (x.sig, o, x)).collect();
    keyed.sort_by_key(|k| (k.0, k.1, k.2.obj));
    let (mut sigs, mut srcs): (Vec<((CardId, AbilityId), crate::query::ObjSig)>, Vec<Src>) = keyed.into_iter().map(|(a, b, x)| ((a, b), x)).unzip();
    let mut next = 0usize;
    // Mana already in the pool (CR 106.4) is spent before any permanent is tapped: by the payer
    // with full fidelity (`full_mana`), in canonical order (units by non-decreasing mana type,
    // so each multiset of pool mana plus set of sources is reachable by exactly one sequence);
    // otherwise automatically (`auto_spend_pool`).
    let full = full_mana(cx.s, p);
    // A full-fidelity seat (`Fidelity::Full`, the human): every
    // step of the payment is posed through `Ctx::choose_full` with a full form over the agent form
    // computed here as for an agent seat: any unused source in any order (CR 601.2g, 605.3a).
    // Pool mana is spent for it automatically as for an agent seat (a known
    // approximation), so after the pool phase the pool
    // is no resource of the payment (as in the agent form); only with `full_mana` (its agent form
    // asks the pool) does the full form also offer pool mana at any point (CR 601.2h, 106.4).
    // `hf_dev`: the seat took a non-agent option in this payment, so there is no agent form any
    // more (`src_used` then tracks the sources tapped, in any order).
    let hf = cx.full(p);
    let mut src_used: Vec<bool> = if hf { vec![false; srcs.len()] } else { vec![] };
    let mut hf_dev = false;
    if !full && !pool_srcs.is_empty() {
        auto_spend_pool(cx, p, &mut req, subject, spell, sel, &srcs)?;
    }
    let mut pool_open = full && !pool_srcs.is_empty();
    let mut pool_min = 0usize;
    while !req.is_empty() {
        let mut opts: Vec<Opt> = vec![];
        let mut picks: Vec<usize> = vec![];
        let mut seen: SmallVec<[(((CardId, AbilityId), crate::query::ObjSig), ManaType); 8]> = SmallVec::new();
        // The cost's other parts in the current state (picks so far consumed candidates for real).
        let reserve = reserve_for.and_then(|r| r.reserve(cx, p, subject));
        if hf_dev {
            // no agent form
        } else if pool_open {
            let pool = crate::ext6::pool_avail(cx.s, cx.db, p, sel);
            for t in ManaType::all() {
                if (t as usize) < pool_min || pool[t as usize] == 0 {
                    continue;
                }
                let mut r2 = req.clone();
                if !r2.apply(t) {
                    continue;
                }
                let mut left = pool;
                left[t as usize] -= 1;
                let mut rest: Vec<Src> = pool_srcs.iter().filter(|x| x.types[0] as usize >= t as usize && left[x.types[0] as usize] > 0).cloned().map(|mut x| {
                    x.amount = left[x.types[0] as usize];
                    x
                }).collect();
                rest.extend(srcs.iter().cloned());
                if payable_reserving(&r2, &rest, reserve.as_ref()) {
                    opts.push(Opt::PoolMana(t));
                    picks.push(usize::MAX);
                }
            }
        }
        // Fodder lists follow the state: a permanent tapped since (as a source or as fodder)
        // can't be tapped for a cost (CR 118.3).
        for sr in srcs.iter_mut().skip(if hf { 0 } else { next }) {
            if !sr.fodder.is_empty() {
                sr.fodder.retain(|f| cx.s.obj(*f).zone == Zone::Battlefield && !cx.s.obj(*f).tapped);
            }
        }
        let agent_srcs = if hf_dev { srcs.len() } else { next };
        for i in agent_srcs..srcs.len() {
            let sr = &srcs[i];
            for &t in &sr.types {
                if seen.contains(&(sigs[i], t)) {
                    continue;
                }
                let mut r2 = req.clone();
                let mut helped = false;
                for _ in 0..sr.amount {
                    helped |= r2.apply(t);
                }
                if !helped {
                    continue;
                }
                let after = reserve.as_ref().map(|r| r.without(sr.obj, sr.sac));
                let ok = if sr.fodder_types & (1 << t as u8) != 0 {
                    // Some fodder must leave the rest payable without it (CR 118.3).
                    sr.fodder.iter().any(|&f| payable_reserving(&r2, &rest_without(&srcs[i + 1..], &[sr.obj, f]), after.as_ref().map(|r| r.without(f, false)).as_ref()))
                } else {
                    payable_reserving(&r2, &rest_without(&srcs[i + 1..], &[sr.obj]), after.as_ref())
                };
                if ok {
                    seen.push((sigs[i], t));
                    let a = if sr.fodder_types & (1 << t as u8) != 0 { sr.fodder_ability } else { sr.ability };
                    opts.push(Opt::Mana(sr.obj, a, t));
                    picks.push(i);
                }
            }
        }
        // The full form: the agent form plus every unused source whose mana helps while the rest
        // stays payable from every other unused source (in any order); with `full_mana` also all
        // pool mana, the pool counting as a resource (otherwise it was spent automatically).
        let full_opts = hf.then(|| {
            let mut fo = opts.clone();
            let mut fp = picks.clone();
            let pool = if full { crate::ext6::pool_avail(cx.s, cx.db, p, sel) } else { [0; 6] };
            let unused: Vec<usize> = (0..srcs.len()).filter(|&k| !src_used[k]).collect();
            let rest_of = |pool: [u8; 6], skip: usize| -> Vec<Src> { self::pool_srcs(pool).chain(unused.iter().filter(|&&k| k != skip).map(|&k| srcs[k].clone())).collect() };
            for t in ManaType::all() {
                if pool[t as usize] == 0 || fo.contains(&Opt::PoolMana(t)) {
                    continue;
                }
                let mut r2 = req.clone();
                if !r2.apply(t) {
                    continue;
                }
                let mut left = pool;
                left[t as usize] -= 1;
                if payable_reserving(&r2, &rest_of(left, usize::MAX), reserve.as_ref()) {
                    fo.push(Opt::PoolMana(t));
                    fp.push(usize::MAX);
                }
            }
            let mut fseen = seen.clone();
            for &j in &unused {
                let sr = &srcs[j];
                for &t in &sr.types {
                    if fseen.contains(&(sigs[j], t)) {
                        continue;
                    }
                    let mut r2 = req.clone();
                    let mut helped = false;
                    for _ in 0..sr.amount {
                        helped |= r2.apply(t);
                    }
                    if !helped {
                        continue;
                    }
                    let rest = rest_of(pool, j);
                    let after = reserve.as_ref().map(|r| r.without(sr.obj, sr.sac));
                    let ok = if sr.fodder_types & (1 << t as u8) != 0 {
                        sr.fodder.iter().any(|&f| payable_reserving(&r2, &rest_without(&rest, &[sr.obj, f]), after.as_ref().map(|r| r.without(f, false)).as_ref()))
                    } else {
                        payable_reserving(&r2, &rest_without(&rest, &[sr.obj]), after.as_ref())
                    };
                    if ok {
                        fseen.push((sigs[j], t));
                        let a = if sr.fodder_types & (1 << t as u8) != 0 { sr.fodder_ability } else { sr.ability };
                        let o = Opt::Mana(sr.obj, a, t);
                        if !fo.contains(&o) {
                            fo.push(o);
                            fp.push(j);
                        }
                    }
                }
            }
            (fo, fp)
        });
        if opts.is_empty() && full_opts.as_ref().is_none_or(|f| f.0.is_empty()) {
            let names: Vec<String> = srcs[next..].iter().map(|x| format!("{}{:?}", cx.db.get(cx.s.obj(x.obj).card).name, x.types)).collect();
            return cx.unsupported(format!("mana payment dead end: {req:?} for {} from {names:?}", cx.db.get(cx.s.obj(subject).card).name));
        }
        let c = cx.choose_full(Decision { player: p, kind: DecisionKind::PayMana, options: opts.clone(), subject: Some(subject) }, full_opts.as_ref().map(|f| f.0.clone()))?;
        if hf && !opts.contains(&c) {
            hf_dev = true;
        }
        let (opts, picks) = full_opts.unwrap_or((opts, picks));
        if let Opt::PoolMana(t) = c {
            crate::ext6::spend_pool(cx, p, t, sel)?;
            pool_min = t as usize;
            req.apply(t);
            note_pool_spent(cx, subject, spell, t);
            continue;
        }
        pool_open = false;
        let Opt::Mana(o, _a, t) = c else { unreachable!() };
        crate::ext6::note_gained_use(cx.s, cx.db, o, _a);
        let i = picks[cx.offered_ix(c, &opts)];
        cx.swap_alt(c, i, &mut srcs, |x| x.obj == o)?;
        next = next.max(i + 1); // agent options: i >= next
        if hf {
            src_used[i] = true;
        }
        cx.s.obj_mut(o).tapped = true;
        if srcs[i].fodder_types & (1 << t as u8) != 0 {
            // Tap another untapped permanent as part of the cost (Springleaf Drum, Gene
            // Pollinator: any matching permanent, a mana source included), keeping the rest
            // of the payment possible.
            let mut r_after = req.clone();
            for _ in 0..srcs[i].amount {
                r_after.apply(t);
            }
            let reserve = reserve_for.and_then(|r| r.reserve(cx, p, subject));
            let items_with = |rest: &[Src]| -> Vec<ObjId> {
                srcs[i]
                    .fodder
                    .iter()
                    .copied()
                    .filter(|&fd| fd != o && !cx.s.obj(fd).tapped && payable_reserving(&r_after, &rest_without(rest, &[o, fd]), reserve.as_ref().map(|r| r.without(fd, false)).as_ref()))
                    .collect::<Vec<ObjId>>()
            };
            let items_with = |rest: &[Src]| crate::query::sig_items(cx.s, cx.db, &items_with(rest));
            // The agent form (an agent option taken so far: every source after `i` is unused).
            let items = if hf_dev { vec![] } else { items_with(&srcs[i + 1..]) };
            // The full form: the rest of the payment from the pool and every other unused source.
            let full_items = hf.then(|| {
                let pool = if full { crate::ext6::pool_avail(cx.s, cx.db, p, sel) } else { [0; 6] };
                let rest: Vec<Src> = self::pool_srcs(pool).chain((0..srcs.len()).filter(|&k| !src_used[k]).map(|k| srcs[k].clone())).collect();
                items_with(&rest)
            });
            if items.is_empty() && full_items.as_ref().is_none_or(|f| f.is_empty()) {
                return cx.unsupported(format!("tap-another-permanent mana cost dead end: {} for {}", cx.db.get(cx.s.obj(o).card).name, cx.db.get(cx.s.obj(subject).card).name));
            }
            let chosen = match full_items {
                Some(full_items) => {
                    let agent_ids: Vec<Opt> = items.iter().map(|x| x.1).collect();
                    let c = cx.choose_one_full(p, DecisionKind::PayCost, items, full_items, Some(o))?;
                    if !agent_ids.contains(&c) {
                        hf_dev = true;
                    }
                    vec![c]
                }
                None => cx.choose_set(p, DecisionKind::PayCost, items, 1, 1, Some(o))?,
            };
            for c in chosen {
                if let Opt::Card(fd) = c {
                    cx.s.obj_mut(fd).tapped = true;
                    // A later source tapped as fodder makes no mana now (CR 118.3); with any-order
                    // payment (full fidelity) an earlier unused one neither.
                    if let Some(k) = (i + 1..srcs.len()).find(|&k| srcs[k].obj == fd) {
                        srcs.remove(k);
                        sigs.remove(k);
                        if hf {
                            src_used.remove(k);
                            if k < next {
                                next -= 1;
                            }
                        }
                    } else if hf {
                        if let Some(k) = (0..i).find(|&k| srcs[k].obj == fd) {
                            src_used[k] = true;
                        }
                    }
                }
            }
        }
        if let Some(k) = crate::ext4::mana_count_unrepresentable(cx.s, cx.db, o, p, srcs[i].ability) {
            return cx.unsupported(format!("mana ability producing {k} mana (a pool holds at most {})", u8::MAX));
        }
        let mut used = 0u8;
        for _ in 0..srcs[i].amount {
            used += req.apply(t) as u8;
        }
        // Mana beyond the payment stays in the pool until the step ends, keeping its spending
        // restriction (CR 106.4, 106.4a, 106.6).
        crate::ext6::add_pool(cx.s, p, t, srcs[i].amount - used, srcs[i].left);
        crate::altcast::note_spent(cx.s, subject, t, used);
        let tbit = 1u8 << t as u8;
        if srcs[i].life > 0 && (srcs[i].life_types == 0 || srcs[i].life_types & tbit != 0) {
            crate::resolve::lose_life(cx.s, cx.db, p, srcs[i].life);
        }
        if spell {
            if srcs[i].nocounter_types & tbit != 0 && !cx.s.ext.uncounterable.contains(&subject) {
                cx.s.ext.uncounterable.push(subject); // CR 101.2
            }
            match cx.s.ext.spent_colors.iter_mut().find(|x| x.0 == subject) {
                Some(x) => x.1 |= tbit,
                None => cx.s.ext.spent_colors.push((subject, tbit)),
            }
        }
        if srcs[i].sac {
            crate::trig_ext::fire_sacrificed(cx, &[o], p);
            crate::zones::move_batch(cx, &[crate::zones::Move::new(o, Zone::Graveyard)])?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- mana abilities at priority

/// Why a mana ability is offered at priority under the agent mana filter (a known approximation:
/// agents are offered only the mana abilities whose activation at priority could matter; bits of
/// `mana_reasons`).
pub mod why {
    /// C1 static: the activation costs more than tapping the source and paying life
    /// (sacrifice, tapping another permanent, ...; life alone: `LIFE`). Compiled mana abilities
    /// never have an effect beyond adding mana (`compile`: a sub-ability makes the card
    /// unsupported) nor a mana cost.
    pub const COST: u8 = 1;
    /// C1 dynamic: a permanent would trigger on the activation's events. No compiled trigger
    /// mode watches tapping, mana being added or the activation of a mana ability (sacrifice and
    /// life-payment triggers only follow costs that are already `COST`), so this never fires;
    /// tests cross-check it (activating an unoffered ability only taps the source and adds
    /// mana).
    pub const TRIGGER: u8 = 2;
    /// C2: the stack is non-empty and an object on it could affect the source
    /// (`stack_threatens`).
    pub const STACK: u8 = 4;
    /// C3: the mana wouldn't empty at the end of the step. Nothing in the engine keeps mana
    /// (Forge's `PersistentMana` isn't compiled: such cards are unsupported), so never set.
    pub const PERSIST: u8 = 8;
    /// C1 narrowed for life costs: the cost is {T} and life
    /// (Starting Town, Great Hall of the Biblioplex: "{T}, Pay 1 life: Add one mana of any
    /// color") and either the stack is non-empty or something would trigger on or count the
    /// life loss (`life_loss_watcher`). With an empty stack and nothing watching, paying the
    /// life now differs from paying it while paying a cost later only in when the life is lost.
    pub const LIFE: u8 = 16;
}

/// Does `p` see every activatable mana ability at priority (`XState::full_mana` or
/// `mana_unfiltered`), rather than the agent mana filter's selection?
pub fn mana_unfiltered(s: &State, p: PlayerId) -> bool {
    (s.ext.x6.full_mana | s.ext.x6.mana_unfiltered) & (1 << p.0) != 0
}

/// (types mask, amount) mana ability `a` of `o` would add for `p` now, if it can add any.
pub(crate) fn mana_output(s: &State, db: &CardDb, o: ObjId, p: PlayerId, a: AbilityId) -> Option<(u8, u8)> {
    let (mask, amt) = match &db.ability(a).effect {
        Effect::Mana { produced, amount } => {
            let mut mask = 0u8;
            match produced {
                Produced::One(t) => mask |= 1 << *t as u8,
                Produced::AnyOf(v) => v.iter().for_each(|t| mask |= 1 << *t as u8),
            }
            (mask, *amount)
        }
        Effect::Ext(crate::effect_ext::ExtEffect::Mr(m)) => crate::ext4::mana_count(s, db, o, p, m)?,
        Effect::Ext(x) => (crate::effect_ext::dynamic_mana(s, db, o, x)?, 1),
        _ => return None,
    };
    (mask != 0 && amt > 0).then_some((mask, amt))
}

/// Can `p` activate mana ability `a` of `o` right now (CR 602.5, 605.3a)?
fn mana_ability_activatable(cx: &Ctx, p: PlayerId, o: ObjId, a: AbilityId) -> bool {
    let s = &*cx.s;
    let ad = cx.db.ability(a);
    let Some(c) = &ad.cost else { return false };
    if crate::static_ext::activation_forbidden(s, cx.db, o, ad) || crate::ext6::gained_limit_reached(s, cx.db, o, a) || c.pay_life > s.player(p).life {
        return false;
    }
    if !ad.activate_if.is_empty() {
        let ecx = crate::expr::EvalCtx { s, db: cx.db, you: p, source: o, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &[], imprinted: &[], base_only: false };
        if !ad.activate_if.iter().all(|c| crate::expr::holds(c, &ecx)) {
            return false;
        }
    }
    if let Some((_, f)) = &c.tap_x {
        if mana_fodder(cx, p, o, f).is_empty() {
            return false;
        }
    }
    mana_output(s, cx.db, o, p, a).is_some()
}

/// Untapped permanents `p` controls that a "tap an untapped [f] you control" cost of `src` may
/// tap (CR 118.3; any matching one).
fn mana_fodder(cx: &Ctx, p: PlayerId, src: ObjId, f: &crate::filter::Filter) -> Vec<ObjId> {
    let s = &*cx.s;
    let fcx = FilterCtx { you: p, source: Some(src), x: 0, rem: &[] };
    s.battlefield.iter().copied().filter(|&x| x != src && s.obj(x).controller == p && !s.obj(x).tapped && obj_matches(s, cx.db, f, x, fcx)).collect()
}

/// Mana abilities `p` may activate now, one per (object signature, ability) (identical
/// sources are interchangeable): (source, ability).
pub fn activatable_mana_abilities(cx: &Ctx, p: PlayerId) -> Vec<(ObjId, AbilityId)> {
    let s = &*cx.s;
    let mut cands: Vec<(ObjId, AbilityId)> = vec![];
    for &o in &s.battlefield {
        let ob = s.obj(o);
        // Every compiled mana ability costs {T} (`compile`).
        if ob.controller != p || ob.tapped {
            continue;
        }
        let ch = chars(s, cx.db, o);
        if ch.types.has(CardType::Creature) && s.is_sick(o) && !ch.kws.has(Kw::Haste) {
            continue; // CR 302.6
        }
        for &a in &ch.abilities {
            if !cx.db.ability(a).is_mana || !mana_ability_activatable(cx, p, o, a) {
                continue;
            }
            cands.push((o, a));
        }
    }
    // One signature per source; from one `obj_sigs` scan when there are many (an `obj_sig` scans
    // the whole state; with a few sources that is cheaper than collecting the references).
    let mut objs: Vec<ObjId> = cands.iter().map(|c| c.0).collect();
    objs.dedup(); // consecutive: one object's abilities are adjacent
    let sigs: Vec<crate::query::ObjSig> = if objs.len() >= 16 { crate::query::obj_sigs(s, cx.db, &objs) } else { objs.iter().map(|&o| obj_sig(s, cx.db, o)).collect() };
    let mut out = vec![];
    let mut seen: SmallVec<[(crate::query::ObjSig, AbilityId); 8]> = SmallVec::new();
    let mut j = 0;
    for (o, a) in cands {
        if objs[j] != o {
            j += 1;
        }
        let k = (sigs[j], a);
        if !seen.contains(&k) {
            seen.push(k);
            out.push((o, a));
        }
    }
    out
}

/// The agent mana filter's reasons (`why` bits) to offer mana ability `a` of `o` at priority; 0 =
/// pruned (activating it now has the same outcome as activating it while paying later).
pub fn mana_reasons(cx: &Ctx, p: PlayerId, o: ObjId, a: AbilityId) -> u8 {
    let c = cx.db.ability(a).cost.clone().unwrap_or_default();
    let mut r = 0;
    if c.sac_self || c.tap_x.is_some() || c.has_choices() || !c.mana.syms.is_empty() || c.counter.is_some() || c.discard_self {
        r |= why::COST;
    } else if c.pay_life > 0 && (!cx.s.stack.is_empty() || life_loss_watcher(cx, p).is_some()) {
        r |= why::LIFE;
    }
    if !cx.s.stack.is_empty() && stack_threatens(cx, o) {
        r |= why::STACK;
    }
    r
}

/// Agent mana filter condition C1 for life costs (`why::LIFE`): what would trigger on or count
/// `p` losing life now, if anything. CR 119.4: "If a player pays life, the payment is
/// subtracted from their life total; in other words, the player loses that much life", so
/// "whenever [a player] loses life" triggers (603.2), "if you lost life this turn" conditions,
/// life-total thresholds and amounts all see a life payment. Conservative, and only from what
/// `p` may see (the option set mustn't depend on hidden information: no library, no card in a
/// hand or face down that `p` doesn't know):
/// - the inherent speed trigger (CR 702.179d, "Whenever one or more opponents lose life during
///   your turn, if your speed is less than 4, your speed increases by 1. This ability triggers
///   only once each turn."): an opponent's turn, their speed 1–3, not yet increased this turn;
/// - an object whose card's Oracle text may watch the life of `p` (`CardDb::life_watch`,
///   `oracle_watches_life`: "you" = its controller, "an opponent" = the controller's opponents,
///   CR 102.2–102.3): permanents (controller; face-down ones only if `p` knows them), cards in
///   hands that `p` knows and in graveyards (owner), cards in exile `p` knows (either player:
///   some are cast by a player other than their owner), the sources of effect objects (emblems,
///   "this turn" effects) and of delayed triggers (their controller);
/// - a continuous effect granting a triggered or activated ability whose text may (or is
///   empty: unclassified), for the affected object's controller.
/// The stack isn't looked at: a non-empty stack is a reason by itself (`mana_reasons`).
/// Returns a short reason (diagnostics, tests).
pub fn life_loss_watcher(cx: &Ctx, p: PlayerId) -> Option<String> {
    let s = &*cx.s;
    let db = cx.db;
    let a = s.turn.active;
    if a != p && (1..4).contains(&s.ext.speed[a.idx()]) && s.ext.speed_turn[a.idx()] != s.turn.number {
        return Some("speed".into());
    }
    let known = |o: ObjId| s.obj(o).known_to & (1 << p.0) != 0;
    // Does a text watching `m` (`LIFE_YOU` / `LIFE_OPP`) for `ctrl` (None: either) see `p`'s life?
    let sees = |m: u8, ctrl: Option<PlayerId>| match ctrl {
        Some(c) => (m & LIFE_YOU != 0 && c == p) || (m & LIFE_OPP != 0 && c != p),
        None => m != 0,
    };
    let watch = |c: CardId, ctrl: Option<PlayerId>| sees(db.life_watch(c), ctrl).then(|| db.get(c).name.clone());
    for &o in &s.battlefield {
        let ob = s.obj(o);
        // Face down (CR 708.2): `card` is the 2/2 without text; the real card if `p` knows it.
        let real = if ob.face_down() { s.ext.x6.face_down.iter().find(|x| x.0 == o).map(|x| x.1).filter(|_| known(o)) } else { Some(ob.card) };
        if let Some(r) = real.and_then(|c| watch(c, Some(ob.controller))) {
            return Some(r);
        }
    }
    let owned = s.players.iter().flat_map(|pl| pl.graveyard.iter().chain(pl.hand.iter().filter(|&&o| known(o))));
    for &o in owned {
        if let Some(r) = watch(s.obj(o).card, Some(s.obj(o).owner)) {
            return Some(r);
        }
    }
    for &o in s.exile.iter().filter(|&&o| known(o)) {
        if let Some(r) = watch(s.obj(o).card, None) {
            return Some(r);
        }
    }
    let effects = s.effect_objs.iter().map(|e| (s.obj(e.source).card, e.controller)).chain(s.ext.delayed.iter().map(|d| (d.source_card, d.controller)));
    for (c, ctrl) in effects {
        if let Some(r) = watch(c, Some(ctrl)) {
            return Some(format!("effect of {r}"));
        }
    }
    for e in &s.effects {
        let ctrl = Some(s.obj(e.affected).controller);
        let mut texts = e.triggers.iter().map(|&i| db.triggers_pool[i as usize].text.as_str()).chain(e.abilities.iter().map(|&x| db.ability(x).text.as_str()));
        if let Some(t) = texts.find(|t| t.trim().is_empty() || sees(oracle_watches_life(t), ctrl)) {
            return Some(format!("granted ability {t:?}"));
        }
    }
    None
}

/// Could Oracle text `text` trigger on, replace or count life loss, or read a life total
/// (`life_loss_watcher`)? Conservative: every word containing "life" counts, except the forms
/// that can't see a life payment:
/// - "lifelink" (CR 702.15b: damage also causes life gain);
/// - gaining life: "gain(s)/gained [N | X | that much | twice that much | N or more] life",
///   "life you gained" (CR 119.9–119.10 concern gains; a payment is a loss, 119.4);
/// - losing an amount as an effect: "lose(s) N / X / that much / twice that much life",
///   "lose(s) life equal to ..." (the amount doesn't depend on life lost or the life total);
/// - life as a cost: "pay(s) N / X life", "pay life equal to ..." (losing life earlier can only
///   make such a payment impossible, CR 119.4, never enable anything).
/// "Whenever ... loses life", "if ... lost life this turn", "would lose life", "N or less
/// life", "life total", "half their life", "most life" and spectacle (CR 702.137a: "if an
/// opponent lost life this turn") count.
///
/// Whose life (bits `LIFE_YOU` / `LIFE_OPP`, from the controller's point of view, CR 102.2-102.3):
/// a clause naming only "you"/"your" watches its controller's life (`LIFE_YOU`), one naming only
/// "opponent(s)" its opponents' (`LIFE_OPP`); anything else (a player, both, no subject: "they
/// lose half their life") both. Start your engines!'s reminder ("It increases once on each of your
/// turns when an opponent loses life", CR 702.179d) is left out: the speed trigger is checked from
/// the players' speed (`life_loss_watcher`). 0 = can't see a life payment.
pub fn oracle_watches_life(text: &str) -> u8 {
    // Reminder texts that state a rule, not an ability: start your engines! (above) and "Damage
    // causes loss of life." (CR 120.3a; printed next to "loses life" triggers, which count).
    let low = text.to_ascii_lowercase().replace("\\n", "\n").replace("it increases once on each of your turns when an opponent loses life", "").replace("damage causes loss of life", "");
    let mut m = if low.contains("spectacle") { LIFE_OPP } else { 0 };
    // "1,000 life": drop thousands separators before splitting clauses at commas.
    let b = low.as_bytes();
    let low: String = low.char_indices().filter(|&(i, ch)| !(ch == ',' && i > 0 && b[i - 1].is_ascii_digit() && b.get(i + 1).is_some_and(u8::is_ascii_digit))).map(|x| x.1).collect();
    for clause in low.split(|ch: char| ".,;:\n()•—\"/".contains(ch)) {
        let words: SmallVec<[&str; 24]> = clause.split_whitespace().collect();
        let watches = words.iter().enumerate().any(|(i, w)| w.contains("life") && *w != "lifelink" && (*w != "life" || !harmless_life(&words[..i], &words[i + 1..])));
        if watches {
            let has = |set: &[&str]| words.iter().any(|w| set.contains(w));
            let you = has(&["you", "your", "you've", "you're", "yourself"]);
            let opp = has(&["opponent", "opponents", "opponent's", "opponents'"]);
            let player = words.iter().any(|w| w.starts_with("player"));
            m |= match (you, opp, player) {
                (true, false, false) => LIFE_YOU,
                (false, true, false) => LIFE_OPP,
                _ => LIFE_YOU | LIFE_OPP,
            };
        }
    }
    m
}

/// `oracle_watches_life` bit: the text watches its controller's life ("you").
pub const LIFE_YOU: u8 = 1;
/// `oracle_watches_life` bit: the text watches its controller's opponents' life.
pub const LIFE_OPP: u8 = 2;

/// The forms of "life" `oracle_watches_life` accepts: `pre` / `post` are the clause's words
/// before / after it.
fn harmless_life(pre: &[&str], post: &[&str]) -> bool {
    const N: &str = "#";
    let word_is = |w: &str, pat: &str| if pat == N { w == "x" || (!w.is_empty() && w.bytes().all(|c| c.is_ascii_digit())) } else { w == pat };
    // `pre` ends with one of `verbs` followed by one of `amounts`.
    let ends = |verbs: &[&str], amounts: &[&[&str]]| {
        amounts.iter().any(|am| pre.len() > am.len() && pre[pre.len() - am.len()..].iter().zip(am.iter()).all(|(w, pat)| word_is(*w, *pat)) && verbs.contains(&pre[pre.len() - am.len() - 1]))
    };
    let equal_to = post.len() >= 2 && post[0] == "equal" && post[1] == "to";
    let gain: &[&str] = &["gain", "gains", "gained"];
    let lose: &[&str] = &["lose", "loses"];
    let pay: &[&str] = &["pay", "pays"];
    ends(gain, &[&[], &[N], &["that", "much"], &["twice", "that", "much"], &[N, "or", "more"]])
        || post.first() == Some(&"gained")
        || (post.len() >= 2 && post[0] == "you" && post[1] == "gained")
        || ends(lose, &[&[N], &["that", "much"], &["twice", "that", "much"]])
        || (equal_to && ends(lose, &[&[]]))
        || ends(pay, &[&[N]])
        || (equal_to && ends(pay, &[&[]]))
}

/// The `Opt::Mana` priority options of `p` (CR 605.3a): every activatable mana ability, or
/// (`filtered`) those the agent mana filter keeps. The option's mana type is the lowest the ability
/// can make; an ability that can make several asks which as it resolves.
pub fn priority_mana_options(cx: &Ctx, p: PlayerId, filtered: bool) -> Vec<Opt> {
    let s = &*cx.s;
    if filtered && s.stack.is_empty() {
        // Only costs beyond {T} can matter with an empty stack (C1 static; C1 dynamic and C3
        // never fire, see `why`): skip the enumeration when no permanent of `p` has one.
        let any = s.battlefield.iter().any(|&o| {
            s.obj(o).controller == p && !s.obj(o).tapped && chars(s, cx.db, o).abilities.iter().any(|&a| {
                let ad = cx.db.ability(a);
                ad.is_mana && ad.cost.as_ref().is_some_and(|c| c.sac_self || c.pay_life > 0 || c.tap_x.is_some())
            })
        });
        if !any {
            return vec![];
        }
    }
    activatable_mana_abilities(cx, p)
        .into_iter()
        .filter(|&(o, a)| !filtered || mana_reasons(cx, p, o, a) != 0)
        .filter_map(|(o, a)| {
            let (mask, _) = mana_output(s, cx.db, o, p, a)?;
            let t = ManaType::all().into_iter().find(|t| mask & (1 << *t as u8) != 0)?;
            Some(Opt::Mana(o, a, t))
        })
        .collect()
}

/// Activate mana ability `a` of `o` at priority (CR 605.3a, 605.3b: it doesn't use the stack and
/// resolves at once): pay its costs (CR 602.2b, 601.2h: {T}, tapping another permanent, life,
/// sacrifice), then add its mana to the pool (CR 106.4), keeping its spending restriction
/// (CR 106.6). "One mana of any color": the color is chosen as it resolves (`ChooseMode` with
/// `Opt::PoolMana` options). The player keeps priority (CR 117.3c).
pub fn activate_mana_ability(cx: &mut Ctx, p: PlayerId, o: ObjId, a: AbilityId) -> R<()> {
    activate_mana_ability_as(cx, p, o, a, None)
}

/// `activate_mana_ability`; `forced`: the mana type it makes was already chosen (web-slinging's
/// "tap this creature for mana, then return it", `altcast::web_candidates`).
pub fn activate_mana_ability_as(cx: &mut Ctx, p: PlayerId, o: ObjId, a: AbilityId, forced: Option<ManaType>) -> R<()> {
    if !mana_ability_activatable(cx, p, o, a) || cx.s.obj(o).tapped {
        return cx.unsupported("mana ability offered at priority can't be activated");
    }
    let c = cx.db.ability(a).cost.clone().unwrap_or_default();
    let Some((mask, amount)) = mana_output(cx.s, cx.db, o, p, a) else { return cx.unsupported("mana ability makes no mana") };
    if let Some(k) = crate::ext4::mana_count_unrepresentable(cx.s, cx.db, o, p, a) {
        return cx.unsupported(format!("mana ability producing {k} mana (a pool holds at most {})", u8::MAX));
    }
    cx.s.obj_mut(o).tapped = true;
    if let Some((_, f)) = &c.tap_x {
        let items = crate::query::sig_items(cx.s, cx.db, &mana_fodder(cx, p, o, f));
        for ch in cx.choose_set(p, DecisionKind::PayCost, items, 1, 1, Some(o))? {
            if let Opt::Card(x) = ch {
                cx.s.obj_mut(x).tapped = true;
            }
        }
    }
    if c.pay_life > 0 {
        crate::resolve::lose_life(cx.s, cx.db, p, c.pay_life);
    }
    crate::ext6::note_gained_use(cx.s, cx.db, o, a);
    if c.sac_self {
        crate::trig_ext::fire_sacrificed(cx, &[o], p);
        crate::zones::move_batch(cx, &[crate::zones::Move::new(o, Zone::Graveyard)])?;
    }
    let types: SmallVec<[ManaType; 6]> = ManaType::all().into_iter().filter(|t| mask & (1 << *t as u8) != 0).collect();
    let t = if let Some(t) = forced.filter(|t| types.contains(t)) {
        t
    } else if forced.is_some() {
        return cx.unsupported("mana ability can't make the chosen type");
    } else if types.len() > 1 {
        let options: Vec<Opt> = types.iter().map(|&t| Opt::PoolMana(t)).collect();
        let Opt::PoolMana(t) = cx.choose(Decision { player: p, kind: DecisionKind::ChooseMode, options, subject: Some(o) })? else { unreachable!() };
        t
    } else {
        types[0]
    };
    crate::ext6::add_pool(cx.s, p, t, amount, crate::ext6::ability_restr(cx.db, a, o));
    Ok(())
}

/// Agent mana filter condition C2: could an object on the stack affect `src` before its mana would be spent
/// (conservative)? It targets `src`; a mass effect's filter may include it (with only the
/// properties that can't change meanwhile kept, `may_match`); a single-object effect's defined
/// object may be it (unknown references count); an edict may make its controller sacrifice
/// it; a permanent spell or an effect brings a static that may weaken it, remove its abilities
/// or stop mana abilities; anything unclassified (`Effect::Ext`, other statics) counts.
/// Spell-only effects (counter, copy), card draw, life, tokens, ... don't.
pub fn stack_threatens(cx: &Ctx, src: ObjId) -> bool {
    stack_threat(cx, src).is_some()
}

/// `stack_threatens` with the reason (diagnostics, tests): "target", "static
/// <kind>" (a permanent spell's static), or the effect of the node that could affect `src`.
pub fn stack_threat(cx: &Ctx, src: ObjId) -> Option<String> {
    let s = &*cx.s;
    let db = cx.db;
    let ctrl = s.obj(src).controller;
    let name = |d: &dyn std::fmt::Debug| format!("{d:?}").split([' ', '(', '{']).next().unwrap_or("").to_string();
    for it in &s.stack {
        // Targeting one of its abilities counts too (as when abilities were targeted by their
        // source, before `ext8`): "that permanent loses all abilities" (Tishana's Tidebinder).
        if it.targets.iter().any(|t| t.1 == Target::Obj(src) || matches!(t.1, Target::Ability { src: x, .. } if x == src)) {
            return Some("target".into());
        }
        let fcx = FilterCtx { you: it.controller, source: Some(it.source), x: it.x, rem: &[] };
        if it.kind == StackKind::Spell && it.ability == permanent_spell_ability(db) {
            let ob = s.obj(it.source);
            if let Some(st) = db.get(ob.card).face(ob.face).statics.iter().find(|st| static_threat(cx, st, src, fcx, it)) {
                return Some(format!("permanent spell static {}", name(&st.kind)));
            }
            continue;
        }
        for n in crate::resolve::nodes(db, it.ability, &it.modes) {
            if node_threat(cx, n, it, src, ctrl, fcx) {
                let e = &db.ability(n).effect;
                let inner = match e {
                    Effect::Ext(x) => format!("Ext {}", format!("{x:?}").chars().take(40).collect::<String>()),
                    _ => name(e),
                };
                return Some(format!("{inner} (defined {:?})", db.ability(n).defined));
            }
        }
    }
    None
}

/// `obj_matches` with only the filter properties that nothing on the stack could change
/// without being a threat itself (controller, owner, token, card types, ...); the others
/// (tapped, power, counters, colors, keywords, remembered, ...) are assumed to match.
fn may_match(cx: &Ctx, f: &crate::filter::Filter, o: ObjId, fcx: FilterCtx) -> bool {
    use crate::filter::Prop;
    // `fcx.rem` non-empty: the objects an effect will remember are known (`CreateEffect`).
    let known_rem = !fcx.rem.is_empty();
    let keep = |p: &Prop| {
        (known_rem && matches!(p, Prop::IsRemembered))
            || matches!(
            p,
            Prop::YouCtrl | Prop::OppCtrl | Prop::YouOwn | Prop::YouDontCtrl | Prop::YouDontOwn | Prop::Other | Prop::SelfCard | Prop::Token | Prop::NonToken | Prop::NonType(_) | Prop::NonSubtype(_) | Prop::IsType(_) | Prop::IsPermanent | Prop::Legendary | Prop::NonLegendary | Prop::Basic | Prop::NonBasic | Prop::InZone(_)
        )
    };
    let g = crate::filter::Filter { alts: f.alts.iter().map(|al| crate::filter::Alt { base: al.base.clone(), props: al.props.iter().filter(|p| keep(p)).cloned().collect() }).collect() };
    obj_matches(cx.s, cx.db, &g, o, fcx)
}

fn nonneg(e: &Option<crate::expr::Expr>) -> bool {
    matches!(e, None | Some(crate::expr::Expr::Num(0..)))
}

/// Could the object(s) a non-targeted single-object effect's `Defined$` names include `src`?
/// (As `resolve::affected` resolves them; objects remembered or chosen during the resolution
/// are unknown here: they count.)
fn defined_may_include(cx: &Ctx, d: &Option<Defined>, it: &StackItem, src: ObjId) -> bool {
    match d {
        Some(Defined::SelfObj) => it.source == src,
        Some(Defined::Targeted) => false, // its targets: checked for the whole stack object
        Some(Defined::Triggered) => it.trig.obj == Some(src) || it.trig.new_obj == Some(src),
        Some(Defined::Attached) => cx.s.obj(it.source).attached_to == Some(src),
        Some(Defined::You | Defined::Opponent | Defined::TargetedController | Defined::TargetedPlayer | Defined::TriggeredController | Defined::TriggeredPlayer | Defined::RememberedController) => false,
        // No `Defined$`: the ability's source (`resolve::affected`).
        None => it.source == src,
        Some(Defined::Remembered | Defined::Replaced | Defined::ChosenCard | Defined::Imprinted) => true,
    }
}

/// Could the players a `Defined$` names (an edict's sacrificing players) include `p`?
fn defined_may_be_player(d: &Option<Defined>, it: &StackItem, p: PlayerId) -> bool {
    match d {
        Some(Defined::You) => it.controller == p,
        Some(Defined::Opponent) => it.controller != p,
        Some(Defined::TargetedPlayer | Defined::Targeted) => it.targets.iter().any(|t| t.1 == Target::Player(p)),
        _ => true,
    }
}

fn node_threat(cx: &Ctx, n: AbilityId, it: &StackItem, src: ObjId, ctrl: PlayerId, fcx: FilterCtx) -> bool {
    let ad = cx.db.ability(n);
    let single = || ad.target.is_none() && defined_may_include(cx, &ad.defined, it, src);
    let mm = |f: &crate::filter::Filter| may_match(cx, f, src, fcx);
    match &ad.effect {
        Effect::Draw { .. } | Effect::GainLife { .. } | Effect::LoseLife { .. } | Effect::Scry { .. } | Effect::Surveil { .. } | Effect::Mill { .. } | Effect::Counter | Effect::Mana { .. } | Effect::Discard { .. } | Effect::Token { .. } | Effect::Charm { .. } | Effect::Cleanup { .. } | Effect::Dig { .. } | Effect::NinjutsuEnter | Effect::Clone { .. } | Effect::ReturnTransformed | Effect::SetPrepared(_) | Effect::AuraEnter | Effect::CopyPermanent => false,
        Effect::DealDamage { .. } | Effect::Destroy | Effect::Pump { .. } | Effect::Tap | Effect::Untap | Effect::PutCounter { .. } | Effect::Animate { .. } | Effect::Attach | Effect::Explore => single(),
        Effect::ChangeZone { origin, hidden, .. } => !*hidden && origin.has(Zone::Battlefield) && single(),
        Effect::DestroyAll { valid } => mm(valid),
        Effect::PumpAll { valid, power, toughness, .. } => !(nonneg(&Some(power.clone())) && nonneg(&Some(toughness.clone()))) && mm(valid),
        Effect::DamageAll { cards, .. } => cards.as_ref().is_some_and(mm),
        Effect::PutCounterAll { valid, kind, .. } => *kind != CounterKind::P1P1 && mm(valid),
        Effect::ChangeZoneAll { origin, change_type, .. } => origin.has(Zone::Battlefield) && mm(change_type),
        Effect::Sacrifice { valid, .. } => defined_may_be_player(&ad.defined, it, ctrl) && mm(valid),
        Effect::CreateEffect { statics, remember, .. } => {
            // The objects the effect will remember, when known now (its statics' "IsRemembered").
            let rem: SmallVec<[ObjId; 2]> = match remember {
                Some(Defined::Targeted) => it.targets.iter().filter_map(|t| if let Target::Obj(x) = t.1 { Some(x) } else { None }).collect(),
                Some(Defined::SelfObj) => smallvec::smallvec![it.source],
                Some(Defined::Triggered) => it.trig.obj.into_iter().chain(it.trig.new_obj).collect(),
                _ => SmallVec::new(),
            };
            let fcx = FilterCtx { rem: &rem, ..fcx };
            // effect-object static reader: none (a threat estimate of a spell's effect, not a rule) (`static_ext::applies_from_effect_object`)
            statics.iter().any(|&i| static_threat(cx, &cx.db.statics_pool[i as usize], src, fcx, it))
        }
        Effect::Alt(_) => it.source == src,
        Effect::Ext(x) => {
            use crate::effect_ext::ExtEffect as E;
            match x {
                // Bookkeeping / other zones / players: a reflexive trigger is its own stack
                // object when it triggers; links to objects already moved; naming a card;
                // tokens; the next spell can't be countered.
                E::Reflexive { .. } | E::LinkMoved { .. } | E::NameCard { .. } | E::TokenFor { .. } | E::Lt(crate::ext3::LtEffect::NextSpellUncounterable) => false,
                // One object, as `Defined$` / its target says.
                E::DealDamageFrom { .. } | E::AllCreatureTypes { .. } | E::Harness | E::SetSolved | E::Earthbend { .. } => single(),
                E::MultiplyCounter { kind, .. } => *kind != CounterKind::P1P1 && single(),
                _ => true,
            }
        }
    }
}

/// Could static ability `st` (of a permanent spell about to resolve, or of an effect about to
/// be created) affect `src` in a way that matters for its mana?
fn static_threat(cx: &Ctx, st: &StaticDef, src: ObjId, fcx: FilterCtx, it: &StackItem) -> bool {
    use crate::static_ext::StaticExt as X;
    let mm = |f: &crate::filter::Filter| may_match(cx, f, src, fcx);
    match &st.kind {
        StaticKind::Continuous { affected, power, toughness, set_power, set_toughness, .. } => (set_power.is_some() || set_toughness.is_some() || !nonneg(power) || !nonneg(toughness)) && mm(affected),
        StaticKind::ReduceCost { .. } | StaticKind::CantBlockBy { .. } | StaticKind::CantPreventDamage { .. } | StaticKind::MayPlay { .. } | StaticKind::Alt(_) => false,
        StaticKind::RemoveAllAbilities { affected } => mm(affected),
        StaticKind::DefinedContinuous(_) => it.source == src,
        StaticKind::Ext(x) => match x {
            X::CantBeActivated { valid, nonmana_only } => !*nonmana_only && mm(valid),
            X::GrantAbility { .. } | X::CantBlock { .. } | X::DisableEtbTriggers { .. } | X::CantGainLife { .. } | X::ExtraLandPlays(_) | X::CantBeCountered { .. } | X::DamagePlus { .. } | X::AbilityCostReduce { .. } | X::FreeEquip { .. } | X::HealBeforeDamage | X::SacrificeReduces | X::DrawPlus { .. } | X::SelfChosenLandType | X::PlayerHexproof => false,
            _ => true,
        },
    }
}
