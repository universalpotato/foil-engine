//! Triggered abilities (CR 603): matching game events against trigger definitions.
//!
//! `fire` is called synchronously when an event happens. Leaves-the-battlefield events are
//! fired *before* the objects move (look back in time, CR 603.10a) by `zones::move_batch`, so
//! the leaving objects and everything else on the battlefield are still in place.

use crate::basics::*;
use crate::carddb::*;
use crate::expr::{holds, EvalCtx};
use crate::query::{obj_matches, FilterCtx};
use crate::state::*;
use smallvec::SmallVec;

#[derive(Clone, Copy, Debug)]
pub enum TrigEvent {
    /// `old` moved from `from` to `to`, becoming `new` (None if it ceased to exist).
    ZoneChange { old: ObjId, new: Option<ObjId>, from: Zone, to: Zone },
    StepBegin { step: Step },
    Attacks { attacker: ObjId },
    SpellCast { spell: ObjId, caster: PlayerId },
    Damage { source: ObjId, target: Target, amount: i32, combat: bool },
    /// `source` caused `player` to gain `amount` life (CR 119.9).
    LifeGained { player: PlayerId, amount: i32, source: ObjId },
    /// `target` became the target of the spell/ability whose source is `by_source`, controlled
    /// by `controller`: stack object `item` (`StackItem::uid`).
    BecomesTarget { target: ObjId, by_source: ObjId, item: u32, controller: PlayerId },
    /// Events added later (see `trig_ext`).
    Ext(crate::trig_ext::ExtEvent),
    /// Lore counters on `obj` went from `before` to `after` (CR 714.2b).
    LoreAdded { obj: ObjId, before: i32, after: i32 },
}

/// Objects whose triggers might fire: the battlefield, plus cards elsewhere that have triggers
/// working from their current zone.
fn candidates(s: &State, db: &CardDb) -> SmallVec<[ObjId; 32]> {
    let mut v: SmallVec<[ObjId; 32]> = s.battlefield.iter().copied().collect();
    let off = |o: ObjId| db.get(s.obj(o).card).front().triggers.iter().any(|t| t.zones.0 & !ZoneSet::of(Zone::Battlefield).0 != 0);
    for p in &s.players {
        v.extend(p.graveyard.iter().copied().filter(|&o| off(o)));
        v.extend(p.hand.iter().copied().filter(|&o| off(o)));
    }
    v.extend(s.exile.iter().copied().filter(|&o| off(o)));
    // Spells with "when you cast this spell" triggers (they function on the stack, CR 113.6).
    for it in &s.stack {
        if it.kind == StackKind::Spell && db.get(s.obj(it.source).card).front().triggers.iter().any(|t| t.zones.has(Zone::Stack)) {
            v.push(it.source);
        }
    }
    v
}

pub fn fire(s: &mut State, db: &CardDb, ev: &TrigEvent) {
    // "Creatures entering don't cause abilities to trigger" (Torpor Orb, Doorkeeper Thrull):
    // no ability triggers on it, whatever path it comes from (printed, gained, delayed,
    // effect objects); "one or more" batches drop such entries (`ext5::batch_matches`).
    if event_suppressed(s, db, ev) {
        return;
    }
    if !s.ext.delayed.is_empty() || !s.effect_objs.is_empty() || !s.ext.x6.granted.is_empty() || !s.ext.x7.paradigm.is_empty() {
        crate::trig_ext::fire_extra(s, db, ev);
    }
    // Matching is read-only: on a big board it runs with characteristics memoized
    // (`query::frozen_if_big`; same values), since every trigger source's filter asks for the
    // event objects' characteristics, which can count the battlefield
    // (Construct tokens entering next to dozens of Phoenix Fleet Airship copies).
    let ro: &State = s;
    let (found, gained) = crate::query::frozen_if_big(ro, db, || matching(ro, db, ev));
    s.pending_triggers.extend(gained);
    for t in found {
        if db.get(t.card).front().triggers[t.trigger_idx as usize].per_turn.is_some() {
            match s.turn_stats.trigger_counts.iter_mut().find(|c| c.0 == t.source && c.1 == t.trigger_idx) {
                Some(c) => c.2 += 1,
                None => s.turn_stats.trigger_counts.push((t.source, t.trigger_idx, 1)),
            }
        }
        s.pending_triggers.push(t);
    }
}

