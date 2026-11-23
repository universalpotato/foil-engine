//! Public game history (encoding v7): what each player did on recent turns, kept in the state so
//! that the observation (and the stateless encoder) can show it.
//!
//! Honesty: every field is derived from *public* information only. Actions are recorded from
//! events that `observe::filter_event` shows to both players with their card (Cast, Activate,
//! Discard; CR 601.2a / 602.2a / 701.9b: casting, activating and discarding are announced or
//! revealed); draws and land plays are counted, never identified (another player's draw is
//! redacted); hand sizes, life totals, untapped lands, combat damage and creature deaths are
//! public game state. Leak tests: lockstep resampling of the opponent's hidden cards, and
//! `encode::tests`.
//!
//! Cost: fixed-size arrays inline in `State` (no allocation), so copying a state stays a
//! memcpy; updated from `State::emit` and three hooks (turn end, combat damage, combat death).
//!
//! The combat fields and `died` ids also serve as *auxiliary labels* (`aux.rs`): those labels
//! are about the future of the turn and are never inputs.

use crate::basics::*;
use crate::state::*;
use serde::{Deserialize, Serialize};

/// Recent actions kept per player.
pub const ACTS: usize = 8;
/// Turn records kept (ring by turn number).
pub const TURNS: usize = 4;
/// Creature deaths in combat recorded per turn (by object id; aux labels).
pub const DIED: usize = 12;

pub mod act {
    pub const NONE: u8 = 0;
    pub const CAST: u8 = 1;
    pub const ACTIVATE: u8 = 2;
    pub const DISCARD: u8 = 3;
}

/// One public action: a spell cast, a (non-mana) ability activated, a card discarded.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Act {
    pub turn: u32,
    pub card: u32,
    pub kind: u8,
    /// Taken during another player's turn (instant speed).
    pub off_turn: bool,
}

/// One player's public record of one turn (theirs or the opponent's).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerRec {
    pub spells: u8,
    pub activations: u8,
    pub draws: u8,
    pub lands: u8,
    pub discards: u8,
    /// Hand size and life as the turn began (before the untap step).
    pub hand_start: u8,
    pub life_start: i32,
    /// Untapped lands they controlled as the turn ended (mana left unused; 0 until it ends).
    pub untapped_end: u8,
    /// Combat damage dealt to this player, and creatures they controlled that died in a combat
    /// damage step (CR 510), this turn.
    pub combat_damage: i32,
    pub combat_deaths: u8,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRec {
    /// Turn number (0 = empty slot).
    pub turn: u32,
    pub active: u8,
    pub ended: bool,
    pub p: [PlayerRec; 2],
    /// Object ids (on the battlefield) of the creatures that died in combat damage steps.
    pub died: [u32; DIED],
    pub n_died: u8,
    /// More than `DIED` deaths: `died` is incomplete.
    pub died_overflow: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct History {
    /// Per player, ring of the last `ACTS` actions (`n_acts[p]` = total recorded).
    pub acts: [[Act; ACTS]; 2],
    pub n_acts: [u32; 2],
    pub turns: [TurnRec; TURNS],
}

impl History {
    /// Record of turn `t`, if still in the ring.
    pub fn turn(&self, t: u32) -> Option<&TurnRec> {
        let r = &self.turns[t as usize % TURNS];
        (t > 0 && r.turn == t).then_some(r)
    }
    fn turn_mut(&mut self, t: u32) -> Option<&mut TurnRec> {
        let r = &mut self.turns[t as usize % TURNS];
        (t > 0 && r.turn == t).then_some(r)
    }
    /// Player `p`'s actions, newest first.
    pub fn recent(&self, p: PlayerId) -> impl Iterator<Item = &Act> + '_ {
        let n = self.n_acts[p.idx()] as usize;
        (0..n.min(ACTS)).map(move |k| &self.acts[p.idx()][(n - 1 - k) % ACTS])
    }
    fn push(&mut self, p: PlayerId, a: Act) {
        let i = p.idx();
        self.acts[i][self.n_acts[i] as usize % ACTS] = a;
        self.n_acts[i] += 1;
    }
}

