//! Preset stacks of scenario positions (scenario/v0 `stack`; testing only, see
//! `scenario`).
//!
//! Each entry becomes the `StackItem` the engine itself would have put on the stack:
//! - `{"kind": "spell", "name", "controller", "key", "targets", "x", "mode"}`: the card object
//!   (created and keyed by `scenario::load`, zone `Stack`) cast normally, costs paid; recorded as
//!   cast this turn (CR 500.2: the stack empties before a step ends, so it was cast this turn).
//! - `{"kind": "triggered"|"activated", "source", "ability", "controller", "targets", "x",
//!   "mode"}`: `ability` is the 1-based index among the source's triggered abilities / non-mana
//!   activated abilities (the numbering of `trigger:` and `activate:` options); costs already
//!   paid; activation bookkeeping (loyalty ability used, activation counts) recorded.
//!
//! Modes and targets go through the engine's own logic (`cast::mode_sets`,
//! `cast::choose_targets_ctx`, `ext2::choose_division`) run on a scratch context whose decisions
//! are answered from the scenario, so per-node targets, target legality and divisions are the
//! engine's. Anything the format can't express is refused with the reason: which way a spell was
//! cast (kicker, alternative and optional costs, mana spent), the event that triggered an ability
//! other than "this enters" / "this attacks" / the beginning of a step, costs paid with chosen
//! objects, a division with a choice.

use crate::basics::*;
use crate::carddb::{AbilityId, CardDb, Effect, TrigMode};
use crate::engine::{Ctx, Decision, DecisionKind, Halt, Opt, R};
use crate::filter::Prop;
use crate::state::*;
use serde_json::Value;
use smallvec::SmallVec;

fn err<T>(m: impl Into<String>) -> Result<T, String> {
    Err(m.into())
}

/// Run `run` on copies of `s`, answering each decision it poses with `answer` (an option index),
/// until it completes. Returns its result and the state it left.
fn solve<T>(s: &State, db: &CardDb, run: &dyn Fn(&mut Ctx) -> R<T>, answer: &mut dyn FnMut(&State, &Decision) -> Result<usize, String>) -> Result<(T, State), String> {
    let mut tape: Vec<u16> = vec![];
    loop {
        let mut st = s.clone();
        let r = {
            let mut cx = Ctx::scripted(&mut st, db, &tape);
            run(&mut cx)
        };
        match r {
            Ok(v) => return Ok((v, st)),
            Err(Halt::Need(d)) => {
                let i = answer(&st, &d)?;
                tape.push(i as u16);
            }
            Err(Halt::Unsupported(m)) => return err(m),
            Err(e) => return err(format!("{e:?}")),
        }
        if tape.len() > 64 {
            return err("too many choices");
        }
    }
}

/// Parse a reference of a stack entry: `p0`/`p1`, `key:<k>` or a bare key.
fn target_of(v: &Value, find: &dyn Fn(&str) -> Option<ObjId>, what: &str) -> Result<Target, String> {
    target_on(v, find, &[], what)
}

/// `target_of`, also accepting `stack:<n>`: the n-th preset stack entry (1-based, bottom first)
/// when it is an ability (`ext8`: abilities are targeted as stack objects; spells by key).
fn target_on(v: &Value, find: &dyn Fn(&str) -> Option<ObjId>, stack: &[StackItem], what: &str) -> Result<Target, String> {
    let r = v.as_str().ok_or_else(|| format!("{what}: expected a reference"))?;
    if let Some(n) = r.strip_prefix("stack:") {
        let it = n.parse::<usize>().ok().filter(|&n| n >= 1).and_then(|n| stack.get(n - 1)).ok_or_else(|| format!("{what}: {r:?}: not an earlier stack entry"))?;
        if it.kind == StackKind::Spell {
            return err(format!("{what}: {r:?} is a spell: name it by its key"));
        }
        return Ok(Target::Ability { uid: it.uid, src: it.source });
    }
    Ok(match r {
        "p0" => Target::Player(PlayerId(0)),
        "p1" => Target::Player(PlayerId(1)),
        _ => {
            let k = r.strip_prefix("key:").unwrap_or(r);
            if r.starts_with("name:") || r.starts_with("new:") {
                return err(format!("{what}: {r:?}: preset targets must be keys or players"));
            }
            Target::Obj(find(k).ok_or_else(|| format!("{what}: unknown key {k:?}"))?)
        }
    })
}

