//! Fixed-size tensor encoding of an [`Observation`] (and the decision it carries) for neural
//! policies: entity tokens + global features + candidate options that point at entity tokens.
//! Bump [`ENCODING_VERSION`] whenever the layout or feature meanings change.
//!
//! Honesty: the input is only an `Observation` (what `observe(player)` returns), so the encoding
//! cannot see hidden state. Raw object ids never reach the tensors; they are only used to link
//! options to entity rows. The decision encoded is `obs.decision`, i.e. only the observer's own.
//!
//! Forward compatibility: decision kinds and option variants grow as rules are added. Anything
//! this module doesn't know maps to an "other" bucket (never a panic); the one-hot blocks have
//! spare slots so new variants can be given their own slot without changing sizes.

use crate::basics::*;
use crate::engine::{DecisionKind, Opt};
use crate::observe::{ObjView, Observation};
use crate::state::*;

pub const ENCODING_VERSION: u32 = 10;
/// Encoding v10.1: same shapes and the same v9 / v10
/// columns as v10, plus v10's spare entity columns 150–151 (`ef::V101`) and option columns 84–95
/// (`of::V101`).
/// v10.2 (minor 2): same shapes again; new values only in columns
/// every earlier net has only seen as zero (the released Bo3 / belief reservations: entity 123–127,
/// globals 128–175; option 85–94), new bits of v10.1's embedded ids, and the delayed-trigger rows
/// (`ef::DELAY_ID`, rows >= E_V9 only, invisible to nets without v10.2).
pub const ENCODING_MINOR: u32 = 2;

/// Entity rows of the v9 layout (2 player tokens + visible objects + stack items + tokens).
pub const E_V9: usize = 128;
/// Max entity tokens (v10: 128 -> 192). Rows `E_V9..E` are **reserved rows**: used only when the
/// v9 layout (capped at `E_V9`) cut an object or token that one of my decision's options names,
/// which then gets a row there (128-entity budget). Rows `0..E_V9` are
/// exactly the v9 layout; a net without v10 inputs reads only them.
pub const E: usize = 192;
/// Features per entity token (v8: 96 -> 128; v9: 128 -> 152, the colour block `ef::COLOR..`;
/// v10 uses v9's spare columns 146–149, v10.1 the last two, 150–151).
pub const F: usize = 152;
/// Global features (v9: 128 -> 400; 128–175 reserved for other v9 blocks, the own-deck block
/// `gf::DECK` at 176–399; v10: 400 -> 416, `gf::V10`).
pub const G: usize = 416;
/// Max options per decision.
pub const O: usize = 64;
/// Features per option (v8: 80 -> 96; v10 uses v8's spare columns 81–83, v10.1 the rest, 84–95).
pub const K: usize = 96;
/// Entity references per option.
pub const R: usize = 3;

pub const OPT_SLOTS: usize = 32;
/// Subtype embedding ids (v7): 0 = none, `1 + subtype` for the CR subtypes (stable across
/// CardDbs, `carddb::cr_subtype_count`), `SUBTYPE_VOCAB - 1` for any other subtype.
pub const SUBTYPE_VOCAB: usize = 1024;
/// History tokens (v7): at most this many of the opponent's / my own recent actions, from the
/// current turn and the `HIST_MAX_AGE` turns before it.
pub const HIST_OPP: usize = 8;
pub const HIST_MINE: usize = 4;
pub const HIST_MAX_AGE: u32 = 3;
/// Effect tokens (v8): at most this many effect objects, newest first.
pub const EFFECT_TOKENS: usize = 8;
/// Delayed-trigger tokens (v10.2): at most this many delayed triggered abilities waiting
/// (`Observation::delayed`, oldest first), placed after every other row.
pub const DELAYED_TOKENS: usize = 8;
pub const KIND_SLOTS: usize = 32;
pub const STEP_SLOTS: usize = 16;

// Entity feature columns.
pub mod ef {
    pub const KIND_PLAYER: usize = 0;
    pub const KIND_OBJECT: usize = 1;
    pub const KIND_STACK: usize = 2;
    /// One-hot: hand, battlefield, graveyard, exile, stack, library.
    pub const ZONE: usize = 3;
    pub const MINE: usize = 9;
    pub const OWNED: usize = 10;
    pub const HIDDEN: usize = 11;
    pub const TAPPED: usize = 12;
    pub const SICK: usize = 13;
    pub const ATTACKING: usize = 14;
    pub const BLOCKING: usize = 15;
    pub const BLOCKED: usize = 16;
    pub const TOKEN: usize = 17;
    pub const HAS_PT: usize = 18;
    pub const POWER: usize = 19;
    pub const TOUGHNESS: usize = 20;
    pub const DAMAGE: usize = 21;
    pub const C_P1P1: usize = 22;
    pub const C_M1M1: usize = 23;
    pub const C_LOYALTY: usize = 24;
    pub const C_OTHER: usize = 25;
    pub const SUBJECT: usize = 26;
    pub const TARGETED_BY_ME: usize = 27;
    pub const TARGETED_BY_OPP: usize = 28;
    /// One-hot: spell, activated, triggered.
    pub const STACK_KIND: usize = 29;
    pub const STACK_DEPTH: usize = 32;
    pub const STACK_X: usize = 33;
    pub const STACK_NTARGETS: usize = 34;
    pub const LIB_POS: usize = 35;
    pub const P_LIFE: usize = 36;
    pub const P_POISON: usize = 37;
    pub const P_HAND: usize = 38;
    pub const P_LIBRARY: usize = 39;
    pub const P_GRAVEYARD: usize = 40;
    pub const P_LANDS_PLAYED: usize = 41;
    pub const P_MULLIGANS: usize = 42;
    pub const P_ACTIVE: usize = 43;
    pub const ZONE_INDEX: usize = 44;
    /// Object named by an option (or the decision subject) that is not in the observation,
    /// e.g. a card drawn earlier in the same resolution.
    pub const UNOBSERVED: usize = 45;
    /// Has the "prepared" designation (CR 722.3a).
    pub const PREPARED: usize = 46;
    /// Equipment/Aura attached to another permanent.
    pub const ATTACHED: usize = 47;
    /// Number of Equipment/Auras attached to it /4.
    pub const N_ATTACHMENTS: usize = 48;
    /// Attacking a planeswalker (rather than a player).
    pub const ATTACKING_PW: usize = 49;
    /// Planeswalker: number of creatures attacking it /4.
    pub const ATTACKED_BY: usize = 50;
    /// Uses an alternative face (Adventure/Omen/prepare spell on the stack or being cast).
    pub const ALT_FACE: usize = 51;
    /// A copy of another permanent, or a transformed double-faced permanent (its `card` is the
    /// copied card / back face).
    pub const COPY_OR_BACK: usize = 52;
    /// Leaves at the beginning of the next end step (warp, mobilize tokens).
    pub const ALT_TEMPORARY: usize = 53;
    /// In exile and castable by its owner (warped, plotted).
    pub const ALT_CASTABLE_EXILE: usize = 54;
    /// Harnessed (CR 701.64b).
    pub const HARNESSED: usize = 55;
    /// Player tokens only: mana pool W U B R G C /5 (CR 106.4), in the stack-only columns 29–34.
    pub const P_POOL: usize = 29;
    /// v6: current card types of a visible object, `CardType` order (artifact, battle,
    /// creature, enchantment, instant, kindred, land, planeswalker, sorcery): 56–64.
    pub const TYPES: usize = 56;
    pub const N_TYPES: usize = 9;
    /// v6: current keywords, `carddb::Kw` order (flying, reach, haste, vigilance, trample,
    /// lifelink, deathtouch, first strike, double strike, menace, defender, flash, hexproof,
    /// indestructible, prowess): 65–79.
    pub const KWS: usize = 65;
    pub const N_KWS: usize = 15;
    /// v6: mana value /10.
    pub const MV: usize = 80;
    /// v6: picked as an attacker in my declaration in progress (CR 508.1a).
    pub const DECLARING: usize = 81;
    /// v7: choice token: the identity of a `Subtype`/`Name` option (option refs point here;
    /// `entity_card` = the named card, subtype id in SUBTYPE_HI/LO).
    pub const KIND_CHOICE: usize = 82;
    /// v7: history token: one recent public action (`hist::Act`); `entity_card` = its card,
    /// MINE = mine.
    pub const KIND_HIST: usize = 83;
    /// v7: history action one-hot: cast, activate, discard (84–86).
    pub const H_ACT: usize = 84;
    /// v7: turns since the action /4.
    pub const H_TURNS_AGO: usize = 87;
    /// v7: taken during the other player's turn (instant speed).
    pub const H_OFF_TURN: usize = 88;
    /// v7: recency rank among that player's history tokens /8 (0 = newest).
    pub const H_RECENCY: usize = 89;
    /// v7: hand cards (hidden ones too): turns since it entered the hand /5 (public).
    pub const HAND_AGE: usize = 90;
    /// v7: choice tokens: my / the opponent's visible cards with that type or name /4.
    pub const CHOICE_N_MINE: usize = 91;
    pub const CHOICE_N_OPP: usize = 92;
    /// v7: subtype embedding id (`SUBTYPE_VOCAB`) as two exact base-32 digits (values 0–31,
    /// exact in fp16): choice tokens of `Subtype` options and
    /// permanents with a chosen type (Cavern of Souls). A net embeds them; not linear inputs.
    pub const SUBTYPE_HI: usize = 93;
    pub const SUBTYPE_LO: usize = 94;
    /// v8: a spell on the stack that can't be countered (CR 101.2; Cavern of Souls mana,
    /// "can't be countered" statics).
    pub const UNCOUNTERABLE: usize = 95;
    /// v8: a Class's level /3 (CR 716; 0 for anything else, 1/3 for an unleveled Class is
    /// left to the card embedding: level 1 is the absence of the designation, 716.2d).
    pub const CLASS_LEVEL: usize = 96;
    /// v8: ward (CR 702.21a; printed or granted): has ward, mana value of its mana costs /4,
    /// life to pay /5, another kind of cost.
    pub const WARD: usize = 97;
    pub const WARD_MANA: usize = 98;
    pub const WARD_LIFE: usize = 99;
    pub const WARD_OTHER: usize = 100;
    /// v8: hexproof from monocolored (CR 702.11d).
    pub const HEXPROOF_FROM_MONO: usize = 101;
    /// v8: effect token: an emblem or an effect of a resolved spell / ability
    /// (`observe::EffectView`); `entity_card` = the card that created it, MINE = mine.
    pub const KIND_EFFECT: usize = 102;
    /// v8: effect duration one-hot: end of turn, permanent (emblems), until (the end of) my
    /// next turn / end step, while its source stays (on the battlefield / tapped): 103–106.
    pub const EFF_DURATION: usize = 103;
    /// v8: its number of static abilities /2, triggered abilities /2, objects it applies to /4.
    pub const EFF_STATICS: usize = 107;
    pub const EFF_TRIGGERS: usize = 108;
    pub const EFF_AFFECTED: usize = 109;
    /// v8: objects: number of my / the opponent's effects that apply to it /2.
    pub const AFFECTED_BY_MINE: usize = 110;
    pub const AFFECTED_BY_OPP: usize = 111;
    /// v8: pending-trigger token (my `OrderTriggers` decision, CR 603.3b): `entity_card` = the
    /// source's card; `Trigger(i)` options point here (and at the source).
    pub const KIND_TRIGGER: usize = 112;
    /// v8: which of the card's triggered abilities, (index + 1) /4 (0: a delayed, reflexive or
    /// granted trigger).
    pub const TRIG_ABILITY: usize = 113;
    /// v8: already picked in my set choice in progress (`Pick::chosen`: targets, cards).
    pub const PICKED: usize = 114;
    /// v8: chosen as a target of an earlier targeted part of the spell / ability whose targets I
    /// am choosing (CR 601.2c; `Pick::earlier`), count.
    pub const TGT_EARLIER: usize = 115;
    /// v8: my cards: restricted pool mana of mine (CR 106.6) spendable on it as a spell /5.
    pub const RMANA: usize = 116;
    /// v8: player tokens: the restricted part of the mana pool, W U B R G C /5 (117–122).
    pub const P_RPOOL: usize = 117;
    // 123–127: the best-of-three reservation, released for encoding v10.2:
    // every v9 / v10 / v10.1 net has only ever seen zero there.
    /// v10.2 links block (opt-in):
    /// first column. Two row pointers and a card id, object rows only, **never** linear
    /// inputs (exact integers read from the raw tensor; v8–v10.1 nets saw only zeros here).
    pub const V102_LINKS: usize = 123;
    /// v10.2, battlefield creatures: the combat pairing (CR 509.1a, 508.1b), as the entity row + 1
    /// of the attacker it blocks (`ObjView::blocking`) or of the planeswalker it attacks
    /// (`ObjView::attacking_pw`); 0 = none, or that object has no row. Written after every row
    /// (v9 layout and reserved) is placed, from the final id -> row map. A row pointer like
    /// `HOST_ROW` (embedded through `pair_proj`, never linear).
    pub const PAIR_ROW: usize = 123;
    /// v10.2, exiled cards the observer sees: the entity row + 1 of the object it was exiled with
    /// (`ObjView::exiled_with`: "exiled with" CR 607.2a, "until it leaves" 610.3a); 0 = none, or
    /// that object has no row. A row pointer (through `exile_proj`, never linear).
    pub const EXILED_WITH_ROW: usize = 124;
    /// v10.2, visible objects: a second card id, `card id + 1` (0 = none; `CARD2_MAX` bounds it):
    /// the card named for it (`ObjView::named_card`: Petrified Hamlet, Ancient Vendetta) or, if
    /// nothing was named, a copy's own printed card (`ObjView::copy_of_printed`, CR 707.2;
    /// Superior Spider-Man, Mockingbird; a transformed permanent's front, as COPY_OR_BACK).
    /// Embedded through the card embedding + `card2_proj`, never linear.
    pub const CARD2: usize = 125;
    pub const V102_LINKS_END: usize = 126;
    /// v10.2, **stack rows only** (spells): colours of mana spent to cast it (`StackView::spent_colors`,
    /// `ColorSet` bits W U B R G) as one id 0..32 (`SPENT_VOCAB`), embedded by a net (never linear).
    pub const SPENT_COLORS: usize = 126;
    /// v10.2, **stack rows** (v10.1 leaves 151 zero there; object rows: `STATUS_FLAGS`, player rows:
    /// `P_LIFE_GAINED`): `stackf::*` bits (chosen modes, optional cost paid) as one id < `stackf::VOCAB`,
    /// embedded by a net (never linear).
    pub const STACK_FLAGS: usize = 151;
    /// v9 colour block (opt-in), visible objects and spells on the
    /// stack: current colours W U B R G, colourless (CR 105.2; 128–133).
    pub const COLOR: usize = 128;
    /// v9: mana cost symbols of the face shown (CR 202.1): pips W U B R G /4, generic (+{C}) /5
    /// (134–139).
    pub const PIPS: usize = 134;
    /// v9: colours of mana its current mana abilities can make, W U B R G C (140–145).
    pub const MAKES: usize = 140;
    pub const V9_END: usize = 146;
    /// v10 block (opt-in):
    /// first column.
    pub const V10: usize = 146;
    /// v10, object rows: the object's counters of kinds other than +1/+1, -1/-1 and loyalty
    /// (those have C_P1P1 / C_M1M1 / C_LOYALTY; all of them are also summed in C_OTHER), one kind
    /// per column, ascending kind id, at most `N_CTR_SLOTS`: `ctr_pack(id, count)` = stable
    /// counter-kind id (`counter_kind_id`) * 32 + min(count, 31). Exact integers < 2048 (fp16 /
    /// bf16 safe), embedded by a net, **not** linear inputs. 146–148.
    pub const CTR_SLOTS: usize = 146;
    pub const N_CTR_SLOTS: usize = 3;
    /// v10, object rows: an Equipment / Aura attached to a permanent (CR 301.5, 303.4): the
    /// host's entity row + 1 (0: not attached, or the host has no row). An exact integer
    /// (row pointer, read by a net), **not** a linear input.
    pub const HOST_ROW: usize = 149;
    /// End of the v10 entity columns.
    pub const V10_END: usize = 150;
    /// v10.1 block (opt-in):
    /// first column. Linear inputs, kind-specific like the player pool in 29–34.
    pub const V101: usize = 150;
    /// v10.1, permanents (`ObjView::until_eot`, CR 514.2): power that goes away when this turn's
    /// "until end of turn" effects end /10 (POWER's scale; negative for a shrink that ends).
    pub const EOT_POWER: usize = 150;
    /// v10.1, permanents: status flags (`status::*` bits; `ObjView::until_eot`, `cant_attack`,
    /// `cant_block`) as one small integer < `status::VOCAB`, embedded by a net, **not** a
    /// linear input on object rows (player rows use this column for `P_LIFE_GAINED`).
    pub const STATUS_FLAGS: usize = 151;
    /// v10.1, player tokens (`PlayerView::life_lost_this_turn` / `life_gained_this_turn`): life
    /// lost / gained this turn /10 (linear). (Spells cast this turn are already the v7 history
    /// globals `gf::HIST + hs::SPELLS`, the same count; the public scry split,
    /// `PlayerView::owner_knows_top` / `_bottom`, is in the observation only: no column every
    /// existing net has only seen as 0 is left on player rows.)
    pub const P_LIFE_LOST: usize = 150;
    pub const P_LIFE_GAINED: usize = 151;
    /// v10.1, **stack rows only** (a per-row-kind reuse, like the player rows' pool columns; on
    /// object rows column 150 is v10.1's own object feature): which of its source card's
    /// abilities the item is, (`StackView::ability` + 1) / 4, 0 for none (a spell, a delayed /
    /// reflexive trigger, a granted ability). Opt-in, so v9 / v10
    /// nets never read it.
    pub const STACK_ABILITY: usize = 150;
    pub const V101_END: usize = 152;
    /// v10.2, delayed-trigger rows (`Observation::delayed`; KIND_EFFECT rows, the only effect rows
    /// with a non-zero 151): the delay id (`delay::id`: when it triggers), 1..`delay::VOCAB`,
    /// embedded by a v10.2 net, **never** a linear input. `KIND_EFFECT` with
    /// `DELAY_ID > 0` marks the row: every consumer without v10.2 must ignore it.
    pub const DELAY_ID: usize = 151;
    /// v10.2, delayed-trigger rows: the entity row of the first object it refers to that has a row,
    /// + 1 (0: none). An exact integer (row pointer), **never** a linear input.
    pub const DELAY_ROW: usize = 150;
}

