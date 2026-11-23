//! Long-tail mechanics, sixth batch:
//! storm granted to spells on the stack, with copies whose controller may choose new targets
//! (Ral, Crackling Wit's emblem; CR 702.40, 707.10); "[-X]" loyalty costs, pool mana with a
//! spending restriction (CR 106.6) and "transforms into" triggers (Chandra, Chill of
//! Compliance; Ashling); manifest dread and face-down permanents (CR 701.62, 701.40, 708;
//! Turn Inside Out, Unidentified Hovership). Kept in its own module to stay additive (like
//! `ext2`..`ext5`).
//!
//! Face-down permanents: the object's `card` is a synthetic vanilla 2/2 creature card with no
//! name, no subtypes, no mana cost and no abilities (`XTables::face_down_card`, CR 708.2a), so
//! every rules path (abilities, triggers, characteristics, events, copies, LKI) sees exactly the
//! face-down values without special cases. The real card is kept in `XState::face_down`;
//! `known_to` holds who may look at it (its controller, CR 708.5). It is restored as the
//! permanent leaves the battlefield (revealed, 708.9) or is turned face up (701.40b).

use crate::basics::*;
use crate::carddb::*;
use crate::engine::*;
use crate::filter::Filter;
use crate::query::*;
use crate::state::*;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// Per-game state of this batch (`Ext::x6`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct XState {
    /// Spells with a storm trigger this turn: the spell as it became cast (CR 601.2i), which
    /// the storm copies copy even if it has left the stack by then (last known information;
    /// XMage `StormEffect` keeps the spell). Cleared as each turn begins.
    pub storm: SmallVec<[StormSnap; 0]>,
    /// Mana in a pool with a spending restriction (CR 106.6): (player, type, amount,
    /// restriction). Empties with the pool (CR 106.4, 500.4).
    pub rpool: SmallVec<[(PlayerId, ManaType, u8, Restr); 0]>,
    /// Face-down permanents (CR 708): (object, its real card). Hidden from players who don't
    /// know it (`Obj::known_to`); see the module docs.
    pub face_down: SmallVec<[(ObjId, CardId); 0]>,
    /// Set by `manifest` around its move: the next object entering the battlefield enters face
    /// down, (who may look at it, the face-down card).
    pub face_down_next: Option<(PlayerId, CardId)>,
    /// Triggered abilities that resolved this turn: (source, ability, times), and how many
    /// times the one resolving now has, counting this one (`Count$ResolvedThisTurn`).
    pub resolved: SmallVec<[(ObjId, AbilityId, u8); 2]>,
    pub resolving_count: u8,
    /// Counters last seen on permanents with "when the Nth [kind] counter is put on this"
    /// triggers: (permanent, kind, count) (`counter_watch`).
    pub counter_seen: SmallVec<[(ObjId, CounterKind, i32); 0]>,
    /// Class levels (CR 716.2b: a designation; 1 when absent, 716.2d): (permanent, level).
    pub levels: SmallVec<[(ObjId, u8); 0]>,
    /// Cards each player discarded since the last batch was announced (`flush_discards`).
    pub discards: [u8; 2],
    /// Cards discarded this turn, as the objects they became (mayhem, CR 702.187b).
    pub discarded_this_turn: SmallVec<[ObjId; 2]>,
    /// Permanents that "attack each combat if able" (an ability granted until they leave), with
    /// the granting effect's timestamp (CR 613.7b; a later loss of all abilities removes it).
    pub must_attack: SmallVec<[(ObjId, u32); 0]>,
    /// Triggered abilities granted to permanents until they leave the battlefield (CR 613.1f,
    /// 611.2c): (permanent, index into `CardDb::triggers_pool`, timestamp of the granting
    /// effect, CR 613.7b: a later "loses all abilities" removes it, `query::grant_removed`).
    pub granted: SmallVec<[(ObjId, u32, u32); 0]>,
    /// Keyword counters' timestamps (CR 613.7c: a counter gets one as it's put on an object, and
    /// all counters of that kind on it get the new one): (permanent, kind, timestamp), for kinds
    /// that are keywords (CR 122.1b) only. Layer 6 orders them against "loses all abilities"
    /// (`query::chars`). Kept up to date by `put_counters`.
    #[serde(default)]
    pub kw_counter_ts: SmallVec<[(ObjId, CounterKind, u32); 0]>,
    /// Objects targeted by the spell or ability whose costs are being paid: set from its
    /// targets' choice (CR 601.2c, 602.2b) until it is on the stack. A target isn't
    /// interchangeable with an identical object for that spell's own cost choices (blight or
    /// sacrifice the target or another one, 601.2h), so `query::refs_of` keeps it apart.
    #[serde(default)]
    pub paying_for: SmallVec<[ObjId; 0]>,
    /// Cards exiled to pay a craft cost, by the exiled crafting card, until it returns
    /// (`ReturnTransformed`): (crafting card in exile, materials in exile).
    pub craft_pending: SmallVec<[(ObjId, SmallVec<[ObjId; 4]>); 0]>,
    /// Permanents that have the activated abilities of the cards exiled to craft them (CR
    /// 702.167; Locus of Enlightenment): (permanent, materials in exile).
    pub crafted: SmallVec<[(ObjId, SmallVec<[ObjId; 4]>); 0]>,
    /// Uses this turn of such gained abilities: (permanent, ability, times). Cleared each turn.
    pub gained_used: SmallVec<[(ObjId, AbilityId, u8); 0]>,
    /// Activated abilities put on the stack, as activated (for "copy it", CR 707.10): (key,
    /// stack item). Cleared each turn.
    pub activated: SmallVec<[(u32, StackItem); 0]>,
    pub next_activated: u32,
    /// Turns each player will skip (CR 614.10: "skip your next N turns").
    pub skip_turns: [u8; 2],
    /// Context of the set choice being made (observation only, never read by the rules; set
    /// while a decision is pending, cleared once the choice is complete): `Observation::pick`.
    pub pick: Pick,
    /// Players (bitmask) whose mana decisions have full fidelity (`cast::full_mana`): set by a
    /// scenario's `rules_options.mana_abilities_at_priority` or for a human seat; 0 for agent
    /// seats and in search.
    #[serde(default)]
    pub full_mana: u8,
    /// Players (bitmask) offered every activatable mana ability at priority without the agent
    /// mana filter, pool spending still automatic (decision-count measurements).
    #[serde(default)]
    pub mana_unfiltered: u8,
    /// Players (bitmask) offered no mana ability at priority: scenario positions without
    /// `rules_options.mana_abilities_at_priority`, so both engines pose the same decisions.
    #[serde(default)]
    pub no_priority_mana: u8,
}

/// What is already chosen in the choice being made (CR 601.2c, 602.2b: targets are chosen one
/// targeted part at a time). Only the paused (observed) state carries a non-empty `Pick`: it is
/// set right before a decision is surfaced and cleared once the choice completes, so committed
/// states never differ by it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pick {
    /// Targeted part being chosen (1-based; 0 = not choosing targets) and the number of
    /// targeted parts of the spell / ability.
    pub part: u8,
    pub parts: u8,
    /// Targets chosen for its earlier targeted parts.
    pub earlier: SmallVec<[Target; 2]>,
    /// Items already picked in the current set choice (`Ctx::choose_set`): targets, cards.
    pub chosen: SmallVec<[Target; 2]>,
}

impl XState {
    /// As each turn begins (CR 500).
    pub fn new_turn(&mut self) {
        self.storm.clear();
        self.resolved.clear();
        self.discarded_this_turn.clear();
        self.gained_used.clear();
        self.activated.clear();
    }
}

/// A spell with storm as it became cast, and what its copies copy (CR 707.10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StormSnap {
    pub spell: ObjId,
    pub item: StackItem,
    /// Spells cast before it this turn, by any player (CR 702.40a).
    pub before: u8,
    /// Casting choices kept by copies: alternative/additional costs paid (`CastInfo` flags,
    /// not the mana spent: a copy wasn't cast, so no mana was spent on it).
    pub flags: u16,
    /// Division of damage/counters among the targets (CR 601.2d), by target slot.
    pub divided: SmallVec<[(Target, i32); 2]>,
    /// The permanent sacrificed to pay its costs (CR 707.10: an effect of the copy that refers
    /// to objects used to pay its costs uses those of the original).
    pub sacrificed: SmallVec<[ObjId; 1]>,
}

