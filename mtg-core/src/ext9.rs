//! Long-tail mechanics, ninth batch.
//! Kept in its own module to stay additive (like `ext2`..`ext8`).
//!
//! - **Firebending N** (CR 702.189a): "Whenever this creature attacks, add N {R}. Until end of
//!   combat, you don't lose this mana as steps and phases end." The mana goes to the ordinary
//!   pool; `X9State::combat_mana` records how much of it survives the ends of combat steps.
//! - **Exile, then return transformed under your control** (Kamigawa-style Sagas, The Legend of
//!   Roku): `ChangeZone | Transformed$ True | GainControl$ True`.
//! - **Airbend** (CR 701.65a): exile; while exiled, the owner may cast the card for {2} rather
//!   than its mana cost (an alternative cost, CR 118.9; `altcast::Method::Airbend`).
//! - "If a creature an opponent controls would die, exile it instead" (`ValidLKI$`), "each player
//!   sacrifices", "can't cast more than one spell each turn", "can't cast spells from anywhere
//!   other than their hands", "you may discard two cards; when you do ... X = the greatest mana
//!   value among them".

use crate::basics::*;
use crate::carddb::*;
use crate::engine::{Ctx, Decision, DecisionKind, Opt, R};
use crate::filter::Filter;
use crate::query::*;
use crate::state::*;
use crate::zones::Move;
use smallvec::SmallVec;

type Res<T> = Result<T, String>;

/// Card-database side tables of this batch (`CardDb::x9`).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct X9Tables {
    /// Compile-time only: the execute of a reflexive trigger whose amount is the greatest mana
    /// value among the cards discarded for it (`TriggerRemembered$GreatestCardManaCost`).
    #[serde(skip)]
    pub compiling_max_mv: bool,
    /// A loaded card has a cast-limit or "only from hand" static (`X9Static`).
    pub any_cast_static: bool,
    /// Targeted nodes whose targets can't be the object that caused their trigger ("target
    /// creature you control other than that creature", `NotDefinedTriggeredTarget`).
    pub exclude_triggered: Vec<AbilityId>,
    /// Charms whose maximum number of modes is an expression ("If you descended this turn, you
    /// may choose both instead", `CharmNum$ Count$Compare ...`): (charm node, max).
    pub charm_max: Vec<(AbilityId, crate::expr::Expr)>,
    /// A loaded card asks whether a spell was cast from exile (`wasCastFromExile`): only then are
    /// spells cast from exile flagged (`altcast::flag::FROM_EXILE`).
    pub any_from_exile: bool,
    /// Cards whose own abilities ask whether they entered this turn (`Card.Self+ThisTurnEntered`:
    /// Hidden Lair's "Activate only if this land entered this turn ..."): for them, entering
    /// this turn is part of the object signature (`query::obj_sig`).
    #[serde(default)]
    pub self_entered: Vec<CardId>,
    /// Cards whose abilities count how many times they resolved this turn
    /// (`Count$ResolvedThisTurn`: South Pole Voyager, Belladonna Took): an object of theirs with
    /// a recorded resolution (`ext6::XState::resolved`) is referred to (`query::refs_of`).
    #[serde(default)]
    pub resolved_readers: Vec<CardId>,
    /// A loaded card has an `X9Static::IgnoreZeroLoyalty` static.
    pub any_zero_loyalty: bool,
    /// `ChangeZone` nodes with `ForgetChanged$ True`: what they moved is no longer remembered.
    pub forget_changed: Vec<AbilityId>,
    /// Hidden `ChangeZone` searches with `Reveal$ True`: the cards found are revealed.
    pub reveal_found: Vec<AbilityId>,
    /// Targeted nodes whose targets (cards in graveyards) must all have the same owner ("from a
    /// single graveyard", `TargetsWithSameController$`).
    pub same_owner_targets: Vec<AbilityId>,
    /// Targeted nodes whose targets must be attached to the previous targeted part's target
    /// ("target Equipment attached to that creature", `AttachedTo ParentTarget`).
    pub attached_to_parent: Vec<AbilityId>,
    /// Targeted `ChangeZone` nodes that move their source together with their targets, as one
    /// event (`ThisDefinedAndTgts$ Self`: "Shuffle this creature and target creature ...").
    #[serde(default)]
    pub this_and_targets: Vec<AbilityId>,
}

/// Per-game state of this batch (`Ext::x9`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct X9State {
    /// Firebending mana (CR 702.189a) in each player's pool, by `ManaType`: kept as combat steps
    /// end, lost as the end of combat step ends (`end_of_step`).
    pub combat_mana: [[u8; 6]; 2],
}

