//! Extra per-game bookkeeping for choice-dependent and linked effects: chosen types and names
//! (CR 614.12 "as enters" choices, 201.3), "exiled with" links (CR 607.2a), "can't be countered"
//! marks, delayed triggered abilities (CR 603.7), and per-turn history used by Forge's
//! `Count$ThisTurnCast_...` / `ThisTurnEntered_...` expressions.
//!
//! Kept in one struct so `State` stays plain data and cheap to clone (all lists are usually
//! empty).

use crate::basics::*;
use crate::carddb::AbilityId;
use crate::state::*;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// When a delayed triggered ability triggers (CR 603.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DelayedWhen {
    /// `obj` leaves the battlefield (dies or is exiled, per `to`).
    Leaves { obj: ObjId, to: crate::carddb::ZoneSet },
    /// The beginning of the next `step` (any player's turn).
    NextStep { step: Step },
    /// The next `step` of `player`'s turn ("at the beginning of your next end step"; `ext5`).
    PlayersNextStep { step: Step, player: PlayerId },
    /// `obj` dies during turn `turn` ("when it dies this turn", `ext6`).
    DiesWithin { obj: ObjId, turn: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelayedTrig {
    pub when: DelayedWhen,
    pub controller: PlayerId,
    pub source: ObjId,
    pub source_card: CardId,
    pub execute: AbilityId,
    /// Objects the delayed ability refers to (`RememberObjects$`).
    pub remembered: SmallVec<[ObjId; 2]>,
}

/// A card exiled "until X leaves the battlefield" or "with X" (CR 610.3, 607.2a).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExiledWith {
    pub obj: ObjId,
    pub source: ObjId,
    /// Return it to the battlefield / its owner's hand when `source` leaves the battlefield.
    pub until_leaves: Option<Zone>,
}

/// The "exiled with" links (CR 607.2a) and "exiled until it leaves" links (610.3a). The list is
/// private: a link outlives the object it names, which may have left exile (a new
/// object, 400.7) or ceased to exist (a token, 111.7), so "the cards exiled with [this]" must be
/// asked through `still_exiled_with` (manifest dread read a token's stale link).
/// `is_linked` answers for one given object, wherever it is (`ExiledWithSource` filters test
/// objects in the zone they search). Serialized as the plain list it wraps.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExiledLinks(Vec<ExiledWith>);

