//! Observations: everything a player legitimately knows, and nothing else. This is the only
//! view of the game agents and front-ends get. Hidden cards appear with `card: None`.
//!
//! Invariant (checked by tests): resampling hidden information consistently with what
//! the observer knows leaves their observation unchanged.

use crate::basics::*;
use crate::engine::{Decision, DecisionKind, Game};
use crate::query::chars;
use crate::state::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjView {
    pub id: ObjId,
    /// None if this object is hidden from the observer.
    pub card: Option<CardId>,
    pub owner: PlayerId,
    pub controller: PlayerId,
    pub tapped: bool,
    pub damage: i32,
    pub power: Option<i32>,
    pub toughness: Option<i32>,
    pub counters: Vec<(CounterKind, i32)>,
    pub token: bool,
    pub sick: bool,
    pub attacking: bool,
    pub blocking: Option<ObjId>,
    /// An attacking creature that is blocked (CR 509.1h: it stays blocked even if every
    /// creature blocking it has been removed from combat; `Combat::blocked`). False for a
    /// creature that isn't attacking. Public (the declaration of blockers is, 509.1).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub blocked: bool,
    /// Public choices and links: the creature/land type chosen for it, the card name chosen
    /// for it, the permanent it was exiled with, whether it's a solved Case.
    pub chosen_type: Option<Subtype>,
    pub named: Option<String>,
    pub exiled_with: Option<ObjId>,
    pub solved: bool,
    /// Public casting designations (`altcast::flag`: warped, plotted, cast for warp/evoke...).
    pub alt: u16,
    /// The planeswalker this creature attacks (None: attacking the player, or not attacking).
    pub attacking_pw: Option<ObjId>,
    /// Has the "prepared" designation (CR 722.3a; public).
    pub prepared: bool,
    /// Which face's characteristics it has (0 = front; alternative/transformed faces otherwise).
    pub face: u8,
    /// The permanent this Equipment/Aura is attached to.
    pub attached_to: Option<ObjId>,
    /// For a permanent copying another (CR 707): its own card (`card` is the copied card).
    pub copy_of_printed: Option<CardId>,
    /// Harnessed (CR 701.64b; public).
    pub harnessed: bool,
    /// Current characteristics of a visible object (CR 109.3, after continuous effects): card
    /// types (`TypeSet` bits), keywords (`KwSet` bits) and mana value. 0 / None when hidden.
    #[serde(default)]
    pub types: u16,
    #[serde(default)]
    pub kws: u32,
    #[serde(default)]
    pub mv: Option<u32>,
    /// Picked as an attacker in the declaration in progress (CR 508.1a). Only in the
    /// declaring player's own observation.
    #[serde(default)]
    pub declaring: bool,
    /// Turn number it entered its current zone (public: zone changes are public even when the
    /// card is hidden, e.g. how long an opponent's unknown card has been in hand). v7.
    #[serde(default)]
    pub entered: u32,
    /// A face-down permanent (CR 708; manifest): its characteristics are the face-down 2/2's;
    /// `card` is its real card for a player who may look at it (its controller, 708.5), else
    /// None.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub face_down: bool,
    /// A Class's level once it has gained one (CR 716.2b; public; level 1 when absent, 716.2d).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class_level: Option<u8>,
    /// Ward (CR 702.21a), printed or granted (Equipment, Auras, other statics): what an opponent
    /// targeting it must pay, summed over its ward abilities (702.21b: each triggers). v8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ward: Option<WardView>,
    /// "Hexproof from" qualities (CR 702.11d), `HEXPROOF_FROM_*` bits. v8.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub hexproof_from: u8,
    /// The observer's own cards (hand, graveyard, exile, being cast): restricted pool mana of
    /// theirs (CR 106.6) that could be spent on it as a spell. v8.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub rmana: u8,
    /// A copy whose copy effect gave it another name (CR 707.9b, "except his name is Superior
    /// Spider-Man"): the card whose name it has (`card` is the copied card). Absent otherwise,
    /// also when the name is the copied card's. Not encoded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_card: Option<CardId>,
    /// v9 colour view (visible objects; 0 when hidden): current colours (CR 105.2, after
    /// continuous effects), `ColorSet` bits W U B R G (0 = colourless).
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub colors: u8,
    /// Mana cost symbols of the face it shows (CR 202.1; a copy's copied cost, CR 707.2; none
    /// face down, 708.2): pips W U B R G (hybrid counts for each of its colours, Phyrexian and
    /// {2/C} for its colour), then generic + {C}. X is in neither (mana value has it on the stack).
    #[serde(default, skip_serializing_if = "is_zero_pips")]
    pub pips: [u8; 6],
    /// Colours of mana its current mana abilities can make (printed or gained, CR 106.1, 605):
    /// bits W U B R G C.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub makes: u8,
    /// v10: the stable kind id (`encode::counter_id`, CardDb-independent) of each entry of
    /// `counters`, same order. Counters are public (CR 122). Empty when it has no counters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub counter_ids: Vec<u8>,
    /// v10.1, permanents: the part of its current characteristics that ends at this turn's cleanup
    /// step, when the "until end of turn" effects end (CR 514.2; 611.2a: effects of resolved spells
    /// and abilities last as stated). Public: those effects come from resolved spells and
    /// abilities. None when nothing changes. "Until your next turn" effects are not in it (they last
    /// through the next turn; effect objects of theirs are `Observation::effects`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until_eot: Option<EotView>,
    /// v10.1, creatures on the battlefield: can't attack because of an effect or rule other than
    /// being tapped, summoning sickness and defender (`combat::cant_attack_by_effect`), and can't
    /// block at all, whatever the attacker (`combat::cant_block_at_all`: CR 509.1b). Public (the
    /// statics and effects are). Attacker-dependent restrictions are not in it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cant_attack: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cant_block: bool,
    /// v10.2: the card `named` names, in this game's CardDb (None when nothing was named, the
    /// object is hidden, or the DB lacks that card). Public like `named` (e.g. Petrified Hamlet's
    /// "as it enters, choose a land card name", Ancient Vendetta). Encoded as `ef::CARD2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub named_card: Option<CardId>,
    /// v10.2, permanents: "already used" state, per object (a new object after a zone change
    /// starts fresh, CR 400.7). All public: the trigger was put on the stack (603.3), the targeting
    /// spell or ability was announced (601.2c, 602.2b), the exhaust ability was activated (602.2).
    /// `trigger_used`: a triggered ability of it with a per-turn limit ("This ability triggers only
    /// once each turn": Forge `ActivationLimit$` on a trigger, `TriggerDef::per_turn`) has reached
    /// that limit this turn (`TurnStats::trigger_counts`), so it won't trigger again this turn.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub trigger_used: bool,
    /// v10.2, permanents: it became the target of a spell or ability its controller controls this
    /// turn (`ext4::was_targeted_by`): its valiant ability ("Whenever this creature becomes the
    /// target of a spell or ability you control for the first time each turn", an ability word, CR
    /// 207.2c) won't trigger again this turn. Set for any permanent (the condition is the same).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub valiant_used: bool,
    /// v10.2, permanents: an exhaust ability of it was activated (CR 702.177a: "activate each
    /// exhaust ability only once"; `Ext::exhausted`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exhausted: bool,
    /// v10.2, permanents: if it would die this turn, it's exiled instead (Torch the Tower, Fiery
    /// Annihilation, ...; `LtState::dies_exile`, a resolved spell's effect: public).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dies_exile: bool,
}

/// v10.2 (`Observation::pay_unless`): the cost of the pending "unless" payment, as evaluated for
/// it (an X or an expression of the effect already applied).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnlessView {
    /// Mana: its mana value and its symbols (`ObjView::pips` layout).
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub mana: u8,
    #[serde(default, skip_serializing_if = "is_zero_pips")]
    pub pips: [u8; 6],
    /// Life to pay.
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub life: i32,
    /// Another kind of cost (behold).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub other: bool,
}

/// v10.1 (`ObjView::until_eot`): a permanent's characteristics now minus what they will be once
/// this turn's "until end of turn" effects end (CR 514.2), everything else unchanged: the
/// characteristics computed on a copy of the state without those effects (`ContEffect::until_eot`
/// and the effect objects `flow::ends_at_cleanup` removes), so an effect is never counted twice and
/// what it does through other objects (a crewed Vehicle counted by another permanent's static) is
/// included. P/T as `ObjView::power` / `toughness` show them: 0 for a non-creature.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EotView {
    /// Power / toughness that goes away (Giant Growth: +3, +3; a crewed 3/3 Vehicle: +3, +3).
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub power: i32,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub toughness: i32,
    /// Keywords (`KwSet` bits) it has only until end of turn, and keywords it lacks only until then
    /// ("loses all abilities until end of turn").
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub kws: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub lost_kws: u32,
    /// Card types (`TypeSet` bits) it has only until end of turn (a crewed Vehicle, an animated land).
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub types: u16,
}

