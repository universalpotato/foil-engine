//! Resolving spells and abilities (CR 608) and effect implementations.

use crate::basics::*;
use crate::carddb::*;
use crate::engine::*;
use crate::expr::{eval, holds, EvalCtx};
use crate::query::*;
use crate::state::*;
use crate::triggers::{fire, TrigEvent};
use crate::zones::{create_tokens, move_batch, Move};
use smallvec::SmallVec;

/// Execution context for one resolving ability chain.
#[derive(Clone, Debug)]
pub struct Exec {
    pub you: PlayerId,
    pub source: ObjId,
    pub source_card: CardId,
    pub x: i32,
    pub kicked: bool,
    pub trig: TrigCtx,
    /// The object a replacement effect is modifying (`Defined$ ReplacedCard`/`Self` in ETB).
    pub replaced: Option<ObjId>,
    pub remembered: SmallVec<[ObjId; 4]>,
    pub last_targets: SmallVec<[Target; 2]>,
    /// New objects from the most recent zone-changing node (for `Duration$`/exiled-with links).
    pub moved: SmallVec<[ObjId; 2]>,
    /// Targets of the targeted node before the latest one (`ParentTarget` of a targeted node).
    pub parent_targets: SmallVec<[Target; 2]>,
    /// Objects "imprinted" earlier in this resolution (Forge `Imprint$`, `Imprinted$Amount`).
    pub imprinted: SmallVec<[ObjId; 2]>,
    /// Objects whose copied "enter as a copy" ability this entering permanent has already
    /// applied (CR 707.9e example, 614.5: each such replacement applies once).
    pub copy_chain: SmallVec<[ObjId; 1]>,
    /// Objects the previous node put onto the battlefield that already got the next node's
    /// type change as they entered (`AnimateMoved`, see `apply_node`).
    pub entered_animated: SmallVec<[ObjId; 1]>,
    /// Nodes of this chain that don't happen because an earlier node's "you may [cost]. If you
    /// do, ..." cost wasn't paid (`CardDb::cost_gates_sub`, CR 608.2c).
    pub gated: SmallVec<[AbilityId; 2]>,
    /// The spell or ability resolving (popped off `State::stack` by `resolve_top`, but still on
    /// the stack until 608.2n): a legal new target for choices made as it resolves (CR 115.5,
    /// `ext8::resolving_target`). `None` outside a stack resolution.
    pub resolving: Option<StackItem>,
}

impl Exec {
    pub fn new(you: PlayerId, source: ObjId, source_card: CardId) -> Exec {
        Exec { you, source, source_card, x: 0, kicked: false, trig: TrigCtx::default(), replaced: None, remembered: SmallVec::new(), last_targets: SmallVec::new(), moved: SmallVec::new(), parent_targets: SmallVec::new(), imprinted: SmallVec::new(), copy_chain: SmallVec::new(), entered_animated: SmallVec::new(), gated: SmallVec::new(), resolving: None }
    }
    pub(crate) fn eval_ctx<'a>(&'a self, s: &'a State, db: &'a CardDb) -> EvalCtx<'a> {
        EvalCtx { s, db, you: self.you, source: self.replaced.unwrap_or(self.source), x: self.x, kicked: self.kicked, triggered: self.trig.obj, trigger_amount: self.trig.amount, targets: &self.last_targets, rem: &self.remembered, imprinted: &self.imprinted, base_only: false }
    }
}

/// Flatten an ability chain, expanding chosen modes of modal (Charm) nodes in place. Target
/// indices on stack items refer to positions in this list.
pub fn nodes(db: &CardDb, a: AbilityId, modes: &[u8]) -> SmallVec<[AbilityId; 6]> {
    let mut out = SmallVec::new();
    let mut cur = Some(a);
    while let Some(id) = cur {
        out.push(id);
        if let Effect::Charm { modes: ms, .. } = &db.ability(id).effect {
            for &m in modes {
                out.extend(nodes(db, ms[m as usize], &[]));
            }
        }
        cur = db.ability(id).sub;
    }
    out
}

/// Run an ability chain immediately (no stack, no targets): replacement bodies.
pub fn run_chain(cx: &mut Ctx, ex: &mut Exec, a: AbilityId) -> R<()> {
    for n in nodes(cx.db, a, &[]) {
        apply_node(cx, ex, n, &[])?;
    }
    crate::effect_ext::end_resolution(cx, ex.source);
    Ok(())
}

/// Resolve the top of the stack (CR 608). The item leaves `State::stack` (nobody can respond
/// to or target it any more) but stays on the stack as `State::resolving` until the final part
/// of its resolution (608.2n, 608.3a; `State::spell_left_stack` for a spell's object), so a
/// decision made as it resolves sees it there. At the end of the resolution it is
/// gone (an ability ceases to exist); a paused unit (`Halt::Need`) keeps it in its view.
pub fn resolve_top(cx: &mut Ctx) -> R<()> {
    let item = cx.s.stack.pop().expect("resolve with empty stack");
    debug_assert!(cx.s.resolving.is_none(), "nested stack resolution");
    cx.s.resolving = Some(item.clone());
    let r = resolve_item(cx, item);
    if !matches!(r, Err(Halt::Need(_))) {
        cx.s.resolving = None;
    }
    r
}

fn resolve_item(cx: &mut Ctx, item: StackItem) -> R<()> {
    let card = item.source_card;
    if item.ability.0 == u32::MAX {
        // Permanent spell (CR 608.3).
        cx.s.emit(Event::Resolve { card });
        // CR 107.3m: X in an "enters" ability of a permanent spell is the X paid for it
        // (`move_batch` moves the entry to the permanent before "enters with" replacements).
        if item.x != 0 {
            cx.s.ext.x_paid.push((item.source, item.x));
        }
        let out = move_batch(cx, &[Move { obj: item.source, to: Zone::Battlefield, to_top: false, tapped: false, controller: Some(item.controller) }])?;
        if out[0].is_none() {
            cx.s.ext.x_paid.retain(|x| x.0 != item.source);
        }
        if let (Some(nid), false) = (out[0], cx.s.ext.lt.sneak.is_empty()) {
            crate::ext3::sneak_entered(cx, item.source, nid);
        }
        return Ok(());
    }
    let ns = nodes(cx.db, item.ability, &item.modes);
    // 608.2b: which targets are still legal?
    let mut legal: SmallVec<[(u8, Target); 2]> = SmallVec::new();
    for &(n, t) in &item.targets {
        let spec = cx.db.ability(ns[n as usize]).target.as_ref().unwrap();
        // "Target Equipment attached to that creature" depends on the earlier target: with that
        // target illegal, so is this one (`&legal`: the legal targets so far; Fiery Annihilation
        // ruling 2024-11-08).
        // An ability's targets are checked against its source's "hexproof from" quality too, as
        // when they were chosen (`target_still_legal`; CR 702.11d, 608.2b: the source's last
        // known information if it left).
        if crate::cast::target_still_legal(cx, &item, spec, t) && crate::ext9::attached_to_parent_ok(cx, ns[n as usize], n as usize, &legal, t) {
            legal.push((n, t));
        }
    }
    let leave_to = if item.flashback { Zone::Exile } else { Zone::Graveyard };
    if !item.targets.is_empty() && legal.is_empty() {
        cx.s.emit(Event::Fizzle { card });
        if item.kind == StackKind::Spell {
            if cx.s.obj(item.source).token {
                cx.s.move_obj(item.source, Zone::Gone, false);
            } else {
                move_batch(cx, &[Move::new(item.source, leave_to)])?;
            }
        }
        return Ok(());
    }
    let mut ex = Exec { you: item.controller, source: item.source, source_card: card, x: item.x, kicked: item.kicked, trig: item.trig, replaced: None, remembered: SmallVec::new(), last_targets: SmallVec::new(), moved: SmallVec::new(), parent_targets: SmallVec::new(), imprinted: SmallVec::new(), copy_chain: SmallVec::new(), entered_animated: SmallVec::new(), gated: SmallVec::new(), resolving: Some(item.clone()) };
    crate::ext6::note_resolving(cx.s, &item);
    if item.kind == StackKind::Triggered && item.trigger_idx.is_none() && !cx.s.ext.le.delayed_rem.is_empty() {
        crate::ext5::delayed_remembered(cx.s, &item, &mut ex);
    }
    if let (StackKind::Triggered, Some(ti)) = (item.kind, item.trigger_idx) {
        let td = &cx.db.get(card).front().triggers[ti as usize];
        // 603.4: intervening "if" rechecked on resolution.
        let ecx = ex.eval_ctx(cx.s, cx.db);
        if !td.intervening.iter().all(|c| matches!(c, crate::expr::Condition::TriggerOnly(_)) || holds(c, &ecx)) {
            return Ok(());
        }
        // 603.5: "may" chosen on resolution.
        if td.optional && cx.choose(Decision { player: item.controller, kind: DecisionKind::OptionalEffect, options: vec![Opt::Yes, Opt::No], subject: Some(item.source) })? == Opt::No {
            return Ok(());
        }
    }
    cx.s.emit(Event::Resolve { card });
    let mut holders = false;
    for (i, &n) in ns.iter().enumerate() {
        let ad = cx.db.ability(n);
        let tg: SmallVec<[Target; 2]> = legal.iter().filter(|t| t.0 as usize == i).map(|t| t.1).collect();
        if ad.target.is_some() {
            if tg.is_empty() {
                continue; // this part's targets are all illegal (608.2b)
            }
            // Consecutive target-only parts ("up to one target card of each type", Forge no-op
            // `Pump`s) together are what a later `Defined$ Targeted` refers to (Forge: all
            // targets of the ability).
            let holder = crate::ext2::target_holder(ad);
            if holder && holders {
                ex.last_targets.extend(tg.iter().copied());
            } else {
                ex.parent_targets = std::mem::replace(&mut ex.last_targets, tg.clone());
            }
            holders = holder;
        }
        apply_node(cx, &mut ex, n, &tg)?;
    }
    if item.kind == StackKind::Spell && cx.s.obj(item.source).zone == Zone::Stack {
        if cx.s.obj(item.source).token {
            // A copy of a paradigm spell also counts as one that resolved (CR 702.192a: "a
            // spell you control with this spell's name"); exiling it = ceasing to exist.
            if cx.db.get(card).face(cx.s.obj(item.source).face).extra.paradigm {
                crate::ext7::paradigm_resolved(cx, &item);
            }
            // CR 704.5e/707.10a: a copy of a spell ceases to exist when it leaves the stack.
            cx.s.move_obj(item.source, Zone::Gone, false);
        } else if cx.s.obj(item.source).face == 1 && cx.db.get(card).layout == Layout::Omen {
            // CR 720.3d: shuffled into its owner's library instead.
            let owner = cx.s.obj(item.source).owner;
            move_batch(cx, &[Move::new(item.source, Zone::Library)])?;
            crate::flow::shuffle_library(cx.s, owner);
        } else if cx.s.obj(item.source).face == 1 && cx.db.get(card).layout == Layout::Adventure {
            // CR 715.3d: exile it "on an adventure"; its controller may play it.
            let ctrl = item.controller;
            if let Some(nid) = move_batch(cx, &[Move::new(item.source, Zone::Exile)])?[0] {
                cx.s.obj_mut(nid).on_adventure = true;
                // Entries of cards that left exile are dropped (their ids are never reused).
                let keep: SmallVec<[(ObjId, PlayerId); 0]> = cx.s.ext.adventure_player.iter().copied().filter(|e| cx.s.obj(e.0).zone == Zone::Exile).collect();
                cx.s.ext.adventure_player = keep;
                if ctrl != cx.s.obj(nid).owner {
                    cx.s.ext.adventure_player.push((nid, ctrl));
                }
            }
        } else if cx.db.get(card).face(cx.s.obj(item.source).face).extra.paradigm {
            // CR 702.192a: "Exile this spell", and the first resolution sets up the copies.
            crate::ext7::paradigm_resolved(cx, &item);
            move_batch(cx, &[Move::new(item.source, Zone::Exile)])?;
        } else {
            move_batch(cx, &[Move::new(item.source, leave_to)])?;
        }
    }
    crate::effect_ext::end_resolution(cx, item.source);
    Ok(())
}

