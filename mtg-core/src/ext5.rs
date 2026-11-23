//! Long-tail mechanics, fifth batch:
//! ending the turn (Ultima, CR 723), priority in the cleanup step (CR 514.3a), doubling the
//! counters of a filtered set (Bristly Bill) and more. Kept in its own module to stay additive
//! (like `ext2`..`ext4`).

use crate::basics::*;
use crate::carddb::*;
use crate::engine::*;
use crate::filter::Filter;
use crate::query::*;
use crate::state::*;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// Per-game state of this batch (`Ext::le`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeState {
    /// State-based actions were performed or triggered abilities were put onto the stack during
    /// this cleanup step, so players get priority and another cleanup step follows (CR 514.3a).
    pub cleanup_again: bool,
    /// Last known information on attachments: (permanent that left the battlefield this turn,
    /// object attached to it as it left). "An Equipment that was attached to [it]" (Zack Fair).
    pub attached_lki: SmallVec<[(ObjId, ObjId); 1]>,
    /// Cards each player drew this turn (CR 121.2; "your second card each turn").
    pub draws: [u8; 2],
    /// The batch of moves being matched by `ZoneBatchAfter` triggers: (old, from, to, new).
    /// Empty except while those triggers are checked.
    pub batch: SmallVec<[(ObjId, Zone, Zone, Option<ObjId>); 2]>,
    /// Remembered objects of fired "your next [step]" delayed triggers, until they resolve:
    /// (source, ability, objects).
    pub delayed_rem: SmallVec<[(ObjId, AbilityId, SmallVec<[ObjId; 4]>); 1]>,
    /// Creatures tapped to pay a "tap an untapped creature" cost: (ability source, creature).
    pub cost_tapped: SmallVec<[(ObjId, ObjId); 1]>,
}

