//! Zone changes as game actions: replacement effects (CR 614), look-back triggers (603.10a),
//! "as enters" modifications, ETB triggers and events. All rules code moves objects through
//! here; `State::move_obj` is the low-level primitive.

use crate::basics::*;
use crate::carddb::*;
use crate::engine::*;
use crate::query::{chars, obj_matches, FilterCtx};
use crate::state::*;
use crate::triggers::{fire, TrigEvent};
use smallvec::SmallVec;

#[derive(Clone, Copy, Debug)]
pub struct Move {
    pub obj: ObjId,
    pub to: Zone,
    pub to_top: bool,
    pub tapped: bool,
    /// Controller on the battlefield (default: owner).
    pub controller: Option<PlayerId>,
}
impl Move {
    pub fn new(obj: ObjId, to: Zone) -> Move {
        Move { obj, to, to_top: false, tapped: false, controller: None }
    }
}

pub fn move_to(cx: &mut Ctx, obj: ObjId, to: Zone) -> R<Option<ObjId>> {
    Ok(move_batch(cx, &[Move::new(obj, to)])?[0])
}

/// Redirect replacements (e.g. Rest in Peace) applying to moving `obj` from its zone to `to`.
///
/// CR 616.1: when several apply, the affected object's controller (or its owner) chooses the
/// order; after each, the others are checked again on the modified event (616.1f). Every
/// redirect the engine supports goes to exile (Rest in Peace, Leyline of the Void, Dryad
/// Militant, Hades, Garruk, Veiled Butcher, Vren; finality counters and "exile it instead if it
/// would die this turn" in `ext3::redirect`) and does nothing else (`compile_replacement`
/// refuses any other part), so once one applies the others ("would be put into a graveyard")
/// don't, and every order gives the same move: no choice is needed. Applicable redirects to
/// different zones would make the order matter: the move is refused (`Err`) until that choice
/// (`DecisionKind::ChooseReplacement`, as `as_enters`) is implemented.
fn redirect(s: &State, db: &CardDb, obj: ObjId, to: Zone) -> Result<Zone, String> {
    let from = s.obj(obj).zone;
    let mut dest = to;
    // Apply each applicable redirect at most once (CR 614.5, 616.1).
    let mut applied: SmallVec<[(ObjId, u8); 2]> = SmallVec::new();
    loop {
        let mut next = None;
        let mut other_zone = None;
        for &src in s.battlefield.iter().chain(std::iter::once(&obj)) {
            let so = s.obj(src);
            for (i, r) in db.get(so.card).front().replacements.iter().enumerate() {
                let ReplAction::Redirect(z) = r.action else { continue };
                // A permanent's own static redirect applies to its own move too: it is still on
                // the battlefield when the event would happen (CR 614.1; Rest in Peace ruling
                // 2018-03-16: "If Rest in Peace is destroyed by a spell, Rest in Peace will be
                // exiled").
                let active = match r.zones {
                    Some(zs) => zs.has(so.zone),
                    None => src == obj,
                };
                if !active || applied.contains(&(src, i as u8)) || !r.origin.has(from) || !r.dest.has(dest) {
                    continue;
                }
                // CR 613.1f / 113.6: a permanent that lost all abilities has no replacement effects.
                if so.zone == Zone::Battlefield && crate::query::may_lose_abilities(s, db) && crate::query::lost_all_abilities(s, db, src) {
                    continue;
                }
                if obj_matches(s, db, &r.valid, obj, FilterCtx { you: so.controller, source: Some(src), x: 0, rem: &[] }) {
                    match next {
                        None => next = Some((src, i as u8, z)),
                        Some((_, _, nz)) if nz != z => other_zone = Some(z),
                        Some(_) => {}
                    }
                }
            }
        }
        // The effect-based redirects (`ext3`) compete with these too.
        let late = crate::ext3::redirect(s, db, obj, from, dest);
        if let Some((_, _, z)) = next {
            if late != dest && late != z {
                other_zone = Some(late);
            }
            if let Some(o) = other_zone {
                return Err(format!("replacement effects moving the object to {z:?} or to {o:?} instead: their order is a choice (CR 616.1), not implemented"));
            }
        }
        match next {
            Some((src, i, z)) => {
                applied.push((src, i));
                dest = z;
            }
            None => return Ok(late),
        }
    }
}