/// The player who performs an effect node, when it isn't the controller (`Defined$ You`): a
/// search by `DefinedPlayer$`, a choice by `Chooser$`. That player makes its "may" choice.
fn optional_actor(e: &Effect) -> Option<Defined> {
    match e {
        Effect::ChangeZone { searcher, .. } if *searcher != Defined::You => Some(*searcher),
        Effect::Ext(crate::effect_ext::ExtEffect::ChooseFromZone { chooser, .. }) if *chooser != Defined::You => Some(*chooser),
        _ => None,
    }
}

/// Player actions (the `player_effect` effects of `apply_effect`) whose "may" the affected
/// player decides, not the controller (`optional_actor` covers searches and `Chooser$`).
fn performer_decides(e: &Effect) -> bool {
    match e {
        Effect::Discard { mode, .. } => *mode != DiscardMode::RevealYouChoose,
        Effect::Draw { .. } | Effect::GainLife { .. } | Effect::LoseLife { .. } | Effect::Scry { .. } | Effect::Surveil { .. } | Effect::Mill { .. } | Effect::Sacrifice { .. } => true,
        _ => false,
    }
}

pub(crate) fn apply_node(cx: &mut Ctx, ex: &mut Exec, n: AbilityId, tg: &[Target]) -> R<()> {
    // `ChangeZone | WithCountersType$` ("return it to the battlefield with a +1/+1 counter on
    // it"): what this node puts onto the battlefield enters with those counters (CR 122.6,
    // 614.1c), so enters triggers see them (603.6a, 603.10). Every form of the
    // move (plain, search, transformed, Aura) enters through `zones::move_batch` /
    // `move_special`, which apply `Ext::enter_with_node`.
    if let Some(&(_, k, c)) = cx.db.enter_counters.iter().find(|e| e.0 == n) {
        let prev = cx.s.ext.enter_with_node.replace((k, c));
        let r = apply_node_inner(cx, ex, n, tg, false);
        cx.s.ext.enter_with_node = prev;
        return r;
    }
    apply_node_inner(cx, ex, n, tg, false)
}

/// `apply_node` for a "you may [cost]. If you do, [effect]" node whose cost its controller
/// already chose to pay (`ext8` "choose one of these costs"): pays it without asking again.
pub(crate) fn apply_node_cost_chosen(cx: &mut Ctx, ex: &mut Exec, n: AbilityId) -> R<()> {
    if cx.db.enter_counters.iter().any(|e| e.0 == n) {
        return cx.unsupported("chosen-cost node that enters with counters");
    }
    apply_node_inner(cx, ex, n, &[], true)
}

/// For an optional node whose action needs something to act on (CR 608.2d: "the player can't
/// choose an option that's illegal or impossible"; 118.3 for "you may [cost]. If you do"): is
/// there anything? True for every other node.
/// - discarding chosen cards ("You may choose a card from it. If you do, that player discards
///   that card"; "you may discard a card"): a matching card in that hand;
/// - "you may sacrifice a [permanent]" (Reactor Raid; "sacrifice this" once it
///   left, 400.7): a matching permanent that player controls (701.21a);
/// - "you may put a [card] from among them / from your hand / graveyard ..." (a hidden-zone
///   choice not from a library): a matching card. (Searching a library may
///   find nothing, 701.23b, and still shuffles: always offered.)
fn optional_choice_possible(cx: &Ctx, ex: &Exec, n: AbilityId, ad: &AbilityDef, tg: &[Target]) -> bool {
    match &ad.effect {
        Effect::Discard { mode: DiscardMode::Choose | DiscardMode::RevealYouChoose, valid, .. } => {
            let fcx = exec_fcx!(ex);
            players(&affected(cx, ex, ad, tg, true)).into_iter().any(|p| cx.s.player(p).hand.iter().any(|&h| valid.as_ref().is_none_or(|f| obj_matches(cx.s, cx.db, f, h, fcx))))
        }
        Effect::Sacrifice { valid, .. } => players(&affected(cx, ex, ad, tg, true)).into_iter().any(|p| {
            let fcx = FilterCtx { you: p, x: 0, ..exec_fcx!(ex) };
            cx.s.battlefield.iter().any(|&o| cx.s.obj(o).controller == p && obj_matches(cx.s, cx.db, valid, o, fcx))
        }),
        Effect::ChangeZone { origin, hidden: true, change_type: Some(f), searcher, .. } if !origin.has(Zone::Library) => {
            affected_players(cx, ex, Some(*searcher)).first().is_some_and(|&p| !hidden_change_pool(cx, ex, n, p, *origin, f).is_empty())
        }
        // The same choice compiled as a one-zone search of a public zone ("then you may return a
        // non-Avatar creature card or a planeswalker card from your graveyard to your hand",
        // Overlord of the Balemurk): a matching card.
        Effect::Ext(crate::effect_ext::ExtEffect::More(crate::ext2::MoreEffect::SearchZone { origin: origin @ (Zone::Graveyard | Zone::Exile), change_type, owner, .. })) => {
            crate::ext2::search_zone_owners(cx, ex, *owner).into_iter().any(|o_p| !crate::ext2::search_zone_pool(cx, ex, *origin, change_type, o_p).is_empty())
        }
        _ => true,
    }
}

/// The cards a hidden-zone `ChangeZone` (`Hidden$`, node `n`) by `p` chooses from: matching
/// cards in `p`'s origin zones (exile: the ones `p` owns, as Forge, or every card exiled with
/// the source for an `ExiledWithSource` choice, CR 607.2a), in library, hand, graveyard,
/// exile order; from public zones without `DefinedPlayer$` every player's graveyard and exile
/// cards (`CardDb::hidden_all_owners`, Forge `game.getCardsIn(origin)`).
/// `IsRemembered` is what this resolution remembered so far ("mill four cards. You may put a land
/// card from among them into your hand", Forge `RememberMilled$` + `ChangeType$ Land.IsRemembered`).
fn hidden_change_pool(cx: &Ctx, ex: &Exec, n: AbilityId, p: PlayerId, origin: ZoneSet, filter: &crate::filter::Filter) -> Vec<ObjId> {
    let fcx = FilterCtx { you: p, x: 0, ..exec_fcx!(ex) };
    let all_owners = cx.db.hidden_all_owners.contains(&n);
    let mut pool: Vec<ObjId> = vec![];
    if origin.has(Zone::Library) {
        pool.extend(cx.s.player(p).library.iter().copied());
    }
    if origin.has(Zone::Hand) {
        pool.extend(cx.s.player(p).hand.iter().copied());
    }
    if origin.has(Zone::Graveyard) {
        for q in [p, p.opp()] {
            if q == p || all_owners {
                pool.extend(cx.s.player(q).graveyard.iter().copied());
            }
        }
    }
    // Cards in exile (Purgatory, Endless Horizons, Roads Go Ever, Ever On). Forge: the searcher's
    // own exiled cards (`player.getCardsIn(Exile)`) with `DefinedPlayer$`, everyone's without
    // (`game.getCardsIn(Exile)`; Memory Theft).
    // "A card exiled with [this]" (`ExiledWithSource` in every alternative) is linked to the
    // object, not to a player (CR 607.2a): whoever controls it now chooses among all of them
    // (Purgatory ruling 2004-10-04: "If an opponent takes control of this card and pays for the
    // upkeep ability, the creatures will enter under their control even if you exiled them").
    let linked = !filter.alts.is_empty() && filter.alts.iter().all(|a| a.props.iter().any(|pr| matches!(pr, crate::filter::Prop::Ext(crate::filter_ext::ExtProp::ExiledWithSource))));
    if origin.has(Zone::Exile) {
        pool.extend(cx.s.exile.iter().copied().filter(|&o| all_owners || linked || cx.s.obj(o).owner == p));
    }
    // Tokens there (just died, not yet gone) aren't cards (`query::is_card`).
    pool.into_iter().filter(|&o| is_card(cx.s, o) && obj_matches(cx.s, cx.db, filter, o, fcx)).collect()
}

/// `n`'s resolution cost wasn't paid: if it is a trigger body's "you may [cost]. If you do,
/// [all of this]" (`CardDb::cost_gates_sub`), its sub-abilities don't happen either (CR 608.2c).
fn gate_chain(db: &CardDb, ex: &mut Exec, n: AbilityId) {
    if !db.cost_gates_sub.contains(&n) {
        return;
    }
    let mut cur = db.ability(n).sub;
    while let Some(a) = cur {
        // `nodes` expands a modal node's modes in place: they are part of the chain too.
        if let Effect::Charm { modes, .. } = &db.ability(a).effect {
            for &m in modes {
                ex.gated.extend(nodes(db, m, &[]));
            }
        }
        ex.gated.push(a);
        cur = db.ability(a).sub;
    }
}

