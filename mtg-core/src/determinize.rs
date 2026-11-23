//! Determinization for search: a copy of a paused game in which every card identity the
//! deciding player can't know is replaced, so search on the copy can't exploit hidden
//! information.
//!
//! - Opponent's hidden cards (hand, library, face-down exile, face-down permanents (CR 708),
//!   sideboard; one pool, since the
//!   observer can't tell which unknown card is where) get identities drawn from a caller-supplied
//!   belief multiset (e.g. a metagame deck model conditioned on the cards seen so far). The
//!   true identities are never read.
//! - Own hidden cards (library, face-down exile) are permuted among themselves: their multiset
//!   is the player's own decklist minus what they have seen, which they know.
//!
//! The edit is made on the committed state and the paused unit is re-run from its choice tape
//! (a mid-unit decision may follow draws or reveals). The result is accepted only if the
//! decider's observation and pending decision are exactly unchanged; otherwise `Err` (the
//! caller should resample or not search).

use crate::basics::*;
use crate::engine::Game;
use crate::observe::observe;
use crate::state::*;

fn hidden_zone(o: &Obj) -> bool {
    matches!(o.zone, Zone::Hand | Zone::Library | Zone::Sideboard) || o.face_down()
}

/// Objects of `owner` whose identity `me` doesn't know in state `s`.
fn hidden_of(s: &State, me: PlayerId, owner: PlayerId) -> Vec<ObjId> {
    let pl = s.player(owner);
    let mut v: Vec<ObjId> = pl.hand.iter().chain(&pl.library).chain(&pl.sideboard).copied().collect();
    v.extend(s.exile.iter().copied().filter(|&o| s.obj(o).owner == owner));
    // Face-down permanents (`ext6`): their identity is hidden from players who can't look.
    if !s.ext.x6.face_down.is_empty() {
        v.extend(s.ext.x6.face_down.iter().map(|x| x.0).filter(|&o| s.obj(o).owner == owner));
    }
    v.retain(|&o| {
        let ob = s.obj(o);
        hidden_zone(ob) && ob.known_to & (1 << me.0) == 0
    });
    v
}

/// Still unknown to `me` in the paused view (or left its zone mid-unit, `Gone`: its new
/// incarnation is re-derived by the re-run and checked through the observation).
fn still_hidden(view: &State, me: PlayerId, o: ObjId) -> bool {
    match view.objs.get(o.idx()) {
        None => true,
        Some(ob) => ob.zone == Zone::Gone || (hidden_zone(ob) && ob.known_to & (1 << me.0) == 0),
    }
}

/// Number of opponent card identities `determinize` will draw from the belief.
pub fn opp_hidden_count(g: &Game, me: PlayerId) -> usize {
    let view = g.view_state();
    hidden_of(&g.s, me, me.opp()).into_iter().filter(|&o| still_hidden(view, me, o)).count()
}

/// See the module docs. `belief`: card ids to draw the opponent's hidden cards from (a
/// multiset; shuffled, then used in order, cycling if shorter than needed; must be non-empty if
/// anything is hidden). `seed` drives the shuffles.
pub fn determinize(g: &Game, me: PlayerId, belief: &[CardId], seed: u64) -> Result<Game, String> {
    determinize_opts(g, me, belief, seed, true)
}

/// `determinize` with `permute_own = false`: only the opponent's hidden cards are replaced
/// (tests use this to build an alternative "true" game consistent with `me`'s observation).
pub fn determinize_opts(g: &Game, me: PlayerId, belief: &[CardId], seed: u64, permute_own: bool) -> Result<Game, String> {
    determinize_impl(g, me, Some(belief), seed, permute_own, false)
}

/// Common-random-numbers determinization (search value smoothing):
/// like `determinize`, but the world is a deterministic function of (`seed`, what `me` knows)
/// alone, and it changes little when that knowledge changes a little, so consecutive searches
/// of one game that reuse a seed see (nearly) the same worlds.
///
/// - Every copy of a card in a pool (the belief multiset for the opponent's hidden cards; the
///   multiset of `me`'s own hidden identities, which `me` knows: decklist minus seen) gets a
///   pseudo-random key `mix(seed, pool, card, copy index)`; the pool sorted by key is dealt to
///   the hidden objects in their zone order (as `determinize` deals). Keys are i.i.d. per seed,
///   so each world is a uniform shuffle as in `determinize`; removing one copy (a card is seen)
///   keeps the other copies' relative order. The input order of a pool is never used (sorted
///   first): `determinize` shuffles `me`'s own hidden cards starting from their TRUE order, which
///   is uniform per call but makes a fixed-seed world depend on that hidden order.
/// - The engine RNG is reseeded from `mix(seed)`.
pub fn determinize_stable(g: &Game, me: PlayerId, belief: &[CardId], seed: u64) -> Result<Game, String> {
    determinize_impl(g, me, Some(belief), seed, true, true)
}

fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// `cards` (any order) as a multiset, ordered by per-copy keys (`determinize_stable`).
fn stable_order(cards: &[CardId], seed: u64, pool: u64) -> Vec<CardId> {
    let mut v: Vec<CardId> = cards.to_vec();
    v.sort_unstable_by_key(|c| c.0);
    let base = mix64(seed ^ mix64(pool));
    let mut keyed: Vec<(u64, u32, u32)> = Vec::with_capacity(v.len());
    let mut copy = 0u32;
    for (i, c) in v.iter().enumerate() {
        copy = if i > 0 && v[i - 1] == *c { copy + 1 } else { 0 };
        keyed.push((mix64(mix64(base ^ c.0 as u64) ^ copy as u64), c.0, copy));
    }
    keyed.sort_unstable();
    keyed.into_iter().map(|(_, c, _)| CardId(c)).collect()
}

/// DIAGNOSTIC ONLY (reads hidden information; never for play): like `determinize`, but the
/// opponent's hand keeps its TRUE identities and only the opponent's hidden library is permuted
/// among itself (= resampled from the true list's remaining cards); their other hidden cards
/// (face-down, sideboard) keep their true identities too. An upper bound on what a better hand
/// model could buy search.
pub fn determinize_oracle_hand(g: &Game, me: PlayerId, seed: u64) -> Result<Game, String> {
    determinize_impl(g, me, None, seed, true, false)
}

/// `belief` None = oracle hand (`determinize_oracle_hand`); `stable`: `determinize_stable`.
fn determinize_impl(g: &Game, me: PlayerId, belief: Option<&[CardId]>, seed: u64, permute_own: bool, stable: bool) -> Result<Game, String> {
    let Some(d0) = g.pending().cloned() else { return Err("no pending decision".into()) };
    if d0.player != me {
        return Err("determinize for the deciding player only".into());
    }
    let obs0 = observe(g, me);
    let mut rng = Rng::new(seed ^ 0x5EA2_C4D3_7E11_0B5D);
    let view = g.view_state();
    let opp: Vec<ObjId> = hidden_of(&g.s, me, me.opp()).into_iter().filter(|&o| still_hidden(view, me, o)).collect();
    // Own hidden cards: only those still unknown in the view (a card drawn mid-unit keeps its
    // identity, which the observer now knows).
    let own: Vec<ObjId> = if !permute_own { vec![] } else { hidden_of(&g.s, me, me).into_iter().filter(|&o| view.objs.get(o.idx()).is_some_and(|ob| ob.zone != Zone::Gone) && still_hidden(view, me, o)).collect() };
    let mut h = g.clone_for_search();
    match belief {
        Some(belief) => {
            if !opp.is_empty() {
                if belief.is_empty() {
                    return Err("empty belief".into());
                }
                let cards = if stable {
                    stable_order(belief, seed, 1)
                } else {
                    let mut cards = belief.to_vec();
                    rng.shuffle(&mut cards);
                    cards
                };
                for (k, &o) in opp.iter().enumerate() {
                    crate::ext6::set_identity(&mut h.s, o, cards[k % cards.len()]);
                }
            }
        }
        None => {
            // oracle hand: hand untouched (true); the hidden library's true identities shuffled among it
            let lib: Vec<ObjId> = opp.iter().copied().filter(|&o| g.s.obj(o).zone == Zone::Library).collect();
            let mut cards: Vec<CardId> = lib.iter().map(|&o| crate::ext6::identity(&h.s, o)).collect();
            rng.shuffle(&mut cards);
            for (&o, c) in lib.iter().zip(cards) {
                crate::ext6::set_identity(&mut h.s, o, c);
            }
        }
    }
    let mut cards: Vec<CardId> = own.iter().map(|&o| crate::ext6::identity(&h.s, o)).collect();
    if stable {
        cards = stable_order(&cards, seed, 2);
    } else {
        rng.shuffle(&mut cards);
    }
    for (&o, c) in own.iter().zip(cards) {
        crate::ext6::set_identity(&mut h.s, o, c);
    }
    // Hidden state other than card identities: the RNG of future shuffles/random choices is
    // reseeded too (it is not a function of anything the observer knows).
    let fresh = if stable {
        Rng::new(mix64(seed ^ mix64(3)))
    } else {
        Rng::new(rng.below(u32::MAX) as u64 ^ ((rng.below(u32::MAX) as u64) << 32))
    };
    h.s.rng = fresh;
    h.rerun_paused();
    if h.error().is_some() {
        return Err(format!("re-run failed: {}", h.error().unwrap()));
    }
    if h.pending() != Some(&d0) {
        return Err("re-run changed the pending decision".into());
    }
    if observe(&h, me) != obs0 {
        return Err("re-run changed the observation".into());
    }
    Ok(h)
}