fn is_zero_u8(x: &u8) -> bool {
    *x == 0
}

fn is_zero_u16(x: &u16) -> bool {
    *x == 0
}

fn is_zero_u32(x: &u32) -> bool {
    *x == 0
}

fn is_zero_i32(x: &i32) -> bool {
    *x == 0
}

fn is_zero_pips(x: &[u8; 6]) -> bool {
    x.iter().all(|&v| v == 0)
}

/// `ObjView::pips` of a mana cost.
pub fn cost_pips(c: &ManaCost) -> [u8; 6] {
    let mut p = [0u8; 6];
    let add = |p: &mut [u8; 6], i: usize, n: u8| p[i] = p[i].saturating_add(n);
    for s in &c.syms {
        match *s {
            ManaSym::Colored(c) | ManaSym::Phyrexian(c) | ManaSym::TwoBrid(c) => add(&mut p, c as usize, 1),
            ManaSym::Hybrid(a, b) => {
                add(&mut p, a as usize, 1);
                add(&mut p, b as usize, 1);
            }
            ManaSym::Generic(n) => add(&mut p, 5, n),
            ManaSym::Colorless | ManaSym::Snow => add(&mut p, 5, 1),
            _ => {}
        }
    }
    p
}

/// `ObjView::makes` bits from abilities: W U B R G C of their `Effect::Mana`.
pub fn mana_bits(db: &crate::CardDb, abilities: &[crate::carddb::AbilityId]) -> u8 {
    use crate::carddb::{Effect, Produced};
    let mut bits = 0u8;
    for &a in abilities {
        if let Effect::Mana { produced, .. } = &db.ability(a).effect {
            match produced {
                Produced::One(m) => bits |= 1 << *m as u8,
                Produced::AnyOf(v) => v.iter().for_each(|&m| bits |= 1 << m as u8),
            }
        }
    }
    bits
}

/// `ObjView::hexproof_from` bit: hexproof from monocolored (CR 702.11d).
pub const HEXPROOF_FROM_MONO: u8 = 1;

/// A permanent's ward costs (CR 702.21a), summed over its ward abilities.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WardView {
    /// Mana value of the mana costs ("Ward {2}").
    pub mana: u8,
    /// Life to pay ("Ward—Pay 3 life"; expression costs evaluated now).
    pub life: u8,
    /// Another kind of cost (behold, a variable generic amount).
    pub other: bool,
}

/// An effect object (`State::effect_objs`): an emblem, or a continuous / triggered effect a
/// resolved spell or ability created ("this turn", "until your next turn"; CR 113.2c, 114,
/// 611.2). Public: they come from resolved spells and abilities. v8.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectView {
    /// The card of the spell / permanent that created it (None if the observer can't see it now).
    pub card: Option<CardId>,
    pub controller: PlayerId,
    pub duration: crate::carddb::EffDuration,
    /// Number of its static abilities and triggered abilities.
    pub statics: u8,
    pub triggers: u8,
    /// The objects it applies to, among those the observer can see.
    pub affected: Vec<ObjId>,
}

/// A delayed triggered ability waiting to trigger (`ext.delayed`, CR 603.7): "at the beginning of the
/// next end step, return it", earthbend's "when it dies or is exiled, return it", "when it dies this
/// turn", Casey Jones's "at the beginning of your next upkeep". Public: created by a resolved spell or
/// ability (603.7a), so its source, controller and condition were seen; the objects it refers to are
/// listed only where the observer can see them (a face-down card they may not look at, a hidden zone:
/// left out, as `EffectView::affected`). v10.2.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelayedView {
    /// The card of the spell / permanent whose ability created it (None: a face-down permanent the
    /// observer may not look at).
    pub card: Option<CardId>,
    pub controller: PlayerId,
    /// The ability that triggers (CardDb ability id; its text is the resolved effect's own).
    pub ability: crate::carddb::AbilityId,
    /// When it triggers.
    pub when: DelayedWhenView,
    /// The objects it refers to (the object whose leaving / dying it waits for, then its
    /// `RememberObjects$`), among those the observer can see; no duplicates.
    pub affected: Vec<ObjId>,
}

