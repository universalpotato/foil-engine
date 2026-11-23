//! Long-tail mechanics, eighth batch.
//! Kept in its own module to stay additive (like `ext2`..`ext7`).
//!
//! - **Stack objects as targets** (CR 113.7a, 115.1b-d, 115.5): an activated or triggered
//!   ability on the stack is its own target, `Target::Ability { uid, src }` (`StackItem::uid`),
//!   so two abilities of one source, or a spell and its own triggered ability, are different
//!   targets. Spells stay `Target::Obj` (a spell is an object). `TargetKind::Stack(StackSel)`
//!   mixes them ("target instant spell, sorcery spell, or triggered ability").

use crate::basics::*;
use crate::carddb::*;
use crate::filter::Filter;
use crate::query::*;
use crate::state::*;
use crate::engine::{Ctx, DecisionKind, Opt, R};

/// Stack objects matching `sel` (and `filter`: a spell, or an ability's source), in stack
/// order (bottom first). A spell can't target itself (`source`, CR 115.5); an ability being
/// put on the stack isn't on it yet while its targets are chosen (601.2c, 602.2b, 603.3d).
pub fn stack_targets(s: &State, db: &CardDb, filter: &Filter, sel: StackSel, source: Option<ObjId>, fcx: FilterCtx, out: &mut Vec<Target>) {
    out.extend(s.stack.iter().filter_map(|it| stack_item_target(s, db, filter, sel, source, fcx, it)));
}

/// Stack object `it` as a target for (`filter`, `sel`), if it matches (`stack_targets`).
pub fn stack_item_target(s: &State, db: &CardDb, filter: &Filter, sel: StackSel, source: Option<ObjId>, fcx: FilterCtx, it: &StackItem) -> Option<Target> {
    if sel.single_target && it.targets.len() != 1 {
        return None;
    }
    let ok = match it.kind {
        StackKind::Spell => {
            sel.spells && Some(it.source) != source && {
                let t = chars(s, db, it.source).types;
                !sel.inst_sorc || t.has(CardType::Instant) || t.has(CardType::Sorcery)
            }
        }
        StackKind::Activated => sel.activated,
        StackKind::Triggered => sel.triggered,
    };
    if !ok || !obj_matches(s, db, filter, it.source, fcx) {
        return None;
    }
    Some(match it.kind {
        StackKind::Spell => Target::Obj(it.source),
        _ => Target::Ability { uid: it.uid, src: it.source },
    })
}

/// The spell or ability now resolving (`resolve::Exec::resolving`: off `State::stack` while it
/// resolves) as a target for `spec`, when that is legal. It is still on the stack until the
/// last step of its resolution (CR 608.2, 608.2n), and a spell or ability is an illegal target
/// only for itself (115.5): "change the target" and a copy's new targets chosen as it resolves
/// may be it (Redirect ruling 2010-08-15). `source`: the spell whose targets are
/// chosen (it can't target itself); `exclude`: as `cast::legal_targets`.
pub fn resolving_target(cx: &Ctx, p: PlayerId, spec: &TargetSpec, source: Option<ObjId>, exclude: &[Target], it: Option<&StackItem>) -> Option<Target> {
    let it = it?;
    let s = &*cx.s;
    // A permanent spell has no effect to resolve; a spell that already left the stack (608.2n)
    // isn't on it.
    if it.ability.0 == u32::MAX || (it.kind == StackKind::Spell && s.obj(it.source).zone != Zone::Stack) {
        return None;
    }
    let fcx = FilterCtx { you: p, source, x: 0, rem: &[] };
    let t = match spec.kind {
        TargetKind::Spell | TargetKind::SpellOrPermanent if it.kind == StackKind::Spell => {
            (Some(it.source) != source && obj_matches(s, cx.db, &spec.filter, it.source, fcx)).then_some(Target::Obj(it.source))
        }
        TargetKind::Ability => stack_item_target(s, cx.db, &spec.filter, StackSel { activated: true, triggered: true, ..Default::default() }, source, fcx, it),
        TargetKind::Stack(sel) => stack_item_target(s, cx.db, &spec.filter, sel, source, fcx, it),
        _ => None,
    }?;
    (!exclude.contains(&t)).then_some(t)
}

