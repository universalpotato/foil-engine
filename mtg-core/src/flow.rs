//! Game flow: setup, mulligans (CR 103), turn structure (CR 500–514), priority (CR 117).

use crate::basics::*;
use crate::carddb::CardDb;
use crate::engine::*;
use crate::state::*;
use crate::{cast, combat, sba};
use smallvec::SmallVec;

pub const HAND_SIZE: usize = 7;
pub const TURN_LIMIT: u32 = 400;

pub fn initial_state(db: &CardDb, decks: [&Deck; 2], seed: u64) -> State {
    initial_state_with(db, decks, seed, None)
}

/// `initial_state` with an optional starting player (CR 103.1; `Game::new_with_starting`). The
/// random draw is made either way, so the rest of the RNG stream (shuffles) doesn't depend on it.
pub fn initial_state_with(_db: &CardDb, decks: [&Deck; 2], seed: u64, starting: Option<PlayerId>) -> State {
    let mut rng = Rng::new(seed);
    let drawn = PlayerId(rng.below(2) as u8);
    let starting = starting.unwrap_or(drawn);
    let mk_player = || Player {
        life: 20,
        library: vec![],
        hand: vec![],
        graveyard: vec![],
        lands_played: 0,
        drew_from_empty: false,
        poison: 0,
        last_turn_started: 0,
        mulligans: 0,
        kept: false,
        sideboard: vec![],
    };
    let mut s = State {
        rng,
        stage: Stage::Mulligan,
        players: [mk_player(), mk_player()],
        objs: ObjVec::default(),
        battlefield: vec![],
        exile: vec![],
        stack: vec![],
        turn: Turn { number: 0, active: starting, step: Step::Untap, tba_done: false, priority: starting, passes: 0 },
        combat: Combat::default(),
        effects: vec![],
        effect_objs: vec![],
        pending_triggers: vec![],
        next_ts: 0,
        starting_player: starting,
        result: None,
        turn_stats: TurnStats::default(),
        cast_info: vec![],
        events: vec![],
        ext: Default::default(),
        hist: Default::default(),
        copy_effects: vec![],
        lineage: None,
        overflow: None,
        resolving: None,
        becoming: None,
        being_put: None,
    };
    for (pi, deck) in decks.iter().enumerate() {
        let p = PlayerId(pi as u8);
        // Shuffle card order before assigning ids so ids don't encode decklist position.
        let mut cards = deck.main.clone();
        s.rng.shuffle(&mut cards);
        for &card in &cards {
            let id = ObjId(s.objs.len() as u32);
            s.objs.push(Obj {
                card,
                owner: p,
                controller: p,
                zone: Zone::Library,
                tapped: false,
                ctrl_since: 0,
                damage: 0,
                deathtouched: false,
                counters: SmallVec::new(),
                ts: 0,
                token: false,
                known_to: 0,
                entered: 0,
                cast_by: None,
                face: 0,
                on_adventure: false,
                attached_to: None,
                prepared: false,
                cast_mana: 0,
            });
            s.player_mut(p).library.push(id);
        }
        shuffle_library(&mut s, p);
        for _ in 0..HAND_SIZE {
            draw(&mut s, p);
        }
    }
    // Cards outside the game (CR 400.11a: the sideboard), only in games where some card can
    // bring them in (cheap clone otherwise). Sideboard cards the engine can't play are left out
    // of the game (deck construction), so a wish never offers them.
    let wishes = !_db.lt.wish_cards.is_empty() && decks.iter().any(|d| d.main.iter().any(|c| _db.lt.wish_cards.contains(c)));
    if wishes {
        for (pi, deck) in decks.iter().enumerate() {
            let p = PlayerId(pi as u8);
            let mut side: Vec<CardId> = deck.side.iter().copied().filter(|&c| _db.get(c).playable()).collect();
            side.sort();
            for card in side {
                let id = ObjId(s.objs.len() as u32);
                s.objs.push(Obj {
                    card,
                    owner: p,
                    controller: p,
                    zone: Zone::Sideboard,
                    tapped: false,
                    ctrl_since: 0,
                    damage: 0,
                    deathtouched: false,
                    counters: SmallVec::new(),
                    ts: 0,
                    token: false,
                    known_to: 1 << p.0,
                    entered: 0,
                    cast_by: None,
                    face: 0,
                    on_adventure: false,
                    attached_to: None,
                    prepared: false,
                    cast_mana: 0,
                });
                s.player_mut(p).sideboard.push(id);
            }
        }
    }
    s
}