/// The triggers `ev` fires (`fire`): (abilities of trigger sources, abilities gained from
/// continuous effects).
#[allow(clippy::type_complexity)]
fn matching(s: &State, db: &CardDb, ev: &TrigEvent) -> (SmallVec<[PendingTrigger; 4]>, SmallVec<[PendingTrigger; 2]>) {
    let mut found: SmallVec<[PendingTrigger; 4]> = SmallVec::new();
    for src in candidates(s, db) {
        let o = s.obj(src);
        let (card, controller, zone) = (o.card, o.controller, o.zone);
        if zone == Zone::Battlefield && crate::query::may_lose_abilities(s, db) && crate::query::lost_all_abilities(s, db, src) {
            continue;
        }
        for (i, td) in db.get(card).front().triggers.iter().enumerate() {
            if !td.zones.has(zone) {
                continue;
            }
            let Some(trig) = matches(s, db, &td.mode, ev, src, controller) else { continue };
            if trigger_suppressed(s, db, ev, td, WardGrant::Of { src, ts: s.obj(src).ts }) {
                continue;
            }
            let ecx = EvalCtx { s, db, you: controller, source: src, x: 0, kicked: false, triggered: trig.obj, trigger_amount: trig.amount, targets: &[], rem: &[], imprinted: &[], base_only: false };
            if !td.intervening.iter().all(|c| holds(c, &ecx)) {
                continue;
            }
            if let Some(limit) = td.per_turn {
                let n = s.turn_stats.trigger_counts.iter().find(|c| c.0 == src && c.1 == i as u8).map(|c| c.2).unwrap_or(0);
                if n >= limit {
                    continue;
                }
            }
            found.push(PendingTrigger { source: src, card, controller, trigger_idx: i as u8, trig, adhoc: None });
        }
    }
    // Triggered abilities gained from continuous effects (layer 6, e.g. Great Hall of the
    // Biblioplex); per-turn limits are rejected when compiling them.
    let mut gained: SmallVec<[PendingTrigger; 2]> = SmallVec::new();
    if s.effects.iter().any(|e| !e.triggers.is_empty()) {
        for e in s.effects.iter().filter(|e| !e.triggers.is_empty()) {
            let src = e.affected;
            let o = s.obj(src);
            // Gained with the effect's timestamp: a later loss of all abilities removes them
            // (CR 613.1f, 613.7b); an earlier one doesn't.
            if o.zone != Zone::Battlefield || crate::query::grant_removed(s, db, src, e.ts) {
                continue;
            }
            for &ti in &e.triggers {
                let td = &db.triggers_pool[ti as usize];
                if trigger_suppressed(s, db, ev, td, WardGrant::Of { src, ts: e.ts }) {
                    continue;
                }
                let Some(trig) = matches(s, db, &td.mode, ev, src, o.controller) else { continue };
                let ecx = EvalCtx { s, db, you: o.controller, source: src, x: 0, kicked: false, triggered: trig.obj, trigger_amount: trig.amount, targets: &[], rem: &[], imprinted: &[], base_only: false };
                if td.intervening.iter().all(|c| holds(c, &ecx)) {
                    gained.push(PendingTrigger { source: src, card: o.card, controller: o.controller, trigger_idx: 0, trig, adhoc: Some(td.execute) });
                }
            }
        }
    }
    (found, gained)
}

/// Does no ability at all trigger on `ev`? "Creatures entering don't cause abilities to trigger"
/// (Torpor Orb, Doorkeeper Thrull: CR 603.6a; their rulings: "the trigger event doesn't have to
/// specify artifacts or creatures"). Checked once per event, before every trigger path
/// (`fire` reaches them all; "one or more ... enter" batches drop such entries instead,
/// `ext5::batch_matches`).
pub(crate) fn event_suppressed(s: &State, db: &CardDb, ev: &TrigEvent) -> bool {
    matches!(ev, TrigEvent::ZoneChange { new: Some(n), to: Zone::Battlefield, .. } if crate::static_ext::etb_triggers_disabled(s, db, *n))
}

/// Who grants a ward ability being checked (`trigger_suppressed`).
#[derive(Clone, Copy)]
pub(crate) enum WardGrant {
    /// An ability of the object `src` (printed, or gained or granted to it, with timestamp `ts`):
    /// when it triggers for another permanent becoming the target, it is the ward `src` grants to
    /// that permanent (an Equipment / Aura / static's grant compiled as a trigger of `src`).
    Of { src: ObjId, ts: u32 },
    /// An effect object's ability (`State::effect_objs`, timestamp `ts`): the ward its effect
    /// grants to the targeted permanent.
    Effect { ts: u32 },
}

