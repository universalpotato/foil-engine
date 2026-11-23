//! Combat (CR 506–511): declare attackers/blockers, first strike, damage assignment.

use crate::basics::*;
use crate::carddb::*;
use crate::engine::*;
use crate::query::*;
use crate::state::*;
use smallvec::SmallVec;

pub fn can_attack(s: &State, db: &CardDb, o: ObjId) -> bool {
    crate::query::frozen(s, db, || can_attack_frozen(s, db, o))
}

fn can_attack_frozen(s: &State, db: &CardDb, o: ObjId) -> bool {
    let ob = s.obj(o);
    let c = chars(s, db, o);
    c.types.has(CardType::Creature) && !ob.tapped && !c.kws.has(Kw::Defender) && (!s.is_sick(o) || c.kws.has(Kw::Haste)) && !cant_attack_by_effect(s, db, o)
}

/// "Can't attack" from an effect or rule other than being tapped, summoning sickness (CR 302.6)
/// and defender (702.3b), which `can_attack` checks and the observation shows on their own (CR
/// 508.1c). None is implemented: Forge `CantAttack` statics ("can't attack", Pacifism's "can't
/// attack or block") are refused when cards compile ("static CantAttack"), so no playable card
/// makes one; a test fails once Pacifism compiles, so the check lands here (and in the
/// observation's `ObjView::cant_attack`) with it.
pub fn cant_attack_by_effect(_s: &State, _db: &CardDb, _o: ObjId) -> bool {
    false
}

/// "Can't block" at all (CR 509.1b), whatever the attacker: a `CantBlock` static
/// (`static_ext::cant_block`: Bloodghast) or a `CantBlockBy` static / effect object whose attacker
/// filter matches every creature (`query::cant_block_any`). Restrictions that depend on the attacker
/// (flying / reach, "can't be blocked by creatures with power 2 or less") are not included; each
/// makes `can_block` false only for its attackers.
pub fn cant_block_at_all(s: &State, db: &CardDb, b: ObjId) -> bool {
    crate::static_ext::cant_block(s, db, b) || crate::query::cant_block_any(s, db, b)
}

