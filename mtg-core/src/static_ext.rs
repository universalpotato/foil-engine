//! Static abilities added after the core set: rule-modifying statics ("can't" effects, CR
//! 101.2), extra land plays (CR 305.2), triggers disabled (Torpor Orb), "can't be countered",
//! damage increases, and self type-changing (layer 4) with intrinsic land mana (CR 305.6).

use crate::basics::*;
use crate::carddb::*;
use crate::filter::Filter;
use crate::query::{obj_matches, FilterCtx};
use crate::state::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StaticExt {
    /// Layer 4 on the permanent itself: it is the chosen (basic land) type, losing other land
    /// types (`AffectedDefined$ Self | AddType$ ChosenType | RemoveLandTypes$ True`).
    SelfChosenLandType,
    /// Matching creatures can't block (CR 509.1b).
    CantBlock { valid: Filter },
    /// Activated abilities of matching permanents can't be activated (CR 602.5);
    /// `nonmana_only`: mana abilities are exempt.
    CantBeActivated { valid: Filter, nonmana_only: bool },
    /// Permanents matching `cause` entering the battlefield don't cause abilities to trigger.
    DisableEtbTriggers { cause: Filter },
    /// Matching players can't gain life (CR 119.7).
    CantGainLife { players: PlayerSel },
    /// The controller may play this many additional lands each turn (CR 305.2).
    ExtraLandPlays(u8),
    /// Matching spells can't be countered (CR 101.2).
    CantBeCountered { valid: Filter },
    /// Damage from matching sources to permanents/players is increased by `plus` (CR 614.1a).
    DamagePlus { source: Filter, plus: i32 },
    /// Matching permanents have this activated ability (layer 6, CR 613.1f).
    GrantAbility { affected: Filter, ability: AbilityId },
    /// Activated abilities cost `amount` generic less (CR 601.2f, 602.2b): abilities of
    /// permanents matching `valid` (if any), only equip abilities (`equip_only`), only if they
    /// target this permanent (`target_self`), activated by `player`.
    AbilityCostReduce { valid: Option<Filter>, equip_only: bool, target_self: bool, player: PlayerSel, amount: crate::expr::Expr },
    /// The first equip ability `player` activates each turn costs {0} instead (an alternative
    /// cost, CR 118.9), under the static's conditions (Kíli the Resourceful).
    FreeEquip { player: PlayerSel },
    /// Damage dealt to this permanent first heals all other damage on it (Wolverine).
    HealBeforeDamage,
    /// Marker: this spell costs {1} less for each permanent sacrificed for its additional cost
    /// (`FaceExtra::sac_any_reduce`, applied while casting).
    SacrificeReduces,
    /// "If you would draw one or more cards, you draw that many plus `plus` instead" (CR 614.1a),
    /// under the static's conditions (Quantum Riddler).
    DrawPlus { plus: i32 },
    /// "You have hexproof" (CR 702.11c: the static's controller can't be the target of spells or
    /// abilities their opponents control), under the static's conditions (Captain America,
    /// Super-Soldier: "as long as ~ has a shield counter on him, you ... have hexproof").
    /// Compiled from the `You` part of a `Continuous | Affected$ You,... | AddKeyword$ Hexproof`
    /// static; read by `player_has_hexproof` wherever player targets are decided.
    PlayerHexproof,
    /// Statics of the second long-tail batch (`ext2`).
    More(crate::ext2::MoreStatic),
    /// Statics of the third long-tail batch (`ext3`).
    Lt(crate::ext3::LtStatic),
    /// Statics of the fourth long-tail batch (`ext4`).
    Mr(crate::ext4::MrStatic),
    /// Statics of the fifth long-tail batch (`ext5`).
    Le(crate::ext5::LeStatic),
    /// Statics of the sixth long-tail batch (`ext6`).
    X(crate::ext6::XStatic),
    /// Layer 4 (`ext8`): "Creatures you control but don't own are [subtype] in addition to their
    /// other types" (Laughing Jasper Flint; applied in `query::chars_base_l15`).
    StolenSubtype(crate::basics::Subtype),
    /// Casting restrictions of the ninth batch (`ext9`; checked in `altcast::cant_cast`).
    X9(crate::ext9::X9Static),
}

