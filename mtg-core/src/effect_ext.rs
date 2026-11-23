//! Effect APIs added after the core set (kept in their own module to stay additive):
//! choices (ChooseType, ChooseCard, NameCard), exile-until links, counters multiplication,
//! damage from another source, reflexive/delayed triggers, earthbend, connive, reveal hand.

use crate::basics::*;
use crate::carddb::*;
use crate::engine::*;
use crate::expr::{Condition, Expr};
use crate::filter::Filter;
use crate::query::*;
use crate::resolve::{affected, objs, players, Exec};
use crate::state::*;
use crate::zones::{move_batch, Move};
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// What kind of type a `ChooseType` picks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypeChoice {
    Creature,
    BasicLand,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExtEffect {
    /// CR 701.10e: double (multiply) the number of a kind of counters.
    MultiplyCounter { kind: CounterKind, factor: i32 },
    /// Deal damage with another object as the source (`DamageSource$`).
    DealDamageFrom { amount: Expr, source: Defined, parent: bool },
    /// Link the objects just moved by the previous node to this ability's source ("exiled with",
    /// CR 607.2a); with `until`, they return to `until` when the source leaves (CR 610.3).
    LinkMoved { until: Option<crate::state::Zone> },
    /// Look at a player's hidden zone and choose cards from it (`ChangeZone` with `Chooser$`
    /// different from the zone's owner, e.g. Deep-Cavern Bat).
    ChooseFromZone { origin: ZoneSet, dest: crate::state::Zone, change_type: Filter, num: Expr, owner: Defined, chooser: Defined, up_to: bool },
    /// "As enters, choose a creature/basic land type" (CR 614.12, 205.3).
    ChooseType { kind: TypeChoice, chooser: Defined },
    /// A player chooses cards (`ChooseCard`), remembered as chosen for this resolution.
    ChooseCard { choices: Filter, chooser: Defined, n: Expr, up_to: bool, zone: crate::state::Zone },
    /// `Branch`: run `then` if the condition holds, else `otherwise` (same targets).
    Branch { cond: Condition, then: Option<AbilityId>, otherwise: Option<AbilityId> },
    /// CR 701.66a: the target land becomes a 0/0 land creature with haste, gets N +1/+1
    /// counters, and "when it dies or is put into exile, return it" (`ret`, a delayed trigger).
    Earthbend { n: Expr, ret: AbilityId },
    /// Reflexive trigger "when you do, ..." (CR 603.12): `execute` goes on the stack next time a
    /// player would get priority; its trigger amount is the number of remembered objects
    /// matching `count` (for "up to that many targets").
    Reflexive { execute: AbilityId, count: Option<Filter>, remember: bool },
    /// An effect object with triggered abilities (`Effect | Triggers$`) for a duration.
    TriggerEffect { triggers: Vec<u32>, duration: EffDuration },
    /// "At the beginning of the next end step, sacrifice them" for the objects just created
    /// (`AtEOT$ Sacrifice`): registers one delayed trigger running `execute` on all of them
    /// (CR 603.7a, 603.7c; its remembered objects, `trig_ext::fire_extra`).
    AtNextEnd { execute: AbilityId },
    /// Creatures just created enter attacking (CR 508.4), if it's their controller's combat: for
    /// each, the controller chooses the defending player or one of that player's planeswalkers
    /// (`combat::enter_attacking`; `TokenAttacking$ True`).
    MakeAttacking,
    /// Creatures just created enter attacking what the triggering attacker attacks ("tapped and
    /// attacking that player": `TokenAttacking$ TriggeredDefender`, an `Attacks` trigger; Seraphic
    /// Greatsword). Before, the key was read but the token didn't attack.
    MakeAttackingTriggeredDefender,
    /// The defined permanents are sacrificed by their controller (CR 701.21), all at once.
    SacrificeDefined,
    /// The defined Cases become solved (CR 719.2).
    SetSolved,
    /// Matching permanents lose all abilities until end of turn (an effect object with the
    /// `RemoveAllAbilities` static `static_idx` over them).
    LoseAllAbilitiesAll { valid: Filter, static_idx: u32 },
    /// The defined/targeted players reveal their hands (CR 701.20a: to all players).
    RevealHand,
    /// CR 701.50a: draw N, then discard N; for each nonland card discarded, a +1/+1 counter.
    Connive { n: Expr },
    /// Run `sub` once for each player (`None`: all players, in APNAP order) with that player
    /// as `Remembered` (`RepeatEach | RepeatPlayers$`).
    RepeatEachPlayer { players: Option<Defined>, sub: AbilityId },
    /// A player chooses a card name (CR 201.3) for the source, among the names of cards in the
    /// card pool matching `valid`.
    NameCard { valid: Filter, chooser: Defined },
    /// Matching cards in your graveyard may be cast from it this turn (effect object with the
    /// `MayPlay` static `static_idx` from `CardDb::statics_pool`).
    GraveyardMayCast { cards: Filter, static_idx: u32 },
    /// Mana ability: one mana of any color among cards exiled with the source (Pit of
    /// Offerings, `ManaReflected | Valid$ Defined.ExiledWith | ColorOrType$ Color`).
    ManaExiledColors,
    /// Counter target spell/ability, putting a countered spell into `zone` (No More Lies).
    CounterTo { zone: crate::state::Zone },
    /// Create tokens for another player (`TokenOwner$ TargetedController`, Get Lost).
    TokenFor { token: CardId, n: Expr, tapped: bool, owner: Defined },
    /// The defined permanents gain all creature types (layer 4, CR 205.3m).
    AllCreatureTypes { duration: Duration },
    /// The objects the previous node put onto the battlefield get a type-changing effect
    /// (`StaticEffect$`, e.g. Enduring Curiosity "It's an enchantment. (It's not a creature.)").
    AnimateMoved { add_types: TypeSet, remove_card_types: bool, add_subtypes: Vec<Subtype>, set_pt: Option<(i32, i32)> },
    /// Put every matching card from `origin` onto the battlefield (under the ability's
    /// controller if `yours`), each with `counters` (Ghost Vacuum).
    PutOntoBattlefieldAll { origin: ZoneSet, change_type: Filter, yours: bool, counters: Option<(CounterKind, i32)> },
    /// The objects the previous node created/moved gain keywords until end of turn
    /// (`Token | PumpKeywords$ .. | PumpDuration$ EOT`).
    PumpMoved { kws: KwSet },
    /// Add mana to the controller's mana pool (CR 106.4) from a resolving spell/ability.
    AddMana(Vec<ManaType>),
    /// The defined/targeted creatures gain the controller's choice of one of these keywords
    /// until end of turn (`Pump | KWChoice$`); chosen as `ChooseMode` with `Opt::Mode(i)`.
    PumpKwChoice { kws: Vec<Kw> },
    /// Copy the spell that caused this `SpellCast` trigger (CR 707.10): the copy is controlled
    /// by this ability's controller, keeps the original's modes, X and targets, and isn't cast;
    /// a copy of a permanent spell becomes a token as it resolves (CR 111.12).
    CopyTriggeredSpell,
    /// CR 701.64a: the defined permanents become harnessed (public designation).
    Harness,
    /// The defined permanents permanently become `card` (a synthetic card: new name, types and
    /// only its abilities), for as long as they stay on the battlefield (`Animate |
    /// RemoveAllAbilities$ | Name$`, The Irencrag). Recorded like a transformed permanent.
    BecomeCard { card: CardId },
    /// Attach each target of the previous targeted node (Equipment) to this node's target
    /// (CR 701.3); with `remember`, the ones that became attached are remembered.
    AttachParentTargets,
    /// A reflexive trigger (CR 603.12) that remembers the first `remember` object as its
    /// trigger object (`ImmediateTrigger | RememberObjects$`).
    ReflexiveWith { execute: AbilityId, remember: Defined },
    /// CR 701.14a: the defined creature and the targeted creature each deal damage equal to
    /// their power to the other (simultaneously). `parent`: the defined creature is the
    /// previous targeted node's target (`Defined$ ParentTarget`: "target creature you control
    /// fights target creature you don't control", Bushwhack).
    Fight { parent: bool },
    /// `times` times: each opponent loses `life` life unless they sacrifice a permanent matching
    /// `sac` or discard a card (Rottenmouth Viper). Each time the opponent chooses
    /// (`ChooseMode`): Mode(0) sacrifice, Mode(1) discard, Mode(2) lose the life.
    Punisher { times: Expr, life: i32, sac: Option<Filter>, discard: bool },
    /// Effects of the second long-tail batch (`ext2`).
    More(crate::ext2::MoreEffect),
    /// Effects of the third long-tail batch (`ext3`).
    Lt(crate::ext3::LtEffect),
    /// Effects of the fourth long-tail batch (`ext4`).
    Mr(crate::ext4::MrEffect),
    /// Effects of the fifth long-tail batch (`ext5`).
    Le(crate::ext5::LeEffect),
    /// Effects of the sixth long-tail batch (`ext6`).
    X(crate::ext6::XEffect),
    /// Effects of the seventh long-tail batch (`ext7`).
    X7(crate::ext7::X7Effect),
    /// Effects of `ext8`.
    X8(crate::ext8::X8Effect),
    /// Effects of `ext9`.
    X9(crate::ext9::X9Effect),
}