/// The stack objects a `TargetType$` list names, for the mixed forms (`TargetKind::Stack`).
pub fn stack_sel(t: Option<&str>) -> Option<StackSel> {
    let all = StackSel { spells: true, activated: true, triggered: true, ..Default::default() };
    Some(match t? {
        // "target instant spell, sorcery spell, or triggered ability" (Spider-Sense)
        "Instant,Sorcery,Triggered" => StackSel { spells: true, inst_sorc: true, triggered: true, ..Default::default() },
        // "target instant spell, sorcery spell, activated ability, or triggered ability"
        "Activated,Triggered,Instant,Sorcery" => StackSel { spells: true, inst_sorc: true, activated: true, triggered: true, ..Default::default() },
        // "target spell or ability with a single target" (CR 115.9a)
        "SpellAbility.singleTarget" => StackSel { single_target: true, ..all },
        // "target spell with a single target"
        "Spell.singleTarget" => StackSel { spells: true, single_target: true, ..Default::default() },
        _ => return None,
    })
}

/// Side tables of this batch (`CardDb::x8`).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct X8Tables {
    /// A loaded card has a `StaticExt::StolenSubtype` static (skip the scan otherwise).
    pub stolen_subtype: bool,
    /// A loaded spell has target counts by the gift's promise (`TargetSpec::by_promise`).
    pub any_by_promise: bool,
}

/// Layer 4 (CR 613.1d), Laughing Jasper Flint: "Creatures you control but don't own are
/// Mercenaries in addition to their other types." Applied after the type-changing effects on
/// `id` (whether it is a creature depends on them, 613.8a) and in timestamp order with the
/// effects that remove creature types (613.7: a later "loses all creature types" wins). A
/// layer-6 loss of the source's abilities doesn't stop it (613.6, as `ext4::land_type_override`);
/// a face-down source has no abilities (CR 708.2).
pub fn stolen_subtype(s: &State, db: &CardDb, id: ObjId, c: &mut Chars) {
    let o = s.obj(id);
    if o.controller == o.owner || !c.types.has(CardType::Creature) {
        return;
    }
    let last_removal = s.effects.iter().filter(|e| e.affected == id && e.remove_creature_types).map(|e| e.ts).max();
    for &src in &s.battlefield {
        let so = s.obj(src);
        if so.controller != o.controller || crate::ext6::face_down_card(s, src).is_some() {
            continue;
        }
        for st in &db.get(so.card).face(so.face).statics {
            let StaticKind::Ext(crate::static_ext::StaticExt::StolenSubtype(t)) = st.kind else { continue };
            if !st.zones.has(Zone::Battlefield) || last_removal.is_some_and(|r| r > so.ts) {
                continue;
            }
            if !c.subtypes.contains(&t) {
                c.subtypes.push(t);
            }
        }
    }
}

/// `TargetMin$ X | TargetMax$ X` with X = `Count$PromisedGift.A.B` on a gift spell: exactly A
/// targets if the gift was promised, else B; as (not promised, promised). `None`: another form
/// (keys untouched).
pub(crate) fn target_count_by_promise(db: &CardDb, f: &forge_script::Face, p: &crate::compile::P) -> Result<Option<(u8, u8)>, String> {
    let (Some(lo), Some(hi)) = (p.peek("TargetMin"), p.peek("TargetMax")) else { return Ok(None) };
    if lo != hi || lo.parse::<u8>().is_ok() || f.svar(lo).is_none() || !f.keywords.iter().any(|k| k == "Gift") {
        return Ok(None);
    }
    match db.value(f, lo)? {
        crate::expr::Expr::Alt(crate::altcast::AltExpr::CastFlag(fl, a, b)) if fl == crate::altcast::flag::OPTCOST && (0..=1).contains(&a) && (0..=1).contains(&b) => {
            p.ack("TargetMin", &[lo], "checked above (`TargetMax$` is the same SVar)");
            Ok(Some((b as u8, a as u8)))
        }
        _ => Err("variable target count".into()),
    }
}

/// The number of targets a `by_promise` spec needs for spell `so` (its gift promised or not).
pub fn promise_count(s: &State, so: ObjId, by: (u8, u8)) -> u8 {
    if s.cast_of(so).has(crate::altcast::flag::OPTCOST) {
        by.1
    } else {
        by.0
    }
}