/// Move several objects simultaneously. Returns the new object ids (None if it ceased to exist).
pub fn move_batch(cx: &mut Ctx, moves: &[Move]) -> R<Vec<Option<ObjId>>> {
    // 1. Final destinations after redirect replacements. Read-only (the snapshots are recorded
    // after: a snapshot of a permanent still on the battlefield is never read, `lki::of`), so on
    // a big board it runs in one memo scope (`frozen_if_big`; same values) instead of every
    // moving permanent's characteristics recounting the battlefield
    // (dozens of Phoenix Fleet Airship copies leaving at once). A redirect order that
    // is a choice (`redirect`'s `Err`, CR 616.1) is refused after the scope.
    #[allow(clippy::type_complexity)]
    let step1: Result<(SmallVec<[(Move, Zone, Zone, CardId, bool); 4]>, SmallVec<[(ObjId, crate::query::Chars); 4]>), String> = crate::query::frozen_if_big(cx.s, cx.db, || {
        let (mut plan, mut snaps) = (SmallVec::new(), SmallVec::new());
        for m in moves {
            let o = cx.s.obj(m.obj);
            if o.zone == Zone::Gone {
                continue;
            }
            let from = o.zone;
            // CR 111.8: a token that has left the battlefield can't move to another zone or come
            // back onto the battlefield; it stays where it is (and ceases to exist at the next
            // state-based action check, 704.5d). (Copies of spells, also `token`, leave the stack
            // under 707.10a instead.)
            if o.token && !matches!(from, Zone::Battlefield | Zone::Stack) {
                continue;
            }
            let card = o.card;
            let to = redirect(cx.s, cx.db, m.obj, m.to)?;
            let mut was_creature = false;
            if from == Zone::Battlefield {
                // Last known information, before any object of this event moves (CR 608.2h, 603.10a).
                let c = chars(cx.s, cx.db, m.obj);
                was_creature = c.types.has(CardType::Creature);
                snaps.push((m.obj, c));
            }
            plan.push((*m, from, to, card, was_creature));
        }
        Ok((plan, snaps))
    });
    let (plan, snaps) = match step1 {
        Ok(x) => x,
        Err(e) => return cx.unsupported(e),
    };
    for (o, c) in snaps {
        crate::lki::record(cx.s, o, c);
    }
    if !plan.is_empty() {
        crate::invariants::move_event(plan.iter().map(|x| (x.1, x.2)));
    }
    // 2. Look-back triggers for objects leaving the battlefield, before anything moves.
    for &(m, from, to, _, _) in &plan {
        if from == Zone::Battlefield {
            fire(cx.s, cx.db, &TrigEvent::ZoneChange { old: m.obj, new: None, from, to });
        }
    }
    // "Whenever one or more ..." triggers see the whole batch once (look-back).
    if cx.db.any_zone_batch && plan.iter().any(|x| x.1 == Zone::Battlefield) {
        cx.s.ext.batch = plan.iter().map(|&(m, from, to, _, _)| (m.obj, from, to)).collect();
        fire(cx.s, cx.db, &TrigEvent::Ext(crate::trig_ext::ExtEvent::Batch));
        cx.s.ext.batch.clear();
    }
    // 3. Move. The objects are put onto the battlefield together once all have moved (one
    // event): one entering doesn't see another entering with it, as a replacement source or a
    // permanent "already" there (CR 614.12, 610.3d; Metallic Mimic ruling 2017-02-09, Curator
    // Beastie ruling 2024-09-20), and the triggers of the other moves see them
    // all (603.2, 603.6a).
    let mut out = vec![None; moves.len()];
    let mut entered: SmallVec<[(ObjId, ObjId, Zone); 4]> = SmallVec::new();
    let mut other_moves: SmallVec<[(ObjId, ObjId, Zone, Zone); 4]> = SmallVec::new();
    let mut into_lib: SmallVec<[(ObjId, bool); 2]> = SmallVec::new();
    for &(m, from, to, card, was_creature) in &plan {
        let idx = moves.iter().position(|x| x.obj == m.obj).unwrap();
        if to == Zone::Battlefield {
            let nid = cx.s.move_obj_limbo(m.obj, Zone::Battlefield);
            if from == Zone::Stack {
                // The resolving permanent spell is still on the stack while it enters (CR 608.3a,
                // 614.12a): its "as enters" choices are about the object it becomes.
                cx.s.spell_entering(m.obj, nid);
            }
            if cx.s.obj(nid).zone == Zone::Gone {
                continue;
            }
            if let Some(c) = m.controller {
                cx.s.obj_mut(nid).controller = c;
            }
            if m.tapped {
                cx.s.obj_mut(nid).tapped = true;
            }
            crate::lki::animate_entering(cx.s, cx.db, nid);
            // A Room enters with the designation of the half cast, if any (CR 709.5d). A Room
            // spell that is a copy enters the same way: a copy cast from Uldaros Theorix's
            // copies (707.12, 709.3c: "the token it becomes") and a copy of a Room spell (707.10;
            // Room ruling 2024-09-20: "the copy retains the choice of which door was cast ...
            // the token it becomes will enter with the appropriate door unlocked"). Every path
            // that puts a Room spell (or its copy) on the stack records the half as its `face`
            // (`altcast::put_on_stack_as_spell`, `ext6::copy_spell`, `CopyTriggeredSpell`).
            // Tokens were excluded, so such a copy entered with both doors locked.
            if cx.db.get(card).layout == crate::carddb::Layout::Room && !cx.s.obj(nid).face_down() {
                let cast_half = (from == Zone::Stack).then(|| cx.s.obj(m.obj).face.min(1));
                crate::ext5::set_room_state(cx.s, cx.db, nid, cast_half.map_or(0, |h| 1 << h));
            }
            // X paid for a permanent spell follows it onto the battlefield (CR 107.3m).
            if let Some(x) = cx.s.ext.x_paid.iter_mut().find(|x| x.0 == m.obj) {
                x.0 = nid;
            }
            // "... onto the battlefield with N [kind] counters" (CR 122.6).
            if let Some(i) = cx.s.ext.enter_with.iter().position(|e| e.0 == m.obj) {
                let (_, k, n) = cx.s.ext.enter_with.remove(i);
                crate::ext6::put_counters(cx.s, cx.db, nid, k, n);
            }
            if let Some((k, n)) = cx.s.ext.enter_with_node {
                crate::ext6::put_counters(cx.s, cx.db, nid, k, n);
            }
            as_enters(cx, nid)?;
            out[idx] = Some(nid);
            entered.push((m.obj, nid, from));
        } else {
            if from == Zone::Stack {
                // A spell moved off the stack by an effect (e.g. returned to its owner's hand)
                // is no longer a spell there (its stack entry goes with it).
                cx.s.stack.retain(|it| !(it.source == m.obj && it.kind == StackKind::Spell));
            }
            let nid = cx.s.move_obj(m.obj, to, m.to_top);
            let gone = cx.s.obj(nid).zone == Zone::Gone;
            if !gone {
                out[idx] = Some(nid);
                if to == Zone::Library {
                    into_lib.push((nid, m.to_top));
                }
            }
            if from == Zone::Battlefield {
                let owner = cx.s.obj(m.obj).owner;
                if to == Zone::Graveyard && was_creature {
                    cx.s.emit(Event::Dies { card, owner });
                    let ctrl = cx.s.obj(m.obj).controller;
                    crate::hist::creature_died(cx.s, ctrl, m.obj);
                }
                // Fill in the new incarnation for look-back triggers about this object.
                for t in cx.s.pending_triggers.iter_mut() {
                    if t.trig.obj == Some(m.obj) && t.trig.new_obj.is_none() && !gone {
                        t.trig.new_obj = Some(nid);
                    }
                }
            } else if !gone {
                other_moves.push((m.obj, nid, from, to));
            }
        }
    }
    for &(old, nid, from) in &entered {
        // A resolving permanent spell leaves the stack as it enters (CR 608.3, 608.3a).
        if from == Zone::Stack {
            cx.s.spell_left_stack(old);
        }
        cx.s.place(nid, false);
    }
    // CR 401.4: "If an effect puts two or more cards in a specific position in a library at the
    // same time, the owner of those cards may arrange them in any order. That library's owner
    // doesn't reveal the order": only the owner knows where each is (one card's
    // position is what `State::move_obj_limbo` says).
    for &(nid, top) in &into_lib {
        let owner = cx.s.obj(nid).owner;
        if into_lib.iter().filter(|&&(o, t)| t == top && cx.s.obj(o).owner == owner).count() > 1 {
            cx.s.obj_mut(nid).known_to &= 1 << owner.0;
        }
    }
    for (old, nid, from, to) in other_moves {
        fire(cx.s, cx.db, &TrigEvent::ZoneChange { old, new: Some(nid), from, to });
    }
    // 4. ETB triggers after everything has entered (CR 603.6a).
    for (old, nid, from) in entered {
        fire(cx.s, cx.db, &TrigEvent::ZoneChange { old, new: Some(nid), from, to: Zone::Battlefield });
        // A door unlocked as the Room entered triggers "when you unlock this door" (709.5h).
        if !cx.db.le.room_cards.is_empty() {
            crate::ext5::entered_unlocked(cx, nid);
        }
    }
    let moved: SmallVec<[(ObjId, Zone, Zone, Option<ObjId>); 4]> = plan.iter().map(|&(m, from, to, _, _)| (m.obj, from, to, out[moves.iter().position(|x| x.obj == m.obj).unwrap()])).collect();
    if cx.db.le.any_after_batch {
        crate::ext5::fire_after_batch(cx, &moved);
    }
    crate::ext6::flush_discards(cx, &moved);
    crate::effect_ext::after_moves(cx, &moved)?;
    Ok(out)
}