/// CR 508.1: the active player declares attackers — a set of (attacker, defender) pairs,
/// chosen in canonical order. Defenders are the defending player and the planeswalkers they
/// control (508.1b). Attackers are picked in increasing (signature, id) order; among
/// interchangeable attackers (equal signature) the defender index may not decrease, so every
/// attack assignment is reachable exactly once.
pub fn declare_attackers(cx: &mut Ctx) -> R<()> {
    let p = cx.s.turn.active;
    let def = p.opp();
    // one memo scope for the whole scan (each `can_attack` outside a scope would compare the
    // state with the memo's snapshot: O(state) per creature)
    let (s, db) = (&*cx.s, cx.db);
    let mut cands: Vec<ObjId> = crate::query::frozen(s, db, || s.battlefield.iter().copied().filter(|&o| s.obj(o).controller == p && can_attack(s, db, o)).collect());
    let mut keyed: Vec<(crate::query::ObjSig, ObjId)> = obj_sigs(cx.s, cx.db, &cands).into_iter().zip(cands.iter().copied()).collect();
    keyed.sort_unstable();
    cands = keyed.iter().map(|k| k.1).collect();
    // Creatures that must attack if able (CR 508.1d) come first and are declared first; they
    // aren't interchangeable with otherwise identical creatures (`ext4`).
    let n_must = crate::ext4::must_attack_first(cx.s, cx.db, &mut cands);
    // Signatures don't change while attackers are declared (nothing below edits what `obj_sig`
    // reads). `run_end[j]`: the first index after j with another signature. A candidate whose
    // signature equals its predecessor's adds no option (its (signature, defender) pairs were
    // all offered or excluded by the predecessor), so each scan visits one candidate per run of
    // equal signatures: O(distinct signatures) per choice instead of O(candidates).
    let sigs = obj_sigs(cx.s, cx.db, &cands);
    let mut run_end = vec![cands.len(); cands.len()];
    for j in (0..cands.len().saturating_sub(1)).rev() {
        run_end[j] = if sigs[j + 1] != sigs[j] { j + 1 } else { run_end[j + 1] };
    }
    // Defender 0 is the player; then planeswalkers, in canonical order.
    let mut pws: Vec<ObjId> = cx.s.battlefield.iter().copied().filter(|&o| cx.s.obj(o).controller == def && chars(cx.s, cx.db, o).types.has(CardType::Planeswalker)).collect();
    crate::query::sort_by_sig(cx.s, cx.db, &mut pws);
    let defender_opt = |a: ObjId, d: usize| if d == 0 { Opt::Attack(a, def) } else { Opt::AttackPw(a, pws[d - 1]) };
    let mut declared: SmallVec<[(ObjId, usize); 8]> = SmallVec::new();
    let mut next = 0usize;
    // (signature of the last pick, its defender index)
    let mut last: Option<(crate::query::ObjSig, usize)> = None;
    loop {
        let mut opts = vec![Opt::Done];
        let mut picks: Vec<(usize, usize)> = vec![(usize::MAX, 0)];
        let mut seen: SmallVec<[(crate::query::ObjSig, usize); 16]> = SmallVec::new();
        if next < n_must {
            opts.remove(0); // no "Done" while a creature that must attack hasn't been declared
            picks.remove(0);
        }
        let mut j = next;
        while j < cands.len() {
            let a = cands[j];
            let sig = sigs[j];
            for d in 0..=pws.len() {
                if seen.contains(&(sig, d)) {
                    continue;
                }
                if let Some((ls, ld)) = last {
                    if ls == sig && d < ld {
                        continue;
                    }
                }
                seen.push((sig, d));
                opts.push(defender_opt(a, d));
                picks.push((j, d));
            }
            if next < n_must {
                break; // a creature that must attack is declared before any other
            }
            j = run_end[j];
        }
        if opts.is_empty() || (opts.len() == 1 && opts[0] == Opt::Done) {
            break;
        }
        let c = cx.choose(Decision { player: p, kind: DecisionKind::DeclareAttackers, options: opts.clone(), subject: None })?;
        if c == Opt::Done {
            break;
        }
        let (j, d) = picks[cx.offered_ix(c, &opts)];
        cx.swap_alt(c, j, &mut cands, |&o| defender_opt(o, d) == c)?;
        declared.push((cands[j], d));
        cx.s.combat.declaring.push(cands[j]);
        last = Some((sigs[j], d));
        next = j + 1;
    }
    cx.s.combat.declaring.clear();
    for &(o, d) in &declared {
        if !has_kw(cx.s, cx.db, o, Kw::Vigilance) {
            cx.s.obj_mut(o).tapped = true;
        }
        cx.s.combat.attackers.push((o, def));
        if d > 0 {
            cx.s.combat.attacking_pw.push((o, pws[d - 1]));
        }
        let card = cx.s.obj(o).card;
        cx.s.emit(Event::Attack { attacker: card, obj: o, defender: def });
        if d > 0 {
            cx.s.emit(Event::AttackPlaneswalker { attacker: card, obj: o, planeswalker: pws[d - 1] });
        }
    }
    for &(o, _) in &declared {
        crate::triggers::fire(cx.s, cx.db, &crate::triggers::TrigEvent::Attacks { attacker: o });
    }
    crate::ext4::note_attackers(cx.s, declared.len());
    if !declared.is_empty() {
        crate::triggers::fire(cx.s, cx.db, &crate::triggers::TrigEvent::Ext(crate::trig_ext::ExtEvent::AttackersDeclared { player: p }));
    }
    Ok(())
}

/// What an unblocked attacker deals combat damage to: the defending player, or the planeswalker
/// it attacks if that is still a planeswalker on the battlefield (CR 506.4, 506.4c: otherwise
/// none).
/// CR 508.4: creature `t` was put onto the battlefield attacking (or an effect states it's
/// attacking) without the effect saying what it attacks: its controller `you` chooses the
/// defending player or a planeswalker that player controls, for this creature on its own (Voice of
/// Victory ruling 2025-04-04: "You choose the player, planeswalker, or battle each Warrior token
/// is attacking. They don't all have to attack the same one"). Interchangeable
/// tokens get the full choice each (the choice is per creature, as the rules phrase it), unlike
/// the canonical order of a declaration (`declare_attackers`). It never "attacked" (no attack
/// triggers or events). Planeswalkers in canonical order.
pub fn enter_attacking(cx: &mut Ctx, you: PlayerId, t: ObjId) -> R<()> {
    let def = you.opp();
    let mut pws: Vec<ObjId> = cx.s.battlefield.iter().copied().filter(|&o| cx.s.obj(o).controller == def && chars(cx.s, cx.db, o).types.has(CardType::Planeswalker)).collect();
    crate::query::sort_by_sig(cx.s, cx.db, &mut pws);
    let mut opts = vec![Opt::Attack(t, def)];
    opts.extend(pws.iter().map(|&pw| Opt::AttackPw(t, pw)));
    let c = cx.choose(Decision { player: you, kind: DecisionKind::DeclareAttackers, options: opts, subject: Some(t) })?;
    cx.s.combat.attackers.push((t, def));
    if let Opt::AttackPw(_, pw) = c {
        cx.s.combat.attacking_pw.push((t, pw));
    }
    Ok(())
}