/// v10.2 delay ids (`ef::DELAY_ID` on delayed-trigger rows): when a delayed triggered ability
/// triggers (CR 603.7). Stable, append-only; 0 = not a delayed-trigger row.
pub mod delay {
    use crate::basics::PlayerId;
    use crate::observe::DelayedWhenView as W;
    /// "When it leaves the battlefield" to a graveyard only ("when it dies").
    pub const LEAVES_DIES: u8 = 1;
    /// ... to exile only.
    pub const LEAVES_EXILED: u8 = 2;
    /// ... to a graveyard or exile ("when it dies or is exiled", earthbend).
    pub const LEAVES_DIES_OR_EXILED: u8 = 3;
    /// ... to any other set of zones ("when it leaves the battlefield").
    pub const LEAVES_OTHER: u8 = 4;
    /// "When it dies this turn".
    pub const DIES_THIS_TURN: u8 = 5;
    /// "At the beginning of the next <step>": `NEXT_STEP + step slot` (the global step one-hot's
    /// order, 16 slots).
    pub const NEXT_STEP: u8 = 16;
    /// "At the beginning of your next <step>" (the observer's): `MY_NEXT_STEP + step slot`.
    pub const MY_NEXT_STEP: u8 = 32;
    /// "At the beginning of [the opponent]'s next <step>": `OPP_NEXT_STEP + step slot`.
    pub const OPP_NEXT_STEP: u8 = 48;
    /// Embedding rows (ids are < VOCAB).
    pub const VOCAB: usize = 64;

    /// The delay id of `w` from `me`'s point of view.
    pub fn id(w: &W, me: PlayerId) -> u8 {
        match *w {
            W::Leaves { dies: true, exiled: false, other: false, .. } => LEAVES_DIES,
            W::Leaves { dies: false, exiled: true, other: false, .. } => LEAVES_EXILED,
            W::Leaves { dies: true, exiled: true, other: false, .. } => LEAVES_DIES_OR_EXILED,
            W::Leaves { .. } => LEAVES_OTHER,
            W::DiesThisTurnOnly { .. } => DIES_THIS_TURN,
            W::NextStep { step } => NEXT_STEP + super::step_slot(step) as u8,
            W::PlayersNextStep { step, player } => (if player == me { MY_NEXT_STEP } else { OPP_NEXT_STEP }) + super::step_slot(step) as u8,
        }
    }
}

/// Exclusive bound of `ef::CARD2`'s card ids (value `id + 1 <= CARD2_MAX`): integers up to 2048
/// are exact in fp16. A larger id (a CardDb of more than 2048 cards) is written as
/// 0 and counted (`card2_dropped`): `encode` never fails over a column only v10.2 nets read, and a
/// v10.2 net refuses such a CardDb itself.
pub const CARD2_MAX: usize = 2048;

/// Process-wide count of CARD2 values left 0 because the id was past `CARD2_MAX` (diagnostics only:
/// never an input, so the encoding stays a pure function of the observation).
static CARD2_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many CARD2 values were dropped (left 0) for an id past `CARD2_MAX` since the process started.
pub fn card2_dropped() -> u64 {
    CARD2_DROPPED.load(std::sync::atomic::Ordering::Relaxed)
}

/// `ef::CARD2`'s value for card `c`: `id + 1`, or None past `CARD2_MAX` (an inexact id would name
/// another card; the caller writes 0 and it is counted in `card2_dropped`).
pub fn card2_value(c: CardId) -> Option<f32> {
    let v = c.0 as usize + 1;
    if v > CARD2_MAX {
        CARD2_DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return None;
    }
    Some(v as f32)
}

/// The distinct card ids an encoding holds in `ef::CARD2` (value = id + 1 on live object rows, 0 = none), sorted:
/// what a consumer that maps card ids to its own vocabulary must cover besides
/// `entity_card` and the own-deck lists.
pub fn card2_ids(e: &Encoded) -> Vec<CardId> {
    let mut v: Vec<u32> = (0..E)
        .filter(|&r| e.entity_mask[r])
        .map(|r| e.entity[r * F + ef::CARD2])
        .filter(|&x| x > 0.0)
        .map(|x| x as u32 - 1)
        .collect();
    v.sort_unstable();
    v.dedup();
    v.into_iter().map(CardId).collect()
}

/// v10.1 status-flag bits of `ef::STATUS_FLAGS` (object rows).
pub mod status {
    /// Has a keyword granted until end of turn (`EotView::kws` non-zero).
    pub const EOT_KW: u8 = 1;
    /// Can't attack by an effect or rule other than tapped / summoning sickness / defender
    /// (`ObjView::cant_attack`).
    pub const CANT_ATTACK: u8 = 2;
    /// Can't block at all (`ObjView::cant_block`, CR 509.1b).
    pub const CANT_BLOCK: u8 = 4;
    /// Embedding rows (ids 0..VOCAB; 0 = no flag, the padding row).
    pub const VOCAB: usize = 8;
    // v10.2 bits (append only). A net without
    // v10.2 inputs reads the id `& (VOCAB - 1)`, exactly its v10.1 id.
    /// A per-turn-limited trigger of it has reached its limit this turn (`ObjView::trigger_used`).
    pub const TRIGGER_USED: u16 = 8;
    /// Targeted this turn by a spell or ability its controller controls: valiant is used
    /// (`ObjView::valiant_used`).
    pub const VALIANT_USED: u16 = 16;
    /// An exhaust ability of it was activated (CR 702.177a; `ObjView::exhausted`).
    pub const EXHAUSTED: u16 = 32;
    /// A solved Case (CR 719.3b; `ObjView::solved`).
    pub const SOLVED: u16 = 64;
    /// It entered its current zone this turn (`ObjView::entered` == the turn; any zone).
    pub const ENTERED: u16 = 128;
    /// It's exiled instead if it would die this turn (`ObjView::dies_exile`).
    pub const DIES_EXILE: u16 = 256;
    /// Embedding rows of the v10.2 id (all nine bits).
    pub const VOCAB2: usize = 512;
}

/// v10.2 bits of `ef::STACK_FLAGS` (stack rows).
pub mod stackf {
    /// Bits 0–4: the chosen modes (`StackView::modes`, CR 700.2a; spree too): mode i sets bit
    /// `min(i, MODES - 1)`, so bit 4 is "the fifth mode or a later one" (no pool card has more than
    /// five modes). A mode chosen more than once counts once.
    pub const MODES: usize = 5;
    /// An optional additional cost was paid (`StackView::paid_optional`).
    pub const PAID_OPTIONAL: u8 = 32;
    /// Embedding rows (ids 0..VOCAB; 0 = none, the padding row).
    pub const VOCAB: usize = 64;
}

/// Embedding rows of the v10.2 spent-colours id (`ef::SPENT_COLORS`).
pub const SPENT_VOCAB: usize = 32;

/// Where a registered field lives (`V10_2_FIELDS`): an entity row kind (told apart by the KIND_*
/// columns: player 0, object 1, stack 2, effect token 102), the global vector, or option rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Block {
    Player,
    Object,
    Stack,
    Effect,
    Global,
    Option,
}

/// One allocation in the registered ranges (`V10_2_FIELDS`): `bits` = 0 for a whole column,
/// else the bits of an embedded id column it owns. `ver` 101 = v10.1's (kept for the collision
/// check), 102 = v10.2's.
#[derive(Clone, Copy, Debug)]
pub struct Field {
    pub block: Block,
    pub col: usize,
    pub bits: u16,
    pub ver: u16,
    pub name: &'static str,
}

const fn fld(block: Block, col: usize, bits: u16, ver: u16, name: &'static str) -> Field {
    Field { block, col, bits, ver, name }
}

/// Column-allocation registry of the ranges every v10 encoding leaves zero (entity 123–127 and
/// 150–151 per row kind, globals 128–175, options 84–95): every value v10.1 / v10.2 write there.
/// Tests check that no two fields share a column (or an id bit), that every v10.2 field is in
/// one of these ranges, and that encodings write nothing there outside the registry. **Append
/// only**.
pub const V10_2_FIELDS: &[Field] = {
    use Block::*;
    &[
        // ---- v10.1 occupants
        fld(Object, ef::EOT_POWER, 0, 101, "v10.1 until-end-of-turn power"),
        fld(Object, ef::STATUS_FLAGS, status::EOT_KW as u16, 101, "v10.1 status: keyword until end of turn"),
        fld(Object, ef::STATUS_FLAGS, status::CANT_ATTACK as u16, 101, "v10.1 status: can't attack"),
        fld(Object, ef::STATUS_FLAGS, status::CANT_BLOCK as u16, 101, "v10.1 status: can't block"),
        fld(Player, 150, 0, 101, "v10.1 life lost this turn"),
        fld(Player, 151, 0, 101, "v10.1 life gained this turn"),
        fld(Stack, 150, 0, 101, "v10.1 which ability a stack item is"),
        fld(Option, of::KIND_REF, 0, 101, "v10.1 other-slot decision kind id"),
        fld(Option, of::ALT_TGT_FILL, 0, 101, "v10.1 alt-method target marker"),
        // ---- v10.2: links
        fld(Object, ef::PAIR_ROW, 0, 102, "combat pairing pointer (row + 1)"),
        fld(Object, ef::EXILED_WITH_ROW, 0, 102, "exiled-with holder pointer (row + 1)"),
        fld(Object, ef::CARD2, 0, 102, "second card id + 1"),
        // ---- v10.2: delayed-trigger rows (KIND_EFFECT rows >= E_V9 with DELAY_ID > 0)
        fld(Effect, ef::DELAY_ID, 0, 102, "delayed-trigger delay id (when it triggers; marks the row)"),
        fld(Effect, ef::DELAY_ROW, 0, 102, "delayed trigger's first referenced object row + 1"),
        // ---- v10.2: status bits, stack flags, globals, options
        fld(Object, ef::STATUS_FLAGS, status::TRIGGER_USED, 102, "status: once-each-turn trigger used"),
        fld(Object, ef::STATUS_FLAGS, status::VALIANT_USED, 102, "status: valiant used"),
        fld(Object, ef::STATUS_FLAGS, status::EXHAUSTED, 102, "status: exhaust ability used"),
        fld(Object, ef::STATUS_FLAGS, status::SOLVED, 102, "status: Case solved"),
        fld(Object, ef::STATUS_FLAGS, status::ENTERED, 102, "status: entered its zone this turn"),
        fld(Object, ef::STATUS_FLAGS, status::DIES_EXILE, 102, "status: exiled instead if it would die this turn"),
        fld(Stack, ef::STACK_FLAGS, (1 << stackf::MODES) - 1, 102, "stack: chosen modes"),
        fld(Stack, ef::STACK_FLAGS, stackf::PAID_OPTIONAL as u16, 102, "stack: optional additional cost paid"),
        fld(Stack, ef::SPENT_COLORS, 0, 102, "stack: colours of mana spent to cast it (id)"),
        fld(Global, gf::DIED, 0, 102, "creatures died this turn (me)"),
        fld(Global, gf::DIED + 1, 0, 102, "creatures died this turn (opp)"),
        fld(Global, gf::LEFT_BF, 0, 102, "nonland permanents left the battlefield this turn (me)"),
        fld(Global, gf::LEFT_BF + 1, 0, 102, "nonland permanents left the battlefield this turn (opp)"),
        fld(Global, gf::SACRIFICED, 0, 102, "permanents sacrificed this turn (me)"),
        fld(Global, gf::SACRIFICED + 1, 0, 102, "permanents sacrificed this turn (opp)"),
        fld(Global, gf::DESCENDED, 0, 102, "descended this turn (me)"),
        fld(Global, gf::DESCENDED + 1, 0, 102, "descended this turn (opp)"),
        fld(Global, gf::WARPED, 0, 102, "spells warped this turn (me)"),
        fld(Global, gf::WARPED + 1, 0, 102, "spells warped this turn (opp)"),
        fld(Global, gf::NEXT_UNCOUNTERABLE, 0, 102, "next spell this turn can't be countered (me)"),
        fld(Global, gf::NEXT_UNCOUNTERABLE + 1, 0, 102, "next spell this turn can't be countered (opp)"),
        fld(Global, gf::PLAYER_HEXPROOF, 0, 102, "player hexproof (me)"),
        fld(Global, gf::PLAYER_HEXPROOF + 1, 0, 102, "player hexproof (opp)"),
        fld(Global, gf::KNOWN_TOP, 0, 102, "scry split: known top cards (me)"),
        fld(Global, gf::KNOWN_TOP + 1, 0, 102, "scry split: known top cards (opp)"),
        fld(Global, gf::KNOWN_BOTTOM, 0, 102, "scry split: known bottom cards (me)"),
        fld(Global, gf::KNOWN_BOTTOM + 1, 0, 102, "scry split: known bottom cards (opp)"),
        fld(Option, of::UNLESS_AMOUNT, 0, 102, "PayUnless amount"),
    ]
};

// Global feature columns.
pub mod gf {
    /// Pairs (me, opp): life, poison, hand, library, graveyard, lands played, mulligans.
    pub const LIFE: usize = 0;
    pub const POISON: usize = 2;
    pub const HAND: usize = 4;
    pub const LIBRARY: usize = 6;
    pub const GRAVEYARD: usize = 8;
    pub const LANDS_PLAYED: usize = 10;
    pub const MULLIGANS: usize = 12;
    pub const TURN: usize = 14;
    pub const ACTIVE: usize = 15;
    /// Pair (me, opp): permanents controlled.
    pub const PERMANENTS: usize = 16;
    pub const STACK: usize = 18;
    pub const HAS_DECISION: usize = 19;
    pub const N_OPTIONS: usize = 20;
    pub const ENT_TRUNC: usize = 21;
    pub const OPT_TRUNC: usize = 22;
    pub const GAME_OVER: usize = 23;
    pub const STEP: usize = 24;
    pub const STAGE: usize = 40;
    pub const KIND: usize = 44;
    /// Pair (me, opp): speed /4 (CR 702.179).
    pub const SPEED: usize = 76;
    /// Pair (me, opp): has an enduring story (CR 702.195b).
    pub const ENDURING_STORY: usize = 78;
    /// v7 public history: for player j (0 = me, 1 = opp) and turn `turn - k` (k = 0 current, 1,
    /// 2): `HIST + (j * 3 + k) * HIST_STATS + stat`, stats in `hs::*`. Columns 80–121.
    pub const HIST: usize = 80;
    pub const HIST_STATS: usize = 7;
    /// v7 pair (me, opp): combat damage taken this turn /10 (122–123), creatures lost in combat
    /// this turn /4 (124–125).
    pub const COMBAT_DMG: usize = 122;
    pub const COMBAT_DEATHS: usize = 124;
    /// v8: my `Target` decision: which targeted part of the spell / ability it is for /4
    /// (1-based; CR 601.2c) and how many targeted parts it has /4. 0 otherwise.
    pub const TGT_PART: usize = 126;
    pub const TGT_PARTS: usize = 127;
    /// v9: first column of the v9 blocks
    /// (128–159 belief block, 160–175 best-of-three match globals; zero in this build). v10.2
    /// released both reservations: 128–149 hold `V102`, 150–175 spare.
    pub const V9: usize = 128;
    /// v10.2 globals (opt-in; linear), pairs (me, opp), this turn:
    /// creatures that died under the player's control /4 (128–129), nonland permanents that left the
    /// battlefield under their control /4 (130–131), permanents they sacrificed /4 (132–133), times
    /// they descended /4 (134–135), spells they cast for warp costs /2 (136–137); the next spell they
    /// cast this turn can't be countered (138–139); they have hexproof (140–141); the public scry
    /// split of their library (v10.1 left it JSON-only): cards at the top /
    /// at the bottom of it that its owner knows (`PlayerView::owner_knows_top` / `_bottom`, the same
    /// values) /4 (142–143, 144–145). `PlayerView`.
    pub const V102: usize = 128;
    pub const DIED: usize = 128;
    pub const LEFT_BF: usize = 130;
    pub const SACRIFICED: usize = 132;
    pub const DESCENDED: usize = 134;
    pub const WARPED: usize = 136;
    pub const NEXT_UNCOUNTERABLE: usize = 138;
    pub const PLAYER_HEXPROOF: usize = 140;
    pub const KNOWN_TOP: usize = 142;
    pub const KNOWN_BOTTOM: usize = 144;
    pub const V102_END: usize = 146;
    /// v9 own-deck block (`dk::*`, `DECK_NB` columns): my remaining library as a multiset and my
    /// sideboard (`Observation::own_deck`).
    pub const DECK: usize = 176;
    /// v10 block (opt-in): first column (400–415, linear inputs).
    pub const V10: usize = 400;
    /// v10: mana in my / the opponent's pool that stays when the current step ends
    /// (firebending, CR 702.189a: kept until the end of combat; `PlayerView::pool_kept`),
    /// W U B R G C /5, me 400–405, opponent 406–411.
    pub const POOL_KEPT: usize = 400;
    /// v10 pair (me, opp): turns the player will skip (CR 614.10; Ral Zarek, Guest Lecturer) /4,
    /// 412–413.
    pub const SKIP_TURNS: usize = 412;
    /// v10 pair (me, opp): cards owned by the player in exile face down that the observer may not
    /// look at (CR 406.3; they get no entity rows) /10, 414–415.
    pub const FACE_DOWN_EXILE: usize = 414;
    pub const V10_END: usize = 416;
}