/// Why a spell's behaviour may depend on how it was cast (which the format can't say), if it
/// may: kicker, alternative/optional/additional costs, mana spent.
fn cast_dependent(db: &CardDb, card: CardId) -> Option<String> {
    let cd = db.get(card);
    for f in &cd.faces {
        if f.kicker.is_some() {
            return Some("it has kicker, and the format has no field for whether it was kicked".into());
        }
        let a = &f.alt;
        let alt_ways = [
            ("warp", a.warp.is_some()),
            ("evoke", a.evoke.is_some()),
            ("impending", a.impending.is_some()),
            ("harmonize", a.harmonize.is_some()),
            ("plot", a.plot.is_some()),
            ("offspring", a.offspring.is_some()),
            ("an additional cost choice", a.either.is_some()),
            ("teamwork", a.teamwork.is_some()),
            ("an additional sacrifice cost", a.extra_cost.is_some()),
            ("sneak", a.sneak.is_some()),
            ("mayhem", a.mayhem.is_some()),
            ("flashback", f.flashback.is_some()),
        ];
        if let Some((w, _)) = alt_ways.iter().find(|x| x.1) {
            return Some(format!("it has {w}, and the format has no field for how it was cast"));
        }
        if f.statics.iter().any(|st| matches!(st.kind, crate::carddb::StaticKind::Alt(_))) {
            return Some("it has an optional or alternative cost, and the format has no field for how it was cast".into());
        }
    }
    if db.spell_life_x.iter().any(|&a| cd.faces.iter().any(|f| f.spell == Some(a))) {
        return Some("X is paid in life (the format's x can't say it was paid)".into());
    }
    // Effects reading what was spent / how it was cast.
    let mut roots: Vec<AbilityId> = vec![];
    let mut texts: Vec<String> = vec![];
    for f in &cd.faces {
        roots.extend(f.spell);
        roots.extend(f.activated.iter().copied());
        for t in &f.triggers {
            roots.push(t.execute);
            texts.push(format!("{:?}", t.intervening));
        }
        for st in &f.statics {
            texts.push(format!("{:?}", st.kind));
        }
    }
    let mut seen: Vec<AbilityId> = vec![];
    while let Some(a) = roots.pop() {
        if seen.contains(&a) || a.0 == u32::MAX {
            continue;
        }
        seen.push(a);
        let ad = db.ability(a);
        texts.push(format!("{:?} {:?} {:?}", ad.effect, ad.conditions, ad.target));
        roots.extend(ad.sub);
        if let Effect::Charm { modes, .. } = &ad.effect {
            roots.extend(modes.iter().copied());
        }
    }
    for t in &texts {
        for (needle, what) in [("ManaSpent", "the mana spent to cast it"), ("Converge", "the colors of mana spent to cast it"), ("Adamant", "the mana spent to cast it"), ("CastFlag", "how it was cast"), ("Kicked", "whether it was kicked")] {
            if t.contains(needle) {
                return Some(format!("its effects read {what}, which the format can't give"));
            }
        }
    }
    None
}