pub fn attack_target(s: &State, db: &CardDb, a: ObjId, def: PlayerId) -> Option<Target> {
    match s.combat.attacking_pw.iter().find(|x| x.0 == a) {
        None => Some(Target::Player(def)),
        Some(&(_, pw)) => (s.obj(pw).zone == Zone::Battlefield && chars(s, db, pw).types.has(CardType::Planeswalker)).then_some(Target::Obj(pw)),
    }
}

fn can_block(s: &State, db: &CardDb, b: ObjId, a: ObjId) -> bool {
    let ak = chars(s, db, a).kws;
    let bk = chars(s, db, b).kws;
    !(ak.has(Kw::Flying) && !bk.has(Kw::Flying) && !bk.has(Kw::Reach)) && !block_forbidden(s, db, a, b) && !crate::static_ext::cant_block(s, db, b)
}

/// CR 509.1: the defending player declares blockers: (blocker, attacker) pairs chosen in
/// canonical blocker order (so each block assignment is reachable once). A menace attacker
/// (702.111b) needs two or more blockers: its first blocker is offered only if a later blocker
/// could also block it, and that second blocker is then a forced follow-up choice.
pub fn declare_blockers(cx: &mut Ctx) -> R<()> {
    let p = cx.s.turn.active.opp();
    let attackers: SmallVec<[ObjId; 8]> = cx.s.combat.attackers.iter().map(|a| a.0).collect();
    let blockers: Vec<ObjId> = cx
        .s
        .battlefield
        .iter()
        .copied()
        .filter(|&o| {
            let ob = cx.s.obj(o);
            ob.controller == p && !ob.tapped && chars(cx.s, cx.db, o).types.has(CardType::Creature)
        })
        .collect();
    let mut keyed: Vec<(crate::query::ObjSig, ObjId)> = obj_sigs(cx.s, cx.db, &blockers).into_iter().zip(blockers.iter().copied()).collect();
    keyed.sort_unstable();
    // (signature, blocker) in canonical order; signatures don't change while blocks are declared
    let (sigs, mut blockers): (Vec<crate::query::ObjSig>, Vec<ObjId>) = keyed.into_iter().unzip();
    let menace: SmallVec<[bool; 8]> = attackers.iter().map(|&a| has_kw(cx.s, cx.db, a, Kw::Menace)).collect();
    // "Can't be blocked by more than one creature" (with menace: can't be blocked at all).
    let max_one: SmallVec<[bool; 8]> = attackers.iter().map(|&a| crate::ext3::max_one_blocker(cx.s, cx.db, a)).collect();
    let mut used = vec![false; blockers.len()];
    let mut next = 0usize;
    loop {
        // Options are enumerated in one memo scope (read-only; `chars` of each blocker and
        // attacker is computed once per scan instead of per (blocker, attacker) pair).
        let (s_, db_) = (&*cx.s, cx.db);
        let (pending_menace, opts, picks) = crate::query::frozen(s_, db_, || {
            // A menace attacker with exactly one blocker must get a second one now.
            let pending_menace = attackers.iter().enumerate().find(|(i, &a)| menace[*i] && s_.combat.blocks.iter().filter(|b| b.1 == a).count() == 1).map(|(_, &a)| a);
            let mut opts = vec![];
            let mut picks = vec![];
            if let Some(a) = pending_menace {
                let first = s_.combat.blocks.iter().rev().find(|b| b.1 == a).unwrap().0;
                let fpos = blockers.iter().position(|&b| b == first).unwrap();
                let mut seen: SmallVec<[crate::query::ObjSig; 8]> = SmallVec::new();
                for j in fpos + 1..blockers.len() {
                    let b = blockers[j];
                    if !used[j] && can_block(s_, db_, b, a) && !seen.contains(&sigs[j]) {
                        seen.push(sigs[j]);
                        opts.push(Opt::Block(b, a));
                        picks.push(j);
                    }
                }
            } else {
                opts.push(Opt::Done);
                picks.push(usize::MAX);
                // membership only (never iterated): (signature, attacker) pairs already offered
                let mut seen: std::collections::HashSet<(crate::query::ObjSig, ObjId)> = std::collections::HashSet::new();
                for j in next..blockers.len() {
                    let b = blockers[j];
                    if used[j] {
                        continue;
                    }
                    for (ai, &a) in attackers.iter().enumerate() {
                        if !can_block(s_, db_, b, a) || seen.contains(&(sigs[j], a)) {
                            continue;
                        }
                        if max_one[ai] && (menace[ai] || s_.combat.blocks.iter().any(|x| x.1 == a)) {
                            continue;
                        }
                        if menace[ai] && s_.combat.blocks.iter().all(|x| x.1 != a) {
                            let partner = (j + 1..blockers.len()).any(|k| !used[k] && can_block(s_, db_, blockers[k], a));
                            if !partner {
                                continue;
                            }
                        }
                        seen.insert((sigs[j], a));
                        opts.push(Opt::Block(b, a));
                        picks.push(j);
                    }
                }
            }
            (pending_menace, opts, picks)
        });
        if opts.len() == 1 && opts[0] == Opt::Done {
            break;
        }
        let c = cx.choose(Decision { player: p, kind: DecisionKind::DeclareBlockers, options: opts.clone(), subject: None })?;
        let j = picks[cx.offered_ix(c, &opts)];
        if let Opt::Block(_, a) = c {
            cx.swap_alt(c, j, &mut blockers, |&b| Opt::Block(b, a) == c)?;
        }
        match c {
            Opt::Block(b, a) => {
                cx.s.combat.blocks.push((b, a));
                used[j] = true;
                if pending_menace.is_none() {
                    next = j + 1;
                }
            }
            _ => break,
        }
    }
    let blocks = cx.s.combat.blocks.clone();
    for (b, a) in blocks {
        if !cx.s.combat.blocked.contains(&a) {
            cx.s.combat.blocked.push(a);
        }
        let (bc, ac) = (cx.s.obj(b).card, cx.s.obj(a).card);
        cx.s.emit(Event::Block { blocker: bc, attacker: ac });
    }
    Ok(())
}