fn apply_node_inner(cx: &mut Ctx, ex: &mut Exec, n: AbilityId, tg: &[Target], cost_chosen: bool) -> R<()> {
    let ad = cx.db.ability(n);
    if !ex.gated.is_empty() && ex.gated.contains(&n) {
        ex.moved.clear();
        return Ok(());
    }
    {
        let ecx = ex.eval_ctx(cx.s, cx.db);
        if !ad.conditions.iter().all(|c| holds(c, &ecx) && crate::effect_ext::defined_cond_holds(cx, ex, c)) {
            ex.moved.clear(); // follow-up nodes acting on what this node moved do nothing
            return Ok(());
        }
    }
    if ad.optional && !optional_choice_possible(cx, ex, n, ad, tg) {
        // CR 608.2d: "you may choose a card from it" with no card in that hand to choose, "you
        // may sacrifice an artifact" with none, ...: the option is impossible, so there's no
        // decision (and nothing happens either way: Oildeep Gearhulk asked about
        // an empty hand).
        ex.moved.clear();
        return Ok(());
    }
    if ad.optional {
        // CR 608.2d: the player the text names makes the "may" choice ("Its controller may
        // search ...", Erode / Assassin's Trophy), not necessarily the controller.
        let decider = match optional_actor(&ad.effect) {
            // "Any opponent may sacrifice a creature" (Desecration Demon), "target player may
            // ...": the player who would perform a player action decides (CR 608.2d: the text
            // names who chooses; Forge `SacrificeEffect` / `MillEffect` / `ScryEffect` / ...:
            // `p.getController().confirmAction` for each affected player `p`). The controller's
            // own choice of a card to discard (`RevealYouChoose`) stays theirs.
            None if performer_decides(&ad.effect) => {
                let ps = players(&affected(cx, ex, ad, tg, true));
                if ps.len() > 1 {
                    return cx.unsupported("optional player action performed by several players");
                }
                ps.first().copied()
            }
            None => Some(ex.you),
            Some(d) => {
                let ps = affected_players(cx, ex, Some(d));
                if ps.len() > 1 {
                    return cx.unsupported(format!("optional effect performed by several players ({d:?})"));
                }
                ps.first().copied()
            }
        };
        // No such player (e.g. the target is gone): nobody performs it.
        let Some(decider) = decider else {
            ex.moved.clear();
            return Ok(());
        };
        if cx.choose(Decision { player: decider, kind: DecisionKind::OptionalEffect, options: vec![Opt::Yes, Opt::No], subject: Some(ex.source) })? == Opt::No {
            ex.moved.clear();
            return Ok(());
        }
    }
    if ad.kind != AbilityKind::Activated {
        if let Some(cost) = ad.cost.clone() {
            // "You may [cost]. If you do, ...": an optional cost paid on resolution. If it
            // can't be paid, it isn't (and this part doesn't happen). A trigger body's cost
            // gates its sub-abilities too (CR 608.2c, "If you do").
            if !resolution_cost_payable(cx, ex.you, ex.source, &cost) {
                gate_chain(cx.db, ex, n);
                return Ok(());
            }
            if !cost_chosen && cx.choose(Decision { player: ex.you, kind: DecisionKind::PayCost, options: vec![Opt::Yes, Opt::No], subject: Some(ex.source) })? == Opt::No {
                gate_chain(cx.db, ex, n);
                return Ok(());
            }
            let before = crate::invariants::cost_begin(cx.s, || crate::invariants::zero_mana(&cost.mana) && Cost { mana: cost.mana.clone(), ..Cost::default() } == cost);
            crate::cast::pay_mana_reserving(cx, ex.you, crate::cast::requirement(&cost.mana, 0), None, ex.source, false, None, Some(crate::cast::ReserveFor::Cost(&cost)))?;
            if let Some((k, n)) = cost.counter {
                crate::ext6::put_counters(cx.s, cx.db, ex.source, k, n);
            }
            if cost.remove_any > 0 {
                crate::ext5::remove_any_counter(cx, ex.you, ex.source)?;
            }
            crate::cast::pay_other_costs(cx, ex.you, ex.source, &cost)?;
            crate::invariants::cost_paid(before, cx.s, cx.db, ex.source_card, "a resolution cost");
        }
    }
    if let Some((cost, payer)) = ad.unless.clone() {
        // "Unless [payer] pays [cost]": if paid, this part of the effect doesn't happen.
        // Switched (`UnlessSwitched`): "[payer] may pay [cost]; if they do, [effect]".
        let (cost, switched) = match cost {
            UnlessCost::Switched(c) => (*c, true),
            c => (c, false),
        };
        let ex_t = Exec { last_targets: if tg.is_empty() { ex.last_targets.clone() } else { tg.iter().copied().collect() }, ..ex.clone() };
        let payers = affected_players(cx, &ex_t, Some(payer));
        // Generic mana given by an expression (evaluated as the effect resolves).
        let cost = match cost {
            UnlessCost::Generic(e) => {
                let n = amount(cx, ex, &e).max(0);
                // `ManaSym::Generic` holds a u8 (as do mana pools): fail loudly past it.
                if n > u8::MAX as i32 {
                    return cx.unsupported(format!("unless-cost of {{{n}}} generic mana (more than {} is not representable)", u8::MAX));
                }
                let n = n as u8;
                UnlessCost::Mana(ManaCost { syms: if n > 0 { smallvec::smallvec![ManaSym::Generic(n)] } else { SmallVec::new() }, none: false })
            }
            UnlessCost::LifeExpr(e) => UnlessCost::Life(amount(cx, ex, &e).max(0)),
            c => c,
        };
        let mut paid = false;
        for p in payers {
            let can = match &cost {
                // CR 119.4: can pay life only if life total >= amount.
                UnlessCost::Life(n) => cx.s.player(p).life >= *n,
                UnlessCost::Mana(m) => crate::cast::payable(&crate::cast::requirement(m, 0), &crate::cast::mana_sources(cx, p, None)),
                UnlessCost::Behold(f) => crate::altcast::can_behold(cx, p, ex.source, f),
                UnlessCost::Generic(_) | UnlessCost::LifeExpr(_) | UnlessCost::Switched(_) => return cx.unsupported("nested unless cost"),
            };
            if !can {
                continue;
            }
            // What is due, for the view of this decision (encoding v10.2; left set while paused).
            cx.s.ext.unless_due = Some(match &cost {
                UnlessCost::Life(n) => crate::observe::UnlessView { life: *n, ..Default::default() },
                UnlessCost::Mana(m) => crate::observe::UnlessView { mana: m.mana_value().min(255) as u8, pips: crate::observe::cost_pips(m), ..Default::default() },
                _ => crate::observe::UnlessView { other: true, ..Default::default() },
            });
            let c = cx.choose(Decision { player: p, kind: DecisionKind::PayUnless, options: vec![Opt::Yes, Opt::No], subject: Some(ex.replaced.unwrap_or(ex.source)) })?;
            cx.s.ext.unless_due = None;
            if c == Opt::Yes {
                // (Beholding a permanent you control, or a card already revealed, spends nothing:
                // CR 701.4a, not checked.)
                let before = crate::invariants::cost_begin(cx.s, || match &cost {
                    UnlessCost::Life(n) => *n <= 0,
                    UnlessCost::Mana(m) => crate::invariants::zero_mana(m),
                    UnlessCost::Behold(_) => true,
                    _ => false,
                });
                match &cost {
                    UnlessCost::Life(n) => lose_life(cx.s, cx.db, p, *n),
                    UnlessCost::Mana(m) => crate::cast::pay_mana(cx, p, crate::cast::requirement(m, 0), None, ex.source)?,
                    UnlessCost::Behold(f) => {
                        crate::altcast::behold(cx, p, ex.source, f)?;
                    }
                    UnlessCost::Generic(_) | UnlessCost::LifeExpr(_) | UnlessCost::Switched(_) => unreachable!(),
                }
                crate::invariants::cost_paid(before, cx.s, cx.db, ex.source_card, "an \"unless\" cost");
                if !switched {
                    return Ok(());
                }
                paid = true;
                break;
            }
        }
        if switched && !paid {
            return Ok(());
        }
    }
    cx.s.ext.res_targets = if tg.is_empty() { ex.last_targets.clone() } else { tg.iter().copied().collect() };
    if !ex.remembered.is_empty() || !cx.s.ext.res_remembered.is_empty() {
        cx.s.ext.res_remembered = ex.remembered.clone();
    }
    // "Return it to the battlefield ... It's an enchantment. (It's not a creature.)": the type
    // change (the next node, `AnimateMoved`) is part of how it enters, so it applies as the
    // permanent enters (CR 614.12) and its enters triggers see it (603.6a, 603.10; Enduring
    // Curiosity ruling 2024-09-20: it "won't be a creature"). `zones::move_batch` applies it.
    let animate = cx.db.ability(n).sub.filter(|&a| matches!(cx.db.ability(a).effect, Effect::Ext(crate::effect_ext::ExtEffect::AnimateMoved { .. })));
    if let Some(a) = animate {
        if cx.s.ext.lki.enter_animate.is_some() {
            return cx.unsupported("nested enters-as-a-type-changed effects");
        }
        cx.s.ext.lki.enter_animate = Some((a, SmallVec::new()));
        apply_effect(cx, ex, n, tg)?;
        ex.entered_animated = cx.s.ext.lki.enter_animate.take().map(|x| x.1).unwrap_or_default();
        return Ok(());
    }
    apply_effect(cx, ex, n, tg)
}

/// Can `att` (an Equipment or Aura) legally be attached to `to` (CR 301.5c, 303.4)?
pub fn can_attach(s: &State, db: &CardDb, att: ObjId, to: ObjId) -> bool {
    if att == to || s.obj(att).zone != Zone::Battlefield || s.obj(to).zone != Zone::Battlefield {
        return false;
    }
    let ac = chars(s, db, att);
    let equipment = db.subtype("Equipment").is_some_and(|st| ac.subtypes.contains(&st));
    if equipment {
        return !ac.types.has(CardType::Creature) && chars(s, db, to).types.has(CardType::Creature);
    }
    let card = s.obj(att).card;
    match &db.get(card).face(s.obj(att).face).enchant {
        Some(f) if ac.types.has(CardType::Enchantment) && !ac.types.has(CardType::Creature) => obj_matches(s, db, f, to, FilterCtx { you: s.obj(att).controller, source: Some(att), x: 0, rem: &[] }),
        _ => false,
    }
}

pub(crate) fn resolution_cost_payable(cx: &Ctx, p: PlayerId, src: ObjId, c: &Cost) -> bool {
    if c.tap || c.sac_self || c.discard_self || c.return_unblocked {
        return false; // not used as resolution costs by supported cards
    }
    if let Some((k, n)) = c.counter {
        // Counters are put on or removed from the source permanent: once it has left the
        // battlefield it is a new object (CR 400.7), and the old one's last known counters can't
        // pay (118.3; Guiding Hydra destroyed in response).
        if cx.s.obj(src).zone != Zone::Battlefield || (n < 0 && cx.s.obj(src).counter(k) < -n) {
            return false;
        }
    }
    if c.remove_any > 0 && (cx.s.obj(src).zone != Zone::Battlefield || cx.s.obj(src).counters.iter().map(|c| c.1.max(0)).sum::<i32>() < c.remove_any as i32) {
        return false;
    }
    crate::cast::other_costs_payable(cx, p, src, c) && crate::cast::payable_reserving(&crate::cast::requirement(&c.mana, 0), &crate::cast::mana_sources(cx, p, None), crate::cast::Reserve::of(cx, p, src, c).as_ref())
}

pub(crate) fn affected_players(cx: &Ctx, ex: &Exec, d: Option<Defined>) -> SmallVec<[PlayerId; 2]> {
    match d {
        Some(Defined::You) | None => smallvec::smallvec![ex.you],
        Some(Defined::Opponent) => smallvec::smallvec![ex.you.opp()],
        Some(Defined::TargetedController) | Some(Defined::TargetedPlayer) | Some(Defined::Targeted) => ex
            .last_targets
            .iter()
            .map(|t| match t {
                Target::Obj(o) => cx.s.obj(*o).controller,
                Target::Player(p) => *p,
                // An ability's controller (CR 113.8); its source's once it left the stack.
                Target::Ability { uid, src } => crate::ext8::ability_index(cx.s, *uid).map_or(cx.s.obj(*src).controller, |i| cx.s.stack[i].controller),
            })
            .collect(),
        Some(Defined::TriggeredController) => ex.trig.obj.map(|o| cx.s.obj(o).controller).or(ex.trig.player).into_iter().collect(),
        Some(Defined::TriggeredPlayer) => ex.trig.player.into_iter().collect(),
        Some(Defined::RememberedController) => ex.remembered.first().map(|&o| cx.s.obj(o).controller).into_iter().collect(),
        _ => smallvec::smallvec![ex.you],
    }
}

/// Objects/players an effect node applies to.
pub(crate) fn affected(cx: &Ctx, ex: &Exec, ad: &AbilityDef, tg: &[Target], player_effect: bool) -> SmallVec<[Target; 2]> {
    if ad.target.is_some() {
        return tg.iter().copied().collect();
    }
    match ad.defined {
        // Inside `RepeatEach | RepeatPlayers$`, "Remembered" is the current player.
        Some(Defined::Remembered) if cx.s.ext.rep_player.is_some() => smallvec::smallvec![Target::Player(cx.s.ext.rep_player.unwrap())],
        Some(Defined::SelfObj) => smallvec::smallvec![Target::Obj(ex.replaced.unwrap_or(ex.source))],
        Some(Defined::Replaced) => ex.replaced.map(Target::Obj).into_iter().collect(),
        Some(Defined::Targeted) if !player_effect => ex.last_targets.clone(),
        Some(Defined::Triggered) => {
            // Prefer the object's current incarnation if it moved (e.g. "return it").
            let o = match (ex.trig.obj, ex.trig.new_obj) {
                (Some(old), Some(new)) if cx.s.obj(old).zone == Zone::Gone => Some(new),
                (Some(old), _) => Some(old),
                (None, n) => n,
            };
            o.map(Target::Obj).into_iter().collect()
        }
        Some(Defined::Remembered) => ex.remembered.iter().map(|&o| Target::Obj(o)).collect(),
        Some(Defined::Imprinted) => ex.imprinted.iter().map(|&o| Target::Obj(o)).collect(),
        Some(Defined::ChosenCard) => cx.s.ext.chosen_cards.iter().filter(|c| c.0 == ex.source).map(|c| Target::Obj(c.1)).collect(),
        // The permanent the source Aura/Equipment is attached to (`Enchanted`, `Equipped`).
        Some(Defined::Attached) => cx.s.obj(ex.source).attached_to.map(Target::Obj).into_iter().collect(),
        d if player_effect => affected_players(cx, ex, d).into_iter().map(Target::Player).collect(),
        None => smallvec::smallvec![Target::Obj(ex.replaced.unwrap_or(ex.source))],
        d => affected_players(cx, ex, d).into_iter().map(Target::Player).collect(),
    }
}