/// Shuffle (CR 701.24) and forget all knowledge of library order. Object ids are kept: agents
/// only ever see a library object's id together with its revealed identity (scry, search), and
/// initial ids are assigned after a shuffle, so ids carry no hidden information.
pub fn shuffle_library(s: &mut State, p: PlayerId) {
    let mut lib = std::mem::take(&mut s.player_mut(p).library);
    s.rng.shuffle(&mut lib);
    for &id in &lib {
        s.obj_mut(id).known_to = 0;
    }
    s.player_mut(p).library = lib;
}

pub fn draw(s: &mut State, p: PlayerId) -> Option<ObjId> {
    match s.library_top(p) {
        Some(top) => {
            let card = s.obj(top).card;
            // Whoever knew the top card sees it drawn: it keeps that knowledge in the hand
            // (perfect recall; the draw is public, CR 121.1 "put the top card of their library
            // into their hand", and the top card's identity was known to them; e.g. a card Braided
            // Quipu put on top, a revealed top card); `State::seen_into_hand`.
            let seen = s.obj(top).known_to;
            let nid = s.move_obj(top, Zone::Hand, false);
            s.seen_into_hand(nid, seen);
            s.emit(Event::Draw { player: p, card });
            Some(nid)
        }
        None => {
            s.player_mut(p).drew_from_empty = true;
            None
        }
    }
}

/// Draw `n` cards (CR 121.2), after replacement effects that modify how many (CR 614.11,
/// Quantum Riddler: "draw that many cards plus one instead").
pub fn draw_n(s: &mut State, db: &CardDb, p: PlayerId, n: i32) {
    let n = if n >= 1 { crate::num::add(n, crate::static_ext::extra_draws(s, db, p)) } else { n };
    for _ in 0..n.max(0) {
        match draw(s, p) {
            Some(c) => crate::ext4::fire_drawn(s, db, p, c),
            // CR 121.4: the library is empty and stays so during this draw; further draws change
            // nothing (and a huge `n` would just spin).
            None => break,
        }
    }
}

/// One unit of game progress (see `engine` docs).
pub fn unit(cx: &mut Ctx) -> R<()> {
    match cx.s.stage {
        Stage::Mulligan => mulligan_unit(cx),
        Stage::Playing => play_unit(cx),
        Stage::Over => Err(Halt::GameOver),
    }
}

/// London mulligan (CR 103.5): in turn order each undecided player keeps or mulligans; those
/// who mulligan shuffle their hand into their library, draw seven and put N cards on the bottom
/// (N = mulligans taken so far) — the bottoming is part of taking the mulligan (103.5, 2026-09-25
/// text), so the next declaration is made with the smaller hand; repeat until all have kept.
fn mulligan_unit(cx: &mut Ctx) -> R<()> {
    let order = [cx.s.starting_player, cx.s.starting_player.opp()];
    let mut mulled = [false; 2];
    for p in order {
        if cx.s.player(p).kept {
            continue;
        }
        if cx.s.player(p).mulligans as usize >= HAND_SIZE {
            cx.s.player_mut(p).kept = true;
            continue;
        }
        let c = cx.choose(Decision { player: p, kind: DecisionKind::Mulligan, options: vec![Opt::Keep, Opt::Mulligan], subject: None })?;
        if c == Opt::Keep {
            cx.s.player_mut(p).kept = true;
        } else {
            mulled[p.idx()] = true;
        }
    }
    for p in order {
        if mulled[p.idx()] {
            for h in cx.s.player(p).hand.clone() {
                cx.s.move_obj(h, Zone::Library, true);
            }
            shuffle_library(cx.s, p);
            for _ in 0..HAND_SIZE {
                draw(cx.s, p);
            }
            let m = cx.s.player(p).mulligans + 1;
            cx.s.player_mut(p).mulligans = m;
            cx.s.emit(Event::Mulligan { player: p, count: m });
            // ... then puts that many of the new hand on the bottom, in any order (103.5).
            let n = (m as usize).min(cx.s.player(p).hand.len());
            for c in cx.choose_set(p, DecisionKind::BottomCard, hand_items(cx, p), n, n, None)? {
                if let Opt::Card(o) = c {
                    cx.s.move_obj(o, Zone::Library, false);
                }
            }
        }
    }
    if cx.s.players.iter().all(|p| p.kept) {
        for p in order {
            let hand = cx.s.player(p).hand.len() as u8;
            cx.s.emit(Event::Kept { player: p, hand });
        }
        // CR 103.6: opening-hand actions (leylines), starting player first. Choosing which
        // copies is a set choice (identical cards are interchangeable).
        for p in order {
            let items: Vec<(crate::query::ObjSig, Opt)> = hand_items(cx, p).into_iter().filter(|(_, o)| matches!(o, Opt::Card(h) if cx.db.get(cx.s.obj(*h).card).front().opening_hand_battlefield)).collect();
            if items.is_empty() {
                continue;
            }
            let n = items.len();
            let chosen = cx.choose_set(p, DecisionKind::OptionalEffect, items, 0, n, None)?;
            let moves: Vec<crate::zones::Move> = chosen.into_iter().filter_map(|c| if let Opt::Card(h) = c { Some(crate::zones::Move { obj: h, to: Zone::Battlefield, to_top: false, tapped: false, controller: Some(p) }) } else { None }).collect();
            crate::zones::move_batch(cx, &moves)?;
        }
        cx.s.stage = Stage::Playing;
        begin_turn(cx.s, cx.s.starting_player, 1);
    }
    Ok(())
}