/// Is the triggered ability `td`, matched to `ev`, kept from triggering? The one per-ability
/// check of every trigger path (printed, gained from continuous effects, granted
/// (`ext6::granted_triggers`), effect objects (`trig_ext::fire_extra`);
/// a test lints that each path asks it):
/// - "Ward abilities of [those] creatures don't trigger" (Nowhere to Run): a ward ability of the
///   targeted permanent, printed or gained from any source;
/// - a ward granted to the targeted permanent is gone if that permanent lost all abilities after
///   the grant (CR 613.1f, 613.7a/e: the grant has the granting source's or effect's timestamp).
pub(crate) fn trigger_suppressed(s: &State, db: &CardDb, ev: &TrigEvent, td: &TriggerDef, grant: WardGrant) -> bool {
    let TrigEvent::BecomesTarget { target, .. } = ev else { return false };
    if !td.text.starts_with("Ward") {
        return false;
    }
    if crate::ext3::ward_disabled(s, db, *target) {
        return true;
    }
    match grant {
        WardGrant::Of { src, ts } => *target != src && crate::query::grant_removed(s, db, *target, ts),
        WardGrant::Effect { ts } => crate::query::grant_removed(s, db, *target, ts),
    }
}

pub(crate) fn matches(s: &State, db: &CardDb, mode: &TrigMode, ev: &TrigEvent, src: ObjId, you: PlayerId) -> Option<TrigCtx> {
    let fcx = FilterCtx { you, source: Some(src), x: 0, rem: &[] };
    match (mode, ev) {
        (TrigMode::ChangesZone { origin, dest, valid }, TrigEvent::ZoneChange { old, new, from, to }) => {
            if !origin.has(*from) || !dest.has(*to) {
                return None;
            }
            // Match the object as it exists where the rule looks: before leaving the battlefield
            // (look-back), after entering it, otherwise its new incarnation if any.
            let subject = if *from == Zone::Battlefield { *old } else { new.unwrap_or(*old) };
            // Look-back for "this card": a dying card's triggers match the card itself.
            let ok = obj_matches(s, db, valid, subject, fcx) || (*from == Zone::Battlefield && subject == src && obj_matches(s, db, valid, src, fcx));
            ok.then(|| TrigCtx { obj: Some(subject), new_obj: *new, player: None, amount: 0, stack_item: None, target: None, gain_source: None })
        }
        (TrigMode::Phase { step, player }, TrigEvent::StepBegin { step: st }) => (step == st && player.matches(s.turn.active, you)).then(|| TrigCtx { player: Some(s.turn.active), ..Default::default() }),
        (TrigMode::Attacks { valid }, TrigEvent::Attacks { attacker }) => obj_matches(s, db, valid, *attacker, fcx).then(|| TrigCtx { obj: Some(*attacker), ..Default::default() }),
        (TrigMode::SpellCast { valid, caster }, TrigEvent::SpellCast { spell, caster: c }) => {
            (caster.matches(*c, you) && obj_matches(s, db, valid, *spell, fcx)).then(|| TrigCtx { obj: Some(*spell), player: Some(*c), ..Default::default() })
        }
        (TrigMode::DamageDone { source, target_player, target_obj, combat_only }, TrigEvent::Damage { source: dsrc, target, amount, combat }) => {
            if *combat_only && !combat {
                return None;
            }
            if !obj_matches(s, db, source, *dsrc, fcx) {
                return None;
            }
            let tgt_ok = match (target, target_player, target_obj) {
                (Target::Player(p), Some(sel), _) => sel.matches(*p, you),
                (Target::Obj(o), _, Some(f)) => obj_matches(s, db, f, *o, fcx),
                _ => false, // damage is never dealt to an ability
            };
            let player = if let Target::Player(p) = target { Some(*p) } else { None };
            tgt_ok.then(|| TrigCtx { obj: Some(*dsrc), new_obj: None, player, amount: *amount, stack_item: None, target: None, gain_source: None })
        }
        (TrigMode::BecomesTarget { valid, by }, TrigEvent::BecomesTarget { target, by_source, item, controller }) => {
            (by.matches(*controller, you) && obj_matches(s, db, valid, *target, fcx)).then(|| TrigCtx { obj: Some(*by_source), new_obj: None, player: Some(*controller), amount: 0, stack_item: Some(*item), target: Some(*target), gain_source: None })
        }
        // A Saga entering with lore counters (714.3a) triggers chapters up to that count.
        (TrigMode::Chapter { n }, TrigEvent::ZoneChange { new: Some(nw), to: Zone::Battlefield, .. }) if *nw == src => {
            let lore = crate::query::lore(s, db, src);
            (*n as i32 <= lore).then(TrigCtx::default)
        }
        (TrigMode::Chapter { n }, TrigEvent::LoreAdded { obj, before, after }) if *obj == src => (*before < *n as i32 && *after >= *n as i32).then(TrigCtx::default),
        (TrigMode::LifeGained { player }, TrigEvent::LifeGained { player: p, amount, source }) => player.matches(*p, you).then(|| TrigCtx { player: Some(*p), amount: *amount, gain_source: Some(*source), ..Default::default() }),
        (TrigMode::Ext(m), e) => crate::trig_ext::matches(s, db, m, e, src, you),
        _ => None,
    }
}