/// The active `ext3` statics: (source, controller, static def, static).
pub fn active_lt<'a>(s: &'a State, db: &'a CardDb) -> impl Iterator<Item = (ObjId, PlayerId, &'a StaticDef, &'a crate::ext3::LtStatic)> + 'a {
    active(s, db).filter_map(|(src, you, st, x)| if let StaticExt::Lt(l) = x { Some((src, you, st, l)) } else { None })
}

/// The active `ext6` statics of permanents: (source, controller, static def, static).
pub fn active_x<'a>(s: &'a State, db: &'a CardDb) -> impl Iterator<Item = (ObjId, PlayerId, &'a StaticDef, &'a crate::ext6::XStatic)> + 'a {
    active(s, db).filter_map(|(src, you, st, x)| if let StaticExt::X(l) = x { Some((src, you, st, l)) } else { None })
}

/// Iterate (source, controller, ext static) over battlefield permanents whose abilities work.
fn active<'a>(s: &'a State, db: &'a CardDb) -> impl Iterator<Item = (ObjId, PlayerId, &'a StaticDef, &'a StaticExt)> + 'a {

    s.battlefield.iter().flat_map(move |&src| {
        let so = s.obj(src);
        db.get(so.card).face(so.face).statics.iter().filter_map(move |st| match &st.kind {
            StaticKind::Ext(x) if st.zones.has(Zone::Battlefield) => Some((src, so.controller, st, x)),
            _ => None,
        })
    })
    .filter(move |(src, ..)| !crate::query::may_lose_abilities(s, db) || !crate::query::lost_all_abilities(s, db, *src))
}

pub fn cant_block(s: &State, db: &CardDb, b: ObjId) -> bool {
    active(s, db).any(|(src, you, _, x)| matches!(x, StaticExt::CantBlock { valid } if obj_matches(s, db, valid, b, FilterCtx { you, source: Some(src), x: 0, rem: &[] })))
}

/// Can't the ability `ad` of `o` be activated (Clarion Conqueror, Petrified Hamlet)?
pub fn activation_forbidden(s: &State, db: &CardDb, o: ObjId, ad: &AbilityDef) -> bool {
    // Effect objects ("its activated abilities can't be activated for as long as it remains
    // tapped", Braided Net).
    let from_effects = s.effect_objs.iter().any(|eo| {
        // effect-object static reader: CantBeActivated (`static_ext::applies_from_effect_object`)
        eo.statics.iter().any(|&si| match &db.statics_pool[si as usize].kind {
            StaticKind::Ext(StaticExt::CantBeActivated { valid, nonmana_only }) => {
                !(*nonmana_only && ad.is_mana) && (eo.duration != EffDuration::UntilUntaps || s.obj(o).tapped) && obj_matches(s, db, valid, o, eo_fcx!(eo))
            }
            _ => false,
        })
    });
    from_effects || active(s, db).any(|(src, you, _, x)| match x {
        StaticExt::CantBeActivated { valid, nonmana_only } => !(*nonmana_only && ad.is_mana) && obj_matches(s, db, valid, o, FilterCtx { you, source: Some(src), x: 0, rem: &[] }),
        _ => false,
    })
}


/// Do triggers caused by `entering` entering the battlefield not trigger (Torpor Orb)?
pub fn etb_triggers_disabled(s: &State, db: &CardDb, entering: ObjId) -> bool {
    active(s, db).any(|(src, you, _, x)| matches!(x, StaticExt::DisableEtbTriggers { cause } if obj_matches(s, db, cause, entering, FilterCtx { you, source: Some(src), x: 0, rem: &[] })))
}

pub fn cant_gain_life(s: &State, db: &CardDb, p: PlayerId) -> bool {
    active(s, db).any(|(_, you, _, x)| matches!(x, StaticExt::CantGainLife { players } if players.matches(p, you)))
}