/// Hand cards as set-choice items keyed by `query::card_sig` (identical cards are one option
/// unless something refers to one of them).
pub fn hand_items(cx: &Ctx, p: PlayerId) -> Vec<(crate::query::ObjSig, Opt)> {
    crate::query::card_items(cx.s, cx.db, &cx.s.player(p).hand)
}

/// Options `Card(o)` for each distinct card in hand (`query::card_sig`).
pub fn dedup_hand(cx: &Ctx, p: PlayerId) -> Vec<Opt> {
    let hand = &cx.s.player(p).hand;
    let first = crate::query::first_of_each_card(cx.s, cx.db, hand);
    hand.iter().zip(first).filter(|x| x.1).map(|x| Opt::Card(*x.0)).collect()
}

fn begin_turn(s: &mut State, active: PlayerId, number: u32) {
    s.turn = Turn { number, active, step: Step::Untap, tba_done: false, priority: active, passes: 0 };
    s.player_mut(active).last_turn_started = number;
    for p in &mut s.players {
        p.lands_played = 0;
    }
    let taken = s.turn_stats.turns_taken;
    s.turn_stats = TurnStats { turns_taken: taken, ..Default::default() };
    s.turn_stats.turns_taken[active.idx()] += 1;
    s.ext.new_turn();
    // Casting info kept as last known information of objects that left (`State::move_obj_limbo`)
    // is dropped with the other per-turn snapshots (`lki::LkiState::new_turn`), except a warp
    // delayed trigger that hasn't triggered yet (warped in an end step: `altcast::end_step_begins`).
    if !s.cast_info.is_empty() {
        let objs = &s.objs;
        s.cast_info.retain(|e| objs[e.0.idx()].zone != Zone::Gone || e.1.has(crate::altcast::flag::WARP));
    }
    // "Until your next turn" effects end as that turn begins (CR 611.2b).
    s.effects.retain(|e| e.until_turn_of != Some(active));
    s.effect_objs.retain(|e| !(e.duration == crate::carddb::EffDuration::UntilYourNextTurn && e.controller == active));
    s.emit(Event::TurnBegan { turn: number, active });
}

fn has_priority(step: Step) -> bool {
    !matches!(step, Step::Untap | Step::Cleanup)
}

fn play_unit(cx: &mut Ctx) -> R<()> {
    if !cx.s.turn.tba_done {
        cx.s.turn.tba_done = true;
        cx.s.turn.priority = cx.s.turn.active;
        cx.s.turn.passes = 0;
        return turn_based_actions(cx);
    }
    crate::ext6::counter_watch(cx);
    if sba::check(cx)? {
        crate::ext5::cleanup_priority(cx.s, true);
        return Ok(());
    }
    if sba::put_triggers_on_stack(cx)? {
        crate::ext5::cleanup_priority(cx.s, true);
        return Ok(());
    }
    // CR 514.3a: players get priority in a cleanup step where something happened.
    if !has_priority(cx.s.turn.step) && !crate::ext5::cleanup_priority(cx.s, false) {
        end_step(cx);
        return Ok(());
    }
    priority(cx)
}

