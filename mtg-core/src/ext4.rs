//! Long-tail mechanics, fourth batch: valiant (Emberheart
//! Challenger), noncombat-damage "+1" replacements restricted to opponents (Tomik), void
//! (Plasma Bolt), ward granted to a filtered set of permanents (Hexing Squelcher), threshold
//! triggers (Tersa Lightshatter), "whenever a player draws a card" (Razorkin Needlehead) and more.
//! Kept in its own module to stay additive (like `ext2`, `ext3`).

use crate::basics::*;
use crate::carddb::*;
use crate::engine::*;
use crate::filter::Filter;
use crate::query::*;
use crate::state::*;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// Per-game state of this batch (`Ext::mr`). Everything here is per-turn history.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MrState {
    /// Objects that became the target of a spell or ability this turn: (object, controller of
    /// that spell/ability). Valiant (Emberheart Challenger) triggers only the first time.
    pub targeted: SmallVec<[(ObjId, PlayerId); 2]>,
    /// A spell was cast for its warp cost this turn (void, Plasma Bolt).
    pub warped: bool,
    /// Creatures declared as attackers this turn (Witchstalker Frenzy, CR 508.1).
    pub attacked: u8,
    /// Creatures that attack this turn's combat if able: (creature, turn number).
    pub must_attack: SmallVec<[(ObjId, u32); 1]>,
    /// Spells cast for their warp cost this turn, by caster (CR 702.185c), saturating at 255
    /// (encoding v10.2, `PlayerView::warped_this_turn`; `warped` is what void reads).
    #[serde(default, skip_serializing_if = "is_zero2")]
    pub warped_by: [u8; 2],
}

fn is_zero2(x: &[u8; 2]) -> bool {
    *x == [0, 0]
}

