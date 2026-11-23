//! Helpers for tests, fuzzing and benchmarks.
use crate::carddb::CardDb;
use crate::engine::{Deck, Game};
use crate::basics::{CardId, Rng};
use std::path::PathBuf;
use std::sync::Arc;

/// Forge's cardsfolder (`$MTG_DATA`, default data). Tests that need it skip when it is
/// missing (`if !cardsfolder().is_dir() { return; }`), which passes silently; with
/// `MTG_REQUIRE_DATA=1` a missing folder panics instead, so a
/// test run without card data fails loudly rather than skipping those tests.
pub fn cardsfolder() -> PathBuf {
    let base = std::env::var("MTG_DATA").unwrap_or_else(|_| "data".into());
    let dir = PathBuf::from(base).join("forge/repo/forge-gui/res/cardsfolder");
    if std::env::var("MTG_REQUIRE_DATA").is_ok_and(|v| v == "1") && !dir.is_dir() {
        panic!("MTG_REQUIRE_DATA=1 but the Forge cardsfolder is missing: {}", dir.display());
    }
    dir
}

/// Two simple decks of classic cards exercising the implemented core (combat keywords, burn,
/// counters, pump, removal, scry, draw, mana creatures).
pub const SYNTH_A: &str = "12 Mountain\n12 Forest\n4 Llanowar Elves\n4 Grizzly Bears\n4 Lightning Bolt\n4 Giant Growth\n4 Boggart Brute\n4 Colossal Dreadmaw\n4 Prodigal Pyromancer\n4 Hill Giant\n4 Shock\n";
pub const SYNTH_B: &str = "12 Island\n12 Plains\n4 Serra Angel\n4 Counterspell\n4 Divination\n4 Opt\n4 Typhoid Rats\n4 Swamp\n4 Wall of Stone\n4 Inspiring Cleric\n4 Youthful Knight\n";

pub fn synth_db_and_decks() -> (Arc<CardDb>, Deck, Deck) {
    use crate::deck::DeckList;
    let a = DeckList::parse("synth_a", SYNTH_A);
    let b = DeckList::parse("synth_b", SYNTH_B);
    let names: Vec<&str> = a.card_names().into_iter().chain(b.card_names()).collect();
    let db = CardDb::load_forge(&cardsfolder(), &names).expect("load cards");
    let uns: Vec<_> = a.unsupported(&db).into_iter().chain(b.unsupported(&db)).collect();
    assert!(uns.is_empty(), "synthetic deck cards unsupported: {uns:?}");
    let (da, dbk) = (a.resolve(&db).unwrap(), b.resolve(&db).unwrap());
    (Arc::new(db), da, dbk)
}

/// Play a game with uniformly random choices. Returns (game, decisions made).
pub fn random_game(db: Arc<CardDb>, a: &Deck, b: &Deck, seed: u64) -> (Game, usize) {
    let mut g = Game::new(db, [a, b], seed);
    let mut rng = Rng::new(seed ^ 0xA5A5_5A5A);
    let mut n = 0;
    while let Some(d) = g.pending() {
        let k = d.options.len() as u32;
        g.submit(rng.below(k) as usize).unwrap();
        n += 1;
    }
    (g, n)
}

/// `random_game` that stops after `bound` decisions (the game is then unfinished: `result()` is
/// None), so a never-ending game fails a test instead of hanging it.
pub fn random_game_bounded(db: Arc<CardDb>, a: &Deck, b: &Deck, seed: u64, bound: usize) -> (Game, usize) {
    let mut g = Game::new(db, [a, b], seed);
    let mut rng = Rng::new(seed ^ 0xA5A5_5A5A);
    let mut n = 0;
    while let Some(d) = g.pending() {
        if n >= bound {
            break;
        }
        let k = d.options.len() as u32;
        g.submit(rng.below(k) as usize).unwrap();
        n += 1;
    }
    (g, n)
}

/// Zone bookkeeping of a committed state (`scenario::check_zones`): every library, hand,
/// graveyard, sideboard (outside the game, CR 400.11), battlefield, exile and stack-spell list
/// holds exactly the objects whose zone says so, once, with the right owner; `Gone` objects are
/// in no list. At a priority decision (state-based actions done, CR 117.5) also that attached
/// permanents are attached to permanents on the battlefield (704.5m–n), in the state the
/// decision is posed from. Panics with `what` (seed, decision index) on a violation.
pub fn assert_zones(g: &Game, what: impl std::fmt::Display) {
    let mut r = crate::scenario::check_zones(&g.s, &g.db, false);
    if r.is_ok() && g.pending().is_some_and(|d| d.kind == crate::engine::DecisionKind::Priority) {
        r = crate::scenario::check_zones(g.view_state(), &g.db, true);
    }
    if let Err(e) = r {
        panic!("{what}: zones: {e} (turn {}, step {:?})", g.s.turn.number, g.s.turn.step);
    }
}

