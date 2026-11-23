//! Long-tail mechanics, seventh batch.
//! Kept in its own module to stay additive (like `ext2`..`ext6`).
//!
//! - **Gift** (CR 702.174): "As an additional cost to cast this spell, you may choose an
//!   opponent" is an optional additional cost (an `OptionalCost` static named "Gift" that costs
//!   nothing; two players, so the chosen opponent is the opponent). The promise is the
//!   `OPTCOST` cast flag. Instants/sorceries: the gift effect is a node put in front of the
//!   spell's chain that only applies if the gift was promised (702.174b, j); permanents: an
//!   intervening-if "when this enters" trigger (702.174b).

use crate::basics::*;
use crate::carddb::*;
use crate::expr::{Condition, Expr};
use crate::state::*;
use forge_script::Face;

type Res<T> = Result<T, String>;

/// "If the gift was promised" (CR 702.174k): the spell's / permanent's optional additional cost.
pub fn promised() -> Condition {
    Condition::Compare(Expr::Alt(crate::altcast::AltExpr::CastFlag(crate::altcast::flag::OPTCOST, 1, 0)), crate::filter::CmpOp::GE, Expr::Num(1))
}

impl CardDb {
    /// `K:Gift` with its `SVar:GiftAbility` (CR 702.174a-b): see the module docs. `spell` is the
    /// face's spell ability (instants/sorceries), replaced by the new chain head.
    pub(crate) fn x7_gift(&mut self, f: &Face, permanent: bool, spell: &mut Option<AbilityId>, synth: &mut Vec<TriggerDef>, uns: &mut Vec<String>) -> Res<()> {
        // The promise is the one optional additional cost (`OPTCOST`): another one would share it.
        if f.keywords.iter().any(|k| k == "Bargain") || f.statics.iter().any(|s| s.get("Mode") == Some("OptionalCost")) {
            return Err("gift with another optional additional cost".into());
        }
        let sv = f.svar("GiftAbility").and_then(|s| s.as_ability()).ok_or("Gift without GiftAbility")?;
        let gift = self.ability_inner(&sv, f, uns, 1, false)?;
        if self.abilities[gift.0 as usize].sub.is_some() || self.abilities[gift.0 as usize].target.is_some() {
            return Err("gift ability form".into());
        }
        self.pending_statics.push(StaticDef {
            kind: StaticKind::Alt(crate::altcast::AltStatic::OptionalCost(crate::altcast::AddCost::Mana(ManaCost::default()))),
            zones: ZoneSet::of(Zone::Stack),
            conditions: vec![],
            text: "Gift".into(),
        });
        let text = format!("Gift {}", sv.get("GiftDescription").unwrap_or("").trim());
        if permanent {
            // 702.174b: "When this permanent enters, if its gift cost was paid, [effect]."
            let self_filter = crate::filter::Filter { alts: vec![crate::filter::Alt { base: crate::filter::Base::Card, props: vec![crate::filter::Prop::SelfCard] }] };
            self.abilities[gift.0 as usize].text = text.clone();
            synth.push(TriggerDef {
                mode: TrigMode::ChangesZone { origin: ZoneSet::ANY, dest: ZoneSet::of(Zone::Battlefield), valid: self_filter },
                zones: ZoneSet::of(Zone::Battlefield),
                execute: gift,
                optional: false,
                intervening: vec![promised()],
                per_turn: None,
                text: format!("{text}: when this enters, if the gift was promised"),
            });
            return Ok(());
        }
        // 702.174b, j: "If this spell's gift cost was paid, [effect]", before its other abilities.
        let head = spell.ok_or("Gift without a spell ability")?;
        let h = &self.abilities[head.0 as usize];
        if matches!(h.effect, Effect::Charm { .. }) || h.cost.is_some() || !h.activate_if.is_empty() || self.spell_life_x.contains(&head) {
            return Err("gift on this spell form".into());
        }
        let orig_text = h.text.clone();
        let g = &mut self.abilities[gift.0 as usize];
        g.kind = AbilityKind::Spell;
        g.conditions.push(promised());
        g.sub = Some(head);
        g.text = orig_text;
        self.abilities[head.0 as usize].kind = AbilityKind::Sub;
        *spell = Some(gift);
        Ok(())
    }
}