/// Effects of this batch.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum X9Effect {
    /// Firebending N (CR 702.189a): add N {R} that lasts until end of combat.
    Firebend { n: u8 },
    /// "Return it to the battlefield transformed under your control" (CR 712.14, 701.27): the
    /// defined cards in exile/graveyard/hand enter back face up under the ability's controller.
    PutTransformedYours,
    /// "Each player sacrifices N [valid] of their choice" (CR 701.21a, 101.4): in APNAP order
    /// each player chooses, then all are sacrificed at once. Remembers the sacrificed objects as
    /// they last existed (old ids keep their last known information).
    SacrificeEach { valid: Filter, n: crate::expr::Expr },
    /// "For each opponent who didn't discard `need` [valid] cards this way, you draw a card"
    /// (Garruk, Veiled Butcher; after a remembering `Discard`): two players, one opponent.
    DrawIfOpponentDiscardedFewer { need: u8, valid: Filter },
    /// Airbend (CR 701.65a) every permanent matching `valid` (but the targets of the previous
    /// targeted node when `except_targeted`, "all other creatures").
    Airbend { valid: Filter, except_targeted: bool },
    /// "Then you may discard N cards. When you do, [execute]" with the reflexive trigger's amount
    /// the greatest mana value among the discarded cards (CR 603.12; Ill-Timed Explosion).
    DiscardReflexiveMaxMv { n: u8, execute: AbilityId },
    /// "Return it to your hand at the beginning of the next end step" for the defined
    /// permanents (`Animate | AtEOT$ Hand`): a delayed trigger per permanent (CR 603.7).
    ReturnAtNextEnd { execute: AbilityId },
    /// "Shuffle your library" (CR 701.24a).
    Shuffle,
    /// Airbend the targets (CR 701.65a; `Airbend | ValidTgts$`): permanents only.
    AirbendTargets,
    /// "Copy that spell. You may choose new targets for the copy." for the spell that caused a
    /// "whenever you cast" trigger (CR 707.10, 707.10c; Leyline of Resonance).
    CopyTriggeredSpellNewTargets,
    /// "You may cast the discarded card without paying its mana cost" (Malcolm, Alluring
    /// Scoundrel; Forge `Play | Defined$ Remembered | ValidZone$ Graveyard | WithoutManaCost$`):
    /// the defined cards still in a graveyard, cast while this resolves ignoring timing (CR
    /// 608.2g), for no mana (118.9); lands can't be cast.
    MayCastDefinedFree,
    /// "Destroy all [valid]" remembering what it destroyed (`DestroyAll | RememberDestroyed$`;
    /// Fumigate, Zero Point Ballad).
    DestroyAllRemember { valid: Filter },
    /// "Return a [valid] card put into a graveyard this way to the battlefield under your
    /// control" (Zero Point Ballad; `ChangeZone | Hidden$ | Origin$ Graveyard | GainControl$`):
    /// `n` cards from any graveyard, chosen by you (`mandatory`: as many as possible).
    ReturnFromGraveyardsYours { valid: Filter, n: u8, mandatory: bool },
}

/// Expressions of this batch (`ExtExpr::X9`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum X9Expr {
    /// The number of times the evaluating player descended this turn: permanent cards put into
    /// their graveyard from anywhere (CR 700.11; tokens aren't cards).
    DescendedThisTurn,
    /// 1 if the triggering spell targets only a single object (all its targets are that one
    /// object, at least one) and that object matches the filter (Leyline of Resonance:
    /// `IsSingleTarget$ True | TargetsValid$ Creature.YouCtrl`; ruling 2024-09-20).
    SingleTargetValid(Filter),
}

/// Statics of this batch (`StaticExt::X9`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum X9Static {
    /// "Each player can't cast more than `n` spells each turn" (High Noon): `caster` = the
    /// players it applies to.
    CastLimit { caster: PlayerSel, n: u8 },
    /// "[players] can't cast spells from anywhere other than their hands" (Avatar's Wrath).
    OnlyFromHand { caster: PlayerSel },
    /// "Planeswalkers you control aren't put into their owners' graveyards for having 0
    /// loyalty" (Sanctum Lurker; an exception to the state-based action CR 704.5i).
    IgnoreZeroLoyalty { valid: Filter },
}

// ---------------------------------------------------------------- compiling

impl CardDb {
    /// `K:Firebending:N` (CR 702.189a): a triggered ability "whenever this creature attacks".
    /// Only a number N (the `X` forms are refused).
    pub(crate) fn x9_firebending(&mut self, n: &str, synth: &mut Vec<TriggerDef>) -> Res<()> {
        let n: u8 = n.parse().map_err(|_| format!("keyword Firebending:{n}"))?;
        let self_filter = Filter { alts: vec![crate::filter::Alt { base: crate::filter::Base::Card, props: vec![crate::filter::Prop::SelfCard] }] };
        let mut a = crate::compile_ext::node(Effect::Ext(crate::effect_ext::ExtEffect::X9(X9Effect::Firebend { n })), None);
        a.text = format!("Firebending {n}");
        let execute = self.push_ability(a);
        synth.push(TriggerDef { mode: TrigMode::Attacks { valid: self_filter }, zones: ZoneSet::of(Zone::Battlefield), execute, optional: false, intervening: vec![], per_turn: None, text: format!("Firebending {n}") });
        Ok(())
    }

