//! Filter properties that depend on choices and links recorded in `State::ext` (chosen type,
//! named card, chosen cards, "exiled with"), on the resolving ability's targets, and a few
//! plain properties added later. Kept separate from `filter.rs` to stay additive.

use crate::basics::*;
use crate::carddb::CardDb;
use crate::filter::{CmpOp, Prop};
use crate::query::{Chars, FilterCtx};
use crate::state::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExtProp {
    /// Has the creature/land type chosen for the filter's source (`ChosenType`).
    ChosenType,
    /// Has the name chosen for the filter's source (`NamedCard`, CR 201.3).
    NamedCard,
    /// Was chosen by the resolving ability (`ChosenCard`).
    ChosenCard,
    /// Was exiled with the filter's source (`ExiledWithSource`, CR 607.2a).
    ExiledWithSource,
    /// Controlled / owned by the player targeted by the resolving ability.
    TargetedPlayerCtrl,
    TargetedPlayerOwn,
    /// Owned by an opponent of the filter's controller.
    OppOwn,
    /// Has the same name as an object remembered by the resolving ability (`sameName`).
    SameNameAsRemembered,
    /// Has the same name as a target of the resolving ability (`TargetedCard.sameName`).
    SameNameAsTargeted,
    /// Number of counters of any kind (`counters_GE1` without a kind).
    AnyCounters(CmpOp, i32),
    /// Not the resolving ability's target (`!TargetedCard`, `NotDefinedTargeted`).
    NotTargeted,
    /// Mana value equal to X of the resolving ability (`cmcEQX` when X is xPaid).
    CmcCmpX(CmpOp),
    /// In a zone of the targeted player / of you: battlefield by control, other zones by
    /// ownership (`DefinedPlayer$` of `ChangeZoneAll`).
    InZoneOfTargetedPlayer,
    InZoneOfYou,
    /// A solved Case (CR 719.2).
    Solved,
    /// Controlled by the player of the current `RepeatEach` repetition.
    RepPlayerCtrl,
    /// Harnessed (CR 701.64b).
    Harnessed,
    /// A spell cast from a graveyard (`wasCastFromGraveyard`).
    CastFromGraveyard,
    /// A spell cast from exile (`wasCastFromExile`; `ext9`, `altcast::flag::FROM_EXILE`).
    CastFromExile,
    /// It was bargained (CR 702.166b: its bargain cost was paid; the permanent keeps how it
    /// was cast, `ext9`).
    Bargained,
    /// Has this name (`named<Name>`, Ancestral Anger; CR 201.2).
    Named(String),
    /// A permanent or spell that was cast for its warp cost (`CastSa Spell.Warp`, Full Bore;
    /// CR 702.185a).
    CastForWarp,
    /// Has an activated ability ("nonlands with activated abilities"; `ext6`): current ones,
    /// and printed ones that function from the hand or graveyard.
    HasActivated,
    /// Its gift was promised (CR 702.174k; the permanent keeps how it was cast, `ext7`).
    PromisedGift,
}

/// Parse an extended property; `Ok(None)` if `p` isn't one.
pub fn parse_ext_prop(p: &str, intern: &mut dyn FnMut(&str) -> Option<Subtype>, _svar: &dyn Fn(&str) -> Option<crate::expr::Expr>) -> Result<Option<Prop>, String> {
    Ok(Some(match p {
        "Outlaw" => {
            // CR 700.12
            let mut v = vec![];
            for t in ["Assassin", "Mercenary", "Pirate", "Rogue", "Warlock"] {
                v.push(intern(t).ok_or_else(|| format!("subtype {t}"))?);
            }
            Prop::AnySubtype(v)
        }
        "ChosenType" => Prop::Ext(ExtProp::ChosenType),
        "NamedCard" => Prop::Ext(ExtProp::NamedCard),
        "ChosenCard" => Prop::Ext(ExtProp::ChosenCard),
        "ExiledWithSource" => Prop::Ext(ExtProp::ExiledWithSource),
        "TargetedPlayerCtrl" => Prop::Ext(ExtProp::TargetedPlayerCtrl),
        "TargetedPlayerOwn" => Prop::Ext(ExtProp::TargetedPlayerOwn),
        "OppOwn" => Prop::Ext(ExtProp::OppOwn),
        "IsSolved" => Prop::Ext(ExtProp::Solved),
        "harnessed" => Prop::Ext(ExtProp::Harnessed),
        // "Other creatures": not this object (Forge distinguishes copies; one object here).
        "StrictlyOther" => Prop::Other,
        "wasCastFromGraveyard" => Prop::Ext(ExtProp::CastFromGraveyard),
        "wasCastFromExile" => Prop::Ext(ExtProp::CastFromExile),
        "bargained" => Prop::Ext(ExtProp::Bargained),
        // "with activated abilities" (any zone; `ext6`).
        "hasAbility Activated" => Prop::Ext(ExtProp::HasActivated),
        "RememberedPlayerCtrl" => Prop::Ext(ExtProp::RepPlayerCtrl),
        "sameName" => Prop::Ext(ExtProp::SameNameAsRemembered),
        // The spell whose cast is being counted is the ability's own source.
        "CastSaSource" => Prop::SelfCard,
        "CastSa Spell.Warp" => Prop::Ext(ExtProp::CastForWarp),
        "PromisedGift" => Prop::Ext(ExtProp::PromisedGift),
        _ => {
            if let Some(n) = p.strip_prefix("named").filter(|n| !n.is_empty()) {
                return Ok(Some(Prop::Ext(ExtProp::Named(n.to_string()))));
            }
            if let Some(rest) = p.strip_prefix("counters_") {
                if !rest.contains('_') {
                    let (op, n) = crate::expr::parse_cmp(rest)?;
                    return Ok(Some(Prop::Ext(ExtProp::AnyCounters(op, n))));
                }
            }
            return Ok(None);
        }
    }))
}

