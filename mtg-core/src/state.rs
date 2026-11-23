//! Game state: plain data, cheap to clone. No references into the card DB other than ids.

use crate::basics::*;
use crate::carddb::{AbilityId, KwSet};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Zone {
    Library,
    Hand,
    Battlefield,
    Graveyard,
    Stack,
    Exile,
    /// No longer exists as this object (left its zone → new object, CR 400.7; or token ceased).
    Gone,
    /// Outside the game (CR 400.11): the player's sideboard (400.11a), cards a "wish" can put
    /// into the game. Known only to its owner; listed in `Player::sideboard`.
    Sideboard,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CounterKind {
    P1P1,
    M1M1,
    Loyalty,
    /// CR 122.1d: replaces untapping with removing a stun counter.
    Stun,
    /// A counter kind with no inherent rules meaning (index into `CardDb::counter_names`).
    Named(u16),
}

/// `Clone` is hand-written (below): copying every unit's state is hot, and the derived clone
/// goes through `SmallVec`'s generic iterator path for `counters`.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Obj {
    pub card: CardId,
    pub owner: PlayerId,
    pub controller: PlayerId,
    pub zone: Zone,
    pub tapped: bool,
    /// Turn number when its controller last gained control (summoning sickness, CR 302.6).
    pub ctrl_since: u32,
    pub damage: i32,
    /// Dealt damage by a deathtouch source since last SBA check (CR 704.5h).
    pub deathtouched: bool,
    pub counters: SmallVec<[(CounterKind, i32); 2]>,
    /// Timestamp (CR 613.7).
    pub ts: u32,
    pub token: bool,
    /// Bitmask of players who know this object's identity while in a hidden zone.
    pub known_to: u8,
    /// Turn number this object entered its current zone.
    pub entered: u32,
    /// Player who cast it, if it became a permanent by resolving as a spell (`wasCast`).
    pub cast_by: Option<PlayerId>,
    /// Which face's characteristics it has (CR 715.3b: an Adventure on the stack uses face 1).
    pub face: u8,
    /// Exiled "on an adventure" (CR 715.3d): its owner may cast it from exile.
    pub on_adventure: bool,
    /// The object this Equipment/Aura is attached to (CR 301.5, 303.4, 701.3).
    pub attached_to: Option<ObjId>,
    /// Has the "prepared" designation (CR 722.3a).
    pub prepared: bool,
    /// Mana spent to cast it (while a spell, and as the permanent it became).
    pub cast_mana: u8,
}

const CHUNK_BITS: usize = 5;
const CHUNK: usize = 1 << CHUNK_BITS;
type Chunk = [Obj; CHUNK];

/// All objects of a game, indexed by `ObjId`, stored in copy-on-write chunks of `CHUNK`.
/// Copying a state (once per engine unit, and for every search clone) shares the chunks
/// (reference-count increments); the first write to a shared chunk copies just that chunk
/// (`Arc::make_mut`). Behaves like `Vec<Obj>` (indexing panics past `len`; `Debug`, serde and
/// equality are those of the object list). Slots past `len` in the last chunk are unused
/// placeholders.
#[derive(Default)]
pub struct ObjVec {
    chunks: Vec<std::sync::Arc<Chunk>>,
    len: usize,
}

impl Clone for ObjVec {
    fn clone(&self) -> ObjVec {
        ObjVec { chunks: self.chunks.clone(), len: self.len }
    }
    /// As `*self = o.clone()` (same chunks shared, same reference counts afterwards), without the
    /// two atomic reference-count updates per chunk both states already share (`State::copy_from`
    /// runs once per engine unit, and most chunks are unchanged between units).
    fn clone_from(&mut self, o: &ObjVec) {
        self.len = o.len;
        self.chunks.truncate(o.chunks.len());
        for (a, b) in self.chunks.iter_mut().zip(&o.chunks) {
            if !std::sync::Arc::ptr_eq(a, b) {
                *a = b.clone();
            }
        }
        let k = self.chunks.len();
        self.chunks.extend(o.chunks[k..].iter().cloned());
    }
}

impl ObjVec {
    fn placeholder() -> Obj {
        Obj { card: CardId(0), owner: PlayerId(0), controller: PlayerId(0), zone: Zone::Gone, tapped: false, ctrl_since: 0, damage: 0, deathtouched: false, counters: SmallVec::new(), ts: 0, token: false, known_to: 0, entered: 0, cast_by: None, face: 0, on_adventure: false, attached_to: None, prepared: false, cast_mana: 0 }
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    #[inline]
    pub fn get(&self, i: usize) -> Option<&Obj> {
        if i < self.len {
            Some(&self.chunks[i >> CHUNK_BITS][i & (CHUNK - 1)])
        } else {
            None
        }
    }
    #[inline]
    pub fn get_mut(&mut self, i: usize) -> Option<&mut Obj> {
        if i < self.len {
            Some(&mut std::sync::Arc::make_mut(&mut self.chunks[i >> CHUNK_BITS])[i & (CHUNK - 1)])
        } else {
            None
        }
    }
    pub fn push(&mut self, o: Obj) {
        let i = self.len;
        if i & (CHUNK - 1) == 0 {
            self.chunks.push(std::sync::Arc::new(std::array::from_fn(|_| Self::placeholder())));
        }
        std::sync::Arc::make_mut(&mut self.chunks[i >> CHUNK_BITS])[i & (CHUNK - 1)] = o;
        self.len += 1;
    }
    pub fn iter(&self) -> impl Iterator<Item = &Obj> + '_ {
        self.chunks.iter().flat_map(|c| c.iter()).take(self.len)
    }
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Obj> + '_ {
        let len = self.len;
        self.chunks.iter_mut().flat_map(|c| std::sync::Arc::make_mut(c).iter_mut()).take(len)
    }
    pub fn to_vec(&self) -> Vec<Obj> {
        self.iter().cloned().collect()
    }
}