/// The trigger event's context for a triggered ability put on the stack by a scenario, when the
/// event is determined by the position: "when this enters" (the source itself), "whenever this
/// attacks", "at the beginning of [step]" (the active player).
fn trig_ctx(s: &State, src: ObjId, td: &crate::carddb::TriggerDef) -> Result<TrigCtx, String> {
    let selfish = |f: &crate::filter::Filter| f.alts.len() == 1 && f.alts[0].props.contains(&Prop::SelfCard);
    match &td.mode {
        TrigMode::ChangesZone { dest, valid, .. } if dest.has(Zone::Battlefield) && selfish(valid) => {
            if s.obj(src).zone != Zone::Battlefield {
                return err(format!("its source isn't on the battlefield, but the ability ({:?}) triggers as it enters", td.text));
            }
            Ok(TrigCtx { obj: Some(src), new_obj: Some(src), player: None, amount: 0, stack_item: None, target: None, gain_source: None })
        }
        TrigMode::Attacks { valid } if selfish(valid) => {
            if !s.combat.attackers.iter().any(|a| a.0 == src) {
                return err(format!("its source isn't attacking, but the ability ({:?}) triggers as it attacks", td.text));
            }
            Ok(TrigCtx { obj: Some(src), ..Default::default() })
        }
        TrigMode::Phase { step, .. } => {
            if *step != s.turn.step {
                return err(format!("the ability ({:?}) triggers at the beginning of {step:?}, not in this step", td.text));
            }
            Ok(TrigCtx { player: Some(s.turn.active), ..Default::default() })
        }
        _ => err(format!("the event that triggered it ({:?}) can't be given in scenario/v0 (only \"this enters\", \"this attacks\" and beginning-of-step triggers have a context determined by the position)", td.text)),
    }
}

/// The modes of an entry (`"mode": [1, 3]`, 1-based Oracle order), checked against the engine's
/// legal mode selections.
fn modes_of(s: &State, db: &CardDb, e: &Value, p: PlayerId, ability: AbilityId, src: ObjId, what: &str) -> Result<SmallVec<[u8; 3]>, String> {
    let mut st = s.clone();
    let sets = {
        let cx = Ctx::scratch(&mut st, db);
        crate::cast::mode_sets(&cx, p, ability, Some(src))
    };
    let given: Option<SmallVec<[u8; 3]>> = match &e["mode"] {
        Value::Null => None,
        Value::Array(a) => Some(a.iter().map(|m| m.as_u64().filter(|&n| n >= 1 && n <= 255).map(|n| (n - 1) as u8).ok_or_else(|| format!("{what}.mode: expected 1-based mode numbers"))).collect::<Result<_, _>>()?),
        Value::Number(n) => Some(SmallVec::from_slice(&[n.as_u64().filter(|&n| n >= 1).ok_or_else(|| format!("{what}.mode: expected a 1-based mode number"))? as u8 - 1])),
        _ => return err(format!("{what}.mode: expected a list of mode numbers")),
    };
    let modal = matches!(db.ability(ability).effect, Effect::Charm { .. });
    match given {
        None if modal => err(format!("{what}: a modal ability needs \"mode\"")),
        None => Ok(sets.into_iter().next().unwrap_or_default()),
        Some(m) if !modal => {
            if m.is_empty() {
                Ok(SmallVec::new())
            } else {
                err(format!("{what}: \"mode\" given for an ability that isn't modal"))
            }
        }
        Some(mut m) => {
            m.sort_unstable();
            if !sets.contains(&m) {
                return err(format!("{what}: modes {:?} are not a legal choice of modes now (CR 700.2)", m.iter().map(|x| x + 1).collect::<Vec<_>>()));
            }
            Ok(m)
        }
    }
}