/// Card-database side tables of this batch (`CardDb::x6`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct XTables {
    /// The storm triggered ability (CR 702.40a), synthesized once a loaded card grants storm.
    pub storm_ability: Option<AbilityId>,
    /// Spending restrictions of pool mana ("spend this mana only to cast [matching] spells"),
    /// as filters on the spell.
    pub restricts: Vec<Filter>,
    /// What each restriction allows, for the text view ("noncreature spells").
    pub restrict_text: Vec<String>,
    /// Some loaded card has a "whenever this transforms into" trigger.
    pub any_transformed_trigger: bool,
    /// The face-down 2/2 creature card (CR 708.2a), synthesized once a loaded card can put a
    /// permanent onto the battlefield face down.
    pub face_down_card: Option<CardId>,
    /// Some loaded card has a "when the Nth counter is put on this" trigger.
    pub any_counter_added: bool,
    /// Some loaded card has a "whenever you discard" trigger.
    pub any_discard_trigger: bool,
    /// Mana abilities whose mana may be spent only to activate abilities (`RestrictValid$
    /// Activated`, CR 106.6; The Enigma Jewel).
    pub ability_mana: Vec<AbilityId>,
    /// Some loaded card has a "whenever you activate an ability that isn't a mana ability"
    /// trigger.
    pub any_activation_trigger: bool,
}

/// The spending restriction of pool mana (CR 106.6).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Restr {
    /// "Spend this mana only to cast [matching] spells": `XTables::restricts[i]`, a filter on
    /// the spell that doesn't depend on the mana's source (resolving effects: Chandra, Ashling).
    Spell(u16),
    /// The restriction of the mana ability `.0` (`CardDb::mana_restrict`: a filter on the spell,
    /// evaluated with the ability's source `.1`, e.g. Cavern of Souls' chosen type, with its
    /// "can't be countered" flag): mana a mana ability put into the pool (left over from a payment, or at priority).
    Ability(AbilityId, ObjId),
    /// "Spend this mana only to activate abilities" (`XTables::ability_mana`, The Enigma Jewel).
    Activations,
}

impl Restr {
    /// May this mana be spent on the payment `sel` (`Some(spell)`, `Some(ABILITY_PAYMENT)` for an
    /// activated ability's cost, `None` for any other cost)?
    pub fn allows(self, s: &State, db: &CardDb, p: PlayerId, sel: Option<ObjId>) -> bool {
        let spell = sel.filter(|&x| x != ABILITY_PAYMENT && x != crate::cast::POOL_OBJ);
        match self {
            Restr::Activations => sel == Some(ABILITY_PAYMENT),
            Restr::Spell(r) => spell.is_some_and(|sp| obj_matches(s, db, &db.x6.restricts[r as usize], sp, FilterCtx { you: p, source: None, x: 0, rem: &[] })),
            Restr::Ability(a, src) => spell.is_some_and(|sp| db.mana_restrict.iter().find(|r| r.0 == a).is_some_and(|r| obj_matches(s, db, &r.1, sp, FilterCtx { you: p, source: Some(src), x: 0, rem: &[] }))),
        }
    }
    /// Spending it on a spell makes that spell uncounterable (Cavern of Souls, CR 106.6).
    pub fn nocounter(self, db: &CardDb) -> bool {
        matches!(self, Restr::Ability(a, _) if db.mana_restrict.iter().any(|r| r.0 == a && r.2))
    }
    /// Code for `Opt::Mode` when the payer chooses which restricted mana to spend
    /// (`spend_pool`): `Spell(i)` is `i` (the text view names `XTables::restrict_text[i]`).
    fn mode_code(self) -> u8 {
        match self {
            Restr::Spell(r) => r as u8,
            Restr::Ability(..) => 254,
            Restr::Activations => 255,
        }
    }
}

/// The spending restriction of the mana that mana ability `a` of `src` produces, if any.
pub fn ability_restr(db: &CardDb, a: AbilityId, src: ObjId) -> Option<Restr> {
    if !db.x6.ability_mana.is_empty() && db.x6.ability_mana.contains(&a) {
        return Some(Restr::Activations);
    }
    db.mana_restrict.iter().any(|r| r.0 == a).then_some(Restr::Ability(a, src))
}

/// Add `n` mana of type `t` with restriction `r` (None: unrestricted) to `p`'s pool (CR 106.4).
pub fn add_pool(s: &mut State, p: PlayerId, t: ManaType, n: u8, r: Option<Restr>) {
    if n == 0 {
        return;
    }
    match r {
        None => {
            let slot = &mut s.ext.pool[p.idx()][t as usize];
            *slot = slot.saturating_add(n);
        }
        Some(r) => match s.ext.x6.rpool.iter_mut().find(|x| x.0 == p && x.1 == t && x.3 == r) {
            Some(x) => x.2 = x.2.saturating_add(n),
            None => s.ext.x6.rpool.push((p, t, n, r)),
        },
    }
}

/// Mana-source selector for paying the cost of an activated ability (`cast::mana_sources_for`):
/// mana restricted to activating abilities counts, mana restricted to spells doesn't.
pub const ABILITY_PAYMENT: ObjId = ObjId(u32::MAX - 1);

/// Expressions of this batch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum XExpr {
    /// Times the resolving triggered ability has resolved this turn, this time included
    /// (`Count$ResolvedThisTurn`, Belladonna Took).
    ResolvedThisTurn,
    /// 1 if this spell's sneak cost was paid (`Count$ValidStack Card.Self+sneaked`, CR
    /// 702.190; The Last Ronin's Technique), else 0.
    SelfSneaked,
    /// `/DivideEvenlyDown.N`: the value divided by N, rounded down.
    DivDown(Box<crate::expr::Expr>, i32),
    /// This Class's level (CR 716.2d: 1 if it has none).
    ClassLevel,
}

pub fn compile_count(body: &str) -> Option<crate::expr::Expr> {
    let x = match body {
        "ResolvedThisTurn" => XExpr::ResolvedThisTurn,
        "ValidStack Card.Self+sneaked" => XExpr::SelfSneaked,
        _ => return None,
    };
    Some(crate::expr::Expr::Ext(Box::new(crate::expr_ext::ExtExpr::X(x))))
}

pub fn eval_expr(e: &XExpr, c: &crate::expr::EvalCtx) -> i32 {
    match e {
        XExpr::ResolvedThisTurn => c.s.ext.x6.resolving_count as i32,
        XExpr::SelfSneaked => c.s.ext.lt.sneak.iter().any(|x| x.0 == c.source) as i32,
        XExpr::DivDown(e, n) => crate::num::div_floor(crate::expr::eval(e, c), *n),
        XExpr::ClassLevel => level_of(c.s, c.source) as i32,
    }
}

/// A triggered ability starts resolving (CR 608.2): count it for `Count$ResolvedThisTurn`.
pub fn note_resolving(s: &mut State, item: &StackItem) {
    if item.kind != StackKind::Triggered {
        return;
    }
    let r = &mut s.ext.x6.resolved;
    let n = match r.iter_mut().find(|x| x.0 == item.source && x.1 == item.ability) {
        Some(x) => {
            x.2 = x.2.saturating_add(1);
            x.2
        }
        None => {
            r.push((item.source, item.ability, 1));
            1
        }
    };
    s.ext.x6.resolving_count = n;
}