/// `Condition::Defined`: compare how many of the defined objects match a filter.
pub fn defined_cond_holds(cx: &Ctx, ex: &Exec, c: &Condition) -> bool {
    let Condition::Defined(d, f, op, n) = c else { return true };
    let os = defined_objs(cx, ex, *d);
    let fcx = exec_fcx!(ex);
    let k = os.iter().filter(|&&o| obj_matches(cx.s, cx.db, f, o, fcx)).count() as i32;
    op.eval(k, *n)
}

/// Objects a `Defined$` refers to in this resolution.
pub fn defined_objs(cx: &Ctx, ex: &Exec, d: Defined) -> SmallVec<[ObjId; 4]> {
    let fake = AbilityDef {
        kind: AbilityKind::Sub,
        effect: Effect::Tap,
        defined: Some(d),
        target: None,
        cost: None,
        sorcery_speed: false,
        is_mana: false,
        activate_if: vec![],
        conditions: vec![],
        unless: None,
        optional: false,
        remember: false,
        sub: None,
        text: String::new(),
    };
    if d == Defined::Targeted {
        return objs(&ex.last_targets);
    }
    objs(&affected(cx, ex, &fake, &[], false))
}

pub fn defined_players(cx: &Ctx, ex: &Exec, d: Defined) -> SmallVec<[PlayerId; 2]> {
    crate::resolve::affected_players(cx, ex, Some(d))
}

