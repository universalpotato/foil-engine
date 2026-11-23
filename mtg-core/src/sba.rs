//! State-based actions (CR 704) and putting triggered abilities on the stack (CR 603.3).

use crate::basics::*;
use crate::carddb::Kw;
use crate::engine::*;
use crate::query::*;
use crate::state::*;
use smallvec::SmallVec;

/// Perform all applicable SBAs simultaneously (CR 704.3). Returns whether any happened.
pub fn check(cx: &mut Ctx) -> R<bool> {
    let mut any = false;
    // 704.5a–c: losing the game.
    let losers: SmallVec<[PlayerId; 2]> = [PlayerId(0), PlayerId(1)]
        .into_iter()
        .filter(|&p| {
            let pl = cx.s.player(p);
            pl.life <= 0 || pl.drew_from_empty || pl.poison >= 10
        })
        .collect();
    if !losers.is_empty() {
        let result = if losers.len() == 2 { GameResult::Draw } else { GameResult::Win(losers[0].opp()) };
        cx.s.result = Some(result);
        cx.s.stage = Stage::Over;
        cx.s.emit(Event::GameOver { result });
        return Err(Halt::GameOver);
    }
    // 704.5d: tokens outside the battlefield cease to exist.
    let stray: Vec<ObjId> = cx
        .s
        .players
        .iter()
        .flat_map(|p| p.graveyard.iter().chain(&p.hand).chain(&p.library))
        .chain(&cx.s.exile)
        .copied()
        .filter(|&o| cx.s.obj(o).token)
        .collect();
    for o in stray {
        cx.s.move_obj(o, Zone::Gone, false);
        any = true;
    }
    crate::static_ext::end_untapped_effects(cx.s);
    // 704.5aa: start your engines! gives a player without speed a speed of 1. Storied
    // (702.195a, a static ability): three or more artifacts/Sagas/legendaries give the
    // controller an enduring story for the rest of the game.
    let saga = cx.db.subtype("Saga");
    for p in [PlayerId(0), PlayerId(1)] {
        let need_speed = cx.s.ext.speed[p.idx()] == 0;
        let need_story = cx.s.ext.enduring_story & (1 << p.0) == 0;
        if !need_speed && !need_story {
            continue;
        }
        let (mut engines, mut storied) = (false, false);
        for &o in &cx.s.battlefield {
            let ob = cx.s.obj(o);
            if ob.controller != p {
                continue;
            }
            let ex = &cx.db.get(ob.card).face(ob.face).extra;
            if ex.start_engines || ex.storied {
                let lost = crate::query::may_lose_abilities(cx.s, cx.db) && lost_all_abilities(cx.s, cx.db, o);
                engines |= ex.start_engines && !lost;
                storied |= ex.storied && !lost;
            }
        }
        let story_count = if need_story && storied {
            cx.s.battlefield
                .iter()
                .filter(|&&o| cx.s.obj(o).controller == p)
                .filter(|&&o| {
                    let c = chars(cx.s, cx.db, o);
                    c.types.has(CardType::Artifact) || c.supertypes.legendary() || saga.is_some_and(|st| c.subtypes.contains(&st))
                })
                .count()
        } else {
            0
        };
        if need_speed && engines {
            cx.s.ext.speed[p.idx()] = 1;
            any = true;
        }
        if need_story && storied && story_count >= 3 {
            cx.s.ext.enduring_story |= 1 << p.0;
            any = true;
        }
    }
    let mut to_gy: SmallVec<[ObjId; 4]> = SmallVec::new();
    let mut to_destroy: SmallVec<[ObjId; 4]> = SmallVec::new();
    // Legendary permanents per player (704.5j, below), computed with the same characteristics
    // while they're memoized; recomputed if anything changes in between.
    let db = cx.db;
    let legends_of = |s: &State, p: PlayerId| -> Vec<ObjId> { s.battlefield.iter().copied().filter(|&o| s.obj(o).controller == p && chars(s, db, o).supertypes.legendary()).collect() };
    let order = [cx.s.turn.active, cx.s.turn.active.opp()];
    let mut legends_pre: [Vec<ObjId>; 2] = Default::default();
    // Power/toughness past the game-number bound (`num`): derived values never wrap, but the
    // game can't continue on them (checked here, before any player gets priority, CR 704.3).
    let mut pt_out: Option<(ObjId, i32, i32)> = None;
    frozen(cx.s, cx.db, || {
        legends_pre = order.map(|p| legends_of(cx.s, p));
        for &o in &cx.s.battlefield {
            let c = chars(cx.s, cx.db, o);
            if (crate::num::big(c.power) || crate::num::big(c.toughness)) && pt_out.is_none() {
                pt_out = Some((o, c.power, c.toughness));
            }
            let ob = cx.s.obj(o);
            if c.types.has(CardType::Creature) {
                if c.toughness <= 0 {
                    to_gy.push(o); // 704.5f
                } else if (ob.damage >= c.toughness || ob.deathtouched) && !c.kws.has(Kw::Indestructible) {
                    to_destroy.push(o); // 704.5g–h
                }
            }
            if c.types.has(CardType::Planeswalker) && ob.counter(CounterKind::Loyalty) <= 0 && !crate::ext9::ignores_zero_loyalty(cx.s, cx.db, o) {
                to_gy.push(o); // 704.5i (unless a static says otherwise: Sanctum Lurker, `ext9`)
            }
        }
    });
    if let Some((o, p, t)) = pt_out {
        let what = format!("power/toughness of {} ({p}/{t})", cx.db.get(cx.s.obj(o).card).name);
        cx.s.note_overflow(&what, if crate::num::big(p) { p as i64 } else { t as i64 });
        return cx.unsupported(cx.s.overflow.clone().unwrap_or_default());
    }
    // 704.5q: +1/+1 and -1/-1 counters annihilate.
    let mut changed = false;
    for &o in &cx.s.battlefield.clone() {
        let (pp, mm) = (cx.s.obj(o).counter(CounterKind::P1P1), cx.s.obj(o).counter(CounterKind::M1M1));
        if pp > 0 && mm > 0 {
            let n = pp.min(mm);
            let ob = cx.s.obj_mut(o);
            ob.add_counter(CounterKind::P1P1, -n);
            ob.add_counter(CounterKind::M1M1, -n);
            any = true;
            changed = true;
        }
    }
    // Clear deathtouch markers (they only matter "since the last time SBAs were checked").
    // Written only where set: a write unshares the object's copy-on-write chunk (`ObjVec`).
    for &o in &cx.s.battlefield.clone() {
        if cx.s.obj(o).deathtouched {
            changed = true;
            cx.s.obj_mut(o).deathtouched = false;
        }
    }
    if changed {
        legends_pre = frozen(cx.s, cx.db, || order.map(|p| legends_of(cx.s, p)));
    }
    // 704.5j: legend rule.
    let mut legend_losers: SmallVec<[ObjId; 2]> = SmallVec::new();
    for (pi, p) in order.into_iter().enumerate() {
        let legends: Vec<ObjId> = std::mem::take(&mut legends_pre[pi]);
        let mut done: SmallVec<[CardId; 2]> = SmallVec::new();
        for &o in &legends {
            let name_card = crate::query::name_card(cx.s, o);
            if done.contains(&name_card) {
                continue;
            }
            done.push(name_card);
            let same: Vec<ObjId> = legends.iter().copied().filter(|&x| cx.db.get(crate::query::name_card(cx.s, x)).name == cx.db.get(name_card).name).collect();
            if same.len() >= 2 {
                let opts: Vec<Opt> = same.iter().map(|&x| Opt::Card(x)).collect();
                let Opt::Card(keep) = cx.choose(Decision { player: p, kind: DecisionKind::LegendRule, options: opts, subject: None })? else { unreachable!() };
                legend_losers.extend(same.into_iter().filter(|&x| x != keep));
            }
        }
    }
    // 704.5s: a Saga with one or more chapter abilities and lore >= its final chapter number,
    // not the source of a chapter ability that has triggered but not yet left the stack, is
    // sacrificed.
    let mut sagas_done: SmallVec<[ObjId; 1]> = SmallVec::new();
    for &o in &cx.s.battlefield {
        let card = cx.s.obj(o).card;
        let fin = crate::query::final_chapter_of(cx.s, cx.db, o);
        if fin == 0 || crate::query::lore(cx.s, cx.db, o) < fin as i32 {
            continue;
        }
        let is_chapter = |idx: u8| matches!(cx.db.get(card).front().triggers.get(idx as usize).map(|t| &t.mode), Some(crate::carddb::TrigMode::Chapter { .. }));
        let pending = cx.s.pending_triggers.iter().any(|t| t.source == o && is_chapter(t.trigger_idx)) || cx.s.stack.iter().any(|it| it.kind == StackKind::Triggered && it.source == o && it.trigger_idx.is_some_and(is_chapter));
        if !pending {
            sagas_done.push(o);
        }
    }
    // 704.5m: an Aura attached illegally or to nothing goes to the graveyard; 704.5n: an
    // Equipment attached illegally becomes unattached.
    let mut auras_to_gy: SmallVec<[ObjId; 2]> = SmallVec::new();
    for &o in &cx.s.battlefield.clone() {
        let ob = cx.s.obj(o);
        let aura = cx.db.get(ob.card).face(ob.face).enchant.is_some() && chars(cx.s, cx.db, o).types.has(CardType::Enchantment);
        match ob.attached_to {
            Some(t) if !crate::resolve::can_attach(cx.s, cx.db, o, t) => {
                if aura {
                    auras_to_gy.push(o);
                } else {
                    cx.s.obj_mut(o).attached_to = None;
                    any = true;
                }
            }
            None if aura => auras_to_gy.push(o),
            _ => {}
        }
    }
    // All SBA zone changes happen simultaneously (CR 704.3).
    let mut moves: Vec<crate::zones::Move> = vec![];
    for o in to_destroy {
        let card = cx.s.obj(o).card;
        cx.s.emit(Event::Destroyed { card });
        moves.push(crate::zones::Move::new(o, Zone::Graveyard));
    }
    // CR 303.7a: extra Roles one player controls on one permanent.
    let roles = crate::ext3::roles_to_graveyard(cx.s, cx.db);
    for o in to_gy.into_iter().chain(legend_losers).chain(auras_to_gy).chain(sagas_done).chain(roles) {
        if !moves.iter().any(|m| m.obj == o) {
            moves.push(crate::zones::Move::new(o, Zone::Graveyard));
        }
    }
    if !moves.is_empty() {
        crate::zones::move_batch(cx, &moves)?;
        any = true;
    }
    Ok(any)
}