impl std::ops::Index<usize> for ObjVec {
    type Output = Obj;
    #[inline]
    fn index(&self, i: usize) -> &Obj {
        match self.get(i) {
            Some(o) => o,
            None => panic!("index out of bounds: the len is {} but the index is {i}", self.len),
        }
    }
}

impl std::ops::IndexMut<usize> for ObjVec {
    #[inline]
    fn index_mut(&mut self, i: usize) -> &mut Obj {
        let len = self.len;
        match self.get_mut(i) {
            Some(o) => o,
            None => panic!("index out of bounds: the len is {len} but the index is {i}"),
        }
    }
}

impl PartialEq for ObjVec {
    fn eq(&self, o: &ObjVec) -> bool {
        self.len == o.len && self.chunks.iter().zip(&o.chunks).enumerate().all(|(k, (a, b))| std::sync::Arc::ptr_eq(a, b) || a[..CHUNK.min(self.len - k * CHUNK)] == b[..CHUNK.min(self.len - k * CHUNK)])
    }
}
impl Eq for ObjVec {}

impl std::fmt::Debug for ObjVec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl FromIterator<Obj> for ObjVec {
    fn from_iter<I: IntoIterator<Item = Obj>>(it: I) -> ObjVec {
        let mut v = ObjVec::default();
        for o in it {
            v.push(o);
        }
        v
    }
}

impl<'a> IntoIterator for &'a ObjVec {
    type Item = &'a Obj;
    type IntoIter = Box<dyn Iterator<Item = &'a Obj> + 'a>;
    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter())
    }
}

impl<'a> IntoIterator for &'a mut ObjVec {
    type Item = &'a mut Obj;
    type IntoIter = Box<dyn Iterator<Item = &'a mut Obj> + 'a>;
    fn into_iter(self) -> Self::IntoIter {
        Box::new(self.iter_mut())
    }
}

impl Serialize for ObjVec {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(self.iter())
    }
}

impl<'de> Deserialize<'de> for ObjVec {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<ObjVec, D::Error> {
        Ok(Vec::<Obj>::deserialize(d)?.into_iter().collect())
    }
}

impl Clone for Obj {
    #[inline]
    fn clone(&self) -> Obj {
        // Every field but `counters` is `Copy`.
        Obj { counters: SmallVec::from_slice(&self.counters), ..*self }
    }
    #[inline]
    fn clone_from(&mut self, src: &Obj) {
        let mut c = std::mem::take(&mut self.counters);
        c.clear();
        c.extend_from_slice(&src.counters);
        *self = Obj { counters: c, ..*src };
    }
}

