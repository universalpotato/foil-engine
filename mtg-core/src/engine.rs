//! Step-based driver. The engine never calls agents: `Game::pending()` exposes the current
//! decision and `Game::submit(i)` answers it.
//!
//! Mechanism ("choice tape"): the game advances in *units* (one turn-based action, one SBA
//! pass, one priority decision plus the action it starts, ...). A unit runs on a scratch clone
//! of the committed state and asks questions via `Ctx::choose`, which reads answers from the
//! tape. If the tape runs out, the scratch state is dropped and the decision is returned; when
//! the answer arrives, the unit re-runs from the committed state with the longer tape. On
//! completion the scratch becomes the committed state and the tape is cleared. Effects are
//! straight-line code, paused games are plain data (cheap clone for search), and determinism
//! follows from the RNG living in the state.

use crate::basics::*;
use crate::carddb::{AbilityId, CardDb};
use crate::state::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionKind {
    Mulligan,
    BottomCard,
    Priority,
    ChooseX,
    Target,
    PayMana,
    DeclareAttackers,
    DeclareBlockers,
    AssignDamage,
    LegendRule,
    OrderTriggers,
    OptionalEffect,
    Scry,
    Surveil,
    Discard,
    PayLife,
    PayUnless,
    PayCost,
    Dig,
    Search,
    Sacrifice,
    ChooseMode,
    Kicker,
    /// Choose a creature/land type (CR 205.3; "as enters, choose a type").
    ChooseType,
    /// Choose objects for an effect (`ChooseCard`).
    ChooseCard,
    /// Choose a card name (CR 201.3).
    NameCard,
    /// Announce optional / "A or B" additional costs (`Opt::CostOption`, codes in `altcast::code`).
    AdditionalCost,
    /// Choose an object to copy ("enter as a copy of", CR 707).
    CopyChoice,
    /// Match level (`bo3::Match`), between games: choose who takes the first turn (CR 103.1).
    /// Options `PlayFirst` / `DrawFirst`.
    PlayDraw,
    /// Match level (`bo3::Match`), between games: modify the deck with the sideboard (CR 100.4,
    /// 100.4a). A canonical set choice of `SideOut` cards then `SideIn` cards, then `Done`.
    Sideboard,
    /// CR 616.1e: the affected object's controller chooses which applicable replacement effect
    /// applies first (`Opt::Card` = the effect's source; the entering permanent itself for its own
    /// "enters" effects). Asked only when the order can change the result (`zones::as_enters`).
    ChooseReplacement,
}

/// One option at a decision. Ids refer to objects visible to the deciding player.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Opt {
    Keep,
    Mulligan,
    Card(ObjId),
    Pass,
    PlayLand(ObjId),
    Cast(ObjId),
    /// Cast a card using an alternative face (CR 715.3: as an Adventure).
    CastFace(ObjId, u8),
    /// Cast from the graveyard with flashback (CR 702.34).
    Flashback(ObjId),
    /// Cast a copy of this prepared permanent's prepare spell (CR 722.3c).
    CastPrepared(ObjId),
    Activate(ObjId, AbilityId),
    Target(Target),
    Number(i32),
    Mana(ObjId, AbilityId, ManaType),
    Attack(ObjId, PlayerId),
    /// Attack a planeswalker (attacker, planeswalker), CR 508.1b.
    AttackPw(ObjId, ObjId),
    Block(ObjId, ObjId),
    /// Two blockers on one attacker at once (needed for menace, CR 702.111b).
    Block2(ObjId, ObjId, ObjId),
    Trigger(u8),
    Done,
    Yes,
    No,
    ScryTop,
    ScryBottom,
    ToGraveyard,
    Mode(u8),
    /// A subtype (creature/land type) for `ChooseType`.
    Subtype(Subtype),
    /// A card name (by card id) for `NameCard`.
    Name(u16),
    /// Cast by an alternative method / take a casting special action (`altcast::Method`).
    Alt(ObjId, u8),
    /// An additional-cost choice (`altcast::code`).
    CostOption(u8),
    /// Spend one mana of this type from the paying player's mana pool (CR 106.4).
    PoolMana(ManaType),
    /// `PlayDraw`: the chooser takes the first turn (is the starting player, CR 103.1).
    PlayFirst,
    /// `PlayDraw`: the opponent takes the first turn.
    DrawFirst,
    /// `Sideboard`: move one copy of this card from the main deck to the sideboard.
    SideOut(CardId),
    /// `Sideboard`: move one copy of this card from the sideboard to the main deck.
    SideIn(CardId),
}

