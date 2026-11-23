//! Best-of-three matches with sideboarding.
//!
//! A [`Match`] strings [`Game`]s together and adds two match-level decisions, posed through the
//! same `pending()` / `submit(i)` protocol as in-game decisions:
//! - `DecisionKind::PlayDraw` (`Opt::PlayFirst` / `Opt::DrawFirst`): CR 103.1. Game 1: a player
//!   chosen at random (from the match seed) chooses; later games: the loser of the previous game;
//!   after a drawn game, the player who chose in that game.
//! - `DecisionKind::Sideboard`: between games each player may modify their deck with their
//!   sideboard (CR 100.4), keeping it legal (CR 100.2a: at least 60 cards; 100.4a: at most 15 in
//!   the sideboard; the four-copy limit applies to deck + sideboard combined, which can't change
//!   by swapping). The choice is a canonical set choice (as `Ctx::choose_set`): `SideOut(card)`
//!   picks, then `SideIn(card)` picks, each after the previous one in a fixed order, then
//!   `Done`. A card can't be both taken out and brought in (that would be a second spelling of
//!   the same deck), so every legal resulting deck is reachable by exactly one sequence; `Done`
//!   is offered only when the result is legal, and a pick only when a legal completion exists.
//!
//! Order between games (CR 103.1 / 103.2a: the starting player is determined at the start of the
//! game, the sideboard is set aside after that): both players sideboard (player 0 then player 1;
//! neither sees the other's choice), then the chooser chooses play/draw, then the game starts.
//! Game 1 has no sideboarding (the registered decks are used).
//!
//! Seats are fixed for the whole match (player 0 keeps deck 0). Determinism: game `k` uses seed
//! `game_seed(seed, k)`; the match is a pure function of (decks, seed, choice history).
//!
//! Hidden information: `Match::view(p)` carries p's own registered and current deck, the public
//! match record (results, who chose and who started), and the opponent's cards p has *seen* in
//! finished games of this match (`opp_seen`): for each card, the most copies visible at once in
//! any observation p received in that game (taken at each of p's decisions and at game end, from
//! `observe(p)` itself). Never the opponent's list, sideboard or sideboarding choices.

use crate::basics::*;
use crate::carddb::CardDb;
use crate::engine::{Deck, Decision, DecisionKind, Game, Opt};
use crate::observe::{observe, Observation};
use crate::state::GameResult;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Minimum main deck size in constructed play (CR 100.2a).
pub const MIN_MAIN: usize = 60;
/// Maximum sideboard size in constructed play (CR 100.4a).
pub const MAX_SIDE: usize = 15;
/// Copies of a card with one English name in deck + sideboard (CR 100.2a, 100.4a).
pub const MAX_COPIES: u32 = 4;

/// Whether in-game encodings of match games carry match context (match globals,
/// opponent-seen tokens). False: match games encode exactly as Bo1 games (`MatchContext` is
/// passed but unused), so a consumer that needs match memory checks this flag.
pub const MATCH_CONTEXT: bool = false;

/// What the in-game encoder of a match game may use besides `observe(p)`: the deciding player's
/// `Match::view` (their own decks, the public match record, `opp_seen`). Hook for encoding v9.
pub struct MatchContext<'a> {
    pub m: &'a Match,
}

impl MatchContext<'_> {
    pub fn view(&self, p: PlayerId) -> MatchView {
        self.m.view(p)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchConfig {
    /// Game wins that win the match (2 = best of three, CR 100.6a).
    pub wins: u8,
    /// Games after which the match ends even if nobody reached `wins` (drawn games don't count
    /// toward a win; this caps them, like a tournament round's time limit): more game wins wins,
    /// else the match is a draw.
    pub max_games: u8,
    /// Accept decks without a sideboard (legal, CR 100.4 says "may"). Off by default so that a
    /// deck list that lost its sideboard fails loudly.
    pub allow_empty_sideboard: bool,
}

impl Default for MatchConfig {
    fn default() -> Self {
        MatchConfig { wins: 2, max_games: 5, allow_empty_sideboard: false }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchResult {
    Win(PlayerId),
    Draw,
}

/// Public record of a finished game.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GameRecord {
    /// Who chose play/draw (CR 103.1).
    pub chooser: PlayerId,
    /// The starting player.
    pub starting: PlayerId,
    pub result: GameResult,
    pub turns: u32,
}

/// Harness-only replay data of one game (the seed is hidden information: never give it to an
/// agent).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GameLog {
    pub seed: u64,
    pub starting: PlayerId,
    pub decks: [Deck; 2],
    /// `Game::history` of that game.
    pub choices: Vec<u16>,
}

/// A deck configuration as card counts, sorted by card id.
pub type Counts = Vec<(CardId, u8)>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeckView {
    pub main: Counts,
    pub side: Counts,
}

/// The observer's own sideboarding picks so far (their pending `Sideboard` decision only).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidePicks {
    pub out: Counts,
    #[serde(rename = "in")]
    pub inn: Counts,
}