/// Merge key of a pending trigger at `OrderTriggers` (CR 603.3b: the player orders their
/// triggers; agent mode offers identical triggers once). Identical = the same ability (card
/// and printed trigger, or the same ad-hoc ability: delayed, reflexive, effect-object triggers)
/// of interchangeable sources (`query::obj_sig`) for the same trigger event: its objects (and
/// a life gain's source, CR 119.9, 702.15e) by signature (the source itself as such), its player, amount and stack item exactly. A source
/// that left the battlefield, a summoning-sick or tapped copy, a different dying creature, ... is
/// a different trigger whose order is a real choice (checked by the canonicalisation
/// soundness test).
/// The objects are also told apart by what the other triggers being ordered say about them
/// (`TrigRefs`): two Simulacrum Synthesizers' triggers for the same artifact entering are not
/// identical when one Synthesizer entered with it (its own trigger and the other's are about it).
pub type TriggerKey = (CardId, u8, Option<crate::carddb::AbilityId>, (ObjSig, TrigRefs), [(u8, ObjSig, TrigRefs); 4], Option<PlayerId>, i32, Option<u32>);

/// The roles an object plays in the other triggers being ordered: (role: 0 source, 1-3 event
/// object / new object / target, 4 life gain source; the trigger's card, printed index, ad-hoc ability), sorted.
pub type TrigRefs = SmallVec<[(u8, CardId, u8, Option<crate::carddb::AbilityId>); 2]>;