    /// `ChangeZone | Transformed$ True | GainControl$ True` from exile/graveyard/hand to the
    /// battlefield ("return it to the battlefield transformed under your control"). `None`: not
    /// this form (keys untouched).
    pub(crate) fn x9_change_zone(&mut self, p: &crate::compile::P, f: &forge_script::Face, origin: ZoneSet, dest: Zone) -> Res<Option<Effect>> {
        let t = p.peek("Transformed").is_some_and(|v| v.eq_ignore_ascii_case("true"));
        let g = p.peek("GainControl").is_some_and(|v| v.eq_ignore_ascii_case("true"));
        // "Return a creature card put into a graveyard this way to the battlefield under your
        // control" (Zero Point Ballad): a choice among any graveyard's matching cards.
        if g && !t && origin == ZoneSet::of(Zone::Graveyard) && dest == Zone::Battlefield && p.peek("Hidden").is_some() {
            let (Some(ct), true) = (p.peek("ChangeType"), p.peek("Defined").is_none() && p.peek("ValidTgts").is_none() && p.peek("DefinedPlayer").is_none() && p.peek("Chooser").is_none() && p.peek("Tapped").is_none()) else {
                return Err("ChangeZone GainControl form".into());
            };
            if !ct.contains("IsRemembered") {
                return Err("ChangeZone GainControl form".into());
            }
            let n: u8 = p.peek("ChangeNum").unwrap_or("1").parse().map_err(|_| "ChangeZone GainControl ChangeNum")?;
            p.matched("GainControl", |v| v.eq_ignore_ascii_case("true"), "checked above");
            p.ack("Hidden", &["True"], "a choice among graveyard cards");
            p.ack("ChangeType", &[ct], "the filter below");
            p.matched("ChangeNum", |v| v.parse::<u8>().is_ok(), "the count above");
            let mandatory = p.flag("Mandatory");
            return Ok(Some(Effect::Ext(crate::effect_ext::ExtEffect::X9(X9Effect::ReturnFromGraveyardsYours { valid: self.filter_f(f, ct)?, n, mandatory }))));
        }
        if !(t && g) {
            return Ok(None);
        }
        if dest != Zone::Battlefield || p.peek("ValidTgts").is_some() || p.peek("Tapped").is_some() || origin.has(Zone::Library) || origin.has(Zone::Battlefield) || origin.has(Zone::Stack) {
            return Err("ChangeZone Transformed form".into());
        }
        if !matches!(p.peek("Defined"), Some("Remembered") | Some("Self")) {
            return Err("ChangeZone Transformed form".into());
        }
        p.matched("Transformed", |v| v.eq_ignore_ascii_case("true"), "checked above");
        p.matched("GainControl", |v| v.eq_ignore_ascii_case("true"), "checked above");
        Ok(Some(Effect::Ext(crate::effect_ext::ExtEffect::X9(X9Effect::PutTransformedYours))))
    }

    /// `Sacrifice | Defined$ Player`: each player sacrifices (`X9Effect::SacrificeEach`).
    pub(crate) fn x9_sacrifice_each(&mut self, p: &crate::compile::P, f: &forge_script::Face) -> Res<Effect> {
        p.ack("Defined", &["Player"], "each player (checked by the caller)");
        let valid = self.filter_f(f, p.get("SacValid").ok_or("Sacrifice Defined Player without SacValid")?)?;
        if p.peek("Amount").is_some() || p.peek("Optional").is_some() || p.peek("Random").is_some() {
            return Err("Sacrifice Defined Player form".into());
        }
        Ok(Effect::Ext(crate::effect_ext::ExtEffect::X9(X9Effect::SacrificeEach { valid, n: crate::expr::Expr::Num(1) })))
    }

    /// Garruk, Veiled Butcher's "For each opponent who didn't discard two nonland cards this way,
    /// you draw a card": exactly Forge's `StoreSVar` / `RepeatEach` / `Draw NumCards$ X` chain
    /// after a remembering `Discard`. The whole chain is this one effect (`absorbs_chain`).
    pub(crate) fn x9_store_svar(&mut self, p: &crate::compile::P, f: &forge_script::Face) -> Res<Effect> {
        let bad = || -> String { "API StoreSVar".into() };
        let ab = |n: &str| f.svar(n).and_then(|s| s.as_ability());
        let (Some("X"), Some("Number"), Some("0"), Some(rep)) = (p.get("SVar"), p.get("Type"), p.get("Expression"), p.peek("SubAbility")) else { return Err(bad()) };
        let rep = ab(rep).ok_or_else(bad)?;
        let inc = ab(rep.get("RepeatSubAbility").ok_or_else(bad)?).ok_or_else(bad)?;
        let draw = ab(rep.get("SubAbility").ok_or_else(bad)?).ok_or_else(bad)?;
        let y = f.svar("Y").map(|v| v.value.trim().to_string());
        let ok = rep.api() == Some(("DB", "RepeatEach"))
            && rep.get("RepeatPlayers") == Some("Opponent")
            && inc.api() == Some(("DB", "StoreSVar"))
            && inc.get("SVar") == Some("X")
            && inc.get("Type") == Some("CountSVar")
            && inc.get("Expression") == Some("X/Plus.1")
            && inc.get("ConditionCheckSVar") == Some("Y")
            && draw.api() == Some(("DB", "Draw"))
            && draw.get("NumCards") == Some("X")
            && draw.get("Defined").is_none()
            && draw.get("SubAbility").is_none_or(|s| ab(s).is_some_and(|c| c.api() == Some(("DB", "Cleanup"))));
        let (Some(cmp), Some(y)) = (inc.get("ConditionSVarCompare"), y) else { return Err(bad()) };
        let (Some(need), Some(valid)) = (cmp.strip_prefix("LT").and_then(|n| n.parse::<u8>().ok()), y.strip_prefix("Remembered$Valid ").and_then(|v| v.strip_suffix("+RememberedPlayerOwn"))) else { return Err(bad()) };
        if !ok {
            return Err(bad());
        }
        Ok(Effect::Ext(crate::effect_ext::ExtEffect::X9(X9Effect::DrawIfOpponentDiscardedFewer { need, valid: self.filter_f(f, valid)? })))
    }