pub fn ext_prop_matches(s: &State, db: &CardDb, p: &ExtProp, id: ObjId, o: &Obj, c: &Chars, cx: FilterCtx) -> bool {
    let tgt_player = || {
        s.ext.res_targets.iter().find_map(|t| match t {
            Target::Player(p) => Some(*p),
            Target::Obj(_) | Target::Ability { .. } => None,
        })
    };
    match p {
        ExtProp::ChosenType => cx.source.and_then(|src| s.ext.chosen_type_of(src)).is_some_and(|st| c.subtypes.contains(&st) || (c.all_creature_types && db.creature_types.contains(&st))),
        // CR 201.2 / 707.9b: the names the object has, a copy's name exception included.
        ExtProp::NamedCard => cx.source.and_then(|src| s.ext.named_of(src)).is_some_and(|i| crate::query::has_name(s, db, id, crate::name_pool::name(i))),
        ExtProp::ChosenCard => cx.source.is_some_and(|src| s.ext.chosen_cards.contains(&(src, id))),
        ExtProp::ExiledWithSource => cx.source.is_some_and(|src| s.ext.exiled_with_src(id, src)),
        ExtProp::TargetedPlayerCtrl => tgt_player() == Some(o.controller),
        ExtProp::TargetedPlayerOwn => tgt_player() == Some(o.owner),
        ExtProp::OppOwn => o.owner != cx.you,
        ExtProp::SameNameAsRemembered => cx.rem.iter().any(|&r| crate::query::same_name(s, db, r, id)),
        ExtProp::SameNameAsTargeted => s.ext.res_targets.iter().any(|t| matches!(t, Target::Obj(x) if crate::query::same_name(s, db, *x, id))),
        ExtProp::AnyCounters(op, n) => op.eval(o.counters.iter().map(|c| c.1).sum(), *n),
        ExtProp::NotTargeted => !s.ext.res_targets.contains(&Target::Obj(id)),
        ExtProp::CmcCmpX(op) => op.eval(c.mv as i32, cx.x),
        ExtProp::Solved => s.ext.solved.contains(&id),
        ExtProp::Harnessed => o.zone == Zone::Battlefield && s.ext.harnessed.contains(&id),
        ExtProp::CastFromGraveyard => o.zone == Zone::Stack && s.cast_of(id).has(crate::altcast::flag::FROM_GRAVE),
        ExtProp::CastFromExile => o.zone == Zone::Stack && s.cast_of(id).has(crate::altcast::flag::FROM_EXILE),
        ExtProp::Bargained => matches!(o.zone, Zone::Battlefield | Zone::Stack) && !s.cast_info.is_empty() && s.cast_of(id).has(crate::altcast::flag::OPTCOST) && db.get(o.card).face(o.face).statics.iter().any(|st| st.text == "Bargain"),
        ExtProp::RepPlayerCtrl => s.ext.rep_player == Some(o.controller),
        ExtProp::Named(n) => crate::query::has_name(s, db, id, n),
        // Its current activated abilities (`c.abilities`: printed + gained), plus those of its
        // printed face that function in other zones (cycling, CR 113.6). A permanent that lost
        // all abilities (CR 613.1f) has only the ones gained later: its printed ones are gone
        // (The Enigma Jewel's craft materials, "nonlands with activated abilities").
        ExtProp::HasActivated if c.lost_abilities => !c.abilities.is_empty(),
        ExtProp::HasActivated => {
            let f = db.get(o.card).face(o.face);
            !c.abilities.is_empty() || !f.activated.is_empty() || !f.hand_abilities.is_empty() || !f.grave_abilities.is_empty()
        }
        ExtProp::CastForWarp => matches!(o.zone, Zone::Battlefield | Zone::Stack) && !s.cast_info.is_empty() && s.cast_of(id).has(crate::altcast::flag::WARP),
        ExtProp::PromisedGift => matches!(o.zone, Zone::Battlefield | Zone::Stack) && !s.cast_info.is_empty() && s.cast_of(id).has(crate::altcast::flag::OPTCOST) && db.get(o.card).face(o.face).statics.iter().any(|st| st.text == "Gift"),
        ExtProp::InZoneOfTargetedPlayer | ExtProp::InZoneOfYou => {
            let holder = if o.zone == Zone::Battlefield { o.controller } else { o.owner };
            if *p == ExtProp::InZoneOfYou {
                return cx.you == holder;
            }
            // Any of the target players (The Death of Gwen Stacy: "any number of target players").
            s.ext.res_targets.contains(&Target::Player(holder))
        }
    }
}