/// Every alternative of a "you may play" filter requires the card to have been exiled with the
/// static's source (`ExiledWithSource`, CR 607.2a): such a permission covers cards another
/// player owns (Null Summoner), unlike the own-cards "play from your graveyard/exile" statics.
pub fn exiled_with_source_only(f: &crate::filter::Filter) -> bool {
    !f.alts.is_empty() && f.alts.iter().all(|a| a.props.iter().any(|p| matches!(p, crate::filter::Prop::Ext(crate::filter_ext::ExtProp::ExiledWithSource))))
}

/// The source's card has a "you may play the exiled card" static that refers to the cards
/// exiled with it (`ExiledWithSource`, CR 607.2a): its "choose a card ... exile it" links them.
pub fn source_refers_to_exiled(s: &State, db: &CardDb, src: ObjId) -> bool {
    let o = s.obj(src);
    db.get(o.card).face(o.face).statics.iter().any(|st| matches!(&st.kind, StaticKind::MayPlay { affected, .. } if exiled_with_source_only(affected)))
}

/// The conditions of a permanent's static ability hold (`Condition$ Threshold`, ...).
pub fn static_conditions_hold(s: &State, db: &CardDb, st: &StaticDef, src: ObjId) -> bool {
    if st.conditions.is_empty() {
        return true;
    }
    let ecx = crate::expr::EvalCtx { s, db, you: s.obj(src).controller, source: src, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &[], imprinted: &[], base_only: false };
    st.conditions.iter().all(|c| crate::expr::holds(c, &ecx))
}

/// Does a "you may cast [it] ... and mana of any type can be spent to cast it" permission (CR
/// 609.4b; `StaticKind::MayPlay { any_type: true }`) let `p` cast `o` from where it is? Mirrors
/// `cast::may_play` for the permissions with `any_type`.
pub fn any_type_permission(cx: &crate::engine::Ctx, p: PlayerId, o: ObjId) -> bool {
    use crate::query::*;
    let s = &*cx.s;
    let z = s.obj(o).zone;
    if !matches!(z, Zone::Exile | Zone::Graveyard) {
        return false;
    }
    for eo in &s.effect_objs {
        if eo.controller != p || !eo.remembered.contains(&o) {
            continue;
        }
        for &si in &eo.statics {
            // effect-object static reader: MayPlay (`static_ext::applies_from_effect_object`)
            let st = &cx.db.statics_pool[si as usize];
            let StaticKind::MayPlay { affected, zones, any_type: true } = &st.kind else { continue };
            if !st.conditions.is_empty() {
                let ecx = crate::expr::EvalCtx { s, db: cx.db, you: p, source: eo.source, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &eo.remembered, imprinted: &[], base_only: false };
                if !st.conditions.iter().all(|c| crate::expr::holds(c, &ecx)) {
                    continue;
                }
            }
            let fcx = FilterCtx { you: p, ..eo_fcx!(eo) };
            if zones.has(z) && obj_matches(s, cx.db, affected, o, fcx) {
                return true;
            }
        }
    }
    for &src in &s.battlefield {
        let so = s.obj(src);
        if so.controller != p {
            continue;
        }
        for st in &cx.db.get(so.card).face(so.face).statics {
            let StaticKind::MayPlay { affected, zones, any_type: true } = &st.kind else { continue };
            if !st.zones.has(Zone::Battlefield) || !zones.has(z) || (crate::query::may_lose_abilities(s, cx.db) && lost_all_abilities(s, cx.db, src)) || !static_conditions_hold(s, cx.db, st, src) {
                continue;
            }
            if (s.obj(o).owner == p || exiled_with_source_only(affected)) && obj_matches(s, cx.db, affected, o, FilterCtx { you: p, source: Some(src), x: 0, rem: &[] }) {
                return true;
            }
        }
    }
    false
}

/// Card-database side tables of this batch (`CardDb::x7`).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct X7Tables {
    /// Compile-time only: the body being compiled is that of a "whenever ~ deals combat damage
    /// to a player" trigger (`rewrite_filter`).
    #[serde(skip)]
    pub combat_to_player: bool,
    /// The paradigm delayed trigger's ability (`X7Effect::ParadigmCopy`), if a loaded card has
    /// paradigm.
    pub paradigm_ab: Option<AbilityId>,
    /// Charm nodes whose modes can each be chosen once per turn per source ("choose one that
    /// hasn't been chosen this turn", `ChoiceRestriction$ ThisTurn`; Monument to Endurance).
    pub this_turn_charms: Vec<AbilityId>,
}

