//! List meta cards and whether they're playable (with reasons). Usage: playable [decks_dir] [--yes|--no]
use mtg_core::testutil::meta_db;
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).cloned().unwrap_or("../decks/standard".into());
    let mode = args.get(2).cloned().unwrap_or_default();
    let db = meta_db(std::path::Path::new(&dir));
    for c in &db.cards {
        if c.is_token { continue; }
        if c.playable() && mode != "--no" { println!("OK  {}", c.name); }
        if !c.playable() && mode != "--yes" { println!("NO  {}: {}", c.name, c.unsupported.join("; ")); }
    }
}
