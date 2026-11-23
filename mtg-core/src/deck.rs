//! Decklists: `N Card Name` lines; a blank line or `Sideboard` separates main from side.
//! Oracle names of multi-face cards (`Front // Back`) are accepted and keyed by the front face,
//! which is how Forge scripts are named (decks/pool/ lists use Oracle names).

use crate::carddb::CardDb;
use crate::engine::Deck;
use std::path::Path;

#[derive(Clone, Debug, Default)]
pub struct DeckList {
    pub name: String,
    pub main: Vec<(u32, String)>,
    pub side: Vec<(u32, String)>,
}

impl DeckList {
    pub fn parse(name: &str, src: &str) -> DeckList {
        let mut d = DeckList { name: name.to_string(), ..Default::default() };
        let mut in_side = false;
        for line in src.lines() {
            let l = line.trim();
            if l.is_empty() {
                if !d.main.is_empty() {
                    in_side = true;
                }
                continue;
            }
            if l.to_ascii_lowercase().starts_with("sideboard") {
                in_side = true;
                continue;
            }
            let (n, rest) = l.split_once(' ').unwrap_or(("1", l));
            let n: u32 = n.trim_end_matches('x').parse().unwrap_or(1);
            let rest = rest.trim();
            let front = rest.split_once(" // ").map_or(rest, |(f, _)| f.trim());
            let entry = (n, front.to_string());
            if in_side {
                d.side.push(entry)
            } else {
                d.main.push(entry)
            }
        }
        d
    }
    pub fn load(path: &Path) -> std::io::Result<DeckList> {
        let name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        Ok(DeckList::parse(&name, &std::fs::read_to_string(path)?))
    }
    pub fn card_names(&self) -> Vec<&str> {
        self.main.iter().chain(&self.side).map(|(_, n)| n.as_str()).collect()
    }
    /// Resolve against the DB. Errors list missing cards.
    pub fn resolve(&self, db: &CardDb) -> Result<Deck, String> {
        let mut missing = vec![];
        let mut conv = |v: &[(u32, String)]| {
            let mut out = vec![];
            for (n, name) in v {
                match db.by_name(name) {
                    Some(id) => out.extend(std::iter::repeat(id).take(*n as usize)),
                    None => missing.push(name.clone()),
                }
            }
            out
        };
        let main = conv(&self.main);
        let side = conv(&self.side);
        if missing.is_empty() {
            Ok(Deck { main, side })
        } else {
            Err(format!("missing cards: {missing:?}"))
        }
    }
    /// Cards in this list the engine can't play yet, with reasons.
    pub fn unsupported<'a>(&self, db: &'a CardDb) -> Vec<(String, &'a [String])> {
        let mut out = vec![];
        for name in self.card_names() {
            if let Some(id) = db.by_name(name) {
                let c = db.get(id);
                if !c.playable() && !out.iter().any(|(n, _): &(String, _)| n == &c.name) {
                    out.push((c.name.clone(), c.unsupported.as_slice()));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::DeckList;

    #[test]
    fn parses_oracle_multiface_names_as_front_face() {
        let d = DeckList::parse("t", "4 Bonecrusher Giant // Stomp\n2 Island\n\nSideboard\n1 Fire // Ice\n");
        assert_eq!(d.main, vec![(4, "Bonecrusher Giant".to_string()), (2, "Island".to_string())]);
        assert_eq!(d.side, vec![(1, "Fire".to_string())]);
    }
}