/// What player `me` knows about the match (besides the current game's `observe(me)`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatchView {
    pub me: PlayerId,
    /// Index of the current game (or of the next one, between games), 0-based.
    pub game: u8,
    pub wins_needed: u8,
    pub max_games: u8,
    /// Game wins by player.
    pub wins: [u8; 2],
    /// Finished games.
    pub games: Vec<GameRecord>,
    /// Who chooses play/draw for the current / next game.
    pub chooser: Option<PlayerId>,
    /// The current game's starting player, once chosen.
    pub starting: Option<PlayerId>,
    /// My registered deck (game 1) and my current configuration.
    pub registered: DeckView,
    pub deck: DeckView,
    /// Per finished game: the opponent's cards I saw (card, most copies visible at once).
    pub opp_seen: Vec<Counts>,
    /// My pending match-level decision (`PlayDraw` / `Sideboard`), if any.
    pub decision: Option<Decision>,
    /// My sideboarding picks so far, while my `Sideboard` decision is pending.
    pub picks: Option<SidePicks>,
    pub result: Option<MatchResult>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    PlayDraw,
    Sideboard(Side),
    Game,
    Over,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Side {
    player: PlayerId,
    out: Vec<CardId>,
    inn: Vec<CardId>,
    /// Next allowed position in the canonical item order (`items`).
    next: usize,
}

/// Which level the pending decision is at.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Level {
    Match,
    Game,
}

#[derive(Clone)]
pub struct Match {
    pub db: Arc<CardDb>,
    pub seed: u64,
    pub cfg: MatchConfig,
    /// Registered decks (sorted), and the current configuration.
    registered: [Deck; 2],
    current: [Deck; 2],
    phase: Phase,
    /// Cached match-level decision (PlayDraw / Sideboard phases).
    decision: Option<Decision>,
    chooser: PlayerId,
    starting: Option<PlayerId>,
    /// The current game, or the last finished one between games (None before game 1).
    game: Option<Game>,
    games: Vec<GameRecord>,
    logs: Vec<GameLog>,
    wins: [u8; 2],
    /// `opp_seen` per player: finished games, and the current game so far.
    seen: [Vec<Counts>; 2],
    seen_cur: [BTreeMap<CardId, u8>; 2],
    result: Option<MatchResult>,
    error: Option<String>,
    /// Every recorded choice of the match in order (match-level and in-game; forced ones are not
    /// recorded): with the decks and seed, a full replay (`Match::replay`).
    pub history: Vec<u16>,
    stop_seat: Option<PlayerId>,
    /// `Game::keep_log` of the current and later games (true = the engine default).
    keep_log: bool,
}

/// Seed of game `k` (0-based) of a match.
pub fn game_seed(seed: u64, k: u8) -> u64 {
    Rng::new(seed ^ 0x6d61_7463_6830_0000 ^ k as u64).next_u64()
}

/// Who chooses play/draw for the next game (CR 103.1): the loser of the previous game; after a
/// draw, the player who chose in it.
pub fn next_chooser(prev_chooser: PlayerId, result: GameResult) -> PlayerId {
    match result {
        GameResult::Win(w) => w.opp(),
        GameResult::Draw => prev_chooser,
    }
}

fn counts(cards: &[CardId]) -> Counts {
    let mut m: BTreeMap<CardId, u8> = BTreeMap::new();
    for &c in cards {
        *m.entry(c).or_default() += 1;
    }
    m.into_iter().collect()
}

fn sorted(d: &Deck) -> Deck {
    let (mut main, mut side) = (d.main.clone(), d.side.clone());
    main.sort();
    side.sort();
    Deck { main, side }
}

/// How many cards named like `c` a deck may hold (CR 100.2a; "A deck can have any number of /
/// up to N cards named ..." overrides it).
fn copy_limit(db: &CardDb, c: CardId) -> Option<u32> {
    let card = db.get(c);
    let f = card.front();
    if f.supertypes.basic() {
        return None;
    }
    let o = f.oracle.to_ascii_lowercase();
    if o.contains("a deck can have any number of cards named") {
        return None;
    }
    if let Some(rest) = o.split("a deck can have up to ").nth(1) {
        let w = rest.split_whitespace().next().unwrap_or("");
        let n = ["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten"].iter().position(|x| *x == w);
        if let Some(n) = n {
            return Some(n as u32);
        }
    }
    Some(MAX_COPIES)
}