/// Put `obj` onto the battlefield under `controller`'s control attached to `to` (an Aura spell
/// resolving, CR 303.4a/608.3): replacements, as-enters effects and ETB triggers as for any
/// battlefield move.
pub fn move_attached(cx: &mut Ctx, obj: ObjId, controller: PlayerId, to: ObjId) -> R<Option<ObjId>> {
    move_special(cx, obj, controller, Some(to), false)
}

/// Put `obj` onto the battlefield under `controller`'s control, attached to `attach` and/or
/// transformed (back face up, CR 712.14) — "as it enters" properties that a plain `Move` lacks.
pub fn move_special(cx: &mut Ctx, obj: ObjId, controller: PlayerId, attach: Option<ObjId>, transformed: bool) -> R<Option<ObjId>> {
    let from = cx.s.obj(obj).zone;
    if cx.s.obj(obj).token && !matches!(from, Zone::Battlefield | Zone::Stack) {
        return Ok(None); // CR 111.8 (see `move_batch`)
    }
    // This path doesn't give a Room its unlocked designations (CR 709.5d; `move_batch` does). No
    // Room is an Aura or a double-faced card, so none comes here: fail loudly if one ever does.
    if cx.db.get(cx.s.obj(obj).card).layout == crate::carddb::Layout::Room {
        return cx.unsupported("a Room entering attached or transformed");
    }
    let back = if transformed { cx.db.back_of(cx.s.obj(obj).card) } else { None };
    if transformed && back.is_none() {
        return cx.unsupported("transforming a card without a back face");
    }
    let to = match redirect(cx.s, cx.db, obj, Zone::Battlefield) {
        Ok(z) => z,
        Err(e) => return cx.unsupported(e),
    };
    if to != Zone::Battlefield {
        return Ok(move_batch(cx, &[Move { obj, to: Zone::Battlefield, to_top: false, tapped: false, controller: Some(controller) }])?[0]);
    }
    crate::invariants::move_event(std::iter::once((from, Zone::Battlefield)));
    let nid = cx.s.move_obj_limbo(obj, Zone::Battlefield);
    if from == Zone::Stack {
        cx.s.spell_entering(obj, nid); // as in `move_batch` (CR 608.3a)
    }
    if cx.s.obj(nid).zone == Zone::Gone {
        return Ok(None);
    }
    cx.s.obj_mut(nid).controller = controller;
    cx.s.obj_mut(nid).attached_to = attach;
    crate::lki::animate_entering(cx.s, cx.db, nid);
    if let Some(b) = back {
        let printed = cx.s.obj(nid).card;
        cx.s.obj_mut(nid).card = b;
        cx.s.copy_effects.push(CopyEffect { obj: nid, printed, name: None, add_types: TypeSet::default(), add_subtypes: SmallVec::new(), kws: KwSet::default(), set_pt: None, transformed: true, add_super: 0 });
    }
    // "... onto the battlefield with N [kind] counters" (CR 122.6), as in `move_batch`.
    if let Some(i) = cx.s.ext.enter_with.iter().position(|e| e.0 == obj) {
        let (_, k, n) = cx.s.ext.enter_with.remove(i);
        crate::ext6::put_counters(cx.s, cx.db, nid, k, n);
    }
    if let Some((k, n)) = cx.s.ext.enter_with_node {
        crate::ext6::put_counters(cx.s, cx.db, nid, k, n);
    }
    as_enters(cx, nid)?;
    if from == Zone::Stack {
        cx.s.spell_left_stack(obj); // as in `move_batch` (CR 608.3a)
    }
    cx.s.place(nid, false);
    fire(cx.s, cx.db, &TrigEvent::ZoneChange { old: obj, new: Some(nid), from, to: Zone::Battlefield });
    if cx.db.le.any_after_batch {
        crate::ext5::fire_after_batch(cx, &[(obj, from, Zone::Battlefield, Some(nid))]);
    }
    Ok(Some(nid))
}