/// A stable ordering key for options (used to canonicalize set choices).
pub fn opt_key(o: &Opt) -> (u8, u32, u32) {
    match *o {
        Opt::Card(x) | Opt::PlayLand(x) | Opt::Cast(x) | Opt::CastFace(x, _) | Opt::Flashback(x) | Opt::CastPrepared(x) | Opt::Alt(x, _) => (0, x.0, 0),
        Opt::Target(Target::Obj(x)) => (1, x.0, 0),
        Opt::Target(Target::Player(p)) => (2, p.0 as u32, 0),
        Opt::Target(Target::Ability { uid, src }) => (7, uid, src.0),
        Opt::Attack(x, p) => (3, x.0, p.0 as u32),
        Opt::AttackPw(x, pw) => (3, x.0, 2 + pw.0),
        Opt::Block(b, a) => (4, b.0, a.0),
        Opt::Block2(b, c, a) => (5, b.0, (c.0 << 8) ^ a.0),
        Opt::Mana(x, a, t) => (6, x.0, (a.0 << 3) | t as u32),
        _ => (9, 0, 0),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub player: PlayerId,
    pub kind: DecisionKind,
    pub options: Vec<Opt>,
    /// Object the decision is about (spell being cast, attacker assigning damage, ...).
    pub subject: Option<ObjId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Halt {
    Need(Decision),
    GameOver,
    /// Engine reached rules it doesn't implement. The game is aborted; this is a bug/coverage gap.
    Unsupported(String),
    /// A submitted choice index was out of range.
    BadChoice,
}

pub type R<T> = Result<T, Halt>;

/// Per-seat decision fidelity. `Agent` (default; agent and
/// search seats): the pruned / canonical decision structure. `Full` (human seats): the
/// extra choices a player has under the CR (e.g. paying mana in any order,
/// ordering identical-looking triggers), posed through [`Ctx::choose_full`]. Invariant (checked
/// by tests): a full seat that always picks an agent-mode option produces the
/// agent-mode game (same `agent_history`, log, result, every observation of the other seat).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Fidelity {
    #[default]
    Agent,
    Full,
}

impl Fidelity {
    pub fn parse(s: &str) -> Result<Fidelity, String> {
        match s {
            "agent" => Ok(Fidelity::Agent),
            "full" => Ok(Fidelity::Full),
            _ => Err(format!("fidelity must be \"agent\" or \"full\", not {s:?}")),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Fidelity::Agent => "agent",
            Fidelity::Full => "full",
        }
    }
}

/// Tape entry flag of a full-fidelity choice that has no agent-mode equivalent (the low 15 bits
/// are the index into the full option list). Untagged entries are agent-mode indices.
pub const FULL_TAG: u16 = 0x8000;

pub struct Ctx<'a> {
    pub s: &'a mut State,
    pub db: &'a CardDb,
    tape: &'a [u16],
    pos: usize,
    /// Options of this unit's first surfaced decision, when it is a priority decision already
    /// computed by the run that posed it (`Game::rerun_hint`).
    rerun_hint: Option<&'a RerunHint>,
    /// `Game::stop_seat`: surface this player's forced priority passes (front ends only, off by default).
    stop: Option<PlayerId>,
    /// Forced stops at the live frontier already acknowledged in this unit (`Game::acks`).
    acks: usize,
    /// Forced stops at the live frontier met so far in this run of the unit.
    forced_seen: usize,
    /// Per-seat fidelity of this unit (`Game::unit_fid`).
    fid: [Fidelity; 2],
    /// Set when halting at a full-fidelity decision.
    fid_map: Option<FullMap>,
    /// Merged alternatives to substitute at tape positions (`Game::submit_alt`; empty in play).
    subs: &'a [AltSub],
    /// The (offered, substituted) pair of the latest choice, if it was substituted.
    last_alt: Option<(Opt, Opt)>,
}

/// Prefix of the engine error a refused merged alternative ends the unit with
/// (`Game::submit_alt`): the alternative isn't an option the decision merged.
pub const ALT_REFUSED: &str = "merged alternative refused:";

/// One `Game::submit_alt` substitution: at tape position `pos`, the offered option `offered` is
/// answered with the merged alternative `alt`. `strict`: only where the decision's own merge key
/// says `alt` is identical to `offered` (`Ctx::choose_set`); refused at any other decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AltSub {
    pub pos: u16,
    pub offered: Opt,
    pub alt: Opt,
    pub strict: bool,
}

/// A pending full-fidelity decision: the tape entry of each full option (`FULL_TAG` = no
/// agent-mode equivalent) and the full indices of the agent-mode options in agent order.
#[derive(Clone, Debug)]
pub struct FullMap {
    pub entry: Vec<u16>,
    pub agent: Vec<u16>,
}