fn turn_based_actions(cx: &mut Ctx) -> R<()> {
    let active = cx.s.turn.active;
    cx.s.emit(Event::StepBegan { step: cx.s.turn.step });
    match cx.s.turn.step {
        Step::Untap => {
            // CR 502.3
            let mine: Vec<ObjId> = cx.s.battlefield.iter().copied().filter(|&o| cx.s.obj(o).controller == active).collect();
            for o in mine {
                let ob = cx.s.obj_mut(o);
                if ob.tapped && ob.counter(CounterKind::Stun) > 0 {
                    ob.add_counter(CounterKind::Stun, -1); // CR 122.1d
                } else {
                    ob.tapped = false;
                }
            }
            crate::static_ext::end_untapped_effects(cx.s);
        }
        Step::Draw => {
            // CR 103.8a: the starting player skips the draw on their first turn.
            if !(cx.s.turn.number == 1) {
                draw_n(cx.s, cx.db, active, 1);
            }
        }
        Step::Main1 => {
            // CR 714.3c: the active player puts a lore counter on each Saga they control with
            // one or more chapter abilities (not one that lost them).
            let sagas: Vec<ObjId> = cx.s.battlefield.iter().copied().filter(|&o| cx.s.obj(o).controller == active && crate::query::final_chapter_of(cx.s, cx.db, o) > 0).collect();
            for o in sagas {
                let before = crate::query::lore(cx.s, cx.db, o);
                let lore = cx.db.counter_names.iter().position(|n| n == "LORE").unwrap();
                crate::ext6::put_counters(cx.s, cx.db, o, CounterKind::Named(lore as u16), 1);
                crate::triggers::fire(cx.s, cx.db, &crate::triggers::TrigEvent::LoreAdded { obj: o, before, after: before + 1 });
            }
        }
        Step::DeclareAttackers => combat::declare_attackers(cx)?,
        Step::DeclareBlockers => combat::declare_blockers(cx)?,
        Step::FirstStrikeDamage => combat::damage_step(cx, true)?,
        Step::CombatDamage => combat::damage_step(cx, false)?,
        Step::Cleanup => cleanup(cx)?,
        _ => {}
    }
    let step = cx.s.turn.step;
    if step != Step::Untap && step != Step::Cleanup {
        crate::triggers::fire(cx.s, cx.db, &crate::triggers::TrigEvent::StepBegin { step });
        if step == Step::End {
            // "Until your next end step" effects end as that step begins.
            let active = cx.s.turn.active;
            cx.s.effect_objs.retain(|e| !(e.duration == crate::carddb::EffDuration::UntilYourNextEndStep && e.controller == active));
            crate::altcast::end_step_begins(cx.s, cx.db);
        }
    }
    Ok(())
}

/// CR 514.1–2: discard to hand size, remove damage, end "until end of turn" effects.
fn cleanup(cx: &mut Ctx) -> R<()> {
    let p = cx.s.turn.active;
    let excess = cx.s.player(p).hand.len().saturating_sub(HAND_SIZE);
    if excess > 0 {
        let chosen = cx.choose_set(p, DecisionKind::Discard, hand_items(cx, p), excess, excess, None)?;
        let mut moves = vec![];
        for c in chosen {
            if let Opt::Card(o) = c {
                let card = cx.s.obj(o).card;
                cx.s.emit(Event::Discard { player: p, card });
                moves.push(crate::zones::Move::new(o, Zone::Graveyard));
            }
        }
        crate::zones::move_batch(cx, &moves)?;
    }
    for &o in &cx.s.battlefield.clone() {
        cx.s.obj_mut(o).damage = 0;
        cx.s.obj_mut(o).deathtouched = false;
    }
    cx.s.effects.retain(|e| !e.until_eot);
    cx.s.ext.x7.player_hexproof = 0; // "you gain hexproof until end of turn" (`ext7`)
    // CR 514.2: "until end of turn" effects end; "until the end of your next turn" ones end at
    // the cleanup of the controller's next turn.
    let (turn, active) = (cx.s.turn.number, cx.s.turn.active);
    cx.s.effect_objs.retain(|e| !ends_at_cleanup(e, turn, active));
    Ok(())
}

/// Does this effect object end at the cleanup step of turn `turn` (active player `active`)? CR 514.2:
/// "until end of turn" / "this turn" effects end; "until the end of your next turn" ones at the
/// cleanup of the controller's next turn. (Resolved continuous effects: `ContEffect::until_eot`.)
/// Shared by `cleanup` and the observation's until-end-of-turn view (`observe::EotView`).
pub fn ends_at_cleanup(e: &EffectObj, turn: u32, active: PlayerId) -> bool {
    match e.duration {
        crate::carddb::EffDuration::EndOfTurn => true,
        crate::carddb::EffDuration::UntilTheEndOfYourNextTurn => e.controller == active && turn > e.created_turn,
        _ => false,
    }
}