/// `ext::DelayedWhen` as the observer sees it (`obj`: the watched object, if visible).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DelayedWhenView {
    /// `obj` leaves the battlefield to a zone of the set (`dies`: a graveyard; `exiled`: exile;
    /// `other`: any other zone).
    Leaves { obj: Option<ObjId>, dies: bool, exiled: bool, other: bool },
    /// `obj` dies this turn ("when it dies this turn"; ends with the turn).
    DiesThisTurnOnly { obj: Option<ObjId> },
    /// The beginning of the next `step` (any player's turn).
    NextStep { step: Step },
    /// The beginning of `player`'s next `step`.
    PlayersNextStep { step: Step, player: PlayerId },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlayerView {
    pub id: PlayerId,
    pub life: i32,
    pub poison: u8,
    pub hand: Vec<ObjView>,
    pub library_size: usize,
    /// Library cards the observer knows, as (position from top, card).
    pub library_known: Vec<(usize, CardId)>,
    /// Object ids of the `library_known` cards (same order), so decision options that refer to
    /// known library cards (search, scry subject) can be matched to them.
    pub library_known_ids: Vec<ObjId>,
    pub graveyard: Vec<ObjView>,
    pub lands_played: u8,
    pub mulligans: u8,
    /// Mana in the player's mana pool by type W U B R G C (CR 106.4; public), restricted mana
    /// included.
    pub pool: [u8; 6],
    /// The part of `pool` with a spending restriction (CR 106.6; "spend this mana only to cast
    /// noncreature spells"). Which cards it can pay for: `ObjView::rmana`. v8.
    #[serde(default)]
    pub pool_restricted: [u8; 6],
    /// Speed (CR 702.179; 0 = none).
    pub speed: u8,
    /// Has an enduring story (CR 702.195b).
    pub enduring_story: bool,
    /// v10: the part of `pool` that stays in the pool when the current step ends: firebending
    /// mana (CR 702.189a) is kept until the end of combat (`ext9::empty_pools`), by type W U B R G
    /// C. Public: the pool and the firebending trigger are.
    #[serde(default, skip_serializing_if = "is_zero_pips")]
    pub pool_kept: [u8; 6],
    /// v10: how many of their next turns the player will skip (CR 614.10; public: the effect
    /// that set it resolved openly, Ral Zarek, Guest Lecturer's coin flips).
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub skip_turns: u8,
    /// v10.1: how many cards at the top / at the bottom of this player's library (each an unbroken
    /// run; the bottom run never includes cards of the top run) the player knows. Public: what a
    /// player learns about their library's cards comes from effects that resolve openly and cards
    /// that move in plain sight (a scry's top / bottom split, CR 701.22a; only the order and the
    /// identities stay hidden, 401.4), and a draw or a shuffle (CR 701.24a) takes it away for
    /// everyone alike. The identities are in `library_known` of the player's own view only.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub owner_knows_top: u8,
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub owner_knows_bottom: u8,
    /// v10.1: life the player lost / gained this turn (`TurnStats`, what "if an opponent lost life
    /// this turn" and "for each 1 life you gained this turn" read; CR 119.3: damage is life loss)
    /// and spells they cast this turn (CR 601.2i). Public: life changes and casts are. A gain doesn't
    /// offset a loss here (Kaito, Bane of Nightmares counts an opponent who paid life and then
    /// gained some back).
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub life_lost_this_turn: i32,
    #[serde(default, skip_serializing_if = "is_zero_i32")]
    pub life_gained_this_turn: i32,
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub spells_cast_this_turn: u8,
    /// v10.2, this turn (`Ext::moved_this_turn`, the zone-change history the rules read; saturating
    /// at 255): creatures that died (CR 700.4) while the player controlled them, all causes; nonland
    /// permanents that left the battlefield under their control (void, CR 207.2c); permanents they
    /// sacrificed (701.21; `Ext::sacrificed_this_turn`); times they descended (permanent cards put
    /// into their graveyard from anywhere, 700.11); spells they cast for warp costs (702.185c). Public:
    /// zone changes to and from the battlefield and graveyards are (400.2), and so are casts.
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub died_this_turn: u8,
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub left_this_turn: u8,
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub sacrificed_this_turn: u8,
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub descended_this_turn: u8,
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub warped_this_turn: u8,
    /// v10.2: the next spell the player casts this turn can't be countered (`LtState::next_uncounterable`,
    /// e.g. Mistrise Village; a resolved ability's effect: public).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub next_uncounterable: bool,
    /// v10.2: the player has hexproof (CR 702.11c; `static_ext::player_has_hexproof`: "you gain
    /// hexproof until end of turn" or a static such as Crystal Barricade's). Public.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hexproof: bool,
    /// Cards outside the game (CR 400.11a) the observer knows: their own (only in games where a
    /// card can bring them in), and the opponent's revealed ones. Their number isn't shown.
    pub sideboard: Vec<ObjView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackView {
    pub card: CardId,
    pub controller: PlayerId,
    pub kind: StackKind,
    pub source: ObjId,
    pub targets: Vec<Target>,
    pub x: i32,
    /// Face of the spell (alternative face for Adventure/Omen/prepare spells).
    pub face: u8,
    /// A spell that can't be countered (CR 101.2): its own "can't be countered", a permanent's
    /// static, or Cavern of Souls-style mana spent on it. Public: the mana and statics are.
    #[serde(default)]
    pub uncounterable: bool,
    /// v9, spells only (abilities have no colour or cost): current colours (`ObjView::colors`)
    /// and the mana cost symbols of the face being cast (`ObjView::pips`).
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub colors: u8,
    #[serde(default, skip_serializing_if = "is_zero_pips")]
    pub pips: [u8; 6],
    /// v10: the item's `StackItem::uid` (what a `Target::Ability` names; public, as the stack is).
    #[serde(default)]
    pub uid: u32,
    /// The item resolving now (`State::resolving`): it stays on the stack until the
    /// final part of its resolution (CR 608.2n; a permanent spell until it enters, 608.3a), so
    /// decisions made as it resolves see it (no player gets priority meanwhile). Items put on
    /// the stack during its resolution (a spell cast as it resolves, 608.2g) are above it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resolving: bool,
    /// The chosen modes of a modal spell or ability (`StackItem::modes`, indices into its
    /// Charm's modes, in order). Public: announced as it is put on the stack (CR 601.2b, 700.2a).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modes: Vec<u8>,
    /// v10.2, spells: an optional additional cost was paid as it was cast (CR 601.2b, 601.2f:
    /// announced and paid openly): kicker (702.33d, `StackItem::kicked`), an `OptionalCost` (gift
    /// 702.174, bargain 702.166, collect evidence 701.59, blight, ...: `altcast::flag::OPTCOST`),
    /// offspring (702.175a) or teamwork (702.194a). A copy has it if the original had it (707.10).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paid_optional: bool,
    /// v10.2, spells: colours of mana spent to cast it (`ColorSet` bits W U B R G; `Ext::spent_colors`:
    /// converge, "if {B} was spent"). Public: mana is paid openly (CR 601.2g-h).
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub spent_colors: u8,
    /// A triggered ability being put on the stack now (`State::being_put`): CR 603.3 "The
    /// ability becomes the topmost object on the stack", and its modes and targets are chosen as
    /// it is put there (603.3c-d, as 601.2b-c for a spell). Always the top item; `targets` are
    /// those of its targeted parts chosen so far, `uid` is 0 (numbered once it is put there).
    /// Public: the stack is (400.2), and modes and targets are announced (601.2b-c).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub being_put: bool,
    /// The spell's owner (CR 112.2: the owner of the card that represents it; a copy's owner is
    /// the player under whose control it was put on the stack), e.g. a spell cast from another
    /// player's card. `None` for an ability (the CR gives abilities a controller only, 113.8).
    /// Public (108.3; the card's owner is known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<PlayerId>,
    /// Which of its source card's abilities an ability on the stack is (`stack_ability_index`;
    /// v10.1): an activated ability's index among the card's activated abilities that
    /// can use the stack (`indexed_activated`: a planeswalker's [+1], [0], [-2] are 0, 1, 2), a
    /// triggered ability's index among the card's triggered abilities (`StackItem::trigger_idx`,
    /// as `TriggerView::trigger`). `None` for a spell, a delayed / reflexive / effect-object
    /// trigger, and an ability the card doesn't have itself (granted by another object). Public:
    /// the ability on the stack has the text of the ability that created it (CR 602.2a, 603.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ability: Option<u8>,
}

/// The activated abilities of `card` that `StackView::ability` indexes, in this order: for each
/// face (`CardDef::faces`, front first), its abilities that function on the battlefield
/// (`FaceDef::activated` as compiled: keyword ones such as Equip / Crew first, then the script's
/// `A:` abilities in script order, which is printed order: a planeswalker's [+1], [0], [-2] are
/// 0, 1, 2), then those that function from the hand (cycling, ninjutsu: Kaito, Bane of
/// Nightmares' ninjutsu is 3), then from the graveyard; mana abilities left out (CR 605.3a: they
/// don't use the stack). Cheap and fixed per card (no Oracle parsing in `observe`); not the
/// scenario interface's `activate:<ref>:<n>` order (Oracle order across zones plus granted
/// ones, `scenario::printed_ability_order`). The card is the item's `source_card`, the object's
/// card when it was activated: a copy's is the copied card (CR 707.2), so a copy's abilities get
/// the original's indices, and a face is counted after the faces before it.
pub fn indexed_activated(db: &crate::carddb::CardDb, card: CardId) -> impl Iterator<Item = crate::carddb::AbilityId> + '_ {
    db.get(card).faces.iter().flat_map(|f| f.activated.iter().chain(&f.hand_abilities).chain(&f.grave_abilities)).copied().filter(move |&a| !db.ability(a).is_mana)
}

/// `StackView::ability` of a stack item (see there).
pub fn stack_ability_index(db: &crate::carddb::CardDb, it: &StackItem) -> Option<u8> {
    match it.kind {
        StackKind::Triggered => it.trigger_idx,
        StackKind::Activated => indexed_activated(db, it.source_card).position(|a| a == it.ability).and_then(|i| u8::try_from(i).ok()),
        _ => None,
    }
}

/// One of the observer's own pending triggers while they order them (CR 603.3b,
/// `DecisionKind::OrderTriggers`): `Opt::Trigger(i)` is `ordering[i]`. Public once on the stack.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerView {
    /// The object whose ability triggered.
    pub source: ObjId,
    pub card: CardId,
    /// Which of the card's triggered abilities (`None`: a delayed / reflexive / effect-object one).
    pub trigger: Option<u8>,
}

/// A permanent entering the battlefield (`Observation::entering`): its `ObjView` fields, and the
/// resolving permanent spell it is (608.3a: on the stack until it enters; its `StackView::source`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnteringView {
    #[serde(flatten)]
    pub view: ObjView,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spell: Option<ObjId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub me: PlayerId,
    pub turn: u32,
    pub active: PlayerId,
    pub step: Step,
    pub stage: Stage,
    pub players: Vec<PlayerView>,
    pub battlefield: Vec<ObjView>,
    pub exile: Vec<ObjView>,
    pub stack: Vec<StackView>,
    /// Spells in the middle of being cast (on the stack zone, not yet stack items; CR 601.2a).
    /// Not a resolving spell (that one is in `stack`, `StackView::resolving`).
    pub casting: Vec<ObjView>,
    /// Permanents in the middle of entering the battlefield while their "as enters" replacement
    /// choices are made (CR 614.12, 614.12a, 614.1c: a copy choice, a shockland's "you may pay
    /// 2 life", a chosen type): the object the permanent will be, as it would exist
    /// on the battlefield (614.12), which the decision's subject (and a `ChooseReplacement`
    /// "its own effect" option) names. Not on the battlefield yet: a permanent spell is still on
    /// the stack meanwhile (608.3a; `EnteringView::spell` names it there), a land being played
    /// is being put onto the battlefield from its hand (116.2a). Public as a permanent is
    /// (`view_permanent`: a face-down one shows no card unless the observer may look at it).
    /// Empty outside such choices.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entering: Vec<EnteringView>,
    /// Objects the observer's own decision names (its subject or options) that are in no zone
    /// any more but left the battlefield this turn: their last known information (CR 113.7a,
    /// 608.2h; `lki`), e.g. a Treasure sacrificed for mana whose mana ability then asks for a
    /// colour (605.3b: no stack item). Public: they were permanents. Only with the observer's
    /// decision.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub last_known: Vec<ObjView>,
    /// The pending decision, only if it is the observer's.
    pub decision: Option<Decision>,
    pub result: Option<GameResult>,
    /// Public history of recent turns (`hist`, v7).
    #[serde(default)]
    pub hist: crate::hist::History,
    /// Identity of the observer's `Subtype` / `Name` options (v7), one per such option.
    #[serde(default)]
    pub choice_info: Vec<ChoiceView>,
    /// Where my targeted Cast / Activate options could legally go right now (v7): one entry per
    /// such option whose spell / ability targets.
    #[serde(default)]
    pub option_targets: Vec<OptTargets>,
    /// The observer's pending triggers that its `OrderTriggers` decision indexes (else empty).
    #[serde(default)]
    pub ordering: Vec<TriggerView>,
    /// Effect objects: emblems and effects of resolved spells / abilities (v8).
    #[serde(default)]
    pub effects: Vec<EffectView>,
    /// Delayed triggered abilities waiting to trigger (v10.2; CR 603.7a: public), in creation order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delayed: Vec<DelayedView>,
    /// What is already chosen in the observer's decision in progress (v8): which targeted part a
    /// `Target` decision is for, the targets of the earlier parts (CR 601.2c), and the items
    /// already picked in a set choice. Default unless the decision is the observer's.
    #[serde(default)]
    pub pick: crate::ext6::Pick,
    /// The observer's own deck: what may still be in their library (a multiset, never an
    /// order) and their sideboard (v9). Only ever the observer's.
    #[serde(default)]
    pub own_deck: OwnDeckView,
    /// v10.2: what the observer's own pending `PayUnless` decision costs ("counter it unless its
    /// controller pays {X}", ward, a shockland's 2 life; `Ext::unless_due`). Only with that decision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pay_unless: Option<UnlessView>,
}