/// Create a token that's a copy of `src` (CR 707.2, 111.1): it copies the copiable values,
/// including `src`'s own copy effect (707.3).
/// Returns every token created: token doublers may create more than one (CR 614.1a; "it" is
/// each of them, Kiki-Jiki ruling 2021-03-19); none for a copy of an Aura with
/// nothing to enchant (CR 303.4g).
pub fn create_token_copy(cx: &mut Ctx, src: ObjId, owner: PlayerId) -> R<SmallVec<[ObjId; 2]>> {
    create_token_copy_except(cx, src, owner, CopyExc::default())
}

/// Exceptions of a "create a token that's a copy of ..., except ..." effect (CR 707.9). They
/// become part of the token's copiable values (707.9b), so a copy of the token has them too
/// (707.3; Three Steps Ahead ruling 2024-04-12).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CopyExc {
    /// "except it's [p]/[t]" (offspring's 1/1, CR 702.175a).
    pub set_pt: Option<(i32, i32)>,
    /// Casting-designation flags the tokens get (`altcast::flag`; not copiable values): the
    /// offspring token's `OFFSPRING_TOKEN` marker, which only the observation shows.
    pub flags: u16,
    /// Supertypes added (`Supertypes::*` bits): "except it's legendary" (Adagia, Windswept
    /// Bastion; ruling 2025-07-25: "only one of them (the newly created token) will be
    /// legendary").
    pub add_super: u8,
}