/// The current phase and step end and the turn skips to its cleanup step (CR 723.1d, "end the
/// turn"); mana pools empty (500.4).
pub fn skip_to_cleanup(s: &mut State) {
    crate::ext9::empty_pools(s, None);
    s.ext.x6.rpool.clear();
    s.turn.step = Step::Cleanup;
    s.turn.tba_done = false;
    s.turn.passes = 0;
    s.turn.priority = s.turn.active;
}

/// All players passed in succession with an empty stack (or a step without priority ended).
fn end_step(cx: &mut Ctx) {
    let s = &mut *cx.s;
    // CR 500.4: mana empties from each player's mana pool at the end of each step and phase
    // (firebending mana only at the end of combat, 702.189a; `ext9`).
    crate::ext9::empty_pools(s, Some(s.turn.step));
    s.ext.x6.rpool.clear();
    if s.turn.step == Step::EndCombat {
        s.combat = Combat::default();
    }
    let next = match s.turn.step {
        // CR 508.8: no attackers → skip declare blockers and combat damage steps.
        Step::DeclareAttackers if s.combat.attackers.is_empty() => Some(Step::EndCombat),
        // CR 510.4: first-strike damage step only exists if a combatant has first/double strike.
        Step::DeclareBlockers if !combat::any_first_strike(s, cx.db) => Some(Step::CombatDamage),
        st => st.next(),
    };
    match next {
        Some(st) => {
            s.turn.step = st;
            s.turn.tba_done = false;
        }
        None => {
            let (mut a, n) = (s.turn.active.opp(), s.turn.number + 1);
            // CR 614.10: a skipped turn doesn't happen; the other player takes the next one.
            if s.ext.x6.skip_turns[a.idx()] > 0 {
                s.ext.x6.skip_turns[a.idx()] -= 1;
                a = a.opp();
            }
            if n > TURN_LIMIT {
                s.result = Some(GameResult::Draw);
                s.stage = Stage::Over;
                s.emit(Event::GameOver { result: GameResult::Draw });
                return;
            }
            crate::hist::turn_end(s, cx.db);
            begin_turn(s, a, n);
        }
    }
}

fn priority(cx: &mut Ctx) -> R<()> {
    let p = cx.s.turn.priority;
    let options = match cx.first_options(p) {
        Some(o) => o,
        None => cast::legal_actions(cx, p),
    };
    // A full-fidelity seat (the human) gets the agent form here too: mana abilities at priority
    // only where the agent mana filter offers them (`cast::mana_reasons`), not "tapping lands for
    // nothing" (while paying, any order stays).
    let c = cx.choose(Decision { player: p, kind: DecisionKind::Priority, options, subject: None })?;
    match c {
        Opt::Pass => {
            cx.s.turn.passes += 1;
            if cx.s.turn.passes >= 2 {
                cx.s.turn.passes = 0;
                if cx.s.stack.is_empty() {
                    // CR 514.3a: after priority in a cleanup step, another cleanup step.
                    if !crate::ext5::repeat_cleanup(cx.s) {
                        end_step(cx);
                    }
                } else {
                    crate::resolve::resolve_top(cx)?;
                    cx.s.turn.priority = cx.s.turn.active; // CR 117.3b
                }
            } else {
                cx.s.turn.priority = p.opp();
            }
        }
        Opt::PlayLand(o) => {
            cast::play_land(cx, p, o)?;
            cx.s.turn.passes = 0;
        }
        Opt::Cast(o) => {
            cast::cast_spell(cx, p, o, 0)?;
            cx.s.turn.passes = 0;
        }
        Opt::Flashback(o) => {
            cast::cast_spell_method(cx, p, o, 0, crate::altcast::Method::Flashback)?;
            cx.s.turn.passes = 0;
        }
        Opt::CastPrepared(o) => {
            cast::cast_prepared(cx, p, o)?;
            cx.s.turn.passes = 0;
        }
        Opt::CastFace(o, face) => {
            cast::cast_spell(cx, p, o, face)?;
            cx.s.turn.passes = 0;
        }
        Opt::Activate(o, a) => {
            cast::activate(cx, p, o, a)?;
            cx.s.turn.passes = 0;
        }
        Opt::Alt(o, m) => {
            crate::altcast::take_action(cx, p, o, m)?;
            cx.s.turn.passes = 0;
        }
        // A mana ability at priority (CR 605.3a): it resolves at once (605.3b) and the player
        // keeps priority (117.3c); it is an action, so passes start over (117.4).
        Opt::Mana(o, a, _) => {
            cast::activate_mana_ability(cx, p, o, a)?;
            cx.s.turn.passes = 0;
        }
        other => return cx.unsupported(format!("priority option {other:?}")),
    }
    Ok(())
}
