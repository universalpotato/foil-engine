//! Auxiliary *labels* read from the TRUE game state.
//!
//! LABELS ONLY. These functions read hidden information (the opponent's actual hand) and the
//! future of the turn (combat results recorded later in `hist`). Their outputs are prediction
//! targets about a game; they must never be decision inputs and never reach an agent at
//! decision time. Agents only ever see `observe(player)` / `encode`.

use crate::basics::*;
use crate::engine::Game;
use crate::hist::TurnRec;

/// Opponent hand cards reported per decision (more are dropped).
pub const HAND: usize = 16;

/// The opponent's true hand (card ids) from `me`'s seat, `-1`-padded into `out` (len `HAND`).
pub fn opp_hand(g: &Game, me: PlayerId, out: &mut [i32]) {
    out.fill(-1);
    let s = g.view_state();
    for (k, &h) in s.player(me.opp()).hand.iter().take(out.len()).enumerate() {
        out[k] = s.obj(h).card.0 as i32;
    }
}

/// Combat so far this turn from `me`'s seat: [damage to me, damage to opp, my creatures died,
/// opp's creatures died] (public; subtracted from the turn's final values to get "the rest of
/// this turn").
pub fn combat_so_far(g: &Game, me: PlayerId) -> [i32; 4] {
    let s = g.view_state();
    match s.hist.turn(s.turn.number) {
        Some(r) => {
            let (a, b) = (&r.p[me.idx()], &r.p[me.opp().idx()]);
            [a.combat_damage, b.combat_damage, a.combat_deaths as i32, b.combat_deaths as i32]
        }
        None => [0; 4],
    }
}

/// Every turn's final record for one game, merged from the state's `hist` ring (which keeps the
/// last `hist::TURNS` turns) after each decision and at the end of the game.
#[derive(Clone, Debug, Default)]
pub struct TurnLog {
    /// Indexed by turn number (`turns[t].turn == t` when present, else 0).
    pub turns: Vec<TurnRec>,
    /// Life totals and turn number when the game ended (set by `finish`).
    pub final_life: [i32; 2],
    pub final_turn: u32,
}

impl TurnLog {
    pub fn clear(&mut self) {
        self.turns.clear();
        self.final_life = [0; 2];
        self.final_turn = 0;
    }
    pub fn merge(&mut self, g: &Game) {
        for r in &g.view_state().hist.turns {
            if r.turn == 0 {
                continue;
            }
            let t = r.turn as usize;
            if self.turns.len() <= t {
                self.turns.resize(t + 1, TurnRec::default());
            }
            self.turns[t] = *r;
        }
    }
    pub fn finish(&mut self, g: &Game) {
        self.merge(g);
        let s = g.view_state();
        self.final_life = [s.player(PlayerId(0)).life, s.player(PlayerId(1)).life];
        self.final_turn = s.turn.number;
    }
}