/// Entries of the own-deck block's card lists (`dk::LIB_IDS`, `dk::SIDE_IDS`): distinct cards
/// that may be in my library (more: the most copies first, `dk::LIB_TRUNC`), and of my
/// sideboard (CR 100.4a: at most 15 cards).
pub const DECK_LIB: usize = 48;
pub const DECK_SIDE: usize = 15;
/// Columns of the own-deck block (`gf::DECK`).
pub const DECK_NB: usize = 224;

/// Own-deck block columns (v9), relative to `gf::DECK`. Summary columns 0–31 are linear inputs;
/// the card lists are not: each entry is (card id / 256, card id % 256, copies), exact small
/// integers (fp16 / bf16 safe) a net embeds, `copies` 0 = no entry.
pub mod dk {
    /// 1 when the observation carries the own-deck view (always, from `observe`).
    pub const KNOWN: usize = 0;
    /// Cards that may be in my library /40 (my library + `UNSEEN`).
    pub const CARDS: usize = 1;
    /// My cards outside the library I can't see /4.
    pub const UNSEEN: usize = 2;
    /// More distinct cards than `DECK_LIB` entries.
    pub const LIB_TRUNC: usize = 3;
    /// /20: lands, creatures, instants, sorceries, other nonland cards.
    pub const LANDS: usize = 4;
    pub const CREATURES: usize = 5;
    pub const INSTANTS: usize = 6;
    pub const SORCERIES: usize = 7;
    pub const OTHER: usize = 8;
    /// Nonland cards by mana value 0..6, 7+ /10 (8 columns).
    pub const MV: usize = 9;
    /// Mean mana value of the nonland cards /5.
    pub const MV_MEAN: usize = 17;
    /// Nonland cards of each colour W U B R G, colourless /20 (6 columns).
    pub const COLORS: usize = 18;
    /// Lands that can make W U B R G /20 (5 columns).
    pub const LAND_COLORS: usize = 24;
    /// My sideboard's size /15.
    pub const SIDE_CARDS: usize = 29;
    /// More distinct sideboard cards than `DECK_SIDE` entries.
    pub const SIDE_TRUNC: usize = 30;
    /// Summary columns (linear inputs): 0..SUMMARY.
    pub const SUMMARY: usize = 32;
    /// `DECK_LIB` entries of 3 columns, then `DECK_SIDE` entries of 3 columns.
    pub const LIB_IDS: usize = 32;
    pub const SIDE_IDS: usize = LIB_IDS + 3 * super::DECK_LIB;
    pub const IDS_END: usize = SIDE_IDS + 3 * super::DECK_SIDE;
}

/// Per-turn history stats (`gf::HIST`): spells cast /3, abilities activated /3, cards drawn /3,
/// lands played, cards discarded /3, hand size as the turn began /7, untapped lands as the turn
/// ended /5 (0 until it ends).
pub mod hs {
    pub const SPELLS: usize = 0;
    pub const ACTIVATIONS: usize = 1;
    pub const DRAWS: usize = 2;
    pub const LANDS: usize = 3;
    pub const DISCARDS: usize = 4;
    pub const HAND_START: usize = 5;
    pub const UNTAPPED_END: usize = 6;
}

/// Subtype embedding id (`SUBTYPE_VOCAB`) of a CardDb subtype.
pub fn subtype_key(st: Subtype) -> usize {
    if (st as usize) < crate::carddb::cr_subtype_count().min(SUBTYPE_VOCAB - 2) {
        st as usize + 1
    } else {
        SUBTYPE_VOCAB - 1
    }
}

// Option feature columns.
pub mod of {
    pub const TYPE: usize = 0;
    pub const KIND: usize = 32;
    pub const PLAYER_ME: usize = 64;
    pub const PLAYER_OPP: usize = 65;
    /// One-hot W U B R G C.
    pub const MANA: usize = 66;
    pub const NUMBER: usize = 72;
    pub const INDEX: usize = 73;
    pub const N_REFS: usize = 74;
    /// v6: Activate/Mana options of the same source object in this decision: this option's
    /// rank by ability (/4) and how many there are (/4), so different abilities of one
    /// permanent are distinguishable (e.g. equip vs. another activated ability).
    pub const ABILITY_RANK: usize = 75;
    pub const ABILITY_COUNT: usize = 76;
    /// v7: Cast / Activate options whose spell or ability targets: how many legal targets are
    /// mine / the opponent's right now (/4), and that it targets at all (`Observation::
    /// option_targets`; e.g. a removal spell whose only legal target is my own creature).
    /// v10.1 (same meaning): also every casting method's option (`Alt`
    /// casts: airbend and its other face, mayhem, harmonize, plotted, the granted second
    /// flashback, ...; `CastPrepared`), from the face its spell comes from.
    pub const TGT_MINE: usize = 77;
    pub const TGT_OPP: usize = 78;
    pub const TARGETS: usize = 79;
    /// v8: of the opponent's legal targets, how many have ward /4 (CR 702.21a).
    pub const TGT_OPP_WARD: usize = 80;
    /// v10 block (opt-in): first column.
    pub const V10: usize = 81;
    /// v10: "remove a counter from [this]" (`ext5::remove_any_counter`, a `PayCost` decision of
    /// `Mode(i)` options): the stable kind id (`counter_kind_id`) of the counter
    /// this option removes. An exact integer a net embeds, **not** a linear input.
    pub const CTR_KIND: usize = 81;
    /// v10: that kind's number of counters on the permanent /5.
    pub const CTR_COUNT: usize = 82;
    /// v10: a `Target` option naming an ability on the stack (`Target::Ability`, `ext8`): its
    /// stack row is in `option_refs[1]` (refs[0] stays its source's row, as in v9).
    pub const TGT_ABILITY: usize = 83;
    pub const V10_END: usize = 84;
    /// v10.1 block (opt-in): first column. Linear inputs.
    pub const V101: usize = 84;
    /// v10.1: decision kinds without a slot of their own share the "other" kind slot
    /// (`KIND_SLOTS - 1` in `KIND`, v9 meaning kept): every option of such a decision carries
    /// which kind it is here, `kind_ref(kind) + 1` (0 = none): an exact small integer a net
    /// embeds (< `KIND_REF_VOCAB`), **not** a linear input. 85: v10.2 `UNLESS_AMOUNT`; 86–94 spare (zero).
    pub const KIND_REF: usize = 84;
    /// v10.1, column 95 (opt-in): 1 on a casting option whose target columns
    /// 77–80 are filled through the casting-method / face mapping: an `Alt` cast
    /// (airbend and its other face, mayhem, harmonize, plotted, the granted second flashback, ...)
    /// or a `CastPrepared` copy, i.e. features that were empty before v10.1; 0 everywhere else
    /// (normal casts, CastFace, Flashback, Activate, untargeted spells). Outside the v9 view.
    pub const ALT_TGT_FILL: usize = 95;
    /// v10.2 (linear): every option of my `PayUnless` decision: what paying costs
    /// (`Observation::pay_unless`): mana value /5 for a mana cost, **minus** life /5 for a life cost
    /// (one column, the sign tells them apart), 0 for another kind (behold) or nothing due.
    pub const UNLESS_AMOUNT: usize = 85;
    pub const V101_END: usize = 96;
}

/// Embedding rows of the v10.1 kind refinement id (`of::KIND_REF`): ids 1..=9 used today.
pub const KIND_REF_VOCAB: usize = 32;

/// v10.1: index of a decision kind in the "other" kind slot's refinement (`of::KIND_REF`), for
/// every kind whose `kind_slot` is `KIND_SLOTS - 1`; None for kinds with their own slot. **Append
/// only** (a trained net reads these columns). A kind that lands in the "other" slot without an
/// index here fails a test.
#[allow(unreachable_patterns)]
pub fn kind_ref(k: DecisionKind) -> Option<usize> {
    use DecisionKind::*;
    if kind_slot(k) != KIND_SLOTS - 1 {
        return None;
    }
    Some(match k {
        PayUnless => 0,
        Dig => 1,
        ChooseType => 2,
        ChooseCard => 3,
        NameCard => 4,
        CopyChoice => 5,
        PlayDraw => 6,
        Sideboard => 7,
        ChooseReplacement => 8,
        _ => return None,
    })
}

/// v10 stable counter-kind ids (`ef::CTR_SLOTS`, `of::CTR_KIND`): index + 1 in this table
/// (0 = none), independent of the CardDb (whose `Named` kinds are interned in load order).
/// Names are Forge counter names normalised by `counter_key` (upper case, letters and digits
/// only). **Append only**: a trained model's embedding rows are keyed by these ids. Any other
/// kind gets `CTR_OTHER`. The first 20: CR 122.1 kinds with rules meaning (+1/+1, -1/-1,
/// loyalty, stun 122.1d, shield 122.1c, the keyword counters of 122.1b); then the kinds the
/// card pool puts on permanents, then common Standard ones.
pub const COUNTER_KINDS: [&str; 56] = [
    "P1P1", "M1M1", "LOYALTY", "STUN", "SHIELD", "FLYING", "FIRSTSTRIKE", "DOUBLESTRIKE", "DEATHTOUCH", "DECAYED",
    "EXALTED", "HASTE", "HEXPROOF", "INDESTRUCTIBLE", "LIFELINK", "MENACE", "REACH", "SHADOW", "TRAMPLE", "VIGILANCE",
    "LORE", "CHARGE", "TIME", "FINALITY", "OIL", "QUEST", "PLAN", "BLIGHT", "PAGE", "INCUBATION",
    "NET", "CONQUEROR", "HONE", "CHORUS", "FLOOD", "DIVINITY", "LANDMARK", "FILM", "REV", "STASH",
    "INGENUITY", "VERSE", "AGE", "FADE", "ICE", "LEVEL", "KI", "FATE", "BOUNTY", "COIN",
    "DEPLETION", "DREAM", "COLLECTION", "HIT", "VOID", "BLOOD",
];
/// Size of the counter-kind id space (embedding rows): ids 0..CTR_VOCAB, `CTR_OTHER` last.
pub const CTR_VOCAB: usize = 64;
pub const CTR_OTHER: u8 = (CTR_VOCAB - 1) as u8;
const _: () = assert!(COUNTER_KINDS.len() < CTR_VOCAB - 1);

/// A counter name as `COUNTER_KINDS` spells it: upper case, letters and digits only
/// ("First Strike", "FIRST_STRIKE" -> "FIRSTSTRIKE").
pub fn counter_key(name: &str) -> String {
    name.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

/// Stable id of a counter kind by name (1.., `CTR_OTHER` for a kind not in `COUNTER_KINDS`).
pub fn counter_kind_id(name: &str) -> u8 {
    let k = counter_key(name);
    COUNTER_KINDS.iter().position(|&n| n == k).map_or(CTR_OTHER, |i| i as u8 + 1)
}

/// Stable id of a `CounterKind` (`db_names` = `CardDb::counter_names`, for `Named`).
#[allow(unreachable_patterns)]
pub fn counter_id(k: CounterKind, db_names: &[String]) -> u8 {
    match k {
        CounterKind::P1P1 => 1,
        CounterKind::M1M1 => 2,
        CounterKind::Loyalty => 3,
        CounterKind::Stun => 4,
        CounterKind::Named(i) => db_names.get(i as usize).map_or(CTR_OTHER, |n| counter_kind_id(n)),
        _ => CTR_OTHER,
    }
}

/// `ef::CTR_SLOTS` value of one counter kind on an object.
pub fn ctr_pack(id: u8, count: i32) -> f32 {
    (id as u32 * 32 + count.clamp(0, 31) as u32) as f32
}

/// Bucket index of a decision kind (`KIND_SLOTS - 1` = other/unknown).
#[allow(unreachable_patterns)]
pub fn kind_slot(k: DecisionKind) -> usize {
    use DecisionKind::*;
    match k {
        Mulligan => 0,
        BottomCard => 1,
        Priority => 2,
        ChooseX => 3,
        Target => 4,
        PayMana => 5,
        DeclareAttackers => 6,
        DeclareBlockers => 7,
        AssignDamage => 8,
        LegendRule => 9,
        OrderTriggers => 10,
        OptionalEffect => 11,
        Scry => 12,
        Surveil => 13,
        Discard => 14,
        PayLife => 15,
        Search => 16,
        Sacrifice => 17,
        ChooseMode => 18,
        Kicker => 19,
        AdditionalCost => 20,
        PayCost => 21,
        _ => KIND_SLOTS - 1,
    }
}

/// Bucket index of an option variant (`OPT_SLOTS - 1` = other/unknown).
#[allow(unreachable_patterns)]
pub fn opt_slot(o: &Opt) -> usize {
    use Opt::*;
    match o {
        Keep => 0,
        Mulligan => 1,
        Card(_) => 2,
        Pass => 3,
        PlayLand(_) => 4,
        Cast(_) => 5,
        Activate(..) => 6,
        // An ability on the stack (`ext8`) encodes as the object target it was before.
        Target(crate::state::Target::Obj(_) | crate::state::Target::Ability { .. }) => 7,
        Target(crate::state::Target::Player(_)) => 8,
        Number(_) => 9,
        Mana(..) => 10,
        Attack(..) => 11,
        Block(..) => 12,
        Block2(..) => 13,
        Trigger(_) => 14,
        Done => 15,
        Yes => 16,
        No => 17,
        ScryTop => 18,
        ScryBottom => 19,
        ToGraveyard => 20,
        Mode(_) => 21,
        Alt(..) => 22,
        CostOption(_) => 23,
        PoolMana(_) => 24,
        Subtype(_) => 25,
        Name(_) => 26,
        // v8 (the "other" slot before)
        CastFace(..) => 27,
        Flashback(_) => 28,
        CastPrepared(_) => 29,
        _ => OPT_SLOTS - 1,
    }
}

#[allow(unreachable_patterns)]
fn step_slot(s: Step) -> usize {
    use Step::*;
    match s {
        Untap => 0,
        Upkeep => 1,
        Draw => 2,
        Main1 => 3,
        BeginCombat => 4,
        DeclareAttackers => 5,
        DeclareBlockers => 6,
        FirstStrikeDamage => 7,
        CombatDamage => 8,
        EndCombat => 9,
        Main2 => 10,
        End => 11,
        Cleanup => 12,
        _ => STEP_SLOTS - 1,
    }
}

#[allow(unreachable_patterns)]
fn stage_slot(s: Stage) -> usize {
    match s {
        Stage::Mulligan => 0,
        Stage::Playing => 1,
        Stage::Over => 2,
        _ => 3,
    }
}

/// Objects an option names (up to R).
#[allow(unreachable_patterns)]
pub(crate) fn opt_objs(o: &Opt) -> [Option<ObjId>; R] {
    match *o {
        Opt::Card(x) | Opt::PlayLand(x) | Opt::Cast(x) | Opt::Alt(x, _) | Opt::Activate(x, _) | Opt::Mana(x, _, _) | Opt::Attack(x, _) | Opt::Target(Target::Obj(x) | Target::Ability { src: x, .. }) | Opt::CastPrepared(x) => [Some(x), None, None],
        Opt::Block(x, a) | Opt::AttackPw(x, a) => [Some(x), Some(a), None],
        Opt::Block2(x, y, a) => [Some(x), Some(y), Some(a)],
        _ => [None; R],
    }
}

/// (source object, ability id, is a mana ability) of an Activate/Mana option.
fn ability_of(o: &Opt) -> Option<(u32, u32, bool)> {
    match *o {
        Opt::Activate(x, a) => Some((x.0, a.0, false)),
        Opt::Mana(x, a, _) => Some((x.0, a.0, true)),
        _ => None,
    }
}

/// Mutable views of one game's slot in the output buffers (row-major, sizes per the constants).
pub struct Out<'a> {
    pub entity: &'a mut [f32],       // E*F
    pub entity_card: &'a mut [i32],  // E
    pub entity_mask: &'a mut [bool], // E
    pub global: &'a mut [f32],       // G
    pub option: &'a mut [f32],       // O*K
    pub option_mask: &'a mut [bool], // O
    pub option_refs: &'a mut [i32],  // O*R
    /// Ability id (CardDb-specific, like card ids) for Activate/Mana options, else -1.
    pub option_ability: &'a mut [i32], // O
}