/// What a player legitimately knows about their own cards in hidden zones (encoding v9): their
/// decklist (CR 100.2a: they built it) minus every card of theirs they can see outside the
/// library, and their sideboard (CR 100.4a, 400.11a). Never the library's order (CR 401.2: no
/// player may look at it; positions already known are `PlayerView::library_known`), never the
/// opponent's cards, never a face-down card's identity they may not look at (CR 406.3, 708.5).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnDeckView {
    /// Set by `observe` (false only in observations deserialized from before v9).
    pub known: bool,
    /// Cards that may be in my library, (card, copies) sorted by card id: my library's cards
    /// plus my `unseen` cards outside it (which of these are where is unknown to me).
    pub library: Vec<(CardId, u8)>,
    /// My cards outside the library whose identity I may not see now (exiled face down, a
    /// face-down permanent an opponent controls): `library` holds this many more cards than my
    /// library does.
    pub unseen: u8,
    /// My sideboard (cards outside the game, CR 400.11a), (card, copies) sorted by card id:
    /// the game's registered sideboard (empty when the game got none, e.g. a best-of-one game
    /// without sideboards) as it stands now (a wish takes cards out of it).
    pub sideboard: Vec<(CardId, u8)>,
    /// Summary of `library` from the card definitions (front faces).
    pub stats: DeckStats,
}

/// `OwnDeckView::library` in numbers (front faces; CR 205.2a card types, 202.3 mana value, 105.2
/// colour). Counts saturate at 255.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeckStats {
    pub cards: u8,
    pub lands: u8,
    /// Nonland cards by type, each counted once: creature, else instant, else sorcery, else
    /// other (artifact, enchantment, planeswalker, battle).
    pub creatures: u8,
    pub instants: u8,
    pub sorceries: u8,
    pub other: u8,
    /// Nonland cards by mana value 0, 1, …, 6, 7+.
    pub mv: [u8; 8],
    /// Sum of the nonland cards' mana values.
    pub mv_sum: u16,
    /// Nonland cards of each colour W U B R G, then colourless ones.
    pub colors: [u8; 6],
    /// Lands with a mana ability that can make W U B R G.
    pub land_colors: [u8; 5],
}

/// Sort `cards` and count copies: (card, copies) sorted by card id (copies saturate at 255).
pub fn card_counts(cards: &mut [CardId]) -> Vec<(CardId, u8)> {
    cards.sort_unstable();
    let mut out: Vec<(CardId, u8)> = Vec::with_capacity(cards.len());
    for &c in cards.iter() {
        match out.last_mut() {
            Some(x) if x.0 == c => x.1 = x.1.saturating_add(1),
            _ => out.push((c, 1)),
        }
    }
    out
}

/// Colours of mana (W U B R G bits) a land's own mana abilities can make (`Effect::Mana`).
fn land_mana_colors(db: &crate::CardDb, f: &crate::carddb::FaceDef) -> u8 {
    mana_bits(db, &f.activated) & 0x1f
}

fn deck_stats(db: &crate::CardDb, lib: &[(CardId, u8)]) -> DeckStats {
    let mut st = DeckStats::default();
    let add = |x: &mut u8, n: u8| *x = x.saturating_add(n);
    for &(c, n) in lib {
        let f = db.get(c).front();
        add(&mut st.cards, n);
        if f.types.has(CardType::Land) {
            add(&mut st.lands, n);
            let bits = land_mana_colors(db, f);
            for i in 0..5 {
                if bits & (1 << i) != 0 {
                    add(&mut st.land_colors[i], n);
                }
            }
            continue;
        }
        let slot = if f.types.has(CardType::Creature) {
            &mut st.creatures
        } else if f.types.has(CardType::Instant) {
            &mut st.instants
        } else if f.types.has(CardType::Sorcery) {
            &mut st.sorceries
        } else {
            &mut st.other
        };
        add(slot, n);
        let mv = f.cost.mana_value();
        add(&mut st.mv[(mv as usize).min(7)], n);
        st.mv_sum = st.mv_sum.saturating_add((mv.min(255) as u16).saturating_mul(n as u16));
        if f.colors.is_empty() {
            add(&mut st.colors[5], n);
        }
        for col in f.colors.iter() {
            add(&mut st.colors[col as usize], n);
        }
    }
    st
}

/// `OwnDeckView` of player `me` (see there). Reads hidden state only as a multiset that equals
/// "my decklist minus my cards I can see outside the library": the identities of my library
/// cards and of my cards outside it that I can't see (the leak tests permute exactly those).
fn own_deck(g: &Game, me: PlayerId) -> OwnDeckView {
    let s = g.view_state();
    let pl = s.player(me);
    let mut cards: Vec<CardId> = pl.library.iter().map(|&o| crate::ext6::identity(s, o)).collect();
    let n_lib = cards.len();
    // My cards in public zones I may not look at (CR 406.3, 708.5). My hand is always mine to
    // see and my cards can't be in another player's hand or library (CR 400.3).
    let hidden = |o: ObjId| {
        let ob = s.obj(o);
        ob.owner == me && !ob.token && ob.known_to & (1 << me.0) == 0
    };
    cards.extend(s.exile.iter().copied().filter(|&o| hidden(o)).map(|o| crate::ext6::identity(s, o)));
    if !s.ext.x6.face_down.is_empty() {
        cards.extend(s.battlefield.iter().copied().filter(|&o| hidden(o)).map(|o| crate::ext6::identity(s, o)));
    }
    let unseen = (cards.len() - n_lib).min(255) as u8;
    let library = card_counts(&mut cards);
    let mut side: Vec<CardId> = pl.sideboard.iter().map(|&o| s.obj(o).card).collect();
    for &(c, n) in g.side_static(me) {
        side.extend(std::iter::repeat_n(c, n as usize));
    }
    let sideboard = card_counts(&mut side);
    let stats = deck_stats(&g.db, &library);
    OwnDeckView { known: true, library, unseen, sideboard, stats }
}

/// Legal targets of a Cast / CastFace / Flashback / CastPrepared / Alt (a casting method,
/// `altcast::Method::cast_face`; v10.1) / Activate option's first targeted part, as
/// the engine would offer them now (`cast::legal_targets_in`; battlefield, stack, graveyards,
/// players: all public): how many are mine (my permanents / spells / cards, or me) and the
/// opponent's. Modal spells (targets depend on the modes) are not covered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptTargets {
    pub option: u16,
    pub mine: u8,
    pub opp: u8,
    /// Of `opp`: how many have ward (CR 702.21a: targeting them triggers it). v8.
    #[serde(default)]
    pub opp_ward: u8,
}

/// What a `Subtype` / `Name` option stands for, with how many of the observer's and the
/// opponent's visible cards (battlefield by controller, hand, graveyard) have that type / name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChoiceView {
    /// Index of the option in the decision.
    pub option: u16,
    /// `Name`: the named card in this game's CardDb (None if the DB lacks it).
    pub card: Option<CardId>,
    /// `Subtype`: the subtype (CardDb id; stable below `carddb::cr_subtype_count()`).
    pub subtype: Option<Subtype>,
    pub mine: u8,
    pub opp: u8,
}