impl<'a> Ctx<'a> {
    /// A context over a scratch copy of a state for read-only "what if" checks (no choices are
    /// made through it: any decision would halt with `Need`).
    pub fn scratch(s: &'a mut State, db: &'a CardDb) -> Ctx<'a> {
        Ctx { s, db, tape: &[], pos: 0, rerun_hint: None, stop: None, acks: 0, forced_seen: 0, fid: [Fidelity::Agent; 2], fid_map: None, subs: &[], last_alt: None }
    }
    /// A context over a scratch state that answers decisions from `tape` (option indices, in
    /// order) and halts with `Need` past its end. Scenario loading (`scenario_stack`) runs the
    /// engine's own target/mode/division logic with it, answering from the scenario; tests run one
    /// engine action on a copy of a state with it.
    pub fn scripted(s: &'a mut State, db: &'a CardDb, tape: &'a [u16]) -> Ctx<'a> {
        Ctx { s, db, tape, pos: 0, rerun_hint: None, stop: None, acks: 0, forced_seen: 0, fid: [Fidelity::Agent; 2], fid_map: None, subs: &[], last_alt: None }
    }
    /// Ask `d.player` to pick one of `d.options`. Single-option decisions are forced and never
    /// surfaced (and not recorded on the tape), except a forced priority pass of the `stop`
    /// player at the live frontier (all tape choices consumed), which halts as a pending decision
    /// until acknowledged (`Game::submit` on it pushes nothing to the tape).
    pub fn choose(&mut self, d: Decision) -> R<Opt> {
        self.choose_sub(d, false)
    }

    /// `choose`; `set_key`: the caller checks a strict merged alternative against its merge key
    /// (`choose_set`). A substituted choice (`Game::submit_alt`) returns the alternative and
    /// records the pair in `last_alt` (`offered_ix`, `swap_alt`).
    fn choose_sub(&mut self, d: Decision, set_key: bool) -> R<Opt> {
        self.last_alt = None;
        // A game number left the bound (`num`): nothing past it is decided or shown.
        if let Some(m) = &self.s.overflow {
            return Err(Halt::Unsupported(m.clone()));
        }
        match d.options.len() {
            0 => Err(Halt::Unsupported(format!("decision {:?} with no options", d.kind))),
            1 => {
                if self.stop == Some(d.player) && d.kind == DecisionKind::Priority && self.pos == self.tape.len() {
                    self.forced_seen += 1;
                    if self.forced_seen > self.acks {
                        return Err(Halt::Need(d));
                    }
                }
                Ok(d.options[0])
            }
            n => {
                if self.pos < self.tape.len() {
                    let c = self.tape[self.pos] as usize;
                    let at = self.pos;
                    self.pos += 1;
                    if c >= n {
                        return Err(Halt::BadChoice);
                    }
                    if !self.subs.is_empty() {
                        if let Some(sub) = self.subs.iter().find(|x| x.pos as usize == at) {
                            if sub.offered != d.options[c] {
                                return Err(Halt::Unsupported(format!("{ALT_REFUSED} {:?} is not the option chosen at {:?}", sub.offered, d.kind)));
                            }
                            if sub.strict && !set_key {
                                return Err(Halt::Unsupported(format!("{ALT_REFUSED} strict alternative at a {:?} decision without a merge key", d.kind)));
                            }
                            self.last_alt = Some((sub.offered, sub.alt));
                            return Ok(sub.alt);
                        }
                    }
                    Ok(d.options[c])
                } else {
                    Err(Halt::Need(d))
                }
            }
        }
    }

    /// The index in `opts` of the offered option choice `c` stands for: `c`'s own, unless `c` is
    /// a substituted merged alternative (`Game::submit_alt`), then the option it was merged into.
    /// For pick loops that map a choice back to their candidate list.
    pub fn offered_ix(&self, c: Opt, opts: &[Opt]) -> usize {
        let o = match self.last_alt {
            Some((o, a)) if a == c => o,
            _ => c,
        };
        opts.iter().position(|x| *x == o).unwrap()
    }

    /// A pick loop resolved choice `c` to its candidate `items[i]` (`offered_ix`). If `c` is a
    /// substituted merged alternative, it must be a later candidate (`is_alt`) of the same
    /// canonical list: it is swapped into slot `i`, so the loop acts on it and offers the original
    /// later (the canonical order among identical candidates is all that changes). Otherwise the
    /// unit fails loudly (`ALT_REFUSED`). No-op in play.
    pub fn swap_alt<T>(&self, c: Opt, i: usize, items: &mut [T], is_alt: impl Fn(&T) -> bool) -> R<()> {
        match self.last_alt {
            Some((o, a)) if a == c && o != a => match (i + 1..items.len()).find(|&j| is_alt(&items[j])) {
                Some(j) => {
                    items.swap(i, j);
                    Ok(())
                }
                None => Err(Halt::Unsupported(format!("{ALT_REFUSED} {a:?} is not a later candidate than {o:?}"))),
            },
            _ => Ok(()),
        }
    }
    /// Is `p` a full-fidelity seat in this unit? Callers compute full option lists only then.
    #[inline]
    pub fn full(&self, p: PlayerId) -> bool {
        self.fid[p.idx()] == Fidelity::Full
    }

    /// A decision with an agent-mode form `agent` and, for a full-fidelity seat, a full form
    /// `full` (a superset of `agent.options`; `None`, equal options or an agent seat: plain
    /// [`Ctx::choose`]). Tape encoding (so agent-mode records and the all-defaults game are
    /// unchanged): a full choice that is one of ≥ 2 agent options is recorded as its agent index;
    /// any other full choice (an extra option, or a decision that agent mode takes without
    /// asking) as `FULL_TAG | full index`. Replay with the same fidelity reads both unambiguously.
    pub fn choose_full(&mut self, agent: Decision, full: Option<Vec<Opt>>) -> R<Opt> {
        let fopts = match full {
            Some(f) if self.full(agent.player) && f != agent.options => f,
            _ => return self.choose(agent),
        };
        if let Some(o) = agent.options.iter().find(|o| !fopts.contains(o)) {
            return Err(Halt::Unsupported(format!("full-fidelity {:?} lacks the agent option {o:?}", agent.kind)));
        }
        match fopts.len() {
            0 => return Err(Halt::Unsupported(format!("full-fidelity decision {:?} with no options", agent.kind))),
            1 => return Ok(fopts[0]),
            _ => {}
        }
        let a = &agent.options;
        if self.pos < self.tape.len() {
            let e = self.tape[self.pos];
            self.pos += 1;
            let o = if e & FULL_TAG != 0 {
                fopts.get((e & !FULL_TAG) as usize)
            } else if a.len() >= 2 {
                a.get(e as usize)
            } else {
                None
            };
            return o.copied().ok_or(Halt::BadChoice);
        }
        let entry = fopts
            .iter()
            .enumerate()
            .map(|(i, o)| match a.iter().position(|x| x == o) {
                Some(k) if a.len() >= 2 => k as u16,
                _ => FULL_TAG | i as u16,
            })
            .collect();
        let agent_ix = a.iter().map(|o| fopts.iter().position(|x| x == o).unwrap() as u16).collect();
        self.fid_map = Some(FullMap { entry, agent: agent_ix });
        Err(Halt::Need(Decision { options: fopts, ..agent }))
    }

    /// [`Ctx::choose_set`] of exactly one item for a full-fidelity seat: the agent form is
    /// `choose_set(agent, 1, 1)`'s decision (items sorted by (signature, id), one per signature),
    /// the full form the same over `full` (agent items first, so it is a superset), posed through
    /// [`Ctx::choose_full`]. Only for a full seat (agent seats call `choose_set`); `full` must not
    /// be empty. The observation context (`ext6::Pick::chosen`) ends cleared, as `choose_set` leaves it.
    pub fn choose_one_full<K: Ord + Copy>(&mut self, player: PlayerId, kind: DecisionKind, agent: Vec<(K, Opt)>, full: Vec<(K, Opt)>, subject: Option<ObjId>) -> R<Opt> {
        let canon = |mut items: Vec<(K, Opt)>| -> Vec<(K, Opt)> {
            items.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| opt_key(&a.1).cmp(&opt_key(&b.1))));
            items.dedup_by(|later, first| later.0 == first.0);
            items
        };
        let ak = canon(agent);
        let a: Vec<Opt> = ak.iter().map(|x| x.1).collect();
        let mut f = a.clone();
        // one option per signature: a class the agent form offers is not offered again
        f.extend(canon(full).into_iter().filter(|x| !ak.iter().any(|y| y.0 == x.0)).map(|x| x.1));
        if !self.s.ext.x6.pick.chosen.is_empty() {
            self.s.ext.x6.pick.chosen.clear();
        }
        self.choose_full(Decision { player, kind, options: a, subject }, Some(f))
    }

    /// Choose a set of `min..=max` items, one at a time, without meaningless permutations:
    /// items are ordered by (signature, id) and each pick must come after the previous one, so
    /// every set (multiset, for identical items with equal signatures) is reachable exactly once.
    /// Identical items are deduplicated; an item is offered only if `min` can still be reached.
    /// `choose_set` where each later pick must be compatible (`compat(state, chosen, candidate)`)
    /// with every earlier one ("up to two target cards from a single graveyard", `ext9`). Only
    /// for `min` 0 (the remaining-count check of `choose_set` ignores compatibility).
    pub fn choose_set_compat<K: Ord + Copy>(&mut self, player: PlayerId, kind: DecisionKind, mut items: Vec<(K, Opt)>, min: usize, max: usize, subject: Option<ObjId>, compat: &dyn Fn(&State, &Opt, &Opt) -> bool) -> R<Vec<Opt>> {
        if min > 0 {
            return self.unsupported("compatible set choice with a minimum");
        }
        items.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| opt_key(&a.1).cmp(&opt_key(&b.1))));
        let mut chosen: Vec<Opt> = vec![];
        let mut next = 0usize;
        while chosen.len() < max {
            let mut opts = vec![Opt::Done];
            let mut pos_of = vec![];
            let mut last_sig: Option<K> = None;
            for (i, (k, o)) in items.iter().enumerate().skip(next) {
                if last_sig == Some(*k) || !chosen.iter().all(|c| compat(&*self.s, c, o)) {
                    continue;
                }
                last_sig = Some(*k);
                opts.push(*o);
                pos_of.push(i);
            }
            if opts.len() == 1 {
                break;
            }
            let c = self.choose_sub(Decision { player, kind, options: opts.clone(), subject }, true)?;
            if c == Opt::Done {
                break;
            }
            let idx = self.offered_ix(c, &opts) - 1;
            self.swap_alt_keyed(c, pos_of[idx], &mut items)?;
            next = pos_of[idx] + 1;
            chosen.push(c);
        }
        Ok(chosen)
    }

    /// `swap_alt` for a set choice's keyed items: the alternative must be a later item with the
    /// same merge key (an item the decision merged into the offered one).
    fn swap_alt_keyed<K: Ord + Copy>(&self, c: Opt, i: usize, items: &mut [(K, Opt)]) -> R<()> {
        let k = items[i].0;
        self.swap_alt(c, i, items, |x| x.1 == c && x.0 == k)
    }

    pub fn choose_set<K: Ord + Copy>(&mut self, player: PlayerId, kind: DecisionKind, mut items: Vec<(K, Opt)>, min: usize, max: usize, subject: Option<ObjId>) -> R<Vec<Opt>> {
        items.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| opt_key(&a.1).cmp(&opt_key(&b.1))));
        let mut chosen = vec![];
        let mut next = 0usize;
        while chosen.len() < max {
            let mut opts = vec![];
            if chosen.len() >= min {
                opts.push(Opt::Done);
            }
            let mut last_sig: Option<K> = None;
            let mut pos_of = vec![];
            for (i, (k, o)) in items.iter().enumerate().skip(next) {
                if last_sig == Some(*k) {
                    continue;
                }
                // After picking position i, positions i+1.. remain.
                let remaining_after = items.len() - i - 1;
                if chosen.len() + 1 + remaining_after < min {
                    continue;
                }
                last_sig = Some(*k);
                opts.push(*o);
                pos_of.push(i);
            }
            if opts.is_empty() {
                break;
            }
            // Observation context (`ext6::Pick::chosen`): only where a decision can surface
            // (the live frontier), so replays never touch the state.
            if self.pos >= self.tape.len() && opts.len() > 1 {
                let pick = &mut self.s.ext.x6.pick.chosen;
                pick.clear();
                pick.extend(chosen.iter().filter_map(|o| match *o {
                    Opt::Target(t) => Some(t),
                    Opt::Card(x) => Some(Target::Obj(x)),
                    _ => None,
                }));
            }
            let c = self.choose_sub(Decision { player, kind, options: opts.clone(), subject }, true)?;
            if c == Opt::Done {
                break;
            }
            let idx = self.offered_ix(c, &opts) - (chosen.len() >= min) as usize;
            self.swap_alt_keyed(c, pos_of[idx], &mut items)?;
            next = pos_of[idx] + 1;
            chosen.push(c);
        }
        if !self.s.ext.x6.pick.chosen.is_empty() {
            self.s.ext.x6.pick.chosen.clear();
        }
        Ok(chosen)
    }

    /// The options of the priority decision `p` faces, if this is the re-run of a unit that
    /// halted at exactly that decision as its first surfaced choice from the same committed state
    /// (`Game::rerun_hint`): the re-run is deterministic up to there, so they are identical.
    pub fn first_options(&self, p: PlayerId) -> Option<Vec<Opt>> {
        let h = self.rerun_hint?;
        (self.pos == 0 && !self.tape.is_empty() && h.player == p).then(|| h.options.clone())
    }

    pub fn unsupported<T>(&self, msg: impl Into<String>) -> R<T> {
        Err(Halt::Unsupported(msg.into()))
    }

    /// `Opt::Number(lo..=hi)`, one option per integer, for announcements whose range is a game
    /// number (pay X life, -X loyalty, a divided total). Nothing in the CR bounds those numbers
    /// (exponential counter growth), so past `MAX_NUMBER_OPTIONS` this fails loudly
    /// instead of allocating an unbounded option list. Combat damage assignment has its own
    /// bounded offer (`combat::damage_amounts`).
    pub fn number_options(&self, lo: i32, hi: i32, what: &str) -> R<Vec<Opt>> {
        if hi as i64 - lo as i64 + 1 > MAX_NUMBER_OPTIONS as i64 {
            return self.unsupported(format!("{what}: {lo}..={hi} is more than {MAX_NUMBER_OPTIONS} amounts to choose from"));
        }
        Ok((lo..=hi).map(Opt::Number).collect())
    }
}