/// Per-game state of this batch (`Ext::x7`).
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct X7State {
    /// Paradigm (CR 702.192a): (player, card, the spell object that first resolved) for each
    /// name whose paradigm spell a player has resolved; each sets up "at the beginning of each of
    /// your precombat main phases for the rest of the game" (never reset).
    pub paradigm: smallvec::SmallVec<[(PlayerId, CardId, ObjId); 0]>,
    /// Modes chosen this turn for `X7Tables::this_turn_charms`: (source, charm node, mode bits).
    pub modes_used: smallvec::SmallVec<[(ObjId, AbilityId, u32); 0]>,
    /// Players with hexproof until end of turn (bit per player; CR 702.11c), cleared in the
    /// cleanup step (514.2).
    pub player_hexproof: u8,
}

impl X7State {
    pub fn new_turn(&mut self) {
        self.modes_used.clear();
    }
}

/// The charm node of `a`'s chain restricted to "not chosen this turn", if any.
fn this_turn_charm(db: &CardDb, a: AbilityId) -> Option<AbilityId> {
    if db.x7.this_turn_charms.is_empty() {
        return None;
    }
    crate::resolve::nodes(db, a, &[]).into_iter().find(|n| db.x7.this_turn_charms.contains(n))
}

/// Mode bits `source` already chose this turn for `a`'s "not chosen this turn" charm (0: none).
pub fn modes_used_this_turn(s: &State, db: &CardDb, a: AbilityId, source: Option<ObjId>) -> u32 {
    let (Some(src), Some(c)) = (source, this_turn_charm(db, a)) else { return 0 };
    s.ext.x7.modes_used.iter().filter(|x| x.0 == src && x.1 == c).fold(0, |m, x| m | x.2)
}

/// Record the modes just chosen for `a` by `source` (`cast::choose_modes`).
pub fn note_modes(s: &mut State, db: &CardDb, a: AbilityId, source: ObjId, modes: &[u8]) {
    if let Some(c) = this_turn_charm(db, a) {
        let bits = modes.iter().fold(0u32, |m, &i| m | (1 << i));
        s.ext.x7.modes_used.push((source, c, bits));
    }
}

/// Effects of this batch.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum X7Effect {
    /// Paradigm's delayed trigger (CR 702.192a): create a copy of the card (`Exec::source_card`)
    /// in exile; its controller may cast the copy without paying its mana cost.
    ParadigmCopy,
    /// "Add N mana in any combination of colors" (`Mana | Produced$ Combo Any`), into the pool
    /// with an optional spending restriction (CR 106.6; `XTables::restricts`). The colors are
    /// chosen one mana at a time in WUBRG order (each multiset offered once).
    AddManaCombo { n: u8, restrict: Option<u16> },
    /// Recruit (CR 701.70a): draw a card, then discard a card; if a nonland card was discarded,
    /// create the token (a 1/1 white Human Soldier).
    Recruit { token: CardId },
    /// Reflexive "when you do" after "create N tokens" (`ImmediateTrigger | TriggerAmount$
    /// Remembered$Amount/DivideEvenlyDown.N`, Faebloom Trick): triggers once if the tokens were
    /// created (CR 603.12: the action was performed once; XMage `FaebloomTrickEffect`). Forge's
    /// amount would trigger twice when a doubler makes four tokens; we follow XMage and 603.12.
    ReflexiveIfCreated { execute: AbilityId },
    /// You and the matching permanents gain hexproof until end of turn (`x7_pump_you_and`).
    HexproofYouAnd { valid: crate::filter::Filter },
}

impl CardDb {
    /// Effect APIs of this batch (`None`: not one of them).
    pub(crate) fn x7_api(&mut self, api: &str, p: &crate::compile::P, f: &Face, uns: &mut Vec<String>, depth: u32) -> Res<Option<Effect>> {
        Ok(Some(match api {
            // CR 701.70a.
            "Recruit" => {
                if p.peek("Defined").is_some_and(|d| d != "You") {
                    return Err("Recruit for another player".into());
                }
                p.ack("Defined", &["You"], "checked above");
                let token = self.token("w_1_1_human_soldier")?;
                Effect::Ext(crate::effect_ext::ExtEffect::X7(X7Effect::Recruit { token }))
            }
            // "Create two ... tokens. When you do, ...": once per two tokens remembered.
            "ImmediateTrigger" if p.peek("TriggerAmount").is_some_and(|a| a.starts_with("Remembered$Amount/DivideEvenlyDown.")) => {
                p.matched("TriggerAmount", |a| a.starts_with("Remembered$Amount/DivideEvenlyDown."), "checked above");
                let exec = p.get("Execute").ok_or("ImmediateTrigger without Execute")?;
                let sv = f.svar(exec).and_then(|s| s.as_ability()).ok_or_else(|| format!("ImmediateTrigger execute {exec} missing"))?;
                let execute = self.ability_inner(&sv, f, uns, depth + 1, false)?;
                Effect::Ext(crate::effect_ext::ExtEffect::X7(X7Effect::ReflexiveIfCreated { execute }))
            }
            _ => return Ok(None),
        }))
    }