/// `trigger_key` of trigger `i` of `all` (the triggers its controller is ordering).
pub fn trigger_key(s: &State, db: &crate::carddb::CardDb, all: &[PendingTrigger], i: usize) -> TriggerKey {
    trigger_key_with(all, i, |x| obj_sig(s, db, x))
}

/// `trigger_key` of every trigger of `all`, in order: the objects' signatures from one
/// `obj_sigs` scan (memoized characteristics on a big board) instead of an `obj_sig` (a
/// whole-state scan) per object per trigger. Same keys. (An end
/// step with dozens of Phoenix Fleet Airship copy triggers to order, one choice at a time.)
pub fn trigger_keys(s: &State, db: &crate::carddb::CardDb, all: &[PendingTrigger]) -> Vec<TriggerKey> {
    let mut objs: Vec<ObjId> = all.iter().flat_map(|t| [Some(t.source), t.trig.obj, t.trig.new_obj, t.trig.target, t.trig.gain_source]).flatten().collect();
    objs.sort_unstable();
    objs.dedup();
    let sigs = crate::query::frozen_if_big(s, db, || crate::query::obj_sigs(s, db, &objs));
    let sig = |x: ObjId| sigs[objs.binary_search(&x).expect("collected above")];
    (0..all.len()).map(|i| trigger_key_with(all, i, sig)).collect()
}