pub(crate) fn objs(ts: &[Target]) -> SmallVec<[ObjId; 4]> {
    ts.iter().filter_map(|t| if let Target::Obj(o) = t { Some(*o) } else { None }).collect()
}
pub(crate) fn players(ts: &[Target]) -> SmallVec<[PlayerId; 2]> {
    ts.iter().filter_map(|t| if let Target::Player(p) = t { Some(*p) } else { None }).collect()
}

fn apply_effect(cx: &mut Ctx, ex: &mut Exec, n: AbilityId, tg: &[Target]) -> R<()> {
    // Rules probes (`invariants`; nothing without the test-only feature): one move event per
    // instruction, created tokens in the moved set.
    let frame = crate::invariants::node_begin(cx.s);
    let r = apply_effect_body(cx, ex, n, tg);
    crate::invariants::node_end(frame, r.is_ok(), cx.s, cx.db, n, ex.source_card, &ex.moved);
    r
}

fn apply_effect_body(cx: &mut Ctx, ex: &mut Exec, n: AbilityId, tg: &[Target]) -> R<()> {
    let node_id = n;
    let ad = cx.db.ability(n).clone();
    let player_effect = matches!(
        ad.effect,
        Effect::Draw { .. } | Effect::GainLife { .. } | Effect::LoseLife { .. } | Effect::Scry { .. } | Effect::Surveil { .. } | Effect::Mill { .. } | Effect::Discard { .. } | Effect::Sacrifice { .. } | Effect::Dig { .. }
    );
    let who = affected(cx, ex, &ad, tg, player_effect);
    let num = |cx: &mut Ctx, ex: &Exec, e: &crate::expr::Expr| amount(cx, ex, e);
    match &ad.effect {
        Effect::DealDamage { amount } => {
            let dmg = num(cx, ex, amount);
            deal_damage_each(cx, ex.source, &who, dmg)?;
        }
        Effect::Draw { n } => {
            let k = num(cx, ex, n);
            for p in players(&who) {
                crate::flow::draw_n(cx.s, cx.db, p, k);
            }
        }
        Effect::GainLife { n } => {
            let k = num(cx, ex, n);
            for p in players(&who) {
                gain_life(cx.s, cx.db, p, k, ex.source);
            }
        }
        Effect::LoseLife { n } => {
            let k = num(cx, ex, n);
            for p in players(&who) {
                lose_life(cx.s, cx.db, p, k);
            }
        }
        Effect::Destroy => destroy_all(cx, &objs(&who))?,
        Effect::DestroyAll { valid } => {
            let fcx = exec_fcx!(ex);
            let v: SmallVec<[ObjId; 4]> = cx.s.battlefield.iter().copied().filter(|&o| obj_matches(cx.s, cx.db, valid, o, fcx)).collect();
            destroy_all(cx, &v)?;
        }
        Effect::Pump { power, toughness, kws, duration } => {
            let (p, t) = (num(cx, ex, power), num(cx, ex, toughness));
            for o in objs(&who) {
                pump(cx.s, ex.you, o, p, t, *kws, *duration);
            }
        }
        Effect::PumpAll { valid, power, toughness, kws, duration } => {
            let (p, t) = (num(cx, ex, power), num(cx, ex, toughness));
            let fcx = exec_fcx!(ex);
            let v: Vec<ObjId> = cx.s.battlefield.iter().copied().filter(|&o| obj_matches(cx.s, cx.db, valid, o, fcx)).collect();
            if ad.remember {
                ex.remembered.extend(v.iter().copied()); // `RememberPumped$`
            }
            for o in v {
                pump(cx.s, ex.you, o, p, t, *kws, *duration);
            }
        }
        Effect::Scry { n } => {
            let k = num(cx, ex, n);
            for p in players(&who) {
                look_and_sort(cx, p, k, false)?;
            }
        }
        Effect::Surveil { n } => {
            let k = num(cx, ex, n);
            for p in players(&who) {
                let gone = look_and_sort(cx, p, k, true)?;
                if ad.remember {
                    ex.remembered.extend(gone);
                }
            }
        }
        Effect::Mill { n } => {
            let k = num(cx, ex, n).max(0) as usize;
            // "Each player mills N": one event (CR 101.4, 608.2c; Forge `MillEffect` moves every
            // player's cards with one zone table), as `Discard` and `Sacrifice`.
            let ps = players(&who);
            let top: Vec<Move> = ps.iter().flat_map(|&p| cx.s.player(p).library.iter().rev().take(k).map(|&o| Move::new(o, Zone::Graveyard))).collect();
            if !ps.is_empty() {
                let new = move_batch(cx, &top)?;
                if ad.remember {
                    ex.remembered.extend(new.into_iter().flatten());
                }
            }
        }
        Effect::Tap | Effect::Untap => {
            for o in objs(&who) {
                if cx.s.obj(o).zone == Zone::Battlefield {
                    let ob = cx.s.obj_mut(o);
                    if matches!(ad.effect, Effect::Untap) && ob.tapped && ob.counter(CounterKind::Stun) > 0 {
                        ob.add_counter(CounterKind::Stun, -1); // CR 122.1d: instead of untapping
                    } else {
                        ob.tapped = matches!(ad.effect, Effect::Tap);
                    }
                }
            }
        }
        Effect::Counter => {
            // "Counter that spell or ability" of a becomes-the-target trigger (ward, CR 702.21a):
            // the stack object that targeted, not another ability of its source (113.7a);
            // nothing if it already left the stack.
            let by_item = match (ad.defined, ex.trig.stack_item) {
                (Some(Defined::Triggered), Some(uid)) => Some(uid),
                _ => None,
            };
            let who: SmallVec<[Target; 2]> = match by_item {
                Some(uid) => cx.s.stack.iter().filter(|it| it.uid == uid).map(|it| if it.kind == StackKind::Spell { Target::Obj(it.source) } else { Target::Ability { uid, src: it.source } }).collect(),
                None => who,
            };
            // A targeted ability (CR 113.7a): exactly that stack object (`ext8`).
            for t in &who {
                let Target::Ability { uid, src } = *t else { continue };
                if let Some(i) = crate::ext8::ability_index(cx.s, uid) {
                    let it = cx.s.stack.remove(i);
                    cx.s.emit(Event::Countered { card: it.source_card });
                    if ad.remember {
                        ex.remembered.push(src);
                    }
                }
            }
            for o in objs(&who) {
                if crate::effect_ext::cant_be_countered(cx, o) || (cx.s.stack.iter().any(|it| it.source == o && it.kind == StackKind::Spell) && crate::static_ext::spell_cant_be_countered(cx.s, cx.db, o)) {
                    continue; // CR 101.2: "can't be countered" wins
                }
                let spell = cx.s.stack.iter().position(|it| it.source == o && it.kind == StackKind::Spell && by_item.is_none_or(|u| it.uid == u));
                if let Some(i) = spell {
                    let it = cx.s.stack.remove(i);
                    cx.s.emit(Event::Countered { card: it.source_card });
                    if cx.s.obj(o).token {
                        cx.s.move_obj(o, Zone::Gone, false); // a countered copy ceases to exist
                    } else {
                        let to = if it.flashback { Zone::Exile } else { Zone::Graveyard };
                        let new = move_batch(cx, &[Move::new(o, to)])?;
                        // `RememberCountered$` (Forge `CounterEffect`: the countered spell's card).
                        if ad.remember {
                            ex.remembered.extend(new.into_iter().flatten());
                        }
                    }
                } else if let Some(i) = cx.s.stack.iter().rposition(|it| it.source == o && it.kind != StackKind::Spell && by_item.is_none_or(|u| it.uid == u)) {
                    // Countering an ability (ward): it's removed from the stack (CR 701.6b).
                    let it = cx.s.stack.remove(i);
                    cx.s.emit(Event::Countered { card: it.source_card });
                    if ad.remember {
                        ex.remembered.push(o);
                    }
                }
            }
        }
        Effect::Discard { n, mode, valid } => {
            let k = num(cx, ex, n);
            // Several players discard at once ("any number of target players each discard a
            // card", Ral Zarek, Guest Lecturer): each chooses (in the order given), then all the
            // cards are discarded in one event (CR 101.4, 608.2c; Forge `DiscardEffect` discards
            // its `discardedMap` together).
            let mut chosen_all: SmallVec<[(PlayerId, ObjId); 4]> = SmallVec::new();
            for p in players(&who) {
                let chooser = if *mode == DiscardMode::RevealYouChoose { ex.you } else { p };
                if *mode == DiscardMode::RevealYouChoose {
                    for &h in &cx.s.player(p).hand.clone() {
                        cx.s.obj_mut(h).known_to |= 1 << ex.you.0;
                    }
                }
                let mut chosen: SmallVec<[ObjId; 4]> = SmallVec::new();
                if *mode == DiscardMode::Hand {
                    chosen.extend(cx.s.player(p).hand.iter().copied());
                } else if *mode == DiscardMode::Random {
                    let mut hand: Vec<ObjId> = cx.s.player(p).hand.clone();
                    for _ in 0..(k.max(0) as usize).min(hand.len()) {
                        let i = cx.s.rng.below(hand.len() as u32) as usize;
                        chosen.push(hand.remove(i));
                    }
                } else {
                    let fcx = exec_fcx!(ex);
                    let items: Vec<(crate::query::ObjSig, Opt)> = crate::flow::hand_items(cx, p)
                        .into_iter()
                        .filter(|(_, o)| match (o, valid) {
                            (Opt::Card(h), Some(f)) => obj_matches(cx.s, cx.db, f, *h, fcx),
                            _ => true,
                        })
                        .collect();
                    let n = (k.max(0) as usize).min(items.len());
                    for c in cx.choose_set(chooser, DecisionKind::Discard, items, n, n, Some(ex.source))? {
                        if let Opt::Card(h) = c {
                            chosen.push(h);
                        }
                    }
                }
                chosen_all.extend(chosen.into_iter().map(|h| (p, h)));
            }
            for &(p, h) in &chosen_all {
                let card = cx.s.obj(h).card;
                cx.s.emit(Event::Discard { player: p, card });
            }
            if !players(&who).is_empty() {
                let moves: Vec<Move> = chosen_all.iter().map(|&(_, h)| Move::new(h, Zone::Graveyard)).collect();
                let new = move_batch(cx, &moves)?;
                if ad.remember {
                    ex.remembered.extend(new.into_iter().flatten());
                }
            }
        }
        Effect::Sacrifice { valid, n, up_to } => {
            let k = num(cx, ex, n);
            // Several players sacrifice at once (CR 101.4): each chooses (in the order given),
            // then all the permanents are sacrificed in one event (look-back, CR 603.10a).
            let mut chosen_all: SmallVec<[(PlayerId, ObjId); 2]> = SmallVec::new();
            for p in players(&who) {
                let fcx = FilterCtx { you: p, x: 0, ..exec_fcx!(ex) };
                let objs: Vec<ObjId> = cx.s.battlefield.iter().copied().filter(|&o| cx.s.obj(o).controller == p && obj_matches(cx.s, cx.db, valid, o, fcx)).collect();
                let items = crate::query::sig_items(cx.s, cx.db, &objs);
                let n = (k.max(0) as usize).min(items.len());
                // "Any number": from none up to all of them (`up_to`).
                let min = if *up_to { 0 } else { n };
                let chosen: SmallVec<[ObjId; 2]> = cx
                    .choose_set(p, DecisionKind::Sacrifice, items, min, n, Some(ex.source))?
                    .into_iter()
                    .filter_map(|c| if let Opt::Card(o) = c { Some(o) } else { None })
                    .collect();
                chosen_all.extend(chosen.into_iter().map(|o| (p, o)));
            }
            for p in players(&who) {
                let mine: SmallVec<[ObjId; 2]> = chosen_all.iter().filter(|c| c.0 == p).map(|c| c.1).collect();
                crate::trig_ext::fire_sacrificed(cx, &mine, p);
            }
            let moves: Vec<Move> = chosen_all.iter().map(|&(_, o)| Move::new(o, Zone::Graveyard)).collect();
            let new = move_batch(cx, &moves)?;
            if ad.remember {
                ex.remembered.extend(new.into_iter().flatten());
            }
        }
        Effect::PutCounter { kind, n } => {
            let k = num(cx, ex, n);
            for o in objs(&who) {
                if matches!(cx.s.obj(o).zone, Zone::Battlefield) {
                    crate::ext6::put_counters(cx.s, cx.db, o, *kind, k);
                }
            }
        }
        Effect::ChangeZone { origin, dest, hidden, change_type, num: cnt, tapped, shuffle, searcher, .. } => {
            let mut new_ids: SmallVec<[ObjId; 4]> = SmallVec::new();
            if *hidden {
                let k = num(cx, ex, cnt);
                let Some(&p) = affected_players(cx, ex, Some(*searcher)).first() else { return Ok(()) };
                let filter = change_type.clone().unwrap();
                let mut chosen: SmallVec<[ObjId; 2]> = SmallVec::new();
                // Searching = looking through the zone: the searcher learns those cards (the
                // library knowledge is forgotten by the shuffle afterwards).
                if origin.has(Zone::Library) {
                    for o in cx.s.player(p).library.clone() {
                        cx.s.obj_mut(o).known_to |= 1 << p.0;
                    }
                }
                let pool = hidden_change_pool(cx, ex, n, p, *origin, &filter);
                let items = crate::query::card_items(cx.s, cx.db, &pool);
                // CR 701.23b: a search for cards with a stated quality may fail to find (min 0).
                // 701.23d: a search for "a card", or a mandatory choice from a public zone,
                // takes as many as possible (`search_all`).
                let k = k.max(0) as usize;
                let min = if cx.db.search_all.contains(&n) { k.min(items.len()) } else { 0 };
                let picked = match cx.db.total_mv.iter().find(|e| e.0 == n) {
                    Some(&(_, budget)) => choose_within_total_mv(cx, p, items, k, budget, ex.source)?,
                    None => cx.choose_set(p, DecisionKind::Search, items, min, k, Some(ex.source))?,
                };
                for c in picked {
                    if let Opt::Card(o) = c {
                        chosen.push(o);
                    }
                }
                // An Aura put onto the battlefield this way: its owner chooses what it enchants
                // (CR 303.4f); with nothing legal it stays where it is (303.4g).
                if *dest == Zone::Battlefield {
                    let auras: SmallVec<[ObjId; 1]> = chosen.iter().copied().filter(|&o| cx.db.get(cx.s.obj(o).card).front().enchant.is_some()).collect();
                    for &o in &auras {
                        if let Some(nid) = crate::effect_ext::aura_onto_battlefield(cx, o)? {
                            if *tapped {
                                cx.s.obj_mut(nid).tapped = true;
                            }
                            new_ids.push(nid);
                        }
                    }
                    chosen.retain(|o| !auras.contains(o));
                }
                // CR 110.2a: under the control of the player who puts it there. (`hidden_all_owners`
                // never reaches the battlefield: compile refuses that form, see `compile.rs`.)
                let moves: Vec<Move> = chosen.iter().map(|&o| Move { obj: o, to: *dest, to_top: true, tapped: *tapped, controller: Some(p) }).collect();
                new_ids.extend(move_batch(cx, &moves)?.into_iter().flatten());
                // "... and reveal it" (`ext9`): known to both players wherever it went.
                if !cx.db.x9.reveal_found.is_empty() && cx.db.x9.reveal_found.contains(&n) {
                    for &o in &new_ids {
                        cx.s.obj_mut(o).known_to = 0b11;
                    }
                }
                if origin.has(Zone::Library) || *shuffle {
                    crate::flow::shuffle_library(cx.s, p);
                }
                if origin.has(Zone::Library) {
                    // CR 701.19: the player searched their library.
                    cx.s.ext.searched |= 1 << p.0;
                    crate::triggers::fire(cx.s, cx.db, &crate::triggers::TrigEvent::Ext(crate::trig_ext::ExtEvent::Searched { player: p }));
                }
            } else {
                // "Under your control" (`CardDb::bf_under_you`, earthbend); otherwise the owner's.
                let under_you = (*dest == Zone::Battlefield && cx.db.bf_under_you.contains(&n)).then_some(ex.you);
                let mut moves = vec![];
                let mut movers = objs(&who);
                // "Shuffle this creature and target creature ...": the source moves with its
                // targets, in one event (`x9.this_and_targets`; CR 608.2c, 603.10a).
                if !cx.db.x9.this_and_targets.is_empty() && cx.db.x9.this_and_targets.contains(&n) {
                    let me = ex.replaced.unwrap_or(ex.source);
                    if !movers.contains(&me) {
                        movers.insert(0, me);
                    }
                }
                for o in movers {
                    if !origin.has(cx.s.obj(o).zone) {
                        continue;
                    }
                    if *dest == Zone::Battlefield && cx.s.obj(o).zone != Zone::Stack && cx.db.get(cx.s.obj(o).card).front().enchant.is_some() {
                        // An Aura returned to the battlefield (The Mind Stone, Daydream-like).
                        if let Some(nid) = crate::effect_ext::aura_onto_battlefield(cx, o)? {
                            new_ids.push(nid);
                        }
                        continue;
                    }
                    moves.push(Move { obj: o, to: *dest, to_top: true, tapped: *tapped, controller: under_you });
                }
                let owners: SmallVec<[PlayerId; 2]> = moves.iter().map(|m| cx.s.obj(m.obj).owner).collect();
                new_ids.extend(move_batch(cx, &moves)?.into_iter().flatten());
                // `ForgetChanged$` (`ext9`): the moved objects leave the remembered set.
                if !cx.db.x9.forget_changed.is_empty() && cx.db.x9.forget_changed.contains(&n) {
                    ex.remembered.retain(|r| !moves.iter().any(|m| m.obj == *r));
                }
                if let Some(&(_, pos)) = cx.db.library_position.iter().find(|e| e.0 == n) {
                    // Nth from the top (moved to the top above; the top is the end of the vec).
                    for &o in &new_ids {
                        if cx.s.obj(o).zone == Zone::Library {
                            let lib = &mut cx.s.player_mut(cx.s.obj(o).owner).library;
                            lib.retain(|&x| x != o);
                            let at = lib.len().saturating_sub(pos as usize);
                            lib.insert(at, o);
                        }
                    }
                }
                // "Shuffle ... into their owners' libraries" (CR 701.24).
                if *shuffle && *dest == Zone::Library {
                    for p in [PlayerId(0), PlayerId(1)] {
                        if owners.contains(&p) {
                            crate::flow::shuffle_library(cx.s, p);
                        }
                    }
                }
            }
            ex.moved = new_ids.iter().copied().collect();
            crate::effect_ext::link_exiled(cx, ex, *dest);
            if ad.remember {
                ex.remembered.extend(new_ids);
            }
        }
        Effect::Token { token, n, tapped } => {
            let k = num(cx, ex, n);
            let ids = create_tokens(cx, *token, ex.you, k, *tapped)?;
            ex.moved = ids.iter().copied().collect();
            if ad.remember {
                ex.remembered.extend(ids);
            }
        }
        Effect::Charm { .. } => {} // modes were expanded into the node list
        Effect::Dig { n, change, dest, rest, valid, up_to, reveal, rest_random } => {
            let k = num(cx, ex, n).max(0) as usize;
            // `Expr::Num(num::ALL)` is "all of them" (`ChangeNum$ All` / `AnyNumber$`), not a
            // game number: only a real amount is bounded.
            let want = match change {
                crate::expr::Expr::Num(crate::num::ALL) => crate::num::ALL as usize,
                e => num(cx, ex, e).max(0) as usize,
            };
            for p in players(&who) {
                let lib = &cx.s.player(p).library;
                let top: Vec<ObjId> = lib.iter().rev().take(k).copied().collect();
                for &o in &top {
                    cx.s.obj_mut(o).known_to |= 1 << p.0; // the player looks at them
                }
                cx.s.forget_arranged(p, &top);
                let fcx = exec_fcx!(ex);
                let pool: Vec<ObjId> = top.iter().copied().filter(|&o| valid.as_ref().is_none_or(|f| obj_matches(cx.s, cx.db, f, o, fcx))).collect();
                let items = crate::query::card_items(cx.s, cx.db, &pool);
                let m = want.min(items.len());
                let min = if *up_to { 0 } else { m };
                let chosen: Vec<ObjId> = cx.choose_set(ex.you, DecisionKind::Dig, items, min, m, Some(ex.source))?.into_iter().filter_map(|o| if let Opt::Card(x) = o { Some(x) } else { None }).collect();
                if *reveal {
                    for &o in &chosen {
                        cx.s.obj_mut(o).known_to = 0b11;
                    }
                }
                // CR 303.4f–g: an Aura put onto the battlefield this way enters attached to
                // something its owner chooses (not targeted; not an object entering at the same
                // time, so it enters first); with nothing to enchant it stays in the library.
                let mut aura_new: Vec<Option<ObjId>> = vec![];
                let mut chosen_rest: Vec<ObjId> = chosen.clone();
                if *dest == Zone::Battlefield && chosen.iter().any(|&o| cx.db.get(cx.s.obj(o).card).front().enchant.is_some()) {
                    chosen_rest.clear();
                    for &o in &chosen {
                        if cx.db.get(cx.s.obj(o).card).front().enchant.is_some() {
                            if let Some(nid) = crate::effect_ext::aura_onto_battlefield(cx, o)? {
                                aura_new.push(Some(nid));
                            }
                        } else {
                            chosen_rest.push(o);
                        }
                    }
                }
                let n_chosen = aura_new.len() + chosen_rest.len();
                let mut moves: Vec<Move> = chosen_rest.iter().map(|&o| Move { obj: o, to: *dest, to_top: true, tapped: false, controller: Some(ex.you) }).collect();
                let mut others: Vec<ObjId> = top.iter().copied().filter(|o| !chosen.contains(o)).collect();
                if *rest == Zone::Library && !cx.db.lt.dig_rest_top.is_empty() && cx.db.lt.dig_rest_top.contains(&node_id) {
                    crate::ext3::dig_rest_on_top(cx, p, &others, *reveal)?;
                } else if *rest == Zone::Library {
                    // Bottom of the library. "Any order" is treated as a non-decision (canonical
                    // order); "random order" shuffles them.
                    if *rest_random {
                        cx.s.rng.shuffle(&mut others);
                    }
                    let lib = &mut cx.s.player_mut(p).library;
                    lib.retain(|o| !others.contains(o));
                    for &o in &others {
                        lib.insert(0, o);
                    }
                } else {
                    moves.extend(others.iter().map(|&o| Move::new(o, *rest)));
                }
                // One looked-at card (the top one) put into the hand: everyone sees the top card
                // go there, so whoever knew it still does (`State::seen_into_hand`). Two
                // or more: a private pick (`forget_arranged` above cleared the others' knowledge).
                let seen_one = (top.len() == 1 && *dest == Zone::Hand && chosen_rest.len() == 1).then(|| cx.s.obj(moves[0].obj).known_to);
                let new: Vec<Option<ObjId>> = aura_new.into_iter().chain(move_batch(cx, &moves)?).collect();
                if let (Some(seen), Some(Some(nid))) = (seen_one, new.first()) {
                    cx.s.seen_into_hand(*nid, seen);
                }
                if *reveal {
                    // Revealed cards stay known to everyone in their new zone.
                    for n in new.iter().take(n_chosen).flatten() {
                        cx.s.obj_mut(*n).known_to = 0b11;
                    }
                }
                if !cx.db.mr.dig_imprint.is_empty() && cx.db.mr.dig_imprint.contains(&node_id) {
                    ex.imprinted.extend(new.iter().take(n_chosen).flatten().copied());
                }
                if ad.remember {
                    ex.remembered.extend(new.into_iter().flatten());
                }
            }
        }
        Effect::ChangeZoneAll { origin, dest, change_type, shuffle } => {
            // `IsRemembered` (Chandra, Chill of Compliance: "if you put a noncreature, nonland card
            // into your graveyard this way") matches what earlier nodes remembered.
            let rem = ex.remembered.clone();
            let fcx = FilterCtx { rem: &rem, ..exec_fcx!(ex) };
            let mut objs_all: Vec<ObjId> = vec![];
            if origin.has(Zone::Battlefield) {
                objs_all.extend(cx.s.battlefield.iter().copied());
            }
            for pl in [ex.you, ex.you.opp()] {
                let pp = cx.s.player(pl);
                if origin.has(Zone::Graveyard) {
                    objs_all.extend(pp.graveyard.iter().copied());
                }
                if origin.has(Zone::Hand) {
                    objs_all.extend(pp.hand.iter().copied());
                }
                if origin.has(Zone::Library) {
                    objs_all.extend(pp.library.iter().copied());
                }
            }
            if origin.has(Zone::Exile) {
                objs_all.extend(cx.s.exile.iter().copied());
            }
            let moves: Vec<Move> = objs_all.into_iter().filter(|&o| obj_matches(cx.s, cx.db, change_type, o, fcx)).map(|o| Move::new(o, *dest)).collect();
            let new = move_batch(cx, &moves)?;
            ex.moved = new.iter().flatten().copied().collect();
            crate::effect_ext::link_exiled(cx, ex, *dest);
            if ad.remember {
                ex.remembered.extend(new.into_iter().flatten());
            }
            if *shuffle {
                crate::flow::shuffle_library(cx.s, ex.you);
            }
        }
        Effect::DamageAll { amount, cards, players: ps } => {
            let dmg = num(cx, ex, amount);
            let fcx = exec_fcx!(ex);
            let mut ts: Vec<Target> = vec![];
            if let Some(f) = cards {
                ts.extend(cx.s.battlefield.iter().copied().filter(|&o| obj_matches(cx.s, cx.db, f, o, fcx)).map(Target::Obj));
            }
            if let Some(sel) = ps {
                ts.extend([ex.you, ex.you.opp()].into_iter().filter(|&p| sel.matches(p, ex.you)).map(Target::Player));
            }
            deal_damage_each(cx, ex.source, &ts, dmg)?;
        }
        Effect::PutCounterAll { valid, kind, n } => {
            let k = num(cx, ex, n);
            let fcx = exec_fcx!(ex);
            let v: Vec<ObjId> = cx.s.battlefield.iter().copied().filter(|&o| obj_matches(cx.s, cx.db, valid, o, fcx)).collect();
            for o in v {
                crate::ext6::put_counters(cx.s, cx.db, o, *kind, k);
            }
        }
        Effect::Explore => {
            // CR 701.44a: reveal the top card; land → hand; otherwise +1/+1 counter and the
            // controller may put the card into the graveyard.
            for o in objs(&who) {
                let p = cx.s.obj(o).controller;
                let Some(top) = cx.s.library_top(p) else {
                    if cx.s.obj(o).zone == Zone::Battlefield {
                        crate::ext6::put_counters(cx.s, cx.db, o, CounterKind::P1P1, 1);
                    }
                    continue;
                };
                cx.s.obj_mut(top).known_to = 0b11;
                if cx.db.get(cx.s.obj(top).card).front().types.has(CardType::Land) {
                    move_batch(cx, &[Move::new(top, Zone::Hand)])?;
                    let nid = *cx.s.player(p).hand.last().unwrap();
                    cx.s.obj_mut(nid).known_to = 0b11;
                } else {
                    if cx.s.obj(o).zone == Zone::Battlefield {
                        crate::ext6::put_counters(cx.s, cx.db, o, CounterKind::P1P1, 1);
                    }
                    if cx.choose(Decision { player: p, kind: DecisionKind::Surveil, options: vec![Opt::ScryTop, Opt::ToGraveyard], subject: Some(top) })? == Opt::ToGraveyard {
                        move_batch(cx, &[Move::new(top, Zone::Graveyard)])?;
                    }
                }
            }
        }
        Effect::Animate { power, toughness, types, subtypes, remove_card_types, remove_creature_types, colors, overwrite_colors, kws, abilities, duration } => {
            let pw = power.as_ref().map(|e| num(cx, ex, e));
            let th = toughness.as_ref().map(|e| num(cx, ex, e));
            let (eot, until) = match duration {
                Duration::EndOfTurn => (true, None),
                Duration::Permanent => (false, None),
                Duration::UntilYourNextTurn => (false, Some(ex.you)),
            };
            for o in objs(&who) {
                if cx.s.obj(o).zone != Zone::Battlefield {
                    continue;
                }
                let ts = cx.s.ts();
                let set_pt = match (pw, th) {
                    (Some(p), Some(t)) => Some((p, t)),
                    (None, None) => None,
                    // Only base power (or toughness) changes (layer 7b, CR 613.4b).
                    (Some(p), None) => Some((p, i32::MIN)),
                    (None, Some(t)) => Some((i32::MIN, t)),
                };
                cx.s.effects.push(ContEffect {
                    ts,
                    affected: o,
                    layer: Layer::L6,
                    kws: *kws,
                    until_eot: eot,
                    until_turn_of: until,
                    add_types: *types,
                    remove_card_types: *remove_card_types,
                    add_subtypes: subtypes.iter().copied().collect(),
                    remove_creature_types: *remove_creature_types,
                    colors: *colors,
                    overwrite_colors: *overwrite_colors,
                    abilities: abilities.iter().copied().collect(),
                    set_pt,
                    ..Default::default()
                });
            }
            // What was animated (for gained triggered abilities, `ext2::GrantTriggers`).
            ex.moved = objs(&who).into_iter().filter(|&o| cx.s.obj(o).zone == Zone::Battlefield).collect();
        }
        Effect::CreateEffect { statics, remember, duration, forget_on_moved, exile_on_moved } => {
            let remembered: SmallVec<[ObjId; 2]> = match remember {
                Some(Defined::Remembered) => ex.remembered.iter().copied().collect(),
                Some(Defined::Targeted) => objs(&ex.last_targets).into_iter().collect(),
                Some(d) => {
                    let fake = AbilityDef { defined: Some(*d), target: None, ..ad.clone() };
                    objs(&affected(cx, ex, &fake, &[], false)).into_iter().collect()
                }
                None => SmallVec::new(),
            };
            if remember.is_some() && remembered.is_empty() {
                return Ok(()); // nothing to apply to
            }
            // CR 611.2b: "for as long as [this] remains on the battlefield" never starts if the
            // source already left (The Wondrous Wasp killed in response to its trigger). Forge
            // `checkValidDuration` (host in play or on the stack), XMage `WhileOnBattlefield`.
            if *duration == EffDuration::AsLongAsInPlay && !matches!(cx.s.obj(ex.source).zone, Zone::Battlefield | Zone::Stack) {
                return Ok(());
            }
            let ts = cx.s.ts();
            let created_turn = cx.s.turn.number;
            let had = !remembered.is_empty();
            cx.s.effect_objs.push(EffectObj {
                source: ex.source,
                controller: ex.you,
                statics: statics.iter().copied().collect(),
                remembered,
                duration: *duration,
                created_turn,
                forget_on_moved: *forget_on_moved,
                exile_on_moved: *exile_on_moved,
                had_remembered: had,
                triggers: SmallVec::new(),
                ts,
            });
        }
        Effect::Cleanup { clear_remembered } => {
            if *clear_remembered {
                ex.remembered.clear();
            }
        }
        Effect::Clone { choices, zone, optional, add_types, add_subtypes, kws, set_pt, keep_name, remember_origin } => {
            // CR 707 / 614.1c: "enter as a copy of" a chosen object. What can be copied is
            // decided before the event (614.12a: the entering object isn't on the battlefield).
            let t = ex.replaced.unwrap_or(ex.source);
            let fcx = FilterCtx { source: Some(t), ..exec_fcx!(ex) };
            let pool: Vec<ObjId> = match zone {
                Zone::Graveyard => cx.s.players.iter().flat_map(|p| p.graveyard.iter().copied()).filter(|&o| is_card(cx.s, o)).collect(),
                _ => cx.s.battlefield.clone(),
            };
            // A card whose copy ability already applied to this event (`copy_chain`, CR 614.5) is
            // not interchangeable with an identical one whose ability hasn't: copying the other
            // applies that one too (canonicalisation soundness test).
            let objs: Vec<ObjId> = pool.into_iter().filter(|&o| o != t && obj_matches(cx.s, cx.db, choices, o, fcx)).collect();
            let sigs = crate::query::frozen_if_big(cx.s, cx.db, || crate::query::obj_sigs(cx.s, cx.db, &objs));
            let items: Vec<((bool, crate::query::ObjSig), Opt)> = objs.into_iter().zip(sigs).map(|(o, k)| ((ex.copy_chain.contains(&o), k), Opt::Card(o))).collect();
            let min = if *optional { 0 } else { 1.min(items.len()) };
            let Some(Opt::Card(c)) = cx.choose_set(ex.you, DecisionKind::CopyChoice, items, min, 1, Some(t))?.into_iter().next() else { return Ok(()) };
            let (ccard, cface) = (cx.s.obj(c).card, cx.s.obj(c).face);
            let prev = cx.s.copy_effects.iter().find(|ce| ce.obj == c).cloned();
            // Its own card, also when an earlier copy effect already applied as it enters.
            let printed = cx.s.copy_effects.iter().find(|x| x.obj == t).map_or(cx.s.obj(t).card, |x| x.printed);
            let mut ce = CopyEffect { obj: t, printed, name: prev.as_ref().and_then(|p| p.name), add_types: *add_types, add_subtypes: add_subtypes.iter().copied().collect(), kws: *kws, set_pt: *set_pt, transformed: false, add_super: 0 };
            if let Some(p) = prev {
                ce.inherit_exceptions(&p);
            }
            if *keep_name {
                ce.name = Some(printed);
            }
            cx.s.copy_effects.retain(|x| x.obj != t);
            cx.s.copy_effects.push(ce.clone());
            let o = cx.s.obj_mut(t);
            o.card = ccard;
            o.face = cface;
            cx.s.emit(Event::BecameCopy { obj: t, card: printed, copied: ccard });
            if *remember_origin {
                ex.remembered.push(c);
            }
            // CR 614.12: the copied card's own "enters with"/"as enters" replacements apply to
            // the entering copy (707.5). That includes a copied "enter as a copy" ability: the
            // permanent now has it and it hasn't applied to this event yet (CR 707.9e example:
            // Altered Ego entering as a copy of an uncopied Clone applies the copy ability it
            // gained; 616.1c/f: copy effects first, then the rest). Each object's copied copy
            // ability applies at most once (614.5), so chains end.
            let cf = cx.db.get(ccard).face(cface);
            let copy_mods: Vec<AbilityId> = cf.replacements.iter().filter_map(|r| match (&r.action, r.zones) {
                (ReplAction::Modify(a), None) if r.dest.has(Zone::Battlefield) && matches!(cx.db.ability(*a).effect, Effect::Clone { .. }) => Some(*a),
                _ => None,
            }).collect();
            if !copy_mods.is_empty() && !ex.copy_chain.contains(&c) {
                for a in copy_mods {
                    let mut ex2 = Exec::new(ex.you, t, ccard);
                    ex2.replaced = Some(t);
                    ex2.copy_chain = ex.copy_chain.clone();
                    ex2.copy_chain.push(c);
                    run_chain(cx, &mut ex2, a)?;
                }
                // It became a copy of something else: that copy's replacements applied there.
                if cx.s.obj(t).card != ccard || cx.s.copy_effects.iter().find(|x| x.obj == t) != Some(&ce) {
                    return Ok(());
                }
            }
            if let (Some(l), true) = (cf.loyalty, cf.types.has(CardType::Planeswalker)) {
                crate::ext6::put_counters(cx.s, cx.db, t, CounterKind::Loyalty, l);
            }
            for &(k, n) in &cf.etb_counters {
                crate::ext6::put_counters(cx.s, cx.db, t, k, n);
            }
            // The copied "enters with X counters" uses the X of the spell that became this
            // permanent (CR 107.3m; 0 if none: a Clone put onto the battlefield): Mockingbird cast
            // with X=1 as a Goldvein Hydra enters with one counter (ruling 2024-07-26: copied
            // "enters with" abilities work). Likewise "enters with a counter for
            // each ..." (`zones::as_enters` for a permanent's own face).
            if !cf.extra.etb_counters_x.is_empty() {
                let x = cx.s.ext.x_paid.iter().find(|x| x.0 == t).map_or(0, |x| x.1);
                for &k in &cf.extra.etb_counters_x {
                    crate::ext6::put_counters(cx.s, cx.db, t, k, x);
                }
            }
            for (k, e) in &cf.extra.etb_counters_expr {
                let ecx = crate::expr::EvalCtx { s: cx.s, db: cx.db, you: cx.s.obj(t).controller, source: t, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &[], imprinted: &[], base_only: false };
                let n = crate::expr::eval(e, &ecx).max(0);
                if n > 0 {
                    crate::ext6::put_counters(cx.s, cx.db, t, *k, n);
                }
            }
            // CR 714.3a: a Saga's intrinsic "enters with a lore counter" is a replacement effect,
            // so it applies to an object entering as a copy of a Saga (614.12); its chapter I
            // then triggers on entering (triggers.rs). No pool Saga has read ahead (714.3b).
            if crate::query::final_chapter(cx.db, ccard) > 0 {
                let lore = cx.db.counter_names.iter().position(|n| n == "LORE").expect("LORE interned by compile");
                crate::ext6::put_counters(cx.s, cx.db, t, CounterKind::Named(lore as u16), 1);
            }
            let mods: Vec<AbilityId> = cf.replacements.iter().filter_map(|r| match (&r.action, r.zones) {
                (ReplAction::Modify(a), None) if r.dest.has(Zone::Battlefield) && !matches!(cx.db.ability(*a).effect, Effect::Clone { .. }) => Some(*a),
                _ => None,
            }).collect();
            for a in mods {
                let mut ex2 = Exec::new(ex.you, t, ccard);
                ex2.replaced = Some(t);
                run_chain(cx, &mut ex2, a)?;
            }
        }
        Effect::ReturnTransformed => {
            // CR 702.167a: the card exiled to pay the craft cost returns transformed.
            if let Some(o) = ex.trig.new_obj {
                if cx.s.obj(o).zone == Zone::Exile {
                    let owner = cx.s.obj(o).owner;
                    if let Some(nid) = crate::zones::move_special(cx, o, owner, None, true)? {
                        let (card, front) = (cx.s.obj(nid).card, cx.s.obj(o).card);
                        cx.s.emit(Event::Transformed { obj: nid, card: front, face_card: card });
                        if let Some(i) = cx.s.ext.x6.craft_pending.iter().position(|x| x.0 == o) {
                            let (_, mats) = cx.s.ext.x6.craft_pending.remove(i);
                            cx.s.ext.x6.crafted.push((nid, mats));
                        }
                    }
                }
            }
        }
        Effect::CopyPermanent => {
            let mut made: SmallVec<[ObjId; 2]> = SmallVec::new();
            for o in objs(&who) {
                // A permanent that left the battlefield is copied as it last existed there (CR
                // 608.2h; 111.12: "This does not apply to an effect that would use the last
                // known information of an object"), e.g. "when this dies, create a token
                // that's a copy of it".
                if cx.s.obj(o).zone == Zone::Battlefield || crate::lki::of(cx.s, o).is_some() {
                    // Every token created, doubled ones included (CR 614.1a).
                    // "Except it's legendary" (Adagia): part of the copy effect from the start.
                    let add_super = if cx.db.le.copy_legendary.contains(&node_id) { Supertypes::LEGENDARY } else { 0 };
                    let ids = crate::zones::create_token_copy_except(cx, o, ex.you, crate::zones::CopyExc { add_super, ..Default::default() })?;
                    if ad.remember {
                        ex.remembered.extend(ids.iter().copied());
                    }
                    made.extend(ids);
                }
            }
            ex.moved = made;
        }
        Effect::Attach => {
            // CR 701.3: attach the source to the target; no move if it can't legally be
            // attached (701.3b), nothing if already attached; a new timestamp otherwise (701.3c).
            let src = ex.source;
            for o in objs(&who) {
                if can_attach(cx.s, cx.db, src, o) && cx.s.obj(src).attached_to != Some(o) {
                    let ts = cx.s.ts();
                    let so = cx.s.obj_mut(src);
                    so.attached_to = Some(o);
                    so.ts = ts;
                    let (card, to) = (so.card, o);
                    cx.s.emit(Event::Attached { card, obj: src, to });
                }
                // `RememberAttached$` (Forge `AttachEffect`): the attachment, if it is now
                // attached to that object ("When you do", Novel Nunchaku).
                if ad.remember && cx.s.obj(src).attached_to == Some(o) {
                    ex.remembered.push(src);
                }
            }
        }
        Effect::AuraEnter => {
            // CR 608.3/303.4a: the Aura spell becomes a permanent attached to its target.
            let src = ex.source;
            if let Some(&t) = objs(&who).first() {
                if cx.s.obj(src).zone == Zone::Stack {
                    if let Some(nid) = crate::zones::move_attached(cx, src, ex.you, t)? {
                        let card = cx.s.obj(nid).card;
                        cx.s.emit(Event::Attached { card, obj: nid, to: t });
                    }
                }
            }
        }
        Effect::SetPrepared(on) => {
            // CR 722.3a: only a permanent with a prepare spell can become prepared.
            for o in objs(&who) {
                if cx.s.obj(o).zone == Zone::Battlefield && cx.db.get(cx.s.obj(o).card).layout == Layout::Prepare {
                    cx.s.obj_mut(o).prepared = *on;
                }
            }
        }
        Effect::NinjutsuEnter => {
            // CR 702.49a/c: from the hand onto the battlefield tapped and attacking the same
            // player or planeswalker as the returned creature (no attack triggers, 508.3a).
            let o = ex.source;
            if cx.s.obj(o).zone == Zone::Hand && cx.s.obj(o).owner == ex.you {
                if let Some(nid) = move_batch(cx, &[Move { obj: o, to: Zone::Battlefield, to_top: false, tapped: true, controller: Some(ex.you) }])?[0] {
                    let def = ex.trig.player.unwrap_or(ex.you.opp());
                    if cx.s.turn.step <= Step::EndCombat && cx.s.turn.step >= Step::DeclareAttackers {
                        cx.s.combat.attackers.push((nid, def));
                        if let Some(pw) = ex.trig.obj {
                            cx.s.combat.attacking_pw.push((nid, pw));
                        }
                    }
                }
            }
        }
        Effect::Mana { .. } => return cx.unsupported("mana ability on the stack"),
        Effect::Ext(x) => return crate::effect_ext::apply(cx, ex, &ad, x, tg, &who),
        Effect::Alt(a) => crate::altcast::apply(cx, ex.you, ex.source, a, &ex.remembered)?,
    }
    Ok(())
}