/// Most options a per-integer announcement may list (`Ctx::number_options`).
pub const MAX_NUMBER_OPTIONS: i32 = 4096;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Deck {
    pub main: Vec<CardId>,
    pub side: Vec<CardId>,
}

/// A game in progress. Cloning is cheap apart from the log (shared via `Arc`-free Vec; use
/// `clone_for_search` to skip it).
#[derive(Clone)]
pub struct Game {
    pub db: Arc<CardDb>,
    pub s: State,
    tape: Vec<u16>,
    pending: Option<Decision>,
    error: Option<String>,
    /// Every choice submitted (excluding forced ones): with the seed and decks, a full replay.
    pub history: Vec<u16>,
    pub log: Vec<Event>,
    pub keep_log: bool,
    pub stats: Stats,
    /// Reusable scratch buffer for units (avoids reallocating on every unit).
    scratch: Option<Box<State>>,
    /// Set when the pending decision is a priority decision posed as the first surfaced choice
    /// of its unit: the re-run after `submit` takes its options from here instead of
    /// enumerating them again, provided the committed state is still equal to `state`.
    rerun_hint: Option<std::sync::Arc<RerunHint>>,
    /// Opt-in, for a human seat in a front end (`set_stop_seat`; None by default, never set for
    /// agents or in search): this player's forced priority passes (only option: Pass) surface as pending
    /// one-option decisions instead of being taken silently. Acknowledging one (`submit(0)`)
    /// records nothing: the tape/history, the state and every other decision are exactly as
    /// without stops, so records replay without it.
    stop_seat: Option<PlayerId>,
    /// Forced stops acknowledged in the current unit (reset when a unit completes or a real
    /// choice is submitted): the unit re-runs from the committed state and passes that many.
    acks: usize,
    /// Each player's sideboard as registered for this game (CR 100.4a; (card, copies), sorted by
    /// card id), minus the copies the game holds as sideboard objects (`Player::sideboard`, wish
    /// games): `observe` adds those back. The player's own knowledge (their 75), shown only to
    /// them (`observe::OwnDeckView`, v9). Empty for built states (`from_state`).
    side: Arc<[Vec<(CardId, u8)>; 2]>,
    /// Per-seat fidelity requested (`set_fidelity`); applied from the next unit start.
    fidelity: [Fidelity; 2],
    /// Fidelity of the unit in progress (fixed while its tape is non-empty, so re-runs read the
    /// tape the same way).
    unit_fid: [Fidelity; 2],
    /// The pending decision's full-fidelity map, when it is a full-fidelity decision.
    pending_map: Option<FullMap>,
    /// Every tape entry submitted (`history` holds the submitted option indices); with agent
    /// seats only the two are equal. See `agent_history`.
    pub tape_history: Vec<u16>,
    /// Merged alternatives substituted in the unit in progress (`submit_alt`; empty in play).
    subs: Vec<AltSub>,
}