/// For a spell with `by_promise` target counts: can it be cast without / with the promise
/// (enough legal targets for every target word, CR 601.2c)? `(true, true)` for other spells.
pub fn promise_gate(cx: &Ctx, p: PlayerId, a: AbilityId, source: ObjId, exclude_self: bool) -> (bool, bool) {
    if !cx.db.x8.any_by_promise {
        return (true, true);
    }
    let ns = crate::resolve::nodes(cx.db, a, &[]);
    if !ns.iter().any(|&n| cx.db.ability(n).target.as_ref().is_some_and(|t| t.by_promise.is_some())) {
        return (true, true);
    }
    let excl: smallvec::SmallVec<[Target; 1]> = if exclude_self { smallvec::smallvec![Target::Obj(source)] } else { smallvec::SmallVec::new() };
    let ok = |promised: bool| {
        ns.iter().all(|&n| match &cx.db.ability(n).target {
            Some(t) => {
                let need = match t.by_promise {
                    Some(by) => if promised { by.1 } else { by.0 },
                    None => t.min,
                };
                crate::cast::legal_targets(cx, p, t, Some(source), &excl).len() >= need as usize
            }
            None => true,
        })
    };
    (ok(false), ok(true))
}

/// Delirium (CR 207.2c): four or more card types among cards in your graveyard (`yours` =
/// `Card.YouOwn`; a graveyard holds only its owner's cards, CR 404.2).
pub fn delirium(yours: Filter) -> crate::expr::Condition {
    use crate::expr::{Condition, Expr, ZoneSel};
    Condition::Compare(Expr::Ext(Box::new(crate::expr_ext::ExtExpr::CardTypesAmong(ZoneSel::Graveyard, yours))), crate::filter::CmpOp::GE, Expr::Num(4))
}

/// The stack index of the ability a `Target::Ability` names, if it is still on the stack.
pub fn ability_index(s: &State, uid: u32) -> Option<usize> {
    s.stack.iter().position(|it| it.uid == uid && it.kind != StackKind::Spell)
}

/// Effects of this batch.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum X8Effect {
    /// "Change the target of target spell or ability with a single target" (CR 115.7a; Return
    /// the Favor ruling 2024-04-12: chosen as it resolves, must change if possible, only to
    /// another legal target, else unchanged).
    ChangeSingleTarget,
    /// "Copy target instant spell, sorcery spell, activated ability, or triggered ability. You
    /// may choose new targets for the copy." (CR 707.10, 707.10c).
    CopyTarget,
    /// "Return target spell to its owner's hand" (Bilbo's Gambit): it leaves the stack without
    /// being countered (so "can't be countered" doesn't stop it); a copy of a spell ceases to
    /// exist (CR 704.5e, 707.10a).
    SpellToHand,
    /// "You may [cost A] or [cost B]. If you do, [effect]." (Chandra, Spark Hunter's +2; Forge
    /// `GenericChoice` whose choices are each "you may [cost]. If you do, [effect]", CR 608.2d):
    /// as it resolves, its controller picks one payable cost (`Opt::Mode(i)`) or neither
    /// (`Opt::No`), then that choice's chain runs with the cost paid.
    ChooseCost { choices: Vec<AbilityId> },
}

impl CardDb {
    /// Effect APIs of this batch (`None`: not one of them; keys untouched).
    pub(crate) fn x8_api(&mut self, api: &str, p: &crate::compile::P, f: &forge_script::Face, uns: &mut Vec<String>, depth: u32) -> Result<Option<Effect>, String> {
        let sel = stack_sel(p.peek("TargetType"));
        let stack_target = p.peek("ValidTgts").is_some() && p.peek("Defined").is_none() && sel.is_some();
        Ok(Some(Effect::Ext(crate::effect_ext::ExtEffect::X8(match api {
            "ChangeTargets" if stack_target && sel.is_some_and(|s| s.single_target) => X8Effect::ChangeSingleTarget,
            "GenericChoice" => {
                if p.peek("Defined").is_some_and(|d| d != "You") || p.peek("ValidTgts").is_some() {
                    return Err("GenericChoice form".into());
                }
                p.ack("Defined", &["You"], "checked above");
                let names = p.get("Choices").ok_or("GenericChoice without Choices")?;
                let mut choices = vec![];
                for n in names.split(',') {
                    let sv = f.svar(n).and_then(|s| s.as_ability()).ok_or_else(|| format!("GenericChoice {n} missing"))?;
                    let a = self.ability_inner(&sv, f, uns, depth + 1, false)?;
                    // Only "you may [cost]. If you do, [effect]" choices (a resolution cost).
                    let ad = &self.abilities[a.0 as usize];
                    if ad.cost.is_none() || ad.optional || ad.unless.is_some() {
                        return Err("GenericChoice of choices other than optional costs".into());
                    }
                    let mut cur = Some(a);
                    while let Some(x) = cur {
                        if self.abilities[x.0 as usize].target.is_some() {
                            return Err("GenericChoice choice with targets".into());
                        }
                        cur = self.abilities[x.0 as usize].sub;
                    }
                    choices.push(a);
                }
                X8Effect::ChooseCost { choices }
            }
            "CopySpellAbility" if stack_target && p.peek("Amount").is_none() && p.peek("Controller").is_none() => {
                if !p.flag("MayChooseTarget") {
                    return Err("CopySpellAbility of a stack target without new targets".into());
                }
                X8Effect::CopyTarget
            }
            _ => return Ok(None),
        }))))
    }

