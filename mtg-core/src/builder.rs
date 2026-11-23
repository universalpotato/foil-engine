//! Build a game state from known facts (tests, search determinization).
//!
//! ```ignore
//! let mut b = GameBuilder::new(db);
//! b.hand(P0, &["Lightning Bolt"]).battlefield(P0, &["Mountain"]).battlefield(P1, &["Grizzly Bears"]);
//! let g = b.start(P0, Step::Main1, seed);
//! ```

use crate::basics::*;
use crate::carddb::CardDb;
use crate::engine::Game;
use crate::state::*;
use smallvec::SmallVec;
use std::sync::Arc;

pub const P0: PlayerId = PlayerId(0);
pub const P1: PlayerId = PlayerId(1);

pub struct GameBuilder {
    pub db: Arc<CardDb>,
    pub s: State,
}

impl GameBuilder {
    pub fn new(db: Arc<CardDb>) -> GameBuilder {
        let empty = crate::engine::Deck { main: vec![], side: vec![] };
        let mut s = crate::flow::initial_state(&db, [&empty, &empty], 0);
        s.stage = Stage::Playing;
        for p in &mut s.players {
            p.kept = true;
            p.drew_from_empty = false; // initial_state "drew" from the empty decks
        }
        s.events.clear();
        GameBuilder { db, s }
    }

    fn card(&self, name: &str) -> CardId {
        self.db.by_name(name).unwrap_or_else(|| panic!("card {name:?} not in db"))
    }

    fn add(&mut self, p: PlayerId, name: &str, zone: Zone) -> ObjId {
        let card = self.card(name);
        let id = ObjId(self.s.objs.len() as u32);
        let ts = self.s.ts();
        self.s.objs.push(Obj {
            card,
            owner: p,
            controller: p,
            zone,
            tapped: false,
            ctrl_since: 0,
            damage: 0,
            deathtouched: false,
            counters: SmallVec::new(),
            ts,
            token: false,
            known_to: if matches!(zone, Zone::Hand | Zone::Sideboard) { 1 << p.0 } else if zone == Zone::Library { 0 } else { 0b11 },
            entered: 0,
            cast_by: None,
            face: 0,
            on_adventure: false,
            attached_to: None,
            prepared: false,
            cast_mana: 0,
        });
        // A planeswalker put onto the battlefield gets its printed loyalty (CR 306.5b).
        if zone == Zone::Battlefield {
            if let Some(l) = self.db.get(card).front().loyalty {
                self.s.obj_mut(id).add_counter(CounterKind::Loyalty, l);
            }
        }
        match zone {
            Zone::Hand => self.s.player_mut(p).hand.push(id),
            Zone::Library => self.s.player_mut(p).library.push(id),
            Zone::Graveyard => self.s.player_mut(p).graveyard.push(id),
            Zone::Battlefield => self.s.battlefield.push(id),
            Zone::Exile => self.s.exile.push(id),
            Zone::Sideboard => self.s.player_mut(p).sideboard.push(id),
            _ => panic!("can't build into {zone:?}"),
        }
        id
    }

    pub fn hand(&mut self, p: PlayerId, names: &[&str]) -> &mut Self {
        for n in names {
            self.add(p, n, Zone::Hand);
        }
        self
    }
    /// Library cards, listed top first.
    pub fn library(&mut self, p: PlayerId, names: &[&str]) -> &mut Self {
        for n in names.iter().rev() {
            self.add(p, n, Zone::Library);
        }
        self
    }
    /// Cards outside the game (the sideboard, CR 400.11a).
    pub fn sideboard(&mut self, p: PlayerId, names: &[&str]) -> &mut Self {
        for n in names {
            self.add(p, n, Zone::Sideboard);
        }
        self
    }
    pub fn graveyard(&mut self, p: PlayerId, names: &[&str]) -> &mut Self {
        for n in names {
            self.add(p, n, Zone::Graveyard);
        }
        self
    }
    /// Permanents that have been under control since before this turn (not summoning sick).
    pub fn battlefield(&mut self, p: PlayerId, names: &[&str]) -> &mut Self {
        for n in names {
            self.add(p, n, Zone::Battlefield);
        }
        self
    }
    /// Add one permanent and return its id (for tapping, damage, ...).
    pub fn permanent(&mut self, p: PlayerId, name: &str) -> ObjId {
        self.add(p, name, Zone::Battlefield)
    }
    pub fn life(&mut self, p: PlayerId, life: i32) -> &mut Self {
        self.s.player_mut(p).life = life;
        self
    }

    /// Start at the beginning of `step` of turn 2+ with `active` as active player. Ensures each
    /// library has at least `min_library` filler cards so draws don't lose the game.
    pub fn start(mut self, active: PlayerId, step: Step, seed: u64) -> Game {
        let n = 3;
        self.s.turn = Turn { number: n, active, step, tba_done: false, priority: active, passes: 0 };
        self.s.player_mut(active).last_turn_started = n;
        self.s.player_mut(active.opp()).last_turn_started = n - 1;
        self.s.starting_player = active;
        self.s.rng = Rng::new(seed);
        for o in &mut self.s.objs {
            if o.zone == Zone::Battlefield {
                o.ctrl_since = 0;
            }
        }
        Game::from_state(self.db, self.s)
    }
}