/// [`create_token_copy`] with the copy effect's exceptions, part of the token's copy effect
/// from the start: its own "as enters" replacements (CR 614.12) and the zone-change triggers see
/// them.
pub fn create_token_copy_except(cx: &mut Ctx, src: ObjId, owner: PlayerId, exc: CopyExc) -> R<SmallVec<[ObjId; 2]>> {
    let (card, face) = (cx.s.obj(src).card, cx.s.obj(src).face);
    // A permanent that left the battlefield: its copiable values as it last existed (the old
    // object keeps the card it was showing; its copy effect is in the snapshot).
    let mut ce = match crate::lki::of(cx.s, src) {
        Some(l) => l.copy.clone(),
        None => cx.s.copy_effects.iter().find(|c| c.obj == src).cloned(),
    };
    // The exceptions override the copied values (707.9b; a copied 1/1 exception or CDA is
    // replaced too, 707.9d). `printed` is the token's "own card": the card it copies when the
    // source isn't itself a copy (observation: `copy_of_printed` hides `printed == card`).
    if exc != CopyExc::default() {
        let mut e = ce.unwrap_or(CopyEffect { obj: src, printed: card, name: None, add_types: TypeSet::default(), add_subtypes: SmallVec::new(), kws: KwSet::default(), set_pt: None, transformed: false, add_super: 0 });
        if exc.set_pt.is_some() {
            e.set_pt = exc.set_pt;
        }
        e.add_super |= exc.add_super;
        ce = Some(e);
    }
    let mut ids: SmallVec<[ObjId; 2]> = SmallVec::new();
    let enchant = cx.db.get(card).front().enchant.clone();
    for _ in 0..tokens_at_once(cx, owner, 1)? {
        // A copy of an Aura entering without being cast (CR 303.4f): its controller chooses what
        // it enchants as it enters; with nothing legal the token isn't created (303.4g).
        let mut attach = None;
        if let Some(f) = &enchant {
            let fcx = FilterCtx { you: owner, source: Some(src), x: 0, rem: &[] };
            let opts: Vec<Opt> = cx.s.battlefield.iter().copied().filter(|&o| obj_matches(cx.s, cx.db, f, o, fcx)).map(Opt::Card).collect();
            if opts.is_empty() {
                continue;
            }
            let Opt::Card(to) = cx.choose(Decision { player: owner, kind: DecisionKind::ChooseCard, options: opts, subject: Some(src) })? else { unreachable!() };
            attach = Some(to);
        }
        let nid = cx.s.new_token(card, owner);
        cx.s.obj_mut(nid).attached_to = attach;
        if exc.flags != 0 {
            cx.s.cast_mut(nid).flags |= exc.flags;
        }
        cx.s.obj_mut(nid).face = face;
        // A copy of a Room enters with neither door unlocked (designations aren't copiable
        // values; neither half was cast, CR 709.5d).
        if let Some(r) = crate::ext5::room_of(cx.db, cx.s.obj(nid).card) {
            cx.s.copy_effects.retain(|c| c.obj != nid);
            cx.s.obj_mut(nid).card = r[0];
            crate::ext5::set_room_state(cx.s, cx.db, nid, 0);
        }
        if let Some(ce) = ce.clone() {
            // A copy of a transformed permanent copies its back face, with mana value 0 (712.8e).
            cx.s.copy_effects.push(CopyEffect { obj: nid, transformed: false, ..ce });
        }
        as_enters(cx, nid)?;
        ids.push(nid);
    }
    // Created together: placed together (one event, as in `move_batch`).
    for &nid in &ids {
        cx.s.place(nid, false);
    }
    for &nid in &ids {
        fire(cx.s, cx.db, &TrigEvent::ZoneChange { old: nid, new: Some(nid), from: Zone::Stack, to: Zone::Battlefield });
    }
    crate::ext5::tokens_entered(cx, &ids);
    Ok(ids)
}

/// Create an Aura token attached to `to` (CR 111.1, 303.4: e.g. a Role token "attached to that
/// creature"), with ETB replacements and triggers. Returns every token created (token
/// doublers, 614.1a).
pub fn create_token_attached(cx: &mut Ctx, card: CardId, owner: PlayerId, to: ObjId) -> R<SmallVec<[ObjId; 2]>> {
    let mut ids: SmallVec<[ObjId; 2]> = SmallVec::new();
    for _ in 0..tokens_at_once(cx, owner, 1)? {
        let nid = cx.s.new_token(card, owner);
        cx.s.obj_mut(nid).attached_to = Some(to);
        as_enters(cx, nid)?;
        ids.push(nid);
    }
    // Created together: placed together (one event, as in `move_batch`).
    for &nid in &ids {
        cx.s.place(nid, false);
    }
    for &nid in &ids {
        fire(cx.s, cx.db, &TrigEvent::ZoneChange { old: nid, new: Some(nid), from: Zone::Stack, to: Zone::Battlefield });
    }
    crate::ext5::tokens_entered(cx, &ids);
    Ok(ids)
}

/// Most tokens one event may create (`create_tokens_with`): every object is materialized, so an
/// exponential token count (doublers, "X tokens" with a huge X) fails loudly here instead of
/// exhausting memory. Far above any normal game.
pub const MAX_TOKENS_AT_ONCE: i32 = 1024;