fn trigger_key_with(all: &[PendingTrigger], i: usize, obj_sig: impl Fn(ObjId) -> crate::query::ObjSig) -> TriggerKey {
    let t = &all[i];
    let refs = |x: ObjId| -> TrigRefs {
        let mut v: TrigRefs = SmallVec::new();
        for (j, u) in all.iter().enumerate() {
            if j == i {
                continue;
            }
            for (role, y) in [(0u8, Some(u.source)), (1, u.trig.obj), (2, u.trig.new_obj), (3, u.trig.target), (4, u.trig.gain_source)] {
                if y == Some(x) {
                    v.push((role, u.card, u.trigger_idx, u.adhoc));
                }
            }
        }
        v.sort_unstable();
        v
    };
    let obj = |o: Option<ObjId>| match o {
        None => (0, (0, 0, 0, 0, 0, 0), SmallVec::new()),
        Some(x) if x == t.source => (1, (0, 0, 0, 0, 0, 0), SmallVec::new()),
        Some(x) => (2, obj_sig(x), refs(x)),
    };
    let c = &t.trig;
    (t.card, t.trigger_idx, t.adhoc, (obj_sig(t.source), refs(t.source)), [obj(c.obj), obj(c.new_obj), obj(c.target), obj(c.gain_source)], c.player, c.amount, c.stack_item)
}