pub fn any_first_strike(s: &State, db: &CardDb) -> bool {
    let fs = |o: ObjId| {
        let k = chars(s, db, o).kws;
        k.has(Kw::FirstStrike) || k.has(Kw::DoubleStrike)
    };
    s.combat.attackers.iter().any(|a| fs(a.0)) || s.combat.blocks.iter().any(|b| fs(b.0))
}

/// Does `o` deal damage in this combat damage step (CR 510.4 / 702.4 / 702.7)?
fn deals_now(s: &State, db: &CardDb, o: ObjId, first: bool) -> bool {
    let k = chars(s, db, o).kws;
    if first {
        k.has(Kw::FirstStrike) || k.has(Kw::DoubleStrike)
    } else {
        // Second step: creatures that had neither first nor double strike as the first-strike
        // step began, plus those that currently have double strike (CR 510.4).
        k.has(Kw::DoubleStrike) || !s.combat.first_strike_dealt.contains(&o)
    }
}

/// Combat damage step (CR 510): assign, then deal all damage simultaneously.
pub fn damage_step(cx: &mut Ctx, first: bool) -> R<()> {
    // Each blocker blocks one attacker (`declare_blockers`; "can block additional creatures",
    // CR 509.1a, is not supported: Forge's `CanBlockAmount`/`CanBlockAny` don't compile, scenarios
    // refuse it). So only that attacker assigns combat damage to it (CR 510.1c) and its lethal
    // damage (702.19b) is toughness minus damage already marked: no other creature's damage is
    // being assigned to it in this step. If that ever stops holding, fail loudly rather than
    // assign without the other creatures' damage (702.19b) or deal a blocker's damage to only one
    // of its attackers (510.1d).
    let blocks = &cx.s.combat.blocks;
    if let Some(i) = (1..blocks.len()).find(|&i| blocks[..i].iter().any(|y| y.0 == blocks[i].0)) {
        return cx.unsupported(format!("{} blocks several attackers (CR 509.1a, 510.1d, 702.19b)", cx.db.get(cx.s.obj(blocks[i].0).card).name));
    }
    let mut assigned: Vec<(ObjId, Target, i32)> = vec![];
    let attackers = cx.s.combat.attackers.clone();
    for &(a, def) in &attackers {
        if cx.s.obj(a).zone != Zone::Battlefield || !deals_now(cx.s, cx.db, a, first) {
            continue;
        }
        let c = chars(cx.s, cx.db, a);
        // Bounded (`num`): the arithmetic below stays within i32.
        let power = cx.s.num_check(c.power.max(0) as i64, "combat damage");
        if power == 0 {
            continue;
        }
        let blockers: SmallVec<[ObjId; 4]> = cx.s.combat.blocks.iter().filter(|b| b.1 == a).map(|b| b.0).collect();
        let dt = attack_target(cx.s, cx.db, a, def);
        if !cx.s.combat.blocked.contains(&a) {
            if let Some(t) = dt {
                assigned.push((a, t, power));
            }
        } else if blockers.is_empty() {
            if let (true, Some(t)) = (c.kws.has(Kw::Trample), dt) {
                assigned.push((a, t, power)); // 702.19e
            }
        } else {
            for (t, n) in assign_among_blockers(cx, a, dt, power, &blockers, c.kws)? {
                assigned.push((a, t, n));
            }
        }
    }
    for &(b, a) in &cx.s.combat.blocks.clone() {
        if cx.s.obj(b).zone != Zone::Battlefield || cx.s.obj(a).zone != Zone::Battlefield || !deals_now(cx.s, cx.db, b, first) {
            continue;
        }
        let power = chars(cx.s, cx.db, b).power.max(0);
        let power = cx.s.num_check(power as i64, "combat damage");
        if power > 0 {
            assigned.push((b, Target::Obj(a), power));
        }
    }
    if first {
        // Record every combatant that had first or double strike as this step began.
        let mut had: SmallVec<[ObjId; 4]> = SmallVec::new();
        let all: Vec<ObjId> = cx.s.combat.attackers.iter().map(|a| a.0).chain(cx.s.combat.blocks.iter().map(|b| b.0)).collect();
        for o in all {
            let k = chars(cx.s, cx.db, o).kws;
            if (k.has(Kw::FirstStrike) || k.has(Kw::DoubleStrike)) && !had.contains(&o) {
                had.push(o);
            }
        }
        cx.s.combat.first_strike_dealt = had;
    }
    // Lifelink etc. are handled per damage event inside deal_damage; all events here are
    // simultaneous because SBAs are only checked after this unit completes.
    // Combat damage is one simultaneous event: a creature that heals its other damage when
    // dealt damage (Wolverine) heals once, before all of it.
    for &(_, t, n) in &assigned {
        if let (Target::Obj(o), true) = (t, n > 0) {
            if crate::static_ext::heals_before_damage(cx.s, cx.db, o) {
                cx.s.obj_mut(o).damage = 0;
            }
        }
    }
    // A source dealing combat damage to several recipients gains life by lifelink once (CR
    // 119.9, 120.3f; 702.15e: one event per source).
    let mut several: SmallVec<[ObjId; 4]> = SmallVec::new();
    for (i, &(src, _, _)) in assigned.iter().enumerate() {
        if !several.contains(&src) && assigned[i + 1..].iter().any(|x| x.0 == src) {
            several.push(src);
        }
    }
    crate::resolve::hold_lifelink(cx.s, &several);
    // Per source, the total it dealt in this one event ("whenever ~ deals damage", CR 510.2).
    let mut dealt: SmallVec<[(ObjId, i32); 8]> = SmallVec::new();
    // One event: a shield counter prevents all of it to its permanent, once (CR 122.1c).
    let mut shielded = SmallVec::new();
    for (src, t, n) in assigned {
        let d = crate::resolve::deal_damage_in(cx, src, t, n, true, &mut shielded)?;
        match dealt.iter_mut().find(|x| x.0 == src) {
            Some(x) => x.1 = crate::num::add(x.1, d),
            None => dealt.push((src, d)),
        }
    }
    crate::resolve::release_lifelink(cx.s, cx.db);
    for (source, amount) in dealt {
        if amount > 0 {
            crate::triggers::fire(cx.s, cx.db, &crate::triggers::TrigEvent::Ext(crate::trig_ext::ExtEvent::DealtDamage { source, amount }));
        }
    }
    Ok(())
}

