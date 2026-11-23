//! Lossless parser for Forge card scripts (`forge-gui/res/cardsfolder/**/*.txt`).
//!
//! Format (one `Key:Value` per line):
//! - header keys: `Name`, `ManaCost`, `Types`, `PT`, `Loyalty`, `Defense`, `Colors`, `Oracle`, ...
//! - repeated keys: `K:` keyword, `A:` spell/activated ability, `T:` trigger, `S:` static,
//!   `R:` replacement, `SVar:Name:Value` sub-ability / variable.
//! - `ALTERNATE` on its own line starts the next face; `AlternateMode:` says how faces relate.
//! - Ability-like values are `Key$ Value | Key$ Value | ...`; the first pair names the API
//!   (`SP$ DealDamage`, `AB$ Mana`, `DB$ Draw`) or the mode (`Mode$ ChangesZone`).
//!
//! This crate only parses; it assigns no game semantics. Semantics live in `mtg-core`.

use std::fmt;

/// Ordered key/value parameters of one ability-like line (`A:`, `T:`, `S:`, `R:`, `DB$` SVars).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Params {
    pub pairs: Vec<(String, String)>,
}

impl Params {
    /// Parse `Key$ Value | Key$ Value`. Fragments without `$` are kept with an empty key so
    /// nothing is silently dropped (they are reported by `lint`).
    pub fn parse(s: &str) -> Params {
        let mut pairs = Vec::new();
        for frag in s.split('|') {
            let frag = frag.trim();
            if frag.is_empty() {
                continue;
            }
            match frag.find('$') {
                Some(i) => pairs.push((frag[..i].trim().to_string(), frag[i + 1..].trim().to_string())),
                None => pairs.push((String::new(), frag.to_string())),
            }
        }
        Params { pairs }
    }
    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
    pub fn has(&self, key: &str) -> bool {
        self.pairs.iter().any(|(k, _)| k == key)
    }
    /// The API/mode this ability uses: value of `SP$`, `AB$`, `DB$` or `Mode$`.
    pub fn api(&self) -> Option<(&str, &str)> {
        self.pairs
            .iter()
            .find(|(k, _)| matches!(k.as_str(), "SP" | "AB" | "DB" | "Mode"))
            .map(|(k, v)| (k.as_str(), v.as_str()))
    }
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.pairs.iter().map(|(k, _)| k.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SVar {
    pub name: String,
    pub value: String,
}

impl SVar {
    /// SVars hold either a sub-ability (`DB$ ...`, `AB$ ...`), or a value expression
    /// (`Count$...`, `Number$...`), or AI hints. Returns params if it's an ability.
    pub fn as_ability(&self) -> Option<Params> {
        let v = self.value.trim_start();
        if v.starts_with("DB$") || v.starts_with("AB$") || v.starts_with("SP$") || v.starts_with("Mode$") {
            Some(Params::parse(v))
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Face {
    pub name: String,
    pub mana_cost: Option<String>,
    pub types: Option<String>,
    pub pt: Option<String>,
    pub loyalty: Option<String>,
    pub defense: Option<String>,
    pub colors: Option<String>,
    /// `CopyFaceFrom:<card>`: this face is a copy of another card's face (e.g. Prepare cards).
    pub copy_face_from: Option<String>,
    pub oracle: Option<String>,
    pub keywords: Vec<String>,
    pub abilities: Vec<Params>,
    pub triggers: Vec<Params>,
    pub statics: Vec<Params>,
    pub replacements: Vec<Params>,
    pub svars: Vec<SVar>,
    /// Any other `Key:Value` lines (Text, DeckHints, AI, ...), kept verbatim.
    pub other: Vec<(String, String)>,
}

impl Face {
    pub fn svar(&self, name: &str) -> Option<&SVar> {
        self.svars.iter().find(|s| s.name == name)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CardScript {
    /// `AlternateMode:` value (DoubleFaced, Split, Adventure, Modal, Flip, Omen, ...), if any.
    pub alternate_mode: Option<String>,
    pub faces: Vec<Face>,
}

impl CardScript {
    pub fn name(&self) -> &str {
        &self.faces[0].name
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub msg: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.msg)
    }
}
impl std::error::Error for ParseError {}

pub fn parse(src: &str) -> Result<CardScript, ParseError> {
    let mut card = CardScript::default();
    let mut face = Face::default();
    for (i, raw) in src.lines().enumerate() {
        let line = raw.trim_end_matches('\r').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line == "ALTERNATE" {
            card.faces.push(std::mem::take(&mut face));
            continue;
        }
        let Some(colon) = line.find(':') else {
            return Err(ParseError { line: i + 1, msg: format!("no ':' in {line:?}") });
        };
        let (key, val) = (&line[..colon], line[colon + 1..].trim());
        match key {
            "Name" => face.name = val.to_string(),
            "ManaCost" => face.mana_cost = Some(val.to_string()),
            "Types" => face.types = Some(val.to_string()),
            "PT" => face.pt = Some(val.to_string()),
            "Loyalty" => face.loyalty = Some(val.to_string()),
            "Defense" => face.defense = Some(val.to_string()),
            "Colors" => face.colors = Some(val.to_string()),
            "CopyFaceFrom" => face.copy_face_from = Some(val.to_string()),
            "Oracle" => face.oracle = Some(val.replace("\\n", "\n")),
            "K" => face.keywords.push(val.to_string()),
            "A" => face.abilities.push(Params::parse(val)),
            "T" => face.triggers.push(Params::parse(val)),
            "S" => face.statics.push(Params::parse(val)),
            "R" => face.replacements.push(Params::parse(val)),
            "SVar" => {
                let Some(c2) = val.find(':') else {
                    return Err(ParseError { line: i + 1, msg: format!("SVar without name: {val:?}") });
                };
                face.svars.push(SVar { name: val[..c2].to_string(), value: val[c2 + 1..].to_string() });
            }
            "AlternateMode" => card.alternate_mode = Some(val.to_string()),
            _ => face.other.push((key.to_string(), val.to_string())),
        }
    }
    card.faces.push(face);
    if card.faces.iter().any(|f| f.name.is_empty() && f.copy_face_from.is_none()) {
        return Err(ParseError { line: 0, msg: "face without Name".into() });
    }
    Ok(card)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bolt() {
        let c = parse("Name:Lightning Bolt\nManaCost:R\nTypes:Instant\nA:SP$ DealDamage | ValidTgts$ Any | NumDmg$ 3 | SpellDescription$ CARDNAME deals 3 damage to any target.\nOracle:Lightning Bolt deals 3 damage to any target.\n").unwrap();
        let f = &c.faces[0];
        assert_eq!(f.name, "Lightning Bolt");
        assert_eq!(f.abilities[0].api(), Some(("SP", "DealDamage")));
        assert_eq!(f.abilities[0].get("NumDmg"), Some("3"));
    }

    #[test]
    fn svar_with_colons_and_faces() {
        let c = parse("Name:A\nSVar:X:Count$ThisTurnActivated_Activated.Equip+YouCtrl\nSVar:T:DB$ Draw | NumCards$ 1\nAlternateMode:Adventure\nALTERNATE\nName:B\n").unwrap();
        assert_eq!(c.faces.len(), 2);
        assert_eq!(c.faces[0].svar("X").unwrap().value, "Count$ThisTurnActivated_Activated.Equip+YouCtrl");
        assert_eq!(c.faces[0].svar("T").unwrap().as_ability().unwrap().api(), Some(("DB", "Draw")));
        assert_eq!(c.alternate_mode.as_deref(), Some("Adventure"));
    }
}