/// CR 603.3b: APNAP order; each player orders their own triggers. Targets chosen now (603.3d);
/// a trigger with no legal targets is removed.
pub fn put_triggers_on_stack(cx: &mut Ctx) -> R<bool> {
    if cx.s.pending_triggers.is_empty() {
        return Ok(false);
    }
    let pending = std::mem::take(&mut cx.s.pending_triggers);
    for p in [cx.s.turn.active, cx.s.turn.active.opp()] {
        let mut mine: Vec<PendingTrigger> = pending.iter().filter(|t| t.controller == p).cloned().collect();
        while !mine.is_empty() {
            let i = if mine.len() == 1 {
                0
            } else {
                // Identical triggers are interchangeable (`trigger_key`).
                let mut opts = vec![];
                let mut seen: SmallVec<[TriggerKey; 4]> = SmallVec::new();
                for (i, k) in trigger_keys(cx.s, cx.db, &mine).into_iter().enumerate() {
                    if !seen.contains(&k) {
                        seen.push(k);
                        opts.push(Opt::Trigger(i as u8));
                    }
                }
                cx.s.ext.ordering.clone_from(&mine);
                // Full fidelity (human seat): every trigger is its own option, so a human can order
                // two "identical" triggers whose sources or trigger events differ (CR 603.3b).
                let full = cx.full(p).then(|| (0..mine.len()).map(|i| Opt::Trigger(i as u8)).collect());
                // Left set in the paused scratch state (`Halt::Need`) for the view.
                let c = cx.choose_full(Decision { player: p, kind: DecisionKind::OrderTriggers, options: opts, subject: None }, full)?;
                cx.s.ext.ordering.clear();
                let Opt::Trigger(i) = c else { unreachable!() };
                i as usize
            };
            let t = mine.remove(i);
            let ability = match t.adhoc {
                Some(a) => a,
                None => cx.db.get(t.card).front().triggers[t.trigger_idx as usize].execute,
            };
            // 603.3c-d: modes and targets are chosen now; no legal choice → removed.
            if crate::cast::mode_sets(cx, p, ability, Some(t.source)).is_empty() || crate::ext9::lacks_targets_excluding_trigger(cx, p, ability, t.source, &t.trig) {
                continue;
            }
            // While its modes and targets are chosen, the sources and event objects of this
            // trigger and of the others being put on the stack (taken from `pending_triggers`,
            // so `query::refs_of` doesn't see them) are referred to: not interchangeable with
            // identical objects (targeting the card another trigger is about, or this trigger's
            // source, differs from targeting its twin). Restored after: a trigger may be put on the
            // stack while a cost is paid.
            let saved = std::mem::take(&mut cx.s.ext.x6.paying_for);
            cx.s.ext.x6.paying_for.extend(pending.iter().flat_map(|u| [Some(u.source), u.trig.obj, u.trig.new_obj, u.trig.target]).flatten());
            // The view of the trigger being put on the stack (CR 603.3: the topmost object on the
            // stack; 603.3c-d: its modes and targets are chosen now): `State::being_put`, with the
            // choices made so far. Read by observations only (behaviour-neutral); kept in the
            // paused state (`Halt::Need`) for the view, else cleared.
            let x0 = cx.s.ext.x_paid.iter().find(|x| x.0 == t.source).map(|x| x.1).unwrap_or(0);
            cx.s.being_put = Some(StackItem {
                kind: StackKind::Triggered,
                controller: p,
                source: t.source,
                source_card: t.card,
                ability,
                targets: SmallVec::new(),
                x: x0,
                kicked: false,
                modes: SmallVec::new(),
                trig: t.trig,
                trigger_idx: if t.adhoc.is_some() { None } else { Some(t.trigger_idx) },
                flashback: false,
                uid: 0,
            });
            let chosen = (|| -> R<_> {
                let modes = crate::cast::choose_modes(cx, p, ability, t.source)?;
                if let Some(bp) = cx.s.being_put.as_mut() {
                    bp.modes = modes.clone();
                }
                let x = cx.s.ext.x_paid.iter().find(|x| x.0 == t.source).map(|x| x.1).unwrap_or(0);
                let targets = crate::cast::choose_targets_ctx(cx, p, ability, &modes, t.source, x, t.trig)?;
                Ok((modes, targets))
            })();
            cx.s.ext.x6.paying_for = saved;
            if !matches!(chosen, Err(Halt::Need(_))) {
                cx.s.being_put = None;
            }
            let (modes, targets) = chosen?;
            let item = cx.s.push_stack(StackItem {
                kind: StackKind::Triggered,
                controller: p,
                source: t.source,
                source_card: t.card,
                ability,
                targets,
                x: cx.s.ext.x_paid.iter().find(|x| x.0 == t.source).map(|x| x.1).unwrap_or(0),
                kicked: false,
                modes,
                trig: t.trig,
                trigger_idx: if t.adhoc.is_some() { None } else { Some(t.trigger_idx) },
                flashback: false,
                uid: 0,
            });
            let tobjs: SmallVec<[ObjId; 2]> = cx.s.stack.last().unwrap().targets.iter().filter_map(|t| if let Target::Obj(o) = t.1 { Some(o) } else { None }).collect();
            crate::cast::fire_targeted(cx, &tobjs, t.source, item, p);
            crate::ext2::fire_crime_top(cx);
            cx.s.emit(Event::TriggerPut { player: p, card: t.card });
        }
    }
    Ok(true)
}
