//! Forge "Valid" filter expressions, e.g. `Creature.YouCtrl+nonToken,Planeswalker`, `Any`,
//! `Player.Opponent`. Comma = OR of alternatives; each alternative is `Base[.Prop+Prop...]`.
//! Unknown bases/properties are compile errors → the card is marked unsupported.

use crate::basics::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Base {
    /// Any object (`Card`, `Permanent` restricts to battlefield via zone context).
    Card,
    Permanent,
    Type(CardType),
    /// `Any` target: creature, player, planeswalker or battle (CR 115.4).
    Any,
    Player,
    Opponent,
    You,
    /// Subtype e.g. `Plains`, `Dwarf`.
    Subtype(Subtype),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Prop {
    YouCtrl,
    OppCtrl,
    YouOwn,
    Other,
    SelfCard,
    Token,
    NonToken,
    Tapped,
    Untapped,
    Attacking,
    Blocking,
    Not(Box<Prop>),
    NonType(CardType),
    NonSubtype(Subtype),
    Legendary,
    NonLegendary,
    Basic,
    NonBasic,
    WithKeyword(crate::carddb::Kw),
    WithoutKeyword(crate::carddb::Kw),
    Color(Color),
    NonColor(Color),
    Multicolor,
    Colorless,
    /// cmc/power/toughness comparisons: `cmcLE3`, `powerGE4`.
    Cmp(Stat, CmpOp, i32),
    /// Comparison against an SVar expression: `cmcLEX`.
    CmpExpr(Stat, CmpOp, crate::expr::Expr),
    YouDontCtrl,
    YouDontOwn,
    /// A card type used as a property (`Card.Creature`).
    IsType(CardType),
    /// Permanent card (not instant/sorcery) as a property.
    IsPermanent,
    /// Entered its current zone this turn.
    ThisTurnEntered,
    WasCast,
    WasCastByYou,
    /// `counters_GE1_STUN`
    Counters(crate::state::CounterKind, CmpOp, i32),
    InZone(crate::state::Zone),
    /// Remembered by the effect/ability evaluating the filter.
    IsRemembered,
    /// Has an Adventure (CR 715).
    AdventureCard,
    /// Has any of these subtypes (`Outlaw`, CR 700.12).
    AnySubtype(Vec<Subtype>),
    /// Extended properties (chosen type/name, exiled-with links, ...), see `filter_ext`.
    Ext(crate::filter_ext::ExtProp),
    /// The permanent the evaluating source (Equipment/Aura) is attached to (`EquippedBy`,
    /// `EnchantedBy`, `AttachedBy`).
    AttachedBySource,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stat {
    Cmc,
    Power,
    Toughness,
}
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CmpOp {
    LT,
    LE,
    EQ,
    NE,
    GE,
    GT,
}
impl CmpOp {
    pub fn parse(s: &str) -> Option<CmpOp> {
        Some(match s {
            "LT" => CmpOp::LT,
            "LE" => CmpOp::LE,
            "EQ" => CmpOp::EQ,
            "NE" => CmpOp::NE,
            "GE" => CmpOp::GE,
            "GT" => CmpOp::GT,
            _ => return None,
        })
    }
    pub fn eval(self, a: i32, b: i32) -> bool {
        match self {
            CmpOp::LT => a < b,
            CmpOp::LE => a <= b,
            CmpOp::EQ => a == b,
            CmpOp::NE => a != b,
            CmpOp::GE => a >= b,
            CmpOp::GT => a > b,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alt {
    pub base: Base,
    pub props: Vec<Prop>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filter {
    pub alts: Vec<Alt>,
}

impl Filter {
    pub fn parse(s: &str, intern: &mut dyn FnMut(&str) -> Option<Subtype>) -> Result<Filter, String> {
        let builtin = |n: &str| match n {
            "P1P1" => Some(crate::state::CounterKind::P1P1),
            "M1M1" => Some(crate::state::CounterKind::M1M1),
            "LOYALTY" => Some(crate::state::CounterKind::Loyalty),
            "STUN" => Some(crate::state::CounterKind::Stun),
            _ => None,
        };
        Self::parse_with(s, intern, &|_| None, &builtin)
    }

    /// Parse with access to the card's SVars (`svar`) and counter-name interning (`counter`).
    pub fn parse_with(
        s: &str,
        intern: &mut dyn FnMut(&str) -> Option<Subtype>,
        svar: &dyn Fn(&str) -> Option<crate::expr::Expr>,
        counter: &dyn Fn(&str) -> Option<crate::state::CounterKind>,
    ) -> Result<Filter, String> {
        let mut alts = vec![];
        for alt in s.split(',') {
            let alt = alt.trim();
            let (base_s, props_s) = match alt.find('.') {
                Some(i) => (&alt[..i], Some(&alt[i + 1..])),
                None => (alt, None),
            };
            let base = match base_s {
                "Card" => Base::Card,
                "Remembered" => {
                    // Plain `Remembered` = Card.IsRemembered; `Remembered.sameName` = a card with
                    // the same name as a remembered object. Other properties aren't supported.
                    match props_s {
                        None => alts.push(Alt { base: Base::Card, props: vec![Prop::IsRemembered] }),
                        Some("sameName") => alts.push(Alt { base: Base::Card, props: vec![Prop::Ext(crate::filter_ext::ExtProp::SameNameAsRemembered)] }),
                        Some(x) => return Err(format!("filter base Remembered.{x}")),
                    }
                    continue;
                }
                // `TargetedCard.sameName`: a card with the same name as the ability's target.
                "TargetedCard" if props_s == Some("sameName") => {
                    alts.push(Alt { base: Base::Card, props: vec![Prop::Ext(crate::filter_ext::ExtProp::SameNameAsTargeted)] });
                    continue;
                }
                "Permanent" => Base::Permanent,
                "Any" => Base::Any,
                "Player" => Base::Player,
                "Opponent" => Base::Opponent,
                "You" => Base::You,
                t => match CardType::parse(t) {
                    Some(ct) => Base::Type(ct),
                    None => match intern(t) {
                        Some(st) => Base::Subtype(st),
                        None => return Err(format!("filter base {t:?}")),
                    },
                },
            };
            let mut props = vec![];
            if let Some(ps) = props_s {
                for p in ps.split('+') {
                    props.push(parse_prop(p, intern, svar, counter)?);
                }
            }
            alts.push(Alt { base, props });
        }
        Ok(Filter { alts })
    }
    pub fn matches_players(&self) -> bool {
        self.alts.iter().any(|a| matches!(a.base, Base::Any | Base::Player | Base::Opponent | Base::You))
    }
}

fn parse_prop(p: &str, intern: &mut dyn FnMut(&str) -> Option<Subtype>, svar: &dyn Fn(&str) -> Option<crate::expr::Expr>, counter: &dyn Fn(&str) -> Option<crate::state::CounterKind>) -> Result<Prop, String> {
    use crate::carddb::Kw;
    if let Some(rest) = p.strip_prefix('!') {
        return Ok(Prop::Not(Box::new(parse_prop(rest, intern, svar, counter)?)));
    }
    if let Some(rest) = p.strip_prefix("counters_") {
        // counters_GE1_STUN
        let mut it = rest.splitn(2, '_');
        let cmp = it.next().unwrap_or("");
        let Some(kind) = it.next() else {
            // `counters_GE1`: any kind of counter.
            let (op, n) = crate::expr::parse_cmp(cmp)?;
            return Ok(Prop::Ext(crate::filter_ext::ExtProp::AnyCounters(op, n)));
        };
        let (op, n) = crate::expr::parse_cmp(cmp)?;
        let k = counter(kind).ok_or_else(|| format!("filter prop {p:?}"))?;
        return Ok(Prop::Counters(k, op, n));
    }
    if let Some(z) = p.strip_prefix("inZone") {
        use crate::state::Zone;
        let zone = match z {
            "Stack" => Zone::Stack,
            "Graveyard" => Zone::Graveyard,
            "Battlefield" => Zone::Battlefield,
            "Hand" => Zone::Hand,
            "Exile" => Zone::Exile,
            "Library" => Zone::Library,
            _ => return Err(format!("filter prop {p:?}")),
        };
        return Ok(Prop::InZone(zone));
    }
    if let Some(t) = CardType::parse(p) {
        return Ok(Prop::IsType(t));
    }
    if let Some(x) = crate::filter_ext::parse_ext_prop(p, intern, svar)? {
        return Ok(x);
    }
    Ok(match p {
        "IsRemembered" => Prop::IsRemembered,
        "AdventureCard" => Prop::AdventureCard,
        "EquippedBy" | "EnchantedBy" | "AttachedBy" => Prop::AttachedBySource,
        "YouDontCtrl" => Prop::YouDontCtrl,
        "YouDontOwn" => Prop::YouDontOwn,
        "Permanent" => Prop::IsPermanent,
        "ThisTurnEntered" => Prop::ThisTurnEntered,
        "wasCast" => Prop::WasCast,
        "wasCastByYou" => Prop::WasCastByYou,
        "YouCtrl" => Prop::YouCtrl,
        "OppCtrl" => Prop::OppCtrl,
        "YouOwn" => Prop::YouOwn,
        "Other" => Prop::Other,
        "Self" => Prop::SelfCard,
        "token" | "Token" => Prop::Token,
        "nonToken" => Prop::NonToken,
        "tapped" => Prop::Tapped,
        "untapped" => Prop::Untapped,
        "attacking" => Prop::Attacking,
        "blocking" => Prop::Blocking,
        "Legendary" => Prop::Legendary,
        "nonLegendary" => Prop::NonLegendary,
        "Basic" => Prop::Basic,
        "nonBasic" => Prop::NonBasic,
        "MultiColor" | "multicolor" => Prop::Multicolor,
        "Colorless" | "colorless" => Prop::Colorless,
        _ => {
            if let Some(rest) = p.strip_prefix("with") {
                if let Some(k) = Kw::parse(rest) {
                    return Ok(Prop::WithKeyword(k));
                }
            }
            if let Some(rest) = p.strip_prefix("without") {
                if let Some(k) = Kw::parse(rest) {
                    return Ok(Prop::WithoutKeyword(k));
                }
            }
            if let Some(c) = Color::from_word(p) {
                return Ok(Prop::Color(c));
            }
            if let Some(rest) = p.strip_prefix("non") {
                if let Some(c) = Color::from_word(rest) {
                    return Ok(Prop::NonColor(c));
                }
                if let Some(t) = CardType::parse(rest) {
                    return Ok(Prop::NonType(t));
                }
                if let Some(st) = intern(rest) {
                    return Ok(Prop::NonSubtype(st));
                }
            }
            for (pre, stat) in [("cmc", Stat::Cmc), ("power", Stat::Power), ("toughness", Stat::Toughness)] {
                if let Some(rest) = p.strip_prefix(pre) {
                    if rest.len() > 2 {
                        if let Some(op) = CmpOp::parse(&rest[..2]) {
                            if let Ok(n) = rest[2..].parse::<i32>() {
                                return Ok(Prop::Cmp(stat, op, n));
                            }
                            if let Some(e) = svar(&rest[2..]) {
                                return Ok(Prop::CmpExpr(stat, op, e));
                            }
                        }
                    }
                }
            }
            // A subtype used as a property (`Card.Spider`).
            if let Some(st) = intern(p) {
                return Ok(Prop::AnySubtype(vec![st]));
            }
            return Err(format!("filter prop {p:?}"));
        }
    })
}