impl Obj {
    /// Exiled face down (CR 406.3), or a face-down permanent (CR 708; `ext6`): exile and the
    /// battlefield are public, so an object there that isn't known to both players is face down
    /// (`known_to` holds who may look at it).
    pub fn face_down(&self) -> bool {
        matches!(self.zone, Zone::Exile | Zone::Battlefield) && self.known_to != 0b11
    }
    pub fn counter(&self, k: CounterKind) -> i32 {
        self.counters.iter().find(|c| c.0 == k).map(|c| c.1).unwrap_or(0)
    }
    pub fn add_counter(&mut self, k: CounterKind, n: i32) {
        if let Some(c) = self.counters.iter_mut().find(|c| c.0 == k) {
            // Never wraps; the game path (`ext6::put_counters`) bounds counts first (`num`).
            c.1 = c.1.saturating_add(n);
        } else if n != 0 {
            self.counters.push((k, n));
        }
        self.counters.retain(|c| c.1 > 0);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Player {
    pub life: i32,
    pub library: Vec<ObjId>,
    pub hand: Vec<ObjId>,
    pub graveyard: Vec<ObjId>,
    pub lands_played: u8,
    pub drew_from_empty: bool,
    pub poison: u8,
    /// Turn number of the most recent turn this player started.
    pub last_turn_started: u32,
    pub mulligans: u8,
    pub kept: bool,
    /// Cards outside the game (CR 400.11a: the sideboard), in canonical order.
    #[serde(default)]
    pub sideboard: Vec<ObjId>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Target {
    Obj(ObjId),
    Player(PlayerId),
    /// An activated or triggered ability on the stack (CR 113.7a, 115.1b-d), named by its stack
    /// object (`StackItem::uid`): several abilities of one source can be on the stack at once,
    /// and a spell can be there with its own triggered ability. `src` = its source (what
    /// "that ability's source / that permanent" refers to, and what the encoder maps this
    /// target to: the source's row). Spells stay
    /// `Obj` (a spell is an object of its own on the stack).
    Ability { uid: u32, src: ObjId },
}

impl Target {
    /// The object a battlefield/stack effect acts on: a spell or permanent (`Obj`), never an
    /// ability's source.
    pub fn obj(self) -> Option<ObjId> {
        match self {
            Target::Obj(o) => Some(o),
            _ => None,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StackKind {
    Spell,
    Activated,
    Triggered,
}

/// Information about the event that caused a triggered ability.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrigCtx {
    /// The object involved, as it last existed (LKI) for leaves-the-battlefield events.
    pub obj: Option<ObjId>,
    /// The object's new incarnation after a zone change.
    pub new_obj: Option<ObjId>,
    pub player: Option<PlayerId>,
    pub amount: i32,
    /// The spell or ability on the stack involved (`StackItem::uid`): the one that made a
    /// permanent its target, for ward's "counter that spell or ability" (CR 702.21a).
    #[serde(default)]
    pub stack_item: Option<u32>,
    /// The permanent that became the target, for a "becomes the target" trigger (`ext9`:
    /// "target creature you control other than that creature").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<ObjId>,
    /// The source of the life gain, for a "whenever [a player] gains life" trigger (CR 119.9:
    /// "whenever a source causes [a player] to gain life"; 702.15e: one event per lifelink
    /// source). Read only to name the trigger instance (`scenario::disambiguate_triggers`)
    /// and to tell instances apart (`sba::trigger_key`); not a reference to the
    /// object (`query::refs_of`): no trigger reads which source it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gain_source: Option<ObjId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackItem {
    pub kind: StackKind,
    pub controller: PlayerId,
    /// The spell object itself, or the source of the ability.
    pub source: ObjId,
    pub source_card: CardId,
    pub ability: AbilityId,
    /// Targets per ability-chain node (node index, target).
    pub targets: SmallVec<[(u8, Target); 2]>,
    pub x: i32,
    pub kicked: bool,
    /// Chosen modes of a modal ability (indices), in order.
    pub modes: SmallVec<[u8; 3]>,
    pub trig: TrigCtx,
    /// Index into the source card's triggers (for intervening-if rechecks).
    pub trigger_idx: Option<u8>,
    /// Cast with flashback: exiled instead of going anywhere else when it leaves the stack.
    pub flashback: bool,
    /// Identifies this stack object while it is on the stack (`State::push_stack`): several
    /// abilities of one source can be on the stack at once, and an ability exists independently
    /// of its source (CR 113.7a).
    #[serde(default)]
    pub uid: u32,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Step {
    Untap,
    Upkeep,
    Draw,
    Main1,
    BeginCombat,
    DeclareAttackers,
    DeclareBlockers,
    FirstStrikeDamage,
    CombatDamage,
    EndCombat,
    Main2,
    End,
    Cleanup,
}

impl Step {
    pub fn next(self) -> Option<Step> {
        use Step::*;
        Some(match self {
            Untap => Upkeep,
            Upkeep => Draw,
            Draw => Main1,
            Main1 => BeginCombat,
            BeginCombat => DeclareAttackers,
            DeclareAttackers => DeclareBlockers,
            DeclareBlockers => FirstStrikeDamage,
            FirstStrikeDamage => CombatDamage,
            CombatDamage => EndCombat,
            EndCombat => Main2,
            Main2 => End,
            End => Cleanup,
            Cleanup => return None,
        })
    }
    pub fn is_main(self) -> bool {
        matches!(self, Step::Main1 | Step::Main2)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    pub number: u32,
    pub active: PlayerId,
    pub step: Step,
    /// Turn-based actions for this step are done.
    pub tba_done: bool,
    pub priority: PlayerId,
    /// Consecutive passes with no action in between.
    pub passes: u8,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Combat {
    /// (attacker, defending player)
    pub attackers: SmallVec<[(ObjId, PlayerId); 8]>,
    /// (blocker, attacker)
    pub blocks: SmallVec<[(ObjId, ObjId); 8]>,
    /// Attackers that became blocked (stay blocked even if blockers leave, CR 509.1h).
    pub blocked: SmallVec<[ObjId; 8]>,
    /// Combatants that had first or double strike as the first-strike damage step began.
    pub first_strike_dealt: SmallVec<[ObjId; 4]>,
    /// Attackers attacking a planeswalker rather than its controller: (attacker, planeswalker)
    /// (CR 508.1b). The planeswalker's controller is the defending player in `attackers`.
    pub attacking_pw: SmallVec<[(ObjId, ObjId); 2]>,
    /// Attackers picked so far while the active player's declaration (CR 508.1a) is in
    /// progress; empty otherwise. Observable to the declaring player only (encoding v6).
    #[serde(default)]
    pub declaring: SmallVec<[ObjId; 8]>,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Layer {
    /// 6: ability adding/removing
    L6,
    /// 7c: P/T modifications
    #[default]
    L7c,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContEffect {
    pub ts: u32,
    pub affected: ObjId,
    /// Layer of the P/T/keyword part (pump effects); other parts apply in their own layers.
    pub layer: Layer,
    pub pt: (i32, i32),
    pub kws: KwSet,
    pub until_eot: bool,
    /// Ends when this player's next turn begins.
    pub until_turn_of: Option<PlayerId>,
    /// Layer 4: card types added / removed, subtypes added, creature types removed.
    pub add_types: TypeSet,
    pub remove_card_types: bool,
    pub add_subtypes: SmallVec<[Subtype; 2]>,
    pub remove_creature_types: bool,
    /// Layer 5: colors (replace if `overwrite_colors`, else add).
    pub colors: Option<ColorSet>,
    pub overwrite_colors: bool,
    /// Layer 6: abilities gained.
    pub abilities: SmallVec<[AbilityId; 1]>,
    /// Layer 7b: base P/T set (`i32::MIN` for a value that isn't changed).
    pub set_pt: Option<(i32, i32)>,
    /// Layer 4: gains all creature types.
    pub all_creature_types: bool,
    /// Layer 6: triggered abilities gained (indices into `CardDb::triggers_pool`).
    pub triggers: SmallVec<[u32; 1]>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TriggerEvent {
    SelfEnters,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingTrigger {
    pub source: ObjId,
    pub card: CardId,
    pub controller: PlayerId,
    pub trigger_idx: u8,
    pub trig: TrigCtx,
    /// Delayed/reflexive/effect-object triggered ability (not one of the card's triggers):
    /// the ability to put on the stack (`trigger_idx` unused).
    pub adhoc: Option<AbilityId>,
}

/// An effect object (CR 611 continuous effects from resolved abilities, emblems): static
/// abilities applying for a duration, often to remembered objects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectObj {
    pub source: ObjId,
    pub controller: PlayerId,
    pub statics: SmallVec<[u32; 2]>,
    pub remembered: SmallVec<[ObjId; 2]>,
    pub duration: crate::carddb::EffDuration,
    /// Turn number when created (for "until the end of your next turn").
    pub created_turn: u32,
    pub forget_on_moved: crate::carddb::ZoneSet,
    pub exile_on_moved: crate::carddb::ZoneSet,
    pub had_remembered: bool,
    pub ts: u32,
    /// Triggered abilities of the effect (indices into `CardDb::triggers_pool`), e.g.
    /// "whenever you attack this turn" (Dalkovan Encampment), emblems.
    pub triggers: SmallVec<[u32; 1]>,
}

/// Per-turn bookkeeping (reset when a turn begins), used by conditions and limits.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnStats {
    pub life_gained: [i32; 2],
    pub life_lost: [i32; 2],
    pub spells_cast: [u8; 2],
    /// (source object, trigger index, times triggered this turn)
    pub trigger_counts: SmallVec<[(ObjId, u8, u8); 4]>,
    /// Not reset: number of turns each player has begun.
    pub turns_taken: [u16; 2],
    /// Permanents whose loyalty abilities were activated this turn (CR 606.3).
    pub loyalty_activated: SmallVec<[ObjId; 2]>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stage {
    Mulligan,
    Playing,
    Over,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GameResult {
    Win(PlayerId),
    Draw,
}

/// Public game events (god-view log; front-ends filter by observer).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    TurnBegan { turn: u32, active: PlayerId },
    StepBegan { step: Step },
    Mulligan { player: PlayerId, count: u8 },
    Kept { player: PlayerId, hand: u8 },
    Draw { player: PlayerId, card: CardId },
    PlayLand { player: PlayerId, card: CardId },
    Cast { player: PlayerId, card: CardId, targets: Vec<Target> },
    /// A double-faced permanent (card `card`) is now showing the face `face_card` (CR 712).
    Transformed { obj: ObjId, card: CardId, face_card: CardId },
    /// A permanent (its own card `card`) entered as a copy of `copied` (CR 707).
    BecameCopy { obj: ObjId, card: CardId, copied: CardId },
    /// An Equipment/Aura became attached to a permanent (CR 701.3).
    Attached { card: CardId, obj: ObjId, to: ObjId },
    /// Follows `Cast` when the spell uses an alternative face (Adventure, Omen, prepare spell).
    CastFace { player: PlayerId, card: CardId, face: u8 },
    /// A copy of a spell was put onto the stack (not cast; CR 707.10), with the targets it
    /// copied (its controller may then choose new ones, 707.10c).
    SpellCopied { player: PlayerId, card: CardId, targets: Vec<Target> },
    /// A copy of an activated/triggered ability of `card` was put onto the stack (CR 707.10),
    /// with the targets it copied.
    AbilityCopied { player: PlayerId, card: CardId, targets: Vec<Target> },
    /// A face-down permanent was turned face up (CR 701.40b, 708.8): it is `card`.
    TurnedFaceUp { player: PlayerId, obj: ObjId, card: CardId },
    /// `player` flipped `flips` coins (no call, CR 705.2): `heads` came up heads.
    CoinFlips { player: PlayerId, flips: u8, heads: u8 },
    Activate { player: PlayerId, card: CardId },
    TriggerPut { player: PlayerId, card: CardId },
    Resolve { card: CardId },
    Fizzle { card: CardId },
    Countered { card: CardId },
    Damage { source: CardId, to: Target, amount: i32 },
    LifeChange { player: PlayerId, delta: i32, life: i32 },
    Dies { card: CardId, owner: PlayerId },
    Destroyed { card: CardId },
    Attack { attacker: CardId, obj: ObjId, defender: PlayerId },
    /// The attacker declared just before attacks this planeswalker (CR 508.1b).
    AttackPlaneswalker { attacker: CardId, obj: ObjId, planeswalker: ObjId },
    Block { blocker: CardId, attacker: CardId },
    Discard { player: PlayerId, card: CardId },
    GameOver { result: GameResult },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub rng: Rng,
    pub stage: Stage,
    pub players: [Player; 2],
    pub objs: ObjVec,
    pub battlefield: Vec<ObjId>,
    pub exile: Vec<ObjId>,
    pub stack: Vec<StackItem>,
    pub turn: Turn,
    pub combat: Combat,
    pub effects: Vec<ContEffect>,
    pub effect_objs: Vec<EffectObj>,
    pub pending_triggers: Vec<PendingTrigger>,
    pub next_ts: u32,
    pub starting_player: PlayerId,
    pub result: Option<GameResult>,
    pub turn_stats: TurnStats,
    /// How objects were cast and casting designations (warped, plotted, ...), for the few
    /// objects that have any (`altcast`). Entries are dropped when the object changes zones.
    pub cast_info: Vec<(ObjId, crate::altcast::CastInfo)>,
    /// Events since the last commit (drained into the game log).
    pub events: Vec<Event>,
    /// Choices, links, delayed triggers and per-turn history (see `ext`).
    pub ext: crate::ext::Ext,
    /// Public history of recent turns (`hist`; encoding v7). Derived from public events only.
    #[serde(default)]
    pub hist: crate::hist::History,
    /// Layer-1 copy effects on permanents (CR 707): the object's `card` is the copied card;
    /// this records its own card and the copy's exceptions (707.9).
    pub copy_effects: Vec<CopyEffect>,
    /// Scenario positions only (`scenario`; `None` in every normal game, so cloning and
    /// serialization are unchanged): every zone change's (new object, old object) pair in
    /// creation order, so a scenario `key` can follow its card across zone changes (CR 400.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage: Option<Vec<(ObjId, ObjId)>>,
    /// A game number left the engine's bound (`num::NUM_MAX`): what and where. Set by
    /// `State::num_add` / `num_check` / `sba::check`; the unit then fails with
    /// `Halt::Unsupported` (`Ctx::choose`, `Game::advance`), so a committed state never has it.
    /// `None` in every normal game (serialization unchanged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow: Option<String>,
    /// The spell or ability resolving now (CR 608.2): `resolve::resolve_top` takes it off
    /// `stack` (the items waiting to resolve) but it is still on the stack until the final part of
    /// its resolution (608.2n: a spell is put into its graveyard, an ability ceases to exist; a
    /// permanent spell until it enters the battlefield, 608.3, 608.3a: its "as enters" choices are
    /// made while it is a spell, 614.12). Cleared when the spell's object leaves the stack zone
    /// (`spell_left_stack`) or the resolution ends, so a committed state never has it (`None`:
    /// serialization unchanged). Read it through `stack_items` / `stack_item_of`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolving: Option<StackItem>,
    /// While the resolving permanent spell enters the battlefield (its "as enters" choices,
    /// CR 608.3a, 614.12a): the new object it becomes (`move_obj_limbo`, not placed yet). The
    /// observation links that object to the spell (`ObjView`s in `Observation::entering`).
    /// Cleared with `resolving`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub becoming: Option<ObjId>,
    /// The triggered ability being put on the stack now (CR 603.3: it "becomes the topmost object
    /// on the stack"; 603.3c-d: its modes and targets are chosen as it is put there, as for a
    /// spell being cast, 601.2b-c): set by `sba::put_triggers_on_stack` while its controller
    /// makes those choices, with the modes and the targets of its earlier targeted parts chosen
    /// so far (`cast::choose_targets_where`); cleared once it is pushed or removed. A view only
    /// (`observe` lists it on top of the stack, `StackView::being_put`; `scenario::state_json`):
    /// no rules reader sees it (`stack` / `stack_items` don't include it;
    /// behaviour-neutral, like `casting` for a spell being cast), and a committed state never
    /// has it (`None`: serialization unchanged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub being_put: Option<StackItem>,
}

/// A copy effect on a permanent (CR 707.2, 707.9): `printed` is the object's own card (what it
/// is again once it leaves the battlefield); the rest are exceptions to the copied values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyEffect {
    pub obj: ObjId,
    pub printed: CardId,
    /// Card whose name the copy has instead (`NewName$`).
    pub name: Option<CardId>,
    pub add_types: TypeSet,
    pub add_subtypes: SmallVec<[Subtype; 2]>,
    pub kws: KwSet,
    pub set_pt: Option<(i32, i32)>,
    /// Not a copy: a transformed double-faced permanent (CR 712) showing its back face
    /// (`card` is the back-face card, `printed` the front); mana value from the front (712.8e).
    pub transformed: bool,
    /// Supertypes added by the copy's exceptions ("except it's legendary", CR 707.9b).
    #[serde(default)]
    pub add_super: u8,
}

impl CopyEffect {
    /// A copy of a copy (CR 707.3): the copied object's copy-effect exceptions are part of its
    /// copiable values (707.9b), so they carry over; this copy's own exceptions win where both
    /// set a value (707.9d). Destructures every field, so a new exception field can't be
    /// forgotten here ("except it's legendary" was).
    pub fn inherit_exceptions(&mut self, prev: &CopyEffect) {
        // `obj`, `printed`: this copy's own; `transformed`: not a copiable value (712.8e is
        // about the transformed permanent itself).
        let CopyEffect { obj: _, printed: _, name, add_types, add_subtypes, kws, set_pt, transformed: _, add_super } = prev;
        self.name = self.name.or(*name);
        self.add_types = TypeSet(self.add_types.0 | add_types.0);
        for st in add_subtypes {
            if !self.add_subtypes.contains(st) {
                self.add_subtypes.push(*st);
            }
        }
        self.kws = self.kws.union(*kws);
        self.set_pt = self.set_pt.or(*set_pt);
        self.add_super |= *add_super;
    }
}

impl State {
    pub fn obj(&self, id: ObjId) -> &Obj {
        &self.objs[id.idx()]
    }
    pub fn obj_mut(&mut self, id: ObjId) -> &mut Obj {
        &mut self.objs[id.idx()]
    }
    /// How `id` was cast / its casting designations (`altcast`).
    pub fn cast_of(&self, id: ObjId) -> crate::altcast::CastInfo {
        self.cast_info.iter().find(|e| e.0 == id).map(|e| e.1).unwrap_or_default()
    }
    pub fn cast_mut(&mut self, id: ObjId) -> &mut crate::altcast::CastInfo {
        let i = match self.cast_info.iter().position(|e| e.0 == id) {
            Some(i) => i,
            None => {
                self.cast_info.push((id, Default::default()));
                self.cast_info.len() - 1
            }
        };
        &mut self.cast_info[i].1
    }
    pub fn player(&self, p: PlayerId) -> &Player {
        &self.players[p.idx()]
    }
    pub fn player_mut(&mut self, p: PlayerId) -> &mut Player {
        &mut self.players[p.idx()]
    }
    pub fn ts(&mut self) -> u32 {
        self.next_ts += 1;
        self.next_ts
    }
    pub fn emit(&mut self, e: Event) {
        crate::hist::on_event(self, &e);
        // Discards are announced per card; `ext6::flush_discards` batches them after the move.
        if let Event::Discard { player, .. } = e {
            let d = &mut self.ext.x6.discards[player.idx()];
            *d = d.saturating_add(1);
        }
        self.events.push(e);
    }
    pub fn is_sick(&self, id: ObjId) -> bool {
        let o = self.obj(id);
        self.player(o.controller).last_turn_started <= o.ctrl_since
    }

    /// Move an object to a new zone, creating a new object (CR 400.7). Returns the new id.
    /// `to_top`: for library moves, put on top (else bottom). Low-level: no replacements or
    /// triggers (use `zones::move_to` for game actions).
    pub fn move_obj(&mut self, id: ObjId, to: Zone, to_top: bool) -> ObjId {
        let nid = self.move_obj_limbo(id, to);
        if self.obj(nid).zone != Zone::Gone {
            self.place(nid, to_top);
        }
        nid
    }

    /// Create the new object for a zone change without adding it to the zone's list yet (so
    /// "as this enters" replacements can see the battlefield without it).
    pub fn move_obj_limbo(&mut self, id: ObjId, to: Zone) -> ObjId {
        let old = self.obj(id).clone();
        // Effect objects that remember this object forget it / end when it moves (Forge
        // ForgetOnMoved / ExileOnMoved); "as long as in play" effects end with their source.
        let from = old.zone;
        self.effect_objs.retain_mut(|e| {
            if e.remembered.contains(&id) {
                if e.exile_on_moved.has(from) {
                    return false;
                }
                if e.forget_on_moved.has(from) {
                    e.remembered.retain(|r| *r != id);
                }
            }
            !(e.had_remembered && e.remembered.is_empty()) && !(e.source == id && from == Zone::Battlefield && e.duration == crate::carddb::EffDuration::AsLongAsInPlay)
        });
        if from == Zone::Battlefield {
            crate::ext5::note_attachments_leaving(self, id);
        }
        self.remove_from_zone(id);
        self.obj_mut(id).zone = Zone::Gone;
        // A spell entering the battlefield is still on the stack while it enters (608.3a; cleared
        // by `zones` as it is placed); any other move takes it off the stack.
        if from == Zone::Stack && to != Zone::Battlefield {
            self.spell_left_stack(id);
        }
        // Casting info: the permanent a spell becomes keeps part of it; otherwise it's dropped.
        let carried = if self.cast_info.is_empty() {
            None
        } else {
            let c = self.cast_of(id);
            // The old object (now `Gone`) keeps how it was cast as last known information
            // until the turn ends (CR 113.7a, 608.2h: Deceit's "if {B}{B} was spent
            // to cast it" is checked again on resolution, 603.4, after Deceit died).
            let lki = from == Zone::Battlefield || (from == Zone::Stack && to != Zone::Battlefield);
            if !lki {
                self.cast_info.retain(|e| e.0 != id);
            } else if from == Zone::Stack {
                // A warped spell that never became a permanent has no delayed exile trigger
                // (CR 702.185a: "the permanent this spell becomes"; `altcast::end_step_begins`).
                if let Some(e) = self.cast_info.iter_mut().find(|e| e.0 == id) {
                    e.1.flags &= !crate::altcast::flag::WARP;
                }
            }
            (from == Zone::Stack && to == Zone::Battlefield).then(|| c.on_battlefield()).filter(|c| *c != Default::default())
        };
        if to == Zone::Gone {
            return id; // removed from its zone above; no new object
        }
        let nid = ObjId(self.objs.len() as u32);
        if let Some(l) = &mut self.lineage {
            l.push((nid, id));
        }
        if let Some(c) = carried {
            self.cast_info.push((nid, c));
        }
        let ts = self.ts();
        let turn = self.turn.number;
        let owner = old.owner;
        // A copy effect ends when the permanent leaves the battlefield (CR 400.7, 707.2).
        let mut old = old;
        if let Some(i) = self.copy_effects.iter().position(|c| c.obj == id) {
            old.card = self.copy_effects.remove(i).printed;
        }
        // A face-down permanent leaving the battlefield is the real card again, revealed (CR
        // 708.9); its last known information stays face down (the old object keeps its card).
        if from == Zone::Battlefield && !self.ext.x6.face_down.is_empty() {
            if let Some(i) = self.ext.x6.face_down.iter().position(|x| x.0 == id) {
                old.card = self.ext.x6.face_down.remove(i).1;
            }
        }
        let mut known_to = match to {
            // Into a library (a face-down pile, CR 401.2): whoever saw the card go to its
            // position knows it (`observe` `library_known`). Random placements and
            // shuffles forget it (`flow::shuffle_library`, `forget_arranged`); several cards put
            // into one library at once are arranged by their owner (401.4, `zones::move_batch`).
            Zone::Library => match from {
                // From a public zone (400.2), in plain sight (603.10a: "an object that all
                // players can see is put into a hand or library"); a face-down permanent or
                // spell is revealed as it leaves (708.9).
                Zone::Battlefield | Zone::Graveyard | Zone::Stack => 0b11,
                // Exile: face up, both; face down (406.3), whoever may look at it.
                Zone::Exile => old.known_to,
                // From the owner's hand or from outside the game: the owner chose it (mulligan
                // bottoms, 103.5; "put a card from your hand on top"); the other player can't
                // tell which card it is.
                Zone::Hand | Zone::Sideboard => 1 << owner.0,
                // Moved within the library: whoever had looked at it.
                Zone::Library => old.known_to,
                Zone::Gone => 0,
            },
            // Cards moving from a public zone into a hand are known to both players. From a
            // library: the owner only here; a move in plain sight keeps what the others knew
            // (`seen_into_hand`: draws).
            Zone::Hand => {
                if matches!(old.zone, Zone::Library | Zone::Sideboard) {
                    1 << owner.0
                } else {
                    0b11
                }
            }
            // Exiled face down (CR 406.3, `ext3::exile_face_down`): nobody may look at it.
            Zone::Exile if self.ext.lt.face_down_next => 0,
            _ => 0b11,
        };
        // Entering face down (manifest, `ext6::manifest`).
        let mut card = old.card;
        if to == Zone::Battlefield {
            if let Some((p, fd)) = self.ext.x6.face_down_next.take() {
                self.ext.x6.face_down.push((nid, card));
                card = fd;
                known_to = 1 << p.0;
            }
        }
        self.objs.push(Obj {
            card,
            owner,
            controller: owner,
            zone: to,
            tapped: false,
            ctrl_since: turn,
            damage: 0,
            deathtouched: false,
            counters: SmallVec::new(),
            ts,
            token: old.token,
            known_to,
            entered: turn,
            cast_by: if old.zone == Zone::Stack && to == Zone::Battlefield { Some(old.controller) } else { None },
            cast_mana: if old.zone == Zone::Stack && to == Zone::Battlefield { old.cast_mana } else { 0 },
            face: 0,
            on_adventure: false,
            attached_to: None,
            prepared: false,
        });
        nid
    }

    /// Add an object (created by `move_obj_limbo`) to its zone's list.
    pub fn place(&mut self, nid: ObjId, to_top: bool) {
        let (owner, to) = (self.obj(nid).owner, self.obj(nid).zone);
        match to {
            Zone::Library => {
                let lib = &mut self.player_mut(owner).library;
                if to_top {
                    lib.push(nid)
                } else {
                    lib.insert(0, nid)
                }
            }
            Zone::Hand => self.player_mut(owner).hand.push(nid),
            Zone::Graveyard => self.player_mut(owner).graveyard.push(nid),
            Zone::Battlefield => self.battlefield.push(nid),
            Zone::Exile => self.exile.push(nid),
            Zone::Sideboard => self.player_mut(owner).sideboard.push(nid),
            Zone::Stack | Zone::Gone => {}
        }
    }

    /// `looker` looked at `cards` (in a library) and put them back or away in an order only
    /// they choose (scry, surveil, "look at the top N ... the rest on the bottom in any / a
    /// random order", CR 701.22a, 401.4): with two or more cards, the other player no longer
    /// knows where any of them is, even one they knew (a card put on top in plain sight).
    /// With one card its new place is public. The looker's own knowledge is untouched
    /// (a random order keeps the looker's positions).
    pub fn forget_arranged(&mut self, looker: PlayerId, cards: &[ObjId]) {
        if cards.len() > 1 {
            for &o in cards {
                self.obj_mut(o).known_to &= 1 << looker.0;
            }
        }
    }

    /// A library card put into its owner's hand in plain sight of its position (a draw takes the
    /// top card, CR 121.1; one looked-at top card put into the hand): whoever knew which card it
    /// was (`seen`, its `known_to` in the library: put there in plain sight, revealed)
    /// saw it go there and still knows it (perfect recall). `move_obj_limbo` alone gives
    /// a library card entering a hand its owner only, right for every private pick (searches,
    /// a choice among several looked-at cards: `forget_arranged`).
    pub fn seen_into_hand(&mut self, nid: ObjId, seen: u8) {
        if self.obj(nid).zone == Zone::Hand {
            self.obj_mut(nid).known_to |= seen;
        }
    }

    /// Every spell and ability on the stack (CR 405.1), bottom first: the items waiting to resolve
    /// and the one resolving now (`resolving`, still on the stack until 608.2n / 608.3a). Items put
    /// on the stack during that resolution (a spell cast as it resolves, CR 608.2g) are above it:
    /// uids grow with every push (`push_stack`). Readers asking "is it on the stack" use this;
    /// `stack` alone means "waiting to resolve" (what priority, targeting and "the top of the
    /// stack" read: nobody can target or respond to the resolving item, 608.2).
    pub fn stack_items(&self) -> impl DoubleEndedIterator<Item = &StackItem> + Clone {
        let k = self.resolving.as_ref().map_or(self.stack.len(), |r| self.stack.iter().position(|it| it.uid > r.uid).unwrap_or(self.stack.len()));
        self.stack[..k].iter().chain(self.resolving.iter()).chain(self.stack[k..].iter())
    }

    /// The stack item of the spell `obj` (waiting or resolving), the topmost if several.
    pub fn stack_item_of(&self, obj: ObjId) -> Option<&StackItem> {
        self.stack_items().rev().find(|it| it.source == obj && it.kind == StackKind::Spell)
    }

    /// The spell `obj` left the stack zone: if it is the resolving item, that resolution's
    /// object is no longer on the stack (CR 608.2n: put into a graveyard / exiled / ceased to
    /// exist; 608.3: entered the battlefield; 608.2b: countered on resolution).
    pub fn spell_left_stack(&mut self, obj: ObjId) {
        if self.resolving.as_ref().is_some_and(|r| r.source == obj && r.kind == StackKind::Spell) {
            self.resolving = None;
            self.becoming = None;
        }
    }

    /// The spell `obj` started entering the battlefield as `nid` (`move_obj_limbo`; placed
    /// later): if it is the resolving spell, record what it becomes (`becoming`, 608.3a).
    pub fn spell_entering(&mut self, obj: ObjId, nid: ObjId) {
        if self.resolving.as_ref().is_some_and(|r| r.source == obj && r.kind == StackKind::Spell) {
            self.becoming = Some(nid);
        }
    }

    /// Put a spell or ability on top of the stack with a fresh `StackItem::uid`; returns it.
    pub fn push_stack(&mut self, mut it: StackItem) -> u32 {
        self.ext.stack_uids += 1;
        it.uid = self.ext.stack_uids;
        self.stack.push(it);
        self.ext.stack_uids
    }

    /// Create a token object in limbo on the battlefield (caller places it).
    pub fn new_token(&mut self, card: CardId, owner: PlayerId) -> ObjId {
        let nid = ObjId(self.objs.len() as u32);
        let ts = self.ts();
        let turn = self.turn.number;
        self.objs.push(Obj {
            card,
            owner,
            controller: owner,
            zone: Zone::Battlefield,
            tapped: false,
            ctrl_since: turn,
            damage: 0,
            deathtouched: false,
            counters: SmallVec::new(),
            ts,
            token: true,
            known_to: 0b11,
            entered: turn,
            cast_by: None,
            face: 0,
            on_adventure: false,
            attached_to: None,
            prepared: false,
            cast_mana: 0,
        });
        nid
    }

    fn remove_from_zone(&mut self, id: ObjId) {
        let o = self.obj(id);
        let (owner, zone) = (o.owner, o.zone);
        let rm = |v: &mut Vec<ObjId>| {
            if let Some(i) = v.iter().position(|&x| x == id) {
                v.remove(i);
            }
        };
        match zone {
            Zone::Library => rm(&mut self.player_mut(owner).library),
            Zone::Hand => rm(&mut self.player_mut(owner).hand),
            Zone::Graveyard => rm(&mut self.player_mut(owner).graveyard),
            Zone::Battlefield => {
                rm(&mut self.battlefield);
                self.effects.retain(|e| e.affected != id);
                self.combat.attackers.retain(|a| a.0 != id);
                // CR 506.4: a planeswalker that leaves is removed from combat; its attackers
                // keep attacking but deal no combat damage (506.4c).
                self.combat.attacking_pw.retain(|a| a.0 != id);
                self.combat.blocks.retain(|b| b.0 != id && b.1 != id);
            }
            Zone::Exile => rm(&mut self.exile),
            Zone::Sideboard => rm(&mut self.player_mut(owner).sideboard),
            Zone::Stack | Zone::Gone => {}
        }
    }

    /// Record that a game number left the bound (`num`); the first reason is kept.
    pub fn note_overflow(&mut self, what: &str, value: i64) {
        if self.overflow.is_none() {
            self.overflow = Some(format!("game number out of range: {what} = {value} (bound ±{})", crate::num::NUM_MAX));
        }
    }
    /// `v` if it is within the bound (`num::NUM_MAX`), else records the overflow and returns
    /// it clamped to the bound. For amounts an effect is about to use and values it stores.
    pub fn num_check(&mut self, v: i64, what: &str) -> i32 {
        let m = crate::num::NUM_MAX as i64;
        if v.abs() > m {
            self.note_overflow(what, v);
        }
        v.clamp(-m, m) as i32
    }
    /// `cur + delta` for a stored game number (life, damage, counters, ...): the amount and the
    /// result are both checked (`num_check`).
    pub fn num_add(&mut self, cur: i32, delta: i64, what: &str) -> i32 {
        let d = self.num_check(delta, what);
        self.num_check(cur as i64 + d as i64, what)
    }

    /// Library top is the end of the vector.
    pub fn library_top(&self, p: PlayerId) -> Option<ObjId> {
        self.player(p).library.last().copied()
    }
}

impl Player {
    /// `*self = o.clone()` reusing allocations. Exhaustive destructuring: a new `Player` field
    /// must be added here to compile (the sideboard was once missed, so the engine's reused
    /// scratch state kept a stale outside-the-game list: a wished card stayed listed there).
    fn copy_from(&mut self, o: &Player) {
        let Player { life, library, hand, graveyard, lands_played, drew_from_empty, poison, last_turn_started, mulligans, kept, sideboard } = o;
        self.life = *life;
        self.library.clone_from(library);
        self.hand.clone_from(hand);
        self.graveyard.clone_from(graveyard);
        self.lands_played = *lands_played;
        self.drew_from_empty = *drew_from_empty;
        self.poison = *poison;
        self.last_turn_started = *last_turn_started;
        self.mulligans = *mulligans;
        self.kept = *kept;
        self.sideboard.clone_from(sideboard);
    }
}

impl State {
    /// Equal in everything except `turn.priority` and `turn.passes`. Those two are read only by
    /// the priority loop in `flow.rs` (enforced by a test in `query.rs`), so every derived
    /// query (characteristics, filters, conditions, mana sources) agrees on two states that
    /// are `eq_but_priority`; the characteristics memo is reused across them (`query::frozen`).
    /// Exhaustive destructuring: a new `State` field must be added here to compile.
    pub fn eq_but_priority(&self, o: &State) -> bool {
        let State { rng, stage, players, objs, battlefield, exile, stack, turn, combat, effects, effect_objs, pending_triggers, next_ts, starting_player, result, turn_stats, cast_info, events, ext, hist, copy_effects, lineage, overflow, resolving, becoming, being_put } = self;
        let Turn { number, active, step, tba_done, priority: _, passes: _ } = turn;
        // Cheap and most often different first.
        *number == o.turn.number
            && *active == o.turn.active
            && *step == o.turn.step
            && *tba_done == o.turn.tba_done
            && *next_ts == o.next_ts
            && objs.len() == o.objs.len()
            && battlefield == &o.battlefield
            && stack == &o.stack
            && effects == &o.effects
            && players == &o.players
            && rng == &o.rng
            && stage == &o.stage
            && exile == &o.exile
            && combat == &o.combat
            && effect_objs == &o.effect_objs
            && pending_triggers == &o.pending_triggers
            && starting_player == &o.starting_player
            && result == &o.result
            && turn_stats == &o.turn_stats
            && cast_info == &o.cast_info
            && events == &o.events
            && ext == &o.ext
            && copy_effects == &o.copy_effects
            && hist == &o.hist
            && lineage == &o.lineage
            && overflow == &o.overflow
            && resolving == &o.resolving
            && becoming == &o.becoming
            && being_put == &o.being_put
            && objs == &o.objs
    }

    /// `*self = o.clone()` but reusing this state's allocations (hot path: one per engine unit).
    /// Must list every field (exhaustive destructuring here and in `Player::copy_from`; a test
    /// checks the result against `clone`).
    pub fn copy_from(&mut self, o: &State) {
        let State { rng, stage, players, objs, battlefield, exile, stack, turn, combat, effects, effect_objs, pending_triggers, next_ts, starting_player, result, turn_stats, cast_info, events, ext, hist, copy_effects, lineage, overflow, resolving, becoming, being_put } = o;
        self.copy_effects.clone_from(copy_effects);
        self.rng = *rng;
        self.stage = *stage;
        self.players[0].copy_from(&players[0]);
        self.players[1].copy_from(&players[1]);
        self.objs.clone_from(objs);
        self.battlefield.clone_from(battlefield);
        self.exile.clone_from(exile);
        self.stack.clone_from(stack);
        self.turn.clone_from(turn);
        self.combat.clone_from(combat);
        self.effects.clone_from(effects);
        self.effect_objs.clone_from(effect_objs);
        self.pending_triggers.clone_from(pending_triggers);
        self.next_ts = *next_ts;
        self.starting_player = *starting_player;
        self.result = *result;
        self.turn_stats.clone_from(turn_stats);
        self.cast_info.clone_from(cast_info);
        self.events.clone_from(events);
        self.ext.clone_from(ext);
        self.hist.clone_from(hist);
        self.lineage.clone_from(lineage);
        self.overflow.clone_from(overflow);
        self.resolving.clone_from(resolving);
        self.becoming = *becoming;
        self.being_put.clone_from(being_put);
    }
}