/// `random_game_bounded` that also checks the zone partition (`assert_zones`) after every
/// decision and at the end (fuzz tests).
pub fn random_game_checked(db: Arc<CardDb>, a: &Deck, b: &Deck, seed: u64, bound: usize) -> (Game, usize) {
    let mut g = Game::new(db, [a, b], seed);
    let mut rng = Rng::new(seed ^ 0xA5A5_5A5A);
    let mut n = 0;
    assert_zones(&g, format_args!("seed {seed}, start"));
    while let Some(d) = g.pending() {
        if n >= bound {
            break;
        }
        let k = d.options.len() as u32;
        g.submit(rng.below(k) as usize).unwrap();
        n += 1;
        assert_zones(&g, format_args!("seed {seed}, decision {n}"));
    }
    (g, n)
}

/// A DB containing just these cards (panics if any is unsupported).
pub fn db_with(names: &[&str]) -> Arc<CardDb> {
    let db = CardDb::load_forge(&cardsfolder(), names).expect("load cards");
    for n in names {
        let c = db.get(db.by_name(n).unwrap());
        assert!(c.playable(), "{n} unsupported: {:?}", c.unsupported);
    }
    Arc::new(db)
}

/// Human-readable option, for test failure messages and debugging.
pub fn describe(g: &Game, o: &crate::engine::Opt) -> String {
    use crate::engine::Opt::*;
    let n = |id: crate::basics::ObjId| g.db.get(g.view_state().obj(id).card).name.clone();
    match *o {
        Card(x) => format!("Card[{}]", n(x)),
        PlayLand(x) => format!("PlayLand[{}]", n(x)),
        Cast(x) => format!("Cast[{}]", n(x)),
        CastFace(x, f) => format!("CastFace[{} {}]", n(x), g.db.get(g.view_state().obj(x).card).face(f).name),
        CastPrepared(x) => format!("CastPrepared[{} {}]", n(x), g.db.get(g.view_state().obj(x).card).face(1).name),
        Activate(x, _) => format!("Activate[{}]", n(x)),
        Mana(x, _, t) => format!("Mana[{} {:?}]", n(x), t),
        Attack(x, _) => format!("Attack[{}]", n(x)),
        AttackPw(x, pw) => format!("AttackPw[{} -> {}]", n(x), n(pw)),
        Block(b, a) => format!("Block[{} -> {}]", n(b), n(a)),
        Block2(b, c, a) => format!("Block2[{} + {} -> {}]", n(b), n(c), n(a)),
        Target(crate::state::Target::Obj(x)) => format!("Target[{}]", n(x)),
        // As an object target, plus which stack object (old tests pick abilities by source name).
        Target(crate::state::Target::Ability { uid, src }) => format!("Target[{}] (ability {uid})", n(src)),
        Alt(x, m) => format!("Alt[{} {:?}]", n(x), crate::altcast::Method::from_u8(m)),
        _ => format!("{o:?}"),
    }
}

/// Submit the first option matching `pred`; panics with the option list otherwise.
pub fn pick(g: &mut Game, pred: impl Fn(&Game, &crate::engine::Opt) -> bool) {
    let d = g.pending().unwrap_or_else(|| panic!("no pending decision; result={:?} err={:?}", g.result(), g.error())).clone();
    match d.options.iter().position(|o| pred(g, o)) {
        Some(i) => g.submit(i).unwrap(),
        None => panic!("no matching option at {:?} for {:?}: {:?}", d.kind, d.player, d.options.iter().map(|o| describe(g, o)).collect::<Vec<_>>()),
    }
}

/// Pick the option whose description contains `text` (e.g. "Cast[Lightning Bolt]", "Pass").
pub fn pick_text(g: &mut Game, text: &str) {
    pick(g, |g, o| describe(g, o).contains(text))
}

/// Pass priority until the pending decision isn't a priority decision, or `max` passes.
pub fn pass_until(g: &mut Game, stop: impl Fn(&Game) -> bool) {
    for _ in 0..500 {
        if stop(g) || g.pending().is_none() {
            return;
        }
        pick_text(g, "Pass");
    }
    panic!("pass_until: condition never reached");
}