    /// `ChangeZone | Origin$ Stack` (`None`: another origin).
    pub(crate) fn x8_change_zone(&mut self, p: &crate::compile::P, origin: ZoneSet, dest: Zone) -> Result<Option<Effect>, String> {
        // Other forms (e.g. Daydream's `Defined$ Self | Origin$ Stack`) are compiled elsewhere.
        if origin != ZoneSet::of(Zone::Stack) || dest != Zone::Hand || p.peek("ValidTgts").is_none() || p.peek("Defined").is_some() || p.peek("TgtZone") != Some("Stack") {
            return Ok(None);
        }
        Ok(Some(Effect::Ext(crate::effect_ext::ExtEffect::X8(X8Effect::SpellToHand))))
    }
}

pub fn apply(cx: &mut Ctx, ex: &mut crate::resolve::Exec, x: &X8Effect, tg: &[Target]) -> R<()> {
    match x {
        X8Effect::ChangeSingleTarget => {
            for &t in tg {
                change_single_target(cx, ex.you, t, ex.resolving.as_ref())?;
            }
            Ok(())
        }
        X8Effect::CopyTarget => {
            for &t in tg {
                match t {
                    Target::Obj(so) => {
                        // Still a spell on the stack (608.2b checked the target is legal).
                        let Some(snap) = crate::ext6::snapshot_spell(cx.s, so, 0) else { continue };
                        crate::ext6::copy_spell(cx, &snap, ex.you, ex.resolving.as_ref())?;
                    }
                    Target::Ability { uid, .. } => {
                        let Some(i) = ability_index(cx.s, uid) else { continue };
                        let it = cx.s.stack[i].clone();
                        copy_ability_checked(cx, &it, ex.you, ex.resolving.as_ref())?;
                    }
                    Target::Player(_) => {}
                }
            }
            Ok(())
        }
        X8Effect::ChooseCost { choices } => {
            let src = ex.source;
            let mut options: Vec<Opt> = choices
                .iter()
                .enumerate()
                .filter(|&(_, &a)| cx.db.ability(a).cost.as_ref().is_some_and(|c| crate::resolve::resolution_cost_payable(cx, ex.you, src, c)))
                .map(|(i, _)| Opt::Mode(i as u8))
                .collect();
            if options.is_empty() {
                return Ok(());
            }
            options.push(Opt::No);
            let Opt::Mode(i) = cx.choose(crate::engine::Decision { player: ex.you, kind: DecisionKind::ChooseMode, options, subject: Some(src) })? else { return Ok(()) };
            let chain = crate::resolve::nodes(cx.db, choices[i as usize], &[]);
            crate::resolve::apply_node_cost_chosen(cx, ex, chain[0])?;
            for &n in &chain[1..] {
                crate::resolve::apply_node(cx, ex, n, &[])?;
            }
            Ok(())
        }
        X8Effect::SpellToHand => {
            for &t in tg {
                let Target::Obj(so) = t else { continue };
                let Some(i) = cx.s.stack.iter().rposition(|it| it.source == so && it.kind == StackKind::Spell) else { continue };
                let it = cx.s.stack.remove(i);
                if cx.s.obj(so).token {
                    cx.s.move_obj(so, Zone::Gone, false);
                } else {
                    // Flashback/harmonize: exiled instead of going anywhere else (CR 702.34a).
                    let to = if it.flashback { Zone::Exile } else { Zone::Hand };
                    crate::zones::move_batch(cx, &[crate::zones::Move::new(so, to)])?;
                }
            }
            Ok(())
        }
    }
}