/// Constructed deck legality for a match: CR 100.2a (≥ 60 main, copy limit), 100.4a (≤ 15
/// sideboard; the copy limit over deck + sideboard), every card playable by the engine (an
/// unimplemented sideboard card would silently misplay once brought in), and a non-empty
/// sideboard unless `allow_empty_sideboard`.
pub fn check_deck(db: &CardDb, d: &Deck, cfg: &MatchConfig) -> Result<(), String> {
    let mut errs = vec![];
    if d.main.len() < MIN_MAIN {
        errs.push(format!("main deck has {} cards (CR 100.2a: at least {MIN_MAIN})", d.main.len()));
    }
    if d.side.len() > MAX_SIDE {
        errs.push(format!("sideboard has {} cards (CR 100.4a: at most {MAX_SIDE})", d.side.len()));
    }
    if d.side.is_empty() && !cfg.allow_empty_sideboard {
        errs.push("no sideboard: a best-of-three match needs one (pass allow_empty_sideboard to play without)".into());
    }
    let mut by_name: BTreeMap<&str, (u32, CardId)> = BTreeMap::new();
    for &c in d.main.iter().chain(&d.side) {
        by_name.entry(db.get(c).name.as_str()).or_insert((0, c)).0 += 1;
    }
    for (name, (n, c)) in &by_name {
        if let Some(lim) = copy_limit(db, *c) {
            if *n > lim {
                errs.push(format!("{n} copies of {name} in deck + sideboard (CR 100.2a / 100.4a: at most {lim})"));
            }
        }
        let def = db.get(*c);
        if !def.playable() {
            errs.push(format!("{name} can't be played faithfully yet: {}", def.unsupported.join("; ")));
        }
        if def.is_token {
            errs.push(format!("{name} is a token, not a card"));
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs.join("\n"))
    }
}

/// The opponent's cards `obs.me` can see in `obs` (identity known, owned by the opponent, not
/// tokens), with the number of copies visible at once. Hidden cards (`card: None`) don't count.
pub fn opp_visible(obs: &Observation) -> BTreeMap<CardId, u8> {
    let opp = obs.me.opp();
    let mut m: BTreeMap<CardId, u8> = BTreeMap::new();
    let mut add = |c: CardId| {
        let e = m.entry(c).or_default();
        *e = e.saturating_add(1);
    };
    for pv in obs.players.iter().filter(|pv| pv.id == opp) {
        // Hand, graveyard and sideboard objects are their owner's (CR 108.3, 400.3).
        for v in pv.hand.iter().chain(&pv.graveyard).chain(&pv.sideboard) {
            if let (Some(c), false) = (v.card, v.token) {
                add(c);
            }
        }
        for &(_, c) in &pv.library_known {
            add(c);
        }
    }
    for v in obs.battlefield.iter().chain(&obs.exile).chain(&obs.casting) {
        if v.owner != opp || v.token {
            continue;
        }
        // A permanent copying another (CR 707) is its own card.
        if let Some(c) = v.copy_of_printed.or(v.card) {
            add(c);
        }
    }
    m
}

impl Match {
    /// A new match; validates both decks (`check_deck`). Seat `i` plays `decks[i]`.
    pub fn new(db: Arc<CardDb>, decks: [&Deck; 2], seed: u64, cfg: MatchConfig) -> Result<Match, String> {
        if cfg.wins == 0 || cfg.max_games < 2 * cfg.wins - 1 {
            return Err(format!("bad match config {cfg:?}: max_games must be at least 2 * wins - 1"));
        }
        for (i, d) in decks.iter().enumerate() {
            check_deck(&db, d, &cfg).map_err(|e| format!("deck {i}: {e}"))?;
        }
        let registered = [sorted(decks[0]), sorted(decks[1])];
        // CR 103.1: game 1's chooser by a random method agreed by the players.
        let chooser = PlayerId(Rng::new(seed ^ 0x6d61_7463_6863_686f).below(2) as u8);
        let mut m = Match {
            db,
            seed,
            cfg,
            current: registered.clone(),
            registered,
            phase: Phase::PlayDraw,
            decision: None,
            chooser,
            starting: None,
            game: None,
            games: vec![],
            logs: vec![],
            wins: [0, 0],
            seen: [vec![], vec![]],
            seen_cur: [BTreeMap::new(), BTreeMap::new()],
            result: None,
            error: None,
            history: vec![],
            stop_seat: None,
            keep_log: true,
        };
        m.settle();
        Ok(m)
    }