impl MrState {
    /// As each turn begins (CR 500).
    pub fn new_turn(&mut self) {
        self.targeted.clear();
        self.warped = false;
        self.warped_by = [0; 2];
        self.attacked = 0;
        self.must_attack.clear();
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MrExpr {
    /// 1 if void holds this turn: a nonland permanent left the battlefield or a spell was warped
    /// this turn (Edge of Eternities ability word, CR 207.2c; Forge `Game.isVoid`).
    Void,
    /// Creatures that attacked this turn (`PlayerCountPlayers$AttackersDeclared`).
    AttackedThisTurn,
}

pub fn eval_expr(e: &MrExpr, c: &crate::expr::EvalCtx) -> i32 {
    match e {
        MrExpr::Void => {
            let s = c.s;
            let left = s.ext.moved_this_turn.iter().any(|&(old, from, _, _)| from == Zone::Battlefield && !chars(s, c.db, old).types.has(CardType::Land));
            (left || s.ext.mr.warped) as i32
        }
        MrExpr::AttackedThisTurn => c.s.ext.mr.attacked as i32,
    }
}

/// A spell `so` was just cast by `p` (CR 601.2i).
pub fn spell_cast(cx: &mut Ctx, p: PlayerId, so: ObjId) {
    if !cx.s.cast_info.is_empty() && cx.s.cast_of(so).flags & crate::altcast::flag::WARP != 0 {
        cx.s.ext.mr.warped = true;
        let n = &mut cx.s.ext.mr.warped_by[p.idx()];
        *n = n.saturating_add(1);
    }
}

/// Record that `targets` became targets of a spell/ability controlled by `controller` (after its
/// "becomes the target" triggers were checked).
pub fn note_targeted(s: &mut State, targets: &[ObjId], controller: PlayerId) {
    for &t in targets {
        if !s.ext.mr.targeted.contains(&(t, controller)) {
            s.ext.mr.targeted.push((t, controller));
        }
    }
}

/// Valiant: has `o` already been the target of a spell or ability `p` controls this turn?
pub fn was_targeted_by(s: &State, o: ObjId, p: PlayerId) -> bool {
    s.ext.mr.targeted.contains(&(o, p))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MrStatic {
    /// "If a source you control would deal noncombat damage to an opponent or a permanent an
    /// opponent controls, it deals that much damage plus `plus` instead" (Tomik; CR 614.1a).
    /// `players`: which damaged players qualify; `objs`: which damaged permanents qualify.
    DamagePlusTo { source: Filter, players: Option<PlayerSel>, objs: Option<Filter>, noncombat: bool, plus: i32 },
    /// "As long as [this] has a `counter` counter on it, nonbasic lands are [basic land type
    /// `basic`, WUBRG index]" (Zhao, the Moon Slayer; CR 305.7): layer 4 sets the land type,
    /// the lands lose their rules-text abilities and have the basic type's mana ability.
    LandsBecome { counter: CounterKind, basic: u8 },
}

/// The active statics of this batch: (source, controller, static def, static).
pub fn active_mr<'a>(s: &'a State, db: &'a CardDb) -> impl Iterator<Item = (ObjId, PlayerId, &'a StaticDef, &'a MrStatic)> + 'a {
    s.battlefield
        .iter()
        .flat_map(move |&src| {
            let so = s.obj(src);
            db.get(so.card).face(so.face).statics.iter().filter_map(move |st| match &st.kind {
                StaticKind::Ext(crate::static_ext::StaticExt::Mr(x)) if st.zones.has(Zone::Battlefield) => Some((src, so.controller, st, x)),
                _ => None,
            })
        })
        .filter(move |(src, ..)| s.effect_objs.is_empty() || !crate::query::lost_all_abilities(s, db, *src))
}

/// Damage after this batch's "deals that much damage plus N instead" replacements.
pub fn modified_damage(s: &State, db: &CardDb, source: ObjId, to: Target, combat: bool, n: i32) -> i32 {
    let mut n = n;
    for (src, you, st, x) in active_mr(s, db) {
        let MrStatic::DamagePlusTo { source: f, players, objs, noncombat, plus } = x else { continue };
        if *noncombat && combat {
            continue;
        }
        let fcx = FilterCtx { you, source: Some(src), x: 0, rem: &[] };
        if !obj_matches(s, db, f, source, fcx) {
            continue;
        }
        let to_ok = match to {
            Target::Player(p) => players.is_some_and(|sel| sel.matches(p, you)),
            Target::Obj(o) => objs.as_ref().is_some_and(|f| obj_matches(s, db, f, o, fcx)),
            Target::Ability { .. } => false,
        };
        if !to_ok {
            continue;
        }
        let ecx = crate::expr::EvalCtx { s, db, you, source: src, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &[], imprinted: &[], base_only: false };
        if st.conditions.iter().all(|c| crate::expr::holds(c, &ecx)) {
            n = crate::num::add(n, *plus);
        }
    }
    n
}

/// Fire "whenever [a player] draws a card" for the card `drawn` (now in `p`'s hand), CR 121.2.
pub fn fire_drawn(s: &mut State, db: &CardDb, p: PlayerId, drawn: ObjId) {
    let d = &mut s.ext.le.draws[p.idx()];
    *d = d.saturating_add(1);
    crate::triggers::fire(s, db, &crate::triggers::TrigEvent::Ext(crate::trig_ext::ExtEvent::Drawn { player: p, card: drawn }));
}

/// Creatures were declared as attackers (CR 508.1).
pub fn note_attackers(s: &mut State, n: usize) {
    s.ext.mr.attacked = s.ext.mr.attacked.saturating_add(n.min(255) as u8);
}

// ---------------------------------------------------------------- compiling

impl CardDb {
    /// Trigger modes of this batch, checked before the others (`None`: not one of them; keys
    /// untouched).
    pub(crate) fn mr_trigger_mode(&self, m: &str, p: &crate::compile::P) -> Result<Option<TrigMode>, String> {
        use crate::trig_ext::TrigExt as T;
        Ok(Some(match m {
            // Valiant — "Whenever [this] becomes the target of a spell or ability you control
            // for the first time each turn" (Bloomburrow ability word, CR 207.2c).
            "BecomesTarget" if p.peek("Valiant").is_some() => {
                if !p.flag("Valiant") || p.get("ValidSource") != Some("SpellAbility.YouCtrl") || p.peek("FirstTime").is_some() {
                    return Err("BecomesTarget Valiant form".into());
                }
                TrigMode::Ext(T::Valiant { valid: self.filter(p.get("ValidTarget").ok_or("BecomesTarget without ValidTarget")?)? })
            }
            // "Whenever [a player] draws a card" (CR 121.2: each card drawn is an event).
            "Drawn" => {
                let valid = self.filter(p.get("ValidCard").unwrap_or("Card"))?;
                if p.peek("ValidPlayer").is_some() {
                    return Err("Drawn form".into());
                }
                // "Whenever you draw your Nth card each turn" (`ext5`, Raven Eagle).
                let nth = match p.get("Number") {
                    None => 0,
                    Some(n) => n.parse::<u8>().ok().filter(|&n| n >= 1).ok_or("Drawn Number")?,
                };
                TrigMode::Ext(T::Drawn { valid, nth })
            }
            _ => return Ok(None),
        }))
    }

    /// Intervening "if" conditions of this batch's triggers (CR 603.4).
    pub(crate) fn mr_trigger_conditions(&self, p: &crate::compile::P) -> Result<Vec<crate::expr::Condition>, String> {
        use crate::expr::{Condition, Expr, PlayerProp, Ref};
        let mut out = vec![];
        // Threshold — "if seven or more cards are in your graveyard" (CR 207.2c ability word;
        // Forge `Threshold$ True`), checked as it triggers and again on resolution (603.4).
        if p.peek("Threshold").is_some() {
            if !p.flag("Threshold") {
                return Err("trigger Threshold form".into());
            }
            out.push(Condition::Compare(Expr::Player(Ref::You, PlayerProp::CardsInGraveyard), crate::filter::CmpOp::GE, Expr::Num(7)));
        }
        Ok(out)
    }

    /// Replacement effects of this batch (`None`: not one of them; keys untouched).
    pub(crate) fn mr_replacement(&mut self, ev: &str, p: &crate::compile::P, f: &forge_script::Face) -> Result<Option<(MrStatic, ZoneSet)>, String> {
        Ok(Some(match ev {
            // Tomik: `ValidTarget$ Permanent.OppCtrl,Opponent | IsCombat$ False`, "+1".
            "DamageDone" if p.peek("IsCombat").is_some() || p.peek("ValidTarget").is_some_and(|t| t.split(',').any(|a| a == "Opponent")) => {
                let zones = ZoneSet::parse(p.get("ActiveZones").unwrap_or("Battlefield"))?;
                let noncombat = match p.get("IsCombat") {
                    None => false,
                    Some(v) if v.eq_ignore_ascii_case("false") => true,
                    Some(v) => return Err(format!("DamageDone IsCombat {v}")),
                };
                let (mut players, mut objs) = (None, vec![]);
                for a in p.get("ValidTarget").ok_or("DamageDone without ValidTarget")?.split(',') {
                    match a {
                        "Opponent" | "Player.Opponent" => players = Some(PlayerSel::Opponent),
                        "Player" => players = Some(PlayerSel::Any),
                        o if o.starts_with("Permanent") || o.starts_with("Creature") || o.starts_with("Planeswalker") => objs.push(o),
                        o => return Err(format!("DamageDone ValidTarget {o}")),
                    }
                }
                let objs = if objs.is_empty() { None } else { Some(self.filter_f(f, &objs.join(","))?) };
                // Emblems aren't separate sources here: an effect object's damage is dealt by its
                // source object, which `Card.YouCtrl` already covers.
                let vs = p.get("ValidSource").ok_or("DamageDone without ValidSource")?;
                let vs: Vec<&str> = vs.split(',').filter(|a| *a != "Emblem.YouCtrl").collect();
                let source = self.filter_f(f, &vs.join(","))?;
                let with = p.get("ReplaceWith").ok_or("replacement without ReplaceWith")?;
                let sv = f.svar(with).and_then(|s| s.as_ability()).ok_or("ReplaceWith missing")?;
                let plus = match (sv.api(), sv.get("VarName"), sv.get("VarValue").and_then(|v| f.svar(v)).map(|v| v.value.clone()), sv.pairs.len()) {
                    (Some(("DB", "ReplaceEffect")), Some("DamageAmount"), Some(v), 3) => v.strip_prefix("ReplaceCount$DamageAmount/Plus.").and_then(|n| n.parse::<i32>().ok()).ok_or("DamageDone amount")?,
                    _ => return Err("replacement DamageDone form".into()),
                };
                (MrStatic::DamagePlusTo { source, players, objs, noncombat, plus }, zones)
            }
            _ => return Ok(None),
        }))
    }
}

/// `Count$<body>` forms of this batch.
pub fn compile_count(body: &str) -> Option<crate::expr::Expr> {
    use crate::expr::Expr;
    // `Count$Void.A.B`: A if void holds, else B.
    if let Some(rest) = body.strip_prefix("Void.") {
        let mut it = rest.split('.');
        let a = it.next()?.parse::<i32>().ok()?;
        let b = it.next()?.parse::<i32>().ok()?;
        if it.next().is_some() {
            return None;
        }
        let v = Expr::Ext(Box::new(crate::expr_ext::ExtExpr::Mr(MrExpr::Void)));
        return Some(Expr::Compare(Box::new(v), crate::filter::CmpOp::EQ, Box::new(Expr::Num(1)), Box::new(Expr::Num(a)), Box::new(Expr::Num(b))));
    }
    None
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MrEffect {
    /// Move `n` cards matching `valid` chosen at random from the affected player's graveyard to
    /// `dest` (Tersa Lightshatter: "exile a card at random from your graveyard"; Forge
    /// `ChangeZone | Origin$ Graveyard | Hidden$ True | AtRandom$ True | Mandatory$ True`).
    RandomFromGraveyard { valid: Filter, n: u8, dest: Zone },
    /// "Discard up to `n` cards" (Forge `Discard | Mode$ TgtChoose | Optional$ True`, whose
    /// minimum is 0): the affected player chooses 0..=n cards of their hand (matching `valid`).
    DiscardUpTo { n: crate::expr::Expr, valid: Option<Filter> },
    /// "You may cast that card" for the cards imprinted earlier in this resolution (Chandra,
    /// Torch of Defiance; Forge `Play | Valid$ Card.IsImprinted | ValidSA$ Spell`). Cast
    /// during the resolution, paying its costs, ignoring timing permissions based on card type
    /// (CR 608.2g, Chandra rulings); lands can't be cast. `remember`: the spell cast is
    /// remembered (`RememberPlayed$`).
    MayCastImprinted { remember: bool },
    /// "{T}: Add {R} for each Goblin you control" (a mana ability whose amount is an
    /// expression, evaluated when `cast::mana_sources` lists it). Never resolves.
    ManaCount { t: ManaType, n: crate::expr::Expr },
    /// The remembered creatures attack this combat if able (Forge `Effect | StaticAbilities$
    /// <MustAttack> | Duration$ UntilEndOfCombat`; CR 508.1d requirement).
    MustAttackThisCombat,
    /// "Look at the top `n` cards of your library and exile them face down" (Flameshape;
    /// Forge `Dig | ExileFaceDown$ True | WithMayLook$ True | ChangeNum$ All`): face down
    /// (CR 406.3) but known to their owner, who looked at them (406.3: "unless instructed").
    ExileTopFaceDownLook { n: crate::expr::Expr },
}

pub fn apply(cx: &mut Ctx, ex: &mut crate::resolve::Exec, ad: &AbilityDef, x: &MrEffect, _tg: &[Target], who: &[Target]) -> R<()> {
    match x {
        MrEffect::ExileTopFaceDownLook { n } => {
            // Your library, or the targeted player's ("target opponent exiles the top X cards of
            // their library face down", Outrageous Robbery); the controller may look at them.
            let p = ex.you;
            let lib = match who.iter().find_map(|t| if let Target::Player(q) = t { Some(*q) } else { None }) {
                Some(q) => q,
                None if ad.target.is_some() => return Ok(()), // its target player is illegal (608.2b)
                None => p,
            };
            let k = crate::resolve::amount(cx, ex, n).max(0) as usize;
            let top: Vec<crate::zones::Move> = cx.s.player(lib).library.iter().rev().take(k).map(|&o| crate::zones::Move::new(o, Zone::Exile)).collect();
            let new: Vec<ObjId> = crate::ext3::exile_face_down(cx, &top)?.into_iter().flatten().collect();
            for &o in &new {
                if cx.s.obj(o).zone == Zone::Exile {
                    cx.s.obj_mut(o).known_to = 1 << p.0;
                }
            }
            ex.moved = new.iter().copied().collect();
            if ad.remember {
                ex.remembered.extend(new);
            }
        }
        MrEffect::ManaCount { .. } => return cx.unsupported("mana ability on the stack"),
        MrEffect::MustAttackThisCombat => {
            let turn = cx.s.turn.number;
            for o in ex.remembered.clone() {
                if cx.s.obj(o).zone == Zone::Battlefield {
                    cx.s.ext.mr.must_attack.push((o, turn));
                }
            }
        }
        MrEffect::MayCastImprinted { remember } => {
            use crate::altcast::Method;
            let p = ex.you;
            let mut opts = vec![Opt::Done];
            for o in ex.imprinted.clone() {
                if cx.s.obj(o).zone != Zone::Exile {
                    continue;
                }
                // "You may cast that card": either face of a two-faced card (`faces`).
                for face in crate::faces::spell_faces(cx.s, cx.db, o, crate::faces::FacePerm::Any) {
                    if crate::altcast::castable(cx, p, o, face, true, Method::Normal) {
                        opts.push(crate::faces::choice_opt(o, face));
                    }
                }
            }
            if opts.len() == 1 {
                return Ok(());
            }
            let opt = cx.choose(Decision { player: p, kind: DecisionKind::ChooseCard, options: opts, subject: Some(ex.source) })?;
            let Some((o, face)) = crate::faces::chosen_face(&opt) else { return Ok(()) };
            crate::cast::cast_spell_method(cx, p, o, face, Method::Normal)?;
            if *remember {
                if let Some(it) = cx.s.stack.last() {
                    ex.remembered.push(it.source);
                }
            }
        }
        MrEffect::DiscardUpTo { n, valid } => {
            let k = crate::resolve::amount(cx, ex, n).max(0) as usize;
            for p in affected_players(ex, who) {
                let fcx = exec_fcx!(ex);
                let items: Vec<(crate::query::ObjSig, Opt)> = crate::flow::hand_items(cx, p)
                    .into_iter()
                    .filter(|(_, o)| match (o, valid) {
                        (Opt::Card(h), Some(f)) => obj_matches(cx.s, cx.db, f, *h, fcx),
                        _ => true,
                    })
                    .collect();
                let max = k.min(items.len());
                let mut chosen: SmallVec<[ObjId; 4]> = SmallVec::new();
                for c in cx.choose_set(p, DecisionKind::Discard, items, 0, max, Some(ex.source))? {
                    if let Opt::Card(h) = c {
                        chosen.push(h);
                    }
                }
                for &h in &chosen {
                    let card = cx.s.obj(h).card;
                    cx.s.emit(Event::Discard { player: p, card });
                }
                let moves: Vec<crate::zones::Move> = chosen.iter().map(|&h| crate::zones::Move::new(h, Zone::Graveyard)).collect();
                let new = crate::zones::move_batch(cx, &moves)?;
                if ad.remember {
                    ex.remembered.extend(new.into_iter().flatten());
                }
            }
        }
        MrEffect::RandomFromGraveyard { valid, n, dest } => {
            for p in affected_players(ex, who) {
                let fcx = exec_fcx!(ex);
                let mut cands: Vec<ObjId> = cx.s.player(p).graveyard.iter().copied().filter(|&o| crate::query::is_card(cx.s, o) && obj_matches(cx.s, cx.db, valid, o, fcx)).collect();
                let mut picked = vec![];
                // The graveyard is public; the pick uses the game's seeded RNG (deterministic).
                for _ in 0..*n {
                    if cands.is_empty() {
                        break;
                    }
                    let i = cx.s.rng.below(cands.len() as u32) as usize;
                    picked.push(cands.remove(i));
                }
                let moves: Vec<crate::zones::Move> = picked.iter().map(|&o| crate::zones::Move::new(o, *dest)).collect();
                let new: Vec<ObjId> = crate::zones::move_batch(cx, &moves)?.into_iter().flatten().collect();
                ex.moved = new.iter().copied().collect();
                if ad.remember {
                    ex.remembered.extend(new);
                }
            }
        }
    }
    Ok(())
}

impl CardDb {
    /// `ChangeZone` forms of this batch (`None`: not one of them; keys untouched).
    pub(crate) fn mr_change_zone(&mut self, p: &crate::compile::P, f: &forge_script::Face, origin: ZoneSet, dest: Zone) -> Result<Option<Effect>, String> {
        if p.peek("AtRandom").is_none() {
            return Ok(None);
        }
        // Only the random, mandatory, hidden pick from a graveyard (no player choice at all).
        if !p.flag("AtRandom") || origin != ZoneSet::of(Zone::Graveyard) || !p.flag("Hidden") || !p.flag("Mandatory") || p.peek("ValidTgts").is_some() || p.peek("Defined").is_some() {
            return Err("ChangeZone AtRandom form".into());
        }
        if !matches!(dest, Zone::Exile | Zone::Hand) {
            return Err("ChangeZone AtRandom destination".into());
        }
        if p.get("DefinedPlayer").unwrap_or("You") != "You" {
            return Err("ChangeZone AtRandom DefinedPlayer".into());
        }
        let valid = self.filter_f(f, p.get("ChangeType").unwrap_or("Card"))?;
        let n = p.get("ChangeNum").unwrap_or("1").parse::<u8>().map_err(|_| "ChangeZone AtRandom ChangeNum")?;
        Ok(Some(Effect::Ext(crate::effect_ext::ExtEffect::Mr(MrEffect::RandomFromGraveyard { valid, n, dest }))))
    }
}

/// `Mode$ Continuous | Affected$ <filter> | AddKeyword$ Ward:<cost>` (ward granted to a filtered
/// set of permanents, Hexing Squelcher): the static without the ward keyword, and the ward cost.
/// The ward becomes a trigger of the source matching the affected permanents (as for Equipment
/// and Auras; CR 702.21a). `None` for other statics.
pub fn split_granted_ward(s: &forge_script::Params) -> Option<(forge_script::Params, String)> {
    if s.get("Mode") != Some("Continuous") || s.get("AffectedDefined").is_some() || s.get("AffectedZone").is_some_and(|z| z != "Battlefield") {
        return None;
    }
    let aff = s.get("Affected")?;
    if aff.ends_with(".EnchantedBy") || aff.ends_with(".EquippedBy") || aff.contains("EnchantedBy") || aff.contains("EquippedBy") {
        return None;
    }
    let kws = s.get("AddKeyword")?;
    let parts: Vec<&str> = kws.split('&').map(|w| w.trim()).collect();
    let wards: Vec<&str> = parts.iter().filter_map(|w| w.strip_prefix("Ward:")).collect();
    if wards.len() != 1 {
        return None;
    }
    let others: Vec<&str> = parts.iter().copied().filter(|w| !w.starts_with("Ward:")).collect();
    let mut rest = forge_script::Params { pairs: vec![] };
    for (k, v) in &s.pairs {
        if k == "AddKeyword" {
            if !others.is_empty() {
                rest.pairs.push((k.clone(), others.join(" & ")));
            }
        } else {
            rest.pairs.push((k.clone(), v.clone()));
        }
    }
    Some((rest, wards[0].to_string()))
}

impl CardDb {
    /// Effect APIs of this batch (`None`: not one of them; keys untouched).
    pub(crate) fn mr_api(&mut self, api: &str, p: &crate::compile::P, _f: &forge_script::Face) -> Result<Option<Effect>, String> {
        Ok(Some(Effect::Ext(crate::effect_ext::ExtEffect::Mr(match api {
            "Play" if p.peek("Valid") == Some("Card.IsImprinted") && p.peek("CopyCard").is_none() => {
                p.ack("Valid", &["Card.IsImprinted"], "checked above");
                if p.get("ValidSA") != Some("Spell") || p.get("ValidZone") != Some("Exile") || p.get("Controller").is_some_and(|c| c != "You") || !p.flag("Optional") {
                    return Err("Play imprinted form".into());
                }
                if p.peek("WithoutManaCost").is_some() || p.get("Amount").is_some_and(|a| a != "All" && a != "1") {
                    return Err("Play imprinted form".into());
                }
                p.ack("ShowCardToActivator", &["True"], "the card is exiled face up anyway");
                MrEffect::MayCastImprinted { remember: p.flag("RememberPlayed") }
            }
            _ => return Ok(None),
        }))))
    }
}

/// Card-database side tables of this batch (`CardDb::mr`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MrTables {
    /// Some loaded card has a "nonbasic lands are [basic type]" static: the counter kind its
    /// condition reads (`MrStatic::LandsBecome`).
    pub lands_become: Option<CounterKind>,
    /// `Dig | Imprint$ True`: the cards taken are imprinted (`Exec::imprinted`).
    pub dig_imprint: Vec<AbilityId>,
}

/// The players a player-affecting node of this batch applies to: its `Defined$` players, else
/// its controller (Forge's default `Defined$ You`).
fn affected_players(ex: &crate::resolve::Exec, who: &[Target]) -> SmallVec<[PlayerId; 2]> {
    let ps = crate::resolve::players(who);
    if ps.is_empty() {
        smallvec::smallvec![ex.you]
    } else {
        ps
    }
}

/// `Mode$ Continuous | AffectedDefined$ Self | AddStaticAbility$ S` where `S` is itself
/// `Mode$ Continuous | AffectedDefined$ Self | AddKeyword$ K` (Burnout Bashtronaut's "Max speed —
/// This creature has double strike"; CR 702.179f: a max speed ability is a static ability
/// that exists only while you have max speed). The permanent granting itself a static that grants
/// itself keywords is the keyword under the outer condition: rewritten as `AddKeyword$ K`.
pub fn flatten_self_static(s: &forge_script::Params, f: &forge_script::Face) -> Option<forge_script::Params> {
    if s.get("Mode") != Some("Continuous") || s.get("AffectedDefined") != Some("Self") || s.get("AddKeyword").is_some() {
        return None;
    }
    let inner = forge_script::Params::parse(&f.svar(s.get("AddStaticAbility")?)?.value);
    let ok_keys = inner.pairs.iter().all(|(k, _)| matches!(k.as_str(), "Mode" | "AffectedDefined" | "AddKeyword" | "Secondary" | "Description"));
    if !ok_keys || inner.get("Mode") != Some("Continuous") || inner.get("AffectedDefined") != Some("Self") {
        return None;
    }
    let kw = inner.get("AddKeyword")?;
    let mut out = forge_script::Params { pairs: vec![] };
    for (k, v) in &s.pairs {
        if k == "AddStaticAbility" {
            out.pairs.push(("AddKeyword".into(), kw.to_string()));
        } else {
            out.pairs.push((k.clone(), v.clone()));
        }
    }
    Some(out)
}

/// (mana mask, amount) of a variable-amount mana ability of `o` for `p` (`cast::mana_sources`);
/// mask 0 (no source) when it would produce no mana.
pub fn mana_count(s: &State, db: &CardDb, o: ObjId, p: PlayerId, m: &MrEffect) -> Option<(u8, u8)> {
    let MrEffect::ManaCount { t, n } = m else { return None };
    let ecx = crate::expr::EvalCtx { s, db, you: p, source: o, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &[], imprinted: &[], base_only: false };
    // Past 255 the amount isn't representable (pool slots are u8): activating it fails loudly
    // (`mana_count_unrepresentable`).
    let k = crate::expr::eval(n, &ecx).clamp(0, 255) as u8;
    Some(if k == 0 { (0, 0) } else { (1 << *t as u8, k) })
}

/// `Some(amount)` when mana ability `a` of `o` is a variable amount beyond what a mana pool slot
/// holds (`u8`; `mana_count` clamps it): the activation must fail loudly instead.
pub fn mana_count_unrepresentable(s: &State, db: &CardDb, o: ObjId, p: PlayerId, a: AbilityId) -> Option<i32> {
    let Effect::Ext(crate::effect_ext::ExtEffect::Mr(MrEffect::ManaCount { n, .. })) = &db.ability(a).effect else { return None };
    let ecx = crate::expr::EvalCtx { s, db, you: p, source: o, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &[], imprinted: &[], base_only: false };
    let k = crate::expr::eval(n, &ecx);
    (k > u8::MAX as i32).then_some(k)
}

/// Move the creatures in `cands` that must attack this combat to the front (keeping the
/// canonical order within each group); returns how many there are.
pub fn must_attack_first(s: &State, db: &CardDb, cands: &mut [ObjId]) -> usize {
    if s.ext.mr.must_attack.is_empty() && s.ext.x6.must_attack.is_empty() {
        return 0;
    }
    let turn = s.turn.number;
    // This combat, or each combat (`ext6`: an ability granted until it leaves).
    let must = |o: &ObjId| s.ext.mr.must_attack.contains(&(*o, turn)) || s.ext.x6.must_attack.iter().any(|x| x.0 == *o && !crate::query::grant_removed(s, db, *o, x.1));
    cands.sort_by_key(|o| !must(o)); // stable
    cands.iter().filter(|o| must(o)).count()
}

/// `Effect | StaticAbilities$ S | RememberObjects$ ... | Duration$ UntilEndOfCombat` where `S` is
/// `Mode$ MustAttack | ValidCreature$ Card.IsRemembered` ("that token attacks this combat if able").
pub(crate) fn is_must_attack_effect(p: &crate::compile::P, f: &forge_script::Face) -> bool {
    let Some(st) = p.peek("StaticAbilities").and_then(|n| f.svar(n.trim())) else { return false };
    let sp = forge_script::Params::parse(&st.value);
    sp.get("Mode") == Some("MustAttack")
}

impl CardDb {
    pub(crate) fn mr_mana_count(&mut self, p: &crate::compile::P, f: &forge_script::Face) -> Result<Effect, String> {
        let t = match p.get("Produced").unwrap_or("") {
            "C" => ManaType::C,
            c if c.len() == 1 => Color::from_char(c.chars().next().unwrap()).map(ManaType::of).ok_or_else(|| format!("Mana Produced {c}"))?,
            c => return Err(format!("Mana Produced {c} with a variable amount")),
        };
        let n = self.value(f, p.get("Amount").unwrap())?;
        if p.peek("RestrictValid").is_some() {
            return Err("Mana with a variable amount and a restriction".into());
        }
        Ok(Effect::Ext(crate::effect_ext::ExtEffect::Mr(MrEffect::ManaCount { t, n })))
    }

    pub(crate) fn mr_must_attack(&mut self, p: &crate::compile::P) -> Result<Effect, String> {
        let names = p.get("StaticAbilities").unwrap();
        if names.contains(',') || p.get("Duration") != Some("UntilEndOfCombat") || !matches!(p.get("RememberObjects"), Some("Remembered") | Some("RememberedCard")) {
            return Err("Effect MustAttack form".into());
        }
        Ok(Effect::Ext(crate::effect_ext::ExtEffect::Mr(MrEffect::MustAttackThisCombat)))
    }
}

impl CardDb {
    /// `Dig | DigNum$ N | ChangeNum$ All | DestinationZone$ Exile | ExileFaceDown$ True |
    /// WithMayLook$ True`.
    pub(crate) fn mr_dig_face_down_look(&mut self, p: &crate::compile::P, f: &forge_script::Face) -> Result<Effect, String> {
        if !p.flag("ExileFaceDown") || !p.flag("WithMayLook") || p.get("ChangeNum") != Some("All") || p.get("DestinationZone") != Some("Exile") {
            return Err("Dig ExileFaceDown WithMayLook form".into());
        }
        if p.peek("Defined").is_some_and(|d| d != "You") || p.peek("ValidTgts").is_some_and(|t| t != "Opponent" && t != "Player") {
            return Err("Dig ExileFaceDown WithMayLook for other players".into());
        }
        p.ack("Defined", &["You"], "checked above");
        let n = self.value(f, p.get("DigNum").ok_or("Dig without DigNum")?)?;
        Ok(Effect::Ext(crate::effect_ext::ExtEffect::Mr(MrEffect::ExileTopFaceDownLook { n })))
    }
}

/// The active "nonbasic lands are [type]" static, if any: the WUBRG index of the basic type.
/// Its condition is read directly (a counter on its source), so this never evaluates filters or
/// characteristics of other objects (it runs inside layer 4).
fn lands_become_active(s: &State, db: &CardDb) -> Option<u8> {
    let counter = db.mr.lands_become?;
    crate::query::memo_lands_become(s, db, || lands_become_scan(s, db, counter))
}

fn lands_become_scan(s: &State, db: &CardDb, counter: CounterKind) -> Option<u8> {
    for &src in &s.battlefield {
        let so = s.obj(src);
        // the static applies only with the counter on its source: skip the static scan of every
        // permanent without one (same result: the loop below can only return for such sources)
        if so.counter(counter) < 1 {
            continue;
        }
        for st in &db.get(so.card).face(so.face).statics {
            if let StaticKind::Ext(crate::static_ext::StaticExt::Mr(MrStatic::LandsBecome { counter: k, basic })) = &st.kind {
                // A type-changing effect applies in layer 4, before any ability loss (layer 6,
                // the only kind there is: effect objects): its source losing its abilities
                // doesn't stop it (CR 613.1, 613.6; Magus of the Moon ruling 2021-03-19: "it
                // continues to turn nonbasic lands into Mountains").
                if *k == counter && so.counter(counter) >= 1 {
                    return Some(*basic);
                }
            }
        }
    }
    None
}

/// Is `o` a nonbasic land on the battlefield by its printed (layer 1) characteristics?
fn printed_nonbasic_land(s: &State, db: &CardDb, o: ObjId) -> bool {
    let ob = s.obj(o);
    let f = db.get(ob.card).face(ob.face);
    ob.zone == Zone::Battlefield && f.types.has(CardType::Land) && !f.supertypes.basic()
}

/// Has the land `o` lost the abilities from its rules text (CR 305.7)?
pub fn rules_text_lost(s: &State, db: &CardDb, o: ObjId) -> bool {
    printed_nonbasic_land(s, db, o) && lands_become_active(s, db).is_some()
}

/// Layer 4 (and the rules-text part of layer 6) for a land under "nonbasic lands are
/// Mountains" (CR 305.7): it loses its other land types and its rules-text abilities, and has
/// the basic type's intrinsic mana ability (CR 305.6). Abilities granted by other effects are
/// added later by `query::chars` and stay.
pub fn land_type_override(s: &State, db: &CardDb, id: ObjId, c: &mut crate::query::Chars) {
    if !printed_nonbasic_land(s, db, id) {
        return;
    }
    let Some(basic) = lands_become_active(s, db) else { return };
    let names = ["Plains", "Island", "Swamp", "Mountain", "Forest"];
    let Some(st) = db.subtype(names[basic as usize]) else { return };
    c.subtypes.retain(|x| !db.land_types.contains(x));
    c.subtypes.push(st);
    c.abilities.clear();
    if let Some(&a) = db.basic_land_mana.get(basic as usize) {
        c.abilities.push(a);
    }
    c.kws = KwSet::default();
    c.lost_abilities = true;
}

impl CardDb {
    /// Static modes of this batch (`None`: not one of them; keys untouched).
    pub(crate) fn mr_static(&mut self, mode: &str, p: &crate::compile::P, _f: &forge_script::Face) -> Result<Option<StaticKind>, String> {
        Ok(Some(StaticKind::Ext(crate::static_ext::StaticExt::Mr(match mode {
            "Continuous" if p.peek("RemoveLandTypes").is_some() && p.peek("Affected") == Some("Land.nonBasic") => {
                let names = ["Plains", "Island", "Swamp", "Mountain", "Forest"];
                let basic = p.get("AddType").and_then(|t| names.iter().position(|n| *n == t)).ok_or("lands become: AddType")? as u8;
                p.ack("Affected", &["Land.nonBasic"], "checked above");
                if !p.flag("RemoveLandTypes") {
                    return Err("lands become: RemoveLandTypes".into());
                }
                // Only "as long as [this] has a [kind] counter on it".
                let cond = p.get("IsPresent").ok_or("lands become without its condition")?;
                let kind = cond.strip_prefix("Card.Self+counters_GE1_").ok_or_else(|| format!("lands become condition {cond}"))?;
                if p.peek("PresentCompare").is_some() || p.peek("CheckSVar").is_some() || p.peek("Condition").is_some() {
                    return Err("lands become: other conditions".into());
                }
                let counter = self.counter_kind(kind);
                if self.mr.lands_become.is_some_and(|k| k != counter) {
                    return Err("lands become: two different counter kinds".into());
                }
                self.mr.lands_become = Some(counter);
                self.ensure_basic_land_mana();
                MrStatic::LandsBecome { counter, basic }
            }
            _ => return Ok(None),
        }))))
    }
}