    /// `Airbend | Defined$ Valid <filter>` (CR 701.65a). `NotDefinedTargeted` in the filter =
    /// "other than the targets" (Avatar's Wrath: "choose up to one target creature, then airbend
    /// all other creatures").
    pub(crate) fn x9_airbend(&mut self, p: &crate::compile::P, f: &forge_script::Face) -> Res<Effect> {
        // "Airbend [up to one / any number of] target ..." (the targets: `ValidTgts$`, compiled by
        // the caller); permanents only.
        if p.peek("ValidTgts").is_some() && p.peek("Defined").is_none() {
            let v = p.peek("ValidTgts").unwrap();
            if p.peek("TgtZone").is_some() || !(v.starts_with("Creature") || v.starts_with("Permanent") || v.starts_with("Artifact")) {
                return Err(format!("Airbend ValidTgts {v}"));
            }
            return Ok(Effect::Ext(crate::effect_ext::ExtEffect::X9(X9Effect::AirbendTargets)));
        }
        let d = p.get("Defined").ok_or("Airbend form")?;
        let v = d.strip_prefix("Valid ").ok_or("Airbend form")?;
        let (v, except_targeted) = match v.strip_suffix(".NotDefinedTargeted") {
            Some(base) => (base, true),
            None => (v, false),
        };
        let valid = self.filter_f(f, v)?;
        // Permanents only: airbending spells (Aang, Swift Savior) isn't implemented.
        if !matches!(v, "Creature" | "Permanent" | "Creature.nonToken") {
            return Err(format!("Airbend Defined {d}"));
        }
        Ok(Effect::Ext(crate::effect_ext::ExtEffect::X9(X9Effect::Airbend { valid, except_targeted })))
    }

    /// Ill-Timed Explosion's `ImmediateTrigger | UnlessCost$ Discard<N/Card> | UnlessPayer$ You |
    /// UnlessSwitched$ True | RememberDiscarded$ True | RememberObjects$ Remembered` with X =
    /// `TriggerRemembered$GreatestCardManaCost` (the face has that SVar).
    pub(crate) fn x9_reflexive_max_mv(&mut self, p: &crate::compile::P, f: &forge_script::Face, uns: &mut Vec<String>, depth: u32) -> Res<Effect> {
        let n = p
            .get("UnlessCost")
            .and_then(|c| c.strip_prefix("Discard<"))
            .and_then(|c| c.strip_suffix("/Card>"))
            .and_then(|c| c.parse::<u8>().ok())
            .ok_or("ImmediateTrigger GreatestCardManaCost form")?;
        if p.get("UnlessPayer") != Some("You") || !p.flag("UnlessSwitched") || !p.flag("RememberDiscarded") || p.get("RememberObjects") != Some("Remembered") || p.peek("TriggerAmount").is_some() {
            return Err("ImmediateTrigger GreatestCardManaCost form".into());
        }
        let exec = p.get("Execute").ok_or("ImmediateTrigger without Execute")?;
        let sv = f.svar(exec).and_then(|s| s.as_ability()).ok_or_else(|| format!("ImmediateTrigger execute {exec} missing"))?;
        self.x9.compiling_max_mv = true;
        let execute = self.ability_inner(&sv, f, uns, depth + 1, false);
        self.x9.compiling_max_mv = false;
        Ok(Effect::Ext(crate::effect_ext::ExtEffect::X9(X9Effect::DiscardReflexiveMaxMv { n, execute: execute? })))
    }

    /// `Animate | AtEOT$ Hand` ("Return it to your hand at the beginning of the next end step"):
    /// the node after the animation (`defined` = the animated objects).
    pub(crate) fn x9_at_eot(&mut self, what: &str, defined: Option<Defined>) -> Res<AbilityDef> {
        if what != "Hand" || !matches!(defined, Some(Defined::Imprinted) | Some(Defined::Remembered) | Some(Defined::Targeted)) {
            return Err(format!("Animate AtEOT {what}"));
        }
        let mut ret = crate::compile_ext::node(
            Effect::ChangeZone { origin: ZoneSet::of(Zone::Battlefield), dest: Zone::Hand, hidden: false, change_type: None, num: crate::expr::Expr::Num(1), tapped: false, optional: false, shuffle: false, searcher: Defined::You },
            Some(Defined::Triggered),
        );
        ret.text = "Return it to your hand at the beginning of the next end step.".into();
        let execute = self.push_ability(ret);
        Ok(crate::compile_ext::node(Effect::Ext(crate::effect_ext::ExtEffect::X9(X9Effect::ReturnAtNextEnd { execute })), defined))
    }

    /// `Play | Defined$ Remembered | ValidZone$ Graveyard | WithoutManaCost$ True | Optional$ True`
    /// (Malcolm): `X9Effect::MayCastDefinedFree`. `None`: another `Play` form (keys untouched).
    pub(crate) fn x9_play(&mut self, p: &crate::compile::P) -> Res<Option<Effect>> {
        if p.peek("ValidZone") != Some("Graveyard") || p.peek("Defined") != Some("Remembered") || p.peek("Valid") != Some("Card.IsRemembered") {
            return Ok(None);
        }
        if p.get("ValidSA") != Some("Spell") || !p.flag("WithoutManaCost") || p.peek("Optional").is_none() || p.get("Controller").is_some_and(|c| c != "You") || p.peek("Amount").is_some() || p.peek("CopyCard").is_some() {
            return Err("Play from graveyard form".into());
        }
        p.ack("ValidZone", &["Graveyard"], "checked above");
        p.ack("Valid", &["Card.IsRemembered"], "checked above");
        p.display("DefinedDesc");
        p.display("IfDesc");
        Ok(Some(Effect::Ext(crate::effect_ext::ExtEffect::X9(X9Effect::MayCastDefinedFree))))
    }