    /// `Pump | Defined$ You & Valid <filter> | KW$ Hexproof` (Dawn's Truce): you and the matching
    /// permanents gain hexproof until end of turn (CR 702.11b-c, 611.2c: the permanents are
    /// those matching as it resolves).
    pub(crate) fn x7_pump_you_and(&mut self, p: &crate::compile::P, f: &Face) -> Res<Effect> {
        let d = p.get("Defined").unwrap();
        let valid = self.filter_f(f, d.strip_prefix("You & Valid ").unwrap())?;
        if p.get("KW") != Some("Hexproof") || p.peek("NumAtt").is_some() || p.peek("NumDef").is_some() || p.peek("Duration").is_some() {
            return Err("Pump You & Valid form".into());
        }
        Ok(Effect::Ext(crate::effect_ext::ExtEffect::X7(X7Effect::HexproofYouAnd { valid })))
    }

    /// The shared paradigm delayed-trigger ability (`X7Tables::paradigm_ab`).
    pub(crate) fn x7_paradigm_ability(&mut self) {
        if self.x7.paradigm_ab.is_none() {
            let id = self.push_ability(AbilityDef {
                kind: AbilityKind::Sub,
                effect: Effect::Ext(crate::effect_ext::ExtEffect::X7(X7Effect::ParadigmCopy)),
                defined: None,
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
                text: "Paradigm: create a copy of this spell in exile. You may cast the copy without paying its mana cost.".into(),
            });
            self.x7.paradigm_ab = Some(id);
        }
    }
}

/// A paradigm spell resolved (CR 702.192a): if it's the first time a spell its controller
/// controls with this name resolved this game, the delayed trigger is set up.
pub fn paradigm_resolved(cx: &mut crate::engine::Ctx, item: &StackItem) {
    let x = &mut cx.s.ext.x7;
    if !x.paradigm.iter().any(|e| e.0 == item.controller && e.1 == item.source_card) {
        x.paradigm.push((item.controller, item.source_card, item.source));
    }
}

/// Paradigm's delayed triggers at the beginning of the active player's precombat main phase
/// (CR 702.192a, 505.1a), in the order the names first resolved (from `trig_ext::fire_extra`).
pub fn paradigm_triggers(s: &State, db: &CardDb, ev: &crate::triggers::TrigEvent, fired: &mut Vec<PendingTrigger>) {
    let (crate::triggers::TrigEvent::StepBegin { step: Step::Main1 }, Some(ab)) = (ev, db.x7.paradigm_ab) else { return };
    for &(p, card, src) in &s.ext.x7.paradigm {
        if p == s.turn.active {
            fired.push(PendingTrigger { source: src, card, controller: p, trigger_idx: 0, trig: TrigCtx::default(), adhoc: Some(ab) });
        }
    }
}

/// Discard `cards` from `p`'s hand (CR 701.9a): announced per card (`Event::Discard`, which
/// discard triggers count) and moved to the graveyard together.
pub fn discard(cx: &mut crate::engine::Ctx, p: PlayerId, cards: &[ObjId]) -> crate::engine::R<()> {
    for &h in cards {
        let card = cx.s.obj(h).card;
        cx.s.emit(Event::Discard { player: p, card });
    }
    let moves: Vec<crate::zones::Move> = cards.iter().map(|&h| crate::zones::Move::new(h, Zone::Graveyard)).collect();
    crate::zones::move_batch(cx, &moves)?;
    Ok(())
}