fn view(g: &Game, id: ObjId, visible: bool) -> ObjView {
    let s = g.view_state();
    let o = s.obj(id);
    let on_bf = o.zone == Zone::Battlefield;
    let c = if visible { Some(chars(s, &g.db, id)) } else { None };
    let creature = c.as_ref().is_some_and(|c| c.types.has(CardType::Creature));
    ObjView {
        id,
        card: visible.then_some(o.card),
        owner: o.owner,
        controller: o.controller,
        tapped: o.tapped,
        damage: o.damage,
        power: if creature { c.as_ref().map(|c| c.power) } else { None },
        toughness: if creature { c.as_ref().map(|c| c.toughness) } else { None },
        counters: o.counters.to_vec(),
        token: o.token,
        sick: on_bf && s.is_sick(id),
        attacking: s.combat.attackers.iter().any(|a| a.0 == id),
        blocking: blocking_of(s, id),
        blocked: s.combat.attackers.iter().any(|a| a.0 == id) && s.combat.blocked.contains(&id),
        chosen_type: if visible { s.ext.chosen_type_of(id) } else { None },
        named: if visible { s.ext.named_of(id).map(|i| crate::name_pool::name(i).to_string()) } else { None },
        exiled_with: if visible { s.ext.exiled_with.source_of(&s.objs, id) } else { None },
        solved: visible && s.ext.solved.contains(&id),
        alt: if visible { s.cast_of(id).flags } else { 0 },
        attacking_pw: s.combat.attacking_pw.iter().find(|a| a.0 == id).map(|a| a.1),
        prepared: o.prepared,
        face: if visible { o.face } else { 0 },
        attached_to: if on_bf { o.attached_to } else { None },
        // An offspring-style token copy (its own card is the copied card, the only exception its
        // P/T: `zones::CopyExc`) shows no other card, as before that exception
        // became a copy effect (observation and encoding unchanged).
        copy_of_printed: if on_bf { s.copy_effects.iter().find(|c| c.obj == id).filter(|c| !(o.token && c.printed == o.card && c.set_pt.is_some() && !c.transformed && c.name.is_none() && c.add_super == 0 && c.add_types == TypeSet::default() && c.add_subtypes.is_empty() && c.kws == crate::carddb::KwSet::default())).map(|c| c.printed) } else { None },
        harnessed: on_bf && s.ext.harnessed.contains(&id),
        types: c.as_ref().map_or(0, |c| c.types.0),
        kws: c.as_ref().map_or(0, |c| c.kws.0),
        mv: c.as_ref().map(|c| c.mv),
        declaring: on_bf && s.combat.declaring.contains(&id),
        entered: o.entered,
        face_down: false,
        class_level: if on_bf && !s.ext.x6.levels.is_empty() { s.ext.x6.levels.iter().find(|x| x.0 == id).map(|x| x.1) } else { None },
        ward: None,
        hexproof_from: 0,
        rmana: 0,
        name_card: if visible { crate::query::name_override(s, id) } else { None },
        colors: c.as_ref().map_or(0, |c| c.colors.0),
        pips: if visible { cost_pips(&g.db.get(o.card).face(o.face).cost) } else { [0; 6] },
        makes: c.as_ref().map_or(0, |c| mana_bits(&g.db, &c.abilities)),
        counter_ids: o.counters.iter().map(|&(k, _)| crate::encode::counter_id(k, &g.db.counter_names)).collect(),
        until_eot: None,
        cant_attack: false,
        cant_block: false,
        named_card: if visible { s.ext.named_of(id).and_then(|i| g.db.by_name(crate::name_pool::name(i))) } else { None },
        trigger_used: on_bf && trigger_limit_reached(s, &g.db, id),
        valiant_used: on_bf && crate::ext4::was_targeted_by(s, id, o.controller),
        exhausted: on_bf && s.ext.exhausted.iter().any(|x| x.0 == id),
        dies_exile: on_bf && s.ext.lt.dies_exile.contains(&id),
    }
}

/// The attacker `id` blocks (`ObjView::blocking`). A creature blocking several attackers (CR
/// 509.1a, "can block additional creatures") is not supported by the engine (`combat::damage_step`,
/// the scenario loader refuse it) and has no view: fail loudly rather than show one of them.
fn blocking_of(s: &State, id: ObjId) -> Option<ObjId> {
    let mut it = s.combat.blocks.iter().filter(|b| b.0 == id);
    let a = it.next().map(|b| b.1);
    assert!(it.next().is_none(), "observe: creature {id:?} blocks several attackers (CR 509.1a): not supported");
    a
}

/// `ObjView::trigger_used`: a per-turn-limited triggered ability of `id` (its card's, as
/// `triggers::fire` counts them) triggered as often as its limit allows this turn.
fn trigger_limit_reached(s: &State, db: &crate::CardDb, id: ObjId) -> bool {
    s.turn_stats.trigger_counts.iter().any(|&(src, i, n)| {
        src == id && db.get(s.obj(id).card).front().triggers.get(i as usize).and_then(|t| t.per_turn).is_some_and(|lim| n >= lim)
    })
}

/// `ObjView::until_eot` of the permanents `bf` (see `EotView`). Nothing to do (and no copy of the
/// state) unless an "until end of turn" effect exists.
fn until_eot(g: &Game, bf: &mut [ObjView]) {
    let s = g.view_state();
    let (turn, active) = (s.turn.number, s.turn.active);
    let ends = |e: &EffectObj| crate::flow::ends_at_cleanup(e, turn, active);
    if !s.effects.iter().any(|e| e.until_eot) && !s.effect_objs.iter().any(|e| !e.statics.is_empty() && ends(e)) {
        return;
    }
    let mut after = s.clone();
    after.effects.retain(|e| !e.until_eot);
    after.effect_objs.retain(|e| !ends(e));
    let db = &*g.db;
    crate::query::frozen_aside(&after, db, || {
        for v in bf.iter_mut() {
            if after.obj(v.id).zone != Zone::Battlefield {
                continue;
            }
            let c = chars(&after, db, v.id);
            let (p, t) = if c.types.has(CardType::Creature) { (c.power, c.toughness) } else { (0, 0) };
            let ev = EotView {
                power: v.power.unwrap_or(0).saturating_sub(p),
                toughness: v.toughness.unwrap_or(0).saturating_sub(t),
                kws: v.kws & !c.kws.0,
                lost_kws: c.kws.0 & !v.kws,
                types: v.types & !c.types.0,
            };
            v.until_eot = (ev != EotView::default()).then_some(ev);
        }
    });
}

/// `PlayerView::{died_this_turn, left_this_turn, descended_this_turn}` of player `p`, from this
/// turn's zone-change history (`Ext::moved_this_turn`: (object before, from, to, object after); the
/// object before keeps its last known characteristics and controller, CR 113.7a). Descended: as
/// `ext9::X9Expr::DescendedThisTurn` counts it (700.11: permanent cards, so not tokens).
fn this_turn_moves(s: &State, db: &crate::CardDb, p: PlayerId) -> (u8, u8, u8) {
    let (mut died, mut left, mut desc) = (0u32, 0u32, 0u32);
    for &(old, from, to, new) in &s.ext.moved_this_turn {
        if from == Zone::Battlefield && s.obj(old).controller == p {
            let t = chars(s, db, old).types;
            died += (to == Zone::Graveyard && t.has(CardType::Creature)) as u32;
            left += !t.has(CardType::Land) as u32;
        }
        if to == Zone::Graveyard {
            let o = s.obj(new);
            let t = db.get(o.card).front().types;
            desc += (!o.token && o.owner == p && [CardType::Artifact, CardType::Creature, CardType::Enchantment, CardType::Land, CardType::Planeswalker, CardType::Battle].iter().any(|&k| t.has(k))) as u32;
        }
    }
    (died.min(255) as u8, left.min(255) as u8, desc.min(255) as u8)
}

/// `PlayerView::owner_knows_top` / `owner_knows_bottom` of player `p`'s library.
fn owner_knows(s: &State, p: PlayerId) -> (u8, u8) {
    let lib = &s.player(p).library; // bottom first
    let knows = |o: &ObjId| s.obj(*o).known_to & (1 << p.0) != 0;
    let top = lib.iter().rev().take_while(|o| knows(o)).count();
    let bottom = lib[..lib.len() - top].iter().take_while(|o| knows(o)).count();
    (top.min(255) as u8, bottom.min(255) as u8)
}

type WardSrc<'d> = (ObjId, PlayerId, &'d crate::carddb::TriggerDef, u32);

/// Ward triggered abilities on the battlefield (CR 702.21a): (source, its controller, trigger,
/// timestamp of the effect granting it): printed or granted ward (an Equipment / Aura / static
/// grants it as a trigger of its source matching the affected permanents, see `compile`),
/// granted triggers and effect objects'. A ward granted to a permanent that later lost all
/// abilities doesn't apply to it (`ward_of`, CR 613.1f, 613.7).
fn ward_sources<'d>(s: &State, db: &'d crate::CardDb) -> Vec<WardSrc<'d>> {
    let ward = |td: &crate::carddb::TriggerDef| td.text.starts_with("Ward") && matches!(td.mode, crate::carddb::TrigMode::BecomesTarget { .. });
    let lost = |o: ObjId| crate::query::may_lose_abilities(s, db) && crate::query::lost_all_abilities(s, db, o);
    let mut out = vec![];
    for &o in &s.battlefield {
        let ob = s.obj(o);
        let face = db.get(ob.card).front();
        if face.triggers.iter().any(ward) && !lost(o) {
            out.extend(face.triggers.iter().filter(|td| ward(td) && td.zones.has(Zone::Battlefield)).map(|td| (o, ob.controller, td, ob.ts)));
        }
    }
    for &(o, ti, ts) in &s.ext.x6.granted {
        let td = &db.triggers_pool[ti as usize];
        if ward(td) && s.obj(o).zone == Zone::Battlefield {
            out.push((o, s.obj(o).controller, td, ts));
        }
    }
    for eo in &s.effect_objs {
        for &ti in &eo.triggers {
            let td = &db.triggers_pool[ti as usize];
            if ward(td) {
                out.push((eo.source, eo.controller, td, eo.ts));
            }
        }
    }
    out
}