/// See `Game::rerun_hint`.
pub struct RerunHint {
    state: State,
    player: PlayerId,
    options: Vec<Opt>,
}

/// Units [`Game::advance`] may run without reaching a decision or the end of the game before it
/// declares an engine error. Normal play needs far fewer: a whole turn without a choice is ~10–20
/// units and games end at turn 400 (`flow::TURN_LIMIT`).
pub const MAX_UNITS_WITHOUT_DECISION: u32 = 50_000;

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub units: u64,
    pub reruns: u64,
}

impl Game {
    pub fn new(db: Arc<CardDb>, decks: [&Deck; 2], seed: u64) -> Game {
        Game::new_with_starting(db, decks, seed, None)
    }

    /// `new` with the starting player decided outside the game (CR 103.1: in a match, by the
    /// play/draw choice, `bo3::Match`). `None`: drawn from the seed (best-of-one). The seed's
    /// shuffles are the same either way.
    pub fn new_with_starting(db: Arc<CardDb>, decks: [&Deck; 2], seed: u64, starting: Option<PlayerId>) -> Game {
        let s = crate::flow::initial_state_with(&db, decks, seed, starting);
        let side = Arc::new([0usize, 1].map(|p| {
            // copies the state holds as sideboard objects (wish games) are counted live
            let mut live: Vec<CardId> = s.players[p].sideboard.iter().map(|&o| s.obj(o).card).collect();
            let mut out: Vec<CardId> = vec![];
            let mut side = decks[p].side.clone();
            side.sort_unstable();
            for c in side {
                match live.iter().position(|&x| x == c) {
                    Some(i) => {
                        live.swap_remove(i);
                    }
                    None => out.push(c),
                }
            }
            crate::observe::card_counts(&mut out)
        }));
        let mut g = Game { db, s, tape: vec![], pending: None, error: None, history: vec![], log: vec![], keep_log: true, stats: Stats::default(), scratch: None, rerun_hint: None, stop_seat: None, acks: 0, side, fidelity: [Fidelity::Agent; 2], unit_fid: [Fidelity::Agent; 2], pending_map: None, tape_history: vec![], subs: vec![] };
        g.advance();
        g
    }