/// Owned single-game encoding.
#[derive(Clone, Debug, PartialEq)]
pub struct Encoded {
    pub entity: Vec<f32>,
    pub entity_card: Vec<i32>,
    pub entity_mask: Vec<bool>,
    pub global: Vec<f32>,
    pub option: Vec<f32>,
    pub option_mask: Vec<bool>,
    pub option_refs: Vec<i32>,
    pub option_ability: Vec<i32>,
}

impl Default for Encoded {
    fn default() -> Self {
        Encoded {
            entity: vec![0.0; E * F],
            entity_card: vec![-1; E],
            entity_mask: vec![false; E],
            global: vec![0.0; G],
            option: vec![0.0; O * K],
            option_mask: vec![false; O],
            option_refs: vec![-1; O * R],
            option_ability: vec![-1; O],
        }
    }
}

impl Encoded {
    pub fn out(&mut self) -> Out<'_> {
        Out {
            entity: &mut self.entity,
            entity_card: &mut self.entity_card,
            entity_mask: &mut self.entity_mask,
            global: &mut self.global,
            option: &mut self.option,
            option_mask: &mut self.option_mask,
            option_refs: &mut self.option_refs,
            option_ability: &mut self.option_ability,
        }
    }
}

/// Encode an observation (and its decision, if any) into a fresh buffer.
pub fn encode(obs: &Observation) -> Encoded {
    let mut e = Encoded::default();
    encode_into(obs, e.out());
    e
}

/// Is entity row `r` of `entity` (an `[E * F]` buffer) a v10.2 delayed-trigger row (`KIND_EFFECT` with
/// `DELAY_ID > 0`)? Consumers without v10.2 ignore these rows.
pub fn is_delayed_row(entity: &[f32], r: usize) -> bool {
    entity[r * F + ef::KIND_EFFECT] > 0.0 && entity[r * F + ef::DELAY_ID] > 0.0
}

/// Remove the v10.2 delayed-trigger rows (features 0, card -1, mask false): what a v10.1 build encodes
/// for the same observation (they are the last rows, so the mask stays a prefix). Returns how many.
pub fn strip_delayed(e: &mut Encoded) -> usize {
    let mut n = 0;
    for r in 0..E {
        if e.entity_mask[r] && is_delayed_row(&e.entity, r) {
            e.entity[r * F..(r + 1) * F].fill(0.0);
            e.entity_card[r] = -1;
            e.entity_mask[r] = false;
            n += 1;
        }
    }
    n
}

/// The v10.1 view of a v10.2 encoding: every
/// v10.2 value removed, i.e. what a v10.1 build encodes for the same observation. The delayed-trigger rows
/// (`strip_delayed`), entity 123–127 on every row (the links, the spent colours), the v10.2
/// bits of the col-151 ids (object rows keep v10.1's bits 0–2; stack rows' 151 is v10.2's alone), globals
/// 128–175 and option column 85. Returns the number of delayed rows removed.
pub fn strip_v10_2(e: &mut Encoded) -> usize {
    let n = strip_delayed(e);
    for r in e.entity.chunks_mut(F) {
        r[ef::V102_LINKS..ef::COLOR].fill(0.0);
        if r[ef::KIND_STACK] > 0.0 {
            r[ef::STACK_FLAGS] = 0.0;
        } else if r[ef::KIND_OBJECT] > 0.0 && r[ef::KIND_PLAYER] == 0.0 {
            r[ef::STATUS_FLAGS] = ((r[ef::STATUS_FLAGS] as u32) & (status::VOCAB as u32 - 1)) as f32;
        }
    }
    e.global[gf::V9..gf::DECK].fill(0.0); // the released reservation 128–175 (v10.2: 128–145)
    for i in 0..O {
        e.option[i * K + of::UNLESS_AMOUNT] = 0.0;
    }
    n
}

/// Summary of what didn't fit (also reflected in the global features).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Overflow {
    pub entities_dropped: usize,
    pub options_dropped: usize,
}

struct Builder<'a, 'o> {
    out: Out<'a>,
    obs: &'o Observation,
    n: usize,
    dropped: usize,
    /// (object id, entity row), sorted after collection.
    ids: Vec<(u32, u16)>,
    /// Rows may be added below this (`E_V9` for the v9 layout, `E` for reserved rows).
    cap: usize,
    /// Lookups see rows below this only (`E_V9`: the refs a v9 encoding has).
    lim: usize,
    /// Objects the v9 layout had no row for: (view, zone, zone index) or a known library card
    /// (player, index into `library_known`), for the reserved rows.
    cut: Vec<Cut<'o>>,
}

#[derive(Clone, Copy)]
enum Cut<'o> {
    Obj(&'o ObjView, usize, usize),
    Lib(PlayerId, usize),
}

impl<'a, 'o> Builder<'a, 'o> {
    fn row(&mut self, card: Option<CardId>) -> Option<usize> {
        if self.n >= self.cap {
            self.dropped += 1;
            return None;
        }
        let r = self.n;
        self.n += 1;
        self.out.entity_mask[r] = true;
        self.out.entity_card[r] = card.map_or(-1, |c| c.0 as i32);
        Some(r)
    }
    fn set(&mut self, r: usize, col: usize, v: f32) {
        self.out.entity[r * F + col] = v;
    }
    fn me(&self) -> PlayerId {
        self.obs.me
    }
    fn obj(&mut self, v: &'o ObjView, zone: usize, zone_index: usize) -> Option<usize> {
        let Some(r) = self.row(v.card) else {
            self.cut.push(Cut::Obj(v, zone, zone_index));
            return None;
        };
        self.ids.push((v.id.0, r as u16));
        let me = self.me();
        self.set(r, ef::KIND_OBJECT, 1.0);
        self.set(r, ef::ZONE + zone, 1.0);
        self.set(r, ef::MINE, (v.controller == me) as u8 as f32);
        self.set(r, ef::OWNED, (v.owner == me) as u8 as f32);
        self.set(r, ef::HIDDEN, v.card.is_none() as u8 as f32);
        self.set(r, ef::TAPPED, v.tapped as u8 as f32);
        self.set(r, ef::SICK, v.sick as u8 as f32);
        self.set(r, ef::ATTACKING, v.attacking as u8 as f32);
        self.set(r, ef::BLOCKING, v.blocking.is_some() as u8 as f32);
        self.set(r, ef::TOKEN, v.token as u8 as f32);
        {
            use crate::altcast::flag;
            self.set(r, ef::ALT_TEMPORARY, (v.alt & (flag::WARP | flag::MOBILIZED) != 0) as u8 as f32);
            self.set(r, ef::ALT_CASTABLE_EXILE, (v.alt & (flag::WARPED | flag::PLOTTED | flag::AIRBENT) != 0) as u8 as f32);
        }
        if let (Some(p), Some(t)) = (v.power, v.toughness) {
            self.set(r, ef::HAS_PT, 1.0);
            self.set(r, ef::POWER, p as f32 / 10.0);
            self.set(r, ef::TOUGHNESS, t as f32 / 10.0);
        }
        self.set(r, ef::DAMAGE, v.damage as f32 / 10.0);
        for &(k, n) in &v.counters {
            #[allow(unreachable_patterns)]
            let (col, scale) = match k {
                CounterKind::P1P1 => (ef::C_P1P1, 5.0),
                CounterKind::M1M1 => (ef::C_M1M1, 5.0),
                CounterKind::Loyalty => (ef::C_LOYALTY, 10.0),
                _ => (ef::C_OTHER, 5.0),
            };
            self.out.entity[r * F + col] += n as f32 / scale;
        }
        // v10: per-kind counters beyond the three dense columns, ascending stable kind id.
        if v.counter_ids.len() == v.counters.len() && v.counters.len() > 0 {
            let mut ks: smallvec::SmallVec<[(u8, i32); 4]> = v
                .counters
                .iter()
                .zip(&v.counter_ids)
                .filter(|((k, n), _)| *n > 0 && !matches!(k, CounterKind::P1P1 | CounterKind::M1M1 | CounterKind::Loyalty))
                .map(|((_, n), &id)| (id, *n))
                .collect();
            ks.sort_unstable();
            for (j, &(id, n)) in ks.iter().take(ef::N_CTR_SLOTS).enumerate() {
                self.set(r, ef::CTR_SLOTS + j, ctr_pack(id, n));
            }
        }
        self.set(r, ef::ZONE_INDEX, zone_index as f32 / 20.0);
        self.set(r, ef::PREPARED, v.prepared as u8 as f32);
        self.set(r, ef::ATTACHED, v.attached_to.is_some() as u8 as f32);
        self.set(r, ef::ATTACKING_PW, v.attacking_pw.is_some() as u8 as f32);
        // A face-down permanent (CR 708) isn't showing its card's front face either.
        self.set(r, ef::ALT_FACE, (v.face != 0 || v.face_down) as u8 as f32);
        self.set(r, ef::COPY_OR_BACK, v.copy_of_printed.is_some() as u8 as f32);
        self.set(r, ef::HARNESSED, v.harnessed as u8 as f32);
        for t in 0..ef::N_TYPES {
            if v.types & (1 << t) != 0 {
                self.set(r, ef::TYPES + t, 1.0);
            }
        }
        for k in 0..ef::N_KWS {
            if v.kws & (1 << k) != 0 {
                self.set(r, ef::KWS + k, 1.0);
            }
        }
        if let Some(mv) = v.mv {
            self.set(r, ef::MV, mv as f32 / 10.0);
            // v9 colour block (characteristics are visible exactly when `mv` is)
            write_colors(&mut self.out.entity[r * F..(r + 1) * F], v.colors, &v.pips, v.makes);
        }
        self.set(r, ef::DECLARING, v.declaring as u8 as f32);
        if let Some(st) = v.chosen_type {
            self.subtype(r, subtype_key(st));
        }
        // v8
        if let Some(l) = v.class_level {
            self.set(r, ef::CLASS_LEVEL, l as f32 / 3.0);
        }
        if let Some(w) = &v.ward {
            self.set(r, ef::WARD, 1.0);
            self.set(r, ef::WARD_MANA, w.mana as f32 / 4.0);
            self.set(r, ef::WARD_LIFE, w.life as f32 / 5.0);
            self.set(r, ef::WARD_OTHER, w.other as u8 as f32);
        }
        self.set(r, ef::HEXPROOF_FROM_MONO, (v.hexproof_from & crate::observe::HEXPROOF_FROM_MONO != 0) as u8 as f32);
        self.set(r, ef::RMANA, v.rmana as f32 / 5.0);
        // v10.1: what ends with this turn's "until end of turn" effects
        if let Some(t) = &v.until_eot {
            self.set(r, ef::EOT_POWER, t.power as f32 / 10.0);
        }
        let flags = (v.until_eot.is_some_and(|t| t.kws != 0) as u8 * status::EOT_KW) | (v.cant_attack as u8 * status::CANT_ATTACK) | (v.cant_block as u8 * status::CANT_BLOCK);
        // v10.2: "already used" state and this turn's zone change, more bits of the same id
        let turn = self.obs.turn;
        let used = (v.trigger_used as u16 * status::TRIGGER_USED)
            | (v.valiant_used as u16 * status::VALIANT_USED)
            | (v.exhausted as u16 * status::EXHAUSTED)
            | (v.solved as u16 * status::SOLVED)
            | ((turn > 0 && v.entered == turn) as u16 * status::ENTERED)
            | (v.dies_exile as u16 * status::DIES_EXILE);
        self.set(r, ef::STATUS_FLAGS, (flags as u16 | used) as f32);
        // v10.2: a second card id: the card named for it, else a copy's own card (visible only)
        if let (Some(_), Some(c)) = (v.card, v.named_card.or(v.copy_of_printed)) {
            self.set(r, ef::CARD2, card2_value(c).unwrap_or(0.0));
        }
        Some(r)
    }
    fn subtype(&mut self, r: usize, key: usize) {
        self.set(r, ef::SUBTYPE_HI, (key / 32) as f32);
        self.set(r, ef::SUBTYPE_LO, (key % 32) as f32);
    }
    fn lookup(&self, id: ObjId) -> Option<usize> {
        self.ids.binary_search_by_key(&id.0, |x| x.0).ok().map(|i| self.ids[i].1 as usize).filter(|&r| r < self.lim)
    }
    /// A known library card's row (v5 layout of `library_known`).
    fn lib_row(&mut self, p: PlayerId, j: usize) -> Option<usize> {
        let me = self.me();
        let pl = self.obs.players.iter().find(|v| v.id == p)?;
        let (pos, card) = pl.library_known[j];
        let Some(r) = self.row(Some(card)) else {
            self.cut.push(Cut::Lib(p, j));
            return None;
        };
        if let Some(id) = pl.library_known_ids.get(j) {
            self.ids.push((id.0, r as u16));
        }
        self.set(r, ef::KIND_OBJECT, 1.0);
        self.set(r, ef::ZONE + 5, 1.0);
        self.set(r, ef::MINE, (p == me) as u8 as f32);
        self.set(r, ef::OWNED, (p == me) as u8 as f32);
        self.set(r, ef::LIB_POS, pos as f32 / 10.0);
        self.set(r, ef::ZONE_INDEX, pos as f32 / 20.0);
        Some(r)
    }
    fn player_row(&self, p: PlayerId) -> usize {
        (p != self.me()) as usize
    }
    fn target_row(&self, t: &Target) -> Option<usize> {
        match *t {
            Target::Obj(o) => self.lookup(o),
            Target::Player(p) => Some(self.player_row(p)),
            // Encoding v9 and before: an ability target points at its source's row, as when
            // abilities were targeted by their source (`ext8`; bit-identical inputs).
            Target::Ability { src, .. } => self.lookup(src),
        }
    }
}

/// Fill `out` from `obs` (the whole slot is overwritten). Pure: depends only on `obs`.
pub fn encode_into(obs: &Observation, out: Out<'_>) -> Overflow {
    encode_impl(obs, out, None, Extent::FULL).0
}

/// `encode_into`, plus the object id of every row that is a creature on the battlefield
/// (`ent_obj[row]`, -1 elsewhere). For auxiliary *labels* only (`aux.rs`): object ids are
/// never inputs.
pub fn encode_into_ids(obs: &Observation, out: Out<'_>, ent_obj: &mut [i32]) -> Overflow {
    encode_impl(obs, out, Some(ent_obj), Extent::FULL).0
}

/// The `entity` / `option` rows of a slot's buffers an encoding may have left non-zero: the rows
/// it allocated (`entity_rows`: rows are allocated in order, so a prefix) and the options it wrote
/// (`option_rows`). Every other cell of `entity` and `option` is 0.0 after an encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent {
    pub entity_rows: usize,
    pub option_rows: usize,
}

impl Extent {
    /// Nothing known about the buffers: clear them all (what `encode_into` does).
    pub const FULL: Extent = Extent { entity_rows: E, option_rows: O };
}

/// `encode_into` for reused buffers. `dirty` = the `Extent` the
/// previous encoding into these same buffers returned (or `Extent::FULL`): every `entity` row at
/// or past `dirty.entity_rows` and every `option` row at or past `dirty.option_rows` already holds
/// 0.0, so only the rows below are cleared (an `entity` slot is E*F = 29k floats, a decision uses
/// ~50 of its 192 rows). The buffers end byte for byte as `encode_into` leaves them. Returns the
/// new extent for the next call on the same buffers. The precondition is the caller's: after
/// anything else wrote into the buffers, pass `Extent::FULL`.
pub fn encode_into_from(obs: &Observation, out: Out<'_>, dirty: Extent) -> (Overflow, Extent) {
    encode_impl(obs, out, None, dirty)
}

/// `encode_into_ids` on reused buffers (see `encode_into_from`).
pub fn encode_into_ids_from(obs: &Observation, out: Out<'_>, ent_obj: &mut [i32], dirty: Extent) -> (Overflow, Extent) {
    encode_impl(obs, out, Some(ent_obj), dirty)
}