/// Choose up to `k` of `items` (keyed by `query::card_sig`) with total mana value at most `budget`: one at a
/// time in canonical order (identical cards are one option, each pick after the previous), an
/// item offered only if it fits; `Done` any time. Each legal set is reachable exactly once.
fn choose_within_total_mv(cx: &mut Ctx, p: PlayerId, mut items: Vec<(crate::query::ObjSig, Opt)>, k: usize, budget: i32, subject: ObjId) -> R<Vec<Opt>> {
    items.sort_by_key(|x| (x.0, opt_key(&x.1)));
    let mv = |cx: &Ctx, o: &Opt| if let Opt::Card(x) = o { chars(cx.s, cx.db, *x).mv as i32 } else { 0 };
    let (mut chosen, mut next, mut total) = (vec![], 0usize, 0i32);
    while chosen.len() < k {
        let mut opts = vec![Opt::Done];
        let mut pos = vec![usize::MAX];
        let mut last = None;
        for (i, (key, o)) in items.iter().enumerate().skip(next) {
            if last == Some(*key) || total + mv(cx, o) > budget {
                continue;
            }
            last = Some(*key);
            opts.push(*o);
            pos.push(i);
        }
        let c = cx.choose(Decision { player: p, kind: DecisionKind::Search, options: opts.clone(), subject: Some(subject) })?;
        let i = pos[cx.offered_ix(c, &opts)];
        if i == usize::MAX {
            break;
        }
        cx.swap_alt(c, i, &mut items, |x| x.1 == c)?;
        total += mv(cx, &c);
        chosen.push(c);
        next = i + 1;
    }
    Ok(chosen)
}

