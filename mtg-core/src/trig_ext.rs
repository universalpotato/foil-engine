//! Trigger modes and trigger sources added after the core set: attacks (as a batch),
//! sacrifices, library searches, noncombat damage to players, delayed triggered abilities
//! (CR 603.7), reflexive triggers (CR 603.12) and triggered abilities of effect objects.

use crate::basics::*;
use crate::carddb::*;
use crate::engine::Ctx;
use crate::ext::{DelayedTrig, DelayedWhen};
use crate::filter::Filter;
use crate::query::{obj_matches, FilterCtx};
use crate::state::*;
use crate::triggers::TrigEvent;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrigExt {
    /// "Whenever [player] attacks [with one or more matching creatures]" (`AttackersDeclared`).
    AttackersDeclared { player: PlayerSel, valid: Option<Filter> },
    /// "Whenever [player] sacrifices a matching permanent" (CR 701.21).
    Sacrificed { valid: Filter, player: PlayerSel },
    /// "Whenever [player] searches their library" (CR 701.19).
    SearchedLibrary { player: PlayerSel },
    /// "Whenever one or more [players] are dealt [noncombat] damage" (`DamageAll`); with one
    /// opponent this is per damage event to that player.
    PlayerDealtDamage { player: PlayerSel, combat: Option<bool> },
    /// "Whenever one or more [matching] objects move from `origin` to `dest`" (`ChangesZoneAll`):
    /// once per simultaneous batch of moves (one event, CR 603.2c), matched by look-back (603.10a).
    ZoneBatch { origin: ZoneSet, dest: ZoneSet, valid: Filter },
    /// "Whenever [player] commits a crime" (CR 700.13).
    CommitCrime { player: PlayerSel },
    /// "Whenever [a matching source] deals damage" (`DamageDealtOnce`): once per simultaneous
    /// damage event (all combat damage in a step is one, CR 510.2), with the total amount.
    DamageDealtOnce { source: Filter },
    /// Valiant (`ext4`): a matching object becomes the target of a spell or ability you control
    /// for the first time this turn.
    Valiant { valid: Filter },
    /// "Whenever [a matching card] is drawn" (`Drawn`, CR 121.2). `nth` > 0: only the drawing
    /// player's `nth` card drawn this turn ("whenever you draw your second card each turn",
    /// Raven Eagle; `Number$ N`).
    Drawn { valid: Filter, nth: u8 },
    /// "Whenever [player] loses life" (`LifeLost`, CR 119.3; each life-loss event).
    LifeLost { player: PlayerSel },
    /// "Whenever one or more [matching] objects move from `origin` to `dest`" for batches not
    /// (only) leaving the battlefield: matched after the batch moved, by the new objects
    /// ("one or more creatures enter", "put into your graveyard from anywhere"), or by the old
    /// ones when they ceased to exist / for "leave your graveyard" (`ext5`).
    ZoneBatchAfter { origin: ZoneSet, dest: ZoneSet, valid: Filter },
    /// "Whenever [player] activates a loyalty ability" (`AbilityCast | ValidSA$
    /// Activated.Loyalty`, Ajani Unrelenting).
    LoyaltyActivated { player: PlayerSel },
    /// "When you unlock this door" (CR 709.5h): the `half` this trigger is printed on.
    DoorUnlocked { half: u8 },
    /// "Whenever this creature transforms into [this face]" (`Transformed | ValidCard$
    /// Card.Self`, CR 701.27e): read from the face that is up after transforming (`ext6`).
    TransformedSelf,
    /// "When the `n`th `kind` counter is put on this permanent" (`ext6::counter_watch`).
    CounterAddedSelf { kind: CounterKind, n: i32 },
    /// "Whenever you discard a card" (each card; `ext6::flush_discards`).
    DiscardedCard,
    /// "Whenever [player] discards one or more cards" (once per batch, with the number).
    DiscardedAll { player: PlayerSel },
    /// "When this Class becomes level N" (CR 716).
    ClassLevelSelf { level: u8 },
    /// "Whenever you activate an ability that isn't a mana ability" (CR 602.2).
    ActivatedNonMana,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtEvent {
    AttackersDeclared { player: PlayerId },
    /// `obj` (still on the battlefield, look-back) is being sacrificed by `player`.
    Sacrificed { obj: ObjId, player: PlayerId },
    Searched { player: PlayerId },
    /// A batch of objects is about to move (listed in `State::ext.batch`, look-back).
    Batch,
    /// `player` committed a crime (CR 700.13).
    Crime { player: PlayerId },
    /// `source` dealt `amount` damage in one event (to one or more recipients).
    DealtDamage { source: ObjId, amount: i32 },
    /// `player` drew the card `card` (now in their hand).
    Drawn { player: PlayerId, card: ObjId },
    /// `player` lost `amount` life (CR 119.3).
    LifeLost { player: PlayerId, amount: i32 },
    /// A batch of objects has moved (listed in `State::ext.le.batch`).
    AfterBatch,
    /// `player` activated a loyalty ability of `source` (it is on the stack).
    LoyaltyActivated { player: PlayerId, source: ObjId },
    /// `player` unlocked `half` of the Room `obj` (as it entered, or by the special action).
    DoorUnlocked { obj: ObjId, half: u8, player: PlayerId },
    /// `obj` transformed (CR 701.27a; not when it entered transformed).
    Transformed { obj: ObjId },
    /// `obj`'s `kind` counters went from `before` to `after` (counters were put on it).
    CounterAdded { obj: ObjId, kind: CounterKind, before: i32, after: i32 },
    /// `player` discarded a card.
    Discarded { player: PlayerId },
    /// `player` discarded `amount` cards at once.
    DiscardedAll { player: PlayerId, amount: i32 },
    /// `obj`'s Class level became `level`.
    ClassLevel { obj: ObjId, level: u8 },
    /// `player` activated a non-mana ability of `source` (`ext6::ability_activated`; `key`
    /// finds it as activated).
    AbilityActivated { player: PlayerId, source: ObjId, key: u32 },
}

pub fn matches(s: &State, db: &CardDb, m: &TrigExt, ev: &TrigEvent, src: ObjId, you: PlayerId) -> Option<TrigCtx> {
    let fcx = FilterCtx { you, source: Some(src), x: 0, rem: &[] };
    match (m, ev) {
        (TrigExt::AttackersDeclared { player, valid }, TrigEvent::Ext(ExtEvent::AttackersDeclared { player: p })) => {
            let ok = player.matches(*p, you) && valid.as_ref().is_none_or(|f| s.combat.attackers.iter().any(|a| obj_matches(s, db, f, a.0, fcx)));
            ok.then(|| TrigCtx { player: Some(*p), amount: s.combat.attackers.len() as i32, ..Default::default() })
        }
        (TrigExt::Sacrificed { valid, player }, TrigEvent::Ext(ExtEvent::Sacrificed { obj, player: p })) => {
            (player.matches(*p, you) && obj_matches(s, db, valid, *obj, fcx)).then(|| TrigCtx { obj: Some(*obj), player: Some(*p), ..Default::default() })
        }
        (TrigExt::SearchedLibrary { player }, TrigEvent::Ext(ExtEvent::Searched { player: p })) => player.matches(*p, you).then(|| TrigCtx { player: Some(*p), ..Default::default() }),
        (TrigExt::CommitCrime { player }, TrigEvent::Ext(ExtEvent::Crime { player: p })) => player.matches(*p, you).then(|| TrigCtx { player: Some(*p), ..Default::default() }),
        (TrigExt::PlayerDealtDamage { player, combat }, TrigEvent::Damage { target: Target::Player(p), amount, combat: c, source }) => {
            (player.matches(*p, you) && combat.is_none_or(|want| want == *c)).then(|| TrigCtx { obj: Some(*source), player: Some(*p), amount: *amount, ..Default::default() })
        }
        (TrigExt::DamageDealtOnce { source }, TrigEvent::Ext(ExtEvent::DealtDamage { source: o, amount })) => {
            obj_matches(s, db, source, *o, fcx).then(|| TrigCtx { obj: Some(*o), amount: *amount, ..Default::default() })
        }
        (TrigExt::ZoneBatch { origin, dest, valid }, TrigEvent::Ext(ExtEvent::Batch)) => {
            let n = s.ext.batch.iter().filter(|&&(o, from, to)| origin.has(from) && dest.has(to) && obj_matches(s, db, valid, o, fcx)).count();
            (n > 0).then(|| TrigCtx { amount: n as i32, ..Default::default() })
        }
        (TrigExt::Valiant { valid }, TrigEvent::BecomesTarget { target, by_source, controller, .. }) => {
            (*controller == you && !crate::ext4::was_targeted_by(s, *target, you) && obj_matches(s, db, valid, *target, fcx)).then(|| TrigCtx { obj: Some(*by_source), player: Some(*controller), ..Default::default() })
        }
        (TrigExt::Drawn { valid, nth }, TrigEvent::Ext(ExtEvent::Drawn { player, card })) => {
            // The draw was already counted (`ext5::LeState::draws`).
            let nth_ok = *nth == 0 || s.ext.le.draws[player.idx()] == *nth;
            (nth_ok && obj_matches(s, db, valid, *card, fcx)).then(|| TrigCtx { obj: Some(*card), player: Some(*player), ..Default::default() })
        }
        (TrigExt::LifeLost { player }, TrigEvent::Ext(ExtEvent::LifeLost { player: p, amount })) => player.matches(*p, you).then(|| TrigCtx { player: Some(*p), amount: *amount, ..Default::default() }),
        (TrigExt::ActivatedNonMana, TrigEvent::Ext(ExtEvent::AbilityActivated { player, source, key })) => (*player == you).then(|| TrigCtx { obj: Some(*source), player: Some(*player), amount: *key as i32, ..Default::default() }),
        (TrigExt::DiscardedCard, TrigEvent::Ext(ExtEvent::Discarded { player })) => (*player == you).then(|| TrigCtx { player: Some(*player), ..Default::default() }),
        (TrigExt::DiscardedAll { player: sel }, TrigEvent::Ext(ExtEvent::DiscardedAll { player, amount })) => sel.matches(*player, you).then(|| TrigCtx { player: Some(*player), amount: *amount, ..Default::default() }),
        (TrigExt::ClassLevelSelf { level }, TrigEvent::Ext(ExtEvent::ClassLevel { obj, level: l })) => (*obj == src && l == level).then(|| TrigCtx { obj: Some(*obj), ..Default::default() }),
        (TrigExt::CounterAddedSelf { kind, n }, TrigEvent::Ext(ExtEvent::CounterAdded { obj, kind: k, before, after })) => (*obj == src && k == kind && *before < *n && *after >= *n).then(|| TrigCtx { obj: Some(*obj), ..Default::default() }),
        (TrigExt::TransformedSelf, TrigEvent::Ext(ExtEvent::Transformed { obj })) => (*obj == src).then(|| TrigCtx { obj: Some(*obj), ..Default::default() }),
        (TrigExt::DoorUnlocked { half }, TrigEvent::Ext(ExtEvent::DoorUnlocked { obj, half: h, player })) => (*obj == src && h == half && *player == you).then(|| TrigCtx { obj: Some(*obj), player: Some(*player), ..Default::default() }),
        (TrigExt::LoyaltyActivated { player }, TrigEvent::Ext(ExtEvent::LoyaltyActivated { player: p, source })) => player.matches(*p, you).then(|| TrigCtx { obj: Some(*source), player: Some(*p), ..Default::default() }),
        (TrigExt::ZoneBatchAfter { origin, dest, valid }, TrigEvent::Ext(ExtEvent::AfterBatch)) => {
            let n = crate::ext5::batch_matches(s, db, *origin, *dest, valid, fcx);
            (n > 0).then(|| TrigCtx { amount: n as i32, ..Default::default() })
        }
        _ => None,
    }
}

/// Delayed triggers and effect-object triggers for an event (called from `triggers::fire`).
pub fn fire_extra(s: &mut State, db: &CardDb, ev: &TrigEvent) {
    // CR 603.7c: a delayed triggered ability triggers only once (the ones here are one-shot).
    let mut fired: Vec<PendingTrigger> = vec![];
    let mut keep: Vec<DelayedTrig> = vec![];
    for d in std::mem::take(&mut s.ext.delayed) {
        let hit = match (d.when, ev) {
            (DelayedWhen::Leaves { obj, to }, TrigEvent::ZoneChange { old, from: Zone::Battlefield, to: z, .. }) => obj == *old && to.has(*z),
            (DelayedWhen::DiesWithin { obj, turn }, TrigEvent::ZoneChange { old, from: Zone::Battlefield, to: Zone::Graveyard, .. }) => obj == *old && turn == s.turn.number,
            (DelayedWhen::NextStep { step }, TrigEvent::StepBegin { step: st }) => step == *st,
            (DelayedWhen::PlayersNextStep { step, player }, TrigEvent::StepBegin { step: st }) => step == *st && s.turn.active == player,
            _ => false,
        };
        if hit && delayed_takes_all(db, d.when, d.execute) {
            // Its remembered objects go with it to resolution (`ext5::delayed_remembered`).
            s.ext.le.delayed_rem.push((d.source, d.execute, d.remembered.iter().copied().collect()));
        }
        if hit {
            let trig = match ev {
                TrigEvent::ZoneChange { old, new, .. } => TrigCtx { obj: Some(*old), new_obj: *new, ..Default::default() },
                _ => TrigCtx { obj: d.remembered.first().copied(), ..Default::default() },
            };
            fired.push(PendingTrigger { source: d.source, card: d.source_card, controller: d.controller, trigger_idx: 0, trig, adhoc: Some(d.execute) });
        } else {
            keep.push(d);
        }
    }
    s.ext.delayed = keep;
    crate::ext7::paradigm_triggers(s, db, ev, &mut fired);
    crate::ext6::granted_triggers(s, db, ev, &mut fired);
    // Effect objects' triggered abilities ("whenever you attack this turn").
    for eo in &s.effect_objs {
        for &ti in &eo.triggers {
            let td = &db.triggers_pool[ti as usize];
            if crate::triggers::trigger_suppressed(s, db, ev, td, crate::triggers::WardGrant::Effect { ts: eo.ts }) {
                continue;
            }
            if let Some(trig) = crate::triggers::matches(s, db, &td.mode, ev, eo.source, eo.controller) {
                fired.push(PendingTrigger { source: eo.source, card: s.obj(eo.source).card, controller: eo.controller, trigger_idx: 0, trig, adhoc: Some(td.execute) });
            }
        }
    }
    s.pending_triggers.extend(fired);
}

/// Fire "whenever [player] sacrifices" before the sacrificed permanents leave (look-back).
pub fn fire_sacrificed(cx: &mut Ctx, objs: &[ObjId], player: PlayerId) {
    for &o in objs {
        if cx.s.obj(o).zone == Zone::Battlefield {
            cx.s.ext.sacrificed_this_turn.push((player, o));
            crate::triggers::fire(cx.s, cx.db, &TrigEvent::Ext(ExtEvent::Sacrificed { obj: o, player }));
        }
    }
}

/// Register a delayed triggered ability about one object (CR 603.7a): "when it dies", "when it
/// leaves" (earthbend), one per object by its own nature. A delayed ability about a set of
/// objects is `add_delayed_over`.
pub fn add_delayed_for(cx: &mut Ctx, when: DelayedWhen, controller: PlayerId, source: ObjId, execute: AbilityId, obj: ObjId) {
    push_delayed(cx, when, controller, source, execute, &[obj]);
}

/// Register one delayed triggered ability over the set `objs` ("sacrifice them / return them at
/// the beginning of the next end step", mobilize's tokens, "at the beginning of your next upkeep,
/// ... them"): CR 603.7a creates one ability, which triggers once (603.7c) and acts on all of them.
/// Its effect must take the whole set (`delayed_takes_all`); over two or more objects an effect
/// that acts on one is refused loudly instead of quietly acting on the first.
pub fn add_delayed_over(cx: &mut Ctx, when: DelayedWhen, controller: PlayerId, source: ObjId, execute: AbilityId, objs: &[ObjId]) -> crate::engine::R<()> {
    if objs.len() > 1 && !delayed_takes_all(cx.db, when, execute) {
        return cx.unsupported("a delayed triggered ability over several objects whose effect acts on one of them");
    }
    push_delayed(cx, when, controller, source, execute, objs);
    Ok(())
}

fn push_delayed(cx: &mut Ctx, when: DelayedWhen, controller: PlayerId, source: ObjId, execute: AbilityId, remembered: &[ObjId]) {
    let source_card = cx.s.obj(source).card;
    cx.s.ext.delayed.push(DelayedTrig { when, controller, source, source_card, execute, remembered: remembered.iter().copied().collect() });
}

/// Does a delayed ability's effect act on all the objects it remembers (they go with it to
/// resolution, `ext5::delayed_remembered`)? "At the beginning of your next [step]" abilities
/// (`DelayedWhen::PlayersNextStep`), mobilize's "sacrifice them" (`AltEffect::SacrificeRemembered`)
/// and `AtEOT$ Sacrifice`'s (`ExtEffect::SacrificeDefined` on `Remembered`); any other effect acts
/// on the trigger's one object (`Defined::Triggered`).
pub(crate) fn delayed_takes_all(db: &CardDb, when: DelayedWhen, execute: AbilityId) -> bool {
    if matches!(when, DelayedWhen::PlayersNextStep { .. }) {
        return true;
    }
    match &db.ability(execute).effect {
        crate::carddb::Effect::Alt(crate::altcast::AltEffect::SacrificeRemembered) => true,
        crate::carddb::Effect::Ext(crate::effect_ext::ExtEffect::SacrificeDefined) => db.ability(execute).defined == Some(crate::carddb::Defined::Remembered),
        _ => false,
    }
}

/// Create a reflexive triggered ability now (CR 603.12): it goes on the stack the next time a
/// player would receive priority, with its targets chosen then.
pub fn add_reflexive(cx: &mut Ctx, controller: PlayerId, source: ObjId, execute: AbilityId, trig: TrigCtx) {
    let card = cx.s.obj(source).card;
    cx.s.pending_triggers.push(PendingTrigger { source, card, controller, trigger_idx: 0, trig, adhoc: Some(execute) });
}