fn encode_impl(obs: &Observation, out: Out<'_>, ent_obj: Option<&mut [i32]>, dirty: Extent) -> (Overflow, Extent) {
    let (de, dop) = (dirty.entity_rows.min(E), dirty.option_rows.min(O));
    debug_assert!(
        out.entity[de * F..].iter().all(|x| x.to_bits() == 0) && out.option[dop * K..].iter().all(|x| x.to_bits() == 0),
        "encode_into_from: rows past the dirty extent are not zero"
    );
    out.entity[..de * F].fill(0.0);
    out.entity_card.fill(-1);
    out.entity_mask.fill(false);
    out.global.fill(0.0);
    out.option[..dop * K].fill(0.0);
    out.option_mask.fill(false);
    out.option_refs.fill(-1);
    out.option_ability.fill(-1);

    let me = obs.me;
    let pv = |p: PlayerId| obs.players.iter().find(|v| v.id == p);
    let mut b = Builder { out, obs, n: 0, dropped: 0, ids: Vec::with_capacity(E), cap: E_V9, lim: E, cut: vec![] };

    // Player tokens: row 0 = observer, row 1 = opponent.
    for p in [me, me.opp()] {
        let r = b.row(None).unwrap();
        b.set(r, ef::KIND_PLAYER, 1.0);
        b.set(r, ef::MINE, (p == me) as u8 as f32);
        b.set(r, ef::OWNED, (p == me) as u8 as f32);
        b.set(r, ef::P_ACTIVE, (p == obs.active) as u8 as f32);
        if let Some(v) = pv(p) {
            b.set(r, ef::P_LIFE, v.life as f32 / 20.0);
            b.set(r, ef::P_POISON, v.poison as f32 / 10.0);
            b.set(r, ef::P_HAND, v.hand.len() as f32 / 7.0);
            b.set(r, ef::P_LIBRARY, v.library_size as f32 / 40.0);
            b.set(r, ef::P_GRAVEYARD, v.graveyard.len() as f32 / 20.0);
            b.set(r, ef::P_LANDS_PLAYED, v.lands_played as f32);
            b.set(r, ef::P_MULLIGANS, v.mulligans as f32);
            for (i, &n) in v.pool.iter().enumerate() {
                b.set(r, ef::P_POOL + i, n as f32 / 5.0);
            }
            for (i, &n) in v.pool_restricted.iter().enumerate() {
                b.set(r, ef::P_RPOOL + i, n as f32 / 5.0);
            }
            // v10.1: life lost / gained this turn
            b.set(r, ef::P_LIFE_LOST, v.life_lost_this_turn as f32 / 10.0);
            b.set(r, ef::P_LIFE_GAINED, v.life_gained_this_turn as f32 / 10.0);
        }
    }

    // Stack items, top first.
    let mut stack_rows: Vec<(usize, usize)> = vec![]; // (stack index, row)
    for (depth, (i, it)) in obs.stack.iter().enumerate().rev().enumerate() {
        let Some(r) = b.row(Some(it.card)) else { continue };
        stack_rows.push((i, r));
        b.set(r, ef::KIND_STACK, 1.0);
        b.set(r, ef::ZONE + 4, 1.0);
        b.set(r, ef::MINE, (it.controller == me) as u8 as f32);
        // A spell's owner (CR 112.2; `StackView::owner`; not its controller, which differs for a
        // spell cast from another player's card); an ability has no owner: its controller.
        b.set(r, ef::OWNED, (it.owner.unwrap_or(it.controller) == me) as u8 as f32);
        #[allow(unreachable_patterns)]
        let k = match it.kind {
            StackKind::Spell => 0,
            StackKind::Activated => 1,
            StackKind::Triggered => 2,
            _ => 3,
        };
        if k < 3 {
            b.set(r, ef::STACK_KIND + k, 1.0);
        }
        b.set(r, ef::STACK_DEPTH, depth as f32 / 10.0);
        b.set(r, ef::STACK_X, it.x as f32 / 10.0);
        b.set(r, ef::STACK_NTARGETS, it.targets.len() as f32 / 4.0);
        b.set(r, ef::ZONE_INDEX, depth as f32 / 20.0);
        b.set(r, ef::ALT_FACE, (it.face != 0) as u8 as f32);
        b.set(r, ef::UNCOUNTERABLE, it.uncounterable as u8 as f32);
        if let Some(i) = it.ability {
            b.set(r, ef::STACK_ABILITY, (i as f32 + 1.0) / 4.0);
        }
        // v10.2: chosen modes and an optional additional cost paid (one embedded id), colours of
        // mana spent to cast it (another)
        let mut sf = it.paid_optional as u8 * stackf::PAID_OPTIONAL;
        for &m in &it.modes {
            sf |= 1 << (m as usize).min(stackf::MODES - 1);
        }
        b.set(r, ef::STACK_FLAGS, sf as f32);
        b.set(r, ef::SPENT_COLORS, (it.spent_colors & 0b11111) as f32);
        if it.kind == StackKind::Spell {
            write_colors(&mut b.out.entity[r * F..(r + 1) * F], it.colors, &it.pips, 0);
        }
    }

    // Visible objects, in order of usefulness (truncation drops from the end).
    for (i, v) in obs.battlefield.iter().enumerate() {
        b.obj(v, 1, i);
    }
    for p in [me, me.opp()] {
        if let Some(pl) = pv(p) {
            for (i, v) in pl.hand.iter().enumerate() {
                if let Some(r) = b.obj(v, 0, i) {
                    b.set(r, ef::HAND_AGE, obs.turn.saturating_sub(v.entered) as f32 / 5.0);
                }
            }
        }
    }
    for p in [me, me.opp()] {
        if let Some(pl) = pv(p) {
            for (i, v) in pl.graveyard.iter().enumerate() {
                b.obj(v, 2, i);
            }
        }
    }
    // Face-down exiled cards the observer can't look at (CR 406.3) have no characteristics and
    // can be dozens (Doomsday Excruciator): no rows for them, so they can't crowd out the
    // entity budget `E` (their number isn't encoded).
    for (i, v) in obs.exile.iter().filter(|v| v.card.is_some()).enumerate() {
        b.obj(v, 3, i);
    }
    // Spells being cast (decisions while casting refer to them).
    for (i, v) in obs.casting.iter().enumerate() {
        b.obj(v, 4, i);
    }
    for p in [me, me.opp()] {
        if let Some(pl) = pv(p) {
            for j in 0..pl.library_known.len() {
                b.lib_row(p, j);
            }
        }
    }
    // Cards outside the game (CR 400.11a) get rows only while my decision names them (a wish
    // choosing among them): object rows with no zone bit set (v5).
    if let Some(d) = obs.decision.as_ref().filter(|d| d.player == me) {
        let named: Vec<u32> = d.options.iter().take(O).flat_map(|o| opt_objs(o).into_iter().flatten()).map(|x| x.0).collect();
        for p in [me, me.opp()] {
            if let Some(pl) = pv(p) {
                for (i, v) in pl.sideboard.iter().enumerate().filter(|(_, v)| named.contains(&v.id.0)) {
                    b.obj(v, 0, i);
                    if let Some(r) = b.ids.iter().rev().find(|x| x.0 == v.id.0).map(|x| x.1 as usize) {
                        b.set(r, ef::ZONE, 0.0);
                    }
                }
            }
        }
    }
    // Spells on the stack are objects too: their ids resolve to the stack token (abilities'
    // sources are the permanents, already mapped).
    for &(i, r) in &stack_rows {
        let it = &obs.stack[i];
        if it.kind == StackKind::Spell && !b.ids.iter().any(|x| x.0 == it.source.0) {
            b.ids.push((it.source.0, r as u16));
        }
    }
    // An ability whose source is in no zone of the observation (it left; the ability exists
    // independently of it, CR 113.7a): the source's id resolves to the ability's stack row (the
    // topmost such), so a decision made as it resolves (its subject) points at it.
    {
        let in_zone = |id: ObjId| {
            obs.battlefield.iter().chain(&obs.exile).chain(&obs.casting).chain(obs.entering.iter().map(|e| &e.view)).any(|v| v.id == id)
                || obs.players.iter().any(|pl| pl.hand.iter().chain(&pl.graveyard).chain(&pl.sideboard).any(|v| v.id == id) || pl.library_known_ids.contains(&id))
        };
        for &(i, r) in &stack_rows {
            let it = &obs.stack[i];
            if it.kind != StackKind::Spell && !b.ids.iter().any(|x| x.0 == it.source.0) && !in_zone(it.source) {
                b.ids.push((it.source.0, r as u16));
            }
        }
    }
    // A permanent entering the battlefield while its "as enters" choices are made (CR 614.12):
    // the resolving permanent spell it is resolves to that spell's stack row (608.3a: still on
    // the stack); any other (a land being played, a card put onto the battlefield by an effect)
    // gets an object row as the permanent it will be (zone battlefield, after the battlefield's
    // rows). Objects the decision names that left the battlefield: their last known
    // information (608.2h), an object row without a zone. Both rows come where the decision's
    // UNOBSERVED tokens for them were.
    let mut ent_rows = 0;
    for e in &obs.entering {
        let spell_row = e.spell.and_then(|so| stack_rows.iter().find(|&&(si, _)| obs.stack[si].source == so && obs.stack[si].resolving)).map(|&(_, r)| r);
        match spell_row {
            Some(r) => {
                if !b.ids.iter().any(|x| x.0 == e.view.id.0) {
                    b.ids.push((e.view.id.0, r as u16));
                }
            }
            None => {
                b.obj(&e.view, 1, obs.battlefield.len() + ent_rows);
                ent_rows += 1;
            }
        }
    }
    for (i, v) in obs.last_known.iter().enumerate() {
        if let Some(r) = b.obj(v, 0, i) {
            b.set(r, ef::ZONE, 0.0);
        }
    }
    b.ids.sort_unstable();
    b.ids.dedup_by_key(|x| x.0);

    // Objects the decision names that the observation doesn't contain get their own
    // (card-less) token, so options stay distinguishable and pointable.
    let decision = obs.decision.as_ref().filter(|d| d.player == me);
    if let Some(d) = decision {
        let mut extra: Vec<u32> = vec![];
        let mut want = |id: ObjId| extra.push(id.0);
        d.subject.map(&mut want);
        for o in d.options.iter().take(O) {
            for id in opt_objs(o).into_iter().flatten() {
                want(id);
            }
        }
        let mut added = false;
        for id in extra {
            if b.lookup(ObjId(id)).is_some() || b.ids.iter().any(|x| x.0 == id) {
                continue;
            }
            let Some(r) = b.row(None) else { continue };
            b.ids.push((id, r as u16));
            b.set(r, ef::KIND_OBJECT, 1.0);
            b.set(r, ef::UNOBSERVED, 1.0);
            added = true;
        }
        if added {
            b.ids.sort_unstable();
        }
    }

    // v7 choice tokens: one row per `Subtype`/`Name` option (its identity); after every object
    // row, so they never displace one (truncation drops from the end).
    let core_dropped = b.dropped;
    let mut choice_rows: Vec<(usize, usize)> = vec![]; // (option index, row)
    let mut choice_cut: Vec<usize> = vec![]; // indices into `choice_info` the v9 layout had no row for
    let choice_row = |b: &mut Builder, c: &crate::observe::ChoiceView| -> Option<usize> {
        let r = b.row(c.card)?;
        b.set(r, ef::KIND_CHOICE, 1.0);
        b.set(r, ef::CHOICE_N_MINE, c.mine as f32 / 4.0);
        b.set(r, ef::CHOICE_N_OPP, c.opp as f32 / 4.0);
        if let Some(st) = c.subtype {
            b.subtype(r, subtype_key(st));
        }
        Some(r)
    };
    if decision.is_some() {
        for (ci, c) in obs.choice_info.iter().enumerate() {
            match choice_row(&mut b, c) {
                Some(r) => choice_rows.push((c.option as usize, r)),
                None => choice_cut.push(ci),
            }
        }
    }
    // v7 history tokens: recent public actions, the opponent's first (more useful), newest first.
    for (p, limit) in [(me.opp(), HIST_OPP), (me, HIST_MINE)] {
        for (k, a) in obs.hist.recent(p).take(limit).enumerate() {
            let ago = obs.turn.saturating_sub(a.turn);
            if a.kind == crate::hist::act::NONE || ago > HIST_MAX_AGE {
                break;
            }
            let Some(r) = b.row(Some(CardId(a.card))) else { break };
            b.set(r, ef::KIND_HIST, 1.0);
            b.set(r, ef::MINE, (p == me) as u8 as f32);
            b.set(r, ef::OWNED, (p == me) as u8 as f32);
            b.set(r, ef::H_ACT + (a.kind as usize - 1).min(2), 1.0);
            b.set(r, ef::H_TURNS_AGO, ago as f32 / 4.0);
            b.set(r, ef::H_OFF_TURN, a.off_turn as u8 as f32);
            b.set(r, ef::H_RECENCY, k as f32 / 8.0);
        }
    }
    // v8 tokens, after every v7 row (so a v7 view of the rows is exactly v7's, truncation
    // included): pending triggers of my `OrderTriggers` decision, then effect objects.
    let mut trigger_rows: Vec<Option<usize>> = vec![];
    let trigger_row = |b: &mut Builder, t: &crate::observe::TriggerView| -> Option<usize> {
        let r = b.row(Some(t.card))?;
        b.set(r, ef::KIND_TRIGGER, 1.0);
        b.set(r, ef::MINE, 1.0);
        b.set(r, ef::OWNED, 1.0);
        b.set(r, ef::TRIG_ABILITY, t.trigger.map_or(0.0, |i| (i as f32 + 1.0) / 4.0));
        Some(r)
    };
    if decision.is_some() {
        for t in obs.ordering.iter().take(O) {
            let r = trigger_row(&mut b, t);
            trigger_rows.push(r);
        }
    }
    for e in obs.effects.iter().rev().take(EFFECT_TOKENS) {
        let Some(r) = b.row(e.card) else { break };
        b.set(r, ef::KIND_EFFECT, 1.0);
        b.set(r, ef::MINE, (e.controller == me) as u8 as f32);
        b.set(r, ef::OWNED, (e.controller == me) as u8 as f32);
        use crate::carddb::EffDuration as D;
        #[allow(unreachable_patterns)]
        let k = match e.duration {
            D::EndOfTurn => 0,
            D::Permanent => 1,
            D::UntilYourNextTurn | D::UntilTheEndOfYourNextTurn | D::UntilYourNextEndStep => 2,
            _ => 3,
        };
        b.set(r, ef::EFF_DURATION + k, 1.0);
        b.set(r, ef::EFF_STATICS, e.statics as f32 / 2.0);
        b.set(r, ef::EFF_TRIGGERS, e.triggers as f32 / 2.0);
        b.set(r, ef::EFF_AFFECTED, e.affected.len() as f32 / 4.0);
    }

    // v10 reserved rows (`E_V9..E`): what my decision's options (or its subject) name and the v9
    // layout cut: the object (full row), or an unobserved token, then cut choice / trigger tokens.
    // Rows `0..E_V9` stay exactly the v9 layout.
    // Only when the v9 layout hit its cap (`b.dropped > 0`, so `b.n == E_V9`): nothing else
    // changes, and every reserved row is at E_V9 or above.
    let mut reserved = false;
    if let Some(d) = decision.filter(|_| b.dropped > 0) {
        debug_assert_eq!(b.n, E_V9);
        // (object, would the v9 layout give it an UNOBSERVED token if rows were left)
        let mut named: Vec<(ObjId, bool)> = vec![];
        d.subject.map(|s| named.push((s, true)));
        for o in d.options.iter().take(O) {
            named.extend(opt_objs(o).into_iter().flatten().map(|x| (x, true)));
            if let Opt::Trigger(k) = *o {
                if let Some(t) = obs.ordering.get(k as usize) {
                    named.push((t.source, false));
                }
            }
        }
        let missing: Vec<(ObjId, bool)> = {
            let mut seen: Vec<u32> = vec![];
            named.into_iter().filter(|x| b.lookup(x.0).is_none() && !seen.contains(&x.0 .0) && { seen.push(x.0 .0); true }).collect()
        };
        let tokens_cut = !choice_cut.is_empty() || trigger_rows.iter().any(|r| r.is_none());
        if !missing.is_empty() || tokens_cut {
            reserved = true;
            b.cap = E;
            let cut = std::mem::take(&mut b.cut);
            for (id, token) in missing {
                let hit = cut.iter().find(|c| match c {
                    Cut::Obj(v, ..) => v.id == id,
                    Cut::Lib(p, j) => pv(*p).and_then(|pl| pl.library_known_ids.get(*j)).is_some_and(|x| *x == id),
                });
                let r = match hit {
                    Some(&Cut::Obj(v, zone, idx)) => {
                        let r = b.obj(v, zone, idx);
                        if let (Some(r), 0) = (r, zone) {
                            let side = obs.players.iter().any(|pl| pl.sideboard.iter().any(|x| x.id == v.id)) || obs.last_known.iter().any(|x| x.id == v.id);
                            if side {
                                b.set(r, ef::ZONE, 0.0);
                            } else {
                                b.set(r, ef::HAND_AGE, obs.turn.saturating_sub(v.entered) as f32 / 5.0);
                            }
                        }
                        r
                    }
                    Some(&Cut::Lib(p, j)) => b.lib_row(p, j),
                    None if !token => None,
                    None => b.row(None).inspect(|&r| {
                        b.ids.push((id.0, r as u16));
                        b.set(r, ef::KIND_OBJECT, 1.0);
                        b.set(r, ef::UNOBSERVED, 1.0);
                    }),
                };
                if r.is_some() {
                    b.ids.sort_unstable();
                }
            }
            for ci in choice_cut {
                let c = &obs.choice_info[ci];
                if let Some(r) = choice_row(&mut b, c) {
                    choice_rows.push((c.option as usize, r));
                }
            }
            for (k, t) in obs.ordering.iter().take(O).enumerate() {
                if trigger_rows[k].is_none() {
                    trigger_rows[k] = trigger_row(&mut b, t);
                }
            }
            b.cap = E_V9;
        }
    }

    // v10.2 delayed-trigger tokens (`Observation::delayed`): after every row the v9 layout and the
    // reserved rows placed (those stay byte-identical), at most DELAYED_TOKENS, dropped (counted in
    // `entities_dropped`) when E runs out. A KIND_EFFECT row with DELAY_ID > 0; every consumer without
    // v10.2 ignores it, so it computes what it did on v10.1.
    // Existing effect-token semantics where they apply: MINE / OWNED, EFF_TRIGGERS (one triggered
    // ability), EFF_AFFECTED, EFF_DURATION "end of turn" for "when it dies this turn" (the others last
    // until they trigger: no duration slot).
    b.cap = E;
    let shown = &obs.delayed[..obs.delayed.len().min(DELAYED_TOKENS)];
    for (k, dl) in shown.iter().enumerate() {
        let Some(r) = b.row(dl.card) else {
            b.dropped += shown.len() - k - 1; // the rest (`row` counted this one)
            break;
        };
        b.set(r, ef::KIND_EFFECT, 1.0);
        b.set(r, ef::MINE, (dl.controller == me) as u8 as f32);
        b.set(r, ef::OWNED, (dl.controller == me) as u8 as f32);
        if matches!(dl.when, crate::observe::DelayedWhenView::DiesThisTurnOnly { .. }) {
            b.set(r, ef::EFF_DURATION, 1.0);
        }
        b.set(r, ef::EFF_TRIGGERS, 0.5);
        b.set(r, ef::EFF_AFFECTED, dl.affected.len() as f32 / 4.0);
        if let Some(t) = dl.affected.iter().find_map(|&o| b.lookup(o)) {
            b.set(r, ef::DELAY_ROW, (t + 1) as f32);
        }
        b.set(r, ef::DELAY_ID, delay::id(&dl.when, me) as f32);
    }
    b.cap = E_V9;

    // Relations: blocked attackers, targets of stack items, decision subject.
    for v in &obs.battlefield {
        // BLOCKED: the attacker's own `blocked` (CR 509.1h: it stays blocked once its blockers
        // are gone). A creature some creature blocks is blocked too (509.1h), which
        // also covers observations serialized before the field existed.
        if v.blocked {
            if let Some(r) = b.lookup(v.id) {
                b.set(r, ef::BLOCKED, 1.0);
            }
        }
        if let Some(a) = v.blocking {
            if let Some(r) = b.lookup(a) {
                b.set(r, ef::BLOCKED, 1.0);
            }
        }
        if let Some(r) = v.attached_to.and_then(|t| b.lookup(t)) {
            b.out.entity[r * F + ef::N_ATTACHMENTS] += 0.25;
            // v10: the Equipment / Aura points at its host's row
            if let Some(a) = b.lookup(v.id) {
                b.set(a, ef::HOST_ROW, (r + 1) as f32);
            }
        }
        if let Some(r) = v.attacking_pw.and_then(|t| b.lookup(t)) {
            b.out.entity[r * F + ef::ATTACKED_BY] += 0.25;
        }
    }
    // v10.2 row pointers, here because every row (v9 layout and reserved) is placed: the final id ->
    // row map, so a target without a row is 0 ("none"), never a stale or shifted row.
    for v in &obs.battlefield {
        assert!(v.blocking.is_none() || v.attacking_pw.is_none(), "a creature both blocking and attacking a planeswalker");
        if let (Some(a), Some(t)) = (b.lookup(v.id), v.blocking.or(v.attacking_pw).and_then(|t| b.lookup(t))) {
            b.set(a, ef::PAIR_ROW, (t + 1) as f32);
        }
    }
    for v in &obs.exile {
        if let (Some(a), Some(t)) = (b.lookup(v.id), v.exiled_with.and_then(|t| b.lookup(t))) {
            b.set(a, ef::EXILED_WITH_ROW, (t + 1) as f32);
        }
    }
    for &(i, _) in &stack_rows {
        let it = &obs.stack[i];
        let col = if it.controller == me { ef::TARGETED_BY_ME } else { ef::TARGETED_BY_OPP };
        for t in &it.targets {
            if let Some(r) = b.target_row(t) {
                b.out.entity[r * F + col] += 1.0;
            }
        }
    }
    if let Some(s) = decision.and_then(|d| d.subject) {
        if let Some(r) = b.lookup(s) {
            b.set(r, ef::SUBJECT, 1.0);
        }
    }
    // v8 relations: objects effects apply to; what my choice in progress already picked.
    for e in &obs.effects {
        let col = if e.controller == me { ef::AFFECTED_BY_MINE } else { ef::AFFECTED_BY_OPP };
        for &o in &e.affected {
            if let Some(r) = b.lookup(o) {
                b.out.entity[r * F + col] += 0.5;
            }
        }
    }
    if decision.is_some() {
        for t in &obs.pick.chosen {
            if let Some(r) = b.target_row(t) {
                b.set(r, ef::PICKED, 1.0);
            }
        }
        for t in &obs.pick.earlier {
            if let Some(r) = b.target_row(t) {
                b.out.entity[r * F + ef::TGT_EARLIER] += 1.0;
            }
        }
    }

    // Options.
    let mut options_dropped = 0;
    if let Some(d) = decision {
        let ks = kind_slot(d.kind);
        options_dropped = d.options.len().saturating_sub(O);
        let enc_opt = |b: &Builder, i: usize, o: &Opt| -> ([f32; K], [Option<usize>; R], Option<crate::carddb::AbilityId>) {
            let below = |r: &usize| *r < b.lim;
            let mut row = [0f32; K];
            row[of::TYPE + opt_slot(o)] = 1.0;
            row[of::KIND + ks] = 1.0;
            let mut refs: [Option<usize>; R] = [None; R];
            let mut ability = None;
            #[allow(unreachable_patterns)]
            match *o {
                Opt::Card(x) | Opt::PlayLand(x) | Opt::Cast(x) | Opt::CastPrepared(x) => refs[0] = b.lookup(x),
                Opt::Activate(x, a) => {
                    refs[0] = b.lookup(x);
                    ability = Some(a);
                }
                Opt::Target(t) => {
                    refs[0] = b.target_row(&t);
                    if let Target::Player(p) = t {
                        row[if p == me { of::PLAYER_ME } else { of::PLAYER_OPP }] = 1.0;
                    }
                }
                Opt::Mana(x, a, t) => {
                    refs[0] = b.lookup(x);
                    ability = Some(a);
                    row[of::MANA + (t as usize).min(5)] = 1.0;
                }
                Opt::Attack(x, p) => {
                    refs[0] = b.lookup(x);
                    refs[1] = Some(b.player_row(p));
                    row[if p == me { of::PLAYER_ME } else { of::PLAYER_OPP }] = 1.0;
                }
                Opt::Block(x, a) => {
                    refs[0] = b.lookup(x);
                    refs[1] = b.lookup(a);
                }
                Opt::AttackPw(x, pw) => {
                    refs[0] = b.lookup(x);
                    refs[1] = b.lookup(pw);
                }
                Opt::Block2(x, y, a) => {
                    refs[0] = b.lookup(x);
                    refs[1] = b.lookup(y);
                    refs[2] = b.lookup(a);
                }
                Opt::PoolMana(t) => row[of::MANA + (t as usize).min(5)] = 1.0,
                Opt::Number(n) => row[of::NUMBER] = n as f32 / 10.0,
                Opt::Trigger(k) | Opt::Mode(k) | Opt::CostOption(k) => row[of::INDEX] = k as f32 / 8.0,
                Opt::Alt(x, m) => {
                    refs[0] = b.lookup(x);
                    row[of::INDEX] = m as f32 / 8.0;
                }
                _ => {}
            }
            // N_REFS counts game objects only (a choice token is not one), and only the refs
            // that existed in v7 (so pre-v8 nets see the same value).
            row[of::N_REFS] = refs.iter().filter(|r| r.is_some()).count() as f32 / R as f32;
            // v8: options that name a card without a ref before: casting an Adventure/Omen face
            // or with flashback (the card); a mode or additional-cost choice (the decision's
            // subject: the spell or ability those modes/costs belong to).
            match *o {
                Opt::CastFace(x, _) | Opt::Flashback(x) => refs[0] = b.lookup(x),
                Opt::Mode(_) | Opt::CostOption(_) => refs[0] = d.subject.and_then(|s| b.lookup(s)),
                // The trigger being ordered (CR 603.3b): its token, and its source if visible.
                Opt::Trigger(k) => {
                    let src = obs.ordering.get(k as usize).and_then(|t| b.lookup(t.source));
                    refs[0] = trigger_rows.get(k as usize).copied().flatten().filter(below).or(src);
                    refs[1] = src.filter(|&s| Some(s) != refs[0]);
                }
                _ => {}
            }
            if let Some(&(_, r)) = choice_rows.iter().find(|x| x.0 == i && below(&x.1)) {
                refs[0] = Some(r);
            }
            // v10: which counter a "remove a counter from [this]" option removes;
            // a target that is an ability on the stack points at its stack row too (refs[1]).
            match *o {
                Opt::Mode(m) if d.kind == DecisionKind::PayCost => {
                    if let Some((id, n)) = d.subject.and_then(|s| obs.battlefield.iter().find(|v| v.id == s)).and_then(|v| remove_counter_kind(v, m as usize)) {
                        row[of::CTR_KIND] = id as f32;
                        row[of::CTR_COUNT] = n as f32 / 5.0;
                    }
                }
                Opt::Target(Target::Ability { uid, .. }) => {
                    row[of::TGT_ABILITY] = 1.0;
                    refs[1] = stack_rows.iter().find(|&&(si, _)| obs.stack[si].uid == uid).map(|&(_, r)| r).filter(below);
                }
                _ => {}
            }
            // v10.1: which of the "other"-slot kinds the decision is
            if let Some(j) = kind_ref(d.kind) {
                row[of::KIND_REF] = (j + 1) as f32;
            }
            // v10.2: what a PayUnless payment costs
            if let Some(u) = obs.pay_unless.filter(|_| d.kind == DecisionKind::PayUnless) {
                row[of::UNLESS_AMOUNT] = if u.mana > 0 { u.mana as f32 / 5.0 } else { -(u.life.max(0) as f32) / 5.0 };
            }
            if let Some(t) = obs.option_targets.iter().find(|t| t.option as usize == i) {
                row[of::TGT_MINE] = t.mine as f32 / 4.0;
                row[of::TGT_OPP] = t.opp as f32 / 4.0;
                row[of::TARGETS] = 1.0;
                row[of::TGT_OPP_WARD] = t.opp_ward as f32 / 4.0;
                if matches!(o, Opt::Alt(..) | Opt::CastPrepared(_)) {
                    row[of::ALT_TGT_FILL] = 1.0;
                }
            }
            if let Some((src, ab, mana)) = ability_of(o) {
                // Distinct abilities of the same source among this decision's options (v6).
                let mut same: Vec<u32> = d.options.iter().filter_map(ability_of).filter(|x| x.0 == src && x.2 == mana).map(|x| x.1).collect();
                same.sort_unstable();
                same.dedup();
                if same.len() > 1 {
                    row[of::ABILITY_RANK] = same.iter().position(|&x| x == ab).unwrap_or(0) as f32 / 4.0;
                    row[of::ABILITY_COUNT] = same.len() as f32 / 4.0;
                }
            }
            (row, refs, ability)
        };
        for (i, o) in d.options.iter().take(O).enumerate() {
            b.out.option_mask[i] = true;
            b.lim = E_V9;
            let (row, mut refs, ability) = enc_opt(&b, i, o);
            b.lim = E;
            if reserved {
                // a v9 encoding's refs, then this option's reserved rows in its free ref slots
                let (_, all, _) = enc_opt(&b, i, o);
                for r in all.into_iter().flatten().filter(|&r| r >= E_V9) {
                    if !refs.contains(&Some(r)) {
                        if let Some(slot) = refs.iter_mut().find(|x| x.is_none()) {
                            *slot = Some(r);
                        }
                    }
                }
            }
            b.out.option[i * K..(i + 1) * K].copy_from_slice(&row);
            for (j, r) in refs.iter().enumerate() {
                b.out.option_refs[i * R + j] = r.map_or(-1, |r| r as i32);
            }
            b.out.option_ability[i] = ability.map_or(-1, |a| a.0 as i32);
        }
    }

    // Globals.
    let n_perm = |p: PlayerId| obs.battlefield.iter().filter(|v| v.controller == p).count() as f32 / 20.0;
    let g = &mut *b.out.global;
    for (j, p) in [me, me.opp()].into_iter().enumerate() {
        if let Some(v) = pv(p) {
            g[gf::LIFE + j] = v.life as f32 / 20.0;
            g[gf::POISON + j] = v.poison as f32 / 10.0;
            g[gf::HAND + j] = v.hand.len() as f32 / 7.0;
            g[gf::LIBRARY + j] = v.library_size as f32 / 40.0;
            g[gf::GRAVEYARD + j] = v.graveyard.len() as f32 / 20.0;
            g[gf::LANDS_PLAYED + j] = v.lands_played as f32;
            g[gf::MULLIGANS + j] = v.mulligans as f32;
            g[gf::SPEED + j] = v.speed as f32 / 4.0;
            g[gf::ENDURING_STORY + j] = v.enduring_story as u8 as f32;
        }
        g[gf::PERMANENTS + j] = n_perm(p);
    }
    g[gf::TURN] = obs.turn as f32 / 20.0;
    g[gf::ACTIVE] = (obs.active == me) as u8 as f32;
    g[gf::STACK] = obs.stack.len() as f32 / 5.0;
    g[gf::GAME_OVER] = obs.result.is_some() as u8 as f32;
    g[gf::STEP + step_slot(obs.step)] = 1.0;
    let st = stage_slot(obs.stage);
    if st < 4 {
        g[gf::STAGE + st] = 1.0;
    }
    if let Some(d) = decision {
        g[gf::HAS_DECISION] = 1.0;
        g[gf::N_OPTIONS] = d.options.len().min(O) as f32 / O as f32;
        g[gf::KIND + kind_slot(d.kind)] = 1.0;
    }
    if decision.is_some() && obs.pick.parts > 0 {
        g[gf::TGT_PART] = obs.pick.part as f32 / 4.0;
        g[gf::TGT_PARTS] = obs.pick.parts as f32 / 4.0;
    }
    // v9 own-deck block (`dk`).
    write_own_deck(&obs.own_deck, &mut g[gf::DECK..gf::DECK + DECK_NB]);
    // v10 globals: pool mana kept past this step, skipped turns, hidden face-down exile by owner.
    for (j, p) in [me, me.opp()].into_iter().enumerate() {
        if let Some(v) = pv(p) {
            for (t, &n) in v.pool_kept.iter().enumerate() {
                g[gf::POOL_KEPT + 6 * j + t] = n as f32 / 5.0;
            }
            g[gf::SKIP_TURNS + j] = v.skip_turns as f32 / 4.0;
        }
        g[gf::FACE_DOWN_EXILE + j] = obs.exile.iter().filter(|v| v.card.is_none() && v.owner == p).count() as f32 / 10.0;
    }
    // v10.2 globals: this turn's zone changes, the next spell can't be countered, player hexproof
    for (j, p) in [me, me.opp()].into_iter().enumerate() {
        if let Some(v) = pv(p) {
            g[gf::DIED + j] = v.died_this_turn as f32 / 4.0;
            g[gf::LEFT_BF + j] = v.left_this_turn as f32 / 4.0;
            g[gf::SACRIFICED + j] = v.sacrificed_this_turn as f32 / 4.0;
            g[gf::DESCENDED + j] = v.descended_this_turn as f32 / 4.0;
            g[gf::WARPED + j] = v.warped_this_turn as f32 / 2.0;
            g[gf::NEXT_UNCOUNTERABLE + j] = v.next_uncounterable as u8 as f32;
            g[gf::PLAYER_HEXPROOF + j] = v.hexproof as u8 as f32;
            g[gf::KNOWN_TOP + j] = v.owner_knows_top as f32 / 4.0;
            g[gf::KNOWN_BOTTOM + j] = v.owner_knows_bottom as f32 / 4.0;
        }
    }
    // Objects dropped (v7 choice/history rows not counted, so the flag means what it did before).
    g[gf::ENT_TRUNC] = (core_dropped > 0) as u8 as f32;
    g[gf::OPT_TRUNC] = (options_dropped > 0) as u8 as f32;
    // v7 public history per player and recent turn.
    for (j, p) in [me, me.opp()].into_iter().enumerate() {
        for k in 0..3u32 {
            let Some(t) = obs.turn.checked_sub(k) else { break };
            let Some(rec) = obs.hist.turn(t) else { continue };
            let pt = &rec.p[p.idx()];
            let base = gf::HIST + (j * 3 + k as usize) * gf::HIST_STATS;
            g[base + hs::SPELLS] = pt.spells as f32 / 3.0;
            g[base + hs::ACTIVATIONS] = pt.activations as f32 / 3.0;
            g[base + hs::DRAWS] = pt.draws as f32 / 3.0;
            g[base + hs::LANDS] = pt.lands as f32;
            g[base + hs::DISCARDS] = pt.discards as f32 / 3.0;
            g[base + hs::HAND_START] = pt.hand_start as f32 / 7.0;
            g[base + hs::UNTAPPED_END] = pt.untapped_end as f32 / 5.0;
            if k == 0 {
                g[gf::COMBAT_DMG + j] = pt.combat_damage as f32 / 10.0;
                g[gf::COMBAT_DEATHS + j] = pt.combat_deaths as f32 / 4.0;
            }
        }
    }
    if let Some(ids) = ent_obj {
        ids.fill(-1);
        let creature = ef::TYPES + crate::basics::CardType::Creature as usize;
        for &(id, r) in b.ids.iter().filter(|x| (x.1 as usize) < E_V9) {
            let r = r as usize;
            let e = &b.out.entity[r * F..(r + 1) * F];
            if e[ef::KIND_OBJECT] == 1.0 && e[ef::ZONE + 1] == 1.0 && e[creature] == 1.0 && e[ef::UNOBSERVED] == 0.0 {
                ids[r] = id as i32;
            }
        }
    }
    let option_rows = decision.map_or(0, |d| d.options.len().min(O));
    (Overflow { entities_dropped: b.dropped, options_dropped }, Extent { entity_rows: b.n, option_rows })
}