pub fn apply(cx: &mut crate::engine::Ctx, ex: &mut crate::resolve::Exec, _ad: &AbilityDef, x: &X7Effect, _tg: &[Target], _who: &[Target]) -> crate::engine::R<()> {
    use crate::engine::{Decision, DecisionKind, Opt};
    match x {
        X7Effect::ParadigmCopy => {
            // CR 707.12-style: the copy is created in exile (a card copy: a token, CR 111.13
            // aside, that ceases to exist when it leaves; 704.5d), then may be cast while this
            // ability resolves (608.2g: timing ignored; targets and restrictions apply).
            let p = ex.you;
            let copy = cx.s.new_token(ex.source_card, p);
            cx.s.obj_mut(copy).zone = Zone::Exile;
            cx.s.place(copy, false);
            let mut opts = vec![Opt::Done];
            for face in crate::faces::spell_faces(cx.s, cx.db, copy, crate::faces::FacePerm::Any) {
                if crate::altcast::castable(cx, p, copy, face, true, crate::altcast::Method::Free) {
                    opts.push(crate::faces::choice_opt(copy, face));
                }
            }
            let opt = cx.choose(Decision { player: p, kind: DecisionKind::ChooseCard, options: opts, subject: Some(ex.source) })?;
            match crate::faces::chosen_face(&opt) {
                Some((_, face)) => crate::cast::cast_spell_method(cx, p, copy, face, crate::altcast::Method::Free)?,
                // Not cast: the copy ceases to exist (CR 704.5d; the rulings' "next time
                // state-based actions are checked" with nothing in between).
                _ => {
                    cx.s.move_obj(copy, Zone::Gone, false);
                }
            }
        }
        X7Effect::AddManaCombo { n, restrict } => {
            let all = [ManaType::W, ManaType::U, ManaType::B, ManaType::R, ManaType::G];
            let mut low = 0usize;
            for _ in 0..*n {
                let options: Vec<Opt> = all[low..].iter().map(|&t| Opt::PoolMana(t)).collect();
                let Opt::PoolMana(t) = cx.choose(Decision { player: ex.you, kind: DecisionKind::ChooseMode, options, subject: Some(ex.source) })? else { unreachable!() };
                low = all.iter().position(|&x| x == t).unwrap_or(0);
                crate::ext6::add_pool(cx.s, ex.you, t, 1, restrict.map(crate::ext6::Restr::Spell));
            }
        }
        X7Effect::Recruit { token } => {
            let p = ex.you;
            crate::flow::draw_n(cx.s, cx.db, p, 1);
            let items = crate::flow::hand_items(cx, p);
            if items.is_empty() {
                return Ok(());
            }
            let chosen: Vec<ObjId> = cx.choose_set(p, DecisionKind::Discard, items, 1, 1, Some(ex.source))?.into_iter().filter_map(|c| if let Opt::Card(x) = c { Some(x) } else { None }).collect();
            let nonland = chosen.iter().any(|&h| !crate::query::chars(cx.s, cx.db, h).types.has(CardType::Land));
            crate::ext7::discard(cx, p, &chosen)?;
            if nonland {
                crate::zones::create_tokens(cx, *token, p, 1, false)?;
            }
        }
        X7Effect::HexproofYouAnd { valid } => {
            cx.s.ext.x7.player_hexproof |= 1 << ex.you.0;
            let fcx = exec_fcx!(ex);
            let objs: Vec<ObjId> = cx.s.battlefield.iter().copied().filter(|&o| crate::query::obj_matches(cx.s, cx.db, valid, o, fcx)).collect();
            for o in objs {
                crate::resolve::pump(cx.s, ex.you, o, 0, 0, KwSet::default().with(Kw::Hexproof), Duration::EndOfTurn);
            }
        }
        X7Effect::ReflexiveIfCreated { execute } => {
            if !ex.remembered.is_empty() {
                crate::trig_ext::add_reflexive(cx, ex.you, ex.source, *execute, ex.trig);
            }
        }
    }
    Ok(())
}

/// Filter rewrites that are identities in a two-player game. `ControlledBy Spawner>TriggeredTarget`
/// in the body of a "whenever ~ deals combat damage to a player" trigger (Leatherhead: "destroy
/// target artifact or enchantment that player controls"): combat damage to a player is dealt by
/// an attacking creature to the player it attacks (CR 510.1a, 506.2), a creature attacks only
/// while its controller is the active player, who attacks an opponent (CR 508.1b), and a
/// change of control removes it from combat (506.4); the trigger's controller is that
/// creature's controller (603.3a). So "that player" is the trigger controller's opponent.
pub fn rewrite_filter(db: &CardDb, s: &str) -> Option<String> {
    // Dreadmaw's Ire grants the trigger with `ControlledBy TriggeredTarget` (the same player).
    if db.x7.combat_to_player && s.contains("ControlledBy ") {
        let r = s.replace("ControlledBy Spawner>TriggeredTarget", "OppCtrl").replace("ControlledBy TriggeredTarget", "OppCtrl");
        return (r != s).then_some(r);
    }
    None
}
