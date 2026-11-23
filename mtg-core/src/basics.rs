//! Small value types: ids, deterministic RNG, colors, mana, card types.

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PlayerId(pub u8);
impl PlayerId {
    pub fn opp(self) -> PlayerId {
        PlayerId(1 - self.0)
    }
    pub fn idx(self) -> usize {
        self.0 as usize
    }
}

/// Index into `State::objs`. A card changing zones becomes a new object with a new id (CR 400.7).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ObjId(pub u32);
impl ObjId {
    pub fn idx(self) -> usize {
        self.0 as usize
    }
}

/// Index into `CardDb::cards`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct CardId(pub u32);

/// xoshiro256++: small, fast, `Copy`, fully deterministic across platforms.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rng {
    s: [u64; 4],
}
impl Rng {
    pub fn new(seed: u64) -> Rng {
        let mut z = seed;
        let mut next = || {
            z = z.wrapping_add(0x9E3779B97F4A7C15);
            let mut x = z;
            x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
            x ^ (x >> 31)
        };
        Rng { s: [next(), next(), next(), next()] }
    }
    pub fn next_u64(&mut self) -> u64 {
        let r = self.s[0].wrapping_add(self.s[3]).rotate_left(23).wrapping_add(self.s[0]);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        r
    }
    /// Uniform in 0..n (Lemire's method, unbiased).
    pub fn below(&mut self, n: u32) -> u32 {
        assert!(n > 0);
        loop {
            let m = (self.next_u64() >> 32) * n as u64;
            let l = m as u32;
            if l >= n.wrapping_neg() % n {
                return (m >> 32) as u32;
            }
        }
    }
    pub fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i as u32 + 1) as usize;
            v.swap(i, j);
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Color {
    W = 0,
    U = 1,
    B = 2,
    R = 3,
    G = 4,
}
pub const COLORS: [Color; 5] = [Color::W, Color::U, Color::B, Color::R, Color::G];
impl Color {
    pub fn from_char(c: char) -> Option<Color> {
        Some(match c.to_ascii_uppercase() {
            'W' => Color::W,
            'U' => Color::U,
            'B' => Color::B,
            'R' => Color::R,
            'G' => Color::G,
            _ => return None,
        })
    }
    pub fn from_word(w: &str) -> Option<Color> {
        Some(match w.to_ascii_lowercase().as_str() {
            "white" => Color::W,
            "blue" => Color::U,
            "black" => Color::B,
            "red" => Color::R,
            "green" => Color::G,
            _ => return None,
        })
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ColorSet(pub u8);
impl ColorSet {
    pub fn has(self, c: Color) -> bool {
        self.0 & (1 << c as u8) != 0
    }
    pub fn with(self, c: Color) -> ColorSet {
        ColorSet(self.0 | (1 << c as u8))
    }
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub fn count(self) -> u32 {
        self.0.count_ones()
    }
    pub fn iter(self) -> impl Iterator<Item = Color> {
        COLORS.into_iter().filter(move |c| self.has(*c))
    }
}

/// A kind of mana that a source can produce or a pool can hold. Index 5 = colorless.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ManaType {
    W = 0,
    U = 1,
    B = 2,
    R = 3,
    G = 4,
    C = 5,
}
impl ManaType {
    pub fn of(c: Color) -> ManaType {
        [ManaType::W, ManaType::U, ManaType::B, ManaType::R, ManaType::G][c as usize]
    }
    pub fn color(self) -> Option<Color> {
        COLORS.get(self as usize).copied()
    }
    pub fn all() -> [ManaType; 6] {
        [ManaType::W, ManaType::U, ManaType::B, ManaType::R, ManaType::G, ManaType::C]
    }
    pub fn sym(self) -> char {
        ['W', 'U', 'B', 'R', 'G', 'C'][self as usize]
    }
}

/// One symbol in a mana cost (CR 107.4).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ManaSym {
    Generic(u8),
    Colored(Color),
    Colorless,
    Hybrid(Color, Color),
    /// {2/W}
    TwoBrid(Color),
    Phyrexian(Color),
    X,
    Snow,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ManaCost {
    pub syms: SmallVec<[ManaSym; 6]>,
    /// "no cost" (CR 202.1b): can't be cast normally.
    pub none: bool,
}

impl ManaCost {
    /// Parse Forge's `ManaCost:` syntax: space-separated shards, e.g. `2 W U`, `X R`, `W/U` or
    /// `WU` (hybrid), `W/P` or `WP` (Phyrexian), `2/W` (twobrid), `C`, `S`, or `no cost`.
    pub fn parse_forge(s: &str) -> Result<ManaCost, String> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("no cost") || s.is_empty() {
            return Ok(ManaCost { syms: SmallVec::new(), none: true });
        }
        let mut syms = SmallVec::new();
        for sh in s.split_whitespace() {
            let sh = sh.trim_matches(|c| c == '{' || c == '}');
            if let Ok(n) = sh.parse::<u8>() {
                if n > 0 {
                    syms.push(ManaSym::Generic(n));
                }
                continue;
            }
            let parts: Vec<&str> = if sh.contains('/') {
                sh.split('/').collect()
            } else if sh.len() == 2 {
                vec![&sh[..1], &sh[1..]]
            } else {
                vec![sh]
            };
            let sym = match parts.as_slice() {
                ["X"] => ManaSym::X,
                ["C"] => ManaSym::Colorless,
                ["S"] => ManaSym::Snow,
                [c] if c.len() == 1 => ManaSym::Colored(Color::from_char(c.chars().next().unwrap()).ok_or(format!("bad shard {sh}"))?),
                [a, "P"] => ManaSym::Phyrexian(Color::from_char(a.chars().next().unwrap()).ok_or(format!("bad shard {sh}"))?),
                ["2", b] => ManaSym::TwoBrid(Color::from_char(b.chars().next().unwrap()).ok_or(format!("bad shard {sh}"))?),
                [a, b] if a.len() == 1 && b.len() == 1 => {
                    let (a, b) = (a.chars().next().unwrap(), b.chars().next().unwrap());
                    ManaSym::Hybrid(Color::from_char(a).ok_or(format!("bad shard {sh}"))?, Color::from_char(b).ok_or(format!("bad shard {sh}"))?)
                }
                _ => return Err(format!("unsupported mana shard {sh:?}")),
            };
            syms.push(sym);
        }
        Ok(ManaCost { syms, none: false })
    }

