//! Engine coverage of a meta snapshot: which decks are fully playable, and which missing
//! features block the most meta share. Usage: coverage [decks_dir] [cardsfolder]
//! A deck-pool dir (decks/pool/<date>/, has pool.json) gets the pool report instead: main-deck
//! playable lists per archetype and split, and the cards blocking the most lists.
use mtg_core::carddb::CardDb;
use mtg_core::deck::DeckList;
use std::collections::BTreeMap;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let decks_dir = PathBuf::from(args.get(1).cloned().unwrap_or("../decks/standard".into()));
    let folder = PathBuf::from(args.get(2).cloned().unwrap_or("data/forge/repo/forge-gui/res/cardsfolder".into()));
    if decks_dir.join("pool.json").exists() {
        return pool_report(&decks_dir, &folder);
    }
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(decks_dir.join("meta.json")).unwrap()).unwrap();
    let mut lists = vec![];
    for a in meta["archetypes"].as_array().unwrap() {
        let dl = DeckList::load(&decks_dir.join(a["file"].as_str().unwrap())).unwrap();
        lists.push((a["name"].as_str().unwrap().to_string(), a["meta_share"].as_f64().unwrap(), dl));
    }
    let names: Vec<&str> = lists.iter().flat_map(|l| l.2.card_names()).collect();
    let db = CardDb::load_forge(&folder, &names).unwrap();
    let total: f64 = lists.iter().map(|l| l.1).sum();
    let mut playable_share = 0.0;
    let mut main_share = 0.0;
    // feature -> (meta share blocked, cards)
    let mut feat: BTreeMap<String, (f64, Vec<String>)> = BTreeMap::new();
    let mut card_share: BTreeMap<String, f64> = BTreeMap::new();
    for (name, share, dl) in &lists {
        let uns = dl.unsupported(&db);
        if uns.is_empty() {
            playable_share += share;
        }
        let main_only = DeckList { name: dl.name.clone(), main: dl.main.clone(), side: vec![] };
        let uns_main = main_only.unsupported(&db);
        if uns_main.is_empty() {
            main_share += share;
        }
        println!("{:5.1}% {:24} unsupported cards: {:2} (main deck: {:2})", share, name, uns.len(), uns_main.len());
        if std::env::var("VERBOSE").is_ok() {
            for (c, r) in &uns { println!("        {c}: {}", r.join("; ")); }
        }
        let mut feats_here: Vec<String> = vec![];
        for (card, reasons) in &uns {
            *card_share.entry(card.clone()).or_default() += share;
            for r in reasons.iter() {
                let key = r.split(" param ").next().unwrap().to_string();
                let key = if key.starts_with("amount ") { "amount <svar expr>".into() } else { key };
                let e = feat.entry(key.clone()).or_default();
                if !e.1.contains(card) { e.1.push(card.clone()); }
                if !feats_here.contains(&key) { feats_here.push(key); }
            }
        }
        for k in feats_here { feat.get_mut(&k).unwrap().0 += share; }
    }
    let n = db.cards.len();
    let ok = db.cards.iter().filter(|c| c.playable()).count();
    println!("\ncards playable: {ok}/{n}; fully playable decks: {:.1}% (main deck only: {:.1}%) of {:.1}% snapshot share", playable_share, main_share, total);
    let mut fv: Vec<_> = feat.into_iter().collect();
    fv.sort_by(|a, b| b.1 .0.partial_cmp(&a.1 .0).unwrap());
    println!("\nmissing features by meta share of decks they appear in (top 40):");
    for (k, (sh, cards)) in fv.iter().take(40) {
        println!("  {:5.1}%  {:3} cards  {}", sh, cards.len(), k);
    }
}