/// An effect's amount (CR 107.1), evaluated as it resolves; past the game-number bound the unit
/// fails loudly (`num`).
pub(crate) fn amount(cx: &mut Ctx, ex: &Exec, e: &crate::expr::Expr) -> i32 {
    let v = eval(e, &ex.eval_ctx(cx.s, cx.db));
    cx.s.num_check(v as i64, "effect amount")
}

pub(crate) fn pump(s: &mut State, you: PlayerId, o: ObjId, p: i32, t: i32, kws: KwSet, d: Duration) {
    if s.obj(o).zone != Zone::Battlefield {
        return;
    }
    let ts = s.ts();
    let (eot, until) = match d {
        Duration::EndOfTurn => (true, None),
        Duration::Permanent => (false, None),
        Duration::UntilYourNextTurn => (false, Some(you)),
    };
    if p != 0 || t != 0 {
        let (p, t) = (s.num_check(p as i64, "power modification"), s.num_check(t as i64, "toughness modification"));
        s.effects.push(ContEffect { ts, affected: o, layer: Layer::L7c, pt: (p, t), until_eot: eot, until_turn_of: until, ..Default::default() });
    }
    if kws.0 != 0 {
        s.effects.push(ContEffect { ts, affected: o, layer: Layer::L6, kws, until_eot: eot, until_turn_of: until, ..Default::default() });
    }
}