    /// Start from an arbitrary (e.g. built) state.
    pub fn from_state(db: Arc<CardDb>, s: State) -> Game {
        let mut g = Game { db, s, tape: vec![], pending: None, error: None, history: vec![], log: vec![], keep_log: true, stats: Stats::default(), scratch: None, rerun_hint: None, stop_seat: None, acks: 0, side: Arc::new([vec![], vec![]]), fidelity: [Fidelity::Agent; 2], unit_fid: [Fidelity::Agent; 2], pending_map: None, tape_history: vec![], subs: vec![] };
        g.advance();
        g
    }

    /// Sideboard part of player `p`'s registered 75 not held as game objects (see `side`).
    pub(crate) fn side_static(&self, p: PlayerId) -> &[(CardId, u8)] {
        &self.side[p.idx()]
    }

    /// A copy without the event log/history, for search and analysis.
    pub fn clone_for_search(&self) -> Game {
        Game {
            db: self.db.clone(),
            s: self.s.clone(),
            tape: self.tape.clone(),
            pending: self.pending.clone(),
            error: self.error.clone(),
            history: vec![],
            log: vec![],
            keep_log: false,
            stats: Stats::default(),
            // The paused state is part of what's observable while a decision is pending.
            scratch: if self.pending.is_some() { self.scratch.clone() } else { None },
            rerun_hint: self.rerun_hint.clone(),
            // search copies never stop on forced passes (a pending stop stays pending as is)
            stop_seat: None,
            acks: self.acks,
            side: self.side.clone(),
            // Search plays every seat in agent mode from the next unit on; the unit in progress
            // keeps its fidelity (its tape may hold full-fidelity entries, its pending decision
            // may be a full one).
            fidelity: [Fidelity::Agent; 2],
            unit_fid: if self.tape.is_empty() && self.pending_map.is_none() { [Fidelity::Agent; 2] } else { self.unit_fid },
            pending_map: self.pending_map.clone(),
            tape_history: vec![],
            subs: self.subs.clone(),
        }
    }

