//! Last known information (CR 608.2h, 113.7a) of permanents that left the battlefield, and
//! characteristic changes that apply to a permanent as it enters (CR 603.10, 614.12).
//!
//! When a permanent leaves the battlefield, the old object (`Zone::Gone`, CR 400.7) keeps its
//! card, controller and counters, but the continuous effects that applied to it end with the
//! move. Effects that ask about "it" later (a dies trigger's "its power", "create a token that's
//! a copy of it") use its characteristics as it last existed on the battlefield, so `zones`
//! snapshots them (before any object of the same event moves, CR 603.10a) and
//! `query::chars` answers from the snapshot for that old object.
//!
//! Snapshots are dropped when a turn begins (the stack is empty then; per-turn history that
//! refers to old objects is reset at the same time). A later question about an object whose
//! snapshot was dropped gets the object's printed characteristics, as before snapshots existed.

use crate::basics::*;
use crate::carddb::AbilityId;
use crate::query::Chars;
use crate::state::*;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// Characteristics of a permanent as it last existed on the battlefield.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lki {
    /// The old object (now `Zone::Gone`).
    pub obj: ObjId,
    pub chars: Chars,
    /// Its copy effect (CR 707.2), for a token copy of it made from this information (111.12).
    pub copy: Option<CopyEffect>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LkiState {
    /// Snapshots of permanents that left the battlefield this turn. Shared between state copies
    /// (one per engine unit and search clone) until one records a new snapshot.
    pub left: std::sync::Arc<Vec<Lki>>,
    /// The old objects in `left`, same order, kept inline: the check in `query::chars` (hot) is
    /// then one length test while there are none.
    pub ids: SmallVec<[ObjId; 4]>,
    /// While a "return it to the battlefield ... It's an [type]" effect moves objects
    /// (`ChangeZone | StaticEffect$`): that type change (the `AnimateMoved` node) applies to them
    /// as they enter (CR 614.12, 603.10), with the objects it was applied to.
    pub enter_animate: Option<(AbilityId, SmallVec<[ObjId; 1]>)>,
}

impl LkiState {
    pub fn new_turn(&mut self) {
        if !self.ids.is_empty() {
            self.left = Default::default();
            self.ids.clear();
        }
    }
}

/// The snapshot of `id`, if it is an old object of a permanent that left the battlefield.
#[inline]
pub fn of(s: &State, id: ObjId) -> Option<&Lki> {
    let ids = &s.ext.lki.ids;
    if ids.is_empty() || s.obj(id).zone != Zone::Gone {
        return None;
    }
    ids.iter().position(|&o| o == id).map(|i| &s.ext.lki.left[i])
}

/// Record `id`'s last known information (it is still on the battlefield).
pub fn record(s: &mut State, id: ObjId, chars: Chars) {
    let copy = s.copy_effects.iter().find(|c| c.obj == id).cloned();
    let left = std::sync::Arc::make_mut(&mut s.ext.lki.left);
    if let Some(i) = s.ext.lki.ids.iter().position(|&o| o == id) {
        left.remove(i);
        s.ext.lki.ids.remove(i);
    }
    left.push(Lki { obj: id, chars, copy });
    s.ext.lki.ids.push(id);
}

/// `nid` is entering the battlefield while a "return it ... It's an [type]" effect is moving
/// objects: it gets that type change now, as it enters (see `LkiState::enter_animate`).
pub fn animate_entering(s: &mut State, db: &crate::carddb::CardDb, nid: ObjId) {
    let Some((a, done)) = &mut s.ext.lki.enter_animate else { return };
    let crate::carddb::Effect::Ext(crate::effect_ext::ExtEffect::AnimateMoved { add_types, remove_card_types, add_subtypes, set_pt }) = &db.ability(*a).effect else {
        unreachable!("enter_animate holds an AnimateMoved node")
    };
    done.push(nid);
    let (add_types, remove_card_types, add_subtypes, set_pt) = (*add_types, *remove_card_types, add_subtypes.iter().copied().collect(), *set_pt);
    let ts = s.ts();
    s.effects.push(ContEffect { ts, affected: nid, layer: Layer::L6, add_types, remove_card_types, add_subtypes, set_pt, ..Default::default() });
}