/// Copy an activated or triggered ability on the stack (CR 707.10): the copy keeps the
/// original's modes, X, targets, trigger event and the objects used to pay its costs. Side
/// tables an ability's resolution consumes are duplicated for the copy; the ones that can't be
/// told apart per copy fail loudly.
fn copy_ability_checked(cx: &mut Ctx, it: &StackItem, p: PlayerId, resolving: Option<&StackItem>) -> R<()> {
    // A divided effect (CR 601.2d) is recorded per source, not per stack object.
    if cx.s.ext.divided.iter().any(|d| d.0 == it.source) {
        return cx.unsupported("copying an ability with a divided effect");
    }
    // A delayed trigger's remembered objects (`ext5::delayed_remembered`) are taken by the first
    // of the two to resolve: the copy remembers the same objects.
    if it.kind == StackKind::Triggered && it.trigger_idx.is_none() {
        if let Some(d) = cx.s.ext.le.delayed_rem.iter().find(|d| d.0 == it.source && d.1 == it.ability).cloned() {
            cx.s.ext.le.delayed_rem.push(d);
        }
    }
    crate::ext6::copy_ability(cx, it, p, resolving)
}

/// CR 115.7a for a spell or ability with a single target (`t`): its controller's other legal
/// targets for that target word; `you` (the changing effect's controller) picks one; none:
/// unchanged. The new target "becomes the target" (ward, CR 702.21a).
fn change_single_target(cx: &mut Ctx, you: PlayerId, t: Target, resolving: Option<&StackItem>) -> R<()> {
    let idx = match t {
        Target::Obj(so) => cx.s.stack.iter().rposition(|it| it.source == so && it.kind == StackKind::Spell),
        Target::Ability { uid, .. } => ability_index(cx.s, uid),
        Target::Player(_) => None,
    };
    let Some(i) = idx else { return Ok(()) };
    let it = cx.s.stack[i].clone();
    if it.targets.len() != 1 {
        return Ok(()); // 115.9a: a single target as chosen; unchanged otherwise
    }
    let (node, old) = it.targets[0];
    let ns = crate::resolve::nodes(cx.db, it.ability, &it.modes);
    let spec = cx.db.ability(ns[node as usize]).target.clone().expect("targeted node without a target spec");
    // "Another" legal target (115.7a), never the spell or ability itself (115.5).
    let mut exclude: smallvec::SmallVec<[Target; 2]> = smallvec::smallvec![old];
    let spell = if it.kind == StackKind::Spell { Some(it.source) } else { None };
    if spell.is_none() {
        exclude.push(Target::Ability { uid: it.uid, src: it.source });
    }
    let mut legal = crate::cast::legal_targets(cx, it.controller, &spec, spell, &exclude);
    // The spell or ability changing it is still on the stack as it resolves (CR 608.2, 115.5).
    legal.extend(resolving_target(cx, it.controller, &spec, spell, &exclude, resolving));
    if legal.is_empty() {
        return Ok(());
    }
    let items: Vec<((u8, ObjSig), Opt)> = legal
        .iter()
        .map(|&x| {
            let k = match x {
                Target::Player(pl) => (0u8, (pl.0 as u32, 0, 0, 0, 0, 0)),
                Target::Obj(o) => (1, obj_sig(cx.s, cx.db, o)),
                Target::Ability { uid, .. } => (2, (uid, 0, 0, 0, 0, 0)),
            };
            (k, Opt::Target(x))
        })
        .collect();
    let Some(Opt::Target(new)) = cx.choose_set(you, DecisionKind::Target, items, 1, 1, Some(it.source))?.first().copied() else {
        return cx.unsupported("change target: no target chosen");
    };
    // Nothing else happens while it resolves (no player gets priority): same stack position.
    cx.s.stack[i].targets[0].1 = new;
    // The division (CR 601.2d) stays with the target slot (115.7f).
    for d in cx.s.ext.divided.iter_mut() {
        if d.0 == it.source && d.1 == old {
            d.1 = new;
        }
    }
    if let Target::Obj(o) = new {
        crate::cast::fire_targeted(cx, &[o], it.source, it.uid, it.controller);
    }
    Ok(())
}