    /// `CantBeCast` forms of this batch (`None`: not one of them; keys untouched).
    pub(crate) fn x9_static(&mut self, mode: &str, p: &crate::compile::P) -> Res<Option<StaticKind>> {
        if mode == "IgnorePlaneswalkerZeroLoyaltyRule" {
            let valid = self.filter(p.get("ValidCard").ok_or("IgnorePlaneswalkerZeroLoyaltyRule without ValidCard")?)?;
            self.x9.any_zero_loyalty = true;
            return Ok(Some(StaticKind::Ext(crate::static_ext::StaticExt::X9(X9Static::IgnoreZeroLoyalty { valid }))));
        }
        if mode != "CantBeCast" {
            return Ok(None);
        }
        if let Some(n) = p.peek("NumLimitEachTurn") {
            // "Each player can't cast more than one spell each turn" (High Noon).
            let n: u8 = n.parse().map_err(|_| format!("NumLimitEachTurn = {n}"))?;
            if p.peek("ValidCard").is_some_and(|v| v != "Card") || p.peek("Condition").is_some() {
                return Err("CantBeCast NumLimitEachTurn form".into());
            }
            p.matched("NumLimitEachTurn", |v| v.parse::<u8>().is_ok(), "parsed above");
            p.ack("ValidCard", &["Card"], "checked above");
            let caster = PlayerSel::parse(p.get("Caster"))?;
            self.x9.any_cast_static = true;
            return Ok(Some(StaticKind::Ext(crate::static_ext::StaticExt::X9(X9Static::CastLimit { caster, n }))));
        }
        if p.peek("ValidCard") == Some("Card.!wasCastFromTheirHand") {
            if p.peek("Condition").is_some() {
                return Err("CantBeCast wasCastFromTheirHand form".into());
            }
            p.ack("ValidCard", &["Card.!wasCastFromTheirHand"], "checked above");
            let caster = PlayerSel::parse(p.get("Caster"))?;
            self.x9.any_cast_static = true;
            return Ok(Some(StaticKind::Ext(crate::static_ext::StaticExt::X9(X9Static::OnlyFromHand { caster }))));
        }
        Ok(None)
    }
}

/// Evaluate an expression of this batch.
pub fn eval(e: &X9Expr, c: &crate::expr::EvalCtx) -> i32 {
    let s = c.s;
    match e {
        X9Expr::DescendedThisTurn => s
            .ext
            .moved_this_turn
            .iter()
            .filter(|&&(_, _, to, new)| {
                let o = s.obj(new);
                to == Zone::Graveyard && !o.token && o.owner == c.you && {
                    let t = c.db.get(o.card).front().types;
                    [CardType::Artifact, CardType::Creature, CardType::Enchantment, CardType::Land, CardType::Planeswalker, CardType::Battle].iter().any(|&k| t.has(k))
                }
            })
            .count() as i32,
        X9Expr::SingleTargetValid(f) => {
            let Some(spell) = c.triggered else { return 0 };
            let Some(it) = s.stack_item_of(spell) else { return 0 }; // waiting or resolving (608.2n)
            let Some(&(_, first)) = it.targets.first() else { return 0 };
            if it.targets.iter().any(|t| t.1 != first) {
                return 0;
            }
            let Target::Obj(o) = first else { return 0 };
            let fcx = FilterCtx { x: 0, ..eval_fcx!(c) };
            obj_matches(s, c.db, f, o, fcx) as i32
        }
    }
}

/// For a trigger whose execute has a target that can't be its triggering object
/// (`X9Tables::exclude_triggered`): fewer legal targets than it needs once that object is
/// excluded (CR 603.3d: then it's removed from the stack).
pub fn lacks_targets_excluding_trigger(cx: &Ctx, p: PlayerId, ability: AbilityId, source: ObjId, trig: &TrigCtx) -> bool {
    if cx.db.x9.exclude_triggered.is_empty() {
        return false;
    }
    let Some(o) = trig.target else { return false };
    crate::resolve::nodes(cx.db, ability, &[]).iter().any(|&n| {
        cx.db.x9.exclude_triggered.contains(&n)
            && cx.db.ability(n).target.as_ref().is_some_and(|t| crate::cast::legal_targets(cx, p, t, Some(source), &[Target::Obj(o)]).len() < t.min as usize)
    })
}

/// "Attached to that creature" (`X9Tables::attached_to_parent`): is `t` attached to the target
/// of the targeted part before node index `i` (`earlier`: (node index, target) so far)?
pub fn attached_to_parent_ok(cx: &Ctx, n: AbilityId, i: usize, earlier: &[(u8, Target)], t: Target) -> bool {
    if cx.db.x9.attached_to_parent.is_empty() || !cx.db.x9.attached_to_parent.contains(&n) {
        return true;
    }
    let parent = earlier.iter().rev().find(|e| (e.0 as usize) < i).map(|e| e.1);
    match (parent, t) {
        (Some(Target::Obj(c)), Target::Obj(o)) => cx.s.obj(o).attached_to == Some(c) && cx.s.obj(c).zone == Zone::Battlefield,
        _ => false,
    }
}

/// The maximum number of modes of charm node `n` for `p` now (`X9Tables::charm_max`), if
/// it's an expression.
pub fn charm_max(cx: &Ctx, p: PlayerId, n: AbilityId, source: Option<ObjId>) -> Option<u8> {
    let (_, e) = cx.db.x9.charm_max.iter().find(|x| x.0 == n)?;
    let src = source.unwrap_or(ObjId(0));
    let ecx = crate::expr::EvalCtx { s: cx.s, db: cx.db, you: p, source: src, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &[], imprinted: &[], base_only: false };
    Some(crate::expr::eval(e, &ecx).clamp(0, 255) as u8)
}

/// A split card's characteristics outside the stack (CR 709.4): both names, the combined mana
/// cost (mana value = the sum, 709.4b), both colors and both card types; no spell ability.
pub fn split_combined(l: &FaceDef, r: &FaceDef) -> FaceDef {
    let mut both = l.clone();
    both.name = format!("{} // {}", l.name, r.name);
    both.cost.syms.extend(r.cost.syms.iter().cloned());
    both.colors = ColorSet(l.colors.0 | r.colors.0);
    both.types = TypeSet(l.types.0 | r.types.0);
    both.oracle = format!("{} // {}", l.oracle, r.oracle);
    both.spell = None;
    both
}