/// (stable kind id, count) of the counter `ext5::remove_any_counter`'s `Opt::Mode(m)` removes
/// from the permanent `v` (the m-th of `ext5::remove_any_kinds` over its counters).
fn remove_counter_kind(v: &ObjView, m: usize) -> Option<(u8, i32)> {
    if v.counter_ids.len() != v.counters.len() {
        return None;
    }
    v.counters.iter().zip(&v.counter_ids).filter(|((_, n), _)| *n > 0).nth(m).map(|((_, n), &id)| (id, *n))
}

/// v9 colour block of one entity row (`ef::COLOR`, `ef::PIPS`, `ef::MAKES`).
fn write_colors(row: &mut [f32], colors: u8, pips: &[u8; 6], makes: u8) {
    for i in 0..5 {
        row[ef::COLOR + i] = ((colors >> i) & 1) as f32;
        row[ef::PIPS + i] = pips[i] as f32 / 4.0;
    }
    row[ef::COLOR + 5] = (colors & 0x1f == 0) as u8 as f32;
    row[ef::PIPS + 5] = pips[5] as f32 / 5.0;
    for i in 0..6 {
        row[ef::MAKES + i] = ((makes >> i) & 1) as f32;
    }
}

/// Write `Observation::own_deck` into the own-deck block (`dk`, `DECK_NB` columns).
fn write_own_deck(d: &crate::observe::OwnDeckView, g: &mut [f32]) {
    if !d.known {
        return;
    }
    let st = &d.stats;
    g[dk::KNOWN] = 1.0;
    g[dk::CARDS] = st.cards as f32 / 40.0;
    g[dk::UNSEEN] = d.unseen as f32 / 4.0;
    g[dk::LIB_TRUNC] = (d.library.len() > DECK_LIB) as u8 as f32;
    g[dk::LANDS] = st.lands as f32 / 20.0;
    g[dk::CREATURES] = st.creatures as f32 / 20.0;
    g[dk::INSTANTS] = st.instants as f32 / 20.0;
    g[dk::SORCERIES] = st.sorceries as f32 / 20.0;
    g[dk::OTHER] = st.other as f32 / 20.0;
    for (i, &n) in st.mv.iter().enumerate() {
        g[dk::MV + i] = n as f32 / 10.0;
    }
    let nonland = st.cards.saturating_sub(st.lands);
    if nonland > 0 {
        g[dk::MV_MEAN] = st.mv_sum as f32 / nonland as f32 / 5.0;
    }
    for (i, &n) in st.colors.iter().enumerate() {
        g[dk::COLORS + i] = n as f32 / 20.0;
    }
    for (i, &n) in st.land_colors.iter().enumerate() {
        g[dk::LAND_COLORS + i] = n as f32 / 20.0;
    }
    g[dk::SIDE_CARDS] = d.sideboard.iter().map(|x| x.1 as u32).sum::<u32>() as f32 / 15.0;
    g[dk::SIDE_TRUNC] = (d.sideboard.len() > DECK_SIDE) as u8 as f32;
    let put = |g: &mut [f32], at: usize, list: &[(CardId, u8)], cap: usize| {
        // Over capacity: keep the cards with the most copies (ties: lower id), written in id order.
        let mut keep: Vec<(CardId, u8)> = list.to_vec();
        if keep.len() > cap {
            keep.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            keep.truncate(cap);
            keep.sort_unstable();
        }
        for (i, &(c, n)) in keep.iter().enumerate() {
            assert!(c.0 < 1 << 16, "own-deck block: card id {} doesn't fit two base-256 digits", c.0);
            g[at + 3 * i] = (c.0 >> 8) as f32;
            g[at + 3 * i + 1] = (c.0 & 0xff) as f32;
            g[at + 3 * i + 2] = n as f32;
        }
    };
    put(g, dk::LIB_IDS, &d.library, DECK_LIB);
    put(g, dk::SIDE_IDS, &d.sideboard, DECK_SIDE);
}