/// Amounts spelled out one by one at a combat damage assignment decision beyond its smallest
/// legal amount (see `damage_amounts`). 61: the bounded offer (`ASSIGN_SPELLED + 1` spelled
/// amounts, lethal, all the rest) is at most 64 options, the encoder's option window, so
/// "lethal" and "all the rest" stay pickable by an agent.
pub const ASSIGN_SPELLED: i32 = 61;

/// The amounts offered to one blocker whose legal amounts are `lo..=hi` (CR 510.1c, 702.19b).
/// Every amount while there are at most `ASSIGN_SPELLED + 1` of them (all normal combats: the
/// option set is exactly the per-integer one). Beyond that (an attacker with tens of millions of
/// power), one option per integer would be unbounded, so the offer is `lo..=lo +
/// ASSIGN_SPELLED`, the blocker's lethal damage (702.2c with deathtouch) and `hi` (all the rest):
/// O(1) options per blocker, O(blockers) decisions. Every "these blockers get lethal damage, the
/// rest goes to that one / the player" division stays reachable: earlier blockers take lethal or
/// 0, the one taking the excess takes `hi`; with trample each takes lethal and the last its
/// lethal (`lo`), the rest trampling over. Only exact excess amounts above the spelled range on a
/// blocker that isn't taking everything left are not offered (Forge and XMage ask for each amount
/// as a number instead: XMage `CombatGroup.blockerDamage` `getMultiAmountWithIndividualConstraints`
/// defaulting to lethal in order; Forge `PlayerController.assignCombatDamage`).
fn damage_amounts(lo: i32, hi: i32, lethal: i32) -> Vec<Opt> {
    debug_assert!(0 <= lo && lo <= hi);
    if hi - lo <= ASSIGN_SPELLED {
        return (lo..=hi).map(Opt::Number).collect();
    }
    let spelled_to = lo + ASSIGN_SPELLED;
    let mut v: Vec<Opt> = (lo..=spelled_to).map(Opt::Number).collect();
    if lethal > spelled_to && lethal < hi {
        v.push(Opt::Number(lethal));
    }
    v.push(Opt::Number(hi));
    v
}