/// How many tokens are created when `n` would be (token doublers, `ext3::token_count`), failing
/// loudly past `MAX_TOKENS_AT_ONCE`.
fn tokens_at_once(cx: &Ctx, owner: PlayerId, n: i32) -> R<i32> {
    let n = crate::ext3::token_count(cx.s, cx.db, owner, n);
    if n > MAX_TOKENS_AT_ONCE {
        return cx.unsupported(format!("creating {n} tokens at once (more than {MAX_TOKENS_AT_ONCE})"));
    }
    Ok(n)
}

/// Create tokens (CR 111.1): they enter the battlefield, with ETB replacements and triggers.
pub fn create_tokens(cx: &mut Ctx, card: CardId, owner: PlayerId, n: i32, tapped: bool) -> R<SmallVec<[ObjId; 2]>> {
    create_tokens_with(cx, card, owner, n, tapped, Default::default())
}

/// Create tokens carrying casting designations from the start (offspring/mobilize tokens).
pub fn create_tokens_with(cx: &mut Ctx, card: CardId, owner: PlayerId, n: i32, tapped: bool, cast: crate::altcast::CastInfo) -> R<SmallVec<[ObjId; 2]>> {
    let mut ids = SmallVec::new();
    // Token-doubling replacements (Elspeth, Storm Slayer; CR 614.1a).
    let n = tokens_at_once(cx, owner, n)?;
    for _ in 0..n {
        let nid = cx.s.new_token(card, owner);
        cx.s.obj_mut(nid).tapped = tapped;
        if cast != Default::default() {
            *cx.s.cast_mut(nid) = cast;
        }
        as_enters(cx, nid)?;
        ids.push(nid);
    }
    // Created together: placed together (one event, as in `move_batch`).
    for &nid in &ids {
        cx.s.place(nid, false);
    }
    for &nid in &ids {
        fire(cx.s, cx.db, &TrigEvent::ZoneChange { old: nid, new: Some(nid), from: Zone::Stack, to: Zone::Battlefield });
    }
    crate::ext5::tokens_entered(cx, &ids);
    Ok(ids)
}

/// Replacement effects that modify how a permanent enters (CR 614.1c–d, 614.12): its own
/// "enters tapped"/"as enters"/"enters with" abilities (`own_enters`, one block in printed
/// order) and other permanents' "[objects] enter tapped / with counters" (`other_enters`).
///
/// CR 616.1: the affected object's controller chooses which applicable effect applies first;
/// a copy effect is chosen first (616.1c); after each, the remaining ones are checked again on
/// the permanent as it would now exist (616.1f, 614.12). The permanent's own "enters with" is a
/// static ability's replacement effect, not a self-replacement effect (614.15), so it doesn't
/// come first by rule: an impending Overlord under Authority of the Consuls may enter tapped
/// (Authority first) or untapped (time counters first: no longer a creature)
/// (Spelunking ruling 2023-11-10). The choice (`DecisionKind::ChooseReplacement`, options the
/// effects' sources) is offered only when the orders can lead to different results
/// (`enter_order_matters`); otherwise the effects apply in the default order (its own first,
/// then the others in battlefield order), as the orders are interchangeable.
fn as_enters(cx: &mut Ctx, nid: ObjId) -> R<()> {
    let card = cx.s.obj(nid).card;
    let face = cx.db.get(card).front();
    if face.enchant.is_some() && cx.s.obj(nid).attached_to.is_none() {
        // CR 303.4f: the player would choose what it enchants as it enters.
        return cx.unsupported("Aura entering the battlefield without being cast (303.4f)");
    }
    let own = own_mods(cx, nid);
    let mut own_pending = has_own_enters(cx, nid, &own);
    // 616.1c: an "enter as a copy" effect is chosen first; the copied card's own "enters"
    // replacements apply as part of it (`Effect::Clone`).
    if own.iter().any(|&a| matches!(cx.db.ability(a).effect, Effect::Clone { .. })) {
        own_enters(cx, nid)?;
        own_pending = false;
    }
    let mut done: SmallVec<[(ObjId, AbilityId); 2]> = SmallVec::new();
    loop {
        let others = other_enters(cx.s, cx.db, nid, &done);
        let n = others.len() + own_pending as usize;
        if n == 0 {
            break;
        }
        // Candidates: `None` = its own block, `Some(x)` = another permanent's effect.
        let cands: SmallVec<[Option<(ObjId, AbilityId)>; 3]> = own_pending.then_some(None).into_iter().chain(others.iter().map(|&x| Some(x))).collect();
        let pick = if n > 1 && enter_order_matters(cx, nid, &cands, own_pending, &done)? {
            let srcs: SmallVec<[ObjId; 3]> = cands.iter().map(|c| c.map_or(nid, |x| x.0)).collect();
            if (1..srcs.len()).any(|i| srcs[..i].contains(&srcs[i])) {
                return cx.unsupported("ordering two enters replacement effects of one source (CR 616.1e)");
            }
            // One option per effect, by its source; identical other sources are one option.
            let items: Vec<((bool, crate::query::ObjSig), Opt)> = srcs.iter().map(|&o| ((o != nid, crate::query::obj_sig(cx.s, cx.db, o)), Opt::Card(o))).collect();
            let player = cx.s.obj(nid).controller;
            let Some(Opt::Card(o)) = cx.choose_set(player, DecisionKind::ChooseReplacement, items, 1, 1, Some(nid))?.into_iter().next() else { unreachable!("a pick of 1") };
            cands[srcs.iter().position(|&x| x == o).expect("chosen source")]
        } else {
            cands[0]
        };
        match pick {
            None => {
                own_enters(cx, nid)?;
                own_pending = false;
            }
            Some(x) => {
                apply_other_enters(cx, nid, x)?;
                done.push(x);
            }
        }
    }
    Ok(())
}