/// Returns the damage dealt (after replacements). Noncombat damage is one event per call; combat
/// damage events are batched by `combat::damage_step` (CR 510.2).
pub fn deal_damage(cx: &mut Ctx, source: ObjId, to: Target, n: i32, combat: bool) -> R<i32> {
    deal_damage_in(cx, source, to, n, combat, &mut SmallVec::new())
}

/// The shield counter kind (CR 122.1c), if a loaded card uses shield counters.
pub fn shield_kind(db: &CardDb) -> Option<CounterKind> {
    db.counter_names.iter().position(|n| n == "SHIELD").map(|i| CounterKind::Named(i as u16))
}

/// Shield counters on a permanent that would be destroyed as the result of an effect (CR 122.1c:
/// "instead remove a shield counter from it"): removes one and says whether that replaced the
/// destruction. Not for state-based destruction (lethal damage, deathtouch: not an effect;
/// ruling 2025-06-06). Removing it isn't regeneration (ruling).
pub fn shield_replaces_destroy(cx: &mut Ctx, o: ObjId) -> bool {
    match shield_kind(cx.db) {
        Some(k) if cx.s.obj(o).counter(k) > 0 => {
            crate::ext6::put_counters(cx.s, cx.db, o, k, -1);
            true
        }
        _ => false,
    }
}