/// Decode the own-deck block's card lists (inverse of the id columns of `write_own_deck`, for
/// tests and tools): (library entries, sideboard entries), (card, copies) in id order.
pub fn decode_own_deck(g: &[f32]) -> (Vec<(CardId, u8)>, Vec<(CardId, u8)>) {
    let b = &g[gf::DECK..gf::DECK + DECK_NB];
    let read = |at: usize, cap: usize| -> Vec<(CardId, u8)> {
        (0..cap)
            .filter(|&i| b[at + 3 * i + 2] > 0.0)
            .map(|i| (CardId(b[at + 3 * i] as u32 * 256 + b[at + 3 * i + 1] as u32), b[at + 3 * i + 2] as u8))
            .collect()
    };
    (read(dk::LIB_IDS, DECK_LIB), read(dk::SIDE_IDS, DECK_SIDE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::observe;
    use crate::testutil::*;
    use crate::Game;

    #[test]
    fn layout_fits() {
        assert!(ef::UNOBSERVED < F);
        assert!(gf::KIND + KIND_SLOTS <= G);
        assert!(gf::STEP + STEP_SLOTS <= gf::STAGE);
        assert!(of::KIND + KIND_SLOTS <= K && of::N_REFS < K);
        assert!(ef::HARNESSED < ef::TYPES && ef::TYPES + ef::N_TYPES <= ef::KWS && ef::KWS + ef::N_KWS <= ef::MV && ef::DECLARING < F);
        assert!(crate::carddb::ALL_KW.len() == ef::N_KWS && of::ABILITY_COUNT < K);
        // v7
        assert!(ef::DECLARING < ef::KIND_CHOICE && ef::H_ACT + 3 == ef::H_TURNS_AGO && ef::SUBTYPE_LO < F);
        assert!(gf::ENDURING_STORY + 2 == gf::HIST && gf::HIST + 6 * gf::HIST_STATS == gf::COMBAT_DMG && gf::COMBAT_DEATHS + 2 <= G);
        // subtype ids fit the embedding (two base-32 digits) and stay below the "other" id
        assert!(SUBTYPE_VOCAB <= 32 * 32 && crate::carddb::cr_subtype_count() < SUBTYPE_VOCAB - 1);
        assert!(opt_slot(&Opt::Subtype(0)) == 25 && opt_slot(&Opt::Name(0)) == 26);
        assert!(of::ABILITY_COUNT < of::TGT_MINE && of::TARGETS < K);
        // v8
        assert!(opt_slot(&Opt::CastFace(ObjId(0), 1)) == 27 && opt_slot(&Opt::Flashback(ObjId(0))) == 28 && opt_slot(&Opt::CastPrepared(ObjId(0))) == 29);
        // v8 columns: appended after v7's (95 and globals 126-127 were spare), no overlap
        assert!(ef::SUBTYPE_LO + 1 == ef::UNCOUNTERABLE && ef::EFF_DURATION + 4 == ef::EFF_STATICS && ef::P_RPOOL + 6 <= F);
        assert!(gf::COMBAT_DEATHS + 2 == gf::TGT_PART && gf::TGT_PARTS < G && of::TARGETS + 1 == of::TGT_OPP_WARD && of::TGT_OPP_WARD < K);
        assert!(opt_slot(&Opt::Trigger(0)) == 14 && opt_slot(&Opt::Mode(0)) == 21 && opt_slot(&Opt::CostOption(0)) == 23);
        // v9: globals appended after v8's; 128..176 reserved (belief, Bo3 match), own-deck block last
        assert!(gf::TGT_PARTS + 1 == gf::V9 && gf::V9 + 48 == gf::DECK && gf::DECK + DECK_NB == gf::V10);
        assert!(dk::SIDE_TRUNC < dk::SUMMARY && dk::SUMMARY == dk::LIB_IDS && dk::IDS_END <= DECK_NB);
        assert!(ef::P_RPOOL + 6 + 5 == ef::COLOR && ef::COLOR + 6 == ef::PIPS && ef::PIPS + 6 == ef::MAKES && ef::MAKES + 6 == ef::V9_END && ef::V9_END <= F);
        // v10: entity / option columns in v9's / v8's spare columns, globals appended last
        assert!(ef::V9_END == ef::V10 && ef::CTR_SLOTS == ef::V10 && ef::CTR_SLOTS + ef::N_CTR_SLOTS == ef::HOST_ROW && ef::HOST_ROW + 1 == ef::V10_END && ef::V10_END <= F);
        assert!(of::TGT_OPP_WARD + 1 == of::V10 && of::CTR_KIND == of::V10 && of::CTR_COUNT + 1 == of::TGT_ABILITY && of::TGT_ABILITY + 1 == of::V10_END && of::V10_END <= K);
        assert!(gf::POOL_KEPT == gf::V10 && gf::POOL_KEPT + 12 == gf::SKIP_TURNS && gf::SKIP_TURNS + 2 == gf::FACE_DOWN_EXILE && gf::FACE_DOWN_EXILE + 2 == gf::V10_END && gf::V10_END == G);
        // packed counter slots stay exact in fp16 (< 2048), ids fit the embedding
        assert!(CTR_VOCAB * 32 <= 2048 && (CTR_OTHER as usize) < CTR_VOCAB);
        // v10.1: v10's spare entity / option columns, up to the end of each row (no shape change)
        assert!(ef::V10_END == ef::V101 && ef::EOT_POWER == ef::V101 && ef::STATUS_FLAGS + 1 == ef::V101_END && ef::V101_END == F);
        assert!((status::EOT_KW | status::CANT_ATTACK | status::CANT_BLOCK) as usize == status::VOCAB - 1);
        assert!(ef::P_LIFE_LOST == ef::V101 && ef::P_LIFE_GAINED + 1 == ef::V101_END);
        assert!(of::V10_END == of::V101 && of::KIND_REF == of::V101 && of::ALT_TGT_FILL == 95 && of::ALT_TGT_FILL + 1 == of::V101_END && of::V101_END == K);
        // v10.2 links: the released Bo3 columns after v8's restricted pool, before the colour block;
        // row pointers fit E, card ids are exact in fp16
        assert!(ef::P_RPOOL + 6 == ef::V102_LINKS && ef::PAIR_ROW == ef::V102_LINKS && ef::EXILED_WITH_ROW == 124 && ef::CARD2 == 125 && ef::V102_LINKS_END == 126 && ef::V102_LINKS_END < ef::COLOR);
        assert!(E < 2048 && CARD2_MAX <= 2048);
        // v10.2: the released reservations (entity 123–127, globals 128–175), option 85, more id bits
        assert!(ef::V102_LINKS_END == ef::SPENT_COLORS && ef::SPENT_COLORS < ef::COLOR && ef::STACK_FLAGS == ef::STATUS_FLAGS && SPENT_VOCAB == 32);
        assert!(gf::V102 == gf::V9 && gf::WARPED + 2 == gf::NEXT_UNCOUNTERABLE && gf::PLAYER_HEXPROOF + 2 == gf::KNOWN_TOP && gf::KNOWN_BOTTOM + 2 == gf::V102_END && gf::V102_END <= 150);
        assert!(of::UNLESS_AMOUNT == of::KIND_REF + 1 && of::UNLESS_AMOUNT < of::ALT_TGT_FILL);
        assert!((status::DIES_EXILE as usize) * 2 == status::VOCAB2 && (status::TRIGGER_USED as usize) == status::VOCAB);
        assert!(stackf::PAID_OPTIONAL as usize == 1 << stackf::MODES && stackf::VOCAB == 2 * stackf::PAID_OPTIONAL as usize);
    }

    /// Own-deck block round trip: the card lists decode to exactly `Observation::own_deck`
    /// (below capacity), the summary columns match its stats, and the block is written for
    /// both players at every decision of pool games.
    #[test]
    fn own_deck_block_round_trip() {
        if !crate::testutil::cardsfolder().is_dir() {
            return;
        }
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../decks/pool/2026-10-31");
        let (db, decks) = pool_decks(&dir);
        let mut rng = Rng::new(41);
        let mut checks = 0;
        for seed in 0..12u64 {
            let a = decks[rng.below(decks.len() as u32) as usize].clone();
            let b = decks[rng.below(decks.len() as u32) as usize].clone();
            let mut g = Game::new(db.clone(), [&a, &b], seed);
            g.keep_log = false;
            while let Some(d) = g.pending().cloned() {
                for p in [PlayerId(0), PlayerId(1)] {
                    let obs = observe(&g, p);
                    let e = encode(&obs);
                    let od = &obs.own_deck;
                    assert!(od.known);
                    let (lib, side) = decode_own_deck(&e.global);
                    assert!(od.library.len() <= DECK_LIB, "pool list with more than {DECK_LIB} distinct cards");
                    assert_eq!(lib, od.library, "seed {seed}: library entries round trip");
                    assert_eq!(side, od.sideboard);
                    let blk = &e.global[gf::DECK..gf::DECK + DECK_NB];
                    assert_eq!(blk[dk::KNOWN], 1.0);
                    assert_eq!(blk[dk::CARDS] * 40.0, od.stats.cards as f32);
                    assert_eq!(blk[dk::UNSEEN] * 4.0, od.unseen as f32);
                    let n: u32 = od.library.iter().map(|x| x.1 as u32).sum();
                    assert_eq!(n, od.stats.cards as u32);
                    let pv = obs.players.iter().find(|v| v.id == p).unwrap();
                    assert_eq!(n as usize, pv.library_size + od.unseen as usize, "pool = library + unseen");
                    // summary and ids only in the block; unused entries zero
                    assert!(blk[dk::IDS_END..].iter().all(|&x| x == 0.0));
                    assert!(blk[dk::LIB_IDS + 3 * lib.len()..dk::SIDE_IDS].iter().all(|&x| x == 0.0));
                    checks += 1;
                }
                if g.submit(policy_like_choice(&mut rng, &d)).is_err() {
                    break;
                }
            }
        }
        assert!(checks > 1000, "{checks}");
        // over capacity: the most copies first, in id order, flagged
        let many: Vec<(CardId, u8)> = (0..60u32).map(|i| (CardId(300 + i), if i % 7 == 0 { 3 } else { 1 })).collect();
        let od = crate::observe::OwnDeckView { known: true, library: many.clone(), ..Default::default() };
        let mut g = vec![0.0f32; G];
        write_own_deck(&od, &mut g[gf::DECK..gf::DECK + DECK_NB]);
        let (lib, _) = decode_own_deck(&g);
        assert_eq!(lib.len(), DECK_LIB);
        assert_eq!(g[gf::DECK + dk::LIB_TRUNC], 1.0);
        assert!(lib.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(lib.iter().filter(|x| x.1 == 3).count(), many.iter().filter(|x| x.1 == 3).count());
    }

    /// Identical observations ⇒ identical tensors (the encoding is a pure function of the
    /// observation, which is the only input it accepts); options always point at valid rows.
    fn check_games(db: std::sync::Arc<crate::CardDb>, decks: &mut dyn FnMut(u64) -> (crate::Deck, crate::Deck), games: u64) {
        let mut rng = Rng::new(3);
        let (mut steps, mut max_e, mut max_o, mut unresolved, mut unobserved, mut crowded) = (0, 0, 0, 0, 0, 0);
        for seed in 0..games {
            let (a, b) = decks(seed);
            let mut g = Game::new(db.clone(), [&a, &b], seed);
            while let Some(d) = g.pending().cloned() {
                for p in [PlayerId(0), PlayerId(1)] {
                    let o1 = observe(&g, p);
                    let o2 = observe(&g.clone_for_search(), p);
                    assert_eq!(o1, o2);
                    let (e1, e2) = (encode(&o1), encode(&o2));
                    assert!(e1 == e2, "encoding not deterministic");
                    let n_ent = e1.entity_mask.iter().filter(|&&m| m).count();
                    unobserved += (0..n_ent).filter(|&r| e1.entity[r * F + ef::UNOBSERVED] == 1.0).count();
                    max_e = max_e.max(n_ent);
                    if p == d.player {
                        let n_opt = e1.option_mask.iter().filter(|&&m| m).count();
                        assert_eq!(n_opt, d.options.len().min(O));
                        max_o = max_o.max(n_opt);
                        for i in 0..n_opt * R {
                            let r = e1.option_refs[i];
                            assert!(r < 0 || (r as usize) < n_ent);
                        }
                        // Every option that names an object must resolve to an entity row, unless
                        // the entity budget `E` is full: rows are filled zone by zone (battlefield,
                        // hands, graveyards, exile, ...), so in a long game an exiled card or a
                        // library card being looked at can be crowded out (a known capacity limit
                        // of the v9 encoding, counted and reported, not asserted). Only the first `O` options are encoded
                        // (`take(O)` above), so only those are checked.
                        for (i, o) in d.options.iter().take(n_opt).enumerate() {
                            if matches!(o, Opt::Card(_) | Opt::Cast(_) | Opt::CastFace(..) | Opt::Flashback(_) | Opt::CastPrepared(_) | Opt::Alt(..) | Opt::PlayLand(_) | Opt::Activate(..) | Opt::Mana(..) | Opt::Target(_) | Opt::Attack(..) | Opt::Block(..) | Opt::Block2(..)) {
                                if e1.option_refs[i * R] < 0 {
                                    if n_ent < E {
                                        unresolved += 1;
                                    } else {
                                        crowded += 1;
                                    }
                                }
                            }
                        }
                    } else {
                        assert!(e1.option_mask.iter().all(|&m| !m), "non-decider sees options");
                    }
                }
                steps += 1;
                g.submit(rng.below(d.options.len() as u32) as usize).unwrap();
            }
        }
        eprintln!("{steps} steps, max entities {max_e}, max options {max_o}, unobserved option objects {unobserved}, option objects crowded out of a full entity budget {crowded}");
        assert_eq!(unresolved, 0, "options naming objects without an entity row");
    }

    #[test]
    fn deterministic_and_consistent_synth() {
        if !cardsfolder().is_dir() {
            return;
        }
        let (db, a, b) = synth_db_and_decks();
        check_games(db, &mut |_| (a.clone(), b.clone()), 20);
    }

    #[test]
    fn deterministic_and_consistent_meta() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../decks/standard");
        if !cardsfolder().is_dir() || !dir.is_dir() {
            return;
        }
        let db = meta_db(&dir);
        let mut rng = Rng::new(5);
        let db2 = db.clone();
        check_games(db, &mut |_| (random_deck(&db2, &mut rng), random_deck(&db2, &mut rng)), 40);
    }

    #[test]
    fn hidden_cards_are_minus_one() {
        if !cardsfolder().is_dir() {
            return;
        }
        let (db, a, b) = synth_db_and_decks();
        let g = Game::new(db, [&a, &b], 1);
        let e = encode(&observe(&g, PlayerId(0)));
        let hidden = (0..E).filter(|&r| e.entity_mask[r] && e.entity[r * F + ef::HIDDEN] == 1.0).collect::<Vec<_>>();
        assert_eq!(hidden.len(), 7, "opponent's opening hand should be 7 hidden tokens");
        assert!(hidden.iter().all(|&r| e.entity_card[r] == -1));
    }

    /// Leak test at the tensor level: permuting card identities among objects the observer
    /// can't see leaves the encoding unchanged.
    #[test]
    fn invariant_under_hidden_resampling() {
        if !cardsfolder().is_dir() {
            return;
        }
        let (db, a, b) = synth_db_and_decks();
        let mut rng = Rng::new(11);
        for seed in 0..10u64 {
            let mut g = Game::new(db.clone(), [&a, &b], seed);
            while let Some(d) = g.pending().cloned() {
                for me in [PlayerId(0), PlayerId(1)] {
                    let mut g2 = g.clone_for_search();
                    let unknown = |s: &State, ids: &[ObjId]| -> Vec<ObjId> { ids.iter().copied().filter(|&o| s.obj(o).known_to & (1 << me.0) == 0).collect() };
                    let mut p1 = unknown(&g2.s, &g2.s.player(me.opp()).hand);
                    p1.extend(unknown(&g2.s, &g2.s.player(me.opp()).library));
                    let p2 = unknown(&g2.s, &g2.s.player(me).library);
                    for pool in [p1, p2] {
                        let mut cards: Vec<CardId> = pool.iter().map(|&o| g2.s.obj(o).card).collect();
                        rng.shuffle(&mut cards);
                        for (o, c) in pool.into_iter().zip(cards) {
                            g2.s.obj_mut(o).card = c;
                        }
                    }
                    assert!(encode(&observe(&g, me)) == encode(&observe(&g2, me)), "seed {seed}: encoding leaks hidden info");
                }
                g.submit(rng.below(d.options.len() as u32) as usize).unwrap();
            }
        }
    }

    /// Cost of observe + encode per decision (run with --ignored --nocapture).
    #[test]
    #[ignore]
    fn encode_speed() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../decks/standard");
        let db = meta_db(&dir);
        let mut rng = Rng::new(9);
        let mut games = vec![];
        for seed in 0..50 {
            let (a, b) = (random_deck(&db, &mut rng), random_deck(&db, &mut rng));
            let h = random_game(db.clone(), &a, &b, seed).0.history;
            games.push((a, b, seed, h));
        }
        // Replay: engine only vs. engine + observe + encode of every decision.
        let mut buf = Encoded::default();
        for with_encode in [false, true, false, true] {
            let t = std::time::Instant::now();
            let mut n = 0;
            for (a, b, seed, h) in &games {
                let mut g = Game::new(db.clone(), [a, b], *seed);
                g.keep_log = false;
                for &c in h {
                    if with_encode {
                        let p = g.pending().unwrap().player;
                        encode_into(&observe(&g, p), buf.out());
                    }
                    g.submit(c as usize).unwrap();
                    n += 1;
                }
            }
            let us = t.elapsed().as_secs_f64() * 1e6 / n as f64;
            eprintln!("encode={with_encode}: {us:.1} us/decision over {n} decisions");
        }
        // observe and encode alone, timed separately on every decision (5 repetitions each).
        let (mut t_obs, mut t_enc, mut n) = (0f64, 0f64, 0usize);
        for (a, b, seed, h) in &games {
            let mut g = Game::new(db.clone(), [a, b], *seed);
            g.keep_log = false;
            for &c in h {
                let p = g.pending().unwrap().player;
                let t = std::time::Instant::now();
                for _ in 0..5 {
                    std::hint::black_box(observe(&g, p));
                }
                t_obs += t.elapsed().as_secs_f64();
                let o = observe(&g, p);
                let t = std::time::Instant::now();
                for _ in 0..5 {
                    encode_into(std::hint::black_box(&o), buf.out());
                }
                t_enc += t.elapsed().as_secs_f64();
                g.submit(c as usize).unwrap();
                n += 1;
            }
        }
        eprintln!("observe alone: {:.2} us/decision, encode alone: {:.2} us/decision (v{ENCODING_VERSION}, {n} decisions)", t_obs * 1e6 / (5 * n) as f64, t_enc * 1e6 / (5 * n) as f64);
    }

    /// v7 sizes (the v7 view below is shaped like a v7 build's tensors).
    const F7: usize = 96;
    const K7: usize = 80;
    /// Option types whose refs v8 added (Trigger, Mode, CostOption, CastFace, Flashback).
    const V8_REF_SLOTS: [usize; 5] = [14, 21, 23, 27, 28];

    /// The v7 view of a v8 encoding, as the bytes of v7-shaped tensors: v8 token rows (effects,
    /// pending triggers) removed, entity columns 95+ / global 126–127 / option columns 80+ cut
    /// (95 and 126–127 were spare, i.e. 0, in v7), option types 27–29 back in "other" (31), and
    /// no refs on the options that had none before v8. Equal to a v7 build's tensors on the same
    /// game states (`v7_view_digest`).
    pub(crate) fn v7_bytes(e: &Encoded) -> Vec<u8> {
        let v8_row = |r: usize| e.entity[r * F + ef::KIND_EFFECT] == 1.0 || e.entity[r * F + ef::KIND_TRIGGER] == 1.0;
        let mut ent = vec![0f32; E * F7];
        let mut card = vec![-1i32; E];
        let mut mask = vec![false; E];
        for r in 0..E {
            if e.entity_mask[r] && !v8_row(r) {
                ent[r * F7..r * F7 + 95].copy_from_slice(&e.entity[r * F..r * F + 95]);
                card[r] = e.entity_card[r];
                mask[r] = true;
            }
        }
        let mut glob = e.global[..126].to_vec();
        glob.extend([0.0, 0.0]);
        let mut opt = vec![0f32; O * K7];
        let mut refs = e.option_refs.clone();
        for i in 0..O {
            let row = &mut opt[i * K7..(i + 1) * K7];
            row.copy_from_slice(&e.option[i * K..i * K + K7]);
            let slot = (0..OPT_SLOTS).find(|&s| row[of::TYPE + s] == 1.0);
            if let Some(s @ (27 | 28 | 29)) = slot {
                row[of::TYPE + s] = 0.0;
                row[of::TYPE + OPT_SLOTS - 1] = 1.0;
            }
            if slot.is_some_and(|s| V8_REF_SLOTS.contains(&s)) {
                refs[i * R..(i + 1) * R].fill(-1);
            }
        }
        let mut b = vec![];
        b.extend(ent.iter().flat_map(|x| x.to_bits().to_le_bytes()));
        b.extend(card.iter().flat_map(|x| x.to_le_bytes()));
        b.extend(mask.iter().map(|&x| x as u8));
        b.extend(glob.iter().flat_map(|x| x.to_bits().to_le_bytes()));
        b.extend(opt.iter().flat_map(|x| x.to_bits().to_le_bytes()));
        b.extend(e.option_mask.iter().map(|&x| x as u8));
        b.extend(refs.iter().flat_map(|x| x.to_le_bytes()));
        b.extend(e.option_ability.iter().flat_map(|x| x.to_le_bytes()));
        b
    }

    /// Digest of the v7 view of every decision's encoding (both players' observations) in
    /// policy-like pool games (`MTG_DIGEST_GAMES`, default 120). Printed, and compared with
    /// `MTG_V7_DIGEST` when set: v8 tensors with the v8 additions removed must equal a v7
    /// build's bit for bit (run with --ignored; the v7 build hashes its tensors as they are).
    /// Also counts how often each v8 input is non-zero, so the digest isn't vacuous.
    #[test]
    #[ignore]
    fn v7_view_digest() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../decks/pool/2026-10-31");
        let (db, decks) = pool_decks(&dir);
        let games: u64 = std::env::var("MTG_DIGEST_GAMES").ok().and_then(|v| v.parse().ok()).unwrap_or(120);
        let mut rng = Rng::new(23);
        let mut h: u64 = 0xcbf29ce484222325;
        let mut n = 0u64;
        // v8 inputs seen non-zero: entity columns 95+, globals 126-127, option column 80
        let mut seen = [0u64; F - 95 + 3];
        for seed in 0..games {
            let a = decks[rng.below(decks.len() as u32) as usize].clone();
            let b = decks[rng.below(decks.len() as u32) as usize].clone();
            let mut g = Game::new(db.clone(), [&a, &b], seed);
            g.keep_log = false;
            let mut k = 0;
            while let Some(d) = g.pending().cloned() {
                k += 1;
                if k > 3000 {
                    break;
                }
                for p in [PlayerId(0), PlayerId(1)] {
                    let e = encode(&observe(&g, p));
                    for x in v7_bytes(&e) {
                        h = (h ^ x as u64).wrapping_mul(0x100000001b3);
                    }
                    for c in 95..F {
                        seen[c - 95] += (0..E).any(|r| e.entity[r * F + c] != 0.0) as u64;
                    }
                    seen[F - 95] += (e.global[gf::TGT_PART] != 0.0) as u64;
                    seen[F - 95 + 1] += (0..O).any(|i| e.option[i * K + of::TGT_OPP_WARD] != 0.0) as u64;
                    seen[F - 95 + 2] += (0..O).any(|i| e.option[i * K + of::TYPE + 14] == 1.0 && e.option_refs[i * R] >= 0) as u64;
                    n += 1;
                }
                if g.submit(policy_like_choice(&mut rng, &d)).is_err() {
                    break;
                }
            }
        }
        let digest = format!("{h:016x}");
        eprintln!("v7-view digest {digest} over {n} encodings (v{ENCODING_VERSION})");
        eprintln!("encodings with v8 entity columns 95.. non-zero: {:?}", &seen[..F - 95]);
        eprintln!("with TGT_PART {}, TGT_OPP_WARD {}, Trigger refs {}", seen[F - 95], seen[F - 95 + 1], seen[F - 95 + 2]);
        if let Ok(want) = std::env::var("MTG_V7_DIGEST") {
            assert_eq!(digest, want, "v7 view differs from the v7 encoding");
        }
    }

    /// Option identity survey (run with --ignored --nocapture; `MTG_SURVEY_GAMES`, default 200):
    /// policy-like games over the frozen pool's playable lists. Per option variant: how often it
    /// is offered, how often its first ref is empty, and in how many decisions two options of
    /// that variant encode identically (same option row and same refs: a net can't tell
    /// them apart).
    #[test]
    #[ignore]
    fn option_identity_survey() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../decks/pool/2026-10-31");
        let (db, decks) = pool_decks(&dir);
        let games: u64 = std::env::var("MTG_SURVEY_GAMES").ok().and_then(|v| v.parse().ok()).unwrap_or(200);
        let mut rng = Rng::new(17);
        // variant -> (offered, no ref, decisions offering it, decisions with identical pairs)
        let mut st: std::collections::BTreeMap<String, [u64; 4]> = Default::default();
        let mut decisions = 0u64;
        for seed in 0..games {
            let a = decks[rng.below(decks.len() as u32) as usize].clone();
            let b = decks[rng.below(decks.len() as u32) as usize].clone();
            let mut g = Game::new(db.clone(), [&a, &b], seed);
            g.keep_log = false;
            let mut n = 0;
            while let Some(d) = g.pending().cloned() {
                n += 1;
                if n > 3000 {
                    break;
                }
                decisions += 1;
                let e = encode(&observe(&g, d.player));
                let name = |o: &Opt| -> String {
                    let s = format!("{o:?}");
                    match o {
                        Opt::Target(Target::Obj(_) | Target::Ability { .. }) => "Target(obj)".into(),
                        Opt::Target(Target::Player(_)) => "Target(player)".into(),
                        _ => s.split('(').next().unwrap().to_string(),
                    }
                };
                let mut seen: std::collections::BTreeMap<String, (bool, bool)> = Default::default();
                let n_opt = d.options.len().min(O);
                for i in 0..n_opt {
                    let o = &d.options[i];
                    let k = name(o);
                    let e_ = st.entry(k.clone()).or_default();
                    e_[0] += 1;
                    if e.option_refs[i * R] < 0 {
                        e_[1] += 1;
                    }
                    let dup = (0..i).any(|j| name(&d.options[j]) == k && e.option[j * K..(j + 1) * K] == e.option[i * K..(i + 1) * K] && e.option_refs[j * R..(j + 1) * R] == e.option_refs[i * R..(i + 1) * R]);
                    let s = seen.entry(k).or_default();
                    s.0 = true;
                    s.1 |= dup;
                }
                for (k, (_, dup)) in seen {
                    let e_ = st.get_mut(&k).unwrap();
                    e_[2] += 1;
                    e_[3] += dup as u64;
                }
                g.submit(policy_like_choice(&mut rng, &d)).unwrap();
            }
        }
        eprintln!("v{ENCODING_VERSION}: {games} games, {decisions} decisions over {} playable pool lists", decks.len());
        eprintln!("{:16} {:>9} {:>8} {:>10} {:>14}", "option", "offered", "no ref", "decisions", "indist. pairs");
        for (k, v) in &st {
            eprintln!("{:16} {:>9} {:>8} {:>10} {:>14}", k, v[0], v[1], v[2], v[3]);
        }
    }

    /// Unknown kinds/options fall into the "other" bucket rather than panicking.
    #[test]
    fn other_buckets() {
        assert!(kind_slot(DecisionKind::Kicker) < KIND_SLOTS - 1);
        assert!(opt_slot(&Opt::Mode(0)) < OPT_SLOTS - 1);
    }
}