/// Pool mode. Cards without a Forge script count as unsupported (the pool contains every card
/// seen in tournaments, the engine DB only what Forge has).
fn pool_report(dir: &std::path::Path, folder: &std::path::Path) {
    let pool: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("pool.json")).unwrap()).unwrap();
    let index = mtg_core::carddb::forge_index(folder).unwrap();
    let mut lists = vec![];
    for l in pool["lists"].as_array().unwrap() {
        let dl = DeckList::load(&dir.join(l["file"].as_str().unwrap())).unwrap();
        lists.push((l["archetype"].as_str().unwrap().to_string(), l["split"].as_str().unwrap_or("train").to_string(), dl));
    }
    let mut names: Vec<&str> = lists.iter().flat_map(|l| l.2.main.iter().map(|(_, n)| n.as_str())).collect();
    names.sort();
    names.dedup();
    let (have, missing): (Vec<&str>, Vec<&str>) = names.iter().partition(|n| index.contains_key(&mtg_core::carddb::name_key(n)));
    let db = CardDb::load_forge(folder, &have).unwrap();
    // archetype -> (lists, playable, train playable, eval playable)
    let mut per: BTreeMap<String, (usize, usize, usize, usize)> = BTreeMap::new();
    let mut blockers: BTreeMap<String, (usize, String)> = BTreeMap::new();
    let mut single: BTreeMap<String, usize> = BTreeMap::new(); // lists blocked by exactly this one card
    // DUMP=path writes one JSON line per list: archetype, split, blocking cards with all reasons.
    let mut dump = std::env::var("DUMP").ok().map(|p| std::fs::File::create(p).unwrap());
    for (arch, split, dl) in &lists {
        let mut bad: Vec<(String, String)> = vec![];
        for (_, n) in &dl.main {
            if missing.contains(&n.as_str()) {
                bad.push((n.clone(), "no Forge script".into()));
            }
        }
        let have_main = DeckList { name: dl.name.clone(), main: dl.main.iter().filter(|(_, n)| !missing.contains(&n.as_str())).cloned().collect(), side: vec![] };
        let mut all_reasons: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (c, r) in have_main.unsupported(&db) {
            bad.push((c.clone(), r.first().cloned().unwrap_or_default()));
            all_reasons.insert(c, r.to_vec());
        }
        if let Some(f) = dump.as_mut() {
            use std::io::Write;
            let b: BTreeMap<&String, Vec<String>> = bad.iter().map(|(c, r)| (c, all_reasons.get(c).cloned().unwrap_or(vec![r.clone()]))).collect();
            writeln!(f, "{}", serde_json::json!({"name": dl.name, "archetype": arch, "split": split, "blockers": b})).unwrap();
        }
        bad.sort();
        bad.dedup_by(|a, b| a.0 == b.0);
        let e = per.entry(arch.clone()).or_default();
        e.0 += 1;
        if bad.is_empty() {
            e.1 += 1;
            if split == "eval" { e.3 += 1 } else { e.2 += 1 }
        }
        if bad.len() == 1 {
            *single.entry(bad[0].0.clone()).or_default() += 1;
        }
        for (c, r) in bad {
            let b = blockers.entry(c).or_insert((0, r));
            b.0 += 1;
        }
    }
    let (mut tl, mut tp, mut tt, mut te) = (0, 0, 0, 0);
    println!("{:24} {:>6} {:>9} {:>6} {:>5}", "archetype", "lists", "playable", "train", "eval");
    for (a, (l, p, t, e)) in &per {
        println!("{:24} {:6} {:9} {:6} {:5}", a, l, p, t, e);
        tl += l; tp += p; tt += t; te += e;
    }
    println!("{:24} {:6} {:9} {:6} {:5}   ({:.1}% of lists main-deck playable)", "TOTAL", tl, tp, tt, te, 100.0 * tp as f64 / tl.max(1) as f64);
    let mut bv: Vec<_> = blockers.into_iter().collect();
    bv.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(a.0.cmp(&b.0)));
    println!("\ncards blocking the most lists (top 30; [n] = lists blocked by this card alone):");
    for (c, (n, r)) in bv.iter().take(30) {
        println!("  {:4} [{:3}]  {:32} {}", n, single.get(c).copied().unwrap_or(0), c, r);
    }
}