/// The entering permanent's own "enters" replacement abilities (`Modify` replacements without
/// active zones), unless it loses its rules text (Zhao: nonbasic lands are Mountains).
fn own_mods(cx: &Ctx, nid: ObjId) -> SmallVec<[AbilityId; 2]> {
    let face = cx.db.get(cx.s.obj(nid).card).front();
    let own_lost = cx.db.mr.lands_become.is_some() && crate::ext4::rules_text_lost(cx.s, cx.db, nid);
    face.replacements
        .iter()
        .filter_map(|r| match (&r.action, r.zones) {
            (ReplAction::Modify(a), None) if r.dest.has(Zone::Battlefield) && !own_lost => Some(*a),
            _ => None,
        })
        .collect()
}

/// Does `own_enters` have anything to apply?
fn has_own_enters(cx: &Ctx, nid: ObjId, own: &[AbilityId]) -> bool {
    let card = cx.s.obj(nid).card;
    let face = cx.db.get(card).front();
    !own.is_empty()
        || crate::query::final_chapter(cx.db, card) > 0
        || (face.loyalty.is_some() && face.types.has(CardType::Planeswalker))
        || !face.etb_counters.is_empty()
        || !face.extra.etb_counters_expr.is_empty()
        || !face.extra.etb_counters_x.is_empty()
        || (cx.s.cast_of(nid).has(crate::altcast::flag::IMPENDING) && face.alt.impending.is_some())
}

/// Other permanents' "enters" replacement effects that apply to `nid` as it would now exist
/// (CR 614.12), in battlefield order, except those already applied to this event (`done`,
/// 614.5). Only permanents already on the battlefield: objects entering in the same event (in
/// limbo until `move_batch` / token creation places them all) don't apply theirs to each other
/// (Curator Beastie ruling 2024-09-20).
fn other_enters(s: &State, db: &CardDb, nid: ObjId, done: &[(ObjId, AbilityId)]) -> SmallVec<[(ObjId, AbilityId); 2]> {
    let mut mods: SmallVec<[(ObjId, AbilityId); 2]> = SmallVec::new();
    for &src in &s.battlefield {
        let so = s.obj(src);
        if crate::query::may_lose_abilities(s, db) && crate::query::lost_all_abilities(s, db, src) {
            continue;
        }
        for r in &db.get(so.card).front().replacements {
            if let (ReplAction::Modify(a), Some(zs)) = (&r.action, r.zones) {
                if zs.has(Zone::Battlefield) && r.dest.has(Zone::Battlefield) && !done.contains(&(src, *a)) && obj_matches(s, db, &r.valid, nid, FilterCtx { you: so.controller, source: Some(src), x: 0, rem: &[] }) {
                    mods.push((src, *a));
                }
            }
        }
    }
    mods
}

fn apply_other_enters(cx: &mut Ctx, nid: ObjId, (src, a): (ObjId, AbilityId)) -> R<()> {
    let you = cx.s.obj(src).controller;
    let mut ex = crate::resolve::Exec::new(you, src, cx.s.obj(src).card);
    ex.replaced = Some(nid);
    crate::resolve::run_chain(cx, &mut ex, a)
}

/// Apply `first`, then the rest in the default order (its own block first, then the others
/// in battlefield order, each checked again, CR 616.1f).
fn complete_enters(cx: &mut Ctx, nid: ObjId, first: Option<(ObjId, AbilityId)>, mut own_pending: bool, done: &[(ObjId, AbilityId)]) -> R<()> {
    let mut done: SmallVec<[(ObjId, AbilityId); 2]> = done.iter().copied().collect();
    match first {
        None => {
            own_enters(cx, nid)?;
            own_pending = false;
        }
        Some(x) => {
            apply_other_enters(cx, nid, x)?;
            done.push(x);
        }
    }
    loop {
        if own_pending {
            own_enters(cx, nid)?;
            own_pending = false;
            continue;
        }
        let Some(&x) = other_enters(cx.s, cx.db, nid, &done).first() else { return Ok(()) };
        apply_other_enters(cx, nid, x)?;
        done.push(x);
    }
}

