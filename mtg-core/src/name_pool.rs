//! The universe of names for "choose a card name" (CR 201.4) and the order they're offered in.
//!
//! Universe: `data/card_names.tsv`: every face name (CR 201.4b-f)
//! of every Standard-legal card in the pinned Scryfall Oracle bulk, plus the pinned deck pool
//! and `decks/standard`, each with its face's type line and the number of pool lists playing it.
//! Sorted by name; `Opt::Name(i)` indexes it. It never depends on the decks in the game, so the
//! option set can't reveal the opponent's list. Any card name outside it (a
//! Standard game can't hold one) is outcome-equivalent to a name in it that no player's card has;
//! a game whose cards do include a matching name outside the universe fails loudly instead
//! (`check_covered`).
//!
//! Order (`offer`): first the names of the cards the chooser knows of in this game (either side:
//! public zones, their hand, revealed cards), then the rest; both by pool popularity (public
//! metagame knowledge), then by name. Only the chooser's information shapes the order. Every
//! legal name is offered; the encoding shows an agent the first `encode::O`.

use crate::basics::{CardId, CardType, ObjId, PlayerId};
use crate::carddb::CardDb;
use crate::filter::{Base, Filter, Prop};
use crate::state::{State, Zone};
use std::collections::HashMap;
use std::sync::OnceLock;

pub struct PoolName {
    pub name: String,
    /// Card types of this face (CR 201.4a-f: the named face's own characteristics).
    pub types: crate::basics::TypeSet,
    pub basic: bool,
    pub legendary: bool,
    /// Pool lists (main or side) playing the card.
    pub plays: u32,
}

static POOL: OnceLock<Vec<PoolName>> = OnceLock::new();
static INDEX: OnceLock<HashMap<String, u16>> = OnceLock::new();

pub fn names() -> &'static [PoolName] {
    POOL.get_or_init(|| {
        let src = include_str!("../../../data/card_names.tsv");
        let mut v: Vec<PoolName> = src
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .map(|l| {
                let mut f = l.split('\t');
                let name = f.next().unwrap_or("").to_string();
                let line = f.next().unwrap_or("");
                let plays = f.next().and_then(|x| x.trim().parse().ok()).unwrap_or(0);
                let front = line.split('—').next().unwrap_or("");
                let mut ts = crate::basics::TypeSet::default();
                let (mut basic, mut legendary) = (false, false);
                for w in front.split_whitespace() {
                    if let Some(t) = CardType::parse(w) {
                        ts = ts.with(t);
                    }
                    basic |= w == "Basic";
                    legendary |= w == "Legendary";
                }
                PoolName { name, types: ts, basic, legendary, plays }
            })
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        assert!(v.len() < u16::MAX as usize, "name universe exceeds Opt::Name's u16");
        v
    })
}

/// Name of pool entry `i`.
pub fn name(i: u16) -> &'static str {
    names().get(i as usize).map(|p| p.name.as_str()).unwrap_or("")
}

/// Index of `name` in the universe (case- and diacritic-insensitive, `carddb::name_key`).
pub fn index_of(name: &str) -> Option<u16> {
    INDEX
        .get_or_init(|| names().iter().enumerate().map(|(i, p)| (crate::carddb::name_key(&p.name), i as u16)).collect())
        .get(&crate::carddb::name_key(name))
        .copied()
}

/// Whether a `NameCard` `ValidCards` filter can be decided from a name's face characteristics:
/// base `Card` or a card type, with card-type / basic / legendary properties (and negations).
pub fn filter_supported(valid: &Filter) -> bool {
    fn prop_ok(p: &Prop) -> bool {
        match p {
            Prop::IsType(_) | Prop::NonType(_) | Prop::Basic | Prop::NonBasic | Prop::Legendary | Prop::NonLegendary => true,
            Prop::Not(q) => prop_ok(q),
            _ => false,
        }
    }
    valid.alts.iter().all(|a| matches!(a.base, Base::Card | Base::Type(_)) && a.props.iter().all(prop_ok))
}

/// Does a face with these characteristics satisfy the restriction (CR 201.4a)?
fn face_matches(valid: &Filter, types: crate::basics::TypeSet, basic: bool, legendary: bool) -> bool {
    fn prop(p: &Prop, types: crate::basics::TypeSet, basic: bool, legendary: bool) -> bool {
        match p {
            Prop::IsType(t) => types.has(*t),
            Prop::NonType(t) => !types.has(*t),
            Prop::Basic => basic,
            Prop::NonBasic => !basic,
            Prop::Legendary => legendary,
            Prop::NonLegendary => !legendary,
            Prop::Not(q) => !prop(q, types, basic, legendary),
            _ => false,
        }
    }
    valid.alts.iter().any(|a| {
        let base = match a.base {
            Base::Card => true,
            Base::Type(t) => types.has(t),
            _ => false,
        };
        base && a.props.iter().all(|p| prop(p, types, basic, legendary))
    })
}