impl LeState {
    /// As each turn begins (CR 500).
    pub fn new_turn(&mut self) {
        self.cleanup_again = false;
        self.attached_lki.clear();
        self.draws = [0; 2];
        self.cost_tapped.clear();
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeEffect {
    /// "End the turn" (CR 723.1).
    EndTurn,
    /// "Double the number of `kind` counters on each [valid]" (Bristly Bill; Forge
    /// `MultiplyCounter | Defined$ Valid <filter>`; CR 701.10e-style doubling of counters means
    /// putting that many more on it, 122.x).
    MultiplyCounterAll { valid: Filter, kind: CounterKind, factor: i32 },
    /// "Attach an Equipment that was attached to [this] to that creature" (Zack Fair; Forge
    /// `Attach | Choices$ Equipment.Attached | Defined$ Targeted`): the source has left the
    /// battlefield (sacrificed as a cost), so "was attached" is last known information. The
    /// controller chooses one such Equipment still on the battlefield (CR 701.3).
    AttachLkiEquipment,
    /// "Sacrifice [these]" / "Each player sacrifices all [valid]" (Forge `SacrificeAll`):
    /// every defined object, or every permanent matching `valid`, is sacrificed by its
    /// controller at once (CR 701.21a; permanents only).
    SacrificeAll { valid: Option<Filter> },
    /// "Remove `n` `kind` counters from [defined]" (Forge `RemoveCounter | Defined$`, CR 122):
    /// as many as it has, up to `n`.
    RemoveCounter { kind: CounterKind, n: crate::expr::Expr },
    /// Remember the previous node's (legal) targets (`RememberObjects$ ThisTargetedCard` on a
    /// targeted node, Burn Together): a later `Defined$ Remembered` acts on them.
    RememberTargets,
    /// "Destroy target [permanent] and all other permanents with the same name as that
    /// permanent" (Maelstrom Pulse; CR 201.2), after a target-only node.
    DestroySameName,
    /// "Amass [subtype] N" (CR 701.47a) by each defined player (default: you): with no Army
    /// creature, create a 0/0 black [subtype] Army token first; choose an Army you control, put
    /// N +1/+1 counters on it, and it becomes a [subtype] if it isn't.
    Amass { token: CardId, subtype: Subtype, n: crate::expr::Expr },
    /// Break Out: "Look at the top `n` cards of your library. You may reveal a creature card
    /// from among them. If that card has mana value `max_mv` or less, you may put it onto the
    /// battlefield and it gains haste until end of turn. If you didn't put the revealed card
    /// onto the battlefield this way, put it into your hand. Put the rest on the bottom of your
    /// library in a random order." (Forge's `PeekAndReveal` + `ChooseCard` + conditional
    /// `ChangeZone`s chain, compiled as one effect.)
    LookRevealCreature { n: u8, max_mv: u32 },
    /// "At the beginning of your next [step], [execute]" remembering this resolution's
    /// remembered objects (Forge `DelayedTrigger | Mode$ Phase | ValidPlayer$ You |
    /// RememberObjects$ Remembered`; CR 603.7).
    DelayedYourStep {
        step: Step,
        execute: AbilityId,
        /// `RememberObjects$ Targeted` (Casey Jones): the previous targeted node's targets
        /// instead of the remembered objects.
        #[serde(default)]
        targeted: bool,
    },
    /// Station (CR 702.184a): put `kind` (charge) counters on the source equal to the power of
    /// the creature tapped to pay the cost, as the ability resolves (last known if it left;
    /// rulings).
    Station { kind: CounterKind },
    /// Marker after a `CopyPermanent | AddTypes$ Legendary` node ("create a token that's a copy
    /// of ..., except it's legendary"): a copy exception, part of the token's copiable values
    /// (CR 707.9b), applied as the token is created (`LeTables::copy_legendary`); this node
    /// does nothing.
    CopiesLegendary,
    /// "Remove a `kind` counter from each of `n` [valid] you control" (Dyadrine; Forge
    /// `RemoveCounter | Choices$ | ChoiceNum$ N | RememberAmount$ True`): all or nothing (with
    /// fewer than `n` such permanents nothing is removed); the permanents are remembered, so
    /// `Count$RememberedNumber` is the number of counters removed.
    RemoveCounterEach { valid: Filter, kind: CounterKind, n: u8 },
}

pub fn apply(cx: &mut Ctx, ex: &mut crate::resolve::Exec, _ad: &AbilityDef, x: &LeEffect, _tg: &[Target], _who: &[Target]) -> R<()> {
    match x {
        LeEffect::EndTurn => end_the_turn(cx, ex),
        LeEffect::RemoveCounterEach { valid, kind, n } => {
            let fcx = exec_fcx!(ex);
            let objs: Vec<ObjId> = cx.s.battlefield.iter().copied().filter(|&o| cx.s.obj(o).controller == ex.you && cx.s.obj(o).counter(*kind) > 0 && obj_matches(cx.s, cx.db, valid, o, fcx)).collect();
            let items = crate::query::sig_items(cx.s, cx.db, &objs);
            if items.len() < *n as usize {
                return Ok(());
            }
            let k = *n as usize;
            for c in cx.choose_set(ex.you, DecisionKind::ChooseCard, items, k, k, Some(ex.source))? {
                if let Opt::Card(o) = c {
                    crate::ext6::put_counters(cx.s, cx.db, o, *kind, -1);
                    ex.remembered.push(o);
                }
            }
            Ok(())
        }
        // The exception is already part of the token's copy effect from its creation
        // (`LeTables::copy_legendary`, `zones::CopyExc::add_super`): applied after the token
        // entered, its enters triggers saw it nonlegendary (Adagia copying an artifact creature
        // under The Irencrag). The node stays as a marker so ability ids don't shift.
        LeEffect::CopiesLegendary => Ok(()),
        LeEffect::Station { kind } => {
            let Some(i) = cx.s.ext.le.cost_tapped.iter().rposition(|x| x.0 == ex.source) else { return Ok(()) };
            let (_, t) = cx.s.ext.le.cost_tapped.remove(i);
            let n = chars(cx.s, cx.db, t).power.max(0);
            if cx.s.obj(ex.source).zone == Zone::Battlefield && n > 0 {
                crate::ext6::put_counters(cx.s, cx.db, ex.source, *kind, n);
            }
            Ok(())
        }
        LeEffect::DelayedYourStep { step, execute, targeted } => {
            let rem: SmallVec<[ObjId; 4]> = if *targeted { ex.last_targets.iter().filter_map(|t| if let Target::Obj(o) = t { Some(*o) } else { None }).collect() } else { ex.remembered.clone() };
            crate::trig_ext::add_delayed_over(cx, crate::ext::DelayedWhen::PlayersNextStep { step: *step, player: ex.you }, ex.you, ex.source, *execute, &rem)
        }
        LeEffect::LookRevealCreature { n, max_mv } => {
            let p = ex.you;
            let top: Vec<ObjId> = cx.s.player(p).library.iter().rev().take(*n as usize).copied().collect();
            for &o in &top {
                cx.s.obj_mut(o).known_to |= 1 << p.0; // the player looks at them
            }
            cx.s.forget_arranged(p, &top);
            let creatures: Vec<ObjId> = top.iter().copied().filter(|&o| chars(cx.s, cx.db, o).types.has(CardType::Creature)).collect();
            let items = crate::query::card_items(cx.s, cx.db, &creatures);
            let chosen = cx.choose_set(p, DecisionKind::Dig, items, 0, 1, Some(ex.source))?;
            let picked = chosen.first().and_then(|c| if let Opt::Card(o) = c { Some(*o) } else { None });
            let mut others: Vec<ObjId> = top.iter().copied().filter(|&o| Some(o) != picked).collect();
            // The rest go to the bottom in a random order (seeded game RNG).
            cx.s.rng.shuffle(&mut others);
            {
                let lib = &mut cx.s.player_mut(p).library;
                lib.retain(|o| !others.contains(o));
                for &o in &others {
                    lib.insert(0, o);
                }
            }
            let Some(c) = picked else { return Ok(()) };
            cx.s.obj_mut(c).known_to = 0b11; // revealed
            let to_bf = chars(cx.s, cx.db, c).mv <= *max_mv && cx.choose(Decision { player: p, kind: DecisionKind::OptionalEffect, options: vec![Opt::Yes, Opt::No], subject: Some(ex.source) })? == Opt::Yes;
            if to_bf {
                let new = crate::zones::move_batch(cx, &[crate::zones::Move { obj: c, to: Zone::Battlefield, to_top: false, tapped: false, controller: Some(p) }])?;
                if let Some(Some(nid)) = new.first() {
                    if cx.s.obj(*nid).zone == Zone::Battlefield {
                        crate::resolve::pump(cx.s, p, *nid, 0, 0, KwSet::default().with(Kw::Haste), Duration::EndOfTurn);
                        return Ok(());
                    }
                }
                Ok(())
            } else {
                let new = crate::zones::move_batch(cx, &[crate::zones::Move::new(c, Zone::Hand)])?;
                if let Some(Some(nid)) = new.first() {
                    cx.s.obj_mut(*nid).known_to = 0b11;
                }
                Ok(())
            }
        }
        LeEffect::Amass { token, subtype, n } => {
            let k = crate::resolve::amount(cx, ex, n).max(0);
            let mut ps = crate::resolve::players(_who);
            if ps.is_empty() && _ad.defined.is_none() {
                ps.push(ex.you);
            }
            let Some(army) = cx.db.subtype("Army") else { return cx.unsupported("Amass without the Army subtype interned") };
            for p in ps {
                let armies = |s: &State| -> Vec<ObjId> { s.battlefield.iter().copied().filter(|&o| s.obj(o).controller == p && { let c = chars(s, cx.db, o); c.types.has(CardType::Creature) && c.subtypes.contains(&army) }).collect() };
                if armies(cx.s).is_empty() {
                    crate::zones::create_tokens(cx, *token, p, 1, false)?;
                }
                let cands = armies(cx.s);
                let a = match cands.len() {
                    0 => continue, // a token doubler / replacement left none (nothing to do)
                    1 => cands[0],
                    _ => {
                        let items = crate::query::sig_items(cx.s, cx.db, &cands);
                        match cx.choose_set(p, DecisionKind::ChooseCard, items, 1, 1, Some(ex.source))?.first() {
                            Some(Opt::Card(o)) => *o,
                            _ => continue,
                        }
                    }
                };
                crate::ext6::put_counters(cx.s, cx.db, a, CounterKind::P1P1, k);
                if !chars(cx.s, cx.db, a).subtypes.contains(subtype) {
                    let ts = cx.s.ts();
                    cx.s.effects.push(ContEffect { ts, affected: a, add_subtypes: smallvec::smallvec![*subtype], ..Default::default() });
                }
            }
            Ok(())
        }
        LeEffect::DestroySameName => {
            // CR 201.2a: the names the permanents have, a copy's name exception included (707.9b);
            // objects without a name (face-down, 708.2a) share no name.
            let Some(&t) = crate::resolve::objs(&ex.last_targets).first() else { return Ok(()) };
            if cx.s.obj(t).zone != Zone::Battlefield {
                return Ok(());
            }
            let v: Vec<ObjId> = cx.s.battlefield.iter().copied().filter(|&o| o == t || crate::query::same_name(cx.s, cx.db, t, o)).collect();
            crate::resolve::destroy_all(cx, &v)
        }
        LeEffect::RememberTargets => {
            for o in crate::resolve::objs(&ex.last_targets) {
                if !ex.remembered.contains(&o) {
                    ex.remembered.push(o);
                }
            }
            Ok(())
        }
        LeEffect::RemoveCounter { kind, n } => {
            let k = crate::resolve::amount(cx, ex, n).max(0);
            for o in crate::resolve::objs(_who) {
                if cx.s.obj(o).zone == Zone::Battlefield {
                    let have = cx.s.obj(o).counter(*kind);
                    crate::ext6::put_counters(cx.s, cx.db, o, *kind, -k.min(have));
                }
            }
            Ok(())
        }
        LeEffect::SacrificeAll { valid } => {
            let objs: Vec<ObjId> = match valid {
                Some(f) => {
                    let fcx = exec_fcx!(ex);
                    cx.s.battlefield.iter().copied().filter(|&o| obj_matches(cx.s, cx.db, f, o, fcx)).collect()
                }
                // "Then sacrifice it" (`Defined$`, Burn Together): the ability's controller
                // sacrifices it only if they control it (CR 608.2c, 701.21a; as
                // `ExtEffect::SacrificeDefined`).
                None => crate::resolve::objs(_who).into_iter().filter(|&o| cx.s.obj(o).zone == Zone::Battlefield && cx.s.obj(o).controller == ex.you).collect(),
            };
            for p in [cx.s.turn.active, cx.s.turn.active.opp()] {
                let mine: Vec<ObjId> = objs.iter().copied().filter(|&o| cx.s.obj(o).controller == p).collect();
                crate::trig_ext::fire_sacrificed(cx, &mine, p);
            }
            let moves: Vec<crate::zones::Move> = objs.iter().map(|&o| crate::zones::Move::new(o, Zone::Graveyard)).collect();
            let new = crate::zones::move_batch(cx, &moves)?;
            if _ad.remember {
                ex.remembered.extend(new.into_iter().flatten());
            }
            Ok(())
        }
        LeEffect::AttachLkiEquipment => {
            let Some(&to) = crate::resolve::objs(&ex.last_targets).first() else { return Ok(()) };
            let eq = cx.db.subtype("Equipment");
            let mut opts = vec![];
            for &(host, att) in &cx.s.ext.le.attached_lki {
                let ok = host == ex.source && cx.s.obj(att).zone == Zone::Battlefield && eq.is_some_and(|st| chars(cx.s, cx.db, att).subtypes.contains(&st)) && crate::resolve::can_attach(cx.s, cx.db, att, to);
                if ok && !opts.contains(&Opt::Card(att)) {
                    opts.push(Opt::Card(att));
                }
            }
            if opts.is_empty() {
                return Ok(());
            }
            let c = cx.choose(Decision { player: ex.you, kind: DecisionKind::ChooseCard, options: opts, subject: Some(ex.source) })?;
            let Opt::Card(att) = c else { return Ok(()) };
            if cx.s.obj(att).attached_to != Some(to) {
                let ts = cx.s.ts();
                let so = cx.s.obj_mut(att);
                so.attached_to = Some(to);
                so.ts = ts;
                let card = so.card;
                cx.s.emit(Event::Attached { card, obj: att, to });
            }
            Ok(())
        }
        LeEffect::MultiplyCounterAll { valid, kind, factor } => {
            let fcx = exec_fcx!(ex);
            let objs: Vec<ObjId> = cx.s.battlefield.iter().copied().filter(|&o| obj_matches(cx.s, cx.db, valid, o, fcx)).collect();
            for o in objs {
                let have = cx.s.obj(o).counter(*kind);
                crate::ext6::put_counters(cx.s, cx.db, o, *kind, crate::num::mul(have, factor - 1));
            }
            Ok(())
        }
    }
}

/// CR 723.1: (a) exile every object on the stack, including the resolving spell; (b) remove all
/// creatures and planeswalkers from combat; (c) state-based actions are checked, but no player
/// gets priority and no triggered ability is put onto the stack yet; (d) the current phase and
/// step end and the game skips straight to the cleanup step, where abilities that triggered are
/// put onto the stack and players get priority (514.3a, `cleanup_priority`).
fn end_the_turn(cx: &mut Ctx, ex: &mut crate::resolve::Exec) -> R<()> {
    // CR 724.1a: abilities that triggered before this process began but aren't on the stack yet
    // (e.g. dies triggers from Ultima's own "destroy all") cease to exist; those triggering during
    // it (the exile below, state-based actions) are put on the stack in the cleanup step (724.1f).
    // Ultima ruling 2025-06-06; Forge `EndTurnEffect` clears the waiting triggers first, XMage
    // `Turn.endTurn` clears the triggered abilities.
    cx.s.pending_triggers.clear();
    let mut moves: Vec<crate::zones::Move> = vec![];
    // The resolving spell (already off the stack list, still in the stack zone).
    if cx.s.obj(ex.source).zone == Zone::Stack {
        moves.push(crate::zones::Move::new(ex.source, Zone::Exile));
    }
    let items = std::mem::take(&mut cx.s.stack);
    for it in items.iter().rev() {
        if it.kind == StackKind::Spell && cx.s.obj(it.source).zone == Zone::Stack && !moves.iter().any(|m| m.obj == it.source) {
            moves.push(crate::zones::Move::new(it.source, Zone::Exile));
        }
    }
    // Copies of spells cease to exist (CR 707.10a); cards go to exile.
    let (copies, cards): (Vec<_>, Vec<_>) = moves.into_iter().partition(|m| cx.s.obj(m.obj).token);
    for m in copies {
        cx.s.move_obj(m.obj, Zone::Gone, false);
    }
    crate::zones::move_batch(cx, &cards)?;
    cx.s.combat = Combat::default();
    crate::sba::check(cx)?;
    crate::flow::skip_to_cleanup(cx.s);
    Ok(())
}

/// CR 514.3a: in the cleanup step, if state-based actions were performed or triggered abilities
/// were put onto the stack, the active player gets priority, and once the stack is empty and all
/// players pass, another cleanup step begins. Called by `flow::play_unit` (`acted`: SBAs or
/// triggers happened this unit). Returns true when players get priority now.
pub fn cleanup_priority(s: &mut State, acted: bool) -> bool {
    if s.turn.step != Step::Cleanup {
        return false;
    }
    if acted {
        s.ext.le.cleanup_again = true;
    }
    s.ext.le.cleanup_again || !s.stack.is_empty()
}

/// All players passed with an empty stack during a cleanup step that gave priority: another
/// cleanup step follows (CR 514.3a). Returns true when that happens instead of ending the turn.
pub fn repeat_cleanup(s: &mut State) -> bool {
    if s.turn.step == Step::Cleanup && s.ext.le.cleanup_again {
        s.ext.le.cleanup_again = false;
        s.turn.tba_done = false;
        return true;
    }
    false
}

impl CardDb {
    /// Effect APIs of this batch (`None`: not one of them; keys untouched).
    pub(crate) fn le_api(&mut self, api: &str, p: &crate::compile::P, f: &forge_script::Face, uns: &mut Vec<String>, depth: u32) -> Result<Option<Effect>, String> {
        Ok(Some(Effect::Ext(crate::effect_ext::ExtEffect::Le(match api {
            "DelayedTrigger" if p.peek("Mode") == Some("Phase") && p.peek("ValidPlayer") == Some("You") && matches!(p.peek("RememberObjects"), Some("Remembered") | Some("Targeted")) => {
                let targeted = p.peek("RememberObjects") == Some("Targeted");
                p.ack("Mode", &["Phase"], "checked above");
                p.ack("ValidPlayer", &["You"], "checked above");
                p.ack("RememberObjects", &["Remembered", "Targeted"], "checked above");
                let step = match p.get("Phase") {
                    Some("End of Turn") => Step::End,
                    Some("Upkeep") => Step::Upkeep,
                    ph => return Err(format!("DelayedTrigger Phase {ph:?}")),
                };
                if p.peek("ThisTurn").is_some() || p.peek("DelayedTriggerDefinedPlayer").is_some() {
                    return Err("DelayedTrigger Phase form".into());
                }
                let exec = p.get("Execute").ok_or("DelayedTrigger without Execute")?;
                let body = f.svar(exec).and_then(|s| s.as_ability()).ok_or("DelayedTrigger Execute missing")?;
                let execute = self.ability_inner(&body, f, uns, depth + 1, false)?;
                LeEffect::DelayedYourStep { step, execute, targeted }
            }
            "EndTurn" => LeEffect::EndTurn,
            "PeekAndReveal" => {
                // Only Break Out's exact chain (checked text by text), compiled as one effect.
                let n = p.get("PeekAmount").and_then(|v| v.parse::<u8>().ok()).ok_or("PeekAndReveal PeekAmount")?;
                let sub = p.get("SubAbility").ok_or("PeekAndReveal form")?;
                let ok = p.get("Defined") == Some("You") && p.flag("NoReveal") && p.flag("RememberPeeked") && break_out_chain(f, sub);
                if !ok {
                    return Err("PeekAndReveal form".into());
                }
                LeEffect::LookRevealCreature { n, max_mv: 2 }
            }
            "Amass" => {
                let t = p.get("Type").ok_or("Amass without Type")?;
                let script = format!("b_0_0_{}_army", t.to_ascii_lowercase());
                let token = self.token(&script)?;
                if !self.get(token).playable() {
                    return Err(format!("token {script}: {:?}", self.get(token).unsupported));
                }
                self.intern_subtype("Army");
                let subtype = self.intern_subtype(t);
                let n = self.value(f, p.get("Num").ok_or("Amass without Num")?)?;
                LeEffect::Amass { token, subtype, n }
            }
            "DestroyAll" if p.peek("ValidCards") == Some("TargetedCard.Self,Permanent.NotDefinedTargeted+sharesNameWith Targeted") => {
                p.ack("ValidCards", &["TargetedCard.Self,Permanent.NotDefinedTargeted+sharesNameWith Targeted"], "checked above");
                LeEffect::DestroySameName
            }
            "RemoveCounter" if p.peek("Choices").is_some() => {
                let valid = self.filter_f(f, p.get("Choices").unwrap())?;
                let kind = self.counter_kind(p.get("CounterType").ok_or("RemoveCounter without CounterType")?);
                let n = p.get("ChoiceNum").and_then(|v| v.parse::<u8>().ok()).ok_or("RemoveCounter ChoiceNum")?;
                if !p.flag("RememberAmount") || p.get("CounterNum").is_some_and(|c| c != "1") || p.peek("Defined").is_some() {
                    return Err("RemoveCounter Choices form".into());
                }
                LeEffect::RemoveCounterEach { valid, kind, n }
            }
            "RemoveCounter" if p.peek("Choices").is_none() && p.peek("Defined").is_some() => {
                let kind = self.counter_kind(p.get("CounterType").ok_or("RemoveCounter without CounterType")?);
                let n = match p.get("CounterNum") {
                    Some("All") | None => return Err("RemoveCounter CounterNum".into()),
                    Some(v) => self.value(f, v)?,
                };
                LeEffect::RemoveCounter { kind, n }
            }
            "SacrificeAll" => {
                let valid = match (p.peek("Defined"), p.get("ValidCards")) {
                    (Some(_), None) => None,
                    (None, Some(v)) => Some(self.filter_f(f, v)?),
                    _ => return Err("SacrificeAll form".into()),
                };
                LeEffect::SacrificeAll { valid }
            }
            "Attach" if p.peek("Choices").is_some() => {
                if p.get("Choices") != Some("Equipment.Attached") || p.get("Defined") != Some("Targeted") || p.peek("ValidTgts").is_some() || p.peek("Object").is_some() {
                    return Err("Attach Choices form".into());
                }
                LeEffect::AttachLkiEquipment
            }
            "MultiplyCounter" if p.peek("Defined").is_some_and(|d| d.starts_with("Valid ")) => {
                let valid = self.filter_f(f, p.get("Defined").unwrap().trim_start_matches("Valid ").trim())?;
                let kind = self.counter_kind(p.get("CounterType").ok_or("MultiplyCounter without CounterType")?);
                let factor = p.get("Multiplier").unwrap_or("2").parse().map_err(|_| "MultiplyCounter Multiplier")?;
                LeEffect::MultiplyCounterAll { valid, kind, factor }
            }
            _ => return Ok(None),
        }))))
    }
}


/// Effects of this batch that read `Defined$ Valid <filter>` themselves.
pub fn consumes_defined_valid(e: &Effect) -> bool {
    matches!(e, Effect::Ext(crate::effect_ext::ExtEffect::Le(LeEffect::MultiplyCounterAll { .. })))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeStatic {
    /// "This creature crews Vehicles as though its power were `n` greater" (Pilot tokens of
    /// Defend the Rider; Forge `Mode$ TapPowerValue | ValidCard$ Card.Self`; CR 702.122a counts
    /// the total power of the creatures tapped). Saddle isn't implemented, so only crew uses it.
    CrewPowerBonus { n: i32 },
}

/// The crew bonus of creature `o` (with current characteristics `c`) from its own statics.
pub fn crew_power_bonus(s: &State, db: &CardDb, o: ObjId, c: &Chars) -> i32 {
    if c.lost_abilities {
        return 0;
    }
    let ob = s.obj(o);
    db.get(ob.card)
        .face(ob.face)
        .statics
        .iter()
        .map(|st| match &st.kind {
            StaticKind::Ext(crate::static_ext::StaticExt::Le(LeStatic::CrewPowerBonus { n })) => *n,
            _ => 0,
        })
        .sum()
}

impl CardDb {
    /// Static modes of this batch (`None`: not one of them; keys untouched).
    pub(crate) fn le_static(&mut self, mode: &str, p: &crate::compile::P, _f: &forge_script::Face) -> Result<Option<StaticKind>, String> {
        Ok(Some(StaticKind::Ext(crate::static_ext::StaticExt::Le(match mode {
            "TapPowerValue" => {
                // Only the self form, for saddle and/or crew (saddle isn't implemented: no
                // Mount can be saddled, so that half changes nothing).
                if p.get("ValidCard") != Some("Card.Self") {
                    return Err("TapPowerValue form".into());
                }
                let sa = p.get("ValidSA").unwrap_or("");
                if !sa.split(',').all(|a| a == "Activated.Crew+Vehicle" || a == "Activated.Saddle+Mount") || !sa.contains("Crew") {
                    return Err(format!("TapPowerValue ValidSA {sa}"));
                }
                let n = p.get("Value").and_then(|v| v.parse::<i32>().ok()).ok_or("TapPowerValue Value")?;
                LeStatic::CrewPowerBonus { n }
            }
            _ => return Ok(None),
        }))))
    }
}

/// `id` is leaving the battlefield: remember what is attached to it (last known information,
/// CR 608.2h), only while something is.
pub fn note_attachments_leaving(s: &mut State, id: ObjId) {
    let atts: SmallVec<[ObjId; 2]> = s.battlefield.iter().copied().filter(|&o| s.obj(o).attached_to == Some(id)).collect();
    for a in atts {
        s.ext.le.attached_lki.push((id, a));
    }
}

/// Card-database side tables of this batch (`CardDb::le`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LeTables {
    /// Compile-time only: the mode of the trigger whose body is being compiled (what
    /// `TriggeredSource` / `TriggeredActivator` refer to depends on it).
    #[serde(skip)]
    pub trig_mode: String,
    /// Compile-time only: the half of a Room whose face is being compiled.
    #[serde(skip)]
    pub compiling_half: u8,
    /// Some loaded card has a `ZoneBatchAfter` trigger (moves then publish their batch).
    pub any_after_batch: bool,
    /// Some loaded card has a "whenever you activate a loyalty ability" trigger.
    pub any_loyalty_trigger: bool,
    /// Rooms: [printed card (left door unlocked on the battlefield), right only, both, neither].
    pub room_cards: Vec<[CardId; 4]>,
    /// `CopyPermanent | AddTypes$ Legendary` nodes ("a token that's a copy of ..., except it's
    /// legendary", Adagia): the exception is part of the token's copy effect as it is created
    /// (CR 707.9b), so its enters replacements and triggers see a legendary permanent
    /// (`zones::CopyExc::add_super`).
    pub copy_legendary: Vec<AbilityId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LeExpr {
    /// Objects matching the filter (last known information) that left the battlefield this
    /// turn (`Count$LeftBattlefieldThisTurn <filter>`, Kutzil's Flanker).
    LeftBattlefield(Filter),
    /// The opponent's number of objects matching the filter from their point of view
    /// (`PlayerCountOpponents$HighestValid <filter>`, two players: the one opponent; Beza).
    OppValid(Filter),
    /// The opponent's life total / cards in hand (`Highest`/`LowestLifeTotal`,
    /// `HighestCardsInHand`).
    OppLife,
    OppHand,
}

pub fn eval_expr(e: &LeExpr, c: &crate::expr::EvalCtx) -> i32 {
    let fcx = eval_fcx!(c);
    match e {
        LeExpr::LeftBattlefield(f) => c.s.ext.moved_this_turn.iter().filter(|(old, from, ..)| *from == Zone::Battlefield && obj_matches(c.s, c.db, f, *old, fcx)).count() as i32,
        LeExpr::OppValid(f) => {
            let ofcx = FilterCtx { you: c.you.opp(), ..fcx };
            c.s.battlefield.iter().filter(|&&o| if c.base_only { crate::query::obj_matches_base(c.s, c.db, f, o, ofcx) } else { obj_matches(c.s, c.db, f, o, ofcx) }).count() as i32
        }
        LeExpr::OppLife => c.s.player(c.you.opp()).life,
        LeExpr::OppHand => c.s.player(c.you.opp()).hand.len() as i32,
    }
}

/// `PlayerCountOpponents$<body>` forms of this batch (two-player games: the one opponent).
pub fn compile_opponent_expr(ec: &crate::expr::ExprCompiler, body: &str) -> Result<Option<crate::expr::Expr>, String> {
    let e = |x: LeExpr| Some(crate::expr::Expr::Ext(Box::new(crate::expr_ext::ExtExpr::Le(x))));
    Ok(match body {
        "HighestLifeTotal" | "LowestLifeTotal" => e(LeExpr::OppLife),
        "HighestCardsInHand" | "LowestCardsInHand" => e(LeExpr::OppHand),
        b => match b.strip_prefix("HighestValid ").or_else(|| b.strip_prefix("LowestValid ")) {
            Some(f) => e(LeExpr::OppValid(ec.filter(f.trim())?)),
            None => None,
        },
    })
}

/// `Count$<body>` forms of this batch.
pub fn compile_count(ec: &crate::expr::ExprCompiler, body: &str) -> Result<Option<crate::expr::Expr>, String> {
    use crate::expr::Expr;
    if let Some(f) = body.strip_prefix("LeftBattlefieldThisTurn ") {
        return Ok(Some(Expr::Ext(Box::new(crate::expr_ext::ExtExpr::Le(LeExpr::LeftBattlefield(ec.filter(f)?))))));
    }
    Ok(None)
}

impl CardDb {
    /// Trigger modes of this batch (`None`: not one of them; keys untouched).
    pub(crate) fn le_trigger_mode(&mut self, m: &str, p: &crate::compile::P) -> Result<Option<TrigMode>, String> {
        use crate::trig_ext::TrigExt as T;
        Ok(Some(match m {
            "UnlockDoor" => {
                if p.get("ValidPlayer") != Some("You") || p.get("ValidCard") != Some("Card.Self") || !p.flag("ThisDoor") {
                    return Err("UnlockDoor form".into());
                }
                TrigMode::Ext(T::DoorUnlocked { half: self.le.compiling_half })
            }
            "AbilityCast" => {
                if p.get("ValidSA") != Some("Activated.Loyalty") || p.peek("ValidCard").is_some() {
                    return Ok(None); // other forms: `ext6`
                }
                self.le.any_loyalty_trigger = true;
                TrigMode::Ext(T::LoyaltyActivated { player: PlayerSel::parse(p.get("ValidActivatingPlayer"))? })
            }
            "LifeLost" => {
                if p.peek("FirstTime").is_some() || p.peek("ValidSource").is_some() {
                    return Err("LifeLost form".into());
                }
                TrigMode::Ext(T::LifeLost { player: PlayerSel::parse(p.get("ValidPlayer"))? })
            }
            _ => return Ok(None),
        }))
    }
}

/// Publish a batch of moves to `ZoneBatchAfter` triggers (CR 603.2c: one event).
pub fn fire_after_batch(cx: &mut Ctx, moved: &[(ObjId, Zone, Zone, Option<ObjId>)]) {
    if moved.is_empty() {
        return;
    }
    cx.s.ext.le.batch = moved.iter().copied().collect();
    crate::triggers::fire(cx.s, cx.db, &crate::triggers::TrigEvent::Ext(crate::trig_ext::ExtEvent::AfterBatch));
    cx.s.ext.le.batch.clear();
}

/// Tokens were created (they enter from outside the game, CR 111.1).
pub fn tokens_entered(cx: &mut Ctx, ids: &[ObjId]) {
    if cx.db.le.any_after_batch {
        let moved: SmallVec<[(ObjId, Zone, Zone, Option<ObjId>); 2]> = ids.iter().map(|&n| (n, Zone::Gone, Zone::Battlefield, Some(n))).collect();
        fire_after_batch(cx, &moved);
    }
}

/// How many moves of the published batch match (`ZoneBatchAfter`).
pub fn batch_matches(s: &State, db: &CardDb, origin: ZoneSet, dest: ZoneSet, valid: &Filter, fcx: FilterCtx) -> usize {
    s.ext
        .le
        .batch
        .iter()
        .filter(|&&(old, from, to, new)| {
            let from_ok = from == Zone::Gone && origin == ZoneSet::ANY || origin.has(from);
            // The object as it is now (where it went), or as it last existed if it's gone or
            // left the zone the trigger is about ("cards leave your graveyard").
            let o = match new {
                Some(n) if dest != ZoneSet::ANY || origin == ZoneSet::ANY => n,
                _ => old,
            };
            // An object whose entering causes no ability to trigger (Torpor Orb, Doorkeeper
            // Thrull) isn't part of a "one or more ... enter" event match (Doorkeeper Thrull
            // ruling 2024-02-02; CR 603.2g); the batch's other objects still are.
            let disabled = to == Zone::Battlefield && new.is_some_and(|n| crate::static_ext::etb_triggers_disabled(s, db, n));
            from_ok && dest.has(to) && !disabled && obj_matches(s, db, valid, o, fcx)
        })
        .count()
}

impl CardDb {
    /// Extra nodes for parameters of this batch on common APIs.
    pub(crate) fn le_extras(&mut self, api: &str, p: &crate::compile::P, _f: &forge_script::Face, extra: &mut Vec<AbilityDef>) -> Result<(), String> {
        if api == "CopyPermanent" && p.peek("AddTypes").is_some() {
            if p.get("AddTypes") != Some("Legendary") {
                return Err("CopyPermanent AddTypes form".into());
            }
            extra.push(crate::compile_ext::node(Effect::Ext(crate::effect_ext::ExtEffect::Le(LeEffect::CopiesLegendary)), None));
        }
        if matches!(p.peek("RememberObjects"), Some("ThisTargetedCard") | Some("Targeted")) && p.peek("ValidTgts").is_some() {
            p.ack("RememberObjects", &["ThisTargetedCard", "Targeted"], "checked above");
            extra.push(crate::compile_ext::node(Effect::Ext(crate::effect_ext::ExtEffect::Le(LeEffect::RememberTargets)), None));
        }
        // "Destroy ... | RememberLKI$ True | AlwaysRemember$ True" (Azog): remember the target as
        // it last existed (its old id keeps last known information).
        if api == "Destroy" && p.peek("RememberLKI").is_some() && p.peek("ValidTgts").is_some() {
            if !p.flag("RememberLKI") || p.get("AlwaysRemember").is_some_and(|v| !v.eq_ignore_ascii_case("true")) {
                return Err("Destroy RememberLKI form".into());
            }
            extra.push(crate::compile_ext::node(Effect::Ext(crate::effect_ext::ExtEffect::Le(LeEffect::RememberTargets)), None));
        }
        Ok(())
    }
}

/// Is the sub-ability chain starting at `sub` exactly Break Out's (reveal a creature; if MV 2 or
/// less may put it onto the battlefield with haste, else to hand; rest on the bottom randomly)?
fn break_out_chain(f: &forge_script::Face, sub: &str) -> bool {
    let want: [(&str, &[&str]); 6] = [
        ("ChooseCard", &["Choices$ Card.Creature+IsRemembered", "ChoiceZone$ Library", "Amount$ 1"]),
        ("ChangeZone", &["Destination$ Battlefield", "Defined$ ChosenCard", "ConditionPresent$ Card.Creature+cmcLE2+ChosenCard", "Optional$ True", "Imprint$ True"]),
        ("Pump", &["KW$ Haste", "Defined$ Imprinted"]),
        ("ChangeZone", &["Destination$ Hand", "Defined$ ChosenCard", "ConditionPresent$ Card.Creature+ChosenCard+inZoneBattlefield", "ConditionCompare$ EQ0"]),
        ("ChangeZoneAll", &["ChangeType$ Card.IsRemembered", "Destination$ Library", "LibraryPosition$ -1", "RandomOrder$ True"]),
        ("Cleanup", &["ClearRemembered$ True"]),
    ];
    let mut name = sub.to_string();
    for (api, parts) in want {
        let Some(sv) = f.svar(&name) else { return false };
        let text = &sv.value;
        if !text.starts_with(&format!("DB$ {api} ")) || !parts.iter().all(|x| text.contains(x)) {
            return false;
        }
        match sv.as_ability().and_then(|a| a.get("SubAbility").map(|s| s.to_string())) {
            Some(n) => name = n,
            None => return api == "Cleanup",
        }
    }
    false
}

/// Effects of this batch that stand for their whole sub-ability chain.
pub fn absorbs_chain(e: &Effect) -> bool {
    matches!(e, Effect::Ext(crate::effect_ext::ExtEffect::Le(LeEffect::LookRevealCreature { .. })))
}

/// A "your next [step]" delayed trigger (adhoc, no trigger index) is resolving: give it back
/// the objects it remembered.
pub fn delayed_remembered(s: &mut State, item: &StackItem, ex: &mut crate::resolve::Exec) {
    if let Some(i) = s.ext.le.delayed_rem.iter().position(|d| d.0 == item.source && d.1 == item.ability) {
        let (_, _, rem) = s.ext.le.delayed_rem.remove(i);
        ex.remembered = rem.into_iter().collect();
    }
}

/// `Count$` forms checked before the generic ones.
pub fn compile_count_pre(ec: &crate::expr::ExprCompiler, body: &str) -> Result<Option<crate::expr::Expr>, String> {
    // "for each of those cards that are still exiled" (Dragonhawk): the delayed trigger's
    // remembered objects that are still in exile (an exiled card that moves is a new object).
    if body == "RememberedNumber" {
        return Ok(Some(crate::expr::Expr::Ext(Box::new(crate::expr_ext::ExtExpr::RememberedSize))));
    }
    if body == "ValidExile Card.IsTriggerRemembered" {
        return Ok(Some(crate::expr::Expr::Ext(Box::new(crate::expr_ext::ExtExpr::RememberedValid(ec.filter("Card.inZoneExile")?)))));
    }
    Ok(None)
}

/// The kinds of counter an object has, in its counter order: the choice of "remove a counter
/// from [this]" (`remove_any_counter`'s `Opt::Mode(i)` is the i-th of these; CR 608.2d, 122.1).
/// Option texts and canonical strings (`counter:<kind>`) name the kind from this.
pub fn remove_any_kinds(counters: &[(CounterKind, i32)]) -> SmallVec<[CounterKind; 4]> {
    counters.iter().filter(|c| c.1 > 0).map(|c| c.0).collect()
}

/// Pay "remove a counter from [this]" (any kind): the controller chooses the kind when it has
/// counters of more than one kind (`Opt::Mode(i)` = the i-th of `remove_any_kinds`).
pub fn remove_any_counter(cx: &mut Ctx, p: PlayerId, o: ObjId) -> R<()> {
    let kinds = remove_any_kinds(&cx.s.obj(o).counters);
    let k = match kinds.len() {
        0 => return cx.unsupported("remove-a-counter cost without counters"),
        1 => kinds[0],
        n => {
            let opts: Vec<Opt> = (0..n).map(|i| Opt::Mode(i as u8)).collect();
            match cx.choose(Decision { player: p, kind: DecisionKind::PayCost, options: opts, subject: Some(o) })? {
                Opt::Mode(i) => kinds[i as usize],
                _ => kinds[0],
            }
        }
    };
    crate::ext6::put_counters(cx.s, cx.db, o, k, -1);
    Ok(())
}

// ---------------------------------------------------------------- Rooms (CR 709.5)

/// The face index of a Room permanent with these unlocked designations (bit 0 left, bit 1
/// right): 0 left only, 1 right only, 2 both, 3 neither.
pub fn room_face(mask: u8) -> u8 {
    match mask & 3 {
        1 => 0,
        2 => 1,
        3 => 2,
        _ => 3,
    }
}

/// The unlocked designations of a Room face (inverse of `room_face`).
pub fn room_mask(face: u8) -> u8 {
    match face {
        0 => 1,
        1 => 2,
        2 => 3,
        _ => 0,
    }
}

/// The "both unlocked" face (both halves' names, mana costs and rules text, CR 709.5; also the
/// card's characteristics outside the battlefield, 709.4) and the "neither unlocked" face (the
/// shared type line only).
pub fn room_faces(l: &FaceDef, r: &FaceDef) -> (FaceDef, FaceDef) {
    let mut both = l.clone();
    both.name = format!("{} // {}", l.name, r.name);
    both.cost.syms.extend(r.cost.syms.iter().cloned());
    both.colors = ColorSet(l.colors.0 | r.colors.0);
    both.kws = l.kws.union(r.kws);
    both.activated.extend(r.activated.iter().copied());
    both.triggers.extend(r.triggers.iter().cloned());
    both.statics.extend(r.statics.iter().cloned());
    both.replacements.extend(r.replacements.iter().cloned());
    both.etb_counters.extend(r.etb_counters.iter().cloned());
    both.oracle = format!("{} // {}", l.oracle, r.oracle);
    let mut none = l.clone();
    // CR 709.5: no name. Shown with a descriptive one (never looked up by name).
    none.name = format!("{} // {} (both doors locked)", l.name, r.name);
    none.cost = ManaCost::default();
    none.colors = ColorSet::default();
    none.kws = KwSet::default();
    none.activated.clear();
    none.triggers.clear();
    none.statics.clear();
    none.replacements.clear();
    none.etb_counters.clear();
    none.oracle = String::new();
    (both, none)
}

/// The Room whose printed card or state card `card` is.
pub fn room_of(db: &CardDb, card: CardId) -> Option<[CardId; 4]> {
    if db.le.room_cards.is_empty() {
        return None;
    }
    db.le.room_cards.iter().find(|r| r.contains(&card)).copied()
}

/// The unlocked designations of the Room permanent `o` (None: not a Room).
pub fn room_mask_of(s: &State, db: &CardDb, o: ObjId) -> Option<u8> {
    let r = room_of(db, s.obj(o).card)?;
    let i = r.iter().position(|&c| c == s.obj(o).card)? as u8;
    Some(room_mask(i))
}

/// Give the Room permanent `o` exactly these unlocked designations: its card becomes the
/// matching state card (the printed card for "left only"), shown like a transformed permanent
/// (a `CopyEffect`, so leaving the battlefield restores the printed card, CR 400.7).
pub fn set_room_state(s: &mut State, db: &CardDb, o: ObjId, mask: u8) {
    let Some(r) = room_of(db, s.obj(o).card) else { return };
    let printed = r[0];
    let target = r[room_face(mask) as usize];
    s.copy_effects.retain(|c| c.obj != o);
    s.obj_mut(o).card = target;
    s.obj_mut(o).face = 0;
    if target != printed {
        // Not `transformed`: a Room's mana value is that of its unlocked halves (709.5), not the
        // printed front's (712.8e is for double-faced cards).
        s.copy_effects.push(CopyEffect { obj: o, printed, name: None, add_types: TypeSet::default(), add_subtypes: SmallVec::new(), kws: KwSet::default(), set_pt: None, transformed: false, add_super: 0 });
    }
}

/// Special actions to unlock the locked halves of `p`'s Rooms (CR 709.5e: any time they have
/// priority and the stack is empty during a main phase of their turn; the caller checks
/// sorcery timing), when the half's mana cost can be paid.
pub fn unlock_actions(cx: &Ctx, p: PlayerId, out: &mut Vec<Opt>) {
    if cx.db.le.room_cards.is_empty() {
        return;
    }
    let s = &*cx.s;
    let srcs = std::cell::OnceCell::new();
    for &o in &s.battlefield {
        if s.obj(o).controller != p {
            continue;
        }
        let Some(mask) = room_mask_of(s, cx.db, o) else { continue };
        let printed = room_of(cx.db, s.obj(o).card).unwrap()[0];
        for half in 0..2u8 {
            if mask & (1 << half) != 0 {
                continue;
            }
            let req = crate::cast::requirement(&cx.db.get(printed).face(half).cost, 0);
            let srcs = srcs.get_or_init(|| crate::cast::mana_sources(cx, p, None));
            if crate::cast::payable(&req, srcs) {
                out.push(Opt::Alt(o, if half == 0 { crate::altcast::Method::UnlockLeft as u8 } else { crate::altcast::Method::UnlockRight as u8 }));
            }
        }
    }
}

/// Pay the mana cost of `half` of the Room `o` and unlock it (special action, CR 709.5e).
pub fn unlock_door(cx: &mut Ctx, p: PlayerId, o: ObjId, half: u8) -> R<()> {
    let Some(mask) = room_mask_of(cx.s, cx.db, o) else { return cx.unsupported("unlocking a non-Room") };
    if cx.s.obj(o).zone != Zone::Battlefield || mask & (1 << half) != 0 {
        return cx.unsupported("unlocking a door that isn't locked");
    }
    let printed = room_of(cx.db, cx.s.obj(o).card).unwrap()[0];
    let req = crate::cast::requirement(&cx.db.get(printed).face(half).cost, 0);
    crate::cast::pay_mana(cx, p, req, None, o)?;
    set_room_state(cx.s, cx.db, o, mask | 1 << half);
    crate::triggers::fire(cx.s, cx.db, &crate::triggers::TrigEvent::Ext(crate::trig_ext::ExtEvent::DoorUnlocked { obj: o, half, player: p }));
    Ok(())
}

/// A Room entered with a half unlocked (it was cast): "when you unlock this door" (709.5h).
pub fn entered_unlocked(cx: &mut Ctx, o: ObjId) {
    let Some(mask) = room_mask_of(cx.s, cx.db, o) else { return };
    let p = cx.s.obj(o).controller;
    for half in 0..2u8 {
        if mask & (1 << half) != 0 {
            crate::triggers::fire(cx.s, cx.db, &crate::triggers::TrigEvent::Ext(crate::trig_ext::ExtEvent::DoorUnlocked { obj: o, half, player: p }));
        }
    }
}