/// "When the Nth [kind] counter is put on this permanent" (Political Triumph; Forge
/// `CounterAdded | ValidCard$ Card.Self | CounterAmount$ EQN`): counters may be put on a
/// permanent by many effects, so the counts are compared with the last ones seen each time the
/// engine is about to check state-based actions and triggers (after every action/resolution);
/// a count that reached N from below triggers (CR 122.6: entering with counters counts too).
pub fn counter_watch(cx: &mut Ctx) {
    if !cx.db.x6.any_counter_added {
        return;
    }
    let mut events: SmallVec<[(ObjId, CounterKind, i32, i32); 1]> = SmallVec::new();
    let mut seen: SmallVec<[(ObjId, CounterKind, i32); 0]> = SmallVec::new();
    for &o in &cx.s.battlefield {
        let ob = cx.s.obj(o);
        for t in &cx.db.get(ob.card).face(ob.face).triggers {
            if let TrigMode::Ext(crate::trig_ext::TrigExt::CounterAddedSelf { kind, .. }) = &t.mode {
                let now = ob.counter(*kind);
                let before = cx.s.ext.x6.counter_seen.iter().find(|x| x.0 == o && x.1 == *kind).map_or(0, |x| x.2);
                if now > before {
                    events.push((o, *kind, before, now));
                }
                if !seen.iter().any(|x| x.0 == o && x.1 == *kind) {
                    seen.push((o, *kind, now));
                }
            }
        }
    }
    cx.s.ext.x6.counter_seen = seen;
    for (obj, kind, before, after) in events {
        crate::triggers::fire(cx.s, cx.db, &crate::triggers::TrigEvent::Ext(crate::trig_ext::ExtEvent::CounterAdded { obj, kind, before, after }));
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum XStatic {
    /// "[Matching] spells you cast have storm" (Ral, Crackling Wit's emblem; Forge `Mode$
    /// Continuous | AffectedZone$ Stack | AddKeyword$ Storm`): each matching spell gets one
    /// instance of storm as it is cast (CR 702.40a–b; storm triggers on casting, so having it on
    /// the stack as the spell becomes cast is what matters).
    GrantStorm { affected: Filter },
    /// "This has each activated ability of the exiled cards used to craft it. You may activate
    /// each of those abilities only once each turn." (Locus of Enlightenment; Forge
    /// `GainsAbilitiesOfDefined$ ExiledWith | GainsAbilitiesLimitPerTurn$ 1`.)
    GainsCraftedAbilities,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum XEffect {
    /// The storm trigger: copy the triggering spell once for each other spell cast before it
    /// this turn; the controller may choose new targets for each copy (CR 702.40a, 707.10c).
    Storm,
    /// Add mana with a spending restriction to the controller's pool (CR 106.4, 106.6): the
    /// fixed `types` (Chandra, Chill of Compliance's "+1: Add {U}. Spend this mana only to cast
    /// a noncreature spell"; a loyalty ability, so not a mana ability, CR 605.1a), or
    /// `one_color` mana of a single color the controller chooses as it resolves ("add two mana
    /// of any one color", Ashling, Rimebound). `restrict` indexes `XTables::restricts`.
    AddMana { types: Vec<ManaType>, one_color: u8, restrict: Option<u16> },
    /// "Manifest dread" (CR 701.62a) by `who`.
    ManifestDread { who: DreadWho },
    /// "When [the targeted creature] dies this turn, [execute]" (Turn Inside Out; Forge
    /// `DelayedTrigger | Mode$ ChangesZone | RememberObjects$ Targeted | Origin$ Battlefield |
    /// Destination$ Graveyard | ThisTurn$ True`): a delayed trigger per object the previous
    /// targeted node targeted (CR 603.7), ending with the turn.
    DelayedDies { execute: AbilityId },
    /// "This Class's level becomes `level`" (CR 716.2a).
    ClassLevel { level: u8 },
    /// The defined permanents gain, until they leave the battlefield, "attacks each combat if
    /// able" and/or triggered abilities (Carnage, Crimson Chaos; Forge `Animate | Duration$
    /// Permanent | staticAbilities$ <MustAttack> | Triggers$ ...`).
    GrantAbilities { must_attack: bool, triggers: Vec<u32> },
    /// "Copy it. You may choose new targets for the copy." for the activated ability that
    /// triggered this (CR 707.10, 707.10c; Locus of Enlightenment).
    CopyActivated,
    /// "Copy target [instant or sorcery] spell you control. You may choose new targets for the
    /// copy." (Kitsa, Otterball Elite; CR 707.10c).
    CopyTargetSpell,
    /// "Flip `n` coins. Target opponent skips their next X turns, where X is the number of
    /// coins that came up heads." (Ral Zarek, Guest Lecturer; CR 705.2: no call, 614.10).
    FlipCoinsSkipTurns { n: u8 },
}

/// Who manifests dread.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DreadWho {
    You,
    /// "The exiled card's owner" (Forge `DefinedPlayer$ RememberedOwner`, Unidentified
    /// Hovership): owners of the cards this ability's source exiled (`ext.exiled_with`).
    ExiledCardOwner,
}

impl CardDb {
    /// Effect APIs of this batch (`None`: not one of them; keys untouched).
    pub(crate) fn x_api(&mut self, api: &str, p: &crate::compile::P, f: &forge_script::Face, uns: &mut Vec<String>, depth: u32) -> Result<Option<Effect>, String> {
        Ok(Some(Effect::Ext(crate::effect_ext::ExtEffect::X(match api {
            "CopySpellAbility" if p.peek("ValidTgts").is_some() && p.peek("TargetType") == Some("Spell") && p.peek("Defined").is_none() => {
                if !p.flag("MayChooseTarget") || p.peek("Amount").is_some() {
                    return Err("CopySpellAbility target form".into());
                }
                XEffect::CopyTargetSpell
            }
            "CopySpellAbility" if p.peek("Defined") == Some("TriggeredSpellAbility") && p.peek("MayChooseTarget").is_some() && self.le.trig_mode == "AbilityCast" => {
                p.ack("Defined", &["TriggeredSpellAbility"], "checked above");
                if !p.flag("MayChooseTarget") || p.peek("Amount").is_some() {
                    return Err("CopySpellAbility form".into());
                }
                XEffect::CopyActivated
            }
            "FlipCoin" => {
                let n = p.get("Amount").and_then(|a| a.parse::<u8>().ok()).ok_or("FlipCoin Amount")?;
                let heads = p.get("HeadsSubAbility").and_then(|h| f.svar(h)).map(|sv| forge_script::Params::parse(&sv.value));
                let ok = p.flag("NoCall")
                    && heads.as_ref().is_some_and(|h| h.get("DB") == Some("SkipTurn") && h.get("NumTurns") == Some("X") && h.get("Defined") == Some("Targeted") && h.pairs.len() == 3)
                    && p.peek("ValidTgts") == Some("Opponent");
                if !ok {
                    return Err("FlipCoin form".into());
                }
                XEffect::FlipCoinsSkipTurns { n }
            }
            "ManifestDread" => {
                let who = match p.get("DefinedPlayer") {
                    None | Some("You") => DreadWho::You,
                    Some("RememberedOwner") => DreadWho::ExiledCardOwner,
                    Some(d) => return Err(format!("ManifestDread DefinedPlayer {d}")),
                };
                self.ensure_face_down_card();
                XEffect::ManifestDread { who }
            }
            "DelayedTrigger" if p.peek("Mode") == Some("ChangesZone") && p.peek("RememberObjects") == Some("Targeted") => {
                let ok = p.ack("Mode", &["ChangesZone"], "checked above").is_some()
                    && p.ack("RememberObjects", &["Targeted"], "checked above").is_some()
                    && p.get("ValidCard") == Some("Card.IsTriggerRemembered")
                    && p.get("Origin") == Some("Battlefield")
                    && p.get("Destination") == Some("Graveyard")
                    && p.flag("ThisTurn");
                if !ok {
                    return Err("DelayedTrigger ChangesZone form".into());
                }
                let exec = p.get("Execute").ok_or("DelayedTrigger without Execute")?;
                let body = f.svar(exec).and_then(|s| s.as_ability()).ok_or("DelayedTrigger Execute missing")?;
                let execute = self.ability_inner(&body, f, uns, depth + 1, false)?;
                XEffect::DelayedDies { execute }
            }
            _ => return Ok(None),
        }))))
    }

    /// The face-down 2/2 creature card (CR 708.2a): no name (shown as "face-down creature"), no
    /// subtypes, no mana cost, colorless, no abilities.
    pub(crate) fn ensure_face_down_card(&mut self) -> CardId {
        if let Some(c) = self.x6.face_down_card {
            return c;
        }
        let face = FaceDef {
            name: "face-down creature".into(),
            cost: ManaCost { syms: SmallVec::new(), none: true },
            types: crate::basics::TypeSet::default().with(CardType::Creature),
            supertypes: Default::default(),
            subtypes: vec![],
            colors: Default::default(),
            power: Some(2),
            toughness: Some(2),
            loyalty: None,
            kws: Default::default(),
            spell: None,
            activated: vec![],
            triggers: vec![],
            statics: vec![],
            replacements: vec![],
            etb_counters: vec![],
            kicker: None,
            flashback: None,
            hand_abilities: vec![],
            grave_abilities: vec![],
            cda_pt: None,
            alt: Default::default(),
            enchant: None,
            opening_hand_battlefield: false,
            oracle: String::new(),
            extra: Default::default(),
        };
        let id = CardId(self.cards.len() as u32);
        self.cards.push(CardDef { name: face.name.clone(), layout: Layout::Normal, faces: vec![face], is_token: false, unsupported: vec![] });
        self.x6.face_down_card = Some(id);
        id
    }
}

/// The real card of face-down permanent `o` (None if it isn't one).
pub fn face_down_card(s: &State, o: ObjId) -> Option<CardId> {
    if s.ext.x6.face_down.is_empty() {
        return None;
    }
    s.ext.x6.face_down.iter().find(|x| x.0 == o).map(|x| x.1)
}

/// The identity of `o`: its real card if it is face down, else its card (for resampling
/// hidden identities: determinization, leak tests).
pub fn identity(s: &State, o: ObjId) -> CardId {
    face_down_card(s, o).unwrap_or(s.obj(o).card)
}

/// Replace the identity of `o` (see `identity`).
pub fn set_identity(s: &mut State, o: ObjId, c: CardId) {
    match s.ext.x6.face_down.iter_mut().find(|x| x.0 == o) {
        Some(x) => x.1 = c,
        None => s.obj_mut(o).card = c,
    }
}

/// Put library card `o` onto the battlefield face down under `p`'s control (manifest, CR
/// 701.40a): a 2/2 face-down creature only `p` may look at.
pub fn manifest(cx: &mut Ctx, p: PlayerId, o: ObjId) -> R<Option<ObjId>> {
    let Some(fd) = cx.db.x6.face_down_card else { return cx.unsupported("manifest without the face-down card") };
    cx.s.ext.x6.face_down_next = Some((p, fd));
    let r = crate::zones::move_batch(cx, &[crate::zones::Move { obj: o, to: Zone::Battlefield, to_top: false, tapped: false, controller: Some(p) }]);
    cx.s.ext.x6.face_down_next = None;
    Ok(r?[0])
}

/// Manifest dread (CR 701.62a): `p` looks at the top two cards of their library, manifests one
/// and puts the other into their graveyard.
pub fn manifest_dread(cx: &mut Ctx, p: PlayerId, source: ObjId) -> R<()> {
    let top: Vec<ObjId> = cx.s.player(p).library.iter().rev().take(2).copied().collect();
    let Some(&first) = top.first() else { return Ok(()) };
    for &o in &top {
        cx.s.obj_mut(o).known_to |= 1 << p.0; // the player looks at them
    }
    let pick = if top.len() == 1 {
        first
    } else {
        let items = crate::query::card_items(cx.s, cx.db, &top);
        match cx.choose_set(p, DecisionKind::Dig, items, 1, 1, Some(source))?.first() {
            Some(Opt::Card(o)) => *o,
            _ => first,
        }
    };
    manifest(cx, p, pick)?;
    let rest: Vec<crate::zones::Move> = top.iter().filter(|&&o| o != pick && cx.s.obj(o).zone == Zone::Library).map(|&o| crate::zones::Move::new(o, Zone::Graveyard)).collect();
    crate::zones::move_batch(cx, &rest)?;
    Ok(())
}

/// Special actions to turn face-down permanents `p` controls face up (CR 701.40b): any time
/// `p` has priority, if the card is a creature card with a mana cost `p` can pay.
pub fn face_up_actions(cx: &Ctx, p: PlayerId, out: &mut Vec<Opt>) {
    if cx.s.ext.x6.face_down.is_empty() {
        return;
    }
    let srcs = std::cell::OnceCell::new();
    for &(o, real) in &cx.s.ext.x6.face_down {
        let ob = cx.s.obj(o);
        if ob.zone != Zone::Battlefield || ob.controller != p {
            continue;
        }
        let f = cx.db.get(real).front();
        if !f.types.has(CardType::Creature) || f.cost.none {
            continue;
        }
        let req = crate::cast::requirement(&f.cost, 0);
        let srcs = srcs.get_or_init(|| crate::cast::mana_sources(cx, p, None));
        if crate::cast::payable(&req, srcs) {
            out.push(Opt::Alt(o, crate::altcast::Method::TurnFaceUp as u8));
        }
    }
}

/// Turn face-down permanent `o` face up by paying its card's mana cost (special action, CR
/// 701.40b, 116.2b): the same object, with its normal characteristics (708.8).
pub fn turn_face_up(cx: &mut Ctx, p: PlayerId, o: ObjId) -> R<()> {
    let Some(i) = cx.s.ext.x6.face_down.iter().position(|x| x.0 == o) else { return cx.unsupported("turning up a permanent that isn't face down") };
    let real = cx.s.ext.x6.face_down[i].1;
    let f = cx.db.get(real).front();
    if !f.types.has(CardType::Creature) || f.cost.none || cx.s.obj(o).controller != p {
        return cx.unsupported("turning up a face-down permanent that can't be");
    }
    // 701.40b: show all players that it is a creature card and its mana cost, then pay it.
    cx.s.emit(Event::TurnedFaceUp { player: p, obj: o, card: real });
    let req = crate::cast::requirement(&f.cost, 0);
    crate::cast::pay_mana(cx, p, req, None, o)?;
    cx.s.ext.x6.face_down.remove(i);
    let ob = cx.s.obj_mut(o);
    ob.card = real;
    ob.known_to = 0b11;
    Ok(())
}

impl CardDb {
    /// Static modes of this batch (`None`: not one of them; keys untouched).
    pub(crate) fn x_static(&mut self, mode: &str, p: &crate::compile::P, f: &forge_script::Face) -> Result<Option<StaticKind>, String> {
        if mode == "Continuous" && p.peek("GainsAbilitiesOfDefined").is_some() {
            let ok = p.get("GainsAbilitiesOfDefined") == Some("ExiledWith") && p.get("GainsAbilitiesLimitPerTurn") == Some("1") && p.get("AffectedDefined") == Some("Self") && matches!(p.get("EffectZone"), None | Some("Battlefield"));
            if !ok {
                return Err("GainsAbilitiesOfDefined form".into());
            }
            return Ok(Some(StaticKind::Ext(crate::static_ext::StaticExt::X(XStatic::GainsCraftedAbilities))));
        }
        if mode == "Continuous" && p.peek("AffectedZone") == Some("Stack") && p.peek("AddKeyword") == Some("Storm") {
            p.ack("AffectedZone", &["Stack"], "checked above");
            p.ack("AddKeyword", &["Storm"], "checked above");
            let affected = self.filter_f(f, p.get("Affected").ok_or("storm grant without Affected")?)?;
            if self.x6.storm_ability.is_none() {
                let a = self.push_ability(AbilityDef {
                    kind: AbilityKind::Sub,
                    effect: Effect::Ext(crate::effect_ext::ExtEffect::X(XEffect::Storm)),
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
                    text: "Storm (copy this spell for each spell cast before it this turn; you may choose new targets for the copies)".into(),
                });
                self.x6.storm_ability = Some(a);
            }
            return Ok(Some(StaticKind::Ext(crate::static_ext::StaticExt::X(XStatic::GrantStorm { affected }))));
        }
        Ok(None)
    }
}

impl CardDb {
    /// Pool mana with a spending restriction or "any one color" (`Mana` that isn't a mana
    /// ability: a loyalty ability or a triggered ability's effect).
    pub(crate) fn x_pool_mana(&mut self, p: &crate::compile::P, f: &forge_script::Face) -> Result<Effect, String> {
        let prod = p.get("Produced").unwrap_or("");
        let n: u8 = p.get("Amount").map(|a| a.parse().map_err(|_| format!("Mana Amount {a}"))).transpose()?.unwrap_or(1);
        let (mut types, mut one_color) = (vec![], 0);
        let combo = prod == "Combo Any";
        if prod == "Any" {
            one_color = n;
        } else if combo {
            // "Add N mana in any combination of colors" (`ext7`): chosen as it resolves.
        } else {
            for w in prod.split_whitespace() {
                types.push(match w {
                    "C" => ManaType::C,
                    c if c.len() == 1 => Color::from_char(c.chars().next().unwrap()).map(ManaType::of).ok_or_else(|| format!("Mana Produced {c}"))?,
                    c => return Err(format!("Mana Produced {c}")),
                });
            }
            if types.is_empty() {
                return Err("Mana without Produced".into());
            }
            types = (0..n).flat_map(|_| types.clone()).collect();
        }
        let restrict = match p.get("RestrictValid") {
            None => None,
            Some(rv) => {
                // "Spend this mana only to cast a [matching] spell" (CR 106.6): spells only.
                if rv.split(',').any(|a| !a.starts_with("Spell.")) {
                    return Err(format!("Mana RestrictValid {rv} (pool)"));
                }
                let rv = rv.split(',').map(|a| format!("Card.{}", &a["Spell.".len()..])).collect::<Vec<_>>().join(",");
                let fl = self.filter_f(f, &rv)?;
                let i = match self.x6.restricts.iter().position(|x| *x == fl) {
                    Some(i) => i,
                    None => {
                        self.x6.restricts.push(fl);
                        self.x6.restricts.len() - 1
                    }
                };
                if i > u8::MAX as usize {
                    return Err("too many mana restrictions".into());
                }
                if self.x6.restrict_text.len() <= i {
                    self.x6.restrict_text.push(restrict_text(&rv));
                }
                Some(i as u16)
            }
        };
        if combo {
            return Ok(Effect::Ext(crate::effect_ext::ExtEffect::X7(crate::ext7::X7Effect::AddManaCombo { n, restrict })));
        }
        Ok(Effect::Ext(crate::effect_ext::ExtEffect::X(XEffect::AddMana { types, one_color, restrict })))
    }
}

impl CardDb {
    /// Trigger modes of this batch (`None`: not one of them).
    pub(crate) fn x_trigger_mode(&mut self, m: &str, p: &crate::compile::P) -> Result<Option<TrigMode>, String> {
        Ok(Some(match m {
            "Discarded" => {
                if !matches!(p.get("ValidCard"), Some("Card.YouCtrl") | Some("Card.YouOwn")) {
                    return Err("Discarded form".into());
                }
                self.x6.any_discard_trigger = true;
                TrigMode::Ext(crate::trig_ext::TrigExt::DiscardedCard)
            }
            "DiscardedAll" => {
                if !matches!(p.get("ValidCard"), None | Some("Card")) {
                    return Err("DiscardedAll ValidCard".into());
                }
                self.x6.any_discard_trigger = true;
                TrigMode::Ext(crate::trig_ext::TrigExt::DiscardedAll { player: PlayerSel::parse(p.get("ValidPlayer"))? })
            }
            "ClassLevelGained" => {
                let n = p.get("ClassLevel").and_then(|v| v.parse::<u8>().ok()).ok_or("ClassLevelGained ClassLevel")?;
                if p.get("ValidCard") != Some("Card.Self") {
                    return Err("ClassLevelGained form".into());
                }
                TrigMode::Ext(crate::trig_ext::TrigExt::ClassLevelSelf { level: n })
            }
            "AbilityCast" if p.peek("ValidSA") == Some("SpellAbility.!ManaAbility") => {
                p.ack("ValidSA", &["SpellAbility.!ManaAbility"], "checked above");
                if p.get("ValidActivatingPlayer") != Some("You") {
                    return Err("AbilityCast form".into());
                }
                self.x6.any_activation_trigger = true;
                TrigMode::Ext(crate::trig_ext::TrigExt::ActivatedNonMana)
            }
            "CounterAdded" => {
                let ok = p.get("ValidCard") == Some("Card.Self");
                let kind = p.get("CounterType").map(|k| self.counter_kind(k));
                let n = p.get("CounterAmount").and_then(|a| a.strip_prefix("EQ")).and_then(|a| a.parse::<i32>().ok());
                let (Some(kind), Some(n), true) = (kind, n, ok) else { return Err("CounterAdded form".into()) };
                self.x6.any_counter_added = true;
                TrigMode::Ext(crate::trig_ext::TrigExt::CounterAddedSelf { kind, n })
            }
            "Transformed" => {
                if p.get("ValidCard") != Some("Card.Self") {
                    return Err("Transformed form".into());
                }
                self.x6.any_transformed_trigger = true;
                TrigMode::Ext(crate::trig_ext::TrigExt::TransformedSelf)
            }
            _ => return Ok(None),
        }))
    }
}

/// `o` just transformed (CR 701.27a): "whenever this transforms into ..." (701.27e).
pub fn transformed(cx: &mut Ctx, o: ObjId) {
    if cx.db.x6.any_transformed_trigger {
        crate::triggers::fire(cx.s, cx.db, &crate::triggers::TrigEvent::Ext(crate::trig_ext::ExtEvent::Transformed { obj: o }));
    }
}

/// Readable form of a compiled `RestrictValid$` filter string (`Card.nonCreature`).
fn restrict_text(rv: &str) -> String {
    rv.split(',')
        .map(|a| match a {
            "Card.nonCreature" => "noncreature spells".to_string(),
            a if a.starts_with("Card.cmcGE") => format!("spells with mana value {} or greater", &a["Card.cmcGE".len()..]),
            a => format!("spells matching {a}"),
        })
        .collect::<Vec<_>>()
        .join(" or ")
}

/// Put a copy of activated ability `it` onto the stack under `p`'s control (CR 707.10: same
/// source, 707.10b), choosing new targets (707.10c). Records of objects used to pay its costs
/// are shared with the original (707.10): each resolution consumes its own copy of them.
pub fn copy_ability(cx: &mut Ctx, it: &StackItem, p: PlayerId, resolving: Option<&StackItem>) -> R<()> {
    if let Some(&(_, o)) = cx.s.ext.cost_sacrificed.iter().rev().find(|x| x.0 == it.source) {
        cx.s.ext.cost_sacrificed.push((it.source, o));
    }
    if let Some(&(_, o)) = cx.s.ext.le.cost_tapped.iter().rev().find(|x| x.0 == it.source) {
        cx.s.ext.le.cost_tapped.push((it.source, o));
    }
    let item = cx.s.push_stack(StackItem { controller: p, ..it.clone() });
    let top = cx.s.stack.len() - 1;
    cx.s.emit(Event::AbilityCopied { player: p, card: it.source_card, targets: it.targets.iter().map(|t| t.1).collect() });
    let new = choose_new_targets(cx, p, it, top, it.source, resolving)?;
    let tobjs: SmallVec<[ObjId; 2]> = new.iter().filter_map(|t| if let Target::Obj(o) = t.1 { Some(o) } else { None }).collect();
    crate::cast::fire_targeted(cx, &tobjs, it.source, item, p);
    Ok(())
}

/// `p` activated (non-mana) ability `item` (now on top of the stack): "whenever you activate an
/// ability that isn't a mana ability" triggers with a key to the ability as activated.
pub fn ability_activated(cx: &mut Ctx, p: PlayerId) {
    if !cx.db.x6.any_activation_trigger {
        return;
    }
    let Some(item) = cx.s.stack.last().cloned() else { return };
    let key = cx.s.ext.x6.next_activated;
    cx.s.ext.x6.next_activated += 1;
    cx.s.ext.x6.activated.push((key, item.clone()));
    crate::triggers::fire(cx.s, cx.db, &crate::triggers::TrigEvent::Ext(crate::trig_ext::ExtEvent::AbilityActivated { player: p, source: item.source, key }));
}

/// Activated abilities `o` has from the cards exiled to craft it (Locus of Enlightenment), with
/// how many of those cards have each (CR 702.167; each may be activated once per turn per card).
fn crafted_abilities(s: &State, db: &CardDb, o: ObjId) -> SmallVec<[(AbilityId, u8); 4]> {
    let mut out: SmallVec<[(AbilityId, u8); 4]> = SmallVec::new();
    let Some((_, mats)) = s.ext.x6.crafted.iter().find(|x| x.0 == o) else { return out };
    let ob = s.obj(o);
    let has = db.get(ob.card).face(ob.face).statics.iter().any(|st| matches!(st.kind, StaticKind::Ext(crate::static_ext::StaticExt::X(XStatic::GainsCraftedAbilities))));
    if !has {
        return out;
    }
    for &m in mats {
        if s.obj(m).zone != Zone::Exile {
            continue; // no longer an exiled card
        }
        for &a in &db.get(s.obj(m).card).front().activated {
            match out.iter_mut().find(|x| x.0 == a) {
                Some(x) => x.1 += 1,
                None => out.push((a, 1)),
            }
        }
    }
    out
}

/// Add the crafted-card abilities of `o` (layer 6) to `abilities` (`query::chars`).
pub fn gained_from_crafted(s: &State, db: &CardDb, o: ObjId, abilities: &mut SmallVec<[AbilityId; 4]>) {
    for (a, _) in crafted_abilities(s, db, o) {
        if !abilities.contains(&a) {
            abilities.push(a);
        }
    }
}

/// Has `o` used up its once-per-turn activations of crafted-card ability `a`?
pub fn gained_limit_reached(s: &State, db: &CardDb, o: ObjId, a: AbilityId) -> bool {
    if s.ext.x6.crafted.is_empty() {
        return false;
    }
    let ob = s.obj(o);
    if db.get(ob.card).face(ob.face).activated.contains(&a) {
        return false;
    }
    match crafted_abilities(s, db, o).iter().find(|x| x.0 == a) {
        None => false,
        Some(&(_, n)) => s.ext.x6.gained_used.iter().find(|x| x.0 == o && x.1 == a).map_or(0, |x| x.2) >= n,
    }
}

/// Count an activation of a crafted-card ability (`gained_limit_reached`).
pub fn note_gained_use(s: &mut State, db: &CardDb, o: ObjId, a: AbilityId) {
    if s.ext.x6.crafted.is_empty() || !crafted_abilities(s, db, o).iter().any(|x| x.0 == a) || db.get(s.obj(o).card).face(s.obj(o).face).activated.contains(&a) {
        return;
    }
    match s.ext.x6.gained_used.iter_mut().find(|x| x.0 == o && x.1 == a) {
        Some(x) => x.2 += 1,
        None => s.ext.x6.gained_used.push((o, a, 1)),
    }
}

/// Level of `o` (CR 716.2d: 1 if it has none).
pub fn level_of(s: &State, o: ObjId) -> u8 {
    s.ext.x6.levels.iter().find(|x| x.0 == o).map_or(1, |x| x.1)
}

/// After a batch of moves: cards announced as discarded (`Event::Discard`) since the last batch
/// trigger "whenever [player] discards a card" once per card and "whenever [player] discards
/// one or more cards" once (CR 603.2c: one event); the cards that went from hand to graveyard
/// are remembered as discarded this turn (mayhem).
pub fn flush_discards(cx: &mut Ctx, moved: &[(ObjId, Zone, Zone, Option<ObjId>)]) {
    if cx.s.ext.x6.discards == [0, 0] {
        return;
    }
    let counts = std::mem::take(&mut cx.s.ext.x6.discards);
    for &(_, from, to, new) in moved {
        if let (Zone::Hand, Zone::Graveyard, Some(n)) = (from, to, new) {
            if counts[cx.s.obj(n).owner.idx()] > 0 {
                cx.s.ext.x6.discarded_this_turn.push(n);
            }
        }
    }
    if !cx.db.x6.any_discard_trigger {
        return;
    }
    for p in [cx.s.turn.active, cx.s.turn.active.opp()] {
        let n = counts[p.idx()];
        if n == 0 {
            continue;
        }
        for _ in 0..n {
            crate::triggers::fire(cx.s, cx.db, &crate::triggers::TrigEvent::Ext(crate::trig_ext::ExtEvent::Discarded { player: p }));
        }
        crate::triggers::fire(cx.s, cx.db, &crate::triggers::TrigEvent::Ext(crate::trig_ext::ExtEvent::DiscardedAll { player: p, amount: n as i32 }));
    }
}

/// Put `n` counters of kind `k` on `o` (a negative `n` removes), recording the timestamp of a
/// keyword counter (CR 613.7c, `XState::kw_counter_ts`). Every rules path that puts counters on
/// an object goes through here.
pub fn put_counters(s: &mut State, db: &CardDb, o: ObjId, k: CounterKind, n: i32) {
    // Bounded (`num`): an amount or count past the bound fails the unit loudly.
    let have = s.obj(o).counter(k);
    let now = s.num_add(have, n as i64, "counters");
    let n = now - have;
    s.obj_mut(o).add_counter(k, n);
    if n <= 0 {
        return;
    }
    let CounterKind::Named(i) = k else { return };
    if db.counter_names.get(i as usize).and_then(|n| Kw::parse(n)).is_none() {
        return;
    }
    let ts = s.ts();
    let State { objs, ext, .. } = s;
    let t = &mut ext.x6.kw_counter_ts;
    t.retain(|x| objs[x.0.idx()].zone == Zone::Battlefield && !(x.0 == o && x.1 == k));
    t.push((o, k, ts));
}

/// Timestamp of the keyword counters of kind `k` on `o` (0 if none was recorded).
pub fn kw_counter_ts(s: &State, o: ObjId, k: CounterKind) -> u32 {
    s.ext.x6.kw_counter_ts.iter().find(|x| x.0 == o && x.1 == k).map_or(0, |x| x.2)
}

/// Granted triggered abilities (`XState::granted`) for an event (called from `trig_ext::fire_extra`).
pub fn granted_triggers(s: &State, db: &CardDb, ev: &crate::triggers::TrigEvent, fired: &mut Vec<PendingTrigger>) {
    for &(o, ti, ts) in &s.ext.x6.granted {
        let ob = s.obj(o);
        if ob.zone != Zone::Battlefield || crate::query::grant_removed(s, db, o, ts) {
            continue;
        }
        let td = &db.triggers_pool[ti as usize];
        if crate::triggers::trigger_suppressed(s, db, ev, td, crate::triggers::WardGrant::Of { src: o, ts }) {
            continue;
        }
        if let Some(trig) = crate::triggers::matches(s, db, &td.mode, ev, o, ob.controller) {
            fired.push(PendingTrigger { source: o, card: ob.card, controller: ob.controller, trigger_idx: 0, trig, adhoc: Some(td.execute) });
        }
    }
}

/// Is this `Animate` the "gains [abilities] for as long as it's on the battlefield" form this
/// batch handles (`XEffect::GrantAbilities`)?
pub(crate) fn is_grant_animate(p: &crate::compile::P) -> bool {
    p.peek("Duration") == Some("Permanent") && p.peek("Defined") == Some("Remembered") && (p.peek("staticAbilities").is_some() || p.peek("Triggers").is_some())
}

impl CardDb {
    pub(crate) fn x_grant_animate(&mut self, p: &crate::compile::P, f: &forge_script::Face, uns: &mut Vec<String>) -> Result<Effect, String> {
        p.ack("Duration", &["Permanent"], "checked by `is_grant_animate`");
        let mut must_attack = false;
        if let Some(names) = p.get("staticAbilities") {
            for n in names.split(',') {
                let sv = f.svar(n.trim()).ok_or_else(|| format!("staticAbilities {n} missing"))?;
                let sp = forge_script::Params::parse(&sv.value);
                let ok = sp.get("Mode") == Some("MustAttack") && sp.get("ValidCreature") == Some("Card.Self") && sp.pairs.iter().all(|(k, _)| matches!(k.as_str(), "Mode" | "ValidCreature" | "Description"));
                if !ok {
                    return Err("Animate staticAbilities form".into());
                }
                must_attack = true;
            }
        }
        let mut triggers = vec![];
        if let Some(names) = p.get("Triggers") {
            for n in names.split(',') {
                let sv = f.svar(n.trim()).ok_or_else(|| format!("Triggers {n} missing"))?;
                let tp = forge_script::Params::parse(&sv.value);
                let td = self.compile_trigger(&tp, f, uns)?.ok_or("granted trigger")?;
                triggers.push(self.triggers_pool.len() as u32);
                self.triggers_pool.push(td);
            }
        }
        Ok(Effect::Ext(crate::effect_ext::ExtEffect::X(XEffect::GrantAbilities { must_attack, triggers })))
    }

    /// `K:Class:N:COST:AddTrigger$ T | AddStaticAbility$ S` (CR 716.2a): "COST: This Class's
    /// level becomes N. Activate only if it is level N-1 and only as a sorcery", and "as long as
    /// it is level N or greater, it has [abilities]" (level-gated triggers and statics).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn x_class_level(&mut self, f: &forge_script::Face, level: &str, cost: &str, adds: &str, activated: &mut Vec<AbilityId>, triggers: &mut Vec<TriggerDef>, statics: &mut Vec<StaticDef>, uns: &mut Vec<String>) -> Result<(), String> {
        use crate::expr::{Condition, Expr};
        let n: u8 = level.parse().map_err(|_| format!("Class level {level}"))?;
        let mc = ManaCost::parse_forge(cost)?;
        let lvl = || Expr::Ext(Box::new(crate::expr_ext::ExtExpr::X(XExpr::ClassLevel)));
        let a = self.push_ability(AbilityDef {
            kind: AbilityKind::Activated,
            effect: Effect::Ext(crate::effect_ext::ExtEffect::X(XEffect::ClassLevel { level: n })),
            defined: Some(Defined::SelfObj),
            target: None,
            cost: Some(Cost { mana: mc, ..Default::default() }),
            sorcery_speed: true,
            is_mana: false,
            activate_if: vec![Condition::Compare(lvl(), crate::filter::CmpOp::EQ, Expr::Num(n as i32 - 1))],
            conditions: vec![],
            unless: None,
            optional: false,
            remember: false,
            sub: None,
            text: format!("Level {n}"),
        });
        activated.push(a);
        let gate = Condition::Compare(lvl(), crate::filter::CmpOp::GE, Expr::Num(n as i32));
        let ap = forge_script::Params::parse(adds);
        for (k, v) in &ap.pairs {
            for name in v.split('&').map(str::trim) {
                let sv = f.svar(name).ok_or_else(|| format!("Class {k} {name} missing"))?;
                let sp = forge_script::Params::parse(&sv.value);
                match k.as_str() {
                    "AddTrigger" => {
                        let mut td = self.compile_trigger(&sp, f, uns)?.ok_or("Class trigger")?;
                        // The Class has it only at this level or above: checked as it triggers.
                        td.intervening.push(Condition::TriggerOnly(Box::new(gate.clone())));
                        triggers.push(td);
                    }
                    "AddStaticAbility" => {
                        let mut sd = self.compile_static(&sp, f)?;
                        sd.conditions.push(gate.clone());
                        let player = self.take_player_hexproof(&sd);
                        statics.push(sd);
                        statics.extend(player);
                    }
                    k => return Err(format!("Class {k}")),
                }
            }
        }
        Ok(())
    }
}

/// Mana in `p`'s pool spendable on `spell` (a spell, `ABILITY_PAYMENT` for an activated
/// ability's cost, None: any other cost), by type.
pub fn pool_avail(s: &State, db: &CardDb, p: PlayerId, spell: Option<ObjId>) -> [u8; 6] {
    let mut pool = s.ext.pool[p.idx()];
    if spell.is_some() {
        for &(pl, t, n, r) in &s.ext.x6.rpool {
            if pl == p && r.allows(s, db, p, spell) {
                pool[t as usize] = pool[t as usize].saturating_add(n);
            }
        }
    }
    pool
}

/// Restricted mana of type `t` in `p`'s pool that `spell` may use (`pool_avail`'s selector).
pub fn restricted_avail(s: &State, db: &CardDb, p: PlayerId, t: ManaType, spell: Option<ObjId>) -> u8 {
    s.ext.x6.rpool.iter().filter(|x| x.0 == p && x.1 == t && x.3.allows(s, db, p, spell)).map(|x| x.2).sum()
}

/// All mana in `p`'s pool, restricted or not (what the observation shows).
pub fn pool_total(s: &State, p: PlayerId) -> [u8; 6] {
    let mut pool = s.ext.pool[p.idx()];
    for &(pl, t, n, _) in &s.ext.x6.rpool {
        if pl == p {
            pool[t as usize] = pool[t as usize].saturating_add(n);
        }
    }
    pool
}

/// Spend one mana of type `t` from `p`'s pool on `spell`: restricted mana the spell may use
/// first (the unrestricted mana left over can pay for anything the restricted could, so this
/// order never loses an option), then unrestricted. If mana with two different restrictions
/// could pay, which one is left over is a real choice: `PayMana` with `Opt::Mode(r)` per
/// restriction `r` (`XTables::restricts`; the text view names the restriction).
pub fn spend_pool(cx: &mut Ctx, p: PlayerId, t: ManaType, spell: Option<ObjId>) -> R<()> {
    if spell.is_some() {
        let m: SmallVec<[usize; 2]> = (0..cx.s.ext.x6.rpool.len())
            .filter(|&i| {
                let (pl, ty, n, r) = cx.s.ext.x6.rpool[i];
                pl == p && ty == t && n > 0 && r.allows(cx.s, cx.db, p, spell)
            })
            .collect();
        if let Some(&first) = m.first() {
            // Entries are kept one per (player, type, restriction): `m` has distinct restrictions.
            let n_spell = m.iter().filter(|&&j| matches!(cx.s.ext.x6.rpool[j].3, Restr::Spell(_))).count();
            let i = if m.len() > n_spell {
                // Restricted mana a mana ability made (left over from a payment, or at priority)
                // is spent before resolving effects' restricted mana; which one is left over isn't
                // asked (fidelity limit, a known approximation).
                m.iter().copied().find(|&j| !matches!(cx.s.ext.x6.rpool[j].3, Restr::Spell(_))).unwrap()
            } else if m.len() > 1 {
                let options: Vec<Opt> = m.iter().map(|&j| Opt::Mode(cx.s.ext.x6.rpool[j].3.mode_code())).collect();
                let Opt::Mode(r) = cx.choose(Decision { player: p, kind: DecisionKind::PayMana, options, subject: spell })? else { unreachable!() };
                m.iter().copied().find(|&j| cx.s.ext.x6.rpool[j].3.mode_code() == r).unwrap()
            } else {
                first
            };
            let r = cx.s.ext.x6.rpool[i].3;
            if let Some(sp) = spell.filter(|&x| x != ABILITY_PAYMENT) {
                if r.nocounter(cx.db) && !cx.s.ext.uncounterable.contains(&sp) {
                    cx.s.ext.uncounterable.push(sp); // CR 101.2 (Cavern of Souls)
                }
            }
            cx.s.ext.x6.rpool[i].2 -= 1;
            cx.s.ext.x6.rpool.retain(|x| x.2 > 0);
            return Ok(());
        }
    }
    if cx.s.ext.pool[p.idx()][t as usize] == 0 {
        return cx.unsupported(format!("spending {t:?} that isn't in the mana pool"));
    }
    cx.s.ext.pool[p.idx()][t as usize] -= 1;
    Ok(())
}

/// The spell `so` on the stack as copies copy it (CR 707.10); `before` is a storm count.
pub(crate) fn snapshot_spell(s: &State, so: ObjId, before: u8) -> Option<StormSnap> {
    let item = s.stack_item_of(so).cloned()?; // waiting or resolving (608.2n)
    let flags = s.cast_of(so).flags & !(crate::altcast::flag::FROM_GRAVE | crate::altcast::flag::WARPED | crate::altcast::flag::PLOTTED | crate::altcast::flag::AIRBENT | crate::altcast::flag::FROM_EXILE);
    let divided = item.targets.iter().map(|&(_, t)| (t, s.ext.divided.iter().find(|d| d.0 == so && d.1 == t).map_or(0, |d| d.2))).collect();
    let sacrificed = s.ext.cost_sacrificed.iter().filter(|c| c.0 == so).map(|c| c.1).collect();
    Some(StormSnap { spell: so, item, before, flags, divided, sacrificed })
}

/// Instances of storm the spell `so` has as it becomes cast (CR 702.40b: each triggers).
fn storm_instances(s: &State, db: &CardDb, so: ObjId) -> usize {
    // effect-object static reader: GrantStorm (`static_ext::applies_from_effect_object`)
    let grants = |st: u32, eo: &crate::state::EffectObj| match &db.statics_pool[st as usize].kind {
        StaticKind::Ext(crate::static_ext::StaticExt::X(XStatic::GrantStorm { affected })) => obj_matches(s, db, affected, so, eo_fcx!(eo)),
        _ => false,
    };
    let from_effects: usize = s.effect_objs.iter().map(|eo| eo.statics.iter().filter(|&&st| grants(st, eo)).count()).sum();
    let from_permanents = crate::static_ext::active_x(s, db).filter(|(src, you, _, x)| matches!(x, XStatic::GrantStorm { affected } if obj_matches(s, db, affected, so, FilterCtx { you: *you, source: Some(*src), x: 0, rem: &[] }))).count();
    from_effects + from_permanents
}

/// `p` just cast `so` (CR 601.2i; the stack item is on top): storm triggers.
pub fn spell_cast(cx: &mut Ctx, p: PlayerId, so: ObjId) {
    let Some(storm) = cx.db.x6.storm_ability else { return };
    let n = storm_instances(cx.s, cx.db, so);
    if n == 0 {
        return;
    }
    let before = cx.s.ext.cast_this_turn.iter().position(|x| x.1 == so).unwrap_or(0) as u8;
    let snap = snapshot_spell(cx.s, so, before).expect("storm: cast spell not on the stack");
    cx.s.ext.x6.storm.push(snap);
    for _ in 0..n {
        crate::trig_ext::add_reflexive(cx, p, so, storm, TrigCtx { obj: Some(so), ..Default::default() });
    }
}

pub fn apply(cx: &mut Ctx, ex: &mut crate::resolve::Exec, _ad: &AbilityDef, x: &XEffect, _tg: &[Target], _who: &[Target]) -> R<()> {
    match x {
        XEffect::Storm => {
            let Some(spell) = ex.trig.obj else { return Ok(()) };
            let Some(snap) = cx.s.ext.x6.storm.iter().find(|x| x.spell == spell).cloned() else { return cx.unsupported("storm without its spell") };
            for _ in 0..snap.before {
                copy_spell(cx, &snap, ex.you, ex.resolving.as_ref())?;
            }
            Ok(())
        }
        XEffect::ManifestDread { who } => {
            let players: SmallVec<[PlayerId; 2]> = match who {
                DreadWho::You => smallvec::smallvec![ex.you],
                DreadWho::ExiledCardOwner => {
                    // Only cards still in exile (CR 607.2a, 400.7; a token exiled this way
                    // ceased to exist, 111.7): with none, nobody manifests dread (Unidentified
                    // Hovership ruling 2024-09-20).
                    let mut v: SmallVec<[PlayerId; 2]> = SmallVec::new();
                    for o in crate::effect_ext::still_exiled_with(cx.s, ex.source) {
                        let owner = cx.s.obj(o).owner;
                        if !v.contains(&owner) {
                            v.push(owner);
                        }
                    }
                    let active = cx.s.turn.active;
                    v.sort_by_key(|&q| q != active); // APNAP (CR 101.4)
                    v
                }
            };
            for q in players {
                manifest_dread(cx, q, ex.source)?;
            }
            Ok(())
        }
        XEffect::FlipCoinsSkipTurns { n } => {
            let mut heads = 0u8;
            for _ in 0..*n {
                heads += (cx.s.rng.below(2) == 0) as u8;
            }
            cx.s.emit(Event::CoinFlips { player: ex.you, flips: *n, heads });
            for t in _tg {
                if let Target::Player(q) = *t {
                    let s = &mut cx.s.ext.x6.skip_turns[q.idx()];
                    *s = s.saturating_add(heads);
                }
            }
            Ok(())
        }
        XEffect::CopyTargetSpell => {
            for t in _tg {
                let Target::Obj(so) = *t else { continue };
                // Still a spell on the stack (608.2b already checked the target is legal).
                let Some(snap) = snapshot_spell(cx.s, so, 0) else { continue };
                copy_spell(cx, &snap, ex.you, ex.resolving.as_ref())?;
            }
            Ok(())
        }
        XEffect::CopyActivated => {
            let key = ex.trig.amount as u32;
            let Some(it) = cx.s.ext.x6.activated.iter().find(|x| x.0 == key).map(|x| x.1.clone()) else { return cx.unsupported("copy of an unknown activated ability") };
            copy_ability(cx, &it, ex.you, ex.resolving.as_ref())
        }
        XEffect::ClassLevel { level } => {
            let o = ex.source;
            if cx.s.obj(o).zone == Zone::Battlefield {
                match cx.s.ext.x6.levels.iter_mut().find(|x| x.0 == o) {
                    Some(x) => x.1 = *level,
                    None => cx.s.ext.x6.levels.push((o, *level)),
                }
                crate::triggers::fire(cx.s, cx.db, &crate::triggers::TrigEvent::Ext(crate::trig_ext::ExtEvent::ClassLevel { obj: o, level: *level }));
            }
            Ok(())
        }
        XEffect::GrantAbilities { must_attack, triggers } => {
            for t in _who {
                let Target::Obj(o) = *t else { continue };
                if cx.s.obj(o).zone != Zone::Battlefield {
                    continue;
                }
                let ts = cx.s.ts();
                if *must_attack && !cx.s.ext.x6.must_attack.iter().any(|x| x.0 == o) {
                    cx.s.ext.x6.must_attack.push((o, ts));
                }
                for &ti in triggers {
                    cx.s.ext.x6.granted.push((o, ti, ts));
                }
            }
            Ok(())
        }
        XEffect::DelayedDies { execute } => {
            let turn = cx.s.turn.number;
            let objs: SmallVec<[ObjId; 2]> = ex.last_targets.iter().filter_map(|t| if let Target::Obj(o) = t { Some(*o) } else { None }).collect();
            for o in objs {
                if cx.s.obj(o).zone == Zone::Battlefield {
                    crate::trig_ext::add_delayed_for(cx, crate::ext::DelayedWhen::DiesWithin { obj: o, turn }, ex.you, ex.source, *execute, o);
                }
            }
            Ok(())
        }
        XEffect::AddMana { types, one_color, restrict } => {
            let mut add: SmallVec<[ManaType; 2]> = types.iter().copied().collect();
            if *one_color > 0 {
                let options: Vec<Opt> = [ManaType::W, ManaType::U, ManaType::B, ManaType::R, ManaType::G].into_iter().map(Opt::PoolMana).collect();
                let Opt::PoolMana(t) = cx.choose(Decision { player: ex.you, kind: DecisionKind::ChooseMode, options, subject: Some(ex.source) })? else { unreachable!() };
                add.extend((0..*one_color).map(|_| t));
            }
            for t in add {
                add_pool(cx.s, ex.you, t, 1, restrict.map(Restr::Spell));
            }
            Ok(())
        }
    }
}

/// "[-X]" loyalty costs: announce X, from 0 to the loyalty the permanent has (CR 107.3, 606.6).
pub fn announce_x_loyalty(cx: &mut Ctx, p: PlayerId, o: ObjId) -> R<i32> {
    let max = cx.s.obj(o).counter(CounterKind::Loyalty).max(0);
    let options = cx.number_options(0, max, "-X loyalty")?;
    let Opt::Number(x) = cx.choose(Decision { player: p, kind: DecisionKind::ChooseX, options, subject: Some(o) })? else { unreachable!() };
    Ok(x)
}

/// Put a copy of the snapshot's spell onto the stack under `p`'s control (CR 707.10): same
/// modes, X, kicker, additional/alternative costs paid and division; not cast. `p` may choose
/// new targets for it (707.10c, 115.7d), one target at a time: keep it (even if illegal now)
/// or pick another legal one. Within one "target" word the targets stay distinct (115.3); a
/// target can't become one the copy keeps elsewhere when targets must be unique.
pub fn copy_spell(cx: &mut Ctx, snap: &StormSnap, p: PlayerId, resolving: Option<&StackItem>) -> R<()> {
    let it = &snap.item;
    let (card, face) = (it.source_card, cx.s.obj(snap.spell).face);
    let copy = cx.s.new_token(card, p);
    {
        let co = cx.s.obj_mut(copy);
        co.zone = Zone::Stack; // a copy of a spell, owned and controlled by `p` (707.10)
        co.face = face;
    }
    if snap.flags != 0 {
        cx.s.cast_mut(copy).flags = snap.flags;
    }
    for &o in &snap.sacrificed {
        cx.s.ext.cost_sacrificed.push((copy, o));
    }
    let item = cx.s.push_stack(StackItem { controller: p, source: copy, flashback: false, ..it.clone() });
    let top = cx.s.stack.len() - 1;
    // Announced before its controller chooses new targets, with the targets it copied (what a
    // player choosing them sees on the stack).
    cx.s.emit(Event::SpellCopied { player: p, card, targets: it.targets.iter().map(|t| t.1).collect() });
    if !it.targets.is_empty() {
        let new = choose_new_targets(cx, p, it, top, copy, resolving)?;
        // The division stays with each target slot (115.7f).
        for (slot, &(_, t)) in new.iter().enumerate() {
            let amount = snap.divided.get(slot).map_or(0, |d| d.1);
            if amount != 0 {
                cx.s.ext.divided.push((copy, t, amount));
            }
        }
        let tobjs: SmallVec<[ObjId; 2]> = new.iter().filter_map(|t| if let Target::Obj(o) = t.1 { Some(o) } else { None }).collect();
        crate::cast::fire_targeted(cx, &tobjs, copy, item, p);
    }
    Ok(())
}

/// For the copy of `it` at stack index `top` (`subject`: the copy's spell object, or the
/// ability's source): target by target, its controller keeps the target (offered even if now
/// illegal, as its own option) or picks another legal one (CR 707.10c, 115.7d). Within one
/// "target" word the targets stay distinct (115.3); with `TargetUnique$` all do.
pub fn choose_new_targets(cx: &mut Ctx, p: PlayerId, it: &StackItem, top: usize, subject: ObjId, resolving: Option<&StackItem>) -> R<SmallVec<[(u8, Target); 2]>> {
    let copy = subject;
    let mut new: SmallVec<[(u8, Target); 2]> = SmallVec::new();
    {
        let ns = crate::resolve::nodes(cx.db, it.ability, &it.modes);
        for (slot, &(n, old)) in it.targets.iter().enumerate() {
            let spec = cx.db.ability(ns[n as usize]).target.clone().expect("targeted node without a target spec");
            // Targets this one must differ from: the others of its node (chosen or kept), and
            // with `TargetUnique$` all others.
            let mut exclude: SmallVec<[Target; 4]> = new.iter().filter(|x| spec.unique || x.0 == n).map(|x| x.1).collect();
            exclude.extend(it.targets[slot + 1..].iter().filter(|x| spec.unique || x.0 == n).map(|x| x.1));
            // A copy of an ability can't target itself (CR 115.5).
            if it.kind != StackKind::Spell {
                exclude.push(Target::Ability { uid: cx.s.stack[top].uid, src: it.source });
            }
            let mut legal = crate::cast::legal_targets(cx, p, &spec, Some(copy), &exclude);
            // The spell or ability making the copy is still on the stack (CR 608.2, 115.5).
            legal.extend(crate::ext8::resolving_target(cx, p, &spec, Some(copy), &exclude, resolving));
            // "Target Equipment attached to that creature": relative to the copy's own earlier
            // target (as when targets are chosen, `cast.rs`).
            legal.retain(|&t| crate::ext9::attached_to_parent_ok(cx, ns[n as usize], n as usize, &new, t));
            let key = |t: Target| match t {
                Target::Player(pl) => (0u8, (pl.0 as u32, 0u8, 0i32, 0u32, 0u32, 0u32)),
                Target::Obj(o) => {
                    (1, obj_sig(cx.s, cx.db, o))
                }
                Target::Ability { uid, .. } => (3, (uid, 0, 0, 0, 0, 0)),
            };
            let mut items: Vec<((u8, crate::query::ObjSig), Opt)> = legal.iter().map(|&t| (key(t), Opt::Target(t))).collect();
            if !legal.contains(&old) {
                // Unchanged even though illegal now (115.7d): its own option, never merged with
                // a legal look-alike.
                items.push(((2, (slot as u32, 0, 0, 0, 0, 0)), Opt::Target(old)));
            }
            let t = match cx.choose_set(p, DecisionKind::Target, items, 1, 1, Some(copy))?.first() {
                Some(Opt::Target(t)) => *t,
                _ => old,
            };
            new.push((n, t));
            cx.s.stack[top].targets = new.iter().copied().chain(it.targets[slot + 1..].iter().copied()).collect();
        }
    }
    Ok(new)
}