/// What applying the enters replacements left: the permanent (tapped, counters,
/// characteristics, controller, attachment) and the players' life and zone sizes (costs paid as
/// it enters, CR 614.12a/614.13).
fn enter_summary(s: &State, db: &CardDb, nid: ObjId) -> String {
    let o = s.obj(nid);
    let mut ctr: Vec<String> = o.counters.iter().filter(|c| c.1 != 0).map(|c| format!("{c:?}")).collect();
    ctr.sort();
    let players: Vec<(i32, usize, usize, usize)> = s.players.iter().map(|p| (p.life, p.hand.len(), p.library.len(), p.graveyard.len())).collect();
    format!("{}|{ctr:?}|{:?}|{:?}|{:?}|{players:?}|{}|{}", o.tapped, crate::query::chars(s, db, nid), o.controller, o.attached_to, s.battlefield.len(), s.exile.len())
}

/// Can the order of the applicable enters replacements `cands` change the result? For each
/// one applied first (then the rest in the default order), every answer to the choices met on
/// the way is explored on a scratch copy; the orders are interchangeable iff every first pick
/// reaches the same set of results. Too many branches: assume it matters (the controller is
/// asked, as the CR says).
fn enter_order_matters(cx: &mut Ctx, nid: ObjId, cands: &[Option<(ObjId, AbilityId)>], own_pending: bool, done: &[(ObjId, AbilityId)]) -> R<bool> {
    // Enough for an "as enters, choose a creature type" (among the chooser's own types, a known
    // approximation) under each candidate; runs are scratch copies of the state, and this only
    // happens when two effects apply at once.
    const MAX_RUNS: usize = 512;
    let mut first_set: Option<Vec<String>> = None;
    let mut runs = 0usize;
    for &c in cands {
        let mut out: Vec<String> = vec![];
        let mut tapes: Vec<Vec<u16>> = vec![vec![]];
        while let Some(tape) = tapes.pop() {
            runs += 1;
            if runs > MAX_RUNS {
                return Ok(true);
            }
            let mut st = cx.s.clone();
            let mut sc = Ctx::scripted(&mut st, cx.db, &tape);
            match complete_enters(&mut sc, nid, c, own_pending && c.is_some(), done) {
                Ok(()) => out.push(enter_summary(&st, cx.db, nid)),
                Err(Halt::Need(d)) => {
                    for i in 0..d.options.len() {
                        let mut t = tape.clone();
                        t.push(i as u16);
                        tapes.push(t);
                    }
                }
                // The real application meets the same problem and reports it.
                Err(_) => return Ok(false),
            }
        }
        out.sort();
        out.dedup();
        match &first_set {
            None => first_set = Some(out),
            Some(f) if *f != out => return Ok(true),
            Some(_) => {}
        }
    }
    Ok(false)
}

/// The entering permanent's own "enters" replacement effects (CR 614.1c), in printed order.
fn own_enters(cx: &mut Ctx, nid: ObjId) -> R<()> {
    let card = cx.s.obj(nid).card;
    let face = cx.db.get(card).front();
    // CR 714.3a: a Saga enters with a lore counter.
    if crate::query::final_chapter(cx.db, card) > 0 {
        let lore = cx.db.counter_names.iter().position(|n| n == "LORE").expect("LORE interned by compile");
        crate::ext6::put_counters(cx.s, cx.db, nid, CounterKind::Named(lore as u16), 1);
    }
    // CR 306.5b: a planeswalker enters with loyalty counters equal to its printed loyalty.
    if let (Some(l), true) = (face.loyalty, face.types.has(CardType::Planeswalker)) {
        crate::ext6::put_counters(cx.s, cx.db, nid, CounterKind::Loyalty, l);
    }
    for &(k, n) in &face.etb_counters {
        crate::ext6::put_counters(cx.s, cx.db, nid, k, n);
    }
    if !face.extra.etb_counters_expr.is_empty() {
        let you = cx.s.obj(nid).controller;
        for (k, e) in &face.extra.etb_counters_expr {
            let ecx = crate::expr::EvalCtx { s: cx.s, db: cx.db, you, source: nid, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &[], imprinted: &[], base_only: false };
            let n = crate::expr::eval(e, &ecx).max(0);
            if n > 0 {
                crate::ext6::put_counters(cx.s, cx.db, nid, *k, n);
            }
        }
    }
    if !face.extra.etb_counters_x.is_empty() {
        let x = cx.s.ext.x_paid.iter().find(|x| x.0 == nid).map(|x| x.1).unwrap_or(0);
        for &k in &face.extra.etb_counters_x {
            crate::ext6::put_counters(cx.s, cx.db, nid, k, x);
        }
    }
    crate::altcast::as_enters(cx.s, cx.db, nid);
    // Its own "as enters" / "enter as a copy" replacements, in printed order.
    for a in own_mods(cx, nid) {
        let you = cx.s.obj(nid).controller;
        let mut ex = crate::resolve::Exec::new(you, nid, cx.s.obj(nid).card);
        ex.replaced = Some(nid);
        crate::resolve::run_chain(cx, &mut ex, a)?;
    }
    Ok(())
}