/// Ward of permanent `id`: the ward abilities that would trigger if an opponent of its
/// controller targeted it now (CR 702.21a), with their costs.
fn ward_of(s: &State, db: &crate::CardDb, srcs: &[WardSrc<'_>], id: ObjId) -> Option<WardView> {
    use crate::carddb::UnlessCost;
    if srcs.is_empty() {
        return None;
    }
    let opp = s.obj(id).controller.opp();
    let ev = crate::triggers::TrigEvent::BecomesTarget { target: id, by_source: id, item: 0, controller: opp };
    let mut w: Option<WardView> = None;
    for &(src, you, td, ts) in srcs {
        let Some(trig) = crate::triggers::matches(s, db, &td.mode, &ev, src, you) else { continue };
        if crate::query::grant_removed(s, db, id, ts) {
            continue;
        }
        let ecx = crate::expr::EvalCtx { s, db, you, source: src, x: 0, kicked: false, triggered: trig.obj, trigger_amount: 0, targets: &[], rem: &[], imprinted: &[], base_only: false };
        if !td.intervening.iter().all(|c| crate::expr::holds(c, &ecx)) {
            continue;
        }
        let v = w.get_or_insert_with(WardView::default);
        match db.ability(td.execute).unless.as_ref().map(|u| &u.0) {
            Some(UnlessCost::Mana(mc)) => v.mana = v.mana.saturating_add(mc.mana_value().min(255) as u8),
            Some(UnlessCost::Life(n)) => v.life = v.life.saturating_add((*n).clamp(0, 255) as u8),
            Some(UnlessCost::LifeExpr(e)) => v.life = v.life.saturating_add(crate::expr::eval(&e, &ecx).clamp(0, 255) as u8),
            _ => v.other = true,
        }
    }
    if w.is_some() && crate::ext3::ward_disabled(s, db, id) {
        return None;
    }
    w
}

/// Restricted pool mana of `p` (CR 106.6) spendable on `o` cast as a spell (`ext6::pool_avail`).
fn rmana_for(s: &State, db: &crate::CardDb, p: PlayerId, o: ObjId) -> u8 {
    let mut n = 0u8;
    for &(pl, _, k, r) in &s.ext.x6.rpool {
        if pl == p && r.allows(s, db, p, Some(o)) {
            n = n.saturating_add(k);
        }
    }
    n
}

/// A permanent as `me` sees it: face-down ones (CR 708) show their face-down characteristics
/// (public) and their real card only to a player who may look at it.
fn view_permanent(g: &Game, id: ObjId, me: PlayerId) -> ObjView {
    let mut v = view(g, id, true);
    let s = g.view_state();
    if let Some(real) = crate::ext6::face_down_card(s, id) {
        v.face_down = true;
        v.card = (s.obj(id).known_to & (1 << me.0) != 0).then_some(real);
        v.name_card = None; // CR 708.2a: no name
    }
    v
}

fn option_targets(g: &Game, me: PlayerId, d: Option<&Decision>, wards: &[WardSrc<'_>]) -> Vec<OptTargets> {
    use crate::engine::Opt;
    let Some(d) = d else { return vec![] };
    let s = g.view_state();
    let db = &*g.db;
    let mut out = vec![];
    for (i, o) in d.options.iter().enumerate().take(crate::encode::O) {
        // Every way of casting a spell maps to the face its spell comes from (encoding v10.1:
        // before it, alternative-method casts and prepared copies had none).
        let face = |x: ObjId, f: u8| (x, db.get(s.obj(x).card).face(f).spell);
        let (x, a) = match *o {
            Opt::Cast(x) | Opt::Flashback(x) => face(x, 0),
            Opt::CastFace(x, f) => face(x, f),
            // CR 722.3c: a copy of the prepare spell, face 1 (`cast::cast_prepared`); its source
            // here is the prepared permanent (the copy exists only once it is cast).
            Opt::CastPrepared(x) => face(x, 1),
            // Airbend, its other face, mayhem, harmonize, plotted, the granted second
            // flashback, warp, evoke, ... (`Method::cast_face`); plot / unlock / turn face up
            // cast nothing.
            Opt::Alt(x, m) => match crate::altcast::Method::from_u8(m).and_then(crate::altcast::Method::cast_face) {
                Some(f) => face(x, f),
                None => continue,
            },
            Opt::Activate(x, a) => (x, Some(a)),
            _ => continue,
        };
        let Some(a) = a else { continue };
        let Some(t) = crate::resolve::nodes(db, a, &[]).iter().find_map(|&n| db.ability(n).target.clone()) else { continue };
        let (mut mine, mut opp, mut opp_ward) = (0u8, 0u8, 0u8);
        for tg in crate::cast::legal_targets_in(s, db, me, &t, Some(x), &[]) {
            let ours = match tg {
                Target::Player(p) => p == me,
                Target::Obj(ob) => s.obj(ob).controller == me,
                // As before stack-object targets (`ext8`): by the ability's source.
                Target::Ability { src, .. } => s.obj(src).controller == me,
            };
            if ours {
                mine = mine.saturating_add(1);
            } else {
                opp = opp.saturating_add(1);
                if let Target::Obj(ob) = tg {
                    if s.obj(ob).zone == Zone::Battlefield && ward_of(s, db, wards, ob).is_some() {
                        opp_ward = opp_ward.saturating_add(1);
                    }
                }
            }
        }
        out.push(OptTargets { option: i as u16, mine, opp, opp_ward });
    }
    out
}

/// `ChoiceView`s of the observer's pending `Subtype` / `Name` options (public: the options are
/// the observer's own; counts are over cards the observer sees).
fn choices(g: &Game, me: PlayerId, d: Option<&Decision>) -> Vec<ChoiceView> {
    use crate::engine::Opt;
    let Some(d) = d else { return vec![] };
    if !d.options.iter().any(|o| matches!(o, Opt::Subtype(_) | Opt::Name(_))) {
        return vec![];
    }
    let s = g.view_state();
    let knows = |id: ObjId| s.obj(id).known_to & (1 << me.0) != 0;
    // Visible objects with their side (true = mine).
    let mut vis: Vec<(ObjId, bool)> = s.battlefield.iter().map(|&o| (o, s.obj(o).controller == me)).collect();
    for p in [me, me.opp()] {
        let pl = s.player(p);
        vis.extend(pl.hand.iter().filter(|&&h| p == me || knows(h)).map(|&h| (h, p == me)));
        vis.extend(pl.graveyard.iter().map(|&o| (o, p == me)));
    }
    let count = |f: &dyn Fn(ObjId) -> bool| -> (u8, u8) {
        let (mut a, mut b) = (0u8, 0u8);
        for &(o, mine) in &vis {
            if f(o) {
                if mine {
                    a = a.saturating_add(1);
                } else {
                    b = b.saturating_add(1);
                }
            }
        }
        (a, b)
    };
    let mut out = vec![];
    for (i, o) in d.options.iter().enumerate().take(crate::encode::O) {
        match *o {
            Opt::Subtype(st) => {
                let (mine, opp) = count(&|id| {
                    let c = chars(s, &g.db, id);
                    c.subtypes.contains(&st) || (c.all_creature_types && g.db.creature_types.contains(&st))
                });
                out.push(ChoiceView { option: i as u16, card: None, subtype: Some(st), mine, opp });
            }
            Opt::Name(n) => {
                let card = g.db.by_name(crate::name_pool::name(n));
                // Objects with that name (CR 201.2; a copy's name exception, 707.9b).
                let (mine, opp) = match card {
                    Some(_) => count(&|id| crate::query::has_name(s, &g.db, id, crate::name_pool::name(n))),
                    None => (0, 0),
                };
                out.push(ChoiceView { option: i as u16, card, subtype: None, mine, opp });
            }
            _ => {}
        }
    }
    out
}

pub fn observe(g: &Game, me: PlayerId) -> Observation {
    // Characteristics of the observed state are memoized (and usually already are from the
    // engine units that produced it, see `query::frozen`).
    crate::query::frozen(g.view_state(), &g.db, || observe_frozen(g, me))
}

fn observe_frozen(g: &Game, me: PlayerId) -> Observation {
    let s = g.view_state();
    let knows = |id: ObjId| s.obj(id).known_to & (1 << me.0) != 0;
    let players = [PlayerId(0), PlayerId(1)]
        .into_iter()
        .map(|p| {
            let pl = s.player(p);
            let n = pl.library.len();
            let moves = this_turn_moves(s, &g.db, p);
            PlayerView {
                id: p,
                life: pl.life,
                poison: pl.poison,
                hand: pl.hand.iter().map(|&h| view(g, h, p == me || knows(h))).collect(),
                library_size: n,
                library_known: pl.library.iter().enumerate().filter(|(_, &o)| knows(o)).map(|(i, &o)| (n - 1 - i, s.obj(o).card)).collect(),
                library_known_ids: pl.library.iter().copied().filter(|&o| knows(o)).collect(),
                graveyard: pl.graveyard.iter().map(|&o| view(g, o, true)).collect(),
                lands_played: pl.lands_played,
                mulligans: pl.mulligans,
                // Restricted mana (CR 106.6) included; its part in `pool_restricted`.
                pool: crate::ext6::pool_total(s, p),
                pool_restricted: {
                    let mut r = [0u8; 6];
                    for &(pl, t, n, _) in &s.ext.x6.rpool {
                        if pl == p {
                            r[t as usize] = r[t as usize].saturating_add(n);
                        }
                    }
                    r
                },
                speed: s.ext.speed[p.idx()],
                enduring_story: s.ext.enduring_story & (1 << p.0) != 0,
                pool_kept: crate::ext9::pool_kept(s, p),
                skip_turns: s.ext.x6.skip_turns[p.idx()],
                owner_knows_top: owner_knows(s, p).0,
                owner_knows_bottom: owner_knows(s, p).1,
                life_lost_this_turn: s.turn_stats.life_lost[p.idx()],
                life_gained_this_turn: s.turn_stats.life_gained[p.idx()],
                spells_cast_this_turn: s.turn_stats.spells_cast[p.idx()],
                died_this_turn: moves.0,
                left_this_turn: moves.1,
                sacrificed_this_turn: s.ext.sacrificed_this_turn.iter().filter(|x| x.0 == p).count().min(255) as u8,
                descended_this_turn: moves.2,
                warped_this_turn: s.ext.mr.warped_by[p.idx()],
                next_uncounterable: s.ext.lt.next_uncounterable & (1 << p.0) != 0,
                hexproof: crate::static_ext::player_has_hexproof(s, &g.db, p),
                sideboard: pl.sideboard.iter().copied().filter(|&o| knows(o)).map(|o| view(g, o, true)).collect(),
            }
        })
        .collect();
    let wards = ward_sources(s, &g.db);
    let mine_decision = g.pending().filter(|d| d.player == me);
    let mut obs = Observation {
        me,
        turn: s.turn.number,
        active: s.turn.active,
        step: s.turn.step,
        stage: s.stage,
        players,
        // The attackers picked so far are the declaring (active) player's private choice.
        battlefield: s.battlefield.iter().map(|&o| view_permanent(g, o, me)).map(|mut v| { v.declaring &= me == s.turn.active; v }).collect(),
        // Face-down exiled cards (CR 406.3) are hidden unless the observer may look at them.
        exile: s.exile.iter().map(|&o| view(g, o, knows(o))).collect(),
        // The trigger being put on the stack (CR 603.3, `State::being_put`) is the topmost item.
        stack: s
            .stack_items()
            .chain(s.being_put.iter())
            .map(|it| {
                let spell = it.kind == StackKind::Spell;
                StackView {
                    card: it.source_card,
                    controller: it.controller,
                    kind: it.kind,
                    source: it.source,
                    targets: it.targets.iter().map(|t| t.1).collect(),
                    x: it.x,
                    face: if spell { s.obj(it.source).face } else { 0 },
                    uncounterable: spell && (s.ext.uncounterable.contains(&it.source) || crate::static_ext::spell_cant_be_countered(s, &g.db, it.source)),
                    colors: if spell { chars(s, &g.db, it.source).colors.0 } else { 0 },
                    pips: if spell { cost_pips(&g.db.get(s.obj(it.source).card).face(s.obj(it.source).face).cost) } else { [0; 6] },
                    uid: it.uid,
                    resolving: s.resolving.as_ref().is_some_and(|r| std::ptr::eq(r, it)),
                    modes: it.modes.to_vec(),
                    being_put: s.being_put.as_ref().is_some_and(|b| std::ptr::eq(b, it)),
                    owner: spell.then(|| s.obj(it.source).owner),
                    ability: stack_ability_index(&g.db, it),
                    paid_optional: spell && {
                        use crate::altcast::flag;
                        it.kicked || s.cast_of(it.source).flags & (flag::OPTCOST | flag::OFFSPRING | flag::TEAMWORK) != 0
                    },
                    spent_colors: if spell { s.ext.spent_colors.iter().find(|x| x.0 == it.source).map_or(0, |x| x.1 & 0b11111) } else { 0 },
                }
            })
            .collect(),
        // CR 601.2: only spells being cast; a resolving spell is a stack item (608.2n).
        casting: s
            .objs
            .iter()
            .enumerate()
            .filter(|(i, o)| o.zone == Zone::Stack && !s.stack_items().any(|it| it.source.idx() == *i))
            .map(|(i, _)| view(g, ObjId(i as u32), true))
            .collect(),
        entering: entering(s).map(|o| EnteringView { view: view_permanent(g, o, me), spell: s.resolving.as_ref().filter(|_| s.becoming == Some(o)).map(|r| r.source) }).collect(),
        last_known: vec![],
        decision: g.pending().filter(|d| d.player == me).cloned(),
        result: s.result,
        hist: s.hist.clone(),
        choice_info: choices(g, me, g.pending().filter(|d| d.player == me)),
        option_targets: option_targets(g, me, mine_decision, &wards),
        ordering: match g.pending() {
            Some(d) if d.player == me && d.kind == DecisionKind::OrderTriggers => s
                .ext
                .ordering
                .iter()
                .map(|t| TriggerView { source: t.source, card: t.card, trigger: t.adhoc.is_none().then_some(t.trigger_idx) })
                .collect(),
            _ => vec![],
        },
        effects: effects(g, me),
        delayed: delayed(g, me),
        pick: if mine_decision.is_some() { s.ext.x6.pick.clone() } else { Default::default() },
        own_deck: own_deck(g, me),
        pay_unless: mine_decision.filter(|d| d.kind == DecisionKind::PayUnless).and(s.ext.unless_due),
    };
    // v10.1: what ends with this turn's "until end of turn" effects (public, both views alike).
    until_eot(g, &mut obs.battlefield);
    // v8 per-object additions: ward / hexproof from (battlefield), restricted mana (my cards).
    let db = &*g.db;
    for v in obs.battlefield.iter_mut() {
        if v.power.is_some() {
            v.cant_attack = crate::combat::cant_attack_by_effect(s, db, v.id);
            v.cant_block = crate::combat::cant_block_at_all(s, db, v.id);
        }
        v.ward = ward_of(s, db, &wards, v.id);
        if crate::cast::has_hexproof_from_mono(s, db, v.id) {
            v.hexproof_from |= HEXPROOF_FROM_MONO;
        }
    }
    obs.last_known = last_known(g, &obs);
    if s.ext.x6.rpool.iter().any(|x| x.0 == me) {
        let set = |v: &mut ObjView| {
            if v.card.is_some() && v.owner == me {
                v.rmana = rmana_for(s, db, me, v.id);
            }
        };
        if let Some(pl) = obs.players.iter_mut().find(|p| p.id == me) {
            pl.hand.iter_mut().for_each(set);
            pl.graveyard.iter_mut().for_each(set);
        }
        obs.exile.iter_mut().for_each(set);
        obs.casting.iter_mut().for_each(set);
    }
    obs
}

/// Objects entering the battlefield now (`Observation::entering`): created for the battlefield
/// (`State::move_obj_limbo`) but not placed yet, while their "as enters" choices are made
/// (`zones::move_batch` / `move_special`, CR 614.12a). In object order.
fn entering(s: &State) -> impl Iterator<Item = ObjId> + '_ {
    let n = s.objs.iter().filter(|o| o.zone == Zone::Battlefield).count();
    let any = n > s.battlefield.len();
    s.objs.iter().enumerate().filter(move |(i, o)| any && o.zone == Zone::Battlefield && !s.battlefield.contains(&ObjId(*i as u32))).map(|(i, _)| ObjId(i as u32))
}

/// Every object id `obs` shows (any zone, known library cards, stack sources, entering).
fn shown_ids(obs: &Observation) -> Vec<ObjId> {
    let mut v: Vec<ObjId> = vec![];
    v.extend(obs.battlefield.iter().chain(&obs.exile).chain(&obs.casting).chain(obs.entering.iter().map(|e| &e.view)).map(|x| x.id));
    for pl in &obs.players {
        v.extend(pl.hand.iter().chain(&pl.graveyard).chain(&pl.sideboard).map(|x| x.id));
        v.extend(pl.library_known_ids.iter().copied());
    }
    v.extend(obs.stack.iter().map(|x| x.source));
    v.extend(obs.ordering.iter().map(|x| x.source));
    v
}

/// `Observation::last_known`: what the observer's decision names that left the battlefield
/// and is in no zone now (an old object with a `lki` snapshot: it was public, CR 400.2).
fn last_known(g: &Game, obs: &Observation) -> Vec<ObjView> {
    let Some(d) = obs.decision.as_ref() else { return vec![] };
    let s = g.view_state();
    if s.ext.lki.ids.is_empty() {
        return vec![];
    }
    let mut named: Vec<ObjId> = d.subject.into_iter().collect();
    for o in d.options.iter() {
        named.extend(crate::encode::opt_objs(o).into_iter().flatten());
    }
    let shown = shown_ids(obs);
    let mut out: Vec<ObjView> = vec![];
    for o in named {
        if o.idx() < s.objs.len() && !shown.contains(&o) && !out.iter().any(|v| v.id == o) && crate::lki::of(s, o).is_some() {
            out.push(view(g, o, true));
        }
    }
    out
}

/// `EffectView`s of the effect objects (public: created by resolved spells and abilities).
fn effects(g: &Game, me: PlayerId) -> Vec<EffectView> {
    let s = g.view_state();
    if s.effect_objs.is_empty() {
        return vec![];
    }
    // Objects the observer can see (a face-down permanent's card only if they may look at it).
    let sees = |o: ObjId| {
        let ob = s.obj(o);
        let knows = ob.known_to & (1 << me.0) != 0;
        match ob.zone {
            Zone::Battlefield => knows || crate::ext6::face_down_card(s, o).is_none(),
            Zone::Graveyard | Zone::Stack => true,
            _ => knows,
        }
    };
    s.effect_objs
        .iter()
        .map(|e| EffectView {
            card: sees(e.source).then(|| s.obj(e.source).card),
            controller: e.controller,
            duration: e.duration,
            statics: e.statics.len().min(255) as u8,
            triggers: e.triggers.len().min(255) as u8,
            affected: e.remembered.iter().copied().filter(|&o| sees(o)).collect(),
        })
        .collect()
}

/// `DelayedView`s of the delayed triggers waiting (`ext.delayed`; public, CR 603.7a). Objects are
/// shown under `effects`' rule: a face-down card only to who may look at it, hidden zones only to who
/// knows the object.
///
/// Only triggers that can still trigger are listed. "When it dies / is exiled / leaves" (`Leaves`,
/// `DiesWithin`) watches one object on the battlefield; once that object has left the battlefield any
/// other way (Unsummon on an earthbent land) its trigger event can never occur: the card in its new
/// zone is a new object (CR 400.7), and an earlier event made the trigger event impossible (603.7a,
/// 603.2g). The engine keeps such an entry (`trig_ext::fire_extra`; inert, never fires); the view
/// drops it. Whether an object is on the battlefield is public. The "at the
/// beginning of the next step" kinds stay listed whatever became of their objects: they still
/// trigger and resolve, affecting only objects still where they're expected (603.7c).
fn delayed(g: &Game, me: PlayerId) -> Vec<DelayedView> {
    let s = g.view_state();
    if s.ext.delayed.is_empty() {
        return vec![];
    }
    let sees = |o: ObjId| {
        let ob = s.obj(o);
        let knows = ob.known_to & (1 << me.0) != 0;
        match ob.zone {
            Zone::Battlefield => knows || crate::ext6::face_down_card(s, o).is_none(),
            Zone::Graveyard | Zone::Stack => true,
            Zone::Hand => knows || ob.owner == me,
            _ => knows,
        }
    };
    let shown = |o: ObjId| sees(o).then_some(o);
    s.ext
        .delayed
        .iter()
        .filter(|d| match d.when {
            crate::ext::DelayedWhen::Leaves { obj, .. } | crate::ext::DelayedWhen::DiesWithin { obj, .. } => s.obj(obj).zone == Zone::Battlefield,
            crate::ext::DelayedWhen::NextStep { .. } | crate::ext::DelayedWhen::PlayersNextStep { .. } => true,
        })
        .map(|d| {
            use crate::ext::DelayedWhen as W;
            let (when, watched) = match d.when {
                W::Leaves { obj, to } => {
                    let (gy, ex) = (crate::carddb::ZoneSet::of(Zone::Graveyard).0, crate::carddb::ZoneSet::of(Zone::Exile).0);
                    let other = to.0 & !(gy | ex) != 0;
                    (DelayedWhenView::Leaves { obj: shown(obj), dies: to.has(Zone::Graveyard), exiled: to.has(Zone::Exile), other }, Some(obj))
                }
                W::DiesWithin { obj, turn: _ } => (DelayedWhenView::DiesThisTurnOnly { obj: shown(obj) }, Some(obj)),
                W::NextStep { step } => (DelayedWhenView::NextStep { step }, None),
                W::PlayersNextStep { step, player } => (DelayedWhenView::PlayersNextStep { step, player }, None),
            };
            let mut affected: Vec<ObjId> = vec![];
            for o in watched.into_iter().chain(d.remembered.iter().copied()) {
                if sees(o) && !affected.contains(&o) {
                    affected.push(o);
                }
            }
            // its source's card was public when the ability resolved; a face-down permanent's card stays
            // hidden from who may not look at it
            let card = (s.obj(d.source).zone != Zone::Battlefield || sees(d.source)).then_some(d.source_card);
            DelayedView { card, controller: d.controller, ability: d.execute, when, affected }
        })
        .collect()
}

/// One entry of a player's filtered event stream: either a god-view [`Event`] that is fully
/// public to the observer, or a redacted form. Serializes untagged, so a redacted draw looks
/// like `{"Draw":{"player":1}}` next to a public `{"Draw":{"player":0,"card":12}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum ObsEvent {
    Public(Event),
    Redacted(Redacted),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum Redacted {
    /// Another player drew a card the observer can't see.
    Draw { player: PlayerId },
    /// An event kind this filter doesn't classify yet: only its name is shown (fail closed, so
    /// a newly added event can't leak before someone decides it is public).
    Hidden { kind: String },
}

fn event_kind(e: &Event) -> String {
    match serde_json::to_value(e) {
        Ok(serde_json::Value::Object(m)) => m.keys().next().cloned().unwrap_or_default(),
        Ok(serde_json::Value::String(s)) => s,
        _ => "?".into(),
    }
}

/// Filter one god-view event for `me`.
#[allow(unreachable_patterns)]
pub fn filter_event(e: &Event, me: PlayerId) -> ObsEvent {
    use Event::*;
    match e {
        // Library and hand are hidden zones (CR 400.2); another player's draw shows no card.
        Draw { player, .. } if *player != me => ObsEvent::Redacted(Redacted::Draw { player: *player }),
        // Events in public zones, or announced (casting, activating, discarding reveals).
        Draw { .. }
        | TurnBegan { .. }
        | StepBegan { .. }
        | Mulligan { .. }
        | Kept { .. }
        | PlayLand { .. }
        | Cast { .. }
        | CastFace { .. }
        | SpellCopied { .. }
        | AbilityCopied { .. }
        | TurnedFaceUp { .. }
        | CoinFlips { .. }
        | Attached { .. }
        | BecameCopy { .. }
        | Transformed { .. }
        | Activate { .. }
        | TriggerPut { .. }
        | Resolve { .. }
        | Fizzle { .. }
        | Countered { .. }
        | Damage { .. }
        | LifeChange { .. }
        | Dies { .. }
        | Destroyed { .. }
        | Attack { .. }
        | AttackPlaneswalker { .. }
        | Block { .. }
        | Discard { .. }
        | GameOver { .. } => ObsEvent::Public(e.clone()),
        _ => ObsEvent::Redacted(Redacted::Hidden { kind: event_kind(e) }),
    }
}

/// The game log from index `since`, as `me` is entitled to see it. Front-ends must use this
/// (never `Game::log`) for anything an agent sees.
/// Cursor just past everything `events_for` returns now (committed log + the paused unit's
/// events). A paused unit re-runs deterministically from its choice tape, so its events so far
/// are a prefix of what it commits: a stream resumed from here sees each event once.
pub fn events_end(g: &Game) -> usize {
    g.log.len() + if g.pending().is_some() { g.view_state().events.len() } else { 0 }
}

pub fn events_for(g: &Game, me: PlayerId, since: usize) -> Vec<ObsEvent> {
    // Committed log, then events of the paused unit (if a decision is pending). Indices into
    // the pending part are provisional: they are re-emitted once the unit commits.
    let pending: &[Event] = if g.pending().is_some() { &g.view_state().events } else { &[] };
    g.log.iter().chain(pending.iter()).skip(since).map(|e| filter_event(e, me)).collect()
}