/// Targets of an entry mapped to the engine's per-node targets by its own target logic.
#[allow(clippy::too_many_arguments)]
fn targets_of(s: &State, db: &CardDb, wanted: &[Target], p: PlayerId, ability: AbilityId, modes: &[u8], src: ObjId, x: i32, trig: TrigCtx, what: &str) -> Result<SmallVec<[(u8, Target); 2]>, String> {
    let mut remaining: Vec<Target> = wanted.to_vec();
    let mut subst: Vec<(ObjId, ObjId)> = vec![];
    let modes_v: SmallVec<[u8; 3]> = SmallVec::from_slice(modes);
    let run = move |cx: &mut Ctx| crate::cast::choose_targets_ctx(cx, p, ability, &modes_v, src, x, trig);
    let mut answer = |st: &State, d: &Decision| -> Result<usize, String> {
        if d.kind != DecisionKind::Target {
            return err(format!("{what}: unexpected {:?} decision while choosing targets", d.kind));
        }
        for (i, o) in d.options.iter().enumerate() {
            let Opt::Target(t) = *o else { continue };
            if let Some(j) = remaining.iter().position(|&w| w == t) {
                remaining.remove(j);
                return Ok(i);
            }
            // An identical object (the engine offers interchangeable objects once).
            if let Target::Obj(y) = t {
                let sig = crate::query::obj_sig(st, db, y);
                if let Some(j) = remaining.iter().position(|&w| matches!(w, Target::Obj(xo) if st.obj(xo).zone == st.obj(y).zone && crate::query::obj_sig(st, db, xo) == sig)) {
                    if let Target::Obj(xo) = remaining.remove(j) {
                        subst.push((y, xo));
                    }
                    return Ok(i);
                }
            }
        }
        if let Some(i) = d.options.iter().position(|o| *o == Opt::Done) {
            return Ok(i);
        }
        if remaining.is_empty() {
            return err(format!("{what}: fewer targets given than its target words need (CR 601.2c, 115.1)"));
        }
        err(format!("{what}: target {} is not a legal target for the next target word now (the engine offers {:?})", remaining.first().map(|t| format!("{t:?}")).unwrap_or_default(), d.options))
    };
    let (mut ts, _) = solve(s, db, &run, &mut answer).map_err(|e| format!("{what}: choosing its targets: {e}"))?;
    // Interchangeable objects: the targets are the scenario's objects.
    let mut used = vec![false; subst.len()];
    for t in ts.iter_mut() {
        if let Target::Obj(y) = t.1 {
            if let Some(k) = subst.iter().enumerate().position(|(k, sp)| !used[k] && sp.0 == y) {
                used[k] = true;
                t.1 = Target::Obj(subst[k].1);
            }
        }
    }
    // A target chosen automatically (the only option, so no decision was asked) may stand for an
    // interchangeable twin the scenario names (an ability targeting the second of
    // two identical graveyard cards).
    for i in 0..ts.len() {
        let Target::Obj(y) = ts[i].1 else { continue };
        if wanted.contains(&ts[i].1) {
            continue;
        }
        let sig = crate::query::obj_sig(s, db, y);
        let twin = wanted.iter().find_map(|&w| match w {
            Target::Obj(xo) if s.obj(xo).zone == s.obj(y).zone && crate::query::obj_sig(s, db, xo) == sig && !ts.iter().any(|t| t.1 == w) => Some(xo),
            _ => None,
        });
        if let Some(xo) = twin {
            ts[i].1 = Target::Obj(xo);
        }
    }
    let mut got: Vec<Target> = ts.iter().map(|t| t.1).collect();
    let mut want: Vec<Target> = wanted.to_vec();
    let key = |t: &Target| match *t {
        Target::Player(p) => (0, p.0 as u32),
        Target::Obj(o) => (1, o.0),
        Target::Ability { uid, .. } => (2, uid),
    };
    got.sort_by_key(key);
    want.sort_by_key(key);
    if got != want {
        return err(format!("{what}: the engine chose targets {got:?} for the ability's target words, not the given {want:?} (a target with only one legal choice is chosen automatically; give every target, in Oracle order)"));
    }
    Ok(ts)
}