/// The effect absorbs the rest of its chain (compiled into it).
pub fn absorbs_chain(e: &Effect) -> bool {
    matches!(e, Effect::Ext(crate::effect_ext::ExtEffect::X9(X9Effect::DrawIfOpponentDiscardedFewer { .. })))
}

// ---------------------------------------------------------------- rules

/// CR 500.4 / 702.189a: mana empties from pools as a step or phase ends, except firebending
/// mana until the end of combat (the end of the end of combat step). Called where the pools are
/// emptied (`flow::end_step`); `step` = the step that is ending (`None`: the turn skips to its
/// cleanup step, CR 723.1d). The player keeps min(pool, firebending mana) of each type: unspent
/// mana of one type is interchangeable, and keeping the firebending part is never worse for its
/// player (as if the expiring mana had been spent first).
pub fn empty_pools(s: &mut State, step: Option<Step>) {
    let combat = matches!(step, Some(Step::BeginCombat | Step::DeclareAttackers | Step::DeclareBlockers | Step::FirstStrikeDamage | Step::CombatDamage));
    let keep = s.ext.x9.combat_mana;
    let pool = s.ext.pool;
    s.ext.pool = [[0; 6]; 2];
    s.ext.x9.combat_mana = [[0; 6]; 2];
    if combat && keep != [[0; 6]; 2] {
        for p in 0..2 {
            for t in 0..6 {
                let k = keep[p][t].min(pool[p][t]);
                s.ext.pool[p][t] = k;
                s.ext.x9.combat_mana[p][t] = k;
            }
        }
    }
}

/// The part of `p`'s pool that stays when the current step ends (what `empty_pools` would keep):
/// firebending mana during the combat steps before the end of combat step (CR 702.189a), else
/// nothing. Observation field `PlayerView::pool_kept` (encoding v10).
pub fn pool_kept(s: &State, p: PlayerId) -> [u8; 6] {
    let mut k = [0u8; 6];
    if matches!(s.turn.step, Step::BeginCombat | Step::DeclareAttackers | Step::DeclareBlockers | Step::FirstStrikeDamage | Step::CombatDamage) {
        for t in 0..6 {
            k[t] = s.ext.x9.combat_mana[p.idx()][t].min(s.ext.pool[p.idx()][t]);
        }
    }
    k
}

/// Does a static keep planeswalker `o` from being put into its owner's graveyard for having 0
/// loyalty (CR 704.5i; `X9Static::IgnoreZeroLoyalty`)?
pub fn ignores_zero_loyalty(s: &State, db: &CardDb, o: ObjId) -> bool {
    if !db.x9.any_zero_loyalty {
        return false;
    }
    s.battlefield.iter().any(|&src| {
        let so = s.obj(src);
        db.get(so.card).face(so.face).statics.iter().any(|st| {
            let StaticKind::Ext(crate::static_ext::StaticExt::X9(X9Static::IgnoreZeroLoyalty { valid })) = &st.kind else { return false };
            st.zones.has(Zone::Battlefield)
                && !(crate::query::may_lose_abilities(s, db) && crate::query::lost_all_abilities(s, db, src))
                && obj_matches(s, db, valid, o, FilterCtx { you: so.controller, source: Some(src), x: 0, rem: &[] })
        })
    })
}

/// Can't-cast statics of this batch for `p` casting `h` (`altcast::cant_cast`): on permanents
/// and on effect objects (resolved spells' effects).
pub fn cant_cast(cx: &Ctx, p: PlayerId, h: ObjId) -> bool {
    if !cx.db.x9.any_cast_static {
        return false;
    }
    let s = &*cx.s;
    let applies = |st: &X9Static, ctrl: PlayerId| match st {
        X9Static::CastLimit { caster, n } => caster.matches(p, ctrl) && s.ext.cast_this_turn.iter().filter(|c| c.0 == p).count() >= *n as usize,
        // CR 601.2a: where the card is before it's cast; a hand is its owner's (CR 402.1).
        X9Static::OnlyFromHand { caster } => caster.matches(p, ctrl) && s.obj(h).zone != Zone::Hand,
        X9Static::IgnoreZeroLoyalty { .. } => false,
    };
    for &src in &s.battlefield {
        let so = s.obj(src);
        for st in &cx.db.get(so.card).face(so.face).statics {
            let StaticKind::Ext(crate::static_ext::StaticExt::X9(x)) = &st.kind else { continue };
            if !st.zones.has(Zone::Battlefield) || (crate::query::may_lose_abilities(s, cx.db) && crate::query::lost_all_abilities(s, cx.db, src)) {
                continue;
            }
            if applies(x, so.controller) {
                return true;
            }
        }
    }
    for eo in &s.effect_objs {
        for &si in &eo.statics {
            // effect-object static reader: CastLimit, OnlyFromHand (`static_ext::applies_from_effect_object`)
            let StaticKind::Ext(crate::static_ext::StaticExt::X9(x)) = &cx.db.statics_pool[si as usize].kind else { continue };
            if applies(x, eo.controller) {
                return true;
            }
        }
    }
    false
}

