//! Print a card's Oracle text next to its compiled IR, for correctness review.
//! Usage: explain "Card Name" ["Card Name"...]
use mtg_core::carddb::*;
use mtg_core::testutil::cardsfolder;

fn show_ability(db: &CardDb, id: AbilityId, indent: usize) {
    let a = db.ability(id);
    let pad = " ".repeat(indent);
    println!("{pad}- {:?} effect={:?}", a.kind, a.effect);
    if let Some(t) = &a.target { println!("{pad}  target={t:?}"); }
    if let Some(d) = &a.defined { println!("{pad}  defined={d:?}"); }
    if let Some(c) = &a.cost { println!("{pad}  cost={c:?}"); }
    if !a.conditions.is_empty() { println!("{pad}  conditions={:?}", a.conditions); }
    if !a.activate_if.is_empty() { println!("{pad}  activate_if={:?}", a.activate_if); }
    if a.optional { println!("{pad}  optional"); }
    if let Some(u) = &a.unless { println!("{pad}  unless={u:?}"); }
    if a.sorcery_speed { println!("{pad}  sorcery speed"); }
    if let Effect::Charm { modes, .. } = &a.effect { for m in modes { println!("{pad}  mode:"); show_ability(db, *m, indent + 4); } }
    if let Effect::Ext(mtg_core::effect_ext::ExtEffect::Reflexive { execute, .. }) = &a.effect { println!("{pad}  reflexive:"); show_ability(db, *execute, indent + 4); }
    if db.exhaust.contains(&id) { println!("{pad}  exhaust"); }
    if let Some(s) = a.sub { show_ability(db, s, indent); }
}

fn main() {
    let names: Vec<String> = std::env::args().skip(1).collect();
    let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let db = CardDb::load_forge(&cardsfolder(), &refs).unwrap();
    for n in &names {
        let c = db.get(db.by_name(n).unwrap());
        if !c.unsupported.is_empty() { println!("UNSUPPORTED: {:?}", c.unsupported); }
        for (i, f) in c.faces.iter().enumerate() {
        if i > 0 { println!("-- face {i} ({:?}):", c.layout); }
        println!("== {} {} {:?} {:?}/{:?} loyalty={:?} playable={}", f.name, f.cost, f.types, f.power, f.toughness, f.loyalty, c.playable());
        println!("Oracle: {}", f.oracle.replace('\n', " | "));
        for a in &f.hand_abilities { println!("hand ability:"); show_ability(&db, *a, 2); }
        if f.kws.0 != 0 { println!("kws: {:?}", f.kws.iter().collect::<Vec<_>>()); }
        if let Some(k) = &f.kicker { println!("kicker: {k}"); }
        if f.alt != Default::default() { println!("alt costs: {:?}", f.alt); }
        if let Some(sp) = f.spell { for n in mtg_core::resolve::nodes(&db, sp, &[]) { if let Effect::Charm { modes, .. } = &db.ability(n).effect { for (i, m) in modes.iter().enumerate() { if let Some((_, c)) = db.mode_costs.iter().find(|x| x.0 == *m) { println!("mode {i} cost: {c}"); } } } } }
        if let Some(s) = f.spell { println!("spell:"); show_ability(&db, s, 2); }
        for a in &f.activated { if !db.ability(*a).is_mana { println!("activated:"); show_ability(&db, *a, 2); } }
        if let Some(c) = &f.cda_pt { println!("characteristic-defining P/T: {c:?}"); }
        for a in &f.grave_abilities { println!("activated from the graveyard:"); show_ability(&db, *a, 2); }
        for t in &f.triggers { println!("trigger {:?} zones={:?} optional={} intervening={:?} per_turn={:?}:", t.mode, t.zones, t.optional, t.intervening, t.per_turn); show_ability(&db, t.execute, 2); }
        for s in &f.statics { println!("static {:?} zones={:?} cond={:?}", s.kind, s.zones, s.conditions); }
        for r in &f.replacements { println!("replacement origin={:?} dest={:?} valid={:?} zones={:?} action={:?}", r.origin, r.dest, r.valid, r.zones, r.action); if let ReplAction::Modify(a) = r.action { show_ability(&db, a, 2); } }
        }
        println!();
    }
}
