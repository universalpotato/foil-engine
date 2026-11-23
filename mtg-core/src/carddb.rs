//! Card database: Forge scripts compiled (by `compile.rs`) into a typed IR. Immutable, shared
//! via `Arc`. A card with any feature we don't implement records it in `unsupported`.

use crate::basics::*;
use crate::expr::{Condition, Expr};
use crate::filter::Filter;
use crate::state::{CounterKind, Step};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Simple keywords (no parameters), as a bitmask on characteristics.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum Kw {
    Flying = 0,
    Reach,
    Haste,
    Vigilance,
    Trample,
    Lifelink,
    Deathtouch,
    FirstStrike,
    DoubleStrike,
    Menace,
    Defender,
    Flash,
    Hexproof,
    Indestructible,
    Prowess,
}
pub const ALL_KW: [Kw; 15] = [
    Kw::Flying,
    Kw::Reach,
    Kw::Haste,
    Kw::Vigilance,
    Kw::Trample,
    Kw::Lifelink,
    Kw::Deathtouch,
    Kw::FirstStrike,
    Kw::DoubleStrike,
    Kw::Menace,
    Kw::Defender,
    Kw::Flash,
    Kw::Hexproof,
    Kw::Indestructible,
    Kw::Prowess,
];
impl Kw {
    pub fn parse(s: &str) -> Option<Kw> {
        Some(match s.replace(' ', "").to_ascii_lowercase().as_str() {
            "flying" => Kw::Flying,
            "reach" => Kw::Reach,
            "haste" => Kw::Haste,
            "vigilance" => Kw::Vigilance,
            "trample" => Kw::Trample,
            "lifelink" => Kw::Lifelink,
            "deathtouch" => Kw::Deathtouch,
            "firststrike" => Kw::FirstStrike,
            "doublestrike" => Kw::DoubleStrike,
            "menace" => Kw::Menace,
            "defender" => Kw::Defender,
            "flash" => Kw::Flash,
            "hexproof" => Kw::Hexproof,
            "indestructible" => Kw::Indestructible,
            "prowess" => Kw::Prowess,
            _ => return None,
        })
    }
}
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct KwSet(pub u32);
impl KwSet {
    pub fn has(self, k: Kw) -> bool {
        self.0 & (1 << k as u8) != 0
    }
    pub fn with(self, k: Kw) -> KwSet {
        KwSet(self.0 | (1 << k as u8))
    }
    pub fn union(self, o: KwSet) -> KwSet {
        KwSet(self.0 | o.0)
    }
    pub fn iter(self) -> impl Iterator<Item = Kw> {
        ALL_KW.into_iter().filter(move |k| self.has(*k))
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AbilityId(pub u32);

/// Who/what an effect refers to without targeting (`Defined$`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Defined {
    You,
    Opponent,
    SelfObj,
    /// The target(s) of the nearest preceding node that targeted (`Targeted`, `ParentTarget`).
    Targeted,
    TargetedController,
    TargetedPlayer,
    /// The object that caused the trigger (`TriggeredCard`, `TriggeredNewCardLKICopy`).
    Triggered,
    TriggeredController,
    /// The player of the triggering event (`TriggeredPlayer`: the player who drew, ...).
    TriggeredPlayer,
    /// Objects remembered earlier in this resolution (`Remembered`).
    Remembered,
    /// The object being moved by a replacement (`ReplacedCard`).
    Replaced,
    /// Cards chosen by this ability's `ChooseCard` (`ChosenCard`).
    ChosenCard,
    /// The controller (owner, for a card outside the battlefield) of the first remembered object
    /// (`RememberedController`, `RememberedOwner`).
    RememberedController,
    /// The permanent the source Aura/Equipment is attached to (`Enchanted`, `Equipped`).
    Attached,
    /// Objects imprinted earlier in this resolution (`Imprinted`; `Exec::imprinted`, `ext9`).
    Imprinted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetKind {
    /// Objects on the battlefield and/or players, per filter.
    Normal,
    /// Spells on the stack (`TargetType$ Spell`).
    Spell,
    /// Cards in a graveyard (`TgtZone$ Graveyard`).
    Graveyard,
    /// Activated or triggered abilities on the stack (`TargetType$ Activated,Triggered`), each
    /// its own target (`Target::Ability`, CR 113.7a).
    Ability,
    /// A spell on the stack or a permanent (`TgtZone$ Stack,Battlefield`).
    SpellOrPermanent,
    /// Spells and/or abilities on the stack (`ext8`): `TargetType$ Instant,Sorcery,Triggered`,
    /// `Activated,Triggered,Instant,Sorcery`, `SpellAbility.singleTarget`, `TgtZone$ Stack`.
    Stack(StackSel),
}

/// Which stack objects a `TargetKind::Stack` target may be (CR 115.1, 115.9a).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackSel {
    /// Spells matching the target filter.
    pub spells: bool,
    /// Only instant and sorcery spells ("target instant spell, sorcery spell, ...").
    pub inst_sorc: bool,
    pub activated: bool,
    pub triggered: bool,
    /// "... with a single target": exactly one target chosen as it was put on the stack
    /// (115.9a; changed targets count as chosen).
    pub single_target: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetSpec {
    pub filter: Filter,
    pub kind: TargetKind,
    pub min: u8,
    pub max: u8,
    /// `TargetUnique$`: targets of other nodes of the same ability can't be chosen again.
    #[serde(default)]
    pub unique: bool,
    /// Variable maximum (`TargetMax$ X`), evaluated as targets are chosen (`max` caps it).
    #[serde(default)]
    pub max_expr: Option<crate::expr::Expr>,
    /// Exactly this many targets (not promised, promised) by whether the gift was promised
    /// (`TargetMin$ X | TargetMax$ X`, X = `Count$PromisedGift.A.B`; Into the Flood Maw, `ext8`):
    /// the promise is announced before targets are chosen (CR 601.2b-c, 702.174a).
    #[serde(default)]
    pub by_promise: Option<(u8, u8)>,
}

/// How long an effect object lasts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffDuration {
    EndOfTurn,
    Permanent,
    UntilYourNextTurn,
    UntilTheEndOfYourNextTurn,
    /// Until the controller's next end step begins (Dragonhawk; `ext5`).
    UntilYourNextEndStep,
    /// While the source remains on the battlefield.
    AsLongAsInPlay,
    /// "For as long as it remains tapped": ends once a remembered object is untapped
    /// (Forge `UntilTargetedUntaps`).
    UntilUntaps,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Duration {
    EndOfTurn,
    Permanent,
    UntilYourNextTurn,
}

/// Zones as a bitmask (for zone-change triggers/replacements and ChangeZone origins).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ZoneSet(pub u8);
impl ZoneSet {
    pub const ANY: ZoneSet = ZoneSet(0xff);
    pub fn of(z: crate::state::Zone) -> ZoneSet {
        ZoneSet(1 << z as u8)
    }
    pub fn has(self, z: crate::state::Zone) -> bool {
        self.0 & (1 << z as u8) != 0
    }
    pub fn parse(s: &str) -> Result<ZoneSet, String> {
        use crate::state::Zone;
        let mut m = 0u8;
        for w in s.split(',') {
            let z = match w.trim() {
                "Any" | "All" => return Ok(ZoneSet::ANY),
                "Battlefield" => Zone::Battlefield,
                "Graveyard" => Zone::Graveyard,
                "Hand" => Zone::Hand,
                "Library" => Zone::Library,
                "Exile" => Zone::Exile,
                "Stack" => Zone::Stack,
                "Sideboard" => Zone::Sideboard,
                z => return Err(format!("zone {z}")),
            };
            m |= 1 << z as u8;
        }
        Ok(ZoneSet(m))
    }
}

/// A player selector (`ValidPlayer$ You`, `Opponent`, ...).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlayerSel {
    You,
    Opponent,
    Any,
}
impl PlayerSel {
    pub fn parse(s: Option<&str>) -> Result<PlayerSel, String> {
        Ok(match s {
            None | Some("Player") | Some("Any") => PlayerSel::Any,
            Some("You") => PlayerSel::You,
            Some("Opponent") | Some("Player.Opponent") => PlayerSel::Opponent,
            Some(x) => return Err(format!("player selector {x}")),
        })
    }
    pub fn matches(self, p: PlayerId, you: PlayerId) -> bool {
        match self {
            PlayerSel::Any => true,
            PlayerSel::You => p == you,
            PlayerSel::Opponent => p != you,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Produced {
    One(ManaType),
    /// One mana of any of these types (Forge `Combo W U`, `Any`).
    AnyOf(Vec<ManaType>),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiscardMode {
    /// Discarding player chooses (`TgtChoose`).
    Choose,
    /// Ability controller looks at the hand and chooses (`RevealYouChoose`).
    RevealYouChoose,
    /// Whole hand.
    Hand,
    /// At random (`Random`, CR 701.9b), with the game's RNG.
    Random,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effect {
    DealDamage { amount: Expr },
    Draw { n: Expr },
    GainLife { n: Expr },
    LoseLife { n: Expr },
    Destroy,
    DestroyAll { valid: Filter },
    Pump { power: Expr, toughness: Expr, kws: KwSet, duration: Duration },
    PumpAll { valid: Filter, power: Expr, toughness: Expr, kws: KwSet, duration: Duration },
    Scry { n: Expr },
    Surveil { n: Expr },
    Mill { n: Expr },
    Tap,
    Untap,
    Counter,
    Mana { produced: Produced, amount: u8 },
    Discard { n: Expr, mode: DiscardMode, valid: Option<Filter> },
    /// `n` permanents matching `valid` (each affected player chooses theirs). `up_to`: "sacrifice
    /// any number of ..." (Forge `Optional$` with an amount other than 1: minimum 0), so choosing
    /// none is the "may"; otherwise exactly `n` (as many as possible, CR 609.3).
    Sacrifice { valid: Filter, n: Expr, up_to: bool },
    PutCounter { kind: CounterKind, n: Expr },
    /// Move objects. `hidden`: choose from a hidden zone (search/tutor) matching `change_type`.
    ChangeZone { origin: ZoneSet, dest: crate::state::Zone, hidden: bool, change_type: Option<Filter>, num: Expr, tapped: bool, optional: bool, shuffle: bool, searcher: Defined },
    Token { token: CardId, n: Expr, tapped: bool },
    /// Modal: choose `min..=max` of the listed modes (each a sub-ability chain).
    Charm { modes: Vec<AbilityId>, min: u8, max: u8 },
    /// Forge bookkeeping (`Cleanup`): forget remembered objects.
    Cleanup { clear_remembered: bool },
    /// Look at the top `n` cards; put `change` of them (matching `valid`) into `dest`, the rest
    /// into `rest` (library: bottom). `any_number`/`optional`: may take fewer.
    Dig { n: Expr, change: Expr, dest: crate::state::Zone, rest: crate::state::Zone, valid: Option<Filter>, up_to: bool, reveal: bool, rest_random: bool },
    /// Move every matching card in the origin zones (all players') to `dest`.
    ChangeZoneAll { origin: ZoneSet, dest: crate::state::Zone, change_type: Filter, shuffle: bool },
    DamageAll { amount: Expr, cards: Option<Filter>, players: Option<PlayerSel> },
    PutCounterAll { valid: Filter, kind: CounterKind, n: Expr },
    /// CR 701.44
    Explore,
    /// Create an effect object with static abilities (`Effect`), remembering `remember` objects.
    CreateEffect { statics: Vec<u32>, remember: Option<Defined>, duration: EffDuration, forget_on_moved: ZoneSet, exile_on_moved: ZoneSet },
    /// Effects added later (choices, links, delayed triggers, ...), see `effect_ext`.
    Ext(crate::effect_ext::ExtEffect),
    /// Ninjutsu resolution (CR 702.49a/c): put the source card from its owner's hand onto the
    /// battlefield tapped and attacking what the returned creature attacked.
    NinjutsuEnter,
    /// "Enter as a copy of" a chosen object (CR 707; Forge `Clone` in an ETB replacement): the
    /// entering object copies one of `choices` in `zone` (none if `optional` and declined).
    Clone { choices: Filter, zone: crate::state::Zone, optional: bool, add_types: TypeSet, add_subtypes: Vec<Subtype>, kws: KwSet, set_pt: Option<(i32, i32)>, keep_name: bool, remember_origin: bool },
    /// Create a token that's a copy of each affected permanent (CR 707.2, 111.1).
    CopyPermanent,
    /// Return the card exiled to pay this ability's cost to the battlefield transformed under its
    /// owner's control (craft, CR 702.167a).
    ReturnTransformed,
    /// Attach the source Equipment/Aura to the target (CR 701.3; equip, 702.6a).
    Attach,
    /// Resolution of an Aura spell (CR 608.3, 303.4a): it enters attached to its target.
    AuraEnter,
    /// Give (`true`) or remove the "prepared" designation (CR 722.3a–b; Forge `AlterAttribute |
    /// Attributes$ Prepared`).
    SetPrepared(bool),
    /// Change characteristics for a duration (layers 4–7b).
    Animate { power: Option<Expr>, toughness: Option<Expr>, types: TypeSet, subtypes: Vec<Subtype>, remove_card_types: bool, remove_creature_types: bool, colors: Option<ColorSet>, overwrite_colors: bool, kws: KwSet, abilities: Vec<AbilityId>, duration: Duration },
    /// Effects implied by alternative-casting keywords (warp exile, evoke sacrifice, ...).
    Alt(crate::altcast::AltEffect),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Cost {
    pub mana: ManaCost,
    pub tap: bool,
    pub sac_self: bool,
    pub pay_life: i32,
    /// Sacrifice N permanents you control matching the filter.
    pub sac: Option<(u8, Filter)>,
    /// Discard N cards matching the filter.
    pub discard: Option<(u8, Filter)>,
    pub discard_self: bool,
    /// Exile N cards from your graveyard matching the filter.
    pub exile_from_grave: Option<(u8, Filter)>,
    /// Tap N untapped permanents you control matching the filter (`tapXType`).
    pub tap_x: Option<(u8, Filter)>,
    /// Put (+N) or remove (-N) counters of a kind on/from the source (`AddCounter<N/K>`,
    /// `SubCounter<N/K>`). A loyalty cost makes this a loyalty ability (CR 606.2).
    pub counter: Option<(CounterKind, i32)>,
    /// Return an unblocked attacking creature you control to its owner's hand (ninjutsu,
    /// CR 702.49a).
    pub return_unblocked: bool,
    /// Exile the source (craft, CR 702.167a).
    pub exile_self: bool,
    /// Exile N objects matching the filter from among permanents you control and/or cards in
    /// your graveyard (craft materials, CR 702.167a–b).
    pub exile_ctrl_or_grave: Option<(u8, Filter)>,
    /// Tap other untapped creatures you control with total power N or more (crew, 702.122a).
    pub crew: Option<u8>,
    /// Remove N counters of any kinds from the source (`RemoveAnyCounter<N/Any/CARDNAME>`).
    #[serde(default)]
    pub remove_any: u8,
    /// "[-X]": remove X loyalty counters, X announced on activation (`counter` is
    /// `(Loyalty, 0)`; CR 606.4, 107.3).
    #[serde(default)]
    pub x_loyalty: bool,
    /// Craft with N *or more* materials (`exile_ctrl_or_grave` is the minimum; `XMin`).
    #[serde(default)]
    pub craft_more: bool,
}
impl Cost {
    /// CR 606.2: an activated ability with a loyalty symbol in its cost is a loyalty ability.
    pub fn is_loyalty(&self) -> bool {
        matches!(self.counter, Some((CounterKind::Loyalty, _)))
    }
    /// Costs beyond mana/tap that require choosing objects.
    pub fn has_choices(&self) -> bool {
        self.sac.is_some() || self.discard.is_some() || self.exile_from_grave.is_some() || self.tap_x.is_some() || self.return_unblocked || self.exile_ctrl_or_grave.is_some() || self.crew.is_some() || self.remove_any > 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnlessCost {
    Life(i32),
    Mana(ManaCost),
    /// Generic mana given by an expression (`UnlessCost$ X`).
    Generic(Expr),
    /// Behold a matching card/permanent (CR 701.4).
    Behold(Filter),
    /// `UnlessSwitched$ True`: the effect happens only *if* the payer pays.
    Switched(Box<UnlessCost>),
    /// Life given by an expression, evaluated as the effect resolves ("Ward—Pay life equal to
    /// Raubahn's power").
    LifeExpr(Expr),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AbilityKind {
    Spell,
    Activated,
    /// Effect body of a trigger / sub-ability / replacement.
    Sub,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AbilityDef {
    pub kind: AbilityKind,
    pub effect: Effect,
    /// Who the effect applies to when not targeting.
    pub defined: Option<Defined>,
    pub target: Option<TargetSpec>,
    pub cost: Option<Cost>,
    pub sorcery_speed: bool,
    pub is_mana: bool,
    /// Activation restriction (`IsPresent$` ...).
    pub activate_if: Vec<Condition>,
    /// Resolution conditions: if any fails, this node does nothing (`Condition*$`).
    pub conditions: Vec<Condition>,
    /// "… unless [payer] pays [cost]" (`UnlessCost$`, `UnlessPayer$`); the payer chooses.
    pub unless: Option<(UnlessCost, Defined)>,
    /// "You may": controller chooses at resolution (`Optional$ True`).
    pub optional: bool,
    /// Remember the objects affected, for later nodes' `Defined$ Remembered`.
    pub remember: bool,
    pub sub: Option<AbilityId>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum TrigMode {
    /// A matching object moves between zones (`ChangesZone`).
    ChangesZone { origin: ZoneSet, dest: ZoneSet, valid: Filter },
    /// Beginning of a step (`Phase`).
    Phase { step: Step, player: PlayerSel },
    /// A matching creature attacks.
    Attacks { valid: Filter },
    /// A matching spell is cast.
    SpellCast { valid: Filter, caster: PlayerSel },
    /// A matching source deals damage to a matching recipient.
    DamageDone { source: Filter, target_player: Option<PlayerSel>, target_obj: Option<Filter>, combat_only: bool },
    LifeGained { player: PlayerSel },
    /// A matching permanent becomes the target of a spell/ability controlled by `by` (ward).
    BecomesTarget { valid: Filter, by: PlayerSel },
    /// Trigger modes added later (see `trig_ext`).
    Ext(crate::trig_ext::TrigExt),
    /// Never matches an event: put on the stack directly by rules (delayed triggers created by
    /// keywords such as warp and mobilize; see `altcast`).
    Delayed,
    /// Saga chapter ability N (CR 714.2b): lore counters on this went from < N to >= N.
    Chapter { n: u8 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerDef {
    pub mode: TrigMode,
    /// Zones the source must be in for this to trigger (`TriggerZones$`, default Battlefield).
    pub zones: ZoneSet,
    pub execute: AbilityId,
    pub optional: bool,
    /// Intervening "if" clause (CR 603.4): checked when triggering and on resolution.
    pub intervening: Vec<Condition>,
    /// Max triggers per turn (`ActivationLimit$`).
    pub per_turn: Option<u8>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ReplAction {
    /// Modify the entering object (tapped, counters, ...) by running this ability on it.
    Modify(AbilityId),
    /// Move it to another zone instead (Rest in Peace).
    Redirect(crate::state::Zone),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplacementDef {
    /// A matching object would move from `origin` to `dest`.
    pub origin: ZoneSet,
    pub dest: ZoneSet,
    pub valid: Filter,
    /// Zones the source must be in (`ActiveZones$`); `None` = self-replacement of the moving card.
    pub zones: Option<ZoneSet>,
    pub action: ReplAction,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum StaticKind {
    /// Layer 6/7 continuous effect on matching permanents.
    Continuous { affected: Filter, power: Option<Expr>, toughness: Option<Expr>, set_power: Option<Expr>, set_toughness: Option<Expr>, kws: KwSet },
    /// Spells matching `valid` cast by `caster` cost `amount` generic less.
    ReduceCost { valid: Filter, caster: PlayerSel, amount: Expr },
    /// Matching attackers can't be blocked by matching blockers.
    CantBlockBy { attacker: Filter, blocker: Option<Filter> },
    /// Damage (combat damage only: `combat_only`, Frenzied Baloth) can't be prevented (CR
    /// 615.12); the engine's only prevention is a shield counter's (CR 122.1c,
    /// `resolve::deal_damage`), read by `static_ext::damage_cant_be_prevented`.
    CantPreventDamage {
        #[serde(default)]
        combat_only: bool,
    },
    /// The effect's controller may play matching cards from `zones` (impulse draw).
    MayPlay {
        affected: Filter,
        zones: ZoneSet,
        /// "... and mana of any type can be spent to cast it" (`MayPlayIgnoreType$`, CR 609.4b;
        /// `ext7::any_type_permission`).
        #[serde(default)]
        any_type: bool,
    },
    /// Matching permanents lose all abilities (layer 6).
    RemoveAllAbilities { affected: Filter },
    /// Statics added later (see `static_ext`).
    Ext(crate::static_ext::StaticExt),
    /// Casting-related statics (optional additional costs, can't-be-cast): `altcast`.
    Alt(crate::altcast::AltStatic),
    /// Continuous effect on a defined object instead of a filtered set (`AffectedDefined$`):
    /// the source itself or the permanent it is attached to (equipped/enchanted). `affected`
    /// further restricts it (`Affected$ Creature`). Layers 4 (types), 6 (keywords), 7b/7c (P/T).
    DefinedContinuous(Box<DefinedCont>),
}

/// Which object an `AffectedDefined$` static applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StaticWho {
    SelfObj,
    /// The permanent the source is attached to (`Equipped`, `Enchanted`).
    Attached,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DefinedCont {
    pub who: StaticWho,
    pub affected: Option<Filter>,
    pub add_types: TypeSet,
    pub add_subtypes: Vec<Subtype>,
    pub remove_card_types: bool,
    pub power: Option<Expr>,
    pub toughness: Option<Expr>,
    pub set_power: Option<Expr>,
    pub set_toughness: Option<Expr>,
    pub kws: KwSet,
    /// Granted ward cost (`AddKeyword$ Ward:N`); implemented as a ward trigger on the source
    /// matching the affected object (see compile).
    pub ward: Option<String>,
    /// Activated abilities granted (layer 6, `AddAbility$`; e.g. a max speed ability).
    #[serde(default)]
    pub abilities: Vec<AbilityId>,
    /// "Hexproof from monocolored" (CR 702.11d; `AddKeyword$ Hexproof:Card.MonoColor`).
    #[serde(default)]
    pub hexproof_from_mono: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StaticDef {
    pub kind: StaticKind,
    /// Zones the source must be in (`EffectZone$`, default Battlefield).
    pub zones: ZoneSet,
    pub conditions: Vec<Condition>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FaceDef {
    pub name: String,
    pub cost: ManaCost,
    pub types: TypeSet,
    pub supertypes: Supertypes,
    pub subtypes: Vec<Subtype>,
    pub colors: ColorSet,
    pub power: Option<i32>,
    pub toughness: Option<i32>,
    pub loyalty: Option<i32>,
    pub kws: KwSet,
    pub spell: Option<AbilityId>,
    pub activated: Vec<AbilityId>,
    pub triggers: Vec<TriggerDef>,
    pub statics: Vec<StaticDef>,
    pub replacements: Vec<ReplacementDef>,
    /// `etbCounter:TYPE:N` keyword.
    pub etb_counters: Vec<(CounterKind, i32)>,
    /// `Kicker:<cost>`.
    pub kicker: Option<ManaCost>,
    /// `Flashback:<cost>` (CR 702.34).
    pub flashback: Option<ManaCost>,
    /// Activated abilities that function from the hand (cycling, CR 702.29).
    pub hand_abilities: Vec<AbilityId>,
    /// Activated abilities that function from the graveyard (`ActivationZone$ Graveyard`,
    /// CR 113.6k / 602.1).
    #[serde(default)]
    pub grave_abilities: Vec<AbilityId>,
    /// Characteristic-defining power/toughness (CR 604.3, 613.4a), evaluated in every zone.
    #[serde(default)]
    pub cda_pt: Option<(Expr, Expr)>,
    /// Alternative/additional casting costs and permissions (warp, evoke, spree, ...).
    pub alt: crate::altcast::AltDef,
    /// `Enchant <quality>` (CR 702.5a): what this Aura can enchant.
    pub enchant: Option<Filter>,
    /// "If this card is in your opening hand, you may begin the game with it on the
    /// battlefield" (CR 103.6a; Forge `MayEffectFromOpeningHand`).
    pub opening_hand_battlefield: bool,
    pub oracle: String,
    /// Later additions (kept in one struct so new face properties don't touch every site).
    #[serde(default)]
    pub extra: FaceExtra,
}

/// Face properties added by the long-tail work (`compile.rs` keywords).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FaceExtra {
    /// `etbCounter:TYPE:X`: enters with X counters, X paid for it (CR 107.3m, 122.6).
    pub etb_counters_x: Vec<CounterKind>,
    /// Start your engines! (CR 702.179a).
    pub start_engines: bool,
    /// Storied (CR 702.195a).
    pub storied: bool,
    /// "As an additional cost to cast this spell, you may sacrifice any number of [matching]
    /// permanents. This spell costs {1} less to cast for each permanent sacrificed this way."
    /// (Rottenmouth Viper; Forge `SP$ PermanentCreature | Cost$ .. Sac<X/..>` + a relative
    /// `ReduceCost`.)
    pub sac_any_reduce: Option<Filter>,
    /// Changeling (CR 702.73a): a characteristic-defining ability, "this object is every
    /// creature type", in every zone.
    pub changeling: bool,
    /// `etbCounter:TYPE:<SVar>`: enters with that many counters, the expression evaluated as it
    /// enters (CR 614.1c; Callous Sell-Sword).
    pub etb_counters_expr: Vec<(CounterKind, crate::expr::Expr)>,
    /// Paradigm (CR 702.192a; `ext7`).
    #[serde(default)]
    pub paradigm: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Layout {
    Normal,
    /// A Room (CR 709.5): a split permanent card with doors. Faces: 0 left half, 1 right half,
    /// 2 both halves (both unlocked; also the card's combined characteristics outside the
    /// battlefield and stack, 709.4), 3 neither (both locked). On the battlefield the face
    /// index is the unlocked designations (`ext5::room_face`).
    Room,
    /// CR 715: face 1 is the Adventure.
    Adventure,
    /// CR 720: face 1 is the Omen (shuffled into the library as it resolves).
    Omen,
    /// CR 722: face 1 is the prepare spell (copies of it may be cast while prepared).
    Prepare,
    /// CR 712: a transforming double-faced card; face 1 is the back face.
    Transform,
    /// CR 709: a split card (not a Room): faces 0 and 1 are its halves, each cast on its own
    /// (709.3); face 2 is the combination of both, its characteristics outside the stack
    /// (709.4). No fuse (`ext9`).
    Split,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CardDef {
    pub name: String,
    pub layout: Layout,
    pub faces: Vec<FaceDef>,
    pub is_token: bool,
    /// Reasons this card can't be played faithfully yet. Empty = playable.
    pub unsupported: Vec<String>,
}
impl CardDef {
    pub fn playable(&self) -> bool {
        self.unsupported.is_empty()
    }
    pub fn front(&self) -> &FaceDef {
        &self.faces[0]
    }
    pub fn face(&self, i: u8) -> &FaceDef {
        &self.faces[(i as usize).min(self.faces.len() - 1)]
    }
}

/// Identity of a `CardDb` instance for caches that outlive one borrow (`query::frozen`):
/// unique per instance and never reused (a clone gets a new one); assigned on first use.
#[derive(Debug, Default)]
pub struct DbId(std::sync::atomic::AtomicU64);

impl Clone for DbId {
    fn clone(&self) -> DbId {
        DbId::default()
    }
}

impl DbId {
    pub fn get(&self) -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let v = self.0.load(Relaxed);
        if v != 0 {
            return v;
        }
        let n = NEXT.fetch_add(1, Relaxed);
        match self.0.compare_exchange(0, n, Relaxed, Relaxed) {
            Ok(_) => n,
            Err(cur) => cur,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CardDb {
    /// Instance identity (see `DbId`).
    #[serde(skip)]
    pub id: DbId,
    pub cards: Vec<CardDef>,
    pub abilities: Vec<AbilityDef>,
    pub subtypes: Vec<String>,
    #[serde(skip)]
    pub(crate) subtype_ix: HashMap<String, Subtype>,
    #[serde(skip)]
    pub(crate) by_name: HashMap<String, CardId>,
    #[serde(skip)]
    pub(crate) tokens: HashMap<String, CardId>,
    #[serde(skip)]
    pub(crate) tokenscripts_dir: Option<std::path::PathBuf>,
    /// Names of counter kinds without rules meaning (`CounterKind::Named`).
    pub counter_names: Vec<String>,
    /// Creature types (CR 205.3m), for "loses all creature types".
    pub creature_types: Vec<Subtype>,
    /// Static abilities defined in SVars and used by effect objects (`CreateEffect`).
    pub statics_pool: Vec<StaticDef>,
    /// Mana abilities whose mana may only be spent on matching spells (`RestrictValid$`), and
    /// whether that spell can't be countered (`AddsNoCounter$`).
    pub mana_restrict: Vec<(AbilityId, Filter, bool)>,
    /// Land types (CR 205.3i), for "loses all land types".
    pub land_types: Vec<Subtype>,
    /// Intrinsic "{T}: Add {W}" .. "{T}: Add {G}" abilities for basic land types gained by
    /// effects (CR 305.6), in WUBRG order; created when first needed.
    pub basic_land_mana: Vec<AbilityId>,
    /// Statics produced while compiling other parts of the current face (e.g. "can't be
    /// countered" replacements); drained into the face's statics.
    #[serde(skip)]
    pub(crate) pending_statics: Vec<StaticDef>,
    /// Triggered abilities of effect objects (`Effect | Triggers$`).
    pub triggers_pool: Vec<TriggerDef>,
    /// Activated abilities usable only N times each turn (`ActivationLimit$`).
    pub activation_limit: Vec<(AbilityId, u8)>,
    /// Exhaust abilities (CR 702.177a: activate only once per object).
    #[serde(default)]
    pub exhaust: Vec<AbilityId>,
    /// Spells with "As an additional cost to cast this spell, pay X life" (X announced, 601.2b).
    #[serde(default)]
    pub spell_life_x: Vec<AbilityId>,
    /// Charms choosing `a` modes if the optional additional cost was paid, else `b`:
    /// (charm node, a, b).
    #[serde(default)]
    pub charm_optcost: Vec<(AbilityId, u8, u8)>,
    /// Implicit target of the ability being compiled (keyword actions like earthbend).
    #[serde(skip)]
    pub(crate) pending_target: Option<TargetSpec>,
    /// While compiling an ability chain: whether something before the current node (or the
    /// trigger / delayed trigger / effect running it) remembers objects for it
    /// (`compile::ability_inner`, `compile_audit`).
    #[serde(skip)]
    pub(crate) rem_avail: bool,
    /// Set by `compile_static` when it split a "you have hexproof" part off the static it
    /// returned (`StaticExt::PlayerHexproof`); taken by `compile::take_player_hexproof`.
    #[serde(skip)]
    pub(crate) pending_player_hexproof: bool,
    /// Additional costs of modes (spree/tiered `ModeCost$`), by mode ability.
    pub mode_costs: Vec<(AbilityId, ManaCost)>,
    /// Transforming double-faced cards (CR 712): (front card, back-face card). The back face is
    /// also compiled as its own (non-deck) card so a transformed permanent can use it.
    #[serde(default)]
    pub dfc_backs: Vec<(CardId, CardId)>,
    /// `ChangeZone | WithCountersType$`: the moved objects enter with these counters (CR 122.6).
    #[serde(default)]
    pub enter_counters: Vec<(AbilityId, CounterKind, i32)>,
    /// Trigger bodies `AB$ ... | Cost$ C` ("you may [C]. If you do, [this node and its
    /// sub-abilities]"): if C isn't paid, the whole chain from this node does nothing (CR 608.2c;
    /// Forge pays a trigger's cost before resolving any of it). `UnlessSwitched` resolution costs
    /// gate only their own node (Forge `UnlessResolveSubs` defaults to always).
    #[serde(default)]
    pub cost_gates_sub: Vec<AbilityId>,
    /// Activated abilities that cost less (`ReduceCost$ X` on the ability): generic reduction,
    /// evaluated with the chosen targets (CR 601.2f).
    #[serde(default)]
    pub ability_reduce: Vec<(AbilityId, Expr)>,
    /// Some loaded card grants "hexproof from" (skip the targeting check otherwise).
    #[serde(default)]
    pub any_hexproof_from: bool,
    /// `ChangeZone | Destination$ Library | LibraryPosition$ N`: N cards from the top (0 = top).
    #[serde(default)]
    pub library_position: Vec<(AbilityId, u8)>,
    /// Some loaded card has a static reducing activated abilities' costs.
    #[serde(default)]
    pub any_ability_reduce: bool,
    /// Some loaded card has a `ChangesZoneAll` ("one or more") trigger.
    #[serde(default)]
    pub any_zone_batch: bool,
    /// `ChangeZone | WithTotalCMC$ N`: the chosen cards' total mana value is at most N.
    #[serde(default)]
    pub total_mv: Vec<(AbilityId, i32)>,
    /// `ChangeZone | Hidden | Mandatory$ True` choices that must take as many cards as possible
    /// (no "Done" before that; CR 701.23d), see `compile.rs`.
    #[serde(default)]
    pub search_all: Vec<AbilityId>,
    /// `ChangeZone` nodes that put objects onto the battlefield "under your control" (the
    /// resolving ability's controller; CR 109.5, 110.2a) instead of their owners' (earthbend's
    /// delayed return, CR 701.66a).
    #[serde(default)]
    pub bf_under_you: Vec<AbilityId>,
    /// Hidden `ChangeZone` choices from public zones only without `DefinedPlayer$`: every
    /// player's cards in those zones are candidates (Forge `game.getCardsIn(origin)`),
    /// not only the searcher's.
    #[serde(default)]
    pub hidden_all_owners: Vec<AbilityId>,
    /// Side tables of the third long-tail batch (`ext3`).
    #[serde(default)]
    pub lt: crate::ext3::LtTables,
    /// Side tables of the fourth long-tail batch (`ext4`).
    #[serde(default)]
    pub mr: crate::ext4::MrTables,
    /// Side tables of the fifth long-tail batch (`ext5`).
    #[serde(default)]
    pub le: crate::ext5::LeTables,
    /// Side tables of the sixth long-tail batch (`ext6`).
    #[serde(default)]
    pub x6: crate::ext6::XTables,
    /// Side tables of the seventh long-tail batch (`ext7`).
    #[serde(default)]
    pub x7: crate::ext7::X7Tables,
    /// Side tables of the eighth batch (`ext8`).
    #[serde(default)]
    pub x8: crate::ext8::X8Tables,
    /// Side tables of the ninth batch (`ext9`).
    #[serde(default)]
    pub x9: crate::ext9::X9Tables,
    /// Per card, built on first use (`life_watch`): whose life loss / life total its Oracle text
    /// may watch (`cast::oracle_watches_life` bits).
    #[serde(skip)]
    pub(crate) life_watch_cache: std::sync::OnceLock<Vec<u8>>,
}

/// The part of a CR list item after its introduction (" are ", ": ", " is ").
fn last_after(w: &str) -> &str {
    let w = w.rsplit(" are ").next().unwrap();
    let w = w.rsplit(": ").next().unwrap();
    w.rsplit(" is ").next().unwrap().trim()
}

/// The official subtype lists from the CR (205.3g–q), so filters can tell types from typos.
fn cr_subtypes() -> Vec<String> {
    let cr = include_str!("../../../docs/cr/MagicCompRules-20260925.txt");
    let mut out = vec![];
    for line in cr.lines() {
        let l = line.trim();
        for pre in ["205.3g", "205.3h", "205.3i", "205.3j", "205.3k", "205.3m", "205.3n", "205.3p", "205.3q"] {
            if let Some(rest) = l.strip_prefix(pre) {
                if let Some(i) = rest.find(" are ").or_else(|| rest.find(" is ")) {
                    for w in rest[i + 4..].split(',') {
                        let w = w.trim().trim_start_matches("and ").trim_end_matches('.');
                        // The first item follows the list's introduction ("... The land types are
                        // Cave", "... one word long: Advisor", "That dungeon type is Undercity").
                        let w = last_after(w);
                        let w = w.split(" (").next().unwrap().trim();
                        if !w.is_empty() && w.len() < 30 && w.chars().next().unwrap().is_uppercase() {
                            out.push(w.to_string());
                        }
                    }
                }
            }
        }
    }
    out
}

/// Number of subtypes from the CR lists. `CardDb::new()` interns them first and in this order, so
/// a `Subtype` below this count names the same type in every CardDb (a stable key, e.g. for the
/// encoding's subtype embedding); ids at or above it depend on the cards loaded.
pub fn cr_subtype_count() -> usize {
    static N: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *N.get_or_init(|| CardDb::new().subtypes.len())
}

impl CardDb {
    pub fn new() -> CardDb {
        let mut db = CardDb::default();
        for s in cr_subtypes() {
            db.intern_subtype(&s);
        }
        let cr = include_str!("../../../docs/cr/MagicCompRules-20260925.txt");
        if let Some(line) = cr.lines().find(|l| l.starts_with("205.3i")) {
            if let Some(i) = line.find(" are ") {
                for w in line[i + 5..].split(". Of").next().unwrap().split(',') {
                    let w = last_after(w.trim().trim_start_matches("and ").trim_end_matches('.'));
                    if let Some(st) = db.subtype(w) {
                        db.land_types.push(st);
                    }
                }
            }
        }
        if let Some(line) = cr.lines().find(|l| l.starts_with("205.3m")) {
            if let Some(i) = line.find(" are ") {
                for w in line[i + 5..].split(',') {
                    let w = last_after(w.trim().trim_start_matches("and ").trim_end_matches('.'));
                    let w = w.split(" (").next().unwrap().trim();
                    if let Some(st) = db.subtype(w) {
                        db.creature_types.push(st);
                    }
                }
            }
        }
        db
    }
    pub fn intern_subtype(&mut self, s: &str) -> Subtype {
        if let Some(&i) = self.subtype_ix.get(s) {
            return i;
        }
        let i = self.subtypes.len() as Subtype;
        self.subtypes.push(s.to_string());
        self.subtype_ix.insert(s.to_string(), i);
        i
    }
    pub fn subtype(&self, s: &str) -> Option<Subtype> {
        self.subtype_ix.get(s).copied()
    }
    pub fn get(&self, id: CardId) -> &CardDef {
        &self.cards[id.0 as usize]
    }
    /// Whose life loss or life total may card `id` trigger on or count (agent mana filter, life costs;
    /// `cast::life_loss_watcher`): `cast::LIFE_YOU` / `LIFE_OPP` bits from the Oracle text of
    /// every face (`cast::oracle_watches_life`; 0 = nobody's). Cached for the cards loaded when
    /// first asked; cards added later are classified on each call.
    pub fn life_watch(&self, id: CardId) -> u8 {
        let of = |c: &CardDef| c.faces.iter().fold(0, |m, f| m | crate::cast::oracle_watches_life(&f.oracle));
        let v = self.life_watch_cache.get_or_init(|| self.cards.iter().map(of).collect());
        v.get(id.0 as usize).copied().unwrap_or_else(|| of(self.get(id)))
    }
    /// The back-face card of a transforming double-faced card (CR 712).
    pub fn back_of(&self, front: CardId) -> Option<CardId> {
        self.dfc_backs.iter().find(|x| x.0 == front).map(|x| x.1)
    }
    pub fn ability(&self, id: AbilityId) -> &AbilityDef {
        &self.abilities[id.0 as usize]
    }
    pub fn by_name(&self, name: &str) -> Option<CardId> {
        self.by_name.get(&name_key(name)).copied()
    }
    pub fn counter_kind(&mut self, s: &str) -> CounterKind {
        match s {
            "P1P1" => CounterKind::P1P1,
            "M1M1" => CounterKind::M1M1,
            "LOYALTY" => CounterKind::Loyalty,
            "STUN" => CounterKind::Stun,
            other => match self.counter_names.iter().position(|n| n == other) {
                Some(i) => CounterKind::Named(i as u16),
                None => {
                    self.counter_names.push(other.to_string());
                    let k = CounterKind::Named((self.counter_names.len() - 1) as u16);
                    self.lt_note_counter(k, other);
                    k
                }
            },
        }
    }
    pub(crate) fn push_ability(&mut self, a: AbilityDef) -> AbilityId {
        let id = AbilityId(self.abilities.len() as u32);
        self.abilities.push(a);
        id
    }

    /// Load cards by name from a Forge cardsfolder (indexes the folder once).
    pub fn load_forge(cardsfolder: &std::path::Path, names: &[&str]) -> Result<CardDb, String> {
        let index = forge_index(cardsfolder)?;
        let mut db = CardDb::new();
        db.tokenscripts_dir = cardsfolder.parent().map(|p| p.join("tokenscripts"));
        for n in names {
            if db.by_name(n).is_some() {
                continue;
            }
            let path = index.get(&name_key(n)).ok_or_else(|| format!("no Forge script for {n:?}"))?;
            let src = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
            let script_name = src.lines().find_map(|l| l.strip_prefix("Name:")).unwrap_or(n).trim().to_string();
            let src = crate::script_fixes::apply(&script_name, src)?;
            let mut script = forge_script::parse(&src).map_err(|e| format!("{n}: {e}"))?;
            // `CopyFaceFrom:<card>` (prepare spells): that face is the named card's front face.
            for i in 0..script.faces.len() {
                if let Some(from) = script.faces[i].copy_face_from.clone() {
                    let p2 = index.get(&name_key(&from)).ok_or_else(|| format!("no Forge script for {from:?}"))?;
                    let other = forge_script::parse(&std::fs::read_to_string(p2).map_err(|e| e.to_string())?).map_err(|e| format!("{from}: {e}"))?;
                    script.faces[i] = other.faces[0].clone();
                }
            }
            db.add(&script);
        }
        Ok(db)
    }

    /// Load (once) a token definition from Forge's tokenscripts folder.
    pub(crate) fn token(&mut self, script: &str) -> Result<CardId, String> {
        if let Some(&id) = self.tokens.get(script) {
            return Ok(id);
        }
        let dir = self.tokenscripts_dir.clone().ok_or("no tokenscripts dir")?;
        let src = std::fs::read_to_string(dir.join(format!("{script}.txt"))).map_err(|_| format!("token script {script} missing"))?;
        let parsed = forge_script::parse(&src).map_err(|e| format!("token {script}: {e}"))?;
        let id = self.add_token(&parsed);
        self.tokens.insert(script.to_string(), id);
        Ok(id)
    }
}

/// Case- and diacritic-insensitive name key (decklists often drop accents: Dain vs Dáin).
pub fn name_key(s: &str) -> String {
    name_key_chars(s).collect()
}

/// The characters of `name_key(s)`, without allocating.
fn name_key_chars(s: &str) -> impl Iterator<Item = char> + '_ {
    s.trim().chars().flat_map(|ch| {
        let base = match ch {
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'Á' | 'À' | 'Â' | 'Ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' | 'É' => 'e',
            'í' | 'ì' | 'î' | 'ï' | 'Í' => 'i',
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'Ó' | 'Ö' => 'o',
            'ú' | 'ù' | 'û' | 'ü' | 'Ú' | 'Û' => 'u',
            'ñ' => 'n',
            c => c,
        };
        base.to_lowercase()
    })
}

/// `name_key(a) == name_key(b)` without allocating (hot in filters such as `NamedCard`).
pub fn name_key_eq(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    // ASCII names (nearly all): `name_key` is trim + ASCII lowercase there (no accent maps, and
    // `char::to_lowercase` of an ASCII char is its ASCII lowercase), so this is the same test
    // without the per-char iterator chains (hot: a "permanents named X" count asks it per
    // permanent per static).
    if a.is_ascii() && b.is_ascii() {
        return a.trim().eq_ignore_ascii_case(b.trim());
    }
    name_key_chars(a).eq(name_key_chars(b))
}

/// Map name_key → script path for every card in a Forge cardsfolder (deterministic order).
pub fn forge_index(dir: &std::path::Path) -> Result<HashMap<String, std::path::PathBuf>, String> {
    let mut idx = HashMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&d).map_err(|e| format!("{}: {e}", d.display()))?.filter_map(|e| e.ok()).map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "txt") {
                let src = std::fs::read_to_string(&p).unwrap_or_default();
                if let Some(l) = src.lines().find(|l| l.starts_with("Name:")) {
                    idx.entry(name_key(&l[5..])).or_insert(p);
                }
            }
        }
    }
    Ok(idx)
}