/// Put the scenario's stack (bottom first) on the stack. `spell_objs[i]`: the object created for
/// entry `i` if it is a spell.
pub fn load_stack(s: &mut State, db: &CardDb, entries: &[&Value], spell_objs: &[Option<ObjId>], find: &dyn Fn(&str) -> Option<ObjId>) -> Result<(), String> {
    for (i, e) in entries.iter().enumerate() {
        let what = format!("stack[{i}]");
        let m = e.as_object().ok_or_else(|| format!("{what}: expected an object"))?;
        let allowed = ["kind", "name", "controller", "key", "targets", "x", "mode", "source", "ability"];
        if let Some(k) = m.keys().find(|k| !allowed.contains(&k.as_str())) {
            return err(format!("{what}: field {k:?} is not supported by the engine's scenario loader"));
        }
        let p = match &e["controller"] {
            Value::Number(n) if n.as_u64().is_some_and(|x| x <= 1) => PlayerId(n.as_u64().unwrap() as u8),
            v => return err(format!("{what}.controller: expected 0 or 1, got {v}")),
        };
        let x = match &e["x"] {
            Value::Null => 0,
            v => v.as_i64().filter(|&n| (0..=1000).contains(&n)).ok_or_else(|| format!("{what}.x: expected a non-negative integer"))? as i32,
        };
        let wanted: Vec<Target> = match &e["targets"] {
            Value::Null => vec![],
            Value::Array(a) => a.iter().enumerate().map(|(j, t)| target_on(t, find, &s.stack, &format!("{what}.targets[{j}]"))).collect::<Result<_, _>>()?,
            _ => return err(format!("{what}.targets: expected a list")),
        };
        let kind = e["kind"].as_str().unwrap_or("");
        let item = match kind {
            "spell" => {
                let so = spell_objs.get(i).copied().flatten().ok_or_else(|| format!("{what}: spell object missing"))?;
                if !e["source"].is_null() || !e["ability"].is_null() {
                    return err(format!("{what}: a spell entry has no \"source\"/\"ability\""));
                }
                let card = s.obj(so).card;
                let f = db.get(card).front();
                if f.types.has(CardType::Land) {
                    return err(format!("{what}: a land isn't cast (CR 305.1)"));
                }
                if let Some(why) = cast_dependent(db, card) {
                    return err(format!("{what}: {}: {why}", db.get(card).name));
                }
                let ability = f.spell.unwrap_or(crate::cast::permanent_spell_ability(db));
                if !f.cost.has_x() && x != 0 {
                    return err(format!("{what}: x given for a spell without X in its cost"));
                }
                let (modes, targets) = if f.spell.is_some() {
                    let modes = modes_of(s, db, e, p, ability, so, &what)?;
                    let targets = targets_of(s, db, &wanted, p, ability, &modes, so, x, TrigCtx::default(), &what)?;
                    (modes, targets)
                } else {
                    if !wanted.is_empty() || !e["mode"].is_null() {
                        return err(format!("{what}: a permanent spell without targets or modes (an Aura spell's target is not supported)"));
                    }
                    (SmallVec::new(), SmallVec::new())
                };
                if f.spell.is_some() && !targets.is_empty() {
                    // CR 601.2d: a division among the targets (forced when there's one choice).
                    let tg = targets.clone();
                    let md = modes.clone();
                    let run = move |cx: &mut Ctx| crate::ext2::choose_division(cx, p, so, ability, &md, &tg, x);
                    let mut answer = |_: &State, d: &Decision| -> Result<usize, String> { err(format!("{what}: its effect is divided among its targets with a choice ({:?}); the format has no field for the division", d.kind)) };
                    let ((), st) = solve(s, db, &run, &mut answer)?;
                    s.ext.divided = st.ext.divided;
                }
                // CR 601.2i: it became cast this turn.
                s.turn_stats.spells_cast[p.idx()] = s.turn_stats.spells_cast[p.idx()].saturating_add(1);
                s.ext.cast_this_turn.push((p, so));
                StackItem { kind: StackKind::Spell, controller: p, source: so, source_card: card, ability, targets, x, kicked: false, modes, trig: TrigCtx::default(), trigger_idx: None, flashback: false, uid: 0 }
            }
            "triggered" | "activated" => {
                if !e["name"].is_null() || !e["key"].is_null() {
                    return err(format!("{what}: an ability entry is named by \"source\" and \"ability\""));
                }
                let src = match target_of(&e["source"], find, &format!("{what}.source"))? {
                    Target::Obj(o) => o,
                    _ => return err(format!("{what}.source: a player is not a source")),
                };
                let n = e["ability"].as_u64().filter(|&n| n >= 1).ok_or_else(|| format!("{what}.ability: expected a 1-based ability number"))? as usize;
                let card = s.obj(src).card;
                if s.obj(src).zone == Zone::Battlefield && crate::ext6::face_down_card(s, src).is_some() {
                    return err(format!("{what}: abilities of a face-down source are not supported"));
                }
                if kind == "triggered" {
                    let trs = &db.get(card).front().triggers;
                    let td = trs.get(n - 1).ok_or_else(|| format!("{what}: {} has {} triggered abilities, no {n}", db.get(card).name, trs.len()))?;
                    if !td.zones.has(s.obj(src).zone) {
                        return err(format!("{what}: the ability ({:?}) doesn't function in its source's zone {:?}", td.text, s.obj(src).zone));
                    }
                    if s.obj(src).controller != p && s.obj(src).zone == Zone::Battlefield {
                        return err(format!("{what}: the controller of a triggered ability is its source's controller (CR 603.3a)"));
                    }
                    let trig = trig_ctx(s, src, td).map_err(|m| format!("{what}: {m}"))?;
                    let ability = td.execute;
                    let modes = modes_of(s, db, e, p, ability, src, &what)?;
                    let targets = targets_of(s, db, &wanted, p, ability, &modes, src, x, trig, &what)?;
                    StackItem { kind: StackKind::Triggered, controller: p, source: src, source_card: card, ability, targets, x, kicked: false, modes, trig, trigger_idx: Some((n - 1) as u8), flashback: false, uid: 0 }
                } else {
                    // `activate:` numbering: non-mana activated abilities, printed then gained.
                    let ob = s.obj(src);
                    let f = db.get(ob.card).face(ob.face);
                    let list: Vec<AbilityId> = match ob.zone {
                        Zone::Hand => f.activated.iter().chain(f.hand_abilities.iter()).copied().collect(),
                        Zone::Graveyard => f.activated.iter().chain(f.grave_abilities.iter()).copied().collect(),
                        Zone::Battlefield => crate::query::chars(s, db, src).abilities.to_vec(),
                        z => return err(format!("{what}: activated abilities of a source in {z:?} are not supported")),
                    };
                    let list: Vec<AbilityId> = list.into_iter().filter(|&b| !db.ability(b).is_mana).collect();
                    let ability = *list.get(n - 1).ok_or_else(|| format!("{what}: {} has {} non-mana activated abilities, no {n}", db.get(card).name, list.len()))?;
                    let ad = db.ability(ability);
                    let cost = ad.cost.clone().unwrap_or_default();
                    if ob.controller != p && ob.zone == Zone::Battlefield {
                        return err(format!("{what}: only its controller activates a permanent's ability (CR 602.2)"));
                    }
                    if cost.has_choices() || cost.exile_self || cost.exile_ctrl_or_grave.is_some() {
                        return err(format!("{what}: its cost is paid with chosen objects (sacrifice, discard, exile, tap, crew, ninjutsu, craft, ...) that the format can't give ({:?})", ad.text));
                    }
                    if x != 0 && !cost.x_loyalty && !cost.mana.has_x() {
                        return err(format!("{what}: x given for an ability without X in its cost"));
                    }
                    let modes = modes_of(s, db, e, p, ability, src, &what)?;
                    let targets = targets_of(s, db, &wanted, p, ability, &modes, src, x, TrigCtx::default(), &what)?;
                    // Activation history (as `cast::activate` records it).
                    if crate::cast::is_equip(ad) {
                        s.ext.equips_this_turn[p.idx()] = s.ext.equips_this_turn[p.idx()].saturating_add(1);
                    }
                    crate::effect_ext::count_activation(s, db, src, ability);
                    if cost.is_loyalty() && !s.turn_stats.loyalty_activated.contains(&src) {
                        s.turn_stats.loyalty_activated.push(src);
                    }
                    StackItem { kind: StackKind::Activated, controller: p, source: src, source_card: card, ability, targets, x, kicked: false, modes, trig: TrigCtx::default(), trigger_idx: None, flashback: false, uid: 0 }
                }
            }
            k => return err(format!("{what}: unknown kind {k:?} (spell, triggered, activated)")),
        };
        // They became targets this turn (`ext4::note_targeted`, valiant).
        let tobjs: Vec<ObjId> = item.targets.iter().filter_map(|t| if let Target::Obj(o) = t.1 { Some(o) } else { None }).collect();
        crate::ext4::note_targeted(s, &tobjs, p);
        s.push_stack(item);
    }
    Ok(())
}