/// Is a static of this kind applied when an effect object (`State::effect_objs`: a resolved
/// spell's or ability's "this turn" effect, CR 611.2a) has it? Most statics are read from the
/// battlefield's permanents only; compiling `Effect | StaticAbilities$` with any other kind fails
/// the card instead of creating an effect that does nothing (Way of the Paradox's
/// extra land play was such a dropped static). Keep in sync with the readers of
/// `effect_objs[..].statics` (query::chars, block_forbidden, ability_loss_ts; cast::may_play,
/// ext7 any-type mana; altcast cant-cast; ext9::cant_cast; ext2::loyalty_instant_ok;
/// ext6::storm_instances; activation_forbidden; land_plays): each reader is marked
/// `// effect-object static reader: <variants>`, and a test checks that every
/// `statics_pool[..]` read has a marker and that the marked variants are exactly this list.
pub fn applies_from_effect_object(kind: &StaticKind) -> bool {
    use crate::ext9::X9Static;
    match kind {
        StaticKind::Continuous { .. } | StaticKind::RemoveAllAbilities { .. } | StaticKind::CantBlockBy { .. } | StaticKind::MayPlay { .. } => true,
        StaticKind::CantPreventDamage { .. } => true,
        StaticKind::Alt(crate::altcast::AltStatic::CantBeCast { .. }) => true,
        StaticKind::Ext(x) => matches!(
            x,
            StaticExt::CantBeActivated { .. }
                | StaticExt::ExtraLandPlays(_)
                | StaticExt::X9(X9Static::CastLimit { .. } | X9Static::OnlyFromHand { .. })
                | StaticExt::More(crate::ext2::MoreStatic::LoyaltyAtInstantSpeed { .. })
                | StaticExt::X(crate::ext6::XStatic::GrantStorm { .. })
        ),
        _ => false,
    }
}

/// Lands `p` may play this turn (CR 305.2): permanents' statics, and effect objects of resolved
/// abilities ("You may play an additional land this turn", Way of the Paradox: CR 611.2a).
pub fn land_plays(s: &State, db: &CardDb, p: PlayerId) -> u8 {
    let extra = |x: &StaticExt| if let StaticExt::ExtraLandPlays(n) = x { *n } else { 0 };
    // effect-object static reader: ExtraLandPlays (`static_ext::applies_from_effect_object`)
    let from_effects: u8 = s.effect_objs.iter().filter(|eo| eo.controller == p).flat_map(|eo| eo.statics.iter()).map(|&si| if let StaticKind::Ext(x) = &db.statics_pool[si as usize].kind { extra(x) } else { 0 }).sum();
    1 + from_effects + active(s, db).filter(|(_, you, ..)| *you == p).map(|(.., x)| extra(x)).sum::<u8>()
}

/// Can't the spell `o` be countered: its own "this spell can't be countered" (functions on the
/// stack) or another permanent's static (CR 101.2)?
pub fn spell_cant_be_countered(s: &State, db: &CardDb, o: ObjId) -> bool {
    let ob = s.obj(o);
    let own = db.get(ob.card).face(ob.face).statics.iter().any(|st| st.zones.has(Zone::Stack) && matches!(&st.kind, StaticKind::Ext(StaticExt::CantBeCountered { valid }) if obj_matches(s, db, valid, o, FilterCtx { you: ob.controller, source: Some(o), x: 0, rem: &[] })));
    own || active(s, db).any(|(src, you, _, x)| matches!(x, StaticExt::CantBeCountered { valid } if obj_matches(s, db, valid, o, FilterCtx { you, source: Some(src), x: 0, rem: &[] })))
}

/// Damage after "deals that much damage plus N instead" replacements (Valley Flamecaller).
pub fn modified_damage(s: &State, db: &CardDb, source: ObjId, n: i32) -> i32 {
    let mut n = n;
    for (src, you, _, x) in active(s, db) {
        if let StaticExt::DamagePlus { source: f, plus } = x {
            if obj_matches(s, db, f, source, FilterCtx { you, source: Some(src), x: 0, rem: &[] }) {
                n = crate::num::add(n, *plus);
            }
        }
    }
    n
}

/// Layer 4/6 for the self type-changing static: (subtypes, extra intrinsic mana abilities).
pub fn self_land_type(s: &State, db: &CardDb, id: ObjId, c: &mut crate::query::Chars) {
    let o = s.obj(id);
    let f = db.get(o.card).face(o.face);
    if !f.statics.iter().any(|st| matches!(st.kind, StaticKind::Ext(StaticExt::SelfChosenLandType))) {
        return;
    }
    let Some(st) = s.ext.chosen_type_of(id) else { return };
    // CR 305.7: setting a land type removes old land types and their intrinsic abilities.
    let basics: Vec<Subtype> = ["Plains", "Island", "Swamp", "Mountain", "Forest"].iter().filter_map(|n| db.subtype(n)).collect();
    c.subtypes.retain(|x| !db.land_types.contains(x));
    c.subtypes.push(st);
    if let Some(i) = basics.iter().position(|&b| b == st) {
        if let Some(&a) = db.basic_land_mana.get(i) {
            c.abilities.retain(|x| !db.basic_land_mana.contains(x));
            c.abilities.push(a);
        }
    }
}