    /// `new` followed by `choices` (a recorded `history`).
    pub fn replay(db: Arc<CardDb>, decks: [&Deck; 2], seed: u64, cfg: MatchConfig, choices: &[u16]) -> Result<Match, String> {
        let mut m = Match::new(db, decks, seed, cfg)?;
        for (k, &c) in choices.iter().enumerate() {
            m.submit(c as usize).map_err(|e| format!("replay choice #{k}: {e}"))?;
        }
        Ok(m)
    }

    pub fn pending(&self) -> Option<&Decision> {
        match self.phase {
            Phase::PlayDraw | Phase::Sideboard(_) => self.decision.as_ref(),
            Phase::Game => self.game.as_ref().and_then(|g| g.pending()),
            Phase::Over => None,
        }
    }

    pub fn level(&self) -> Option<Level> {
        match self.phase {
            Phase::PlayDraw | Phase::Sideboard(_) => Some(Level::Match),
            Phase::Game => self.game.as_ref().and_then(|g| g.pending()).map(|_| Level::Game),
            Phase::Over => None,
        }
    }

    /// The current game (or the last finished one, between games; None before game 1 starts).
    pub fn game(&self) -> Option<&Game> {
        self.game.as_ref()
    }

    /// The game in progress (None between games).
    pub fn game_in_progress(&self) -> Option<&Game> {
        (self.phase == Phase::Game).then_some(()).and(self.game.as_ref())
    }

    /// Index of the current game (or of the next one, between games).
    pub fn game_index(&self) -> u8 {
        let done = self.games.len() as u8;
        if self.phase == Phase::Over {
            done.saturating_sub(1)
        } else {
            done
        }
    }