impl ExiledLinks {
    /// Record that `obj` was exiled with `source` (returned when `source` leaves the battlefield
    /// if `until_leaves`).
    pub fn link(&mut self, obj: ObjId, source: ObjId, until_leaves: Option<Zone>) {
        self.0.push(ExiledWith { obj, source, until_leaves });
    }
    /// Was the object `o` (this object id) exiled with `src`?
    pub fn is_linked(&self, o: ObjId, src: ObjId) -> bool {
        self.0.iter().any(|e| e.obj == o && e.source == src)
    }
    /// The objects exiled with `src` that are still in exile (CR 607.2a, 400.7, 111.7), in the
    /// order they were linked: every "cards exiled with this" reader.
    pub fn still_exiled_with<'a>(&'a self, objs: &'a ObjVec, src: ObjId) -> impl Iterator<Item = ObjId> + 'a {
        self.0.iter().filter(move |e| e.source == src && objs[e.obj.idx()].zone == Zone::Exile).map(|e| e.obj)
    }
    /// The source the object `o` in exile was (first) exiled with; None outside exile.
    pub fn source_of(&self, objs: &ObjVec, o: ObjId) -> Option<ObjId> {
        if objs[o.idx()].zone != Zone::Exile {
            return None;
        }
        self.0.iter().find(|e| e.obj == o).map(|e| e.source)
    }
    /// Is anything exiled until a permanent leaves the battlefield (CR 610.3a)?
    pub fn any_until_leaves(&self) -> bool {
        self.0.iter().any(|e| e.until_leaves.is_some())
    }
    /// Remove and return the "until it leaves" links of `srcs` (they return once, 610.3a); the
    /// caller returns those still in exile.
    pub fn take_until_leaves(&mut self, srcs: &[ObjId]) -> Vec<ExiledWith> {
        let back: Vec<ExiledWith> = self.0.iter().copied().filter(|e| srcs.contains(&e.source) && e.until_leaves.is_some()).collect();
        if !back.is_empty() {
            self.0.retain(|e| !(srcs.contains(&e.source) && e.until_leaves.is_some()));
        }
        back
    }
    /// Every link, stale ones included: only for the objects a state refers to (`query::refs_of`
    /// merge keys), never for "which cards are exiled with X".
    pub fn all_links(&self) -> &[ExiledWith] {
        &self.0
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ext {
    /// Creature/land type chosen for a permanent as it entered (`ChooseType`).
    pub chosen_type: SmallVec<[(ObjId, Subtype); 2]>,
    /// Card name chosen for a permanent or resolving ability (`NameCard`).
    pub named: SmallVec<[(ObjId, u16); 1]>,
    /// Cards chosen by a resolving ability (`ChooseCard`), keyed by its source; cleared after
    /// the ability resolves.
    pub chosen_cards: SmallVec<[(ObjId, ObjId); 2]>,
    /// Spells that can't be countered (CR 101.2: "can't" beats "can").
    pub uncounterable: SmallVec<[ObjId; 1]>,
    /// Private behind `ExiledLinks`' methods (stale links: `still_exiled_with`).
    pub exiled_with: ExiledLinks,
    /// Targets of the ability node currently resolving (`TargetedPlayerCtrl`-style filters).
    pub res_targets: SmallVec<[Target; 2]>,
    pub delayed: Vec<DelayedTrig>,
    /// Spells cast this turn: (caster, spell object as it was on the stack).
    pub cast_this_turn: SmallVec<[(PlayerId, ObjId); 4]>,
    /// Zone changes this turn: (object before, from, to, new object).
    pub moved_this_turn: Vec<(ObjId, Zone, Zone, ObjId)>,
    /// Mana types spent to cast a spell (bitmask by `ManaType`), for converge.
    pub spent_colors: SmallVec<[(ObjId, u8); 2]>,
    /// Activations this turn of abilities with an activation limit: (source, ability, count).
    pub activations: SmallVec<[(ObjId, crate::carddb::AbilityId, u8); 2]>,
    /// Permanents sacrificed this turn: (player who sacrificed, object as it last existed).
    pub sacrificed_this_turn: SmallVec<[(PlayerId, ObjId); 2]>,
    /// X paid for permanent spells that resolved (CR 107.3m), by the permanent.
    pub x_paid: SmallVec<[(ObjId, i32); 1]>,
    /// The player the current `RepeatEach` repetition is for (`Remembered` player).
    pub rep_player: Option<PlayerId>,
    /// Solved Cases (CR 719.2).
    pub solved: SmallVec<[ObjId; 1]>,
    /// Players who searched their library this turn (bitmask), for tests and conditions.
    pub searched: u8,
    /// Mana pools (CR 106.4), by player and `ManaType`, unrestricted mana (restricted mana:
    /// `x6.rpool`): mana added by resolving effects (Flamecache Gecko) and mana a mana ability
    /// made beyond a payment. Empties at the end of each step and phase (CR 500.5).
    pub pool: [[u8; 6]; 2],
    /// Speed (CR 702.179): 0 = no speed; else 1–4.
    pub speed: [u8; 2],
    /// Turn number in which each player's speed last increased (once per turn, 702.179d).
    pub speed_turn: [u32; 2],
    /// Players with an enduring story (storied, bitmask by player).
    pub enduring_story: u8,
    /// Harnessed permanents (`AlterAttribute | Attributes$ Harnessed`).
    pub harnessed: SmallVec<[ObjId; 1]>,
    /// Equip abilities each player activated this turn (Kíli the Resourceful).
    pub equips_this_turn: [u8; 2],
    /// Free activations this turn by (permanent, ability), capped by `loops::FREE_ACTIVATION_CAP`.
    #[serde(default)]
    pub free_activations: SmallVec<[(ObjId, AbilityId, u8); 1]>,
    /// Objects about to enter the battlefield with extra counters (`WithCountersType$`, CR
    /// 122.6): (object before the move, kind, amount); consumed as they enter.
    pub enter_with: SmallVec<[(ObjId, CounterKind, i32); 1]>,
    /// While an ability node with `WithCountersType$` resolves: every object it puts onto the
    /// battlefield enters with these counters (`resolve::apply_node`, CR 122.6).
    #[serde(default)]
    pub enter_with_node: Option<(CounterKind, i32)>,
    /// Permanents sacrificed to pay costs of abilities not yet resolved: (ability source,
    /// sacrificed object as it last existed), for `Sacrificed$...` (Repurposing Bay).
    pub cost_sacrificed: SmallVec<[(ObjId, ObjId); 1]>,
    /// Objects remembered by the ability node currently resolving (`Remembered$Valid`).
    pub res_remembered: SmallVec<[ObjId; 4]>,
    /// The simultaneous moves being processed by `zones::move_batch` (for `ChangesZoneAll`
    /// triggers): (object before the move, from, to). Empty outside of it.
    pub batch: SmallVec<[(ObjId, Zone, Zone); 4]>,
    /// Exhaust abilities already activated (CR 702.177a), by object; never reset.
    pub exhausted: SmallVec<[(ObjId, crate::carddb::AbilityId); 1]>,
    /// Graveyard cards that have flashback until end of turn (granted, cost = mana cost).
    pub granted_flashback: SmallVec<[ObjId; 1]>,
    /// Division of a spell's damage/counters among its targets (CR 601.2d): (spell, target,
    /// amount); dropped when the spell leaves the stack.
    pub divided: SmallVec<[(ObjId, Target, i32); 2]>,
    /// Committed a crime this turn (CR 700.13), bitmask by player.
    pub crimes: u8,
    /// State of the third long-tail batch (`ext3`).
    pub lt: crate::ext3::LtState,
    /// While a player orders their triggers (CR 603.3b, `DecisionKind::OrderTriggers`): the
    /// triggers not yet put on the stack, which `Opt::Trigger(i)` indexes. Display only (the
    /// text front-end names the options); empty otherwise.
    #[serde(default)]
    pub ordering: Vec<crate::state::PendingTrigger>,
    /// State of the fourth long-tail batch (`ext4`).
    #[serde(default)]
    pub mr: crate::ext4::MrState,
    /// State of the fifth long-tail batch (`ext5`).
    #[serde(default)]
    pub le: crate::ext5::LeState,
    /// State of the sixth long-tail batch (`ext6`).
    #[serde(default)]
    pub x6: crate::ext6::XState,
    /// State of the seventh long-tail batch (`ext7`).
    #[serde(default)]
    pub x7: crate::ext7::X7State,
    /// State of the ninth batch (`ext9`): firebending mana.
    #[serde(default)]
    pub x9: crate::ext9::X9State,
    /// Last known information of permanents that left the battlefield (`lki`).
    #[serde(default)]
    pub lki: crate::lki::LkiState,
    /// While one source deals damage to several recipients at once (`resolve::hold_lifelink`):
    /// (source, its controller once it dealt lifelink damage, life to gain), one life gain event
    /// per source when released (CR 119.9, 120.3f, 702.15e). Empty outside of it.
    #[serde(default)]
    pub lifelink_held: SmallVec<[(ObjId, Option<PlayerId>, i32); 2]>,
    /// The last `StackItem::uid` given (`State::push_stack`).
    #[serde(default)]
    pub stack_uids: u32,
    /// Cards exiled on an adventure whose Adventure spell's controller wasn't their owner: (card
    /// in exile, that controller), who may play it (CR 715.3d; `cast::adventure_player`). Empty
    /// when every Adventure was cast by its owner.
    #[serde(default)]
    pub adventure_player: SmallVec<[(ObjId, PlayerId); 0]>,
    /// While a player decides whether to pay an "unless" cost (`DecisionKind::PayUnless`,
    /// `resolve`): what it costs, as evaluated for this payment (encoding v10.2,
    /// `Observation::pay_unless`). Left set in the paused scratch state (`Halt::Need`) for the
    /// view, like `ordering`; None otherwise (never in a committed state).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unless_due: Option<crate::observe::UnlessView>,
}

impl Ext {
    pub fn chosen_type_of(&self, o: ObjId) -> Option<Subtype> {
        self.chosen_type.iter().find(|x| x.0 == o).map(|x| x.1)
    }
    /// Index into `name_pool::names()` of the name chosen for `o`.
    pub fn named_of(&self, o: ObjId) -> Option<u16> {
        self.named.iter().find(|x| x.0 == o).map(|x| x.1)
    }
    pub fn exiled_with_src(&self, o: ObjId, src: ObjId) -> bool {
        self.exiled_with.is_linked(o, src)
    }
    /// Reset per-turn history (CR 500: as a turn begins).
    pub fn new_turn(&mut self) {
        self.cast_this_turn.clear();
        self.moved_this_turn.clear();
        self.searched = 0;
        self.crimes = 0;
        self.divided.clear(); // the stack is empty between turns
        self.granted_flashback.clear();
        self.activations.clear();
        self.sacrificed_this_turn.clear();
        // The stack is empty between turns, so no spell is mid-cast or on the stack.
        self.spent_colors.clear();
        self.uncounterable.clear();
        self.equips_this_turn = [0; 2];
        self.free_activations.clear();
        self.lt.dies_exile.clear();
        self.lt.next_uncounterable = 0;
        self.lt.sneak.clear();
        self.mr.new_turn();
        self.le.new_turn();
        self.x6.new_turn();
        self.x7.new_turn();
        self.lki.new_turn();
        // "This turn" delayed triggers end with the turn.
        self.delayed.retain(|d| !matches!(d.when, DelayedWhen::DiesWithin { .. }));
    }
}