/// Does universe entry `n` satisfy the restriction?
pub fn matches(valid: &Filter, n: &PoolName) -> bool {
    face_matches(valid, n.types, n.basic, n.legendary)
}

/// The printed card a card id stands for: itself when it was loaded by name, the front of a
/// transforming card's back face, a Room's printed card for its battlefield state cards (`ext5`).
/// None for engine-made cards that aren't printed cards (the face-down card, CR 708.2a; a token; a
/// permanent that became a new named object, The Irencrag's Everflame): no card name (CR 201.4).
fn printed(db: &CardDb, c: CardId) -> Option<CardId> {
    if let Some(r) = crate::ext5::room_of(db, c) {
        return Some(r[0]);
    }
    let cd = db.get(c);
    if cd.is_token || db.x6.face_down_card == Some(c) {
        return None;
    }
    if db.by_name(&cd.name) == Some(c) {
        return Some(c);
    }
    db.dfc_backs.iter().find(|x| x.1 == c).map(|x| x.0)
}

/// Every face name of the printed card behind `c` (split / Room halves, Adventure and prepare
/// faces, a transforming card's back face), with its characteristics: (name, types, basic,
/// legendary). Empty for a card id that isn't a printed card (`printed`).
fn card_faces(db: &CardDb, c: CardId) -> Vec<(&str, crate::basics::TypeSet, bool, bool)> {
    let mut out = vec![];
    let Some(c) = printed(db, c) else { return out };
    for id in std::iter::once(c).chain(db.back_of(c)) {
        // Not the engine's combined faces of a split card / Room ("A // B", CR 709.4; a Room with
        // both doors locked has no name, 709.5): a name is chosen per half (CR 201.4b).
        for f in db.get(id).faces.iter().filter(|f| !f.name.is_empty() && !f.name.contains(" // ")) {
            out.push((f.name.as_str(), f.types, f.supertypes.basic(), f.supertypes.legendary()));
        }
    }
    out
}

/// The real card of object `id` (a face-down permanent's hidden card, `ext6`).
fn real_card(s: &State, id: ObjId) -> CardId {
    s.ext.x6.face_down.iter().find(|x| x.0 == id).map_or(s.obj(id).card, |x| x.1)
}

/// Fail loudly when a card in the game (any zone, either player, sideboards included) has a face
/// matching the restriction whose name isn't in the universe: it would be unnameable. Uses
/// hidden information only to abort the game, never to shape the options.
pub fn check_covered(s: &State, db: &CardDb, valid: &Filter) -> Result<(), String> {
    for (i, o) in s.objs.iter().enumerate() {
        if o.zone == Zone::Gone {
            continue;
        }
        for (n, t, b, l) in card_faces(db, real_card(s, ObjId(i as u32))) {
            if face_matches(valid, t, b, l) && index_of(n).is_none() {
                return Err(format!("NameCard: {n:?} (a card in this game) is not in the name universe data/card_names.tsv"));
            }
        }
    }
    Ok(())
}

/// Indices of the universe names matching `valid`, in offer order for `chooser` (module docs).
pub fn offer(s: &State, db: &CardDb, chooser: PlayerId, valid: &Filter) -> Vec<u16> {
    let pool = names();
    let mut seen = vec![false; pool.len()];
    let mut note = |n: &str| {
        if let Some(i) = index_of(n) {
            seen[i as usize] = true;
        }
    };
    for (i, o) in s.objs.iter().enumerate() {
        // Known to the chooser (`known_to`): public zones (battlefield, stack, graveyards, face-up
        // exile), their own hand, revealed or looked-at cards, cards exiled face down that they may
        // look at. Not a sideboard (outside the game) nor an unrevealed library card.
        if o.zone == Zone::Gone || o.zone == Zone::Sideboard || o.known_to & (1 << chooser.0) == 0 {
            continue;
        }
        let id = ObjId(i as u32);
        // Its current name(s) (a copy's name, CR 707.9b; none for a face-down permanent, CR
        // 708.2a) and, for a card, every face (the chooser may look at their own face-down
        // permanents, CR 708.6).
        for n in crate::query::names(s, db, id) {
            note(n);
        }
        if !o.token {
            let c = if o.controller == chooser { real_card(s, id) } else { o.card };
            for (n, ..) in card_faces(db, c) {
                note(n);
            }
        }
    }
    let mut v: Vec<u16> = (0..pool.len() as u16).filter(|&i| matches(valid, &pool[i as usize])).collect();
    v.sort_by_key(|&i| (!seen[i as usize], std::cmp::Reverse(pool[i as usize].plays), i));
    v
}