    pub fn games(&self) -> &[GameRecord] {
        &self.games
    }
    /// Harness-only replay data of the finished games.
    pub fn logs(&self) -> &[GameLog] {
        &self.logs
    }
    pub fn wins(&self) -> [u8; 2] {
        self.wins
    }
    pub fn result(&self) -> Option<MatchResult> {
        self.result
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn is_over(&self) -> bool {
        self.phase == Phase::Over
    }
    /// The current deck configuration of player `p` (harness view; agents use `view`).
    pub fn deck(&self, p: PlayerId) -> &Deck {
        &self.current[p.idx()]
    }
    pub fn registered(&self, p: PlayerId) -> &Deck {
        &self.registered[p.idx()]
    }

    /// For a front end (`Game::set_stop_seat`): applies to the current and every later game.
    pub fn set_stop_seat(&mut self, p: Option<PlayerId>) {
        self.stop_seat = p;
        if let (Phase::Game, Some(g)) = (&self.phase, self.game.as_mut()) {
            g.set_stop_seat(p);
        }
    }

    /// `Game::keep_log` (the readable engine trace) for the current and every later game; false
    /// drops it.
    pub fn set_keep_log(&mut self, keep: bool) {
        self.keep_log = keep;
        if let Some(g) = self.game.as_mut() {
            g.keep_log = keep;
            if !keep {
                g.log.clear();
            }
        }
    }

    pub fn submit(&mut self, choice: usize) -> Result<(), String> {
        match self.phase {
            Phase::Over => Err("match is over".into()),
            Phase::Game => {
                let g = self.game.as_mut().ok_or("no game")?;
                let d = g.pending().ok_or("no pending decision")?.clone();
                if choice >= d.options.len() {
                    return Err(format!("choice {choice} out of range ({} options)", d.options.len()));
                }
                // What the decider sees now (`observe` of their pending decision).
                let obs = observe(g, d.player);
                note(&mut self.seen_cur[d.player.idx()], &obs);
                let forced = g.pending_is_forced();
                g.submit(choice)?;
                if !forced {
                    self.history.push(choice as u16);
                }
                if g.is_over() {
                    self.end_game();
                }
                Ok(())
            }
            Phase::PlayDraw | Phase::Sideboard(_) => {
                let d = self.decision.as_ref().ok_or("no pending decision")?;
                if choice >= d.options.len() {
                    return Err(format!("choice {choice} out of range ({} options)", d.options.len()));
                }
                let o = d.options[choice];
                self.history.push(choice as u16);
                self.apply(o);
                self.settle();
                Ok(())
            }
        }
    }

    /// Apply a match-level option.
    fn apply(&mut self, o: Opt) {
        let phase = std::mem::replace(&mut self.phase, Phase::Over);
        match phase {
            Phase::PlayDraw => {
                let starting = match o {
                    Opt::PlayFirst => self.chooser,
                    Opt::DrawFirst => self.chooser.opp(),
                    _ => unreachable!("PlayDraw option {o:?}"),
                };
                self.start_game(starting);
            }
            Phase::Sideboard(mut sd) => {
                let n_main = self.current[sd.player.idx()].main.len();
                match o {
                    Opt::Done => {
                        let p = sd.player;
                        let d = &mut self.current[p.idx()];
                        for c in &sd.out {
                            let i = d.main.iter().position(|x| x == c).expect("out card in main");
                            d.main.remove(i);
                            d.side.push(*c);
                        }
                        for c in &sd.inn {
                            let i = d.side.iter().position(|x| x == c).expect("in card in side");
                            d.side.remove(i);
                            d.main.push(*c);
                        }
                        d.main.sort();
                        d.side.sort();
                        if let Err(e) = check_deck(&self.db, d, &MatchConfig { allow_empty_sideboard: true, ..self.cfg.clone() }) {
                            self.error = Some(format!("sideboarding produced an illegal deck (engine bug): {e}"));
                            return;
                        }
                        self.phase = if p.0 == 0 { Phase::Sideboard(Side { player: PlayerId(1), out: vec![], inn: vec![], next: 0 }) } else { Phase::PlayDraw };
                    }
                    Opt::SideOut(c) | Opt::SideIn(c) => {
                        let pos = side_items(&self.current[sd.player.idx()]).iter().enumerate().skip(sd.next).find(|(_, x)| **x == o).map(|(i, _)| i).expect("picked item");
                        sd.next = pos + 1;
                        if matches!(o, Opt::SideOut(_)) {
                            sd.out.push(c);
                        } else {
                            sd.inn.push(c);
                        }
                        debug_assert!(sd.out.len() <= n_main);
                        self.phase = Phase::Sideboard(sd);
                    }
                    _ => unreachable!("Sideboard option {o:?}"),
                }
            }
            Phase::Game | Phase::Over => unreachable!(),
        }
    }

    /// Compute the next match-level decision; take forced ones (one option) without recording
    /// them, as `Ctx::choose` does in games.
    fn settle(&mut self) {
        loop {
            if self.error.is_some() {
                self.phase = Phase::Over;
                self.decision = None;
                return;
            }
            let d = match &self.phase {
                Phase::PlayDraw => Decision { player: self.chooser, kind: DecisionKind::PlayDraw, options: vec![Opt::PlayFirst, Opt::DrawFirst], subject: None },
                Phase::Sideboard(sd) => Decision { player: sd.player, kind: DecisionKind::Sideboard, options: side_options(&self.current[sd.player.idx()], sd), subject: None },
                Phase::Game | Phase::Over => {
                    self.decision = None;
                    return;
                }
            };
            match d.options.len() {
                0 => {
                    self.error = Some(format!("match decision {:?} with no options", d.kind));
                }
                1 => self.apply(d.options[0]),
                _ => {
                    self.decision = Some(d);
                    return;
                }
            }
        }
    }

    fn start_game(&mut self, starting: PlayerId) {
        let k = self.games.len() as u8;
        let mut g = Game::new_with_starting(self.db.clone(), [&self.current[0], &self.current[1]], game_seed(self.seed, k), Some(starting));
        g.set_stop_seat(self.stop_seat);
        if !self.keep_log {
            g.keep_log = false;
            g.log.clear();
        }
        self.starting = Some(starting);
        self.game = Some(g);
        self.phase = Phase::Game;
        self.decision = None;
        if self.game.as_ref().unwrap().is_over() {
            self.end_game();
        }
    }

    fn end_game(&mut self) {
        let g = self.game.as_ref().expect("game");
        let k = self.games.len();
        if let Some(e) = g.error() {
            self.error = Some(format!("game {}: engine error: {e}", k + 1));
            self.phase = Phase::Over;
            return;
        }
        let result = g.result().expect("finished game has a result");
        for p in [PlayerId(0), PlayerId(1)] {
            let obs = observe(g, p);
            note(&mut self.seen_cur[p.idx()], &obs);
            let cur = std::mem::take(&mut self.seen_cur[p.idx()]);
            self.seen[p.idx()].push(cur.into_iter().collect());
        }
        let starting = self.starting.expect("starting player");
        self.games.push(GameRecord { chooser: self.chooser, starting, result, turns: g.s.turn.number });
        self.logs.push(GameLog { seed: game_seed(self.seed, k as u8), starting, decks: self.current.clone(), choices: g.history.clone() });
        if let GameResult::Win(w) = result {
            self.wins[w.idx()] += 1;
        }
        self.chooser = next_chooser(self.chooser, result);
        self.starting = None;
        if let Some(w) = [PlayerId(0), PlayerId(1)].into_iter().find(|p| self.wins[p.idx()] >= self.cfg.wins) {
            self.result = Some(MatchResult::Win(w));
            self.phase = Phase::Over;
        } else if self.games.len() >= self.cfg.max_games as usize {
            self.result = Some(match self.wins[0].cmp(&self.wins[1]) {
                std::cmp::Ordering::Greater => MatchResult::Win(PlayerId(0)),
                std::cmp::Ordering::Less => MatchResult::Win(PlayerId(1)),
                std::cmp::Ordering::Equal => MatchResult::Draw,
            });
            self.phase = Phase::Over;
        } else {
            self.phase = Phase::Sideboard(Side { player: PlayerId(0), out: vec![], inn: vec![], next: 0 });
            self.settle();
        }
    }

    /// Whole-deck sideboarding (harnesses):
    /// player `p`'s pending `Sideboard` decision (no picks made yet) is
    /// answered with the multisets `out` (main -> sideboard) and `inn` (sideboard -> main),
    /// submitted as the unique canonical pick sequence (out picks in item order, in picks in item
    /// order, `Done`), so `history` and `replay` are exactly as if picked one by one. Validated
    /// first (nothing is submitted on error): `out` within the main deck, `inn` within the
    /// sideboard, no card both out and in, result legal (CR 100.2a / 100.4a).
    pub fn submit_sideboard(&mut self, p: PlayerId, out: &[CardId], inn: &[CardId]) -> Result<(), String> {
        match &self.phase {
            Phase::Sideboard(sd) if sd.player == p => {
                if !sd.out.is_empty() || !sd.inn.is_empty() {
                    return Err("submit_sideboard: picks already made one by one".into());
                }
            }
            _ => return Err(format!("submit_sideboard: no pending sideboarding of player {}", p.0)),
        }
        let d = &self.current[p.idx()];
        let (cm, cs, co, ci) = (counts(&d.main), counts(&d.side), counts(out), counts(inn));
        let has = |m: &Counts, c: CardId, n: u8| m.iter().any(|&(x, k)| x == c && k >= n);
        if let Some(&(c, n)) = co.iter().find(|&&(c, n)| !has(&cm, c, n)) {
            return Err(format!("submit_sideboard: {n} x card {} out, not in the main deck", c.0));
        }
        if let Some(&(c, n)) = ci.iter().find(|&&(c, n)| !has(&cs, c, n)) {
            return Err(format!("submit_sideboard: {n} x card {} in, not in the sideboard", c.0));
        }
        if let Some(&(c, _)) = co.iter().find(|&&(c, _)| ci.iter().any(|&(x, _)| x == c)) {
            return Err(format!("submit_sideboard: card {} both out and in", c.0));
        }
        let main = d.main.len() as i64 + inn.len() as i64 - out.len() as i64;
        let side = d.side.len() as i64 + out.len() as i64 - inn.len() as i64;
        if main < MIN_MAIN as i64 || side > MAX_SIDE as i64 {
            return Err(format!("submit_sideboard: {main} main / {side} sideboard cards (CR 100.2a: >= {MIN_MAIN}, 100.4a: <= {MAX_SIDE})"));
        }
        let mut want_main = d.main.clone();
        for c in out {
            want_main.remove(want_main.iter().position(|x| x == c).expect("validated"));
        }
        want_main.extend_from_slice(inn);
        want_main.sort();
        let mut picks: Vec<Opt> = co.iter().flat_map(|&(c, n)| std::iter::repeat(Opt::SideOut(c)).take(n as usize)).collect();
        picks.extend(ci.iter().flat_map(|&(c, n)| std::iter::repeat(Opt::SideIn(c)).take(n as usize)));
        picks.push(Opt::Done);
        // Picks with a single legal option are taken by the match itself, unrecorded (`settle`, as
        // `Ctx::choose`), exactly as when picking one by one: submit the next pick not made yet.
        loop {
            let made = match &self.phase {
                Phase::Sideboard(sd) if sd.player == p => sd.out.len() + sd.inn.len(),
                _ => break,
            };
            let o = picks[made.min(picks.len() - 1)];
            let d = self.decision.as_ref().filter(|d| d.kind == DecisionKind::Sideboard && d.player == p);
            let Some(i) = d.and_then(|d| d.options.iter().position(|x| *x == o)) else {
                // validated above: every legal split has a canonical sequence (bo3::tests)
                self.error = Some(format!("submit_sideboard: canonical pick {o:?} not offered (engine bug)"));
                self.phase = Phase::Over;
                self.decision = None;
                return Err(self.error.clone().unwrap());
            };
            self.submit(i)?;
        }
        if self.error.is_none() && self.current[p.idx()].main != want_main {
            self.error = Some("submit_sideboard: the resulting deck differs from the choice (engine bug)".into());
            self.phase = Phase::Over;
            self.decision = None;
            return Err(self.error.clone().unwrap());
        }
        Ok(())
    }

    /// Answer the pending `PlayDraw` decision (CR 103.1).
    pub fn submit_playdraw(&mut self, play_first: bool) -> Result<(), String> {
        let want = if play_first { Opt::PlayFirst } else { Opt::DrawFirst };
        let i = match (&self.phase, &self.decision) {
            (Phase::PlayDraw, Some(d)) => d.options.iter().position(|x| *x == want).ok_or("PlayDraw option missing")?,
            _ => return Err("submit_playdraw: no pending play/draw decision".into()),
        };
        self.submit(i)
    }

    /// The declared `keep` baseline for a pending match-level decision
    /// (a reference baseline, never a strategy given to an agent): `Done` at once
    /// for sideboarding, `PlayFirst` for play/draw. Returns the option index, or None when the
    /// pending decision isn't match-level.
    pub fn keep_choice(&self) -> Option<usize> {
        let d = match self.phase {
            Phase::PlayDraw | Phase::Sideboard(_) => self.decision.as_ref()?,
            _ => return None,
        };
        let want = match d.kind {
            DecisionKind::Sideboard => Opt::Done,
            DecisionKind::PlayDraw => Opt::PlayFirst,
            _ => return None,
        };
        d.options.iter().position(|x| *x == want)
    }

    /// What player `p` knows about the match (see the module docs).
    pub fn view(&self, p: PlayerId) -> MatchView {
        let dv = |d: &Deck| DeckView { main: counts(&d.main), side: counts(&d.side) };
        let decision = match self.phase {
            Phase::PlayDraw | Phase::Sideboard(_) => self.decision.clone().filter(|d| d.player == p),
            _ => None,
        };
        let picks = match &self.phase {
            Phase::Sideboard(sd) if sd.player == p => Some(SidePicks { out: counts(&sd.out), inn: counts(&sd.inn) }),
            _ => None,
        };
        MatchView {
            me: p,
            game: self.game_index(),
            wins_needed: self.cfg.wins,
            max_games: self.cfg.max_games,
            wins: self.wins,
            games: self.games.clone(),
            chooser: (self.phase != Phase::Over).then_some(self.chooser),
            starting: self.starting,
            registered: dv(&self.registered[p.idx()]),
            deck: dv(&self.current[p.idx()]),
            opp_seen: self.seen[p.idx()].clone(),
            decision,
            picks,
            result: self.result,
        }
    }
}

fn note(m: &mut BTreeMap<CardId, u8>, obs: &Observation) {
    for (c, n) in opp_visible(obs) {
        let e = m.entry(c).or_default();
        *e = (*e).max(n);
    }
}

/// The canonical item order of a sideboarding choice: every main-deck card (`SideOut`), then
/// every sideboard card (`SideIn`), each sorted by card id (copies adjacent).
fn side_items(d: &Deck) -> Vec<Opt> {
    d.main.iter().map(|&c| Opt::SideOut(c)).chain(d.side.iter().map(|&c| Opt::SideIn(c))).collect()
}

/// Options of a pending `Sideboard` decision (see the module docs): `Done` first when the
/// result would be legal, then each distinct item at or after `sd.next` that keeps a legal
/// completion reachable.
fn side_options(d: &Deck, sd: &Side) -> Vec<Opt> {
    let items = side_items(d);
    // With k cards out and j in: main = m - k + j >= 60 and side = s + k - j <= 15, i.e.
    // j >= k - slack.
    let slack = (d.main.len() as i64 - MIN_MAIN as i64).min(MAX_SIDE as i64 - d.side.len() as i64);
    let (k, j) = (sd.out.len() as i64, sd.inn.len() as i64);
    let avail_in = |extra: Option<CardId>| d.side.iter().filter(|c| !sd.out.contains(c) && Some(**c) != extra).count() as i64;
    let mut opts = vec![];
    if j >= k - slack {
        opts.push(Opt::Done);
    }
    let mut last: Option<Opt> = None;
    for (i, &o) in items.iter().enumerate().skip(sd.next) {
        if last == Some(o) {
            continue;
        }
        let ok = match o {
            // Taking one more out needs enough sideboard cards (not taken out) to bring in.
            Opt::SideOut(c) => k + 1 - slack <= avail_in(Some(c)),
            // A card taken out isn't brought back in (one spelling per deck); picking this one
            // skips the earlier ones, so enough eligible cards must remain after it.
            Opt::SideIn(c) => {
                let after = items[i + 1..].iter().filter(|x| matches!(x, Opt::SideIn(c2) if !sd.out.contains(c2))).count() as i64;
                !sd.out.contains(&c) && j + 1 + after >= k - slack
            }
            _ => unreachable!(),
        };
        if ok {
            last = Some(o);
            opts.push(o);
        }
    }
    opts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deck(main: &[(u32, u32)], side: &[(u32, u32)]) -> Deck {
        let ex = |v: &[(u32, u32)]| v.iter().flat_map(|&(c, n)| std::iter::repeat(CardId(c)).take(n as usize)).collect::<Vec<_>>();
        Deck { main: ex(main), side: ex(side) }
    }

    /// Every sequence of picks of a sideboarding choice, depth first: the resulting decks.
    fn enumerate(d: &Deck) -> Vec<(Vec<CardId>, Vec<CardId>)> {
        let mut out = vec![];
        let mut stack = vec![Side { player: PlayerId(0), out: vec![], inn: vec![], next: 0 }];
        while let Some(sd) = stack.pop() {
            let opts = side_options(d, &sd);
            assert!(!opts.is_empty(), "dead end after out {:?} in {:?}", sd.out, sd.inn);
            for o in opts {
                let mut s2 = sd.clone();
                match o {
                    Opt::Done => {
                        let mut main = d.main.clone();
                        let mut side = d.side.clone();
                        for c in &sd.out {
                            main.remove(main.iter().position(|x| x == c).unwrap());
                            side.push(*c);
                        }
                        for c in &sd.inn {
                            side.remove(side.iter().position(|x| x == c).unwrap());
                            main.push(*c);
                        }
                        main.sort();
                        side.sort();
                        out.push((main, side));
                        continue;
                    }
                    Opt::SideOut(c) | Opt::SideIn(c) => {
                        let pos = side_items(d).iter().enumerate().skip(sd.next).find(|(_, x)| **x == o).unwrap().0;
                        s2.next = pos + 1;
                        if matches!(o, Opt::SideOut(_)) {
                            s2.out.push(c)
                        } else {
                            s2.inn.push(c)
                        }
                    }
                    _ => unreachable!(),
                }
                stack.push(s2);
            }
        }
        out
    }

    /// Brute force: every legal (main, side) split of the pool reachable by moving cards.
    fn legal_splits(d: &Deck) -> Vec<(Vec<CardId>, Vec<CardId>)> {
        let pool = counts(&d.main.iter().chain(&d.side).copied().collect::<Vec<_>>());
        let mut out = vec![];
        let mut cur = vec![0u8; pool.len()];
        loop {
            let main: Vec<CardId> = pool.iter().zip(&cur).flat_map(|(&(c, _), &k)| std::iter::repeat(c).take(k as usize)).collect();
            let side: Vec<CardId> = pool.iter().zip(&cur).flat_map(|(&(c, n), &k)| std::iter::repeat(c).take((n - k) as usize)).collect();
            if main.len() >= MIN_MAIN && side.len() <= MAX_SIDE {
                out.push((main, side));
            }
            // odometer
            let mut i = 0;
            loop {
                if i == pool.len() {
                    return out;
                }
                if cur[i] < pool[i].1 {
                    cur[i] += 1;
                    break;
                }
                cur[i] = 0;
                i += 1;
            }
        }
    }

    fn check_canonical(d: &Deck) {
        let mut got = enumerate(d);
        let n = got.len();
        got.sort();
        got.dedup();
        assert_eq!(got.len(), n, "a deck is reachable by two different pick sequences");
        let mut want = legal_splits(d);
        want.sort();
        assert_eq!(got, want, "reachable decks != legal decks");
        for (main, side) in &got {
            assert!(main.len() >= MIN_MAIN && side.len() <= MAX_SIDE);
        }
    }

    #[test]
    fn sideboard_choices_reach_every_legal_deck_exactly_once() {
        // 61 main / 3 side: slack 1 (one card more out than in).
        check_canonical(&deck(&[(0, 20), (1, 20), (2, 21)], &[(3, 2), (4, 1)]));
        // 60 / 15: slack 0 (one-for-one or more in); a name in both main and side.
        check_canonical(&deck(&[(0, 54), (1, 3), (2, 3)], &[(1, 1), (3, 4), (5, 4), (6, 3), (7, 3)]));
        // 62 / 2 with copies on both sides.
        check_canonical(&deck(&[(0, 58), (1, 2), (2, 2)], &[(1, 2)]));
    }

    #[test]
    fn chooser_rules() {
        // CR 103.1: the loser chooses; after a draw, the previous chooser again.
        assert_eq!(next_chooser(PlayerId(0), GameResult::Win(PlayerId(0))), PlayerId(1));
        assert_eq!(next_chooser(PlayerId(0), GameResult::Win(PlayerId(1))), PlayerId(0));
        assert_eq!(next_chooser(PlayerId(1), GameResult::Draw), PlayerId(1));
        assert_eq!(next_chooser(PlayerId(0), GameResult::Draw), PlayerId(0));
    }
}