/// Airbend permanents (CR 701.65a): exile them at once; each card exiled this way may be cast
/// by its owner for {2} while it stays exiled (`altcast::flag::AIRBENT`).
fn airbend(cx: &mut Ctx, picked: &[ObjId]) -> R<()> {
    let moves: Vec<Move> = picked.iter().map(|&o| Move::new(o, Zone::Exile)).collect();
    let new = crate::zones::move_batch(cx, &moves)?;
    for nid in new.into_iter().flatten() {
        // A token ceases to exist (CR 111.7, 704.5d) and can't be cast (ruling 2025-10-02); a
        // card a replacement put elsewhere isn't "exiled this way".
        let o = cx.s.obj(nid);
        if o.zone == Zone::Exile && !o.token {
            cx.s.cast_mut(nid).flags |= crate::altcast::flag::AIRBENT;
        }
    }
    Ok(())
}

pub fn apply(cx: &mut Ctx, ex: &mut crate::resolve::Exec, ad: &AbilityDef, x: &X9Effect, _tg: &[Target], who: &[Target]) -> R<()> {
    use crate::resolve::objs;
    match x {
        X9Effect::Firebend { n } => {
            let p = ex.you;
            let r = ManaType::R as usize;
            let slot = &mut cx.s.ext.pool[p.idx()][r];
            *slot = slot.saturating_add(*n);
            let c = &mut cx.s.ext.x9.combat_mana[p.idx()][r];
            *c = c.saturating_add(*n);
        }
        X9Effect::PutTransformedYours => {
            let mut new_ids: SmallVec<[ObjId; 2]> = SmallVec::new();
            for o in objs(who) {
                if !matches!(cx.s.obj(o).zone, Zone::Exile | Zone::Graveyard | Zone::Hand) {
                    continue;
                }
                // Ruling (The Legend of Roku, 2025-10-02): a card that isn't double-faced isn't
                // put onto the battlefield at all; it stays where it is.
                if cx.db.back_of(cx.s.obj(o).card).is_none() {
                    continue;
                }
                if let Some(nid) = crate::zones::move_special(cx, o, ex.you, None, true)? {
                    let (card, front) = (cx.s.obj(nid).card, cx.s.obj(o).card);
                    cx.s.emit(Event::Transformed { obj: nid, card: front, face_card: card });
                    new_ids.push(nid);
                }
            }
            ex.moved = new_ids.clone();
            if ad.remember {
                ex.remembered.extend(new_ids);
            }
        }
        X9Effect::SacrificeEach { valid, n } => {
            let k = crate::resolve::amount(cx, ex, n).max(0) as usize;
            // CR 101.4: APNAP order; each choice is made knowing the previous ones.
            let first = cx.s.turn.active;
            let mut chosen_all: SmallVec<[(PlayerId, ObjId); 2]> = SmallVec::new();
            for p in [first, first.opp()] {
                let fcx = FilterCtx { you: p, x: 0, ..exec_fcx!(ex) };
                let objs: Vec<ObjId> = cx.s.battlefield.iter().copied().filter(|&o| cx.s.obj(o).controller == p && obj_matches(cx.s, cx.db, valid, o, fcx)).collect();
                let items: Vec<(ObjSig, Opt)> = crate::query::sig_items(cx.s, cx.db, &objs);
                let m = k.min(items.len());
                for c in cx.choose_set(p, DecisionKind::Sacrifice, items, m, m, Some(ex.source))? {
                    if let Opt::Card(o) = c {
                        chosen_all.push((p, o));
                    }
                }
            }
            for p in [first, first.opp()] {
                let mine: SmallVec<[ObjId; 2]> = chosen_all.iter().filter(|c| c.0 == p).map(|c| c.1).collect();
                if !mine.is_empty() {
                    crate::trig_ext::fire_sacrificed(cx, &mine, p);
                }
            }
            let moves: Vec<Move> = chosen_all.iter().map(|&(_, o)| Move::new(o, Zone::Graveyard)).collect();
            crate::zones::move_batch(cx, &moves)?;
            if ad.remember {
                // As they last existed (the old ids keep controller and characteristics), so
                // "if you sacrificed a creature this way" holds for a token or a creature that
                // was exiled instead (Garruk's own replacement applies to opponents' creatures).
                ex.remembered.extend(chosen_all.iter().map(|c| c.1));
            }
        }
        X9Effect::DrawIfOpponentDiscardedFewer { need, valid } => {
            let opp = ex.you.opp();
            let fcx = FilterCtx { x: 0, ..exec_fcx!(ex) };
            let k = ex.remembered.iter().filter(|&&o| cx.s.obj(o).owner == opp && obj_matches(cx.s, cx.db, valid, o, fcx)).count();
            if k < *need as usize {
                crate::flow::draw_n(cx.s, cx.db, ex.you, 1);
            }
        }
        X9Effect::Airbend { valid, except_targeted } => {
            let fcx = FilterCtx { x: 0, ..exec_fcx!(ex) };
            let skip: SmallVec<[ObjId; 4]> = if *except_targeted { objs(&ex.last_targets) } else { SmallVec::new() };
            let picked: Vec<ObjId> = cx.s.battlefield.iter().copied().filter(|&o| !skip.contains(&o) && obj_matches(cx.s, cx.db, valid, o, fcx)).collect();
            airbend(cx, &picked)?;
        }
        X9Effect::DiscardReflexiveMaxMv { n, execute } => {
            let p = ex.you;
            let n = *n as usize;
            if cx.s.player(p).hand.len() < n {
                return Ok(()); // can't discard N cards: no discard, no reflexive trigger
            }
            if cx.choose(Decision { player: p, kind: DecisionKind::PayCost, options: vec![Opt::Yes, Opt::No], subject: Some(ex.source) })? == Opt::No {
                return Ok(());
            }
            let items = crate::flow::hand_items(cx, p);
            let cards: SmallVec<[ObjId; 2]> = cx.choose_set(p, DecisionKind::Discard, items, n, n, Some(ex.source))?.into_iter().filter_map(|c| if let Opt::Card(o) = c { Some(o) } else { None }).collect();
            // The mana value of a card in a hand (CR 202.3; X is 0, ruling 2024-02-02); a
            // double-faced card's is its front face's (CR 712.8a), a split card's the sum
            // (709.4b).
            let mv = cards.iter().map(|&o| chars(cx.s, cx.db, o).mv).max().unwrap_or(0) as i32;
            crate::ext7::discard(cx, p, &cards)?;
            crate::trig_ext::add_reflexive(cx, p, ex.source, *execute, TrigCtx { amount: mv, ..Default::default() });
        }
        X9Effect::ReturnAtNextEnd { execute } => {
            // "Return it/them ... at the beginning of the next end step": one delayed ability over
            // the set (CR 603.7a, 603.7c; `add_delayed_over`). Its `Defined$ Triggered` return acts
            // on one object: over several it is refused (no pool card animates more than one:
            // Anzrag's Rampage's is its imprinted card).
            let on_bf: SmallVec<[ObjId; 2]> = objs(who).into_iter().filter(|&o| cx.s.obj(o).zone == Zone::Battlefield).collect();
            if !on_bf.is_empty() {
                crate::trig_ext::add_delayed_over(cx, crate::ext::DelayedWhen::NextStep { step: Step::End }, ex.you, ex.source, *execute, &on_bf)?;
            }
        }
        X9Effect::Shuffle => crate::flow::shuffle_library(cx.s, ex.you),
        X9Effect::AirbendTargets => {
            let picked: Vec<ObjId> = objs(who).into_iter().filter(|&o| cx.s.obj(o).zone == Zone::Battlefield).collect();
            airbend(cx, &picked)?;
        }
        X9Effect::DestroyAllRemember { valid } => {
            let fcx = exec_fcx!(ex);
            let v: SmallVec<[ObjId; 4]> = cx.s.battlefield.iter().copied().filter(|&o| obj_matches(cx.s, cx.db, valid, o, fcx)).collect();
            let mut moves = vec![];
            for &o in &v {
                // A shield counter replaces the destruction (CR 122.1c): not destroyed this way.
                if !crate::query::has_kw(cx.s, cx.db, o, Kw::Indestructible) && !crate::resolve::shield_replaces_destroy(cx, o) {
                    let card = cx.s.obj(o).card;
                    cx.s.emit(Event::Destroyed { card });
                    moves.push(Move::new(o, Zone::Graveyard));
                }
            }
            let new = crate::zones::move_batch(cx, &moves)?;
            // Every creature destroyed this way, wherever it went (Fumigate ruling 2024-11-08):
            // its new object, or as it last existed if it has none (a token that ceased to
            // exist). "Put into a graveyard this way" checks the zone where it's read.
            for (m, nid) in moves.iter().zip(new) {
                ex.remembered.push(nid.unwrap_or(m.obj));
            }
        }
        X9Effect::ReturnFromGraveyardsYours { valid, n, mandatory } => {
            let p = ex.you;
            let fcx = FilterCtx { you: p, ..exec_fcx!(ex) };
            let pool: Vec<ObjId> = [p, p.opp()].iter().flat_map(|&q| cx.s.player(q).graveyard.iter().copied()).filter(|&o| crate::query::is_card(cx.s, o) && obj_matches(cx.s, cx.db, valid, o, fcx)).collect();
            let items: Vec<(ObjSig, Opt)> = crate::query::sig_items(cx.s, cx.db, &pool);
            let k = (*n as usize).min(items.len());
            let min = if *mandatory { k } else { 0 };
            let chosen: Vec<ObjId> = cx.choose_set(p, DecisionKind::ChooseCard, items, min, k, Some(ex.source))?.into_iter().filter_map(|o| if let Opt::Card(x) = o { Some(x) } else { None }).collect();
            let moves: Vec<Move> = chosen.iter().map(|&o| Move { obj: o, to: Zone::Battlefield, to_top: false, tapped: false, controller: Some(p) }).collect();
            let new = crate::zones::move_batch(cx, &moves)?;
            ex.moved = new.into_iter().flatten().collect();
        }
        X9Effect::MayCastDefinedFree => {
            use crate::altcast::Method;
            let p = ex.you;
            let mut opts = vec![Opt::Done];
            for o in objs(who) {
                let ob = cx.s.obj(o);
                if ob.zone != Zone::Graveyard || ob.owner != p {
                    continue;
                }
                // "You may cast it without paying": either face of a two-faced card (`faces`).
                for face in crate::faces::spell_faces(cx.s, cx.db, o, crate::faces::FacePerm::Any) {
                    if crate::altcast::castable(cx, p, o, face, true, Method::Free) {
                        opts.push(crate::faces::choice_opt(o, face));
                    }
                }
            }
            if opts.len() == 1 {
                return Ok(());
            }
            let opt = cx.choose(Decision { player: p, kind: DecisionKind::ChooseCard, options: opts, subject: Some(ex.source) })?;
            let Some((o, face)) = crate::faces::chosen_face(&opt) else { return Ok(()) };
            crate::cast::cast_spell_method(cx, p, o, face, Method::Free)?;
        }
        X9Effect::CopyTriggeredSpellNewTargets => {
            let Some(o) = ex.trig.obj else { return Ok(()) };
            // Still a spell on the stack (it may have been countered meanwhile: no copy).
            let Some(snap) = crate::ext6::snapshot_spell(cx.s, o, 0) else { return Ok(()) };
            crate::ext6::copy_spell(cx, &snap, ex.you, ex.resolving.as_ref())?;
        }
    }
    Ok(())
}