/// Update from an event as it is emitted (`State::emit`).
pub fn on_event(s: &mut State, e: &Event) {
    let (turn, active) = (s.turn.number, s.turn.active);
    if turn == 0 {
        return; // mulligans: opening hands are not history
    }
    let rec = |s: &mut State, p: PlayerId, f: &dyn Fn(&mut PlayerRec)| {
        if let Some(r) = s.hist.turn_mut(turn) {
            f(&mut r.p[p.idx()]);
        }
    };
    let act = |s: &mut State, p: PlayerId, card: CardId, kind: u8| s.hist.push(p, Act { turn, card: card.0, kind, off_turn: p != active });
    match *e {
        Event::TurnBegan { turn: t, active: a } => {
            let mut r = TurnRec { turn: t, active: a.0, ..Default::default() };
            for p in [PlayerId(0), PlayerId(1)] {
                let pl = s.player(p);
                r.p[p.idx()].hand_start = pl.hand.len().min(255) as u8;
                r.p[p.idx()].life_start = pl.life;
            }
            s.hist.turns[t as usize % TURNS] = r;
        }
        Event::Cast { player, card, .. } => {
            rec(s, player, &|r| r.spells = r.spells.saturating_add(1));
            act(s, player, card, act::CAST);
        }
        Event::Activate { player, card } => {
            rec(s, player, &|r| r.activations = r.activations.saturating_add(1));
            act(s, player, card, act::ACTIVATE);
        }
        Event::Discard { player, card } => {
            rec(s, player, &|r| r.discards = r.discards.saturating_add(1));
            act(s, player, card, act::DISCARD);
        }
        // Counted only: the card drawn is hidden from the other player.
        Event::Draw { player, .. } => rec(s, player, &|r| r.draws = r.draws.saturating_add(1)),
        Event::PlayLand { player, .. } => rec(s, player, &|r| r.lands = r.lands.saturating_add(1)),
        _ => {}
    }
}

/// The current turn ends (before the next one begins): untapped lands per player.
pub fn turn_end(s: &mut State, db: &crate::CardDb) {
    let t = s.turn.number;
    let mut n = [0u8; 2];
    for &o in &s.battlefield {
        let ob = s.obj(o);
        if !ob.tapped && crate::query::chars(s, db, o).types.has(CardType::Land) {
            n[ob.controller.idx()] = n[ob.controller.idx()].saturating_add(1);
        }
    }
    if let Some(r) = s.hist.turn_mut(t) {
        r.ended = true;
        for p in 0..2 {
            r.p[p].untapped_end = n[p];
        }
    }
}

/// Combat damage dealt to player `p` (CR 510.2).
pub fn combat_damage(s: &mut State, p: PlayerId, n: i32) {
    let t = s.turn.number;
    if let Some(r) = s.hist.turn_mut(t) {
        // Observation history only; each `n` is already bounded (`num`), the sum never wraps.
        r.p[p.idx()].combat_damage = r.p[p.idx()].combat_damage.saturating_add(n);
    }
}

/// A creature controlled by `controller` (battlefield object `obj`) died; recorded if this is a
/// combat damage step.
pub fn creature_died(s: &mut State, controller: PlayerId, obj: ObjId) {
    if !matches!(s.turn.step, Step::FirstStrikeDamage | Step::CombatDamage) {
        return;
    }
    let t = s.turn.number;
    if let Some(r) = s.hist.turn_mut(t) {
        r.p[controller.idx()].combat_deaths = r.p[controller.idx()].combat_deaths.saturating_add(1);
        if (r.n_died as usize) < DIED {
            r.died[r.n_died as usize] = obj.0;
            r.n_died += 1;
        } else {
            r.died_overflow = true;
        }
    }
}