    /// Set player `p`'s decision fidelity (see [`Fidelity`]). Takes effect at the next unit start;
    /// if no choice of the current unit has been made yet, the pending decision is posed again
    /// under the new setting (so a game set to full before its first choice is full from the
    /// start, exactly like a replay with the same setting).
    pub fn set_fidelity(&mut self, p: PlayerId, f: Fidelity) {
        self.fidelity[p.idx()] = f;
        if self.tape.is_empty() && self.unit_fid != self.fidelity {
            self.unit_fid = self.fidelity;
            self.rerun_paused();
        }
    }

    pub fn fidelity(&self, p: PlayerId) -> Fidelity {
        self.fidelity[p.idx()]
    }

    /// The pending decision is a full-fidelity form (its options differ from agent mode's).
    pub fn pending_is_full(&self) -> bool {
        self.pending_map.is_some()
    }

    /// For a pending full-fidelity decision: the indices (into its options) of the agent-mode
    /// options, in agent-mode order (one: agent mode takes it without asking; empty: none of the
    /// agent options is offered, e.g. after an earlier non-default choice). `None` for any other
    /// decision. Lets a driver "take the default".
    pub fn pending_agent_indices(&self) -> Option<Vec<usize>> {
        self.pending_map.as_ref().map(|m| m.agent.iter().map(|&i| i as usize).collect())
    }

    /// The agent-mode tape of this game: the tape without full-fidelity-only entries. A game whose
    /// full seats always took agent-mode options has exactly the agent-mode game's history here.
    pub fn agent_history(&self) -> Vec<u16> {
        self.tape_history.iter().copied().filter(|e| e & FULL_TAG == 0).collect()
    }

    /// See `stop_seat`. Takes effect from the next unit run (a pending decision is unchanged).
    pub fn set_stop_seat(&mut self, p: Option<PlayerId>) {
        self.stop_seat = p;
    }

    pub fn stop_seat(&self) -> Option<PlayerId> {
        self.stop_seat
    }

    /// The pending decision is a forced stop (one option, see `stop_seat`), not a real choice.
    pub fn pending_is_forced(&self) -> bool {
        self.pending.as_ref().is_some_and(|d| d.options.len() == 1)
    }

    /// The state players see: while a decision is pending this is the paused mid-unit state
    /// (e.g. the spell being cast is already on the stack, scried cards are known, cards drawn
    /// earlier in the resolution are in hand). Otherwise the committed state.
    pub fn view_state(&self) -> &State {
        match (&self.pending, &self.scratch) {
            (Some(_), Some(sc)) => sc,
            _ => &self.s,
        }
    }

    /// Mutable access to the observable state (for tests that perturb hidden information).
    pub fn view_state_mut(&mut self) -> &mut State {
        match (&self.pending, &mut self.scratch) {
            (Some(_), Some(sc)) => sc,
            _ => &mut self.s,
        }
    }

    /// Re-run the paused unit from the committed state with the current choice tape (after the
    /// committed state was edited, e.g. by `determinize`): the paused view is rebuilt from it.
    pub fn rerun_paused(&mut self) {
        if self.pending.take().is_some() {
            self.scratch = None;
            self.rerun_hint = None;
            self.pending_map = None;
            self.advance();
        }
    }