/// Divide an attacker's damage among its blockers (CR 510.1c), respecting trample (702.19b–c)
/// and deathtouch (702.2c: 1 damage is lethal).
fn assign_among_blockers(cx: &mut Ctx, a: ObjId, def: Option<Target>, power: i32, blockers: &[ObjId], kws: KwSet) -> R<Vec<(Target, i32)>> {
    let p = cx.s.obj(a).controller;
    let lethal: Vec<i32> = blockers
        .iter()
        .map(|&b| {
            let c = chars(cx.s, cx.db, b);
            let need = crate::num::sub(c.toughness, cx.s.obj(b).damage).max(0);
            if kws.has(Kw::Deathtouch) {
                need.min(1)
            } else {
                need
            }
        })
        .collect();
    // Trample assigns excess to the player or planeswalker it attacks (CR 702.19c).
    let trample = kws.has(Kw::Trample) && def.is_some();
    let total_lethal: i32 = crate::num::sum(lethal.iter().copied());
    let mut out = vec![];
    let mut rem = power;
    if trample && power >= total_lethal {
        // CR 702.19b: excess may go to the player/planeswalker only once every blocker is
        // assigned lethal damage, but "the attacking creature's controller need not assign lethal
        // damage to all those blocking creatures but in that case can't assign any damage to the
        // player or planeswalker it's attacking" — any division among the blockers stays legal
        // (510.1c). Each blocker in turn gets 0..=rem; the last one also may leave the excess
        // for the player when every earlier blocker got lethal damage. Huge amounts are offered
        // in bounded form (`damage_amounts`).
        let mut all_lethal = true;
        for (i, &b) in blockers.iter().enumerate() {
            let last = i + 1 == blockers.len();
            let opts: Vec<Opt> = if !last {
                damage_amounts(0, rem, lethal[i])
            } else if all_lethal && rem >= lethal[i] {
                damage_amounts(lethal[i], rem, lethal[i])
            } else {
                vec![Opt::Number(rem)]
            };
            let Opt::Number(n) = cx.choose(Decision { player: p, kind: DecisionKind::AssignDamage, options: opts, subject: Some(b) })? else { unreachable!() };
            all_lethal &= n >= lethal[i];
            if n > 0 {
                out.push((Target::Obj(b), n));
            }
            rem -= n;
        }
        if let (true, Some(t)) = (rem > 0, def) {
            out.push((t, rem));
        }
    } else {
        for (i, &b) in blockers.iter().enumerate() {
            let n = if i + 1 == blockers.len() {
                rem
            } else {
                let opts = damage_amounts(0, rem, lethal[i]);
                let Opt::Number(n) = cx.choose(Decision { player: p, kind: DecisionKind::AssignDamage, options: opts, subject: Some(b) })? else { unreachable!() };
                n
            };
            if n > 0 {
                out.push((Target::Obj(b), n));
            }
            rem -= n;
        }
    }
    Ok(out)
}