pub fn obj_named(g: &Game, name: &str, zone: crate::state::Zone) -> Option<crate::basics::ObjId> {
    (0..g.s.objs.len()).map(|i| crate::basics::ObjId(i as u32)).find(|&o| g.s.obj(o).zone == zone && g.db.get(g.s.obj(o).card).name == name)
}

/// DB with every card from a meta snapshot, plus basic lands.
pub fn meta_db(decks_dir: &std::path::Path) -> Arc<CardDb> {
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(decks_dir.join("meta.json")).unwrap()).unwrap();
    let mut names: Vec<String> = ["Plains", "Island", "Swamp", "Mountain", "Forest"].iter().map(|s| s.to_string()).collect();
    for a in meta["archetypes"].as_array().unwrap() {
        let dl = crate::deck::DeckList::load(&decks_dir.join(a["file"].as_str().unwrap())).unwrap();
        names.extend(dl.card_names().into_iter().map(String::from));
    }
    let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    Arc::new(CardDb::load_forge(&cardsfolder(), &refs).unwrap())
}

/// A random 60-card deck from playable non-token cards: 24 lands (basics + playable lands), 36 spells.
pub fn random_deck(db: &CardDb, rng: &mut Rng) -> Deck {
    use crate::basics::CardType;
    let playable: Vec<CardId> = (0..db.cards.len() as u32).map(CardId).filter(|&c| db.get(c).playable() && !db.get(c).is_token).collect();
    let lands: Vec<CardId> = playable.iter().copied().filter(|&c| db.get(c).front().types.has(CardType::Land)).collect();
    let spells: Vec<CardId> = playable.iter().copied().filter(|&c| !db.get(c).front().types.has(CardType::Land)).collect();
    let mut main = vec![];
    for _ in 0..24 { main.push(lands[rng.below(lands.len() as u32) as usize]); }
    for _ in 0..36 { main.push(spells[rng.below(spells.len() as u32) as usize]); }
    Deck { main, side: vec![] }
}

/// Main-deck-playable lists of a frozen deck pool (`decks/pool/<snapshot>`), in pool order, with
/// a DB of every card they use plus basics. Sideboard cards without a Forge script are dropped
/// (as in the pool fuzz); lists whose main deck has an unknown or unsupported card are skipped.
pub fn pool_decks(dir: &std::path::Path) -> (Arc<CardDb>, Vec<Deck>) {
    use crate::deck::DeckList;
    let pool: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("pool.json")).unwrap()).unwrap();
    let index = crate::carddb::forge_index(&cardsfolder()).unwrap();
    let mut lists = vec![];
    for l in pool["lists"].as_array().unwrap() {
        let mut dl = DeckList::load(&dir.join(l["file"].as_str().unwrap())).unwrap();
        dl.side.retain(|(_, n)| index.contains_key(&crate::carddb::name_key(n)));
        if dl.main.iter().all(|(_, n)| index.contains_key(&crate::carddb::name_key(n))) {
            lists.push(dl);
        }
    }
    let mut names: Vec<&str> = lists.iter().flat_map(|l| l.card_names()).collect();
    names.extend(["Plains", "Island", "Swamp", "Mountain", "Forest"]);
    names.sort();
    names.dedup();
    let db = Arc::new(CardDb::load_forge(&cardsfolder(), &names).unwrap());
    let main_ok = |l: &DeckList| DeckList { name: l.name.clone(), main: l.main.clone(), side: vec![] }.unsupported(&db).is_empty();
    let decks = lists.iter().filter(|l| main_ok(l)).map(|l| l.resolve(&db).unwrap()).collect();
    (db, decks)
}

/// A cheap stand-in for a trained policy in benchmarks and tests: proactive play (with
/// probability 3/4 picks uniformly among options other than Pass/Done/No when there are any,
/// otherwise uniformly among all options), so games develop boards and end like real games
/// rather than drifting like uniformly random ones.
pub fn policy_like_choice(rng: &mut Rng, d: &crate::engine::Decision) -> usize {
    use crate::engine::Opt;
    let n = d.options.len() as u32;
    if rng.below(4) != 0 {
        let act: Vec<usize> = (0..d.options.len()).filter(|&i| !matches!(d.options[i], Opt::Pass | Opt::Done | Opt::No)).collect();
        if !act.is_empty() {
            return act[rng.below(act.len() as u32) as usize];
        }
    }
    rng.below(n) as usize
}