    /// Mana value (CR 202.3); X counts as 0 except on the stack (caller adds X).
    pub fn mana_value(&self) -> u32 {
        self.syms
            .iter()
            .map(|s| match s {
                ManaSym::Generic(n) => *n as u32,
                ManaSym::X => 0,
                ManaSym::TwoBrid(_) => 2,
                _ => 1,
            })
            .sum()
    }
    pub fn colors(&self) -> ColorSet {
        let mut cs = ColorSet::default();
        for s in &self.syms {
            match *s {
                ManaSym::Colored(c) | ManaSym::Phyrexian(c) | ManaSym::TwoBrid(c) => cs = cs.with(c),
                ManaSym::Hybrid(a, b) => cs = cs.with(a).with(b),
                _ => {}
            }
        }
        cs
    }
    pub fn has_x(&self) -> bool {
        self.syms.contains(&ManaSym::X)
    }
}

impl std::fmt::Display for ManaCost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.none {
            return write!(f, "(no cost)");
        }
        for s in &self.syms {
            match s {
                ManaSym::Generic(n) => write!(f, "{{{n}}}")?,
                ManaSym::Colored(c) => write!(f, "{{{c:?}}}")?,
                ManaSym::Colorless => write!(f, "{{C}}")?,
                ManaSym::Hybrid(a, b) => write!(f, "{{{a:?}/{b:?}}}")?,
                ManaSym::TwoBrid(c) => write!(f, "{{2/{c:?}}}")?,
                ManaSym::Phyrexian(c) => write!(f, "{{{c:?}/P}}")?,
                ManaSym::X => write!(f, "{{X}}")?,
                ManaSym::Snow => write!(f, "{{S}}")?,
            }
        }
        Ok(())
    }
}

/// Card types (CR 300.1) as a bitmask.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TypeSet(pub u16);
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CardType {
    Artifact = 0,
    Battle,
    Creature,
    Enchantment,
    Instant,
    Kindred,
    Land,
    Planeswalker,
    Sorcery,
}
impl CardType {
    pub fn parse(s: &str) -> Option<CardType> {
        Some(match s {
            "Artifact" => CardType::Artifact,
            "Battle" => CardType::Battle,
            "Creature" => CardType::Creature,
            "Enchantment" => CardType::Enchantment,
            "Instant" => CardType::Instant,
            "Kindred" | "Tribal" => CardType::Kindred,
            "Land" => CardType::Land,
            "Planeswalker" => CardType::Planeswalker,
            "Sorcery" => CardType::Sorcery,
            _ => return None,
        })
    }
}
impl TypeSet {
    pub fn has(self, t: CardType) -> bool {
        self.0 & (1 << t as u16) != 0
    }
    pub fn with(self, t: CardType) -> TypeSet {
        TypeSet(self.0 | (1 << t as u16))
    }
    pub fn is_permanent(self) -> bool {
        !self.has(CardType::Instant) && !self.has(CardType::Sorcery) && self.0 != 0
    }
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Supertypes(pub u8);
impl Supertypes {
    pub const LEGENDARY: u8 = 1;
    pub const BASIC: u8 = 2;
    pub const SNOW: u8 = 4;
    pub const WORLD: u8 = 8;
    pub fn legendary(self) -> bool {
        self.0 & Self::LEGENDARY != 0
    }
    pub fn basic(self) -> bool {
        self.0 & Self::BASIC != 0
    }
}

/// Interned subtype (creature type, land type, ...). Names live in `CardDb::subtypes`.
pub type Subtype = u16;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mana_cost_parse() {
        let c = ManaCost::parse_forge("2 W U").unwrap();
        assert_eq!(c.mana_value(), 4);
        assert_eq!(ManaCost::parse_forge("UB").unwrap().syms[0], ManaSym::Hybrid(Color::U, Color::B));
        assert_eq!(ManaCost::parse_forge("W/P").unwrap().syms[0], ManaSym::Phyrexian(Color::W));
        assert!(ManaCost::parse_forge("no cost").unwrap().none);
        assert_eq!(ManaCost::parse_forge("X R").unwrap().mana_value(), 1);
    }
    #[test]
    fn rng_deterministic_and_unbiased_enough() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        let mut counts = [0u32; 6];
        for _ in 0..60000 {
            let x = a.below(6);
            assert_eq!(x, b.below(6));
            counts[x as usize] += 1;
        }
        assert!(counts.iter().all(|&c| (9000..11000).contains(&c)), "{counts:?}");
    }
}