/// Extra cards drawn by `p` because of "draw that many plus N instead" replacements (each
/// Can't damage (`combat`: combat damage) be prevented (CR 615.12)? A permanent's static ("Damage
/// can't be prevented", Sunspine Lynx; combat damage only, Frenzied Baloth) or an effect's
/// ("Damage can't be prevented this turn", Impractical Joke).
pub fn damage_cant_be_prevented(s: &State, db: &CardDb, combat: bool) -> bool {
    let applies = |k: &StaticKind| matches!(k, StaticKind::CantPreventDamage { combat_only } if combat || !*combat_only);
    let lose = crate::query::may_lose_abilities(s, db);
    let from_permanents = s.battlefield.iter().any(|&src| {
        let so = s.obj(src);
        db.get(so.card).face(so.face).statics.iter().any(|st| {
            applies(&st.kind) && st.zones.has(Zone::Battlefield) && !(lose && crate::query::lost_all_abilities(s, db, src)) && {
                let ecx = crate::expr::EvalCtx { s, db, you: so.controller, source: src, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &[], imprinted: &[], base_only: false };
                st.conditions.iter().all(|c| crate::expr::holds(c, &ecx))
            }
        })
    });
    from_permanents
        || s.effect_objs.iter().any(|eo| {
            // effect-object static reader: CantPreventDamage (`static_ext::applies_from_effect_object`)
            eo.statics.iter().any(|&si| applies(&db.statics_pool[si as usize].kind))
        })
}

/// Does player `p` have hexproof (CR 702.11c)? "You gain hexproof until end of turn" (`ext7`),
/// or a static "you have hexproof" of a permanent `p` controls whose conditions hold. Nowhere to
/// Run's "as though they didn't have hexproof" covers creatures only (its Oracle text), so it
/// doesn't apply here.
pub fn player_has_hexproof(s: &State, db: &CardDb, p: PlayerId) -> bool {
    s.ext.x7.player_hexproof & (1 << p.0) != 0
        || active(s, db).any(|(src, you, st, x)| {
            matches!(x, StaticExt::PlayerHexproof) && you == p && {
                let ecx = crate::expr::EvalCtx { s, db, you, source: src, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &[], imprinted: &[], base_only: false };
                st.conditions.iter().all(|c| crate::expr::holds(c, &ecx))
            }
        })
}

/// applies once to the event, CR 614.5; conditions are checked before anything is drawn).
pub fn extra_draws(s: &State, db: &CardDb, p: PlayerId) -> i32 {
    let mut n = 0;
    for (src, you, st, x) in active(s, db) {
        if let StaticExt::DrawPlus { plus } = x {
            if you != p {
                continue;
            }
            let ecx = crate::expr::EvalCtx { s, db, you, source: src, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &[], imprinted: &[], base_only: false };
            if st.conditions.iter().all(|c| crate::expr::holds(c, &ecx)) {
                n = crate::num::add(n, *plus);
            }
        }
    }
    n
}

/// End "for as long as it remains tapped" effects whose object is untapped (Braided Net). Run
/// with state-based actions and after untapping, so the effect ends once the object untaps.
pub fn end_untapped_effects(s: &mut State) {
    if s.effect_objs.iter().any(|e| e.duration == EffDuration::UntilUntaps) {
        let objs = &s.objs;
        s.effect_objs.retain(|e| e.duration != EffDuration::UntilUntaps || e.remembered.iter().any(|&r| objs[r.idx()].tapped && objs[r.idx()].zone == Zone::Battlefield));
    }
}

/// Does `o` heal its other damage when dealt damage (Wolverine's replacement)?
pub fn heals_before_damage(s: &State, db: &CardDb, o: ObjId) -> bool {
    let ob = s.obj(o);
    ob.zone == Zone::Battlefield
        && db.get(ob.card).face(ob.face).statics.iter().any(|st| matches!(st.kind, StaticKind::Ext(StaticExt::HealBeforeDamage)))
        && (!crate::query::may_lose_abilities(s, db) || !crate::query::lost_all_abilities(s, db, o))
}