/// One creature of `combat_board` (analysis only; see there).
#[derive(Clone, Debug, serde::Serialize)]
pub struct BoardCreature {
    pub id: ObjId,
    pub controller: PlayerId,
    pub power: i32,
    pub toughness: i32,
    /// `KwSet` bits (current characteristics, CR 109.3).
    pub kws: u32,
    pub tapped: bool,
    /// Picked as an attacker in the declaration in progress (CR 508.1a).
    pub declaring: bool,
    /// Attacking in the current combat (declared).
    pub attacking: bool,
    /// Can attack now (`can_attack`: untapped, not defender, not summoning sick unless haste).
    pub can_attack_now: bool,
    /// Would be able to attack on its controller's next turn once it untaps (CR 502.3): no
    /// defender (summoning sickness is gone by then, CR 302.6).
    pub can_attack_next: bool,
    /// Under a "can't block" static (`static_ext::cant_block`).
    pub cant_block: bool,
    /// "Can't be blocked by more than one creature" (`ext3::max_one_blocker`).
    pub max_one_blocker: bool,
}

/// God-view combat snapshot (labels and measurement
/// only, never an agent input: agents only ever see `observe(player)`). Every creature on the
/// battlefield of the view state (the paused mid-declaration state while a decision is pending)
/// with its current characteristics, plus `can_block` pairs (blocker, attacker) for every pair
/// of creatures with different controllers by the engine's own rule (`can_block`: flying /
/// reach, `CantBlockBy` statics, "can't block"; CR 509.1b–c). Tapped status is ignored there.
#[derive(Clone, Debug, serde::Serialize)]
pub struct CombatBoard {
    pub life: [i32; 2],
    pub active: PlayerId,
    pub turn: u32,
    pub creatures: Vec<BoardCreature>,
    pub can_block: Vec<(ObjId, ObjId)>,
}

pub fn combat_board(s: &State, db: &CardDb) -> CombatBoard {
    crate::query::frozen(s, db, || {
        let ids: Vec<ObjId> = s.battlefield.iter().copied().filter(|&o| chars(s, db, o).types.has(CardType::Creature)).collect();
        let creatures = ids
            .iter()
            .map(|&o| {
                let ob = s.obj(o);
                let c = chars(s, db, o);
                BoardCreature {
                    id: o,
                    controller: ob.controller,
                    power: c.power,
                    toughness: c.toughness,
                    kws: c.kws.0,
                    tapped: ob.tapped,
                    declaring: s.combat.declaring.contains(&o),
                    attacking: s.combat.attackers.iter().any(|a| a.0 == o),
                    can_attack_now: can_attack(s, db, o),
                    can_attack_next: !c.kws.has(Kw::Defender),
                    cant_block: crate::static_ext::cant_block(s, db, o),
                    max_one_blocker: crate::ext3::max_one_blocker(s, db, o),
                }
            })
            .collect();
        let mut pairs = vec![];
        for &b in &ids {
            for &a in &ids {
                if s.obj(a).controller != s.obj(b).controller && can_block(s, db, b, a) {
                    pairs.push((b, a));
                }
            }
        }
        CombatBoard { life: [s.player(PlayerId(0)).life, s.player(PlayerId(1)).life], active: s.turn.active, turn: s.turn.number, creatures, can_block: pairs }
    })
}