/// `deal_damage` as part of one damage event that may deal damage to the same permanent from
/// several sources (combat damage, CR 510.2): `shielded` collects the permanents whose shield
/// counter already applied to this event. CR 122.1c: "If damage would be dealt to this
/// permanent, prevent that damage and remove a shield counter from it", one prevention effect
/// however many shield counters: one counter per event, all its damage to the permanent prevented
/// (ruling 2025-06-06: "that damage is prevented and only one shield counter is removed"). If
/// damage can't be prevented (CR 615.12), the damage is dealt and the counter is still removed
/// (ruling 2025-06-06; XMage `ShieldCounterEffect`).
pub fn deal_damage_in(cx: &mut Ctx, source: ObjId, to: Target, n: i32, combat: bool, shielded: &mut SmallVec<[ObjId; 2]>) -> R<i32> {
    let n = if n > 0 { crate::static_ext::modified_damage(cx.s, cx.db, source, n) } else { n };
    let n = if n > 0 { crate::ext4::modified_damage(cx.s, cx.db, source, to, combat, n) } else { n };
    // Damage is dealt only to players and permanents (CR 120.1); a stack ability isn't one.
    if n <= 0 || matches!(to, Target::Ability { .. }) {
        return Ok(0);
    }
    let n = cx.s.num_check(n as i64, "damage dealt");
    if let (Target::Obj(o), Some(k)) = (to, shield_kind(cx.db)) {
        if cx.s.obj(o).zone == Zone::Battlefield && (shielded.contains(&o) || cx.s.obj(o).counter(k) > 0) {
            // The damage modifications above ("that much plus N") and this prevention give the
            // same result in either order (616.1); "heal all other damage instead" doesn't.
            if crate::static_ext::heals_before_damage(cx.s, cx.db, o) {
                return cx.unsupported("shield counter and a damage replacement of the same permanent: 616.1 order choice not implemented");
            }
            if !shielded.contains(&o) {
                shielded.push(o);
                crate::ext6::put_counters(cx.s, cx.db, o, k, -1);
            }
            if !crate::static_ext::damage_cant_be_prevented(cx.s, cx.db, combat) {
                return Ok(0);
            }
        }
    }
    let src_chars = chars(cx.s, cx.db, source);
    let src_card = cx.s.obj(source).card;
    let src_ctrl = cx.s.obj(source).controller;
    match to {
        Target::Player(p) => lose_life(cx.s, cx.db, p, n),
        Target::Ability { .. } => unreachable!("returned above"),
        Target::Obj(o) => {
            if cx.s.obj(o).zone != Zone::Battlefield {
                return Ok(0);
            }
            let c = chars(cx.s, cx.db, o);
            if c.types.has(CardType::Creature) {
                // Wolverine: "instead that damage is dealt, but all other damage already dealt to
                // him is healed" (combat damage heals once per step, see combat::damage_step).
                if !combat && crate::static_ext::heals_before_damage(cx.s, cx.db, o) {
                    cx.s.obj_mut(o).damage = 0;
                }
                let marked = cx.s.num_add(cx.s.obj(o).damage, n as i64, "damage marked");
                let ob = cx.s.obj_mut(o);
                ob.damage = marked;
                if src_chars.kws.has(Kw::Deathtouch) {
                    ob.deathtouched = true;
                }
            }
            if c.types.has(CardType::Planeswalker) {
                crate::ext6::put_counters(cx.s, cx.db, o, CounterKind::Loyalty, -n);
            }
        }
    }
    cx.s.emit(Event::Damage { source: src_card, to, amount: n });
    if let (true, Target::Player(p)) = (combat, to) {
        crate::hist::combat_damage(cx.s, p, n);
    }
    if src_chars.kws.has(Kw::Lifelink) {
        match cx.s.ext.lifelink_held.iter().position(|h| h.0 == source) {
            Some(i) => {
                let held = cx.s.num_add(cx.s.ext.lifelink_held[i].2, n as i64, "life gained");
                cx.s.ext.lifelink_held[i] = (source, Some(src_ctrl), held);
            }
            None => gain_life(cx.s, cx.db, src_ctrl, n, source),
        }
    }
    fire(cx.s, cx.db, &TrigEvent::Damage { source, target: to, amount: n, combat });
    if !combat {
        fire(cx.s, cx.db, &TrigEvent::Ext(crate::trig_ext::ExtEvent::DealtDamage { source, amount: n }));
    }
    Ok(n)
}

/// `sources` are about to deal damage to several recipients at the same time (combat damage,
/// CR 510.2; one effect damaging several objects/players): each one's lifelink gains are summed
/// into one life gain event, released by `release_lifelink` (CR 120.3f, 119.9: "whenever a source
/// causes [a player] to gain life"; 702.15e: separate events per source; Ajani's Pridemate ruling
/// 2024-11-08). A source dealing damage to one recipient gains as it deals it.
pub fn hold_lifelink(s: &mut State, sources: &[ObjId]) {
    debug_assert!(s.ext.lifelink_held.is_empty(), "nested lifelink hold");
    for &o in sources {
        if !s.ext.lifelink_held.iter().any(|h| h.0 == o) {
            s.ext.lifelink_held.push((o, None, 0));
        }
    }
}

/// The held lifelink gains (`hold_lifelink`): one life gain per source that dealt damage, in
/// the order the sources were held.
pub fn release_lifelink(s: &mut State, db: &CardDb) {
    for (src, p, n) in std::mem::take(&mut s.ext.lifelink_held) {
        if let Some(p) = p {
            gain_life(s, db, p, n, src);
        }
    }
}

/// `deal_damage` from `source` to each of `targets`: one simultaneous event (one lifelink gain
/// when there are several recipients, `hold_lifelink`).
pub fn deal_damage_each(cx: &mut Ctx, source: ObjId, targets: &[Target], n: i32) -> R<()> {
    let several = targets.len() > 1;
    if several {
        hold_lifelink(cx.s, &[source]);
    }
    for &t in targets {
        deal_damage(cx, source, t, n, false)?;
    }
    if several {
        release_lifelink(cx.s, cx.db);
    }
    Ok(())
}

/// `source` causes `p` to gain `n` life (CR 119.9: every life gain event has a source).
pub fn gain_life(s: &mut State, db: &CardDb, p: PlayerId, n: i32, source: ObjId) {
    if n <= 0 || crate::static_ext::cant_gain_life(s, db, p) {
        return;
    }
    let life = s.num_add(s.player(p).life, n as i64, "life total");
    s.player_mut(p).life = life;
    s.turn_stats.life_gained[p.idx()] = s.num_add(s.turn_stats.life_gained[p.idx()], n as i64, "life gained this turn");
    let life = s.player(p).life;
    s.emit(Event::LifeChange { player: p, delta: n, life });
    fire(s, db, &TrigEvent::LifeGained { player: p, amount: n, source });
}

pub fn lose_life(s: &mut State, db: &CardDb, p: PlayerId, n: i32) {
    if n <= 0 {
        return;
    }
    let life = s.num_add(s.player(p).life, -(n as i64), "life total");
    s.player_mut(p).life = life;
    s.turn_stats.life_lost[p.idx()] = s.num_add(s.turn_stats.life_lost[p.idx()], n as i64, "life lost this turn");
    // CR 702.179d: "Whenever one or more opponents lose life during your turn, if your speed is
    // less than 4, your speed increases by 1. This ability triggers only once each turn."
    // Approximation: applied immediately instead of using the stack.
    let a = s.turn.active;
    if a != p && (1..4).contains(&s.ext.speed[a.idx()]) && s.ext.speed_turn[a.idx()] != s.turn.number {
        s.ext.speed[a.idx()] += 1;
        s.ext.speed_turn[a.idx()] = s.turn.number;
    }
    let life = s.player(p).life;
    s.emit(Event::LifeChange { player: p, delta: -n, life });
    fire(s, db, &TrigEvent::Ext(crate::trig_ext::ExtEvent::LifeLost { player: p, amount: n }));
}

/// Destroy (CR 701.8): to graveyard unless indestructible (702.12b: it can't be destroyed, so a
/// shield counter isn't removed either) or a shield counter replaces it (CR 122.1c);
/// simultaneous for several objects, each destroyed (or shielded) once.
pub fn destroy_all(cx: &mut Ctx, v: &[ObjId]) -> R<()> {
    let mut moves = vec![];
    let mut seen: SmallVec<[ObjId; 4]> = SmallVec::new();
    for &o in v {
        if seen.contains(&o) {
            continue;
        }
        seen.push(o);
        if cx.s.obj(o).zone == Zone::Battlefield && !has_kw(cx.s, cx.db, o, Kw::Indestructible) && !shield_replaces_destroy(cx, o) {
            let card = cx.s.obj(o).card;
            cx.s.emit(Event::Destroyed { card });
            moves.push(Move::new(o, Zone::Graveyard));
        }
    }
    move_batch(cx, &moves)?;
    Ok(())
}

/// Scry N (CR 701.22) / Surveil N (CR 701.25): look at the top N; each goes to the bottom
/// (scry) or graveyard (surveil), or stays on top; then order the cards kept on top.
fn look_and_sort(cx: &mut Ctx, p: PlayerId, n: i32, surveil: bool) -> R<Vec<ObjId>> {
    let lib = &cx.s.player(p).library;
    let k = (n.max(0) as usize).min(lib.len());
    let top: Vec<ObjId> = lib[lib.len() - k..].iter().rev().copied().collect();
    for &o in &top {
        cx.s.obj_mut(o).known_to |= 1 << p.0;
    }
    cx.s.forget_arranged(p, &top);
    let (kind, away) = if surveil { (DecisionKind::Surveil, Opt::ToGraveyard) } else { (DecisionKind::Scry, Opt::ScryBottom) };
    let mut keep = vec![];
    let mut gone = vec![];
    for &o in &top {
        let c = cx.choose(Decision { player: p, kind, options: vec![Opt::ScryTop, away], subject: Some(o) })?;
        if c == Opt::ScryTop {
            keep.push(o)
        } else {
            gone.push(o)
        }
    }
    let mut ordered = vec![];
    while !keep.is_empty() {
        // Order of identical cards is meaningless: one option per distinct card
        // (`query::card_sig`: unless something refers to one of them).
        let first = crate::query::first_of_each_card(cx.s, cx.db, &keep);
        let opts: Vec<Opt> = keep.iter().zip(first).filter(|x| x.1).map(|x| Opt::Card(*x.0)).collect();
        let Opt::Card(o) = cx.choose(Decision { player: p, kind, options: opts, subject: None })? else { unreachable!() };
        keep.retain(|&x| x != o);
        ordered.push(o);
    }
    let lib = &mut cx.s.player_mut(p).library;
    lib.truncate(lib.len() - k);
    if !surveil {
        for &o in &gone {
            lib.insert(0, o);
        }
    }
    for &o in ordered.iter().rev() {
        lib.push(o);
    }
    if surveil && !gone.is_empty() {
        // Put back temporarily so move_batch can take them from the library.
        for &o in &gone {
            cx.s.player_mut(p).library.push(o);
        }
        let moves: Vec<Move> = gone.iter().map(|&o| Move::new(o, Zone::Graveyard)).collect();
        return Ok(move_batch(cx, &moves)?.into_iter().flatten().collect());
    }
    Ok(vec![])
}
