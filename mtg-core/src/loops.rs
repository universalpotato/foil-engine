//! Ending optional loops of free activations (CR 732, MTR 4.4): the engine's deterministic
//! stand-in for "a player can't repeat an optional loop indefinitely".
//!
//! An activated ability whose total cost to activate is nothing — no mana after reductions and
//! alternative costs (CR 118.9, 601.2f via 602.2b), no tap, sacrifice, discard, life, counter
//! or other cost — can be activated again as soon as it resolves. In the pool this happens
//! only with equip abilities: Dwarven Mauler ("equip abilities ... that target this creature
//! cost {2} less") makes equip {1}/{2} free onto it, so Equipment can shuttle between two
//! Maulers (or onto the one it already equips, which does nothing: CR 701.3b) forever. Kíli's
//! free equip is once per turn and doesn't loop by itself.
//!
//! Rule: each permanent may activate each of its abilities for free at most
//! [`FREE_ACTIVATION_CAP`] times per turn. Once reached, choices that would make the activation
//! free are no longer offered (the ability stays available where it still costs something).

use crate::carddb::{AbilityId, Cost};
use crate::cast::Req;
use crate::basics::*;
use crate::state::*;

/// Free activations of one ability of one permanent per turn. Equip is sorcery-speed (CR
/// 702.6a), so it's only activatable in the two main phases; moving an Equipment more than
/// twice per main phase only matters if something else happened in between. 4 covers one
/// move per main phase plus one re-move after an intervening action in each, and bounds a
/// turn's free activations by 4 × (abilities that can be free) instead of infinity.
pub const FREE_ACTIVATION_CAP: u8 = 4;

/// Is an activation with this cost and this mana requirement (after reductions) free?
pub fn is_free(cost: &Cost, mana: &Req) -> bool {
    mana.is_empty() && {
        let mut rest = cost.clone();
        rest.mana = Default::default();
        rest == Cost::default()
    }
}

/// Has ability `a` of `o` used up its free activations this turn?
pub fn free_capped(s: &State, o: ObjId, a: AbilityId) -> bool {
    s.ext.free_activations.iter().any(|x| x.0 == o && x.1 == a && x.2 >= FREE_ACTIVATION_CAP)
}

/// Would activating `a` of `o` with this cost/mana be a free activation no longer allowed?
pub fn blocked(s: &State, o: ObjId, a: AbilityId, cost: &Cost, mana: &Req) -> bool {
    !s.ext.free_activations.is_empty() && is_free(cost, mana) && free_capped(s, o, a)
}

/// Count a free activation (called as the costs are paid).
pub fn record(s: &mut State, o: ObjId, a: AbilityId) {
    match s.ext.free_activations.iter_mut().find(|x| x.0 == o && x.1 == a) {
        Some(x) => x.2 = x.2.saturating_add(1),
        None => s.ext.free_activations.push((o, a, 1)),
    }
}