pub fn apply(cx: &mut Ctx, ex: &mut Exec, ad: &AbilityDef, x: &ExtEffect, _tg: &[Target], who: &[Target]) -> R<()> {
    let num = |cx: &mut Ctx, ex: &Exec, e: &Expr| crate::resolve::amount(cx, ex, e);
    match x {
        ExtEffect::MultiplyCounter { kind, factor } => {
            for o in objs(who) {
                if cx.s.obj(o).zone == Zone::Battlefield {
                    let have = cx.s.obj(o).counter(*kind);
                    crate::ext6::put_counters(cx.s, cx.db, o, *kind, crate::num::mul(have, factor - 1));
                }
            }
        }
        ExtEffect::DealDamageFrom { amount, source, parent } => {
            // `ParentTarget`: the previous targeted node's target ("target creature you control
            // deals damage equal to its power to target creature"), also for the amount.
            let (src, dmg) = if *parent {
                let Some(&src) = objs(&ex.parent_targets).first() else { return Ok(()) };
                // CR 608.2b: an illegal (gone) target isn't affected, so it deals no damage.
                if cx.s.obj(src).zone != Zone::Battlefield {
                    return Ok(());
                }
                let ex2 = Exec { last_targets: ex.parent_targets.clone(), ..ex.clone() };
                (src, num(cx, &ex2, amount))
            } else {
                // e.g. "that creature deals 1 damage": last known information if it left.
                let Some(&src) = defined_objs(cx, ex, *source).first() else { return Ok(()) };
                (src, num(cx, ex, amount))
            };
            crate::resolve::deal_damage_each(cx, src, who, dmg)?;
        }
        ExtEffect::LinkMoved { until } => {
            for &o in &ex.moved.clone() {
                cx.s.ext.exiled_with.link(o, ex.source, *until);
            }
            // CR 610.3a/b: if the source already left the battlefield the object doesn't move;
            // the move node's resolution condition (compile.rs, `Duration$`) skips it. This
            // fallback only covers a source that left during the move itself.
            if until.is_some() && cx.s.obj(ex.source).zone != Zone::Battlefield {
                crate::effect_ext::return_linked(cx, ex.source)?;
            }
        }
        ExtEffect::ChooseFromZone { origin, dest, change_type, num: n, owner, chooser, up_to } => {
            let k = num(cx, ex, n).max(0) as usize;
            let owners: SmallVec<[PlayerId; 2]> = if ad.target.is_some() { players(who) } else { defined_players(cx, ex, *owner) };
            let Some(&ch) = defined_players(cx, ex, *chooser).first() else { return Ok(()) };
            let mut new_ids: SmallVec<[ObjId; 2]> = SmallVec::new();
            for p in owners {
                let mut pool: Vec<ObjId> = vec![];
                if origin.has(Zone::Hand) {
                    pool.extend(cx.s.player(p).hand.iter().copied());
                }
                if origin.has(Zone::Library) {
                    pool.extend(cx.s.player(p).library.iter().copied());
                }
                if origin.has(Zone::Graveyard) {
                    pool.extend(cx.s.player(p).graveyard.iter().copied());
                }
                // The chooser looks at the cards (CR 701.20? "look at": revealed to them only).
                for &o in &pool {
                    cx.s.obj_mut(o).known_to |= 1 << ch.0;
                }
                let fcx = exec_fcx!(ex);
                // Tokens there (just died, not yet gone) aren't cards (`query::is_card`).
                let pool: Vec<ObjId> = pool.into_iter().filter(|&o| crate::query::is_card(cx.s, o) && obj_matches(cx.s, cx.db, change_type, o, fcx)).collect();
                let items = crate::query::card_items(cx.s, cx.db, &pool);
                let m = k.min(items.len());
                let min = if *up_to { 0 } else { m };
                let chosen: Vec<ObjId> = cx.choose_set(ch, DecisionKind::Search, items, min, m, Some(ex.source))?.into_iter().filter_map(|o| if let Opt::Card(x) = o { Some(x) } else { None }).collect();
                let moves: Vec<Move> = chosen.iter().map(|&o| Move::new(o, *dest)).collect();
                new_ids.extend(move_batch(cx, &moves)?.into_iter().flatten());
                if origin.has(Zone::Library) {
                    crate::flow::shuffle_library(cx.s, p);
                }
            }
            if ad.remember {
                ex.remembered.extend(new_ids.iter().copied());
            }
            ex.moved = new_ids;
            // "You may cast the exiled card" of the same source (Null Summoner; CR 607.2a).
            if crate::ext7::source_refers_to_exiled(cx.s, cx.db, ex.source) {
                link_exiled(cx, ex, *dest);
            }
        }
        ExtEffect::ChooseType { kind, chooser } => {
            let Some(&p) = defined_players(cx, ex, *chooser).first() else { return Ok(()) };
            let target = ex.replaced.unwrap_or(ex.source);
            let opts = type_options(cx, p, *kind);
            if opts.is_empty() {
                return Ok(());
            }
            let items: Vec<Opt> = opts.iter().map(|&st| Opt::Subtype(st)).collect();
            let Opt::Subtype(st) = cx.choose(Decision { player: p, kind: DecisionKind::ChooseType, options: items, subject: Some(target) })? else { unreachable!() };
            cx.s.ext.chosen_type.retain(|x| x.0 != target);
            cx.s.ext.chosen_type.push((target, st));
        }
        ExtEffect::Earthbend { n, ret } => {
            let k = num(cx, ex, n);
            for o in objs(who) {
                if cx.s.obj(o).zone != Zone::Battlefield {
                    continue;
                }
                let ts = cx.s.ts();
                cx.s.effects.push(ContEffect { ts, affected: o, layer: Layer::L6, kws: KwSet::default().with(Kw::Haste), add_types: TypeSet::default().with(CardType::Creature), set_pt: Some((0, 0)), ..Default::default() });
                crate::ext6::put_counters(cx.s, cx.db, o, CounterKind::P1P1, k);
                let to = ZoneSet(ZoneSet::of(Zone::Graveyard).0 | ZoneSet::of(Zone::Exile).0);
                crate::trig_ext::add_delayed_for(cx, crate::ext::DelayedWhen::Leaves { obj: o, to }, ex.you, ex.source, *ret, o);
            }
        }
        ExtEffect::Reflexive { execute, count, remember } => {
            let amount = match count {
                Some(f) => {
                    let fcx = exec_fcx!(ex);
                    ex.remembered.iter().filter(|&&o| obj_matches(cx.s, cx.db, f, o, fcx)).count() as i32
                }
                None => 0,
            };
            // With remembered objects, the first is "that card" (`DelayTriggerRememberedLKI`).
            if *remember && ex.remembered.len() > 1 && count.is_none() {
                return cx.unsupported("reflexive trigger remembering several objects");
            }
            let obj = if *remember { ex.remembered.first().copied() } else { None };
            crate::trig_ext::add_reflexive(cx, ex.you, ex.source, *execute, TrigCtx { amount, obj, ..Default::default() });
        }
        ExtEffect::TriggerEffect { triggers, duration } => {
            // CR 611.2b (as for `CreateEffect`): the duration never starts if the source left.
            if *duration == EffDuration::AsLongAsInPlay && !matches!(cx.s.obj(ex.source).zone, Zone::Battlefield | Zone::Stack) {
                return Ok(());
            }
            let ts = cx.s.ts();
            let created_turn = cx.s.turn.number;
            cx.s.effect_objs.push(EffectObj {
                source: ex.source,
                controller: ex.you,
                statics: SmallVec::new(),
                remembered: SmallVec::new(),
                duration: *duration,
                created_turn,
                forget_on_moved: ZoneSet::default(),
                exile_on_moved: ZoneSet::default(),
                had_remembered: false,
                ts,
                triggers: triggers.iter().copied().collect(),
            });
        }
        ExtEffect::AtNextEnd { execute } => {
            // One delayed triggered ability for everything the previous node created, token
            // doublers' extra tokens included (CR 603.7a, 603.7c, 614.1a; Kiki-Jiki ruling
            // 2021-03-19), as for mobilize.
            let made: SmallVec<[ObjId; 2]> = ex.moved.clone();
            if !made.is_empty() {
                crate::trig_ext::add_delayed_over(cx, crate::ext::DelayedWhen::NextStep { step: Step::End }, ex.you, ex.source, *execute, &made)?;
            }
        }
        ExtEffect::MakeAttacking => {
            let in_combat = matches!(cx.s.turn.step, Step::DeclareAttackers | Step::DeclareBlockers | Step::FirstStrikeDamage | Step::CombatDamage) && cx.s.turn.active == ex.you;
            if in_combat {
                // CR 508.4: each creature's controller chooses the defending player or one of
                // that player's planeswalkers for it (Forge `addToCombat`: "TokenAttacking$ True"
                // = any defender, per creature).
                for o in ex.moved.clone() {
                    if cx.s.obj(o).zone == Zone::Battlefield && chars(cx.s, cx.db, o).types.has(CardType::Creature) && !cx.s.combat.attackers.iter().any(|a| a.0 == o) {
                        crate::combat::enter_attacking(cx, ex.you, o)?;
                    }
                }
            }
        }
        ExtEffect::MakeAttackingTriggeredDefender => {
            let in_combat = matches!(cx.s.turn.step, Step::DeclareAttackers | Step::DeclareBlockers | Step::FirstStrikeDamage | Step::CombatDamage) && cx.s.turn.active == ex.you;
            // The player or planeswalker the triggering creature attacks (as declared; the
            // defending player if it has left combat since).
            let a = ex.trig.obj;
            let def = a.and_then(|a| cx.s.combat.attackers.iter().find(|x| x.0 == a)).map_or(ex.you.opp(), |x| x.1);
            let pw = a.and_then(|a| cx.s.combat.attacking_pw.iter().find(|x| x.0 == a)).map(|x| x.1);
            // CR 508.4a: a planeswalker that left the battlefield or changed control: not attacking.
            let pw_gone = pw.is_some_and(|pw| cx.s.obj(pw).zone != Zone::Battlefield || cx.s.obj(pw).controller != def);
            if in_combat && !pw_gone {
                for o in ex.moved.clone() {
                    if cx.s.obj(o).zone == Zone::Battlefield && chars(cx.s, cx.db, o).types.has(CardType::Creature) && !cx.s.combat.attackers.iter().any(|x| x.0 == o) {
                        cx.s.combat.attackers.push((o, def));
                        if let Some(pw) = pw {
                            cx.s.combat.attacking_pw.push((o, pw));
                        }
                    }
                }
            }
        }
        ExtEffect::SacrificeDefined => {
            // "Sacrifice them": one event for all of them (CR 701.21a; 603.7c: those no longer
            // on the battlefield aren't affected). The ability's controller follows the
            // instruction (608.2c; the delayed trigger's controller, 603.7d-e) and can't
            // sacrifice a permanent they don't control (701.21a; Stangg ruling: "any tokens you
            // no longer control can't be sacrificed and will remain on the battlefield"; Sneak
            // Attack, Arms Race, Kav Landseeker rulings; XMage `SacrificeTargetEffect` with the
            // source's controller).
            let on: SmallVec<[ObjId; 2]> = objs(who).into_iter().filter(|&o| cx.s.obj(o).zone == Zone::Battlefield && cx.s.obj(o).controller == ex.you).collect();
            for &o in &on {
                let p = cx.s.obj(o).controller;
                crate::trig_ext::fire_sacrificed(cx, &[o], p);
            }
            let moves: Vec<Move> = on.iter().map(|&o| Move::new(o, Zone::Graveyard)).collect();
            move_batch(cx, &moves)?;
        }
        ExtEffect::ManaExiledColors => return cx.unsupported("mana ability on the stack"),
        ExtEffect::Branch { cond, then, otherwise } => {
            let ok = crate::expr::holds(cond, &ex.eval_ctx(cx.s, cx.db));
            if let Some(a) = if ok { then } else { otherwise } {
                for n in crate::resolve::nodes(cx.db, *a, &[]) {
                    crate::resolve::apply_node(cx, ex, n, &[])?;
                }
            }
        }
        ExtEffect::RevealHand => {
            for p in players(who) {
                for &h in &cx.s.player(p).hand.clone() {
                    cx.s.obj_mut(h).known_to = 0b11;
                }
            }
        }
        ExtEffect::Connive { n } => {
            let k = num(cx, ex, n).max(0);
            for o in objs(who) {
                let p = cx.s.obj(o).controller;
                crate::flow::draw_n(cx.s, cx.db, p, k);
                let items = crate::flow::hand_items(cx, p);
                let m = (k as usize).min(items.len());
                let chosen: Vec<ObjId> = cx.choose_set(p, DecisionKind::Discard, items, m, m, Some(o))?.into_iter().filter_map(|c| if let Opt::Card(x) = c { Some(x) } else { None }).collect();
                let nonland = chosen.iter().filter(|&&h| !chars(cx.s, cx.db, h).types.has(CardType::Land)).count() as i32;
                for &h in &chosen {
                    let card = cx.s.obj(h).card;
                    cx.s.emit(Event::Discard { player: p, card });
                }
                move_batch(cx, &chosen.iter().map(|&h| Move::new(h, Zone::Graveyard)).collect::<Vec<_>>())?;
                if nonland > 0 && cx.s.obj(o).zone == Zone::Battlefield {
                    crate::ext6::put_counters(cx.s, cx.db, o, CounterKind::P1P1, nonland);
                }
            }
        }
        ExtEffect::RepeatEachPlayer { players: sel, sub } => {
            let ps: SmallVec<[PlayerId; 2]> = match sel {
                None => smallvec::smallvec![cx.s.turn.active, cx.s.turn.active.opp()],
                Some(d) => {
                    let mut v = players(&ex.last_targets);
                    if v.is_empty() {
                        v = defined_players(cx, ex, *d);
                    }
                    v
                }
            };
            for p in ps {
                cx.s.ext.rep_player = Some(p);
                for n in crate::resolve::nodes(cx.db, *sub, &[]) {
                    crate::resolve::apply_node(cx, ex, n, &[])?;
                }
            }
            cx.s.ext.rep_player = None;
        }
        ExtEffect::LoseAllAbilitiesAll { valid, static_idx } => {
            let fcx = exec_fcx!(ex);
            let rem: SmallVec<[ObjId; 2]> = cx.s.battlefield.iter().copied().filter(|&o| obj_matches(cx.s, cx.db, valid, o, fcx)).collect();
            if !rem.is_empty() {
                let ts = cx.s.ts();
                let created_turn = cx.s.turn.number;
                cx.s.effect_objs.push(EffectObj {
                    source: ex.source,
                    controller: ex.you,
                    statics: smallvec::smallvec![*static_idx],
                    remembered: rem,
                    duration: EffDuration::EndOfTurn,
                    created_turn,
                    forget_on_moved: ZoneSet::of(Zone::Battlefield),
                    exile_on_moved: ZoneSet::default(),
                    had_remembered: true,
                    ts,
                    triggers: SmallVec::new(),
                });
            }
        }
        ExtEffect::SetSolved => {
            for o in objs(who) {
                if cx.s.obj(o).zone == Zone::Battlefield && !cx.s.ext.solved.contains(&o) {
                    cx.s.ext.solved.push(o);
                }
            }
        }
        ExtEffect::NameCard { valid, chooser } => {
            let Some(&p) = defined_players(cx, ex, *chooser).first() else { return Ok(()) };
            // Every legal name (CR 201.4) from the fixed universe (`name_pool`, the same in every
            // game: never the loaded decks, which would reveal the opponent's list), in an order
            // that depends only on what the chooser knows.
            crate::name_pool::check_covered(cx.s, cx.db, valid).map_err(Halt::Unsupported)?;
            let opts: Vec<Opt> = crate::name_pool::offer(cx.s, cx.db, p, valid).into_iter().map(Opt::Name).collect();
            if opts.is_empty() {
                return Err(Halt::Unsupported("NameCard: no card name matches the restriction".into()));
            }
            let Opt::Name(c) = cx.choose(Decision { player: p, kind: DecisionKind::NameCard, options: opts, subject: Some(ex.source) })? else { unreachable!() };
            cx.s.ext.named.retain(|x| x.0 != ex.source);
            cx.s.ext.named.push((ex.source, c));
        }
        ExtEffect::GraveyardMayCast { cards, static_idx } => {
            let fcx = exec_fcx!(ex);
            let rem: SmallVec<[ObjId; 2]> = cx.s.player(ex.you).graveyard.iter().copied().filter(|&o| obj_matches(cx.s, cx.db, cards, o, fcx)).collect();
            if rem.is_empty() {
                return Ok(());
            }
            let ts = cx.s.ts();
            let created_turn = cx.s.turn.number;
            cx.s.effect_objs.push(EffectObj {
                source: ex.source,
                controller: ex.you,
                statics: smallvec::smallvec![*static_idx],
                remembered: rem,
                duration: EffDuration::EndOfTurn,
                created_turn,
                forget_on_moved: ZoneSet::of(Zone::Graveyard),
                exile_on_moved: ZoneSet::default(),
                had_remembered: true,
                ts,
                triggers: SmallVec::new(),
            });
        }
        ExtEffect::CounterTo { zone } => {
            // A targeted ability (`ext8`): removed from the stack (CR 701.6b); no card moves.
            for t in who {
                let Target::Ability { uid, .. } = *t else { continue };
                if let Some(i) = crate::ext8::ability_index(cx.s, uid) {
                    let it = cx.s.stack.remove(i);
                    cx.s.emit(Event::Countered { card: it.source_card });
                }
            }
            for o in objs(who) {
                if cant_be_countered(cx, o) || crate::static_ext::spell_cant_be_countered(cx.s, cx.db, o) && cx.s.stack.iter().any(|it| it.source == o && it.kind == StackKind::Spell) {
                    continue;
                }
                if let Some(i) = cx.s.stack.iter().position(|it| it.source == o && it.kind == StackKind::Spell) {
                    let it = cx.s.stack.remove(i);
                    cx.s.emit(Event::Countered { card: it.source_card });
                    let to = if it.flashback { Zone::Exile } else { *zone };
                    move_batch(cx, &[Move::new(o, to)])?;
                } else if let Some(i) = cx.s.stack.iter().rposition(|it| it.source == o && it.kind != StackKind::Spell) {
                    let it = cx.s.stack.remove(i);
                    cx.s.emit(Event::Countered { card: it.source_card });
                }
            }
        }
        ExtEffect::TokenFor { token, n, tapped, owner } => {
            let k = num(cx, ex, n);
            // Every token this node created is what the next nodes act on, each player's (a
            // per-player assignment kept only the last player's; `invariants::node_end`).
            ex.moved.clear();
            for p in defined_players(cx, ex, *owner) {
                let ids = crate::zones::create_tokens(cx, *token, p, k, *tapped)?;
                ex.moved.extend(ids.iter().copied());
                if ad.remember {
                    ex.remembered.extend(ids);
                }
            }
        }
        ExtEffect::AllCreatureTypes { duration } => {
            let (eot, until) = match duration {
                Duration::EndOfTurn => (true, None),
                Duration::Permanent => (false, None),
                Duration::UntilYourNextTurn => (false, Some(ex.you)),
            };
            for o in objs(who) {
                if cx.s.obj(o).zone == Zone::Battlefield {
                    let ts = cx.s.ts();
                    cx.s.effects.push(ContEffect { ts, affected: o, layer: Layer::L6, until_eot: eot, until_turn_of: until, all_creature_types: true, ..Default::default() });
                }
            }
        }
        ExtEffect::AnimateMoved { add_types, remove_card_types, add_subtypes, set_pt } => {
            // Applied as they entered (`resolve::apply_node`); those must be what it moved.
            let pre = std::mem::take(&mut ex.entered_animated);
            if pre.iter().any(|o| !ex.moved.contains(o)) {
                return cx.unsupported("type change applied to an entering object the effect didn't move");
            }
            for o in ex.moved.clone() {
                if cx.s.obj(o).zone == Zone::Battlefield && !pre.contains(&o) {
                    let ts = cx.s.ts();
                    cx.s.effects.push(ContEffect { ts, affected: o, layer: Layer::L6, add_types: *add_types, remove_card_types: *remove_card_types, add_subtypes: add_subtypes.iter().copied().collect(), set_pt: *set_pt, ..Default::default() });
                }
            }
        }
        ExtEffect::PutOntoBattlefieldAll { origin, change_type, yours, counters } => {
            let fcx = exec_fcx!(ex);
            let mut pool: Vec<ObjId> = vec![];
            if origin.has(Zone::Exile) {
                pool.extend(cx.s.exile.iter().copied());
            }
            for p in [ex.you, ex.you.opp()] {
                if origin.has(Zone::Graveyard) {
                    pool.extend(cx.s.player(p).graveyard.iter().copied());
                }
            }
            let moves: Vec<Move> = pool
                .into_iter()
                .filter(|&o| obj_matches(cx.s, cx.db, change_type, o, fcx))
                .map(|o| Move { obj: o, to: Zone::Battlefield, to_top: false, tapped: false, controller: if *yours { Some(ex.you) } else { None } })
                .collect();
            // "... onto the battlefield with a [kind] counter": they enter with it (CR 122.6), so
            // enters triggers see it (603.6a, 603.10).
            if let Some((k, n)) = counters {
                for m in &moves {
                    cx.s.ext.enter_with.push((m.obj, *k, *n));
                }
            }
            let new: SmallVec<[ObjId; 2]> = move_batch(cx, &moves)?.into_iter().flatten().collect();
            if counters.is_some() {
                cx.s.ext.enter_with.retain(|e| !moves.iter().any(|m| m.obj == e.0));
            }
            ex.moved = new;
        }
        ExtEffect::PumpKwChoice { kws } => {
            let targets: SmallVec<[ObjId; 4]> = objs(who).into_iter().filter(|&o| cx.s.obj(o).zone == Zone::Battlefield).collect();
            if targets.is_empty() {
                return Ok(());
            }
            let options: Vec<Opt> = (0..kws.len() as u8).map(Opt::Mode).collect();
            let Opt::Mode(i) = cx.choose(Decision { player: ex.you, kind: DecisionKind::ChooseMode, options, subject: Some(ex.source) })? else { unreachable!() };
            for o in targets {
                crate::resolve::pump(cx.s, ex.you, o, 0, 0, KwSet::default().with(kws[i as usize]), Duration::EndOfTurn);
            }
        }
        ExtEffect::CopyTriggeredSpell => {
            let Some(o) = ex.trig.obj else { return Ok(()) };
            let Some(it) = cx.s.stack_item_of(o).cloned() else { return Ok(()) };
            if !it.targets.is_empty() {
                return cx.unsupported("copying a spell with targets");
            }
            let (card, face) = (cx.s.obj(o).card, cx.s.obj(o).face);
            let copy = cx.s.new_token(card, ex.you);
            let co = cx.s.obj_mut(copy);
            co.zone = Zone::Stack; // not cast: no mana was spent on it (cast_mana stays 0)
            co.face = face;
            cx.s.push_stack(StackItem { controller: ex.you, source: copy, flashback: false, ..it });
        }
        ExtEffect::Harness => {
            for o in objs(who) {
                if cx.s.obj(o).zone == Zone::Battlefield && !cx.s.ext.harnessed.contains(&o) {
                    cx.s.ext.harnessed.push(o);
                }
            }
        }
        ExtEffect::BecomeCard { card } => {
            for o in objs(who) {
                if cx.s.obj(o).zone != Zone::Battlefield || cx.s.obj(o).card == *card {
                    continue;
                }
                let printed = match cx.s.copy_effects.iter().position(|c| c.obj == o) {
                    Some(i) => cx.s.copy_effects.remove(i).printed,
                    None => cx.s.obj(o).card,
                };
                cx.s.copy_effects.push(CopyEffect { obj: o, printed, name: None, add_types: TypeSet::default(), add_subtypes: SmallVec::new(), kws: KwSet::default(), set_pt: None, transformed: true, add_super: 0 });
                let ts = cx.s.ts();
                let ob = cx.s.obj_mut(o);
                ob.card = *card;
                ob.face = 0;
                ob.ts = ts;
                cx.s.emit(Event::Transformed { obj: o, card: printed, face_card: *card });
            }
        }
        ExtEffect::AttachParentTargets => {
            let Some(&to) = objs(who).first() else { return Ok(()) };
            for e in objs(&ex.parent_targets) {
                if crate::resolve::can_attach(cx.s, cx.db, e, to) && cx.s.obj(e).attached_to != Some(to) {
                    let ts = cx.s.ts();
                    let eo = cx.s.obj_mut(e);
                    eo.attached_to = Some(to);
                    eo.ts = ts;
                    let card = eo.card;
                    cx.s.emit(Event::Attached { card, obj: e, to });
                    if ad.remember {
                        ex.remembered.push(e);
                    }
                }
            }
        }
        ExtEffect::ReflexiveWith { execute, remember } => {
            let obj = defined_objs(cx, ex, *remember).first().copied();
            crate::trig_ext::add_reflexive(cx, ex.you, ex.source, *execute, TrigCtx { obj, ..Default::default() });
        }
        ExtEffect::Fight { parent } => {
            let Some(&b) = objs(who).first() else { return Ok(()) };
            // `ParentTarget` compiles to `Defined::Targeted`, but this node targets too, so
            // `last_targets` is its own target: the parent's are `parent_targets` (empty if the
            // parent's target became illegal: no fight, CR 701.14b).
            let a = if *parent { objs(&ex.parent_targets).first().copied() } else { defined_objs(cx, ex, ad.defined.unwrap_or(Defined::SelfObj)).first().copied() };
            let Some(a) = a else { return Ok(()) };
            // CR 701.14b: no damage if either isn't a creature on the battlefield any more.
            let ok = |cx: &Ctx, o: ObjId| cx.s.obj(o).zone == Zone::Battlefield && chars(cx.s, cx.db, o).types.has(CardType::Creature);
            if a == b || !ok(cx, a) || !ok(cx, b) {
                return Ok(());
            }
            let (pa, pb) = (chars(cx.s, cx.db, a).power.max(0), chars(cx.s, cx.db, b).power.max(0));
            crate::resolve::deal_damage(cx, a, Target::Obj(b), pa, false)?;
            crate::resolve::deal_damage(cx, b, Target::Obj(a), pb, false)?;
        }
        ExtEffect::Punisher { times, life, sac, discard } => {
            // Rottenmouth Viper rulings 2024-07-26: the opponent makes every choice first (a card to
            // discard, a nonland permanent to sacrifice, or neither; one already chosen can't be
            // chosen again), "Finally, that player discards the chosen cards, sacrifices the
            // chosen permanents, and loses 4 life the appropriate number of times": one event
            // (CR 608.2c, 603.10a).
            let n = num(cx, ex, times).max(0);
            let opp = ex.you.opp();
            let (mut sacs, mut discards, mut losses): (SmallVec<[ObjId; 2]>, SmallVec<[ObjId; 2]>, i32) = (SmallVec::new(), SmallVec::new(), 0);
            for _ in 0..n {
                let fcx = FilterCtx { you: opp, x: 0, ..exec_fcx!(ex) };
                let perms: Vec<(crate::query::ObjSig, Opt)> = match sac {
                    Some(f) => crate::query::sig_items(cx.s, cx.db, &cx.s.battlefield.iter().copied().filter(|&o| cx.s.obj(o).controller == opp && !sacs.contains(&o) && obj_matches(cx.s, cx.db, f, o, fcx)).collect::<Vec<ObjId>>()),
                    None => vec![],
                };
                let hand: Vec<(crate::query::ObjSig, Opt)> = crate::flow::hand_items(cx, opp).into_iter().filter(|(_, o)| !matches!(o, Opt::Card(h) if discards.contains(h))).collect();
                let can_discard = *discard && !hand.is_empty();
                let mut options = vec![];
                if !perms.is_empty() {
                    options.push(Opt::Mode(0));
                }
                if can_discard {
                    options.push(Opt::Mode(1));
                }
                options.push(Opt::Mode(2));
                match cx.choose(Decision { player: opp, kind: DecisionKind::ChooseMode, options, subject: Some(ex.source) })? {
                    Opt::Mode(0) => sacs.extend(cx.choose_set(opp, DecisionKind::Sacrifice, perms, 1, 1, Some(ex.source))?.into_iter().filter_map(|c| if let Opt::Card(o) = c { Some(o) } else { None })),
                    Opt::Mode(1) => discards.extend(cx.choose_set(opp, DecisionKind::Discard, hand, 1, 1, Some(ex.source))?.into_iter().filter_map(|c| if let Opt::Card(o) = c { Some(o) } else { None })),
                    _ => losses += 1,
                }
            }
            if !sacs.is_empty() {
                crate::trig_ext::fire_sacrificed(cx, &sacs, opp);
            }
            for &h in &discards {
                let card = cx.s.obj(h).card;
                cx.s.emit(Event::Discard { player: opp, card });
            }
            let moves: Vec<Move> = discards.iter().chain(&sacs).map(|&o| Move::new(o, Zone::Graveyard)).collect();
            if !moves.is_empty() {
                move_batch(cx, &moves)?;
            }
            for _ in 0..losses {
                crate::resolve::lose_life(cx.s, cx.db, opp, *life);
            }
        }
        ExtEffect::AddMana(v) => {
            for &t in v {
                let slot = &mut cx.s.ext.pool[ex.you.idx()][t as usize];
                *slot = slot.saturating_add(1);
            }
        }
        ExtEffect::PumpMoved { kws } => {
            for o in ex.moved.clone() {
                crate::resolve::pump(cx.s, ex.you, o, 0, 0, *kws, Duration::EndOfTurn);
            }
        }
        ExtEffect::More(m) => return crate::ext2::apply(cx, ex, ad, m, _tg, who),
        ExtEffect::Lt(m) => return crate::ext3::apply(cx, ex, ad, m, _tg, who),
        ExtEffect::Mr(m) => return crate::ext4::apply(cx, ex, ad, m, _tg, who),
        ExtEffect::Le(m) => return crate::ext5::apply(cx, ex, ad, m, _tg, who),
        ExtEffect::X(m) => return crate::ext6::apply(cx, ex, ad, m, _tg, who),
        ExtEffect::X7(m) => return crate::ext7::apply(cx, ex, ad, m, _tg, who),
        ExtEffect::X8(m) => return crate::ext8::apply(cx, ex, m, _tg),
        ExtEffect::X9(m) => return crate::ext9::apply(cx, ex, ad, m, _tg, who),
        ExtEffect::ChooseCard { choices, chooser, n, up_to, zone } => {
            let chooser_p = if ad.target.is_some() && *chooser == Defined::Targeted { players(who).first().copied() } else { defined_players(cx, ex, *chooser).first().copied() };
            let Some(p) = chooser_p else { return Ok(()) };
            let k = num(cx, ex, n).max(0) as usize;
            let fcx = exec_fcx!(ex);
            let pool: &[ObjId] = if *zone == Zone::Exile { &cx.s.exile } else { &cx.s.battlefield };
            let objs: Vec<ObjId> = pool.iter().copied().filter(|&o| obj_matches(cx.s, cx.db, choices, o, fcx)).collect();
            let items = crate::query::sig_items(cx.s, cx.db, &objs);
            let m = k.min(items.len());
            let min = if *up_to { 0 } else { m };
            let picked = cx.choose_set(p, DecisionKind::ChooseCard, items, min, m, Some(ex.source))?;
            for c in picked {
                if let Opt::Card(o) = c {
                    cx.s.ext.chosen_cards.push((ex.source, o));
                    if ad.remember {
                        ex.remembered.push(o);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Types offered by `ChooseType`. Basic land types: all five. Creature types: those of the
/// cards the chooser owns (their own deck is known to them); any other type has the same effect
/// as naming a type none of their cards has, so one such choice would be a meaningless
/// duplicate (it is not offered unless nothing else is). Ordered by subtype index.
fn type_options(cx: &Ctx, p: PlayerId, kind: TypeChoice) -> Vec<Subtype> {
    let mut out: Vec<Subtype> = vec![];
    match kind {
        TypeChoice::BasicLand => {
            for n in ["Plains", "Island", "Swamp", "Mountain", "Forest"] {
                if let Some(st) = cx.db.subtype(n) {
                    out.push(st);
                }
            }
        }
        TypeChoice::Creature => {
            for o in &cx.s.objs {
                if o.owner != p || o.token {
                    continue;
                }
                for f in &cx.db.get(o.card).faces {
                    if f.types.has(CardType::Creature) || f.types.has(CardType::Kindred) {
                        for &st in &f.subtypes {
                            if cx.db.creature_types.contains(&st) && !out.contains(&st) {
                                out.push(st);
                            }
                        }
                    }
                }
            }
            out.sort();
            if out.is_empty() {
                out.extend(cx.db.creature_types.first().copied());
            }
        }
    }
    out
}

/// Return objects exiled "until `src` leaves the battlefield" (CR 610.3a).
pub fn return_linked(cx: &mut Ctx, src: ObjId) -> R<()> {
    return_linked_all(cx, &[src])
}

/// Return the objects exiled until any of `srcs` leaves the battlefield, all in one event (CR
/// 610.3, 610.3d: the one-shot effects created by simultaneous events are simultaneous).
fn return_linked_all(cx: &mut Ctx, srcs: &[ObjId]) -> R<()> {
    let back = cx.s.ext.exiled_with.take_until_leaves(srcs);
    if back.is_empty() {
        return Ok(());
    }
    let mut moves: Vec<Move> = vec![];
    for e in back.iter() {
        if cx.s.obj(e.obj).zone != Zone::Exile {
            continue;
        }
        let card = cx.s.obj(e.obj).card;
        match (cx.db.get(card).front().enchant.clone(), e.until_leaves.unwrap()) {
            (Some(_), Zone::Battlefield) => {
                aura_onto_battlefield(cx, e.obj)?;
            }
            (_, z) => moves.push(Move::new(e.obj, z)),
        }
    }
    move_batch(cx, &moves)?;
    Ok(())
}

/// The objects still exiled with `src` (CR 607.2a: "the exiled card" / cards "exiled with
/// [this]" are only those still in exile; one that left is a new object, 400.7, and a token
/// that went there ceased to exist, 111.7). Every reader that asks "which cards are exiled with
/// this" goes through here; the link list itself is private (`ext::ExiledLinks`).
pub fn still_exiled_with(s: &State, src: ObjId) -> impl Iterator<Item = ObjId> + '_ {
    s.ext.exiled_with.still_exiled_with(&s.objs, src)
}

/// Remember which objects a resolving ability exiled, for "cards exiled with this" (CR 607.2a).
pub fn link_exiled(cx: &mut Ctx, ex: &Exec, dest: Zone) {
    if dest != Zone::Exile {
        return;
    }
    for &o in &ex.moved {
        if cx.s.obj(o).zone == Zone::Exile && !cx.s.ext.exiled_with_src(o, ex.source) {
            cx.s.ext.exiled_with.link(o, ex.source, None);
        }
    }
}

/// Forget per-resolution choices once an ability has finished resolving.
pub fn end_resolution(cx: &mut Ctx, source: ObjId) {
    cx.s.ext.chosen_cards.retain(|x| x.0 != source);
    cx.s.ext.enter_with.clear();
    cx.s.ext.res_remembered.clear();
    // A triggered ability whose source is a spell still on the stack (storm, CR 702.40a) must
    // leave that spell's own casting records alone.
    let spell_on_stack = cx.s.stack.iter().any(|it| it.source == source && it.kind == StackKind::Spell);
    if !spell_on_stack {
        if let Some(i) = cx.s.ext.cost_sacrificed.iter().rposition(|x| x.0 == source) {
            cx.s.ext.cost_sacrificed.remove(i);
        }
        if !cx.s.ext.divided.is_empty() {
            cx.s.ext.divided.retain(|x| x.0 != source);
        }
    }
    cx.s.ext.res_targets.clear();
}

/// Called by `zones::move_batch` after objects moved: history, and objects exiled until a
/// permanent leaves the battlefield come back (CR 610.3a).
pub fn after_moves(cx: &mut Ctx, moved: &[(ObjId, Zone, Zone, Option<ObjId>)]) -> R<()> {
    for &(old, from, to, new) in moved {
        cx.s.ext.moved_this_turn.push((old, from, to, new.unwrap_or(old)));
    }
    if cx.s.ext.exiled_with.any_until_leaves() {
        let left: smallvec::SmallVec<[ObjId; 4]> = moved.iter().filter(|m| m.1 == Zone::Battlefield).map(|m| m.0).collect();
        return_linked_all(cx, &left)?;
    }
    Ok(())
}

/// Mana types a dynamic mana ability of `src` can produce now (`ManaType` bitmask).
pub fn dynamic_mana(s: &State, db: &CardDb, src: ObjId, x: &ExtEffect) -> Option<u8> {
    match x {
        ExtEffect::ManaExiledColors => {
            let mut mask = 0u8;
            for o in still_exiled_with(s, src) {
                for c in crate::query::chars(s, db, o).colors.iter() {
                    mask |= 1 << ManaType::of(c) as u8;
                }
            }
            Some(mask)
        }
        _ => None,
    }
}

/// Has `ability` of `o` been activated as often as its `ActivationLimit$` allows this turn?
pub fn activation_limit_reached(s: &State, db: &CardDb, o: ObjId, ability: AbilityId) -> bool {
    if crate::ext6::gained_limit_reached(s, db, o, ability) {
        return true;
    }
    if !db.exhaust.is_empty() && db.exhaust.contains(&ability) && s.ext.exhausted.contains(&(o, ability)) {
        return true;
    }
    match db.activation_limit.iter().find(|x| x.0 == ability) {
        None => false,
        Some(&(_, n)) => s.ext.activations.iter().find(|x| x.0 == o && x.1 == ability).map(|x| x.2).unwrap_or(0) >= n,
    }
}

pub fn count_activation(s: &mut State, db: &CardDb, o: ObjId, ability: AbilityId) {
    crate::ext6::note_gained_use(s, db, o, ability);
    if db.exhaust.contains(&ability) {
        s.ext.exhausted.push((o, ability));
    }
    if db.activation_limit.iter().any(|x| x.0 == ability) {
        match s.ext.activations.iter_mut().find(|x| x.0 == o && x.1 == ability) {
            Some(x) => x.2 += 1,
            None => s.ext.activations.push((o, ability, 1)),
        }
    }
}

/// Is the spell `o` on the stack uncounterable (Cavern of Souls mana, CR 101.2)?
pub fn cant_be_countered(cx: &Ctx, o: ObjId) -> bool {
    cx.s.ext.uncounterable.contains(&o) && cx.s.stack.iter().any(|it| it.source == o && it.kind == StackKind::Spell)
}


/// CR 303.4f: an Aura put onto the battlefield without being cast: its owner chooses something
/// it can enchant (not targeted); if there's nothing, it stays where it is (303.4g).
pub fn aura_onto_battlefield(cx: &mut Ctx, obj: ObjId) -> R<Option<ObjId>> {
    let card = cx.s.obj(obj).card;
    let Some(f) = cx.db.get(card).front().enchant.clone() else { return Ok(None) };
    let owner = cx.s.obj(obj).owner;
    let fcx = FilterCtx { you: owner, source: Some(obj), x: 0, rem: &[] };
    let opts: Vec<Opt> = cx.s.battlefield.iter().copied().filter(|&o| obj_matches(cx.s, cx.db, &f, o, fcx)).map(Opt::Card).collect();
    if opts.is_empty() {
        return Ok(None);
    }
    let Opt::Card(to) = cx.choose(Decision { player: owner, kind: DecisionKind::ChooseCard, options: opts, subject: Some(obj) })? else { unreachable!() };
    crate::zones::move_attached(cx, obj, owner, to)
}