    pub fn pending(&self) -> Option<&Decision> {
        self.pending.as_ref()
    }
    pub fn result(&self) -> Option<GameResult> {
        self.s.result
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn is_over(&self) -> bool {
        self.s.result.is_some() || self.error.is_some()
    }

    pub fn submit(&mut self, choice: usize) -> Result<(), String> {
        let d = self.pending.as_ref().ok_or("no pending decision")?;
        if choice >= d.options.len() {
            return Err(format!("choice {choice} out of range ({} options)", d.options.len()));
        }
        if d.options.len() == 1 {
            // acknowledging a forced stop: not a choice, nothing recorded; re-run the unit
            self.acks += 1;
            self.pending = None;
            self.pending_map = None;
            self.advance();
            return Ok(());
        }
        self.acks = 0;
        let entry = match self.pending_map.take() {
            Some(m) => m.entry[choice],
            None => choice as u16,
        };
        self.tape.push(entry);
        self.history.push(choice as u16);
        self.tape_history.push(entry);
        self.pending = None;
        self.advance();
        Ok(())
    }

    /// Answer the pending decision with `alt`, an option it merged into its option `choice`
    /// (an identical object in place of the one offered: `scenario::merged_alternatives`), for
    /// the canonicalisation soundness test. The unit re-runs with
    /// `alt` at this tape position: pick loops over canonical candidate lists swap it with the
    /// offered candidate (`Ctx::swap_alt`), a set choice also checks that its merge key says the
    /// two are identical. `strict`: refused wherever no merge key is checked. A refused
    /// alternative ends the game with an `ALT_REFUSED` engine error. The tape records `choice`, so
    /// `history` doesn't replay such a game.
    pub fn submit_alt(&mut self, choice: usize, alt: Opt, strict: bool) -> Result<(), String> {
        let d = self.pending.as_ref().ok_or("no pending decision")?;
        let offered = *d.options.get(choice).ok_or("choice out of range")?;
        if d.options.len() < 2 || self.pending_map.is_some() {
            return Err("submit_alt: only at an agent-mode decision with two or more options".into());
        }
        self.subs.push(AltSub { pos: self.tape.len() as u16, offered, alt, strict });
        self.submit(choice)
    }

    /// Choices made so far in the unit in progress (the pending decision's position on the
    /// choice tape): 0 when the pending decision is the first of its unit.
    pub fn unit_choices(&self) -> usize {
        self.tape.len()
    }

    /// Run units until a decision is needed or the game ends.
    fn advance(&mut self) {
        let mut units = 0u32;
        loop {
            if self.s.result.is_some() || self.error.is_some() {
                return;
            }
            units += 1;
            if units > MAX_UNITS_WITHOUT_DECISION {
                // A rules loop that asks nobody anything (e.g. two "exile until this leaves"
                // permanents re-triggering each other) would otherwise never return: fail loudly
                // and deterministically, as an engine error (callers drop / count error games).
                self.error = Some(format!("engine loop: {MAX_UNITS_WITHOUT_DECISION} units without a decision (turn {}, step {:?}, stack {})",
                                          self.s.turn.number, self.s.turn.step, self.s.stack.len()));
                return;
            }
            let mut scratch = match self.scratch.take() {
                Some(mut b) => {
                    b.copy_from(&self.s);
                    b
                }
                None => Box::new(self.s.clone()),
            };
            // The hint applies to the first run after `submit` only, and only if the committed
            // state is exactly the one the hinted decision was posed from.
            let hint = self.rerun_hint.take().filter(|h| !self.tape.is_empty() && h.state == self.s);
            if self.tape.is_empty() && self.acks == 0 {
                self.unit_fid = self.fidelity;
            }
            let (r, fid_map) = {
                let mut cx = Ctx { s: &mut scratch, db: &self.db, tape: &self.tape, pos: 0, rerun_hint: hint.as_deref(), stop: self.stop_seat, acks: self.acks, forced_seen: 0, fid: self.unit_fid, fid_map: None, subs: &self.subs, last_alt: None };
                let r = crate::flow::unit(&mut cx);
                (r, cx.fid_map)
            };
            self.stats.units += 1;
            // A unit that ran past a game-number overflow (`num`) is never committed.
            let r = match (r, &scratch.overflow) {
                (Ok(()) | Err(Halt::GameOver), Some(m)) => Err(Halt::Unsupported(m.clone())),
                (r, _) => r,
            };
            match r {
                Ok(()) | Err(Halt::GameOver) => {
                    let ev = std::mem::take(&mut scratch.events);
                    if self.keep_log {
                        self.log.extend(ev);
                    }
                    std::mem::swap(&mut self.s, &mut *scratch);
                    self.scratch = Some(scratch);
                    self.tape.clear();
                    self.subs.clear();
                    self.acks = 0;
                }
                Err(Halt::Need(d)) => {
                    // A full-fidelity decision is never hinted (the hint stands for agent options).
                    self.pending_map = fid_map.filter(|m| m.entry.len() == d.options.len());
                    if self.tape.is_empty() && d.kind == DecisionKind::Priority && self.pending_map.is_none() {
                        self.rerun_hint = Some(std::sync::Arc::new(RerunHint { state: self.s.clone(), player: d.player, options: d.options.clone() }));
                    }
                    self.scratch = Some(scratch);
                    self.stats.reruns += !self.tape.is_empty() as u64;
                    self.pending = Some(d);
                    return;
                }
                Err(Halt::Unsupported(m)) => {
                    self.error = Some(m);
                    return;
                }
                Err(Halt::BadChoice) => {
                    self.error = Some("bad choice on tape (non-deterministic option enumeration?)".into());
                    return;
                }
            }
        }
    }
}

