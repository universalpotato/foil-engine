//! Compile every card of the NameCard universe (`data/card_names.tsv`: every Standard-legal card
//! of the pinned Oracle bulk plus every pool card) and print one line per card:
//! `OK<TAB>name` or `NO<TAB>name<TAB>reasons`. Diff two builds' outputs to see which cards a
//! compiler change newly rejects or accepts.
//! Usage: compile_all [card_names.tsv] [cardsfolder]
use mtg_core::carddb::{forge_index, name_key, CardDb};
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let tsv = PathBuf::from(args.get(1).cloned().unwrap_or("../data/card_names.tsv".into()));
    let folder = args.get(2).map(PathBuf::from).unwrap_or_else(mtg_core::testutil::cardsfolder);
    let index = forge_index(&folder).unwrap();
    let mut names: Vec<String> = std::fs::read_to_string(&tsv)
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split('\t').next())
        .filter(|n| index.contains_key(&name_key(n)))
        .map(String::from)
        .collect();
    names.sort();
    names.dedup();
    let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let db = CardDb::load_forge(&folder, &refs).unwrap();
    let mut seen = std::collections::HashSet::new();
    for n in &names {
        // a back face's name resolves to its card's script: report each card once
        let Some(id) = db.by_name(n) else { continue };
        if !seen.insert(id) {
            continue;
        }
        let c = db.get(id);
        if c.playable() {
            println!("OK\t{}", c.name);
        } else {
            println!("NO\t{}\t{}", c.name, c.unsupported.join("; "));
        }
    }
}
