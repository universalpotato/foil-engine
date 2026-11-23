//! Forge script → IR compiler.
//!
//! Strictness is automatic: parameters are read through [`P`], which records every key it was
//! asked for. Any non-cosmetic key left unread after compiling a line marks the card
//! unsupported, so new Forge parameters can never be silently ignored.
//!
//! Consumed means honoured: a
//! key counts as read only through a reader that looks at its value. [`P::get`] hands the value
//! to the caller (`#[must_use]`; the compile modules deny discarding it), [`P::ack`] accepts only
//! a listed set of values, [`P::flag`] / [`P::bool_val`] only `True` / `False`, and [`P::has`]
//! is a presence test that does not count as a read. Any other value is an "unsupported Forge
//! parameter value" and makes the card unsupported at compile time; a lint test
//! rejects the shapes that would bypass this (`let _ = p.get(..)`, `p.get(..).is_some()`).


use crate::basics::*;
use crate::carddb::*;
use crate::expr::{Condition, Expr, ExprCompiler};
use crate::filter::Filter;
use crate::state::{CounterKind, Step, Zone};
use forge_script::{CardScript, Face, Params};
use std::cell::Cell;

/// Keys with no game semantics.
const COSMETIC: &[&str] = &[
    "SpellDescription",
    "StackDescription",
    "TgtPrompt",
    "AILogic",
    "AITgts",
    "AIPreference",
    "AITgtOwnCards",
    "AIManaPref",
    "AINoRecursiveCheck",
    "IsCurse",
    "PrecostDesc",
    "Description",
    "TriggerDescription",
    "ValidTgtsDesc",
    "ChangeTypeDesc",
    "DiscardValidDesc",
    "ValidDescription",
    "SacMessage",
    "Secondary",
    "AdditionalDescription",
    "OptionQuestion",
    "SelectPrompt",
    "SpellDescriptionText",
    "ConditionDescription",
    "ChoiceTitle",
    "ChoiceDesc",
    // Gift's reminder text (`ext7::x7_gift`).
    "GiftDescription",
];

/// An `ImmediateTrigger` node ("when you do, ...", CR 603.12).
fn is_immediate_trigger(p: &P) -> bool {
    ["DB", "AB", "SP"].iter().any(|k| p.peek(k) == Some("ImmediateTrigger"))
}

/// Parameter reader that tracks which keys were consumed, and rejects values it does not handle.
pub(crate) struct P<'a> {
    p: &'a Params,
    used: Vec<Cell<bool>>,
    /// `Key$Value (why)` of values the compiler does not handle ([`P::ack`], [`P::bool_val`],
    /// [`P::reject`]); reported by [`P::unused`].
    bad: std::cell::RefCell<Vec<String>>,
}
impl<'a> P<'a> {
    pub(crate) fn new(p: &'a Params) -> Self {
        P { p, used: p.pairs.iter().map(|_| Cell::new(false)).collect(), bad: Default::default() }
    }
    /// The value of a key, marking it read: the caller interprets the value.
    #[must_use]
    pub(crate) fn get(&self, k: &str) -> Option<&'a str> {
        let i = self.p.pairs.iter().position(|(key, _)| key == k)?;
        self.used[i].set(true);
        Some(self.p.pairs[i].1.as_str())
    }
    /// Value of a key without marking it read.
    #[must_use]
    pub(crate) fn peek(&self, k: &str) -> Option<&'a str> {
        self.p.pairs.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
    }
    /// Whether a key is present, without marking it read: its value must still be consumed by
    /// another reader, or the card is unsupported.
    #[must_use]
    pub(crate) fn has(&self, k: &str) -> bool {
        self.peek(k).is_some()
    }
    /// A key the compiler handles only for the `accepted` values (exact Forge spellings): marks
    /// it read and returns the value; any other value makes the card unsupported. `why` says
    /// what the accepted values mean here (it is part of the error).
    pub(crate) fn ack(&self, k: &str, accepted: &[&str], why: &str) -> Option<&'a str> {
        let v = self.get(k)?;
        if !accepted.contains(&v) {
            self.reject(k, v, why);
        }
        Some(v)
    }
    /// [`P::ack`] with a predicate: the value is handled only where `ok` holds.
    pub(crate) fn matched(&self, k: &str, ok: impl Fn(&str) -> bool, why: &str) -> Option<&'a str> {
        let v = self.get(k)?;
        if !ok(v) {
            self.reject(k, v, why);
        }
        Some(v)
    }
    /// A boolean key (`True` / `False`, any case), marking it read; any other value makes the
    /// card unsupported.
    #[must_use]
    pub(crate) fn bool_val(&self, k: &str) -> Option<bool> {
        let v = self.get(k)?;
        if v.eq_ignore_ascii_case("true") {
            Some(true)
        } else if v.eq_ignore_ascii_case("false") {
            Some(false)
        } else {
            self.reject(k, v, "not True/False");
            Some(false)
        }
    }
    /// `K$ True`: [`P::bool_val`], absent = false.
    #[must_use]
    pub(crate) fn flag(&self, k: &str) -> bool {
        self.bool_val(k).unwrap_or(false)
    }
    /// A display-only key (names and art of effect objects, reminder text): any value is
    /// accepted, because no rule reads it.
    pub(crate) fn display(&self, k: &str) {
        self.get(k).map(drop).unwrap_or(())
    }
    /// A key Forge itself never reads in this context (`forge_class`: the Java class that
    /// ignores it): any value is accepted, as in Forge.
    pub(crate) fn forge_ignores(&self, k: &str, forge_class: &str) {
        let _ = forge_class;
        self.get(k).map(drop).unwrap_or(())
    }
    /// Whether a key was already read (by an earlier reader of this line).
    #[must_use]
    pub(crate) fn consumed(&self, k: &str) -> bool {
        self.p.pairs.iter().zip(&self.used).any(|((key, _), u)| key == k && u.get())
    }
    /// Mark value `v` of key `k` unsupported (the card fails to compile).
    pub(crate) fn reject(&self, k: &str, v: &str, why: &str) {
        self.bad.borrow_mut().push(format!("{k}${v} (unsupported Forge parameter value: {why})"));
    }
    /// Unread non-cosmetic keys, then rejected values. Empty = every parameter was honoured.
    #[must_use]
    pub(crate) fn unused(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .p
            .pairs
            .iter()
            .zip(&self.used)
            .filter(|((k, _), u)| !u.get() && !COSMETIC.contains(&k.as_str()) && !k.starts_with("AI"))
            .map(|((k, _), _)| k.clone())
            .collect();
        out.extend(self.bad.borrow().iter().cloned());
        out
    }
}

type Res<T> = Result<T, String>;

/// `<X>All | ValidTgts$ Player | ValidCards$ F`: only the matching cards the target player
/// controls. Forge `PumpAllEffect`, `DamageAllEffect`, `CountersPutAllEffect`,
/// `AnimateAllEffect` (`getTargetPlayers` / `getFirstTargetedPlayer`); CR 115.1, 608.2b. The
/// shape of ChangeZoneAll (which restricts by zone ownership instead): without
/// this, "creatures target opponent controls get -1/-1" shrank every creature.
pub(crate) fn targeted_players_only(p: &P, api: &str, mut valid: Filter) -> Res<Filter> {
    if let Some(vt) = p.peek("ValidTgts") {
        if !vt.split(',').all(|t| matches!(t.split('.').next(), Some("Player" | "Opponent"))) {
            return Err(format!("{api} targeting {vt}"));
        }
        for a in &mut valid.alts {
            a.props.push(crate::filter::Prop::Ext(crate::filter_ext::ExtProp::TargetedPlayerCtrl));
        }
    }
    Ok(valid)
}

/// Whether the resolver of `e` implements `RememberX$ True` (`ad.remember`: it remembers the
/// objects the node affected) with Forge's meaning of key `k`: the cards moved
/// (`ChangeZoneEffect`, `DigEffect`: RememberChanged), discarded (`DiscardEffect`), milled
/// (`MillEffect`), put into the graveyard by a surveil (`Player.surveil`: RememberMoved),
/// sacrificed (`SacrificeEffect`), created (`TokenEffectBase`: RememberTokens /
/// RememberOriginalTokens), countered (`CounterEffect`), pumped by a PumpAll
/// (`PumpAllEffect`), attached (`AttachEffect`), or the targets
/// (`AbilityUtils.handleRemembering`: RememberTargets, only where the targets are what the
/// resolver records, a targeted move). Any other pairing would silently remember nothing.
/// The node kinds `ability_inner` may synthesize after a Forge line's own node (`extra_after`):
/// follow-ups that act on what that node moved or created, or set up delayed / linked / layer
/// effects for it. None of them changes zones itself (one instruction, one event: CR 608.2c,
/// 603.10a). A new synthesized kind must be added here, after checking it is not
/// a second instruction of the same event.
fn follow_up_node(e: &Effect) -> bool {
    use crate::effect_ext::ExtEffect as X;
    matches!(
        e,
        Effect::Ext(
            X::MakeAttacking
                | X::MakeAttackingTriggeredDefender
                | X::AtNextEnd { .. }
                | X::PumpMoved { .. }
                | X::TriggerEffect { .. }
                | X::LinkMoved { .. }
                | X::AnimateMoved { .. }
                | X::AllCreatureTypes { .. }
                | X::Lt(crate::ext3::LtEffect::ExileIfDiesThisTurn)
                | X::X9(crate::ext9::X9Effect::ReturnAtNextEnd { .. })
                | X::Le(crate::ext5::LeEffect::CopiesLegendary | crate::ext5::LeEffect::RememberTargets)
                | X::More(crate::ext2::MoreEffect::GrantTriggers { .. })
        )
    )
}

fn remember_honoured(k: &str, e: &Effect, targeted: bool) -> bool {
    use crate::effect_ext::ExtEffect as X;
    use crate::ext2::MoreEffect as More;
    use crate::ext3::LtEffect as Lt;
    use crate::ext4::MrEffect as Mr;
    use crate::ext5::LeEffect as Le;
    use crate::ext9::X9Effect as X9;
    match k {
        "RememberChanged" => matches!(
            e,
            Effect::ChangeZone { .. }
                | Effect::ChangeZoneAll { .. }
                | Effect::Dig { .. }
                | Effect::Ext(X::ChooseFromZone { .. })
                | Effect::Ext(X::More(More::SearchZone { .. }))
                | Effect::Ext(X::Lt(Lt::PutTransformed))
                | Effect::Ext(X::Lt(Lt::WishToHand { .. }))
                | Effect::Ext(X::Mr(Mr::ExileTopFaceDownLook { .. }))
                | Effect::Ext(X::Mr(Mr::RandomFromGraveyard { .. }))
                | Effect::Ext(X::X9(X9::PutTransformedYours))
        ),
        "RememberTargets" => targeted && matches!(e, Effect::ChangeZone { .. }),
        "RememberDiscarded" => matches!(e, Effect::Discard { .. } | Effect::Ext(X::More(More::DiscardUnless { .. })) | Effect::Ext(X::Mr(Mr::DiscardUpTo { .. }))),
        "RememberMilled" => matches!(e, Effect::Mill { .. }),
        "RememberMoved" => matches!(e, Effect::Surveil { .. }),
        "RememberSacrificed" => matches!(e, Effect::Sacrifice { .. } | Effect::Ext(X::Le(Le::SacrificeAll { .. })) | Effect::Ext(X::X9(X9::SacrificeEach { .. }))),
        // Without token-creation replacement effects the original tokens are all the tokens.
        "RememberTokens" | "RememberOriginalTokens" => matches!(e, Effect::Token { .. } | Effect::CopyPermanent | Effect::Ext(X::TokenFor { .. }) | Effect::Ext(X::Lt(Lt::TokenAttached { .. }))),
        "RememberCountered" => matches!(e, Effect::Counter),
        "RememberPumped" => matches!(e, Effect::PumpAll { .. }),
        "RememberAttached" => matches!(e, Effect::Attach | Effect::Ext(X::AttachParentTargets)),
        _ => false,
    }
}

impl CardDb {
    /// Compile a parsed Forge script and add it. Always succeeds; problems go to `unsupported`.
    pub fn add(&mut self, script: &CardScript) -> CardId {
        self.lt.compiling_wish = false;
        let mut uns = vec![];
        let layout = match script.alternate_mode.as_deref() {
            None => Layout::Normal,
            Some("Adventure") => Layout::Adventure,
            Some("Omen") => Layout::Omen,
            Some("Prepare") => Layout::Prepare,
            Some("DoubleFaced") => Layout::Transform,
            // Only Rooms (split permanents with a shared type line, CR 709.5); other split cards
            // stay unsupported.
            Some("Split") if script.faces.iter().all(|f| f.types.as_deref().is_some_and(|t| t.split_whitespace().any(|w| w == "Room"))) => Layout::Room,
            // Split instants/sorceries without fuse (CR 709; `ext9`).
            Some("Split")
                if script.faces.len() == 2
                    && script.faces.iter().all(|f| f.types.as_deref().is_some_and(|t| t.split_whitespace().all(|w| w == "Instant" || w == "Sorcery")))
                    && !script.faces.iter().any(|f| f.keywords.iter().any(|k| k.starts_with("Fuse") || k.starts_with("Aftermath"))) =>
            {
                Layout::Split
            }
            Some(m) => {
                uns.push(format!("layout {m}"));
                Layout::Normal
            }
        };
        // `wasCastFromExile` anywhere in the script (`ext9`): spells cast from exile are flagged.
        if script.faces.iter().any(|f| f.triggers.iter().chain(&f.statics).chain(&f.abilities).any(|p| p.pairs.iter().any(|(_, v)| v.contains("wasCastFromExile"))) || f.svars.iter().any(|s| s.value.contains("wasCastFromExile"))) {
            self.x9.any_from_exile = true;
        }
        // `Card.Self+ThisTurnEntered` anywhere in the script: entering this turn is part of
        // what its objects are (`query::obj_sig`, `ext9::X9Tables::self_entered`).
        let mentions = |pat: &str| script.faces.iter().any(|f| f.triggers.iter().chain(&f.statics).chain(&f.abilities).any(|p| p.pairs.iter().any(|(_, v)| v.contains(pat))) || f.svars.iter().any(|s| s.value.contains(pat)));
        let self_entered = mentions("Self+ThisTurnEntered");
        // `Count$ResolvedThisTurn`: how often its ability resolved this turn is part of what its
        // objects are (`ext9::X9Tables::resolved_readers`).
        let resolved_reader = mentions("ResolvedThisTurn");
        self.le.compiling_half = 0;
        let mut faces = vec![self.compile_face(&script.faces[0], &mut uns)];
        if matches!(layout, Layout::Adventure | Layout::Omen | Layout::Prepare | Layout::Transform | Layout::Room | Layout::Split) {
            self.le.compiling_half = 1;
            match script.faces.get(1) {
                Some(f) => faces.push(self.compile_face(f, &mut uns)),
                None => uns.push("alternate face missing".into()),
            }
            self.le.compiling_half = 0;
        }
        // Triggered abilities are found, counted and resolved by their index in the card's front
        // face (`triggers::fire`, `PendingTrigger::trigger_idx`). That is the permanent's current
        // face (a transformed permanent or a Room state is its own card, face 0; `check_zones`), but
        // a spell cast as an Adventure, Omen, split half, Room door or prepare spell is on the stack
        // as that face (CR 715.3b, 720.3, 709.3a, 722.3): a trigger functioning on the stack
        // ("when you cast this spell", CR 113.6) on either face would be read from the wrong face.
        // No pool card has one; refuse instead of misplaying.
        if matches!(layout, Layout::Adventure | Layout::Omen | Layout::Prepare | Layout::Room | Layout::Split) && faces.iter().any(|f| f.triggers.iter().any(|t| t.zones.has(Zone::Stack))) {
            uns.push("a trigger functioning on the stack on a card cast as another face".into());
        }
        // A split card's combined characteristics (CR 709.4): face 2.
        if layout == Layout::Split && faces.len() == 2 {
            let both = crate::ext9::split_combined(&faces[0], &faces[1]);
            faces.push(both);
        }
        let room_states = if layout == Layout::Room && faces.len() == 2 {
            let (both, none) = crate::ext5::room_faces(&faces[0], &faces[1]);
            faces.push(both.clone());
            faces.push(none.clone());
            Some([faces[1].clone(), both, none])
        } else {
            None
        };
        let id = CardId(self.cards.len() as u32);
        self.by_name.insert(name_key(script.name()), id);
        let back = (layout == Layout::Transform).then(|| faces.get(1).cloned()).flatten();
        self.cards.push(CardDef { name: script.name().to_string(), layout, faces, is_token: false, unsupported: uns.clone() });
        if self_entered {
            self.x9.self_entered.push(id);
        }
        if resolved_reader {
            self.x9.resolved_readers.push(id);
        }
        if std::mem::take(&mut self.lt.compiling_wish) {
            self.lt.wish_cards.push(id);
        }
        if let Some(st) = room_states {
            // The Room's states on the battlefield as their own cards (like a transformed DFC's
            // back face): right door only, both doors, neither (`ext5::set_room_state`).
            let mut ids = [id; 4];
            for (i, f) in st.into_iter().enumerate() {
                ids[i + 1] = CardId(self.cards.len() as u32);
                self.cards.push(CardDef { name: f.name.clone(), layout: Layout::Normal, faces: vec![f], is_token: false, unsupported: uns.clone() });
            }
            self.le.room_cards.push(ids);
        }
        if let Some(b) = back {
            // The back face as its own card (not in decks): a transformed permanent uses it.
            let bid = CardId(self.cards.len() as u32);
            self.cards.push(CardDef { name: b.name.clone(), layout: Layout::Normal, faces: vec![b], is_token: false, unsupported: uns });
            self.dfc_backs.push((id, bid));
        }
        id
    }

    pub(crate) fn add_token(&mut self, script: &CardScript) -> CardId {
        let mut uns = vec![];
        let face = self.compile_face(&script.faces[0], &mut uns);
        let id = CardId(self.cards.len() as u32);
        self.cards.push(CardDef { name: script.name().to_string(), layout: Layout::Normal, faces: vec![face], is_token: true, unsupported: uns });
        id
    }

    fn compile_face(&mut self, f: &Face, uns: &mut Vec<String>) -> FaceDef {
        // Faces compiled while compiling this one (tokens) don't see its pending spell cost.
        let outer_pending = self.lt.pending_spell_cost.take();
        // What this face compiles, for the IR audits at the end (`compile_audit`).
        let audit_from = (self.abilities.len(), self.statics_pool.len(), self.triggers_pool.len());
        self.preintern_counters(f);
        let cost = ManaCost::parse_forge(f.mana_cost.as_deref().unwrap_or("no cost")).unwrap_or_else(|e| {
            uns.push(e);
            ManaCost::default()
        });
        let (mut types, mut supertypes, mut subtypes) = (TypeSet::default(), Supertypes::default(), vec![]);
        for w in f.types.as_deref().unwrap_or("").split_whitespace() {
            match w {
                "Legendary" => supertypes.0 |= Supertypes::LEGENDARY,
                "Basic" => supertypes.0 |= Supertypes::BASIC,
                "Snow" => supertypes.0 |= Supertypes::SNOW,
                "World" => supertypes.0 |= Supertypes::WORLD,
                _ => match CardType::parse(w) {
                    Some(t) => types = types.with(t),
                    None => subtypes.push(self.intern_subtype(w)),
                },
            }
        }
        let colors = match &f.colors {
            Some(c) => {
                let mut cs = ColorSet::default();
                for w in c.split(',') {
                    match Color::from_word(w.trim()) {
                        Some(col) => cs = cs.with(col),
                        None if w.trim() == "colorless" => {}
                        None => uns.push(format!("color {w}")),
                    }
                }
                cs
            }
            None => cost.colors(),
        };
        let (mut power, mut toughness) = (None, None);
        // Characteristic-defining P/T (CR 604.3): `*/*` with a `CharacteristicDefining$` static.
        let cda_pt = match self.cda_pt(f) {
            Ok(c) => c,
            Err(e) => {
                uns.push(e);
                None
            }
        };
        if let Some(pt) = &f.pt {
            let mut it = pt.split('/');
            match (it.next().and_then(|p| p.parse().ok()), it.next().and_then(|t| t.parse().ok())) {
                (Some(p), Some(t)) => {
                    power = Some(p);
                    toughness = Some(t)
                }
                // `*/*`, `*/1+*` (Tarmogoyf): both set by the characteristic-defining ability.
                // Printed values with * = 0 (CR 208.2a), used when the CDA doesn't apply (the
                // creature lost all abilities, 613.6; Merfolk Trickster ruling 2018-04-27: "*/*+1
                // ... it's 0/1").
                _ if (pt == "*/*" || pt == "*/1+*") && cda_pt.is_some() => {
                    power = Some(0);
                    toughness = Some(if pt == "*/1+*" { 1 } else { 0 });
                }
                // `*/N`: power defined by the CDA (`ext2::cda_pt`), printed toughness N.
                (None, Some(t)) if pt.starts_with("*/") && cda_pt.is_some() => {
                    power = Some(0);
                    toughness = Some(t);
                }
                _ => uns.push(format!("PT {pt}")),
            }
        }
        let loyalty = f.loyalty.as_ref().and_then(|l| l.parse().ok());
        let mut kws = KwSet::default();
        let mut etb_counters = vec![];
        let mut kicker = None;
        let mut flashback = None;
        let mut hand_abilities = vec![];
        let mut enchant = None;
        let mut opening_hand_battlefield = false;
        let mut synth_activated: Vec<AbilityId> = vec![];
        let mut has_crew = false;
        let mut synth_triggers: Vec<TriggerDef> = vec![];
        let self_filter = Filter { alts: vec![crate::filter::Alt { base: crate::filter::Base::Card, props: vec![crate::filter::Prop::SelfCard] }] };
        let base_ab = |effect: Effect, defined: Option<Defined>| AbilityDef {
            kind: AbilityKind::Sub,
            effect,
            defined,
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
        let mut replacements = vec![];
        let mut class_statics: Vec<StaticDef> = vec![];
        let mut alt = crate::altcast::AltDef::default();
        let mut extra = FaceExtra::default();
        let mut gift = false;
        for k in &f.keywords {
            if let Some(kw) = Kw::parse(k) {
                kws = kws.with(kw);
                continue;
            }
            let parts: Vec<&str> = k.split(':').collect();
            match parts.as_slice() {
                ["etbCounter", kind, n] => match n.parse::<i32>() {
                    Ok(n) => {
                        let c = self.counter_kind(kind);
                        etb_counters.push((c, n))
                    }
                    // CR 107.3m: X in "enters with X counters" of a permanent spell is the X paid.
                    _ if *n == "X" && f.svar("X").is_some_and(|v| v.value.trim() == "Count$xPaid") => {
                        let c = self.counter_kind(kind);
                        extra.etb_counters_x.push(c);
                    }
                    _ => uns.push(format!("keyword {k}")),
                },
                // `etbCounter:TYPE:SVAR:no Condition:<text>` ("enters with a counter for each
                // ...", the amount evaluated as it enters; `ext5`).
                ["etbCounter", kind, n, "no Condition", _] if f.svar(n).is_some() && f.svar(n).is_some_and(|v| v.value.trim() != "Count$xPaid") => match self.value(f, n) {
                    Ok(e) => {
                        let c = self.counter_kind(kind);
                        extra.etb_counters_expr.push((c, e));
                    }
                    Err(e) => uns.push(e),
                },
                ["Start your engines"] => extra.start_engines = true,
                // CR 716.2a: class level bars.
                ["Class", level, cost, adds] => {
                    if let Err(e) = self.x_class_level(f, level, cost, adds, &mut synth_activated, &mut synth_triggers, &mut class_statics, uns) {
                        uns.push(e);
                    }
                }
                ["Storied"] => extra.storied = true,
                // CR 702.174 (`ext7::x7_gift`, after the spell ability is compiled).
                ["Gift"] => gift = true,
                // CR 702.192a (`ext7`).
                ["Paradigm"] => {
                    extra.paradigm = true;
                    self.x7_paradigm_ability();
                }
                ["Changeling"] => extra.changeling = true,
                // CR 702.184a: "Tap another untapped creature you control: Put a number of charge
                // counters on this permanent equal to the tapped creature's power. Activate only
                // as a sorcery."
                ["Station", _] => {
                    let charge = self.counter_kind("CHARGE");
                    let mut a = base_ab(Effect::Ext(crate::effect_ext::ExtEffect::Le(crate::ext5::LeEffect::Station { kind: charge })), Some(Defined::SelfObj));
                    a.kind = AbilityKind::Activated;
                    a.cost = Some(Cost { tap_x: Some((1, self.filter("Creature.Other+YouCtrl").unwrap())), ..Default::default() });
                    a.sorcery_speed = true;
                    a.text = "Station".into();
                    synth_activated.push(self.push_ability(a));
                }
                ["Kicker", c] => match ManaCost::parse_forge(c) {
                    Ok(mc) => kicker = Some(mc),
                    Err(e) => uns.push(e),
                },
                ["Flashback", c] if !c.contains('<') => match ManaCost::parse_forge(c) {
                    Ok(mc) => flashback = Some(mc),
                    Err(e) => uns.push(e),
                },
                ["Cycling", c] if !c.contains('<') => match ManaCost::parse_forge(c) {
                    Ok(mc) => {
                        let mut a = base_ab(Effect::Draw { n: Expr::Num(1) }, Some(Defined::You));
                        a.kind = AbilityKind::Activated;
                        a.cost = Some(Cost { mana: mc, discard_self: true, ..Default::default() });
                        a.text = format!("Cycling {c}");
                        hand_abilities.push(self.push_ability(a));
                    }
                    Err(e) => uns.push(e),
                },
                ["Ward", c] => {
                    // CR 702.21a: counter the targeting spell/ability unless its controller pays.
                    let cost = if let Some(n) = c.strip_prefix("PayLife<").and_then(|r| r.strip_suffix('>')) {
                        // `PayLife<X/description>` with X an SVar: life given by an expression.
                        let n = n.split('/').next().unwrap_or(n);
                        match n.parse() {
                            Ok(k) => Some(UnlessCost::Life(k)),
                            Err(_) if f.svar(n).is_some() => self.value(f, n).ok().map(UnlessCost::LifeExpr),
                            Err(_) => None,
                        }
                    } else if !c.contains('<') {
                        ManaCost::parse_forge(c).ok().map(UnlessCost::Mana)
                    } else {
                        None
                    };
                    match cost {
                        Some(cost) => {
                            let mut a = base_ab(Effect::Counter, Some(Defined::Triggered));
                            a.unless = Some((cost, Defined::TriggeredController));
                            let execute = self.push_ability(a);
                            synth_triggers.push(TriggerDef {
                                mode: TrigMode::BecomesTarget { valid: self_filter.clone(), by: PlayerSel::Opponent },
                                zones: ZoneSet::of(Zone::Battlefield),
                                execute,
                                optional: false,
                                intervening: vec![],
                                per_turn: None,
                                text: format!("Ward {c}"),
                            });
                        }
                        None => uns.push(format!("keyword {k}")),
                    }
                }
                ["Ninjutsu", c] if !c.contains('<') => match ManaCost::parse_forge(c) {
                    // CR 702.49a: an activated ability functioning from the hand.
                    Ok(mc) => {
                        let mut a = base_ab(Effect::NinjutsuEnter, Some(Defined::SelfObj));
                        a.kind = AbilityKind::Activated;
                        a.cost = Some(Cost { mana: mc, return_unblocked: true, ..Default::default() });
                        a.text = format!("Ninjutsu {c}");
                        hand_abilities.push(self.push_ability(a));
                    }
                    Err(e) => uns.push(e),
                },
                ["Equip", c, rest @ ..] if !c.contains('<') && (rest.is_empty() || matches!(rest, ["", "", rc, _] if rc.starts_with("ReduceCost$ "))) => match ManaCost::parse_forge(c) {
                    // CR 702.6a: "[Cost]: Attach this permanent to target creature you control.
                    // Activate only as a sorcery." Optionally "This ability costs {1} less to
                    // activate for each ..." (`ReduceCost$ X`, CR 601.2f).
                    Ok(mc) => {
                        let reduce = match rest {
                            [_, _, rc, _] => match self.value(f, rc.trim_start_matches("ReduceCost$ ")) {
                                Ok(e) => Some(e),
                                Err(e) => {
                                    uns.push(e);
                                    None
                                }
                            },
                            _ => None,
                        };
                        let mut a = base_ab(Effect::Attach, None);
                        a.kind = AbilityKind::Activated;
                        a.cost = Some(Cost { mana: mc, ..Default::default() });
                        a.sorcery_speed = true;
                        a.target = Some(TargetSpec { filter: self.filter("Creature.YouCtrl").unwrap(), kind: TargetKind::Normal, min: 1, max: 1, unique: false, max_expr: None, by_promise: None });
                        a.text = format!("Equip {c}");
                        let id = self.push_ability(a);
                        if let Some(e) = reduce {
                            self.ability_reduce.push((id, e));
                        }
                        synth_activated.push(id);
                    }
                    Err(e) => uns.push(e),
                },
                ["Enchant", v, ..] if enchant.is_none() => match self.filter_f(f, v) {
                    Ok(flt) if flt.alts.iter().any(|a| matches!(a.base, crate::filter::Base::Player | crate::filter::Base::Opponent | crate::filter::Base::You | crate::filter::Base::Any)) => uns.push(format!("keyword {k}: enchanting players")),
                    Ok(flt) => enchant = Some(flt),
                    Err(e) => uns.push(e),
                },
                ["MayEffectFromOpeningHand", svar] => match f.svar(svar).and_then(|s| s.as_ability()) {
                    // Only the leyline form: put this card from the opening hand onto the battlefield.
                    Some(ab) if ab.api() == Some(("DB", "ChangeZone")) && ab.get("Defined") == Some("Self") && ab.get("Origin") == Some("Hand") && ab.get("Destination") == Some("Battlefield") && ab.pairs.len() <= 5 => opening_hand_battlefield = true,
                    _ => uns.push(format!("keyword {k}")),
                },
                ["Craft", rest] => match rest.split_once(" ExileCtrlOrGrave<").and_then(|(c, m)| Some((c, m.strip_suffix('>')?))).map(|(c, m)| {
                    // "Craft with N or more": `<cost> XMin<N> ExileCtrlOrGrave<X/...>`.
                    match c.rsplit_once(" XMin").and_then(|(c2, n)| n.parse::<u8>().ok().map(|n| (c2, n))) {
                        Some((c2, n)) if m.starts_with("X/") && f.svar("X").is_some_and(|v| v.value.trim() == "Count$xPaid") => (c2, format!("{n}/{}", &m[2..]), true),
                        _ => (c, m.to_string(), false),
                    }
                }) {
                    // CR 702.167a: "[Cost], Exile this permanent, Exile [materials] from among
                    // permanents you control and/or cards in your graveyard: Return this card to
                    // the battlefield transformed under its owner's control. Sorcery speed."
                    Some((c, mat, more)) => {
                        let mut it = mat.splitn(3, '/');
                        let n = it.next().and_then(|x| x.parse::<u8>().ok());
                        let flt = it.next().map(|t| self.filter_f(f, t));
                        match (ManaCost::parse_forge(c), n, flt) {
                            (Ok(mc), Some(n), Some(Ok(flt))) => {
                                let mut a = base_ab(Effect::ReturnTransformed, None);
                                a.kind = AbilityKind::Activated;
                                a.cost = Some(Cost { mana: mc, exile_self: true, exile_ctrl_or_grave: Some((n, flt)), craft_more: more, ..Default::default() });
                                a.sorcery_speed = true;
                                a.text = format!("Craft {rest}");
                                synth_activated.push(self.push_ability(a));
                            }
                            _ => uns.push(format!("keyword {k}")),
                        }
                    }
                    None => uns.push(format!("keyword {k}")),
                },
                ["Crew", n] => match n.parse::<u8>() {
                    // CR 702.122a: "Tap any number of other untapped creatures you control with
                    // total power N or greater: This permanent becomes an artifact creature until
                    // end of turn."
                    Ok(n) => {
                        let mut a = base_ab(
                            Effect::Animate { power: None, toughness: None, types: TypeSet::default().with(CardType::Artifact).with(CardType::Creature), subtypes: vec![], remove_card_types: false, remove_creature_types: false, colors: None, overwrite_colors: false, kws: KwSet::default(), abilities: vec![], duration: Duration::EndOfTurn },
                            Some(Defined::SelfObj),
                        );
                        a.kind = AbilityKind::Activated;
                        a.cost = Some(Cost { crew: Some(n), ..Default::default() });
                        a.text = format!("Crew {n}");
                        synth_activated.push(self.push_ability(a));
                        has_crew = true;
                    }
                    Err(_) => uns.push(format!("keyword {k}")),
                },
                ["Chapter", n, svars] => {
                    // CR 714.2: chapter symbols are triggered abilities.
                    self.counter_kind("LORE");
                    let list: Vec<&str> = svars.split(',').map(|x| x.trim()).collect();
                    if n.parse::<usize>().ok() != Some(list.len()) {
                        uns.push(format!("keyword {k}"));
                    }
                    for (i, sv) in list.iter().enumerate() {
                        match f.svar(sv).and_then(|s| s.as_ability()) {
                            Some(ab) => {
                                if let Some(execute) = self.compile_ability(&ab, f, uns, 1, false) {
                                    synth_triggers.push(TriggerDef { mode: TrigMode::Chapter { n: i as u8 + 1 }, zones: ZoneSet::of(Zone::Battlefield), execute, optional: false, intervening: vec![], per_turn: None, text: format!("Chapter {}", i + 1) });
                                }
                            }
                            None => uns.push(format!("keyword {k}: {sv} missing")),
                        }
                    }
                }
                ["ETBReplacement", "Copy", svar, rest @ ..] if rest.is_empty() || rest == ["Optional"] => match f.svar(svar).and_then(|s| s.as_ability()) {
                    // "You may have this enter as a copy of ..." (CR 707, 614.1c).
                    Some(ab) if ab.api() == Some(("DB", "Clone")) => {
                        if let Some(a) = self.compile_ability(&ab, f, uns, 1, true) {
                            if let Effect::Clone { optional, .. } = &mut self.abilities[a.0 as usize].effect {
                                *optional = rest == ["Optional"];
                            }
                            replacements.push(ReplacementDef { origin: ZoneSet::ANY, dest: ZoneSet::of(Zone::Battlefield), valid: self_filter.clone(), zones: None, action: ReplAction::Modify(a), text: k.clone() });
                        }
                    }
                    _ => uns.push(format!("keyword {k}")),
                },
                ["ETBReplacement", "Other", svar] => match f.svar(svar).and_then(|s| s.as_ability()) {
                    Some(ab) => {
                        if let Some(a) = self.compile_ability(&ab, f, uns, 1, true) {
                            replacements.push(ReplacementDef {
                                origin: ZoneSet::ANY,
                                dest: ZoneSet::of(Zone::Battlefield),
                                valid: Filter { alts: vec![crate::filter::Alt { base: crate::filter::Base::Card, props: vec![crate::filter::Prop::SelfCard] }] },
                                zones: None,
                                action: ReplAction::Modify(a),
                                text: k.clone(),
                            });
                        }
                    }
                    None => uns.push(format!("keyword {k}")),
                },
                // "Other [matching permanents] enter with ..." (Gev): a replacement of another
                // permanent's entering (CR 614.1c), active while this is on the battlefield.
                ["ETBReplacement", "Other", svar, "Mandatory", "Battlefield", valid] => match (f.svar(svar).and_then(|s| s.as_ability()), self.filter_f(f, valid)) {
                    (Some(ab), Ok(vf)) => {
                        if let Some(a) = self.compile_ability(&ab, f, uns, 1, true) {
                            replacements.push(ReplacementDef { origin: ZoneSet::ANY, dest: ZoneSet::of(Zone::Battlefield), valid: vf, zones: Some(ZoneSet::of(Zone::Battlefield)), action: ReplAction::Modify(a), text: k.clone() });
                        }
                    }
                    _ => uns.push(format!("keyword {k}")),
                },
                // CR 702.189a (`ext9`).
                ["Firebending", n] => {
                    if let Err(e) = self.x9_firebending(n, &mut synth_triggers) {
                        uns.push(e);
                    }
                }
                _ => match self.alt_keyword(&parts, &mut alt, &mut synth_triggers) {
                    Some(Ok(())) => {}
                    Some(Err(e)) => uns.push(e),
                    None => uns.push(format!("keyword {k}")),
                },
            }
        }
        if kws.has(Kw::Prowess) {
            // CR 702.108a: whenever you cast a noncreature spell, +1/+1 until end of turn.
            let execute = self.push_ability(base_ab(
                Effect::Pump { power: Expr::Num(1), toughness: Expr::Num(1), kws: KwSet::default(), duration: Duration::EndOfTurn },
                Some(Defined::SelfObj),
            ));
            let noncreature = Filter { alts: vec![crate::filter::Alt { base: crate::filter::Base::Card, props: vec![crate::filter::Prop::NonType(CardType::Creature)] }] };
            synth_triggers.push(TriggerDef {
                mode: TrigMode::SpellCast { valid: noncreature, caster: PlayerSel::You },
                zones: ZoneSet::of(Zone::Battlefield),
                execute,
                optional: false,
                intervening: vec![],
                per_turn: None,
                text: "Prowess".into(),
            });
        }
        let mut spell = None;
        let mut activated = synth_activated;
        let mut grave_abilities = vec![];
        for a in &f.abilities {
            if a.api() == Some(("SP", "PermanentCreature")) && a.get("Cost").is_some_and(|c| c.contains("ExileFromGrave<")) {
                // "As an additional cost to cast this spell, exile N cards from your graveyard"
                // (Abhorrent Oculus): a mandatory additional cost (CR 601.2b, 601.2h).
                match self.exile_grave_spell(a, f) {
                    Ok(c) if alt.extra_cost.is_none() => alt.extra_cost = Some(c),
                    Ok(_) => uns.push("two mandatory additional costs".into()),
                    Err(e) => uns.push(e),
                }
                continue;
            }
            if a.api() == Some(("SP", "PermanentCreature")) {
                match self.sac_any_spell(a, f) {
                    Ok(flt) => extra.sac_any_reduce = Some(flt),
                    Err(e) => uns.push(e),
                }
                continue;
            }
            if let Some(id) = self.compile_ability(a, f, uns, 0, false) {
                match self.abilities[id.0 as usize].kind {
                    AbilityKind::Spell => spell = Some(id),
                    _ if a.get("ActivationZone") == Some("Graveyard") => grave_abilities.push(id),
                    // Activated abilities that function from the hand (CR 113.6k, e.g. "{1}{B},
                    // Discard this card: ..."), like cycling.
                    _ if a.get("ActivationZone") == Some("Hand") => hand_abilities.push(id),
                    _ => activated.push(id),
                }
            }
        }
        if let Some(c) = self.lt.pending_spell_cost.take() {
            if alt.either.is_some() || spell.is_none() {
                uns.push("mandatory additional cost with another additional cost".into());
            }
            alt.extra_cost = Some(c);
        }
        // Intrinsic mana abilities of basic land types (CR 305.6).
        if types.has(CardType::Land) {
            for (name, mt) in [("Plains", ManaType::W), ("Island", ManaType::U), ("Swamp", ManaType::B), ("Mountain", ManaType::R), ("Forest", ManaType::G)] {
                if subtypes.contains(&self.intern_subtype(name)) {
                    let id = self.push_ability(AbilityDef {
                        kind: AbilityKind::Activated,
                        effect: Effect::Mana { produced: Produced::One(mt), amount: 1 },
                        defined: None,
                        target: None,
                        cost: Some(Cost { tap: true, ..Default::default() }),
                        sorcery_speed: false,
                        is_mana: true,
                        activate_if: vec![],
                        conditions: vec![],
                        unless: None,
                        optional: false,
                        remember: false,
                        sub: None,
                        text: format!("{{T}}: Add {{{}}}.", mt.sym()),
                    });
                    activated.push(id);
                }
            }
        }
        let mut synth_triggers = synth_triggers;
        if gift {
            let permanent = !(types.has(CardType::Instant) || types.has(CardType::Sorcery));
            if let Err(e) = self.x7_gift(f, permanent, &mut spell, &mut synth_triggers, uns) {
                uns.push(e);
            }
        }
        let mut triggers = synth_triggers;
        for t in &f.triggers {
            match self.compile_trigger(t, f, uns) {
                Ok(Some(td)) => triggers.push(td),
                Ok(None) => {}
                Err(e) => uns.push(e),
            }
        }
        let mut statics = class_statics;
        for s in &f.statics {
            if s.get("CharacteristicDefining").is_some() {
                continue; // compiled into `cda_pt`
            }
            // "Other creatures you control have ward ..." (`ext4`): the ward part becomes a ward
            // trigger on this source; the rest of the static compiles as usual.
            // "Max speed — This creature has double strike." (`AddStaticAbility$` of a self static
            // granting keywords to itself): the same as the keywords under the outer condition.
            let flat = crate::ext4::flatten_self_static(s, f);
            let s = flat.as_ref().unwrap_or(s);
            if let Some((rest, cost)) = crate::ext4::split_granted_ward(s) {
                match self.compile_static(&rest, f) {
                    Ok(sd) => {
                        match self.filter_f(f, s.get("Affected").unwrap_or("Card")).ok().and_then(|valid| self.ward_trigger(&cost, valid, &sd.conditions)) {
                            Some(t) => triggers.push(t),
                            None => uns.push(format!("granted ward {cost}")),
                        }
                        let player = self.take_player_hexproof(&sd);
                        statics.push(sd);
                        statics.extend(player);
                    }
                    Err(e) => uns.push(e),
                }
                continue;
            }
            match self.compile_static(s, f) {
                Ok(sd) => {
                    let player = self.take_player_hexproof(&sd);
                    statics.push(sd);
                    statics.extend(player);
                }
                Err(e) => uns.push(e),
            }
        }
        // Ward granted by an Equipment/Aura (or self) static: CR 702.21a's trigger, put on the
        // source and matching the affected object. A later layer-6 "loses all abilities" of the
        // affected object removes it (`triggers::fire`: `query::grant_removed`, by timestamp).
        for st in &statics {
            if let StaticKind::DefinedContinuous(dc) = &st.kind {
                if let Some(c) = &dc.ward {
                    let prop = match dc.who {
                        StaticWho::SelfObj => crate::filter::Prop::SelfCard,
                        StaticWho::Attached => crate::filter::Prop::AttachedBySource,
                    };
                    let valid = Filter { alts: vec![crate::filter::Alt { base: crate::filter::Base::Card, props: vec![prop] }] };
                    match self.ward_trigger(c, valid, &st.conditions) {
                        Some(t) => triggers.push(t),
                        None => uns.push(format!("granted ward {c}")),
                    }
                }
            }
        }
        for r in &f.replacements {
            match self.compile_replacement(r, f, uns) {
                Ok(Some(rd)) => replacements.push(rd),
                Ok(None) => {}
                Err(e) => uns.push(e),
            }
        }
        statics.append(&mut self.pending_statics);
        let has_marker = statics.iter().any(|st| matches!(st.kind, StaticKind::Ext(crate::static_ext::StaticExt::SacrificeReduces)));
        if has_marker != extra.sac_any_reduce.is_some() {
            uns.push("sacrifice-to-reduce cost without its parts".into());
        }
        if types.has(CardType::Battle) {
            uns.push("battle".into());
        }
        if types.has(CardType::Planeswalker) && loyalty.is_none() {
            uns.push("planeswalker without loyalty".into());
        }
        if spell.is_none() && (types.has(CardType::Instant) || types.has(CardType::Sorcery)) && uns.is_empty() {
            uns.push("instant/sorcery without spell ability".into());
        }
        if types.has(CardType::Enchantment) && subtypes.iter().any(|&s| self.subtypes[s as usize] == "Aura") {
            // CR 303.4a: an Aura spell targets what its enchant ability allows.
            match (&enchant, spell) {
                (Some(e), None) => {
                    let mut a = base_ab(Effect::AuraEnter, None);
                    a.kind = AbilityKind::Spell;
                    a.target = Some(TargetSpec { filter: e.clone(), kind: TargetKind::Normal, min: 1, max: 1, unique: false, max_expr: None, by_promise: None });
                    spell = Some(self.push_ability(a));
                }
                _ => uns.push("Aura without Enchant".into()),
            }
        } else if enchant.is_some() {
            uns.push("Enchant on a non-Aura".into());
        }
        if types.has(CardType::Artifact) && subtypes.iter().any(|&s| matches!(self.subtypes[s as usize].as_str(), "Vehicle")) && !has_crew {
            uns.push("Vehicle".into());
        }
        if f.copy_face_from.is_some() {
            uns.push("CopyFaceFrom not resolved".into());
        }
        // `cast::mana_sources` merges a permanent's mana abilities into one source with one
        // amount: unrestricted abilities producing different amounts can't be represented (a
        // restricted one only counts for spells it may pay for, Tablet of Discovery).
        let amounts: Vec<u8> = activated.iter().filter_map(|&a| match &self.abilities[a.0 as usize].effect {
            Effect::Mana { amount, .. } if self.abilities[a.0 as usize].is_mana && !self.mana_restrict.iter().any(|r| r.0 == a) => Some(*amount),
            _ => None,
        }).collect();
        if amounts.windows(2).any(|w| w[0] != w[1]) {
            uns.push("mana abilities producing different amounts".into());
        }
        // `cast::mana_sources` models one "{T}, Tap an untapped [permanent]: Add ..." ability per
        // permanent (`Src::fodder_ability`).
        if activated.iter().filter(|&&a| self.abilities[a.0 as usize].is_mana && self.abilities[a.0 as usize].cost.as_ref().is_some_and(|c| c.tap_x.is_some())).count() > 1 {
            uns.push("two tap-another-permanent mana abilities".into());
        }
        if activated.iter().any(|a| self.lt.power_up.contains(a)) && cost.syms.iter().any(|s| !matches!(s, ManaSym::Generic(_) | ManaSym::Colored(_) | ManaSym::Colorless)) {
            uns.push("power-up with a hybrid/X mana cost".into());
        }
        if f.other.iter().any(|(k, _)| k == "AlternateMode") {
            uns.push("alternate mode".into());
        }
        self.lt.pending_spell_cost = outer_pending;
        // Loud compile checks (`compile_audit`): a permanent's statics and replacements have no
        // memory to read; a player alternative outside a target's filter matches nothing.
        for st in &statics {
            match crate::compile_audit::static_reads_memory(st) {
                Ok(false) => {}
                Ok(true) => uns.push(format!("static {:?} reads IsRemembered (Forge card-level memory)", st.text.chars().take(60).collect::<String>())),
                Err(e) => uns.push(e),
            }
        }
        for r in &replacements {
            match crate::compile_audit::replacement_reads_memory(r) {
                Ok(false) => {}
                Ok(true) => uns.push(format!("replacement {:?} reads IsRemembered (Forge card-level memory)", r.text.chars().take(60).collect::<String>())),
                Err(e) => uns.push(e),
            }
        }
        match crate::compile_audit::face_player_alts(self, audit_from.0..self.abilities.len(), &triggers, &statics, &replacements, (audit_from.1, audit_from.2)) {
            Ok(v) => uns.extend(v),
            Err(e) => uns.push(e),
        }
        FaceDef {
            name: f.name.clone(),
            cost,
            types,
            supertypes,
            subtypes,
            colors,
            power,
            toughness,
            loyalty,
            kws,
            spell,
            activated,
            triggers,
            statics,
            replacements,
            etb_counters,
            kicker,
            flashback,
            hand_abilities,
            grave_abilities,
            cda_pt,
            alt,
            enchant,
            opening_hand_battlefield,
            oracle: f.oracle.clone().unwrap_or_default(),
            extra,
        }
    }

    fn exprs<'a>(&'a self, f: &'a Face) -> (impl Fn(&str) -> Option<Subtype> + 'a, &'a Face) {
        (move |s: &str| self.subtype(s), f)
    }

    pub(crate) fn filter(&self, s: &str) -> Res<Filter> {
        let mut intern = |x: &str| self.subtype(x);
        Filter::parse(s, &mut intern)
    }

    /// Filter whose comparisons may refer to the face's SVars (`cmcLEX`) and counter names.
    pub(crate) fn filter_f(&mut self, f: &Face, s: &str) -> Res<Filter> {
        let rewritten = crate::ext7::rewrite_filter(self, s);
        let s = rewritten.as_deref().unwrap_or(s);
        // Intern counter names mentioned in `counters_..._NAME` props up front.
        for part in s.split(['+', ',', '.']) {
            if let Some(rest) = part.strip_prefix("counters_") {
                if let Some((_, name)) = rest.split_once('_') {
                    self.counter_kind(name);
                }
            }
        }
        let db: &CardDb = self;
        let mut intern = |x: &str| db.subtype(x);
        let svar = |name: &str| db.value(f, name).ok().filter(|e| !matches!(e, Expr::Num(_)) || f.svar(name).is_some());
        let counter = |name: &str| match name {
            "P1P1" => Some(CounterKind::P1P1),
            "M1M1" => Some(CounterKind::M1M1),
            "LOYALTY" => Some(CounterKind::Loyalty),
            "STUN" => Some(CounterKind::Stun),
            n => db.counter_names.iter().position(|c| c == n).map(|i| CounterKind::Named(i as u16)),
        };
        Filter::parse_with(s, &mut intern, &svar, &counter)
    }

    pub(crate) fn value(&self, f: &Face, v: &str) -> Res<Expr> {
        // The reflexive trigger's amount (`ext9::X9Effect::DiscardReflexiveMaxMv`).
        if self.x9.compiling_max_mv && f.svar(v).is_some_and(|s| s.value.trim() == "TriggerRemembered$GreatestCardManaCost") {
            return Ok(Expr::TriggerAmount);
        }
        let (intern, face) = self.exprs(f);
        ExprCompiler { face, intern: &intern, counters: &self.counter_names }.value(v)
    }

    /// `ConditionPresent`, `ConditionCheckSVar`, `ConditionPlayerTurn`, `IsPresent`, `CheckSVar`.
    pub(crate) fn conditions(&self, p: &P, f: &Face, prefix: &str) -> Res<Vec<Condition>> {
        let (intern, face) = self.exprs(f);
        let ec = ExprCompiler { face, intern: &intern, counters: &self.counter_names };
        let mut out = vec![];
        let k = |s: &str| format!("{prefix}{s}");
        if prefix == "Condition" {
            if p.peek("ConditionZone") == Some("Stack") && p.peek(&k("Present")) == Some("Card.Self+PromisedGift") {
                // "If the gift was promised" (CR 702.174k; `ext7`).
                p.ack("ConditionZone", &["Stack"], "PromisedGift condition");
                p.ack(&k("Present"), &["Card.Self+PromisedGift"], "PromisedGift condition");
                if !matches!(p.get(&k("Compare")), None | Some("EQ1") | Some("GE1")) {
                    return Err("PromisedGift condition form".into());
                }
                out.push(crate::ext7::promised());
            } else if p.peek("ConditionDefined") == Some("Collected") {
                // "If evidence was collected" (CR 701.59c): the optional additional cost (collect
                // evidence) was paid for this spell.
                p.ack("ConditionDefined", &["Collected"], "collected-evidence condition");
                if p.get(&k("Present")) != Some("Card") || p.get(&k("Compare")).is_some_and(|c| c != "GE1") {
                    return Err("ConditionDefined Collected form".into());
                }
                out.push(Condition::Compare(Expr::Alt(crate::altcast::AltExpr::CastFlag(crate::altcast::flag::OPTCOST, 1, 0)), crate::filter::CmpOp::GE, Expr::Num(1)));
            } else if let Some(d) = p.get("ConditionDefined") {
                // Count the defined objects (not the battlefield) matching ConditionPresent.
                let pres = p.get(&k("Present")).ok_or("ConditionDefined without ConditionPresent")?;
                let (op, n) = crate::expr::parse_cmp(p.get(&k("Compare")).unwrap_or("GE1"))?;
                out.push(Condition::Defined(self.defined(d)?, ec.filter(pres)?, op, n));
            } else if let Some(pres) = p.get(&k("Present")) {
                let cmp = p.get(&k("Compare"));
                // "If there are N or more counters on [this]": last known information (`ExprCompiler::self_counters`).
                // Not on `ImmediateTrigger` ("when you do, if it has ..."): there the condition also
                // stands in for "when you do" (a counter put on a source that left is never put),
                // which nothing else checks.
                let self_counters = match p.peek("ConditionZone").is_none() && !is_immediate_trigger(p) {
                    true => ec.self_counters(pres, cmp)?,
                    false => None,
                };
                match self_counters {
                    Some(c) => out.push(c),
                    None => out.push(ec.present(pres, cmp)?),
                }
            }
            if let Some(sv) = p.get(&k("CheckSVar")) {
                out.push(ec.check_svar(sv, p.get(&k("SVarCompare")))?);
            }
            if let Some(t) = p.bool_val(&k("PlayerTurn")) {
                out.push(Condition::YourTurn(t));
            }
            // "If this spell was bargained" (CR 702.166b).
            if p.peek("Condition") == Some("Bargain") {
                p.ack("Condition", &["Bargain"], "bargained condition");
                out.push(Condition::Compare(Expr::Alt(crate::altcast::AltExpr::CastFlag(crate::altcast::flag::OPTCOST, 1, 0)), crate::filter::CmpOp::GE, Expr::Num(1)));
            }
        } else {
            if let Some(pres) = p.get("IsPresent") {
                // "If [this] has N or more counters": last known information (an intervening "if"
                // checked again on resolution, CR 603.4, 608.2h; `ExprCompiler::self_counters`).
                let lki = match p.peek("PresentZone").is_none() && !is_immediate_trigger(p) {
                    true => ec.self_counters(pres, p.peek("PresentCompare"))?,
                    false => None,
                };
                if let Some(c) = lki {
                    p.ack("PresentCompare", &["GE1", "EQ1"], "checked by self_counters");
                    out.push(c);
                } else {
                    // `PresentZone$`: count in that zone instead of the battlefield (Ghitu Lavarunner).
                    let mut c = ec.present(pres, p.get("PresentCompare"))?;
                    if let (Some(z), Condition::Compare(Expr::CountValid(zs, _), ..)) = (p.get("PresentZone"), &mut c) {
                        *zs = match z {
                            "Graveyard" => crate::expr::ZoneSel::Graveyard,
                            "Exile" => crate::expr::ZoneSel::Exile,
                            "Hand" => crate::expr::ZoneSel::Hand,
                            "Battlefield" => crate::expr::ZoneSel::Battlefield,
                            z => return Err(format!("PresentZone {z}")),
                        };
                    }
                    out.push(c);
                }
            }
            if let Some(sv) = p.get("CheckSVar") {
                out.push(ec.check_svar(sv, p.get("SVarCompare"))?);
            }
        }
        Ok(out)
    }

    pub(crate) fn defined(&self, v: &str) -> Res<Defined> {
        Ok(match v {
            "You" => Defined::You,
            "Self" => Defined::SelfObj,
            "Opponent" | "Player.Opponent" => Defined::Opponent,
            // Gift (CR 702.174a): the opponent chosen as the gift was promised; two players, so
            // the opponent (`ext7`).
            "Promised" => Defined::Opponent,
            "Targeted" | "ParentTarget" => Defined::Targeted,
            "TargetedController" => Defined::TargetedController,
            "TargetedPlayer" => Defined::TargetedPlayer,
            "TriggeredCard" | "TriggeredNewCardLKICopy" | "TriggeredCardLKICopy" | "TriggeredAttackerLKICopy" | "TriggeredAttacker" => Defined::Triggered,
            "ChosenCard" => Defined::ChosenCard,
            "Enchanted" | "Equipped" => Defined::Attached,
            // Inside `RepeatEach | RepeatPlayers$`: the player of the current repetition.
            "Player.IsRemembered" => Defined::Remembered,
            // The object a `RememberLKI$` targeted ChangeZone moved, as it last existed: its
            // target (old ids keep last known information).
            "RememberedLKI" => Defined::Targeted,
            // The object a reflexive trigger remembered (`ImmediateTrigger | RememberObjects$`).
            "DelayTriggerRememberedLKI" | "DelayTriggerRemembered" => Defined::Triggered,
            "RememberedCard" => Defined::Remembered,
            "RememberedController" | "RememberedOwner" => Defined::RememberedController,
            "TriggeredCardController" => Defined::TriggeredController,
            // The damage source / targeting source of a DamageDone / BecomesTarget trigger
            // (Drake Hatcher): the trigger's object there (`triggers::matches`).
            "TriggeredSource" | "TriggeredSourceLKICopy" if matches!(self.le.trig_mode.as_str(), "DamageDone" | "BecomesTarget") => Defined::Triggered,
            // The caster of the spell of a SpellCast trigger (Scalding Viper).
            "TriggeredActivator" if self.le.trig_mode == "SpellCast" => Defined::TriggeredPlayer,
            "TriggeredPlayer" => Defined::TriggeredPlayer,
            "Remembered" => Defined::Remembered,
            "ReplacedCard" => Defined::Replaced,
            // `Imprint$` earlier in this resolution (Anzrag's Rampage; `ext9`).
            "Imprinted" => Defined::Imprinted,
            d => return Err(format!("Defined {d}")),
        })
    }

    /// Compile an ability line or `DB$` SVar. `etb_ok`: allow `ETB$ True` (replacement bodies).
    fn compile_ability(&mut self, params: &Params, f: &Face, uns: &mut Vec<String>, depth: u32, etb_ok: bool) -> Option<AbilityId> {
        match self.ability_inner(params, f, uns, depth, etb_ok) {
            Ok(id) => Some(id),
            Err(e) => {
                uns.push(e);
                None
            }
        }
    }

    /// Compile one node of an ability chain and its sub-abilities. `CardDb::rem_avail` says
    /// whether something before this node in its chain (or the trigger / delayed trigger / effect
    /// running it) remembers objects for it: a node reading `Remembered` / `IsRemembered`
    /// without that reads Forge's card-level memory, which the engine doesn't have, and is
    /// refused (`compile_audit`).
    pub(crate) fn ability_inner(&mut self, params: &Params, f: &Face, uns: &mut Vec<String>, depth: u32, etb_ok: bool) -> Res<AbilityId> {
        let outer = self.rem_avail;
        let writes = crate::compile_audit::writes_memory(params);
        self.rem_avail = outer || writes;
        let r = self.ability_node(params, f, uns, depth, etb_ok, outer || writes);
        self.rem_avail = outer;
        r
    }

    fn ability_node(&mut self, params: &Params, f: &Face, uns: &mut Vec<String>, depth: u32, etb_ok: bool, rem_avail: bool) -> Res<AbilityId> {
        if depth > 16 {
            return Err("sub-ability chain too deep".into());
        }
        self.pending_target = None;
        let p = P::new(params);
        let (kind, api) = if let Some(a) = p.get("SP") {
            (AbilityKind::Spell, a)
        } else if let Some(a) = p.get("AB") {
            (AbilityKind::Activated, a)
        } else if let Some(a) = p.get("DB") {
            (AbilityKind::Sub, a)
        } else {
            return Err("ability without API".into());
        };
        let num = |db: &CardDb, k: &str, default: i32| -> Res<Expr> {
            match p.get(k) {
                Some(v) => db.value(f, v),
                None => Ok(Expr::Num(default)),
            }
        };
        let kw_list = |s: Option<&str>| -> Res<KwSet> {
            let mut kws = KwSet::default();
            if let Some(k) = s {
                for w in k.split('&') {
                    kws = kws.with(crate::compile_audit::granted_kw(w, "keyword")?);
                }
            }
            Ok(kws)
        };
        let duration = |v: Option<&str>| -> Res<Duration> {
            Ok(match v {
                None => Duration::EndOfTurn,
                Some("Permanent") => Duration::Permanent,
                Some("UntilYourNextTurn") => Duration::UntilYourNextTurn,
                Some(d) => return Err(format!("Duration {d}")),
            })
        };
        // Extra nodes to run right after this one (before its SubAbility).
        let mut extra_after: Vec<AbilityDef> = vec![];
        // A charm whose number of modes depends on an optional additional cost (`charm_optcost`).
        let mut charm_optcost: Option<(u8, u8)> = None;
        // "Choose one that hasn't been chosen this turn" (`ChoiceRestriction$ ThisTurn`, `ext7`).
        let mut charm_this_turn = false;
        let mut mana_restrict: Option<(Filter, bool)> = None;
        let mut ability_only_mana = false;
        let mut enter_counters: Option<(CounterKind, i32)> = None;
        // "This ability costs {1} less to activate for each ..." (CR 601.2f).
        let ability_reduce = match p.get("ReduceCost") {
            Some(v) if kind == AbilityKind::Activated => Some(self.value(f, v)?),
            Some(_) => return Err("ReduceCost on a non-activated ability".into()),
            None => None,
        };
        let mut library_position: Option<u8> = None;
        let mut total_mv: Option<i32> = None;
        let mut must_find = false;
        let mut all_owners = false;
        let mut dig_rest_top = false;
        let mut dig_imprint = false;
        let mut exclude_triggered = false;
        let mut forget_changed = false;
        let mut this_and_targets = false;
        let mut reveal_found = false;
        let mut attached_to_parent = false;
        let mut same_owner_targets = false;
        let mut charm_max_expr: Option<Expr> = None;
        let pt = |db: &CardDb, k: &str| -> Res<Expr> {
            match p.get(k) {
                None => Ok(Expr::Num(0)),
                Some(v) => db.value(f, v.trim_start_matches('+')),
            }
        };
        let effect = match api {
            "DealDamage" if p.peek("ReplaceDyingDefined").is_some() => {
                // "If a permanent dealt damage this way would die this turn, exile it instead."
                // `Remembered` + `RememberDamaged$`, or directly `Targeted` (Combustion Technique).
                let ok = match p.get("ReplaceDyingDefined") {
                    Some("Remembered") => p.flag("RememberDamaged"),
                    Some("Targeted") => p.peek("RememberDamaged").is_none(),
                    _ => false,
                };
                if !ok || p.peek("ValidTgts").is_none() || p.peek("DamageSource").is_some() {
                    return Err("DealDamage ReplaceDyingDefined form".into());
                }
                extra_after.push(crate::compile_ext::node(Effect::Ext(crate::effect_ext::ExtEffect::Lt(crate::ext3::LtEffect::ExileIfDiesThisTurn)), Some(Defined::Targeted)));
                Effect::DealDamage { amount: num(self, "NumDmg", 0)? }
            }
            "DealDamage" => match p.get("DamageSource") {
                None => Effect::DealDamage { amount: num(self, "NumDmg", 0)? },
                Some(d) => Effect::Ext(crate::effect_ext::ExtEffect::DealDamageFrom { amount: num(self, "NumDmg", 0)?, source: self.defined(d)?, parent: d == "ParentTarget" }),
            },
            "Draw" => Effect::Draw { n: num(self, "NumCards", 1)? },
            "GainLife" => Effect::GainLife { n: num(self, "LifeAmount", 0)? },
            "LoseLife" => Effect::LoseLife { n: num(self, "LifeAmount", 0)? },
            "Destroy" => Effect::Destroy,
            // "Destroy all ... put into a graveyard this way" (`RememberDestroyed$`, `ext9`).
            "DestroyAll" if p.peek("RememberDestroyed").is_some() && !p.peek("ValidCards").is_some_and(|v| v.contains("sharesNameWith Targeted")) => {
                if !p.flag("RememberDestroyed") {
                    return Err("DestroyAll RememberDestroyed form".into());
                }
                Effect::Ext(crate::effect_ext::ExtEffect::X9(crate::ext9::X9Effect::DestroyAllRemember { valid: self.filter_f(f, p.get("ValidCards").ok_or("DestroyAll without ValidCards")?)? }))
            }
            "DestroyAll" if !p.peek("ValidCards").is_some_and(|v| v.contains("sharesNameWith Targeted")) => Effect::DestroyAll { valid: self.filter_f(f, p.get("ValidCards").ok_or("DestroyAll without ValidCards")?)? },
            "Scry" => Effect::Scry { n: num(self, "ScryNum", 1)? },
            "Surveil" => Effect::Surveil { n: num(self, "Amount", 1)? },
            "Mill" => Effect::Mill { n: num(self, "NumCards", 1)? },
            "Tap" => Effect::Tap,
            "Untap" => Effect::Untap,
            "Counter" => {
                if !matches!(p.peek("TargetType"), Some("Spell") | Some("Activated,Triggered")) && crate::ext8::stack_sel(p.peek("TargetType")).is_none() {
                    return Err("Counter: non-spell target".into());
                }
                match p.get("Destination") {
                    None | Some("Graveyard") => Effect::Counter,
                    Some("Exile") => Effect::Ext(crate::effect_ext::ExtEffect::CounterTo { zone: Zone::Exile }),
                    Some(d) => return Err(format!("Counter Destination {d}")),
                }
            }
            // "You and permanents you control gain hexproof until end of turn" (`ext7`).
            "Pump" if p.peek("Defined").is_some_and(|d| d.starts_with("You & Valid ")) => self.x7_pump_you_and(&p, f)?,
            "Pump" if params.get("NumAtt") == Some("Double") => {
                // CR 701.10b: doubling power = +X/+0, X its power as the ability resolves.
                p.ack("NumAtt", &["Double"], "doubling power (CR 701.10b)");
                if p.has("NumDef") {
                    return Err("Pump Double with NumDef".into());
                }
                let r = if params.get("ValidTgts").is_some() { crate::expr::Ref::Targeted } else if params.get("Defined").is_none_or(|d| d == "Self") { crate::expr::Ref::SelfObj } else { return Err("Pump Double Defined".into()) };
                Effect::Pump { power: Expr::Obj(r, crate::expr::ObjProp::Power), toughness: Expr::Num(0), kws: kw_list(p.get("KW"))?, duration: duration(p.get("Duration"))? }
            }
            "Pump" if p.peek("KWChoice").is_some() => {
                // "gains your choice of A or B until end of turn": chosen on resolution.
                let mut kws = vec![];
                for w in p.get("KWChoice").unwrap().split(',') {
                    kws.push(crate::compile_audit::granted_kw(w, "KWChoice")?);
                }
                if p.has("NumAtt") || p.has("NumDef") || p.has("KW") || p.has("Duration") {
                    return Err("Pump KWChoice with other changes".into());
                }
                Effect::Ext(crate::effect_ext::ExtEffect::PumpKwChoice { kws })
            }
            // "Target instant or sorcery card in your graveyard gains flashback until end of turn.
            // The flashback cost is equal to its mana cost." (CR 702.34a)
            "Pump" if p.peek("KW") == Some("Flashback") && p.peek("PumpZone") == Some("Graveyard") && p.peek("NumAtt").is_none() && p.peek("Duration").is_none() => {
                p.ack("KW", &["Flashback"], "grant flashback (CR 702.34a)");
                p.ack("PumpZone", &["Graveyard"], "grant flashback to a graveyard card");
                Effect::Ext(crate::effect_ext::ExtEffect::More(crate::ext2::MoreEffect::GrantFlashback))
            }
            "Pump" => Effect::Pump { power: pt(self, "NumAtt")?, toughness: pt(self, "NumDef")?, kws: kw_list(p.get("KW"))?, duration: duration(p.get("Duration"))? },
            "PumpAll" => Effect::PumpAll {
                valid: targeted_players_only(&p, api, self.filter_f(f, p.get("ValidCards").ok_or("PumpAll without ValidCards")?)?)?,
                power: pt(self, "NumAtt")?,
                toughness: pt(self, "NumDef")?,
                kws: kw_list(p.get("KW"))?,
                duration: duration(p.get("Duration"))?,
            },
            "Mana" if (kind != AbilityKind::Activated || p.peek("Planeswalker").is_some_and(|v| v.eq_ignore_ascii_case("true"))) && (p.peek("RestrictValid").is_some() || p.peek("Produced") == Some("Any") || p.peek("Produced") == Some("Combo Any")) => self.x_pool_mana(&p, f)?,
            "Mana" if kind != AbilityKind::Activated || p.peek("Planeswalker").is_some_and(|v| v.eq_ignore_ascii_case("true")) => {
                // Mana added by a resolving spell/triggered ability (not a mana ability, CR
                // 605.1): it goes to the mana pool (CR 106.4), e.g. Flamecache Gecko's "add {B}{R}".
                let mut v = vec![];
                for w in p.get("Produced").unwrap_or("").split_whitespace() {
                    v.push(match w {
                        "C" => ManaType::C,
                        c if c.len() == 1 => Color::from_char(c.chars().next().unwrap()).map(ManaType::of).ok_or_else(|| format!("Mana Produced {c}"))?,
                        c => return Err(format!("Mana Produced {c}")),
                    });
                }
                let n: usize = p.get("Amount").map(|a| a.parse().map_err(|_| format!("Mana Amount {a}"))).transpose()?.unwrap_or(1);
                if v.is_empty() {
                    return Err("Mana without Produced".into());
                }
                let all: Vec<ManaType> = (0..n).flat_map(|_| v.iter().copied()).collect();
                Effect::Ext(crate::effect_ext::ExtEffect::AddMana(all))
            }
            "Mana" if kind == AbilityKind::Activated && p.peek("Amount").is_some_and(|a| a.parse::<u8>().is_err()) => self.mr_mana_count(&p, f)?,
            "Mana" => {
                let prod = p.get("Produced").unwrap_or("");
                let parse_mt = |s: &str| -> Option<ManaType> {
                    match s {
                        "C" => Some(ManaType::C),
                        c if c.len() == 1 => Color::from_char(c.chars().next().unwrap()).map(ManaType::of),
                        _ => None,
                    }
                };
                let produced = if prod == "Any" {
                    Produced::AnyOf(vec![ManaType::W, ManaType::U, ManaType::B, ManaType::R, ManaType::G])
                } else if let Some(rest) = prod.strip_prefix("Combo ") {
                    Produced::AnyOf(rest.split_whitespace().map(parse_mt).collect::<Option<Vec<_>>>().ok_or_else(|| format!("Mana Produced {prod}"))?)
                } else {
                    Produced::One(parse_mt(prod).ok_or_else(|| format!("Mana Produced {prod}"))?)
                };
                let amount = match p.get("Amount") {
                    None => 1,
                    Some(a) => a.parse().map_err(|_| format!("Mana Amount {a}"))?,
                };
                if amount != 1 && matches!(produced, Produced::AnyOf(_)) {
                    return Err("Mana: multiple mana of chosen colors".into());
                }
                if p.peek("RestrictValid") == Some("Activated") {
                    // "Spend this mana only to activate abilities" (CR 106.6; `ext6`).
                    p.ack("RestrictValid", &["Activated"], "mana only for activated abilities (CR 106.6)");
                    ability_only_mana = true;
                } else
                if let Some(rv) = p.get("RestrictValid") {
                    // "Spend this mana only to cast a ... spell" (CR 106.6).
                    let rv = rv.split(',').map(|alt| alt.strip_prefix("Spell.").map(|r| format!("Card.{r}")).unwrap_or_else(|| alt.to_string())).collect::<Vec<_>>().join(",");
                    if rv.contains("Activated") || rv == "Spell" {
                        return Err(format!("Mana RestrictValid {rv}"));
                    }
                    mana_restrict = Some((self.filter_f(f, &rv)?, p.flag("AddsNoCounter")));
                }
                Effect::Mana { produced, amount }
            }
            "Discard" => {
                let mode = match p.get("Mode") {
                    Some("TgtChoose") => DiscardMode::Choose,
                    Some("RevealYouChoose") => DiscardMode::RevealYouChoose,
                    Some("Hand") => DiscardMode::Hand,
                    Some("Random") => DiscardMode::Random,
                    m => return Err(format!("Discard Mode {m:?}")),
                };
                let valid = match p.get("DiscardValid") {
                    Some(v) => Some(self.filter_f(f, v)?),
                    None => None,
                };
                match p.get("UnlessType") {
                    // "Discard N cards unless you discard a [type] card" (Winternight Stories).
                    Some(t) if mode == DiscardMode::Choose && valid.is_none() => {
                        // Plain types (Winternight Stories) or full filters (Steamcore Scholar's
                        // `Creature.withFlying,Instant,Sorcery`).
                        let unless = match self.filter_f(f, t) {
                            Ok(fl) => fl,
                            Err(_) => self.filter_f(f, &t.split(',').map(|x| format!("Card.{x}")).collect::<Vec<_>>().join(","))?,
                        };
                        Effect::Ext(crate::effect_ext::ExtEffect::More(crate::ext2::MoreEffect::DiscardUnless { n: num(self, "NumCards", 1)?, unless }))
                    }
                    Some(t) => return Err(format!("Discard UnlessType {t} with mode/valid")),
                    // "Discard up to N cards" (`Optional$` with N > 1: Forge's minimum 0).
                    None if mode == DiscardMode::Choose && p.peek("Optional").is_some_and(|v| v.eq_ignore_ascii_case("true")) && p.peek("NumCards").and_then(|v| v.parse::<i32>().ok()).is_some_and(|n| n > 1) => {
                        Effect::Ext(crate::effect_ext::ExtEffect::Mr(crate::ext4::MrEffect::DiscardUpTo { n: num(self, "NumCards", 1)?, valid }))
                    }
                    None => Effect::Discard { n: num(self, "NumCards", 1)?, mode, valid },
                }
            }
            // "Each player sacrifices ..." (`ext9`).
            "Sacrifice" if p.peek("Defined") == Some("Player") => self.x9_sacrifice_each(&p, f)?,
            "Sacrifice" => {
                // "Sacrifice any number of ..." (Pitiless Carnage, Lich-Knights' Conquest, Malevolent
                // Witchkite): `Optional$` with an amount other than 1 is Forge's "minimum 0"
                // (`SacrificeEffect`: `optional && !StrictAmount ? 0 : amount`; rulings 2024-04-12 /
                // 2023-09-01: "You can choose to sacrifice zero permanents"), the choice itself
                // being the "may" (`ext2::optional_in_effect`).
                let optional = p.peek("Optional").is_some_and(|v| v.eq_ignore_ascii_case("true") || v == "You");
                let up_to = optional && p.peek("Amount").is_some_and(|a| a.trim() != "1");
                Effect::Sacrifice { valid: self.filter_f(f, match p.get("SacValid") { None | Some("Self") => "Card.Self", Some(v) => v })?, n: num(self, "Amount", 1)?, up_to }
            }
            "PutCounter" if p.peek("CounterTypes").is_some() || p.peek("CounterType") == Some("EachFromSource") => self.lt_put_counter(&p, f)?,
            "PutCounter" => {
                // Only read by `Count$RememberedNumber`, which nothing supported evaluates.
                p.ack("RememberAmount", &["True"], "remembers the number for Count$RememberedNumber only");
                let ct = p.get("CounterType").ok_or("PutCounter without CounterType")?;
                let ck = self.counter_kind(ct);
                match p.get("DividedAsYouChoose") {
                    // "Distribute N counters among any number of targets" (CR 601.2d): divided as
                    // the spell is cast (only spells announce it here).
                    Some(d) if kind == AbilityKind::Spell && p.peek("CounterNum") == Some(d) => Effect::Ext(crate::effect_ext::ExtEffect::More(crate::ext2::MoreEffect::PutCounterDivided { kind: ck, total: num(self, "CounterNum", 1)? })),
                    Some(d) => return Err(format!("PutCounter DividedAsYouChoose {d}")),
                    None => Effect::PutCounter { kind: ck, n: num(self, "CounterNum", 1)? },
                }
            }
            "Cleanup" => {
                let clear_remembered = p.flag("ClearRemembered");
                // Chosen cards are forgotten when the ability finishes resolving anyway; so are
                // imprinted ones (`Exec::imprinted`), and no supported card reads them after this.
                p.ack("ClearChosenCard", &["True"], "chosen cards are forgotten after resolution anyway");
                p.ack("ClearImprinted", &["True"], "imprints are forgotten after resolution anyway");
                Effect::Cleanup { clear_remembered }
            }
            // Cards from outside the game (CR 400.11, "wishes").
            "ChangeZone" if p.peek("Origin").is_some_and(|o| o.contains("Sideboard")) => self.lt_wish(&p, f)?,
            "ChangeZone" => {
                let origin = ZoneSet::parse(p.get("Origin").ok_or("ChangeZone without Origin")?)?;
                let dest = match p.get("Destination").ok_or("ChangeZone without Destination")? {
                    "Battlefield" => Zone::Battlefield,
                    "Graveyard" => Zone::Graveyard,
                    "Hand" => Zone::Hand,
                    "Exile" => Zone::Exile,
                    "Library" => Zone::Library,
                    d => return Err(format!("ChangeZone Destination {d}")),
                };
                let hidden_origin = origin.has(Zone::Library) || origin.has(Zone::Hand);
                if let Some(se) = p.get("StaticEffect") {
                    self.static_effect_on_moved(f, se, &mut extra_after)?;
                }
                // `RememberLKI$ True`: the moved target is remembered as it last existed;
                // `Defined$ RememberedLKI` is that target (`defined`).
                if p.ack("RememberLKI", &["True"], "remember the moved target's last known information").is_some() && !p.has("ValidTgts") {
                    return Err("RememberLKI without targets".into());
                }
                // "Return it to the battlefield with a +1/+1 counter on it" (`WithCountersType$`):
                // it enters with the counters (CR 122.6, 614.1c), so enters-trigger conditions
                // ("another creature with power 2 or less enters") see them (603.6a, 603.10).
                // Applied as they enter, whatever form the move takes
                // (`resolve::apply_node`).
                if let Some(k) = p.get("WithCountersType") {
                    if dest != Zone::Battlefield {
                        return Err("ChangeZone WithCountersType off the battlefield".into());
                    }
                    let n = match p.get("WithCountersAmount") {
                        Some(v) => v.parse::<i32>().map_err(|_| format!("WithCountersAmount {v}"))?,
                        None => 1,
                    };
                    enter_counters = Some((self.counter_kind(k), n));
                }
                // `ThisDefinedAndTgts$ Self` ("Shuffle this creature and target creature into
                // their owners' libraries"): one instruction moving both, one event (CR 608.2c),
                // so leaves-the-battlefield abilities of either see the other leave (603.10a);
                // Forge `SpellAbilityEffect::getTargetCards` takes them together.
                let with_source = p.peek("ThisDefinedAndTgts") == Some("Self");
                let e = if let Some(e) = self.x9_change_zone(&p, f, origin, dest)? {
                    e
                } else if let Some(e) = self.x8_change_zone(&p, origin, dest)? {
                    e
                } else if let Some(e) = self.mr_change_zone(&p, f, origin, dest)? {
                    e
                } else if let Some(e) = self.lt_change_zone(&p, f, origin, dest)? {
                    e
                } else if let Some(e) = self.more_change_zone(&p, f, origin, dest, &mut extra_after)? {
                    e
                } else if let Some(e) = self.change_zone_ext(&p, f, origin, dest, hidden_origin, &mut extra_after)? {
                    e
                } else {
                let hidden = p.flag("Hidden") || (hidden_origin && !p.has("ValidTgts") && !p.has("Defined"));
                let change_type_raw = p.peek("ChangeType");
                let change_type = match p.get("ChangeType") {
                    // A hidden choice among exiled cards "exiled with this" kept as Forge
                    // card-level memory (`IsRemembered`, Endless Horizons).
                    Some(c) if hidden => Some(self.filter_f(f, &crate::compile_ext::card_memory_change_type(f, params, origin == ZoneSet::of(Zone::Exile), c))?),
                    Some(c) => Some(self.filter_f(f, c)?),
                    None => None,
                };
                // A choice from public zones only (graveyards, exile) with no `DefinedPlayer$`
                // looks at every player's cards there (Forge `ChangeZoneEffect`:
                // `game.getCardsIn(origin)`), the `ChangeType$` saying whose (Memory Theft's "a
                // card that has an Adventure that player owns from exile").
                all_owners = hidden && !hidden_origin && !p.has("DefinedPlayer");
                if hidden && change_type.is_none() {
                    return Err("ChangeZone hidden without ChangeType".into());
                }
                if all_owners && dest == Zone::Battlefield {
                    // Onto the battlefield from every player's public zones: CR 110.2a puts another
                    // player's card under the putting player's control "unless the effect states
                    // otherwise", and Forge's default is its owner's (scripts add `GainControl$`
                    // for "under your control"), so the script doesn't say which the Oracle means:
                    // unsupported (loud). Kept as before (the searcher's own cards, entering under
                    // their control) when every alternative ties the candidates to that player or to
                    // this object's own earlier actions: `YouOwn` / `YouCtrl`, cards it remembered
                    // (milled from its controller's library: Avatar Destiny, Bramble Familiar,
                    // Vastlands Scavenger) or exiled (`ExiledWithSource`: Purgatory).
                    use crate::filter::Prop;
                    let tied = change_type.as_ref().is_some_and(|f| {
                        !f.alts.is_empty()
                            && f.alts.iter().all(|a| a.props.iter().any(|pr| matches!(pr, Prop::YouOwn | Prop::YouCtrl | Prop::IsRemembered | Prop::Ext(crate::filter_ext::ExtProp::ExiledWithSource))))
                    });
                    if !tied {
                        return Err("hidden ChangeZone onto the battlefield from every player's public zones: whose control is not stated (CR 110.2a; GainControl$)".into());
                    }
                    all_owners = false;
                }
                // Whose zone is searched; that player also chooses (Forge's default Chooser).
                let searcher = self.defined(p.get("DefinedPlayer").unwrap_or("You"))?;
                if let Some(ch) = p.get("Chooser") {
                    if self.defined(ch)? != searcher {
                        return Err(format!("ChangeZone Chooser {ch}"));
                    }
                }
                // `Mandatory$ True`: the choice isn't "up to"; find as many as possible, except
                // that a search of a library for cards with a stated quality may still fail to
                // find (CR 701.23b). A search for "a card" must find one (701.23d).
                let quality_less = matches!(change_type_raw, Some("Card") | Some("Card.YouOwn"));
                must_find = hidden && p.flag("Mandatory") && (!origin.has(Zone::Library) || quality_less);
                let shuffle = p.flag("Shuffle") || p.flag("ShuffleNonMandatory");
                if let Some(t) = p.get("WithTotalCMC") {
                    // "up to two creature cards with total mana value 4 or less" (Lively Dirge).
                    if !hidden {
                        return Err("WithTotalCMC without a hidden choice".into());
                    }
                    total_mv = Some(t.parse::<i32>().map_err(|_| format!("WithTotalCMC {t}"))?);
                }
                if let Some(pos) = p.get("LibraryPosition") {
                    // "put it into its owner's library Nth from the top".
                    let n = pos.parse::<u8>().map_err(|_| format!("LibraryPosition {pos}"))?;
                    if dest != Zone::Library || hidden {
                        return Err("LibraryPosition form".into());
                    }
                    library_position = Some(n);
                }
                // "Search your library for a ... card and reveal it" (`Reveal$ True`, `ext9`): the
                // card found becomes known to both players (CR 701.20a).
                if p.peek("Reveal").is_some() {
                    if !hidden || !p.flag("Reveal") {
                        return Err("ChangeZone Reveal form".into());
                    }
                    reveal_found = true;
                }
                // `ForgetChanged$`: the moved objects are no longer remembered (`ext9`).
                if p.flag("ForgetChanged") {
                    if hidden {
                        return Err("ChangeZone hidden ForgetChanged".into());
                    }
                    forget_changed = true;
                }
                Effect::ChangeZone { origin, dest, hidden, change_type, num: num(self, "ChangeNum", 1)?, tapped: p.flag("Tapped"), optional: false, shuffle, searcher }
                };
                if with_source {
                    if !matches!(e, Effect::ChangeZone { hidden: false, .. }) || !p.has("ValidTgts") {
                        return Err("ThisDefinedAndTgts form".into());
                    }
                    this_and_targets = true;
                }
                e
            }
            "Token" if p.peek("AttachedTo").is_some() => self.lt_token_attached(&p)?,
            "Token" if p.peek("TokenOwner") == Some("Player") => self.lt_token_each_player(&p, f)?,
            "Token" => {
                let script = p.get("TokenScript").ok_or("Token without TokenScript")?;
                if script.contains(',') {
                    return Err("Token: multiple scripts".into());
                }
                let token = self.token(script)?;
                if !self.get(token).playable() {
                    return Err(format!("token {script}: {:?}", self.get(token).unsupported));
                }
                self.token_ext(&p, &mut extra_after)?;
                // Forge: replacement effects that change which token is created don't apply to
                // this one (Into the Flood Maw's gift Fish). No compiled replacement changes a
                // token's script, so there is nothing to lock.
                p.ack("LockTokenScript", &["True"], "no compiled replacement changes a token's script");
                match p.get("TokenOwner") {
                    None | Some("You") => Effect::Token { token, n: num(self, "TokenAmount", 1)?, tapped: p.flag("TokenTapped") },
                    Some(o) => Effect::Ext(crate::effect_ext::ExtEffect::TokenFor { token, n: num(self, "TokenAmount", 1)?, tapped: p.flag("TokenTapped"), owner: self.defined(o)? }),
                }
            }
            "Charm" => {
                let choices = p.get("Choices").ok_or("Charm without Choices")?;
                let mut modes = vec![];
                for c in choices.split(',') {
                    let mut sv = f.svar(c.trim()).and_then(|s| s.as_ability()).ok_or_else(|| format!("Charm choice {c} missing"))?;
                    // Spree/tiered mode costs (CR 700.2h) are recorded separately.
                    let mode_cost = sv.pairs.iter().position(|(k, _)| k == "ModeCost").map(|i| sv.pairs.remove(i).1);
                    let id = self.ability_inner(&sv, f, uns, depth + 1, false)?;
                    if let Some(mc) = mode_cost {
                        let cost = ManaCost::parse_forge(&mc).map_err(|e| format!("ModeCost {mc}: {e}"))?;
                        if !f.keywords.iter().any(|k| k == "Spree" || k == "Tiered") {
                            return Err("ModeCost without spree/tiered".into());
                        }
                        self.mode_costs.push((id, cost));
                    }
                    modes.push(id);
                }
                // "Choose one. If this spell's additional cost was paid, choose both instead."
                // (`CharmNum$ X`, X = `Count$OptionalGenericCostPaid.A.B`, CR 601.2b).
                if let Some(v) = p.peek("CharmNum").filter(|v| v.parse::<u8>().is_err()) {
                    match self.value(f, v)? {
                        Expr::Alt(crate::altcast::AltExpr::CastFlag(crate::altcast::flag::OPTCOST, a, b)) if a > b && b >= 1 && a <= modes.len() as i32 && p.peek("MinCharmNum").is_none() => {
                            p.ack("CharmNum", &[v], "matched above");
                            charm_optcost = Some((a as u8, b as u8));
                        }
                        // "Choose one. If you descended this turn, you may choose both instead."
                        // (`MinCharmNum$ 1 | CharmNum$ Count$Compare Y GE1.2.1`; `ext9`): the
                        // maximum is evaluated as the modes are chosen (CR 601.2b).
                        e if v.starts_with("Count$Compare ") && p.peek("MinCharmNum").is_some() && modes.len() <= 3 => {
                            p.ack("CharmNum", &[v], "matched above");
                            charm_max_expr = Some(e);
                        }
                        _ => return Err(format!("CharmNum {v}")),
                    }
                }
                charm_this_turn = match p.get("ChoiceRestriction") {
                    None => false,
                    // Per source object: abilities only (a spell is a new object each cast).
                    Some("ThisTurn") if kind != AbilityKind::Spell => true,
                    Some(r) => return Err(format!("Charm ChoiceRestriction {r}")),
                };
                let (max, min) = match charm_optcost {
                    Some((a, b)) => (a, b),
                    None if charm_max_expr.is_some() => {
                        let min = p.get("MinCharmNum").map(|v| v.parse::<u8>().map_err(|_| format!("MinCharmNum {v}"))).transpose()?.unwrap_or(1);
                        (modes.len() as u8, min)
                    }
                    None => {
                        let max = p.get("CharmNum").map(|v| v.parse::<u8>().map_err(|_| format!("CharmNum {v}"))).transpose()?.unwrap_or(1);
                        (max, p.get("MinCharmNum").map(|v| v.parse::<u8>().map_err(|_| format!("MinCharmNum {v}"))).transpose()?.unwrap_or(max))
                    }
                };
                Effect::Charm { modes, min, max }
            }
            "Dig" if p.peek("ExileFaceDown").is_some() && p.peek("WithMayLook").is_some() => self.mr_dig_face_down_look(&p, f)?,
            "Dig" if p.peek("ExileFaceDown").is_some() => self.lt_dig_face_down(&p, f)?,
            "Dig" => {
                let zone = |v: Option<&str>, d: Zone| -> Res<Zone> {
                    Ok(match v {
                        None => d,
                        Some("Hand") => Zone::Hand,
                        Some("Graveyard") => Zone::Graveyard,
                        Some("Exile") => Zone::Exile,
                        Some("Battlefield") => Zone::Battlefield,
                        Some("Library") => Zone::Library,
                        Some(z) => return Err(format!("Dig zone {z}")),
                    })
                };
                let dest = zone(p.get("DestinationZone"), Zone::Hand)?;
                let rest = zone(p.get("DestinationZone2"), Zone::Library)?;
                // `LibraryPosition2$ 0`: the rest stay on top (`ext3::dig_rest_on_top`).
                let rest_top = rest == Zone::Library && p.peek("LibraryPosition2") == Some("0");
                if p.has("LibraryPosition") || p.get("LibraryPosition2").is_some_and(|v| v != "-1" && !rest_top) {
                    return Err("Dig LibraryPosition".into());
                }
                dig_rest_top = rest_top;
                // `Imprint$`: the cards taken are imprinted for later nodes (Chandra's "cast that card").
                dig_imprint = p.flag("Imprint");
                let valid = match p.get("ChangeValid") {
                    Some(v) => Some(self.filter_f(f, v)?),
                    None => None,
                };
                let up_to = p.flag("Optional");
                // Hidden information follows the Oracle wording: "reveal" vs "look at".
                p.ack("NoReveal", &["True"], "Dig reveal follows the Oracle wording");
                p.ack("Reveal", &["True"], "Dig reveal follows the Oracle wording");
                // The chosen card is shown to the controller anyway (they look at the cards).
                p.ack("ForceRevealToController", &["True"], "the controller looks at the cards anyway");
                // Without a description (trigger bodies), the face's Oracle text decides.
                let reveal = match params.get("SpellDescription") {
                    Some(d) => d.to_ascii_lowercase().contains("reveal"),
                    None => f.oracle.as_deref().is_some_and(|o| o.to_ascii_lowercase().contains("you may reveal")),
                };
                // `ChangeNum$ All`: every looked-at card (that matches) goes to `dest`.
                // (Forge's DigEffect reads no `AnyNumber$`; `ChangeNum$ Any` is unsupported.)
                let change = if p.peek("ChangeNum") == Some("All") {
                    p.ack("ChangeNum", &["All"], "every looked-at card");
                    Expr::Num(crate::num::ALL)
                } else {
                    num(self, "ChangeNum", 1)?
                };
                Effect::Dig { n: num(self, "DigNum", 1)?, change, dest, rest, valid, up_to, reveal, rest_random: p.flag("RestRandomOrder") }
            }
            "ChangeZoneAll" => {
                let origin = ZoneSet::parse(p.get("Origin").ok_or("ChangeZoneAll without Origin")?)?;
                let dest = match p.get("Destination").ok_or("ChangeZoneAll without Destination")? {
                    "Graveyard" => Zone::Graveyard,
                    "Hand" => Zone::Hand,
                    "Exile" => Zone::Exile,
                    "Library" => Zone::Library,
                    "Battlefield" => Zone::Battlefield,
                    d => return Err(format!("ChangeZoneAll Destination {d}")),
                };
                // Card-level memory of exiled cards (`compile_ext::card_memory_change_type`).
                let ct = crate::compile_ext::card_memory_change_type(f, params, origin == ZoneSet::of(Zone::Exile), p.get("ChangeType").unwrap_or("Card"));
                let mut change_type = self.filter_f(f, &ct)?;
                if let Some(dp) = p.get("DefinedPlayer") {
                    // Only that player's zones: their battlefield (control), other zones (ownership).
                    let prop = match dp {
                        "Targeted" | "TargetedPlayer" => crate::filter_ext::ExtProp::InZoneOfTargetedPlayer,
                        "You" => crate::filter_ext::ExtProp::InZoneOfYou,
                        d => return Err(format!("ChangeZoneAll DefinedPlayer {d}")),
                    };
                    for a in &mut change_type.alts {
                        a.props.push(crate::filter::Prop::Ext(prop.clone()));
                    }
                } else if let Some(vt) = p.peek("ValidTgts") {
                    // "Exile all attacking creatures target player controls" (Settle the Wreckage),
                    // "exile target player's graveyard": only the target players' zones (CR 115.1,
                    // 608.2b). Forge `ChangeZoneAllEffect`: a targeted ChangeZoneAll
                    // takes the cards of `getTargetPlayers` only.
                    if !vt.split(',').all(|t| matches!(t.split('.').next(), Some("Player" | "Opponent"))) {
                        return Err(format!("ChangeZoneAll targeting {vt}"));
                    }
                    for a in &mut change_type.alts {
                        a.props.push(crate::filter::Prop::Ext(crate::filter_ext::ExtProp::InZoneOfTargetedPlayer));
                    }
                }
                if p.has("Duration") {
                    self.duration_link(&p, origin, &mut extra_after)?;
                }
                if let Some(se) = p.get("StaticEffect") {
                    self.static_effect_on_moved(f, se, &mut extra_after)?;
                }
                if p.peek("GainControl").is_some() || p.peek("WithCountersType").is_some() {
                    // "Put ... onto the battlefield under your control with a [kind] counter".
                    if dest != Zone::Battlefield {
                        return Err("ChangeZoneAll GainControl/WithCounters off the battlefield".into());
                    }
                    let counters = match p.get("WithCountersType") {
                        Some(k) => Some((self.counter_kind(k), p.get("WithCountersAmount").map(|n| n.parse::<i32>().map_err(|_| "WithCountersAmount")).transpose()?.unwrap_or(1))),
                        None => None,
                    };
                    Effect::Ext(crate::effect_ext::ExtEffect::PutOntoBattlefieldAll { origin, change_type, yours: p.flag("GainControl"), counters })
                } else {
                    Effect::ChangeZoneAll { origin, dest, change_type, shuffle: p.flag("Shuffle") }
                }
            }
            "DamageAll" => Effect::DamageAll {
                amount: num(self, "NumDmg", 0)?,
                cards: p.get("ValidCards").map(|v| self.filter_f(f, v)).transpose()?.map(|c| targeted_players_only(&p, api, c)).transpose()?,
                players: p.get("ValidPlayers").map(|v| PlayerSel::parse(Some(v))).transpose()?,
            },
            "PutCounterAll" => {
                let ct = p.get("CounterType").ok_or("PutCounterAll without CounterType")?;
                let kind = self.counter_kind(ct);
                // "each creature target player controls".
                let valid = targeted_players_only(&p, api, self.filter_f(f, p.get("ValidCards").ok_or("PutCounterAll without ValidCards")?)?)?;
                Effect::PutCounterAll { valid, kind, n: num(self, "CounterNum", 1)? }
            }
            "Explore" => Effect::Explore,
            "Effect" if crate::ext4::is_must_attack_effect(&p, f) => self.mr_must_attack(&p)?,
            "Effect" => {
                let mut statics = vec![];
                if let Some(names) = p.get("StaticAbilities") {
                    for name in names.split(',') {
                        let sv = f.svar(name.trim()).ok_or_else(|| format!("Effect static {name} missing"))?;
                        let mut sp = forge_script::Params::parse(&sv.value);
                        // An effect's statics function from the command zone (CR 114/611).
                        sp.pairs.retain(|(k, v)| !(k == "EffectZone" && v == "Command"));
                        let sd = self.compile_static(&sp, f)?;
                        // ("You gain hexproof" from an effect is `ext7`'s Pump form, not a static.)
                        if self.take_player_hexproof(&sd).is_some() {
                            return Err(format!("Effect static {}: player hexproof from an effect object", name.trim()));
                        }
                        if !crate::static_ext::applies_from_effect_object(&sd.kind) {
                            return Err(format!("Effect static {}: not applied from an effect object", name.trim()));
                        }
                        // `altcast::cant_cast` matches an effect's can't-cast filter without the
                        // effect's remembered objects (it shares the permanents' path).
                        if matches!(sd.kind, StaticKind::Alt(crate::altcast::AltStatic::CantBeCast { .. })) && crate::compile_audit::static_reads_memory(&sd)? {
                            return Err(format!("Effect static {}: can't-cast filter reads IsRemembered", name.trim()));
                        }
                        statics.push(self.statics_pool.len() as u32);
                        self.statics_pool.push(sd);
                    }
                }
                if p.has("ReplacementEffects") {
                    return Err("Effect with triggers/replacements".into());
                }
                let effect_triggers = p.get("Triggers");
                // Display name/art of the effect object or emblem (CR 114).
                p.display("Name");
                p.display("Image");
                let remember = p.get("RememberObjects").map(|d| self.defined(d)).transpose()?;
                let duration = match p.get("Duration") {
                    None => EffDuration::EndOfTurn,
                    Some("Permanent") => EffDuration::Permanent,
                    Some("UntilYourNextTurn") => EffDuration::UntilYourNextTurn,
                    Some("UntilTheEndOfYourNextTurn") => EffDuration::UntilTheEndOfYourNextTurn,
                    Some("UntilYourNextEndStep") => EffDuration::UntilYourNextEndStep,
                    Some("AsLongAsInPlay") => EffDuration::AsLongAsInPlay,
                    Some("UntilTargetedUntaps") => EffDuration::UntilUntaps,
                    Some(d) => return Err(format!("Effect Duration {d}")),
                };
                let zs = |k: &str| -> Res<ZoneSet> { p.get(k).map(ZoneSet::parse).transpose().map(|z| z.unwrap_or_default()) };
                if p.get("EffectOwner").is_some_and(|o| o != "You") {
                    return Err("Effect owner".into());
                }
                if let Some(names) = effect_triggers {
                    self.effect_triggers(names, f, uns, duration, &mut extra_after)?;
                }
                Effect::CreateEffect { statics, remember, duration, forget_on_moved: zs("ForgetOnMoved")?, exile_on_moved: zs("ExileOnMoved")? }
            }
            "Animate" if p.peek("staticAbilities").is_some() && p.peek("Defined").is_some_and(|d| d.starts_with("ValidGraveyard ")) => self.graveyard_may_cast(&p, f)?,
            "Animate" if p.peek("RemoveAllAbilities").is_some() && p.peek("Name").is_some() => self.animate_become(&p, f)?,
            "Animate" if crate::ext6::is_grant_animate(&p) => self.x_grant_animate(&p, f, uns)?,
            "Animate" => {
                if p.flag("AddAllCreatureTypes") {
                    // "... and all creature types" (Soulstone Sanctuary): layer 4, CR 205.3m.
                    let duration = match p.peek("Duration") {
                        None => Duration::EndOfTurn,
                        Some("Permanent") => Duration::Permanent,
                        Some(d) => return Err(format!("Duration {d}")),
                    };
                    let defined = p.peek("Defined").map(|d| self.defined(d)).transpose()?;
                    extra_after.push(crate::compile_ext::node(Effect::Ext(crate::effect_ext::ExtEffect::AllCreatureTypes { duration }), defined));
                }
                let mut types = TypeSet::default();
                let mut subtypes = vec![];
                if let Some(t) = p.get("Types") {
                    for w in t.split(',') {
                        let w = w.trim();
                        match CardType::parse(w) {
                            Some(ct) => types = types.with(ct),
                            None => subtypes.push(self.subtype(w).ok_or_else(|| format!("Animate type {w}"))?),
                        }
                    }
                }
                let colors = match p.get("Colors") {
                    Some(c) => {
                        let mut cs = ColorSet::default();
                        for w in c.split(',') {
                            cs = cs.with(Color::from_word(w.trim()).ok_or_else(|| format!("Animate color {w}"))?);
                        }
                        Some(cs)
                    }
                    None => None,
                };
                let mut abilities = vec![];
                if let Some(a) = p.get("Abilities") {
                    for name in a.split(',') {
                        let sv = f.svar(name.trim()).and_then(|s| s.as_ability()).ok_or_else(|| format!("Animate ability {name} missing"))?;
                        abilities.push(self.ability_inner(&sv, f, uns, depth + 1, false)?);
                    }
                }
                if let Some(t) = p.get("Triggers") {
                    // Gained triggered abilities (layer 6): granted to what this node animated.
                    self.animate_triggers(t, f, uns, p.peek("Duration"), &mut extra_after)?;
                }
                // "Return it to your hand at the beginning of the next end step" (`ext9`).
                if let Some(w) = p.get("AtEOT") {
                    let d = p.peek("Defined").map(|d| self.defined(d)).transpose()?;
                    let n = self.x9_at_eot(w, d)?;
                    extra_after.push(n);
                }
                Effect::Animate {
                    power: p.get("Power").map(|v| self.value(f, v)).transpose()?,
                    toughness: p.get("Toughness").map(|v| self.value(f, v)).transpose()?,
                    types,
                    subtypes,
                    remove_card_types: p.flag("RemoveCardTypes"),
                    remove_creature_types: p.flag("RemoveCreatureTypes"),
                    colors,
                    overwrite_colors: p.flag("OverwriteColors"),
                    kws: kw_list(p.get("Keywords"))?,
                    abilities,
                    duration: duration(p.get("Duration"))?,
                }
            }
            "Clone" => {
                let choices = self.filter_f(f, p.get("Choices").ok_or("Clone without Choices")?)?;
                let zone = match p.get("ChoiceZone") {
                    None | Some("Battlefield") => Zone::Battlefield,
                    Some("Graveyard") => Zone::Graveyard,
                    Some(z) => return Err(format!("Clone ChoiceZone {z}")),
                };
                let (mut add_types, mut add_subtypes) = (TypeSet::default(), vec![]);
                if let Some(t) = p.get("AddTypes") {
                    for w in t.split('&') {
                        match CardType::parse(w.trim()) {
                            Some(ct) => add_types = add_types.with(ct),
                            None => add_subtypes.push(self.subtype(w.trim()).ok_or_else(|| format!("Clone AddTypes {w}"))?),
                        }
                    }
                }
                let kws = kw_list(p.get("AddKeywords").map(|k| k.replace(" & ", "&")).as_deref())?;
                let set_pt = match (p.get("SetPower"), p.get("SetToughness")) {
                    (None, None) => None,
                    (Some(a), Some(b)) => Some((a.parse().map_err(|_| "Clone SetPower")?, b.parse().map_err(|_| "Clone SetToughness")?)),
                    _ => return Err("Clone with only one of SetPower/SetToughness".into()),
                };
                let keep_name = match p.get("NewName") {
                    None => false,
                    Some(n) if n == f.name => true,
                    Some(n) => return Err(format!("Clone NewName {n}")),
                };
                Effect::Clone { choices, zone, optional: false, add_types, add_subtypes, kws, set_pt, keep_name, remember_origin: p.flag("RememberCloneOrigin") }
            }
            "CopyPermanent" => {
                // "Sacrifice it at the beginning of the next end step" (`AtEOT$`).
                self.token_ext(&p, &mut extra_after)?;
                Effect::CopyPermanent
            }
            // "Attach any number of target Equipment you control to target creature you control"
            // (Thorin): the previous node's targets are attached to this node's target.
            "Attach" if p.peek("Object") == Some("ParentTarget") && p.peek("Defined").is_none() && p.peek("ValidTgts").is_some() => {
                p.ack("Object", &["ParentTarget"], "the previous node's targets are attached");
                Effect::Ext(crate::effect_ext::ExtEffect::AttachParentTargets)
            }
            // Attach the source to a defined object ("you may attach this Equipment to it", Doc Ock's
            // Tentacles): `Defined$` is read below.
            "Attach" if p.peek("Choices").is_none() && !p.has("Object") && (p.peek("Defined").is_none() || (p.peek("ValidTgts").is_none() && p.peek("Defined") != Some("Self"))) => Effect::Attach,
            "AlterAttribute" => match (p.get("Attributes"), p.get("Activate")) {
                (Some("Prepared"), None | Some("True") | Some("true")) => Effect::SetPrepared(true),
                (Some("Prepared"), Some("False") | Some("false")) => Effect::SetPrepared(false),
                // Cases (CR 719.2): "solve" this Case.
                (Some("Solved"), None) => Effect::Ext(crate::effect_ext::ExtEffect::SetSolved),
                // CR 701.64a: "Harness [this permanent]".
                (Some("Harnessed"), None) => Effect::Ext(crate::effect_ext::ExtEffect::Harness),
                (a, _) => return Err(format!("AlterAttribute {a:?}")),
            },
            // Effects of `ext9`.
            "StoreSVar" => self.x9_store_svar(&p, f)?,
            "Play" if p.peek("ValidZone") == Some("Graveyard") && p.peek("Defined") == Some("Remembered") => self.x9_play(&p)?.ok_or("Play from graveyard form")?,
            // CR 701.24a: "Shuffle your library".
            "Shuffle" if p.peek("Defined").is_none_or(|d| d == "You") && p.peek("ValidTgts").is_none() => Effect::Ext(crate::effect_ext::ExtEffect::X9(crate::ext9::X9Effect::Shuffle)),
            // "Copy that spell. You may choose new targets for the copy." (Leyline of Resonance).
            "CopySpellAbility" if p.peek("Defined") == Some("TriggeredSpellAbility") && p.peek("MayChooseTarget").is_some() && p.peek("Amount").is_none() && self.le.trig_mode == "SpellCast" => {
                if !p.flag("MayChooseTarget") || p.get("Controller").is_some_and(|c| c != "You") {
                    return Err("CopySpellAbility TriggeredSpellAbility form".into());
                }
                Effect::Ext(crate::effect_ext::ExtEffect::X9(crate::ext9::X9Effect::CopyTriggeredSpellNewTargets))
            }
            "Airbend" => self.x9_airbend(&p, f)?,
            "ImmediateTrigger" if f.svars.iter().any(|s| s.value.trim() == "TriggerRemembered$GreatestCardManaCost") => self.x9_reflexive_max_mv(&p, f, uns, depth)?,
            other => match match match self.x8_api(other, &p, f, uns, depth)?.map_or_else(|| self.x7_api(other, &p, f, uns, depth), |e| Ok(Some(e)))? {
                Some(e) => Some(e),
                None => self.x_api(other, &p, f, uns, depth)?,
            }
            .map_or_else(|| self.le_api(other, &p, f, uns, depth), |e| Ok(Some(e)))?
            {
                Some(e) => Some(e),
                None => self.mr_api(other, &p, f)?,
            } {
                Some(e) => e,
                None => match self.lt_api(other, &p, f)? {
                    Some(e) => e,
                    None => match self.ext_api(other, &p, f, uns, depth, &mut extra_after)? {
                        Some(e) => e,
                        None => return Err(format!("API {other}")),
                    },
                },
            },
        };
        // "Except it's legendary" (`le_extras` checks the form): applied as the token is created.
        let copy_legendary = api == "CopyPermanent" && p.peek("AddTypes") == Some("Legendary");
        self.le_extras(api, &p, f, &mut extra_after)?;
        let target = match p.get("ValidTgts") {
            Some(v) => {
                // An ability's filter is about its source; emblems have none here.
                let v = if p.peek("TargetType") == Some("Activated,Triggered") || crate::ext8::stack_sel(p.peek("TargetType")).is_some() { v.trim_end_matches(",Emblem") } else { v };
                // "Other than that creature" (the trigger's object, `ext9`).
                let v_own;
                // "... from a single graveyard" (`TargetsWithSameController$`, `ext9`).
                if p.flag("TargetsWithSameController") {
                    same_owner_targets = true;
                }
                let v = if !v.contains(',') && v.ends_with(".AttachedTo ParentTarget") {
                    // "Target Equipment attached to that creature" (the previous part's target,
                    // `ext9`): checked as targets are chosen and as it resolves.
                    attached_to_parent = true;
                    v_own = v.trim_end_matches(".AttachedTo ParentTarget").to_string();
                    v_own.as_str()
                } else if !v.contains(',') && v.contains("+NotDefinedTriggeredTarget") {
                    // Only "becomes the target" triggers record that creature (`TrigCtx::target`).
                    if self.le.trig_mode != "BecomesTarget" {
                        return Err("NotDefinedTriggeredTarget outside a BecomesTarget trigger".into());
                    }
                    exclude_triggered = true;
                    v_own = v.replace("+NotDefinedTriggeredTarget", "");
                    v_own.as_str()
                } else {
                    v
                };
                let filter = self.filter_f(f, v)?;
                // "Exactly X targets" with X by the gift's promise (`ext8`).
                let by_promise = crate::ext8::target_count_by_promise(self, f, &p)?;
                self.x8.any_by_promise |= by_promise.is_some();
                let min = if by_promise.is_some() { 0 } else { p.get("TargetMin").map(|x| x.parse::<u8>()).transpose().map_err(|_| "variable target count".to_string())?.unwrap_or(1) };
                let mut max_expr = None;
                let max = match p.get("TargetMax") {
                    _ if by_promise.is_some() => 1,
                    // "For each opponent, up to one target ... that player controls": in a
                    // two-player game that is at most one target (`TargetsForEachPlayer`).
                    Some(m) if f.svar(m).is_some_and(|sv| sv.value.trim() == "PlayerCountOpponents$Amount") && p.flag("TargetsForEachPlayer") => 1,
                    Some(m) if m.parse::<u8>().is_err() && f.svar(m).is_some() => match self.value(f, m)? {
                        // A constant (`PlayerCountPlayers$Amount`: two players).
                        Expr::Num(n) => n.clamp(0, 255) as u8,
                        // "Up to X targets": evaluated when targets are chosen (CR 601.2c).
                        e => {
                            max_expr = Some(e);
                            u8::MAX
                        }
                    },
                    m => m.map(|x| x.parse::<u8>()).transpose().map_err(|_| "variable target count".to_string())?.unwrap_or(1),
                };
                let unique = p.flag("TargetUnique");
                let kind = match (p.get("TargetType"), p.get("TgtZone")) {
                    (Some("Spell"), _) => TargetKind::Spell,
                    (Some("Activated,Triggered"), None) => TargetKind::Ability,
                    (t, None) if crate::ext8::stack_sel(t).is_some() => TargetKind::Stack(crate::ext8::stack_sel(t).unwrap()),
                    (_, Some("Graveyard")) => TargetKind::Graveyard,
                    // Forge (AbilityFactory.adjustChangeZoneTarget): a ChangeZone card target is in
                    // its Origin zone unless TgtZone says otherwise (player targets name whose zone).
                    (None, None) if api == "ChangeZone" && !filter.matches_players() && p.get("Origin") == Some("Graveyard") => TargetKind::Graveyard,
                    (None, None) if api == "ChangeZone" && !filter.matches_players() && !matches!(p.get("Origin"), Some("Battlefield") | None) => return Err(format!("ChangeZone target in {:?}", p.get("Origin"))),
                    (None, None) | (None, Some("Battlefield")) => TargetKind::Normal,
                    (None, Some("Stack,Battlefield")) | (None, Some("Battlefield,Stack")) => TargetKind::SpellOrPermanent,
                    // "target spell" (`TgtZone$ Stack`, Bilbo's Gambit; `ext8`).
                    (None, Some("Stack")) => TargetKind::Spell,
                    (t, z) => return Err(format!("target type {t:?} zone {z:?}")),
                };
                let max = by_promise.map_or(max, |(a, b)| a.max(b));
                Some(TargetSpec { filter, kind, min, max, unique, max_expr, by_promise })
            }
            None => self.pending_target.take(),
        };
        let mut defined_filter_cond = None;
        let defined = match p.get("Defined") {
            // Consumed by the effect itself (cards in a zone, not a Defined$ object).
            Some(d) if d.starts_with("ValidGraveyard ") && matches!(effect, Effect::Ext(crate::effect_ext::ExtEffect::GraveyardMayCast { .. })) => None,
            Some("TriggeredSpellAbility") if matches!(effect, Effect::Ext(crate::effect_ext::ExtEffect::CopyTriggeredSpell) | Effect::Ext(crate::effect_ext::ExtEffect::X(crate::ext6::XEffect::CopyActivated)) | Effect::Ext(crate::effect_ext::ExtEffect::X9(crate::ext9::X9Effect::CopyTriggeredSpellNewTargets))) => None,
            // `Defined$ Valid <filter>` consumed by an `ext5` effect acting on every match.
            Some(d) if d.starts_with("Valid ") && crate::ext5::consumes_defined_valid(&effect) => None,
            // Consumed by `ext9` effects ("each player sacrifices", airbend all other creatures).
            Some("Player") if matches!(effect, Effect::Ext(crate::effect_ext::ExtEffect::X9(crate::ext9::X9Effect::SacrificeEach { .. }))) => None,
            Some(d) if d.starts_with("Valid ") && matches!(effect, Effect::Ext(crate::effect_ext::ExtEffect::X9(crate::ext9::X9Effect::Airbend { .. }))) => None,
            // Consumed by `ext7::x7_pump_you_and`.
            Some(d) if d.starts_with("You & Valid ") && matches!(effect, Effect::Ext(crate::effect_ext::ExtEffect::X7(crate::ext7::X7Effect::HexproofYouAnd { .. }))) => None,
            Some(d) if self.defined(d).is_err() && d.contains('.') => {
                // `TriggeredCardLKICopy.Spider`: the defined object, only if it matches.
                let (base, filt) = d.split_once('.').unwrap();
                let bd = self.defined(base)?;
                defined_filter_cond = Some(Condition::Defined(bd, self.filter_f(f, &format!("Card.{filt}"))?, crate::filter::CmpOp::GE, 1));
                Some(bd)
            }
            Some(d) => Some(self.defined(d)?),
            None => None,
        };
        // The "all" effects act on every match, not on `who` (`resolve`): a `Defined$` there would
        // be silently ignored (Forge: the defined players' cards, `getTargetPlayers`).
        if defined.is_some() && matches!(effect, Effect::PumpAll { .. } | Effect::DestroyAll { .. } | Effect::DamageAll { .. } | Effect::ChangeZoneAll { .. } | Effect::PutCounterAll { .. } | Effect::Ext(crate::effect_ext::ExtEffect::LoseAllAbilitiesAll { .. })) {
            return Err(format!("{api} with Defined$"));
        }
        // Token/ChangeZone `Defined` in replacement/trigger bodies already read above.
        let activation_limit = match p.get("ActivationLimit") {
            Some(v) if kind == AbilityKind::Activated => Some(v.parse::<u8>().map_err(|_| format!("ActivationLimit {v}"))?),
            Some(v) => return Err(format!("ActivationLimit {v} on a non-activated ability")),
            None => None,
        };
        // Abilities that function from the graveyard (sorted out by `compile_face`).
        match p.get("ActivationZone") {
            None => {}
            Some("Graveyard") | Some("Hand") if kind == AbilityKind::Activated && depth == 0 => {}
            Some(z) => return Err(format!("ActivationZone {z}")),
        }
        // CR 702.177a: "Exhaust — [Cost]: [Effect]" = "Activate only once."
        // CR 702.193a: power-up = "activate only once" (like exhaust) + a cost reduction by the
        // permanent's mana cost if it entered this turn (`ext3::power_up_reduce`).
        let power_up = p.flag("PowerUp");
        let exhaust = p.flag("Exhaust") || power_up;
        if exhaust && kind != AbilityKind::Activated {
            return Err("Exhaust on a non-activated ability".into());
        }
        let mut activate_if = self.conditions(&p, f, "")?;
        if let Some(c) = self.activation_ext(&p)? {
            activate_if.push(c);
        }
        let mut conditions = self.conditions(&p, f, "Condition")?;
        conditions.extend(defined_filter_cond);
        // (`peek`: `Duration$` is read by the effects that implement it, `duration_link`.)
        if p.peek("Duration") == Some("UntilHostLeavesPlay") {
            // CR 610.3a/b: if the host already left the battlefield (after the ability was put on
            // the stack / triggered), the object doesn't move at all. Moving it and returning it
            // at once made a zone change (new object, new ETB triggers): Solitary Cell vs
            // Pinnacle Starcage looped forever inside one step.
            conditions.push(Condition::Defined(Defined::SelfObj, self.filter_f(f, "Card.inZoneBattlefield")?, crate::filter::CmpOp::GE, 1));
        }
        // In a trigger body, `AB$ ... | Cost$ Behold<1/T>` is "you may behold a T. If you do, ...".
        let mut optional_behold = None;
        let mut spell_life_x = false;
        let mut cost = match p.get("Cost") {
            Some(c) if depth > 0 && kind == AbilityKind::Activated && c.starts_with("Behold<1/") && c.ends_with('>') && !c.contains(' ') => {
                optional_behold = Some(UnlessCost::Switched(Box::new(UnlessCost::Behold(self.filter_f(f, &c[9..c.len() - 1])?))));
                None
            }
            // A spell's `Cost$` repeats its mana cost; "As an additional cost to cast this spell,
            // pay X life" (`PayLife<X>`) is announced with X (`ext2::spell_life_x`).
            Some(c) if kind == AbilityKind::Spell => {
                let mut life_x = false;
                let rest: Vec<&str> = c.split_whitespace().filter(|t| if *t == "PayLife<X>" { life_x = true; false } else { true }).collect();
                let pc = self.parse_cost(f, &rest.join(" "))?;
                if let Some((1, flt)) = pc.sac.clone().filter(|_| pc == Cost { mana: pc.mana.clone(), sac: pc.sac.clone(), ..Default::default() }) {
                    // "As an additional cost to cast this spell, sacrifice a ..." (CR 601.2b):
                    // the face's `AltDef::extra_cost` (taken over in `compile_face`).
                    self.lt.pending_spell_cost = Some(crate::altcast::AddCost::Sac(1, flt));
                } else if let Some((n, flt)) = pc.discard.clone().filter(|_| pc == Cost { mana: pc.mana.clone(), discard: pc.discard.clone(), ..Default::default() }) {
                    // "As an additional cost to cast this spell, discard N cards" (CR 601.2b).
                    self.lt.pending_spell_cost = Some(crate::altcast::AddCost::Discard(n, flt));
                } else if pc.pay_life > 0 && pc == (Cost { mana: pc.mana.clone(), pay_life: pc.pay_life, ..Default::default() }) {
                    // "As an additional cost to cast this spell, pay N life" (CR 601.2b, 119.4).
                    self.lt.pending_spell_cost = Some(crate::altcast::AddCost::Life(pc.pay_life));
                } else if pc != (Cost { mana: pc.mana.clone(), ..Default::default() }) {
                    return Err(format!("spell Cost$ with non-mana parts: {c}"));
                }
                spell_life_x = life_x;
                // The mana cost is the face's; keeping it here would make it a "you may pay"
                // resolution cost (`resolve::apply_node`).
                None
            }
            Some(c) => Some(self.parse_cost(f, c)?),
            None => None,
        };
        let kind = if optional_behold.is_some() { AbilityKind::Sub } else { kind };
        if kind == AbilityKind::Activated && cost.is_none() {
            return Err("activated ability without Cost".into());
        }
        if optional_behold.is_some() && p.has("UnlessCost") {
            return Err("behold cost with UnlessCost".into());
        }
        let mut resolution_cost = None;
        let unless = match p.get("UnlessCost") {
            // Paid by the effect itself ("you may discard two cards. When you do", `ext9`).
            Some(_) if matches!(effect, Effect::Ext(crate::effect_ext::ExtEffect::X9(crate::ext9::X9Effect::DiscardReflexiveMaxMv { .. }))) => None,
            // "You may [cost]. If you do, [effect]": an optional cost paid on resolution.
            Some(c) if p.flag("UnlessSwitched") && p.get("UnlessPayer") == Some("You") && !c.starts_with("Behold<") => {
                resolution_cost = Some(self.parse_cost(f, c)?);
                None
            }
            Some(c) => {
                let mut cost = if let Some(n) = c.strip_prefix("PayLife<").and_then(|r| r.strip_suffix('>')) {
                    UnlessCost::Life(n.parse().map_err(|_| format!("UnlessCost {c}"))?)
                } else if let Some(t) = c.strip_prefix("Behold<1/").and_then(|r| r.strip_suffix('>')) {
                    UnlessCost::Behold(self.filter_f(f, t.split('/').next().unwrap_or(t))?)
                } else if c.split_whitespace().all(|t| t.len() <= 3 && !t.contains('<') && t != "X") {
                    UnlessCost::Mana(ManaCost::parse_forge(c)?)
                } else if f.svar(c).is_some() {
                    // `UnlessCost$ X` with X an SVar: that much generic mana.
                    UnlessCost::Generic(self.value(f, c)?)
                } else {
                    return Err(format!("UnlessCost {c}"));
                };
                if p.flag("UnlessSwitched") {
                    cost = UnlessCost::Switched(Box::new(cost));
                }
                // Forge's default payer is the targeted object's controller.
                let payer = self.defined(p.get("UnlessPayer").unwrap_or("TargetedController"))?;
                Some((cost, payer))
            }
            None => optional_behold.map(|b| (b, Defined::You)),
        };
        if etb_ok {
            p.ack("ETB", &["True"], "a replacement body acting on the entering object");
        }
        if let Some(rc) = resolution_cost {
            if cost.is_some() {
                return Err("UnlessSwitched with Cost".into());
            }
            cost = Some(rc);
        }
        // An effect that is itself a "you may" choice (casting any number of copies) asks once.
        // Dig consumes `Optional$` as "may take none" (up to); it isn't "you may [look]".
        // `Optional$ You` is `Optional$ True`: Forge reads only the key's presence, and the
        // controller decides unless `resolve::optional_actor` names another player.
        let optional = match p.get("Optional") {
            None => false,
            Some(v) if v.eq_ignore_ascii_case("true") || v == "You" => true,
            Some(v) if v.eq_ignore_ascii_case("false") => false,
            Some(v) => {
                p.reject("Optional", v, "only True / You (the controller decides)");
                false
            }
        } && api != "Dig"
            && !crate::ext2::optional_in_effect(&effect);
        // `RememberX$ True`: the resolving node remembers the objects it affected (`ad.remember`).
        // Each key is honoured only by the resolvers that implement it (`remember_honoured`);
        // a key an ext compiler already consumed is that compiler's. (`RememberObjects$` takes a
        // Defined, never `True`: it is read only by the effects that implement it.)
        let mut remember = false;
        for k in ["RememberAttached", "RememberChanged", "RememberDiscarded", "RememberMilled", "RememberSacrificed", "RememberTargets", "RememberMoved", "RememberCountered", "RememberPumped", "RememberTokens", "RememberOriginalTokens"] {
            let owned = p.consumed(k);
            if p.flag(k) {
                remember = true;
                if !owned && !remember_honoured(k, &effect, p.has("ValidTgts")) {
                    p.reject(k, "True", &format!("the {api} resolver does not remember these objects"));
                }
            }
        }
        let sub = match p.get("SubAbility") {
            // The whole chain was compiled into this one effect (`ext5`, Break Out).
            Some(_) if crate::ext5::absorbs_chain(&effect) || crate::ext9::absorbs_chain(&effect) => None,
            Some(name) => {
                let sp = f.svar(name).and_then(|s| s.as_ability()).ok_or_else(|| format!("SubAbility {name} missing"))?;
                Some(self.ability_inner(&sp, f, uns, depth + 1, etb_ok)?)
            }
            None => None,
        };
        let mut sub = sub;
        // One Forge instruction is one event (CR 608.2c, 603.10a): a node synthesized after this
        // line's node only follows up on it (acts on what it moved or made, sets up a delayed or
        // linked effect); a second zone change synthesized here would be a second event, as
        // "shuffle this and target creature" was. Every synthesized kind is listed.
        for e in &extra_after {
            if !follow_up_node(&e.effect) {
                return Err(format!("{api}: synthesized follow-up node {:?} is not a known follow-up kind (one instruction, one event)", e.effect));
            }
        }
        for mut e in extra_after.into_iter().rev() {
            e.sub = sub;
            sub = Some(self.push_ability(e));
        }
        let sorcery_speed = p.flag("SorcerySpeed");
        // Loyalty abilities are recognised by their cost (CR 606.2); Forge's markers are redundant.
        if cost.as_ref().is_some_and(|c| c.is_loyalty()) {
            p.ack("Planeswalker", &["True", "true"], "loyalty ability marker");
            p.ack("Ultimate", &["True"], "loyalty ability marker");
        }
        let text = p.get("SpellDescription").unwrap_or("").to_string();
        let unused = p.unused();
        if !unused.is_empty() {
            return Err(format!("{api} param {}", unused.join(",")));
        }
        let is_mana = matches!(effect, Effect::Mana { .. } | Effect::Ext(crate::effect_ext::ExtEffect::ManaExiledColors) | Effect::Ext(crate::effect_ext::ExtEffect::Mr(crate::ext4::MrEffect::ManaCount { .. }))) && target.is_none();
        if is_mana {
            let c = cost.clone().unwrap_or_default();
            // "{T}, Tap an untapped [permanent] you control: Add ..." is supported (`tap_x` of 1).
            let tap_other_only = c.tap_x.as_ref().is_some_and(|t| t.0 == 1) && Cost { tap_x: None, ..c.clone() }.has_choices() == false;
            // Counter costs (Weather Maker's "Remove two charge counters") aren't modelled by
            // mana sources (`cast::mana_sources`).
            if !c.tap || !c.mana.syms.is_empty() || (c.has_choices() && !tap_other_only) || c.discard_self || c.counter.is_some() || sub.is_some() || !conditions.is_empty() {
                return Err(format!("mana ability with complex cost/effect {:?}", params.get("Cost")));
            }
        }
        if same_owner_targets && target.as_ref().is_none_or(|t| t.kind != TargetKind::Graveyard) {
            return Err("TargetsWithSameController outside a graveyard".into());
        }
        let ad = AbilityDef { kind, effect, defined, target, cost, sorcery_speed, is_mana, activate_if, conditions, unless, optional, remember, sub, text };
        if !rem_avail && crate::compile_audit::node_reads_memory(&ad)? {
            return Err(format!("{api} reads Remembered/IsRemembered with nothing remembered before it in its chain (Forge card-level memory)"));
        }
        let id = self.push_ability(ad);
        if copy_legendary {
            self.le.copy_legendary.push(id);
        }
        if ability_only_mana {
            self.x6.ability_mana.push(id);
        }
        if let Some((rf, nc)) = mana_restrict {
            self.mana_restrict.push((id, rf, nc));
        }
        if let Some((k, n)) = enter_counters {
            self.enter_counters.push((id, k, n));
        }
        if let Some(n) = library_position {
            self.library_position.push((id, n));
        }
        if let Some(e) = ability_reduce {
            self.ability_reduce.push((id, e));
        }
        if let Some(n) = total_mv {
            self.total_mv.push((id, n));
        }
        if must_find {
            self.search_all.push(id);
        }
        if all_owners {
            self.hidden_all_owners.push(id);
        }
        if dig_rest_top {
            self.lt.dig_rest_top.push(id);
        }
        if dig_imprint {
            self.mr.dig_imprint.push(id);
        }
        if let Some(n) = activation_limit {
            self.activation_limit.push((id, n));
        }
        if exhaust {
            self.exhaust.push(id);
        }
        if power_up {
            self.lt.power_up.push(id);
        }
        if spell_life_x {
            self.spell_life_x.push(id);
        }
        if let Some((a, b)) = charm_optcost {
            self.charm_optcost.push((id, a, b));
        }
        if charm_this_turn {
            self.x7.this_turn_charms.push(id);
        }
        if exclude_triggered {
            self.x9.exclude_triggered.push(id);
        }
        if forget_changed {
            self.x9.forget_changed.push(id);
        }
        if this_and_targets {
            self.x9.this_and_targets.push(id);
        }
        if reveal_found {
            self.x9.reveal_found.push(id);
        }
        if attached_to_parent {
            self.x9.attached_to_parent.push(id);
        }
        if same_owner_targets {
            self.x9.same_owner_targets.push(id);
        }
        if let Some(e) = charm_max_expr {
            self.x9.charm_max.push((id, e));
        }
        Ok(id)
    }

    /// Compile a triggered ability. Its body starts a resolution of its own: only the trigger's
    /// own `Remember…$` keys give it memory. Its event filter and intervening "if" are evaluated
    /// without memory (`triggers::matches`), so reading `IsRemembered` there is refused
    /// (`compile_audit`).
    pub(crate) fn compile_trigger(&mut self, t: &Params, f: &Face, uns: &mut Vec<String>) -> Res<Option<TriggerDef>> {
        let outer = self.rem_avail;
        self.rem_avail = crate::compile_audit::writes_memory(t);
        let r = self.trigger_def(t, f, uns);
        self.rem_avail = outer;
        let r = r?;
        if let Some(td) = &r {
            if crate::compile_audit::trigger_reads_memory(td)? {
                return Err(format!("trigger {:?} reads IsRemembered in its event or intervening if (Forge card-level memory)", t.get("Mode").unwrap_or("?")));
            }
        }
        Ok(r)
    }

    fn trigger_def(&mut self, t: &Params, f: &Face, uns: &mut Vec<String>) -> Res<Option<TriggerDef>> {
        let p = P::new(t);
        let mode_s = p.get("Mode").unwrap_or("?");
        let filt = |k: &str| -> Res<Filter> { self.filter(p.get(k).unwrap_or("Card")) };
        // These keys compile to object filters, which never match a player: a player alternative
        // in them ("Player,Planeswalker", "You,Planeswalker.YouCtrl") would be silently dropped
        //. Only the modes that split off the player part may have one.
        for k in ["ValidCard", "ValidCards", "ValidSource", "ValidTarget", "ValidAttackers"] {
            let Some(v) = p.peek(k) else { continue };
            let player_alt = v.split(',').any(|a| matches!(a.trim().split('.').next(), Some("Player" | "Opponent" | "You")));
            if player_alt && !matches!((mode_s, k), ("DamageDone" | "DamageAll", "ValidTarget")) {
                return Err(format!("trigger {mode_s} {k} {v}: players in an object filter"));
            }
        }
        let mode = match mode_s {
            "ChangesZone" => TrigMode::ChangesZone {
                origin: ZoneSet::parse(p.get("Origin").unwrap_or("Any"))?,
                dest: ZoneSet::parse(p.get("Destination").unwrap_or("Any"))?,
                valid: filt("ValidCard")?,
            },
            "Phase" => {
                let ph = p.get("Phase").ok_or("Phase trigger without Phase")?;
                let count = p.get("PhaseCount");
                let step = match (ph, count) {
                    ("Upkeep", None) => Step::Upkeep,
                    ("Draw", None) => Step::Draw,
                    ("Main1", None) | ("Main", Some("1")) => Step::Main1,
                    ("Main2", None) | ("Main", Some("2")) => Step::Main2,
                    ("BeginCombat", None) => Step::BeginCombat,
                    ("End of Turn", None) | ("End", None) => Step::End,
                    _ => return Err(format!("Phase {ph} {count:?}")),
                };
                TrigMode::Phase { step, player: PlayerSel::parse(p.get("ValidPlayer"))? }
            }
            "Attacks" => TrigMode::Attacks { valid: filt("ValidCard")? },
            "SpellCast" => TrigMode::SpellCast { valid: filt("ValidCard")?, caster: PlayerSel::parse(p.get("ValidActivatingPlayer"))? },
            "DamageDone" => {
                // `ValidTarget$` lists players and objects ("to a player or planeswalker":
                // `Player,Planeswalker`): split it into the player part and the object part, so
                // damage to a player is matched against the former (the whole list
                // became an object filter, which no player matches). No `ValidTarget$`: anything.
                let vt = p.get("ValidTarget").unwrap_or("Any");
                let (mut target_player, mut objs) = (None, vec![]);
                for a in vt.split(',').map(str::trim) {
                    let sel = match a {
                        "Player" => PlayerSel::Any,
                        "Opponent" | "Player.Opponent" => PlayerSel::Opponent,
                        "You" => PlayerSel::You,
                        "Any" => {
                            objs.push(a);
                            PlayerSel::Any
                        }
                        a if matches!(a.split('.').next(), Some("Player" | "Opponent" | "You")) => return Err(format!("DamageDone ValidTarget {a}")),
                        a => {
                            objs.push(a);
                            continue;
                        }
                    };
                    if target_player.replace(sel).is_some() {
                        return Err(format!("DamageDone ValidTarget {vt}: several player parts"));
                    }
                }
                let target_obj = if objs.is_empty() { None } else { Some(self.filter(&objs.join(","))?) };
                TrigMode::DamageDone { source: filt("ValidSource")?, target_player, target_obj, combat_only: p.flag("CombatDamage") }
            }
            "LifeGained" => TrigMode::LifeGained { player: PlayerSel::parse(p.get("ValidPlayer"))? },
            m if self.le_trigger_mode(m, &p)?.is_some() => self.le_trigger_mode(m, &p)?.unwrap(),
            m if self.x_trigger_mode(m, &p)?.is_some() => self.x_trigger_mode(m, &p)?.unwrap(),
            m if self.mr_trigger_mode(m, &p)?.is_some() => self.mr_trigger_mode(m, &p)?.unwrap(),
            m => match (self.more_trigger_mode(m, &p)?, self.ext_trigger_mode(m, &p)?) {
                (Some(t), _) => t,
                (None, Some(x)) => {
                    self.any_zone_batch |= matches!(x, crate::trig_ext::TrigExt::ZoneBatch { .. });
                    self.le.any_after_batch |= matches!(x, crate::trig_ext::TrigExt::ZoneBatchAfter { .. });
                    TrigMode::Ext(x)
                }
                (None, None) => return Err(format!("trigger {m}")),
            },
        };
        // "When you cast this spell" triggers from the stack (CR 113.6, e.g. storm 702.40a: a
        // triggered ability that functions on the stack).
        let self_cast = mode_s == "SpellCast" && t.get("ValidCard") == Some("Card.Self");
        let zones = ZoneSet::parse(p.get("TriggerZones").unwrap_or(if self_cast { "Stack" } else { "Battlefield" }))?;
        let optional = match p.get("OptionalDecider") {
            None => false,
            Some("You") => true,
            Some(d) => return Err(format!("OptionalDecider {d}")),
        };
        let mut intervening = self.conditions(&p, f, "")?;
        // "Solved — [triggered ability]" (Forge `IsPresent$ Card.Self+IsSolved`) means "[ability].
        // This ability triggers only if this Case is solved" (CR 702.169c; Case rulings
        // 2024-02-02): a condition on triggering, not an intervening "if" (603.4), so it isn't
        // checked again on resolution (XMage `withTriggerCondition(SolvedSourceCondition)`). A
        // Case removed in response still has its trigger resolve. `conditions` puts `IsPresent$`
        // first. ("To solve", `Card.Self+!IsSolved`, is an intervening "if", 719.3a.)
        if p.peek("IsPresent") == Some("Card.Self+IsSolved") {
            match intervening.first() {
                Some(Condition::Compare(Expr::CountValid(..), ..)) => {
                    let c = intervening.remove(0);
                    intervening.insert(0, Condition::TriggerOnly(Box::new(c)));
                }
                _ => return Err("Solved trigger condition form".into()),
            }
        }
        // Trigger-event conditions of this batch ("attacks the player with the most life").
        intervening.extend(self.lt_trigger_conditions(mode_s, &p)?);
        intervening.extend(self.mr_trigger_conditions(&p)?);
        // "During your turn" (Suki, Courageous Rescuer): a condition on the trigger event, not an
        // intervening "if" (CR 603.4), so checked only as it triggers (`ext9`).
        if p.peek("PlayerTurn").is_some_and(|v| v.eq_ignore_ascii_case("true")) {
            p.matched("PlayerTurn", |v| v.eq_ignore_ascii_case("true"), "checked above");
            intervening.push(Condition::TriggerOnly(Box::new(Condition::YourTurn(true))));
        }
        // "Whenever you cast an instant or sorcery spell that targets only a single creature you
        // control" (Leyline of Resonance; ruling 2024-09-20: all its targets are that creature).
        if mode_s == "SpellCast" && p.peek("IsSingleTarget").is_some() {
            let (Some(true), Some(v)) = (p.bool_val("IsSingleTarget"), p.get("TargetsValid")) else { return Err("SpellCast IsSingleTarget form".into()) };
            let flt = self.filter_f(f, v)?;
            intervening.push(Condition::TriggerOnly(Box::new(Condition::Compare(Expr::Ext(Box::new(crate::expr_ext::ExtExpr::X9(crate::ext9::X9Expr::SingleTargetValid(flt)))), crate::filter::CmpOp::EQ, Expr::Num(1)))));
        }
        // Hellbent ("if you have no cards in hand"; Case of the Crimson Pulse's "To solve", CR
        // 719.3a: an intervening if, `ext8`).
        if p.flag("Hellbent") {
            intervening.push(Condition::Compare(Expr::Player(crate::expr::Ref::You, crate::expr::PlayerProp::CardsInHand), crate::filter::CmpOp::EQ, Expr::Num(0)));
        }
        if p.flag("NoResolvingCheck") {
            intervening = intervening.into_iter().map(|c| Condition::TriggerOnly(Box::new(c))).collect();
        }
        let per_turn = p.get("ActivationLimit").map(|v| v.parse::<u8>().map_err(|_| format!("ActivationLimit {v}"))).transpose()?;
        let exec = p.get("Execute").ok_or("trigger without Execute")?;
        let text = p.get("TriggerDescription").unwrap_or("").to_string();
        let unused = p.unused();
        if !unused.is_empty() {
            return Err(format!("trigger {mode_s} param {}", unused.join(",")));
        }
        let sv = f.svar(exec).and_then(|s| s.as_ability()).ok_or_else(|| format!("trigger execute {exec} missing"))?;
        // In these modes the trigger's object is the source (`TriggeredSource`, `ext5`).
        self.le.trig_mode = mode_s.to_string();
        // "Whenever ~ deals combat damage to a player" (`ext7`: "that player" in its body).
        self.x7.combat_to_player = mode_s == "DamageDone" && t.get("CombatDamage") == Some("True") && matches!(t.get("ValidTarget"), Some("Player") | Some("Opponent") | Some("Player.Opponent"));
        let execute = self.ability_inner(&sv, f, uns, 1, false);
        self.le.trig_mode.clear();
        self.x7.combat_to_player = false;
        let execute = execute?;
        // `AB$ ... | Cost$ X` as a trigger body: "you may pay X; if you do, ..." (Forge pays it
        // as an optional cost on resolution). Non-activated nodes with a cost do exactly that.
        if self.abilities[execute.0 as usize].kind == AbilityKind::Activated {
            self.abilities[execute.0 as usize].kind = AbilityKind::Sub;
            // "If you do" covers the rest of the body too (CR 608.2c: The
            // Sackville-Bagginses made its Treasure without the sacrifice).
            if self.abilities[execute.0 as usize].cost.is_some() && self.abilities[execute.0 as usize].sub.is_some() {
                self.cost_gates_sub.push(execute);
            }
        }
        // "You may [pay cost]. If you do, ..." with `OptionalDecider$`: the cost prompt already is
        // the choice; asking "use it?" first would be a meaningless extra decision.
        let optional = optional && self.abilities[execute.0 as usize].cost.is_none();
        Ok(Some(TriggerDef { mode, zones, execute, optional, intervening, per_turn, text }))
    }

    fn compile_replacement(&mut self, r: &Params, f: &Face, uns: &mut Vec<String>) -> Res<Option<ReplacementDef>> {
        let p = P::new(r);
        let ev = p.get("Event").unwrap_or("?");
        if ev != "Moved" {
            return self.ext_replacement(ev, &p, f).map(|()| None);
        }
        let mut valid = self.filter(p.get("ValidCard").unwrap_or("Card"))?;
        let dest = ZoneSet::parse(p.get("Destination").unwrap_or("Any"))?;
        let mut origin = ZoneSet::parse(p.get("Origin").unwrap_or("Any"))?;
        // "If a creature an opponent controls would die" (`ValidLKI$`, Garruk, Veiled Butcher;
        // `ext9`): the permanent as it last existed on the battlefield = as it is now, before the
        // move (CR 614.6). Dying = battlefield to graveyard (CR 700.4).
        if let Some(v) = p.get("ValidLKI") {
            if p.peek("ValidCard").is_some() || p.peek("Origin").is_some() || dest != ZoneSet::of(Zone::Graveyard) || !v.starts_with("Creature") {
                return Err("replacement Moved param ValidLKI".into());
            }
            valid = self.filter(v)?;
            origin = ZoneSet::of(Zone::Battlefield);
        }
        let zones = p.get("ActiveZones").map(ZoneSet::parse).transpose()?;
        // `Updated`: the move still happens, modified (Forge `ReplacementHandler`); only the
        // `Modify` action below is that.
        let updated = p.ack("ReplacementResult", &["Updated"], "the move proceeds, modified").is_some();
        let with = p.get("ReplaceWith").ok_or("replacement without ReplaceWith")?;
        let text = p.get("Description").unwrap_or("").to_string();
        let unused = p.unused();
        if !unused.is_empty() {
            return Err(format!("replacement Moved param {}", unused.join(",")));
        }
        let sv = f.svar(with).and_then(|s| s.as_ability()).ok_or_else(|| format!("ReplaceWith {with} missing"))?;
        // `DB$ ChangeZone | Defined$ ReplacedCard | Destination$ X` = redirect the move.
        let action = if sv.api() == Some(("DB", "ChangeZone")) && sv.get("Defined") == Some("ReplacedCard") {
            let d = match sv.get("Destination") {
                Some("Exile") => Zone::Exile,
                Some("Graveyard") => Zone::Graveyard,
                Some("Hand") => Zone::Hand,
                d => return Err(format!("replacement redirect to {d:?}")),
            };
            if updated {
                return Err("replacement redirect with ReplacementResult Updated".into());
            }
            // The redirect is the whole replacement (`zones::redirect`): anything else its
            // ability would do (a sub-ability, counters, remembering the card: The Darkness
            // Crystal's life gain, Head of the Hunt's trigger, Goliath Daydreamer's dream
            // counter) was ignored. Refused; it also keeps every order of applicable redirects
            // to one zone equivalent (CR 616.1).
            if let Some((k, _)) = sv.pairs.iter().find(|(k, _)| !matches!(k.as_str(), "DB" | "Defined" | "Destination" | "Origin" | "Hidden")) {
                return Err(format!("replacement redirect param {k}"));
            }
            ReplAction::Redirect(d)
        } else {
            if !dest.has(Zone::Battlefield) || dest != ZoneSet::of(Zone::Battlefield) {
                return Err("replacement Modify on non-ETB move".into());
            }
            ReplAction::Modify(self.ability_inner(&sv, f, uns, 1, true)?)
        };
        Ok(Some(ReplacementDef { origin, dest, valid, zones, action, text }))
    }

    /// The "you have hexproof" part `compile_static` split off `sd` (same zones, conditions and
    /// text), if any.
    pub(crate) fn take_player_hexproof(&mut self, sd: &StaticDef) -> Option<StaticDef> {
        std::mem::take(&mut self.pending_player_hexproof).then(|| StaticDef { kind: StaticKind::Ext(crate::static_ext::StaticExt::PlayerHexproof), ..sd.clone() })
    }

    pub(crate) fn compile_static(&mut self, s: &Params, f: &Face) -> Res<StaticDef> {
        self.pending_player_hexproof = false;
        let p = P::new(s);
        let mode = p.get("Mode").unwrap_or("?");
        let ext_kind = match self.x9_static(mode, &p)?.map_or_else(|| self.x_static(mode, &p, f), |k| Ok(Some(k)))?.map_or_else(|| self.le_static(mode, &p, f), |k| Ok(Some(k)))? {
            Some(k) => Some(k),
            None => match self.mr_static(mode, &p, f)? {
                Some(k) => Some(k),
                None => self.ext_static(mode, &p, f)?,
            },
        };
        let kind = match mode {
            _ if ext_kind.is_some() => ext_kind.unwrap(),
            // "Damage can't be prevented" (`IsCombat$ True`: combat damage, Frenzied Baloth; CR
            // 615.12). The engine's only prevention effect is a shield counter's (CR 122.1c); any
            // other card that prevents damage is unsupported until it honours this too.
            "CantPreventDamage" => StaticKind::CantPreventDamage { combat_only: p.flag("IsCombat") },
            "Continuous" if p.flag("MayPlay") => {
                let affected = self.filter_f(f, p.get("Affected").ok_or("MayPlay without Affected")?)?;
                let zones = ZoneSet::parse(p.get("AffectedZone").unwrap_or("Exile"))?;
                // CR 609.4b: "mana of any type can be spent to cast it" (`ext7`).
                let any_type = p.flag("MayPlayIgnoreType");
                StaticKind::MayPlay { affected, zones, any_type }
            }
            "Continuous" if p.flag("RemoveAllAbilities") => {
                // Applied to permanents (`StaticKind::RemoveAllAbilities` class).
                p.ack("AffectedZone", &["Battlefield"], "abilities are removed from permanents only");
                StaticKind::RemoveAllAbilities { affected: self.filter_f(f, p.get("Affected").ok_or("RemoveAllAbilities without Affected")?)? }
            }
            "Continuous" if p.has("AffectedDefined") => self.defined_continuous(&p, f)?,
            // "Enchanted creature ... has ward {N}" written with `Affected$ Creature.EnchantedBy`: the
            // same as `AffectedDefined$ Enchanted | Affected$ Creature` (granted ward needs that form).
            "Continuous" if p.peek("Affected").is_some_and(|a| a.ends_with(".EnchantedBy") || a.ends_with(".EquippedBy")) && p.peek("AddKeyword").is_some_and(|k| k.contains("Ward:")) => self.defined_continuous(&p, f)?,
            "Continuous" => {
                if p.get("AffectedZone").is_some_and(|z| z != "Battlefield") {
                    return Err("Continuous outside the battlefield".into());
                }
                let affected = match (p.get("Affected"), p.peek("AffectedDefined")) {
                    (Some(a), _) => self.filter_f(f, a)?,
                    (None, Some("Self")) => {
                        p.ack("AffectedDefined", &["Self"], "checked above");
                        self.filter("Card.Self")?
                    }
                    _ => return Err("Continuous without Affected".into()),
                };
                let ex = |k: &str| -> Res<Option<Expr>> { p.get(k).map(|v| self.value(f, v.trim_start_matches('+'))).transpose() };
                let mut kws = KwSet::default();
                if let Some(k) = p.get("AddKeyword") {
                    for w in k.split('&') {
                        kws = kws.with(crate::compile_audit::granted_kw(w, "static AddKeyword")?);
                    }
                }
                let (power, toughness, set_power, set_toughness) = (ex("AddPower")?, ex("AddToughness")?, ex("SetPower")?, ex("SetToughness")?);
                // "You [and ...] have hexproof" (CR 702.11c): the `You` part is a player static of
                // its own (`StaticExt::PlayerHexproof`), the rest stays an object static; the
                // caller adds the split part (`take_player_hexproof`). Objects never match a
                // player alternative, so it would otherwise be dropped (`compile_audit`).
                let mut affected = affected;
                if kws == KwSet::default().with(Kw::Hexproof) && power.is_none() && toughness.is_none() && set_power.is_none() && set_toughness.is_none() {
                    let n = affected.alts.len();
                    affected.alts.retain(|a| !(a.base == crate::filter::Base::You && a.props.is_empty()));
                    if affected.alts.is_empty() {
                        StaticKind::Ext(crate::static_ext::StaticExt::PlayerHexproof)
                    } else {
                        self.pending_player_hexproof = affected.alts.len() < n;
                        StaticKind::Continuous { affected, power, toughness, set_power, set_toughness, kws }
                    }
                } else {
                    StaticKind::Continuous { affected, power, toughness, set_power, set_toughness, kws }
                }
            }
            "ReduceCost" if p.peek("Type") == Some("Ability") || p.peek("ValidSpell") == Some("Activated.Equip") => {
                // Activated abilities cost less (CR 601.2f via 602.2b): of matching permanents
                // (`Type$ Ability`, Mutagen Man) or equip abilities targeting this creature
                // (`ValidSpell$ Activated.Equip | ValidTarget$ Card.Self`, Dwarven Mauler).
                let equip_only = p.ack("ValidSpell", &["Activated.Equip"], "equip abilities").is_some();
                if equip_only && p.has("Type") {
                    return Err("ReduceCost ability form".into());
                }
                p.ack("Type", &["Ability"], "activated abilities");
                let target_self = match p.get("ValidTarget") {
                    None => false,
                    Some("Card.Self") => true,
                    Some(t) => return Err(format!("ReduceCost ValidTarget {t}")),
                };
                if p.get("AffectedZone").is_some_and(|z| z != "Battlefield") {
                    return Err("ReduceCost AffectedZone".into());
                }
                let valid = p.get("ValidCard").map(|v| self.filter_f(f, v)).transpose()?;
                self.any_ability_reduce = true;
                StaticKind::Ext(crate::static_ext::StaticExt::AbilityCostReduce { valid, equip_only, target_self, player: PlayerSel::parse(p.get("Activator"))?, amount: self.value(f, p.get("Amount").unwrap_or("1"))? })
            }
            "AlternativeCost" if p.peek("ValidSA") == Some("Activated.Equip") => {
                // "you may pay {0} rather than pay the equip cost of the first equip ability you
                // activate each turn" (Kíli): paying {0} is never worse, so it's applied
                // automatically when allowed (no meaningless choice).
                p.ack("ValidSA", &["Activated.Equip"], "equip abilities");
                if p.get("Cost") != Some("0") {
                    return Err("AlternativeCost for equip other than 0".into());
                }
                self.any_ability_reduce = true;
                StaticKind::Ext(crate::static_ext::StaticExt::FreeEquip { player: PlayerSel::parse(p.get("ValidPlayer"))? })
            }
            "ReduceCost" if p.flag("Relative") => {
                // "This spell costs {1} less to cast for each permanent sacrificed this way": the
                // reduction of `FaceExtra::sac_any_reduce` (checked against it in compile_face).
                let x_paid = p.get("Amount").and_then(|a| f.svar(a)).is_some_and(|v| v.value.trim() == "Count$xPaid");
                if p.get("ValidCard") != Some("Card.Self") || p.get("Type") != Some("Spell") || p.get("EffectZone") != Some("All") || !x_paid {
                    return Err("ReduceCost Relative form".into());
                }
                StaticKind::Ext(crate::static_ext::StaticExt::SacrificeReduces)
            }
            "ReduceCost" => {
                if p.get("Type").unwrap_or("Spell") != "Spell" {
                    return Err("ReduceCost non-spell".into());
                }
                StaticKind::ReduceCost {
                    valid: self.filter(p.get("ValidCard").unwrap_or("Card"))?,
                    caster: PlayerSel::parse(p.get("Activator"))?,
                    amount: self.value(f, p.get("Amount").unwrap_or("1"))?,
                }
            }
            "CantBlockBy" => StaticKind::CantBlockBy {
                attacker: self.filter_f(f, p.get("ValidAttacker").ok_or("CantBlockBy without ValidAttacker")?)?,
                blocker: p.get("ValidBlocker").map(|b| self.filter_f(f, b)).transpose()?,
            },
            "OptionalCost" | "CantBeCast" => StaticKind::Alt(self.alt_static(&p, mode)?),
            m => return Err(format!("static {m}")),
        };
        if matches!(&kind, StaticKind::DefinedContinuous(dc) if dc.hexproof_from_mono) {
            self.any_hexproof_from = true;
        }
        let zones = ZoneSet::parse(p.get("EffectZone").unwrap_or("Battlefield"))?;
        let mut conditions = self.conditions(&p, f, "")?;
        match p.get("Condition") {
            None => {}
            Some("PlayerTurn") => conditions.push(Condition::YourTurn(true)),
            Some("NotPlayerTurn") => conditions.push(Condition::YourTurn(false)),
            // Storied's designation (CR 702.195b).
            Some("EnduringStory") => conditions.push(Condition::Compare(Expr::Ext(Box::new(crate::expr_ext::ExtExpr::EnduringStory)), crate::filter::CmpOp::EQ, Expr::Num(1))),
            // CR 702.178a: "As long as your speed is 4".
            // Threshold (CR 207.2c ability word): "as long as seven or more cards are in your
            // graveyard".
            Some("Threshold") => conditions.push(Condition::Compare(Expr::Player(crate::expr::Ref::You, crate::expr::PlayerProp::CardsInGraveyard), crate::filter::CmpOp::GE, Expr::Num(7))),
            Some("MaxSpeed") => conditions.push(Condition::Compare(Expr::Ext(Box::new(crate::expr_ext::ExtExpr::Speed)), crate::filter::CmpOp::EQ, Expr::Num(4))),
            Some(c) => return Err(format!("static Condition {c}")),
        }
        let text = p.get("Description").unwrap_or("").to_string();
        let unused = p.unused();
        if !unused.is_empty() {
            return Err(format!("static {mode} param {}", unused.join(",")));
        }
        Ok(StaticDef { kind, zones, conditions, text })
    }
}

impl CardDb {
    /// A ward trigger (CR 702.21a) for objects matching `valid`: counter the spell/ability unless
    /// its controller pays the cost.
    fn ward_trigger(&mut self, c: &str, valid: Filter, conditions: &[Condition]) -> Option<TriggerDef> {
        let cost = if let Some(n) = c.strip_prefix("PayLife<").and_then(|r| r.strip_suffix('>')) {
            UnlessCost::Life(n.parse().ok()?)
        } else if !c.contains('<') {
            UnlessCost::Mana(ManaCost::parse_forge(c).ok()?)
        } else {
            return None;
        };
        let execute = self.push_ability(AbilityDef {
            kind: AbilityKind::Sub,
            effect: Effect::Counter,
            defined: Some(Defined::Triggered),
            target: None,
            cost: None,
            sorcery_speed: false,
            is_mana: false,
            activate_if: vec![],
            conditions: vec![],
            unless: Some((cost, Defined::TriggeredController)),
            optional: false,
            remember: false,
            sub: None,
            text: format!("Ward {c}"),
        });
        Some(TriggerDef { mode: TrigMode::BecomesTarget { valid, by: PlayerSel::Opponent }, zones: ZoneSet::of(Zone::Battlefield), execute, optional: false, intervening: conditions.to_vec(), per_turn: None, text: format!("Ward {c} (granted)") })
    }

    /// `Mode$ Continuous | AffectedDefined$ Self/Equipped/Enchanted | ...` (keywords, types, P/T).
    fn defined_continuous(&mut self, p: &P, f: &Face) -> Res<StaticKind> {
        let who = match p.get("AffectedDefined").unwrap_or("Enchanted") {
            "Self" => StaticWho::SelfObj,
            "Equipped" | "Enchanted" => StaticWho::Attached,
            d => return Err(format!("AffectedDefined {d}")),
        };
        if p.get("AffectedZone").is_some_and(|z| z != "Battlefield") {
            return Err("Continuous outside the battlefield".into());
        }
        let mut abilities = vec![];
        if let Some(names) = p.get("AddAbility") {
            for name in names.split('&') {
                let sv = f.svar(name.trim()).and_then(|s| s.as_ability()).ok_or_else(|| format!("AddAbility {name} missing"))?;
                let mut uns = vec![];
                let a = self.ability_inner(&sv, f, &mut uns, 1, false)?;
                if !uns.is_empty() {
                    return Err(uns.join("; "));
                }
                abilities.push(a);
            }
        }
        let affected = p.get("Affected").map(|a| self.filter_f(f, a)).transpose()?;
        let ex = |k: &str| -> Res<Option<Expr>> { p.get(k).map(|v| self.value(f, v.trim_start_matches('+'))).transpose() };
        let mut kws = KwSet::default();
        let mut ward = None;
        let mut hexproof_from_mono = false;
        if let Some(k) = p.get("AddKeyword") {
            for w in k.split('&') {
                if let Some(c) = w.trim().strip_prefix("Ward:") {
                    ward = Some(c.to_string());
                    continue;
                }
                if w.trim() == "Hexproof:Card.MonoColor:monocolored" {
                    hexproof_from_mono = true;
                    continue;
                }
                kws = kws.with(crate::compile_audit::granted_kw(w, "static AddKeyword")?);
            }
        }
        let (mut add_types, mut add_subtypes) = (TypeSet::default(), vec![]);
        if let Some(t) = p.get("AddType") {
            for w in t.split('&') {
                let w = w.trim();
                match CardType::parse(w) {
                    Some(ct) => add_types = add_types.with(ct),
                    None => add_subtypes.push(self.subtype(w).ok_or_else(|| format!("static AddType {w}"))?),
                }
            }
        }
        Ok(StaticKind::DefinedContinuous(Box::new(DefinedCont {
            who,
            affected,
            add_types,
            add_subtypes,
            remove_card_types: p.flag("RemoveCardTypes"),
            power: ex("AddPower")?,
            toughness: ex("AddToughness")?,
            set_power: ex("SetPower")?,
            set_toughness: ex("SetToughness")?,
            kws,
            ward,
            abilities,
            hexproof_from_mono,
        })))
    }
}

impl CardDb {
    /// `SP$ PermanentCreature | Cost$ <mana cost> ExileFromGrave<N/<filter>>`: the permanent
    /// spell with "exile N [filter] cards from your graveyard" as a mandatory additional cost.
    fn exile_grave_spell(&mut self, a: &forge_script::Params, f: &Face) -> Res<crate::altcast::AddCost> {
        let p = P::new(a);
        p.ack("SP", &["PermanentCreature"], "checked by the caller");
        let cost = p.get("Cost").ok_or("PermanentCreature without Cost")?;
        let mut mana = vec![];
        let mut add = None;
        for part in cost_tokens(cost) {
            if let Some(b) = part.strip_prefix("ExileFromGrave<").and_then(|r| r.strip_suffix('>')) {
                let mut it = b.splitn(3, '/');
                let n = it.next().and_then(|x| x.parse::<u8>().ok()).ok_or("ExileFromGrave count")?;
                let t = it.next().unwrap_or("Card");
                add = Some(crate::altcast::AddCost::ExileFromGrave(n, self.filter_f(f, t)?));
            } else {
                mana.push(part);
            }
        }
        if mana.join(" ") != f.mana_cost.as_deref().unwrap_or("").trim() || !p.unused().is_empty() {
            return Err("API PermanentCreature".into());
        }
        add.ok_or_else(|| "API PermanentCreature".into())
    }

    /// `SP$ PermanentCreature | Cost$ <mana cost> Sac<X/<filter>/..>`: the permanent spell with
    /// "you may sacrifice any number of [filter]" as an additional cost. Returns the filter.
    fn sac_any_spell(&mut self, a: &forge_script::Params, f: &Face) -> Res<crate::filter::Filter> {
        let p = P::new(a);
        p.ack("SP", &["PermanentCreature"], "checked by the caller");
        p.display("AdditionalDesc");
        let cost = p.get("Cost").ok_or("PermanentCreature without Cost")?;
        let mut mana = vec![];
        let mut flt = None;
        for part in cost_tokens(cost) {
            if let Some(b) = part.strip_prefix("Sac<X/").and_then(|r| r.strip_suffix('>')) {
                flt = Some(self.filter_f(f, b.split('/').next().unwrap_or(b))?);
            } else {
                mana.push(part);
            }
        }
        if mana.join(" ") != f.mana_cost.as_deref().unwrap_or("").trim() || !p.unused().is_empty() {
            return Err("API PermanentCreature".into());
        }
        flt.ok_or_else(|| "API PermanentCreature".into())
    }

    /// `Animate | Defined$ Self | RemoveAllAbilities$ True | Name$ N | Types$ .. | Keywords$ ..
    /// | staticAbilities$ .. | Duration$ Permanent` (The Irencrag becoming Everflame): the permanent
    /// permanently becomes a named object with only the listed abilities. Compiled as a
    /// synthetic card (the source face with the new name, added types and only those abilities)
    /// that the permanent shows from then on, like a transformed permanent.
    fn animate_become(&mut self, p: &P, f: &Face) -> Res<Effect> {
        if p.get("Defined") != Some("Self") || p.get("Duration") != Some("Permanent") || !p.flag("RemoveAllAbilities") {
            return Err("Animate RemoveAllAbilities form".into());
        }
        let name = p.get("Name").unwrap().to_string();
        let mut types: Vec<String> = f.types.as_deref().unwrap_or("").split_whitespace().map(String::from).collect();
        for t in p.get("Types").unwrap_or("").split(',').map(str::trim).filter(|t| !t.is_empty()) {
            if !types.iter().any(|x| x == t) {
                types.push(t.to_string());
            }
        }
        // Supertypes first, then card types, then subtypes (Forge type-line order).
        types.sort_by_key(|t| match t.as_str() {
            "Legendary" | "Basic" | "Snow" | "World" => 0,
            t if CardType::parse(t).is_some() => 1,
            _ => 2,
        });
        let mut face = Face { name: name.clone(), types: Some(types.join(" ")), oracle: Some(String::new()), ..f.clone() };
        face.keywords = p.get("Keywords").map(|k| k.split('&').map(|x| x.trim().to_string()).collect()).unwrap_or_default();
        face.abilities = vec![];
        face.triggers = vec![];
        face.replacements = vec![];
        face.statics = vec![];
        for n in p.get("staticAbilities").unwrap_or("").split(',').map(str::trim).filter(|n| !n.is_empty()) {
            let sv = f.svar(n).ok_or_else(|| format!("Animate static {n} missing"))?;
            face.statics.push(forge_script::Params::parse(&sv.value));
        }
        face.other.retain(|(k, _)| k != "AlternateMode");
        let saved = std::mem::take(&mut self.pending_statics);
        let mut uns = vec![];
        let fd = self.compile_face(&face, &mut uns);
        self.pending_statics = saved;
        self.pending_target = None;
        if !uns.is_empty() {
            return Err(format!("Animate into {name}: {}", uns.join("; ")));
        }
        let card = CardId(self.cards.len() as u32);
        self.cards.push(CardDef { name, layout: Layout::Normal, faces: vec![fd], is_token: false, unsupported: vec![] });
        Ok(Effect::Ext(crate::effect_ext::ExtEffect::BecomeCard { card }))
    }
}

/// Split a Forge cost string on whitespace, keeping `<...>` groups (which may contain spaces).
fn cost_tokens(s: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let mut depth = 0i32;
    for w in s.split_whitespace() {
        if depth > 0 {
            let last = out.last_mut().unwrap();
            last.push(' ');
            last.push_str(w);
        } else {
            out.push(w.to_string());
        }
        depth += w.matches('<').count() as i32 - w.matches('>').count() as i32;
    }
    out
}

impl CardDb {
    fn parse_cost(&mut self, f: &Face, s: &str) -> Res<Cost> {
        let mut c = Cost::default();
        let mut mana = vec![];
        for part in cost_tokens(s) {
            let inner = |pre: &str| part.strip_prefix(pre).and_then(|r| r.strip_suffix('>')).map(|r| r.to_string());
            // `<N/Type[/description]>` → (N, Type)
            let n_type = |body: &str| -> Res<(u8, String)> {
                let mut it = body.splitn(3, '/');
                let n = it.next().unwrap_or("").parse::<u8>().map_err(|_| format!("cost part {part}"))?;
                Ok((n, it.next().unwrap_or("").to_string()))
            };
            if part == "T" {
                c.tap = true;
            } else if let Some(b) = inner("Sac<") {
                let (n, t) = n_type(&b)?;
                if t == "CARDNAME" {
                    c.sac_self = true;
                } else {
                    // `Creature.Other;Artifact.Other`: ';' separates alternatives in costs.
                    c.sac = Some((n, self.filter_f(f, &t.replace(';', ","))?));
                }
            } else if let Some(b) = inner("Discard<") {
                let (n, t) = n_type(&b)?;
                if t == "CARDNAME" {
                    c.discard_self = true;
                } else {
                    c.discard = Some((n, self.filter_f(f, &t)?));
                }
            } else if let Some(b) = inner("ExileFromGrave<") {
                let (n, t) = n_type(&b)?;
                if t == "CARDNAME" {
                    // "Exile this card from your graveyard" (graveyard-activated abilities).
                    if n != 1 {
                        return Err(format!("cost part {part}"));
                    }
                    c.exile_self = true;
                } else {
                    c.exile_from_grave = Some((n, self.filter_f(f, &t)?));
                }
            } else if let Some(b) = inner("tapXType<") {
                let (n, t) = n_type(&b)?;
                c.tap_x = Some((n, self.filter_f(f, &t)?));
            } else if inner("Exile<1/CARDNAME").is_some() || part.starts_with("Exile<1/CARDNAME/") {
                c.exile_self = true;
            } else if let Some(b) = inner("RemoveAnyCounter<").filter(|b| b.starts_with("1/Any/CARDNAME") || b.starts_with("1/Any/NICKNAME")) {
                // "Remove a counter from [this]" (any kind; `ext5`).
                let _ = b;
                c.remove_any = 1;
            } else if inner("SubCounter<").is_some_and(|b| b == "X/LOYALTY") && f.svar("X").is_some_and(|v| v.value.trim() == "Count$xPaid") {
                // "[-X]" (CR 606.4, 107.3): X is announced as the ability is activated and that
                // many loyalty counters are removed; the effect's X is the value paid (`ext6`).
                if c.counter.is_some() {
                    return Err(format!("cost part {part}"));
                }
                c.counter = Some((CounterKind::Loyalty, 0));
                c.x_loyalty = true;
            } else if let Some(b) = inner("AddCounter<").or_else(|| inner("SubCounter<")) {
                // CR 606.4: loyalty costs; also other counter costs on the source.
                let (n, t) = n_type(&b)?;
                let sign = if part.starts_with("Sub") { -1 } else { 1 };
                if c.counter.is_some() || t.is_empty() {
                    return Err(format!("cost part {part}"));
                }
                let kind = self.counter_kind(&t);
                c.counter = Some((kind, sign * n as i32));
            } else if let Some(b) = inner("PayLife<") {
                c.pay_life = b.parse().map_err(|_| format!("cost part {part}"))?;
            } else if !part.contains('<') && part.len() <= 3 && part.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '/') {
                mana.push(part.clone());
            } else {
                return Err(format!("cost part {part}"));
            }
        }
        if !mana.is_empty() {
            c.mana = ManaCost::parse_forge(&mana.join(" "))?;
        }
        Ok(c)
    }
}

// ---------------------------------------------------------------- alternative/additional costs

impl CardDb {
    fn alt_ability(&mut self, e: crate::altcast::AltEffect, text: &str) -> AbilityId {
        self.push_ability(AbilityDef {
            kind: AbilityKind::Sub,
            effect: Effect::Alt(e),
            defined: Some(Defined::SelfObj),
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
            text: text.to_string(),
        })
    }

    fn alt_trigger(&mut self, mode: TrigMode, intervening: Vec<Condition>, e: crate::altcast::AltEffect, text: &str) -> TriggerDef {
        let execute = self.alt_ability(e, text);
        TriggerDef { mode, zones: ZoneSet::of(Zone::Battlefield), execute, optional: false, intervening, per_turn: None, text: text.to_string() }
    }

    /// `Cost$`-style additional cost component (`PayLife<3>`, `Discard<1/Card>`, `Blight<1>`, `{1}`...).
    fn alt_add_cost(&mut self, s: &str) -> Res<crate::altcast::AddCost> {
        use crate::altcast::AddCost;
        let inner = |pre: &str| s.strip_prefix(pre).and_then(|r| r.strip_suffix('>'));
        let n_type = |b: &str| -> Res<(u8, String)> {
            let mut it = b.splitn(3, '/');
            let n = it.next().unwrap_or("").parse::<u8>().map_err(|_| format!("cost {s}"))?;
            Ok((n, it.next().unwrap_or("").replace(';', ",")))
        };
        Ok(if let Some(b) = inner("PayLife<") {
            AddCost::Life(b.parse().map_err(|_| format!("cost {s}"))?)
        } else if let Some(b) = inner("Discard<") {
            let (n, t) = n_type(b)?;
            AddCost::Discard(n, self.filter(&t)?)
        } else if let Some(b) = inner("Sac<") {
            let (n, t) = n_type(b)?;
            AddCost::Sac(n, self.filter(&t)?)
        } else if let Some(b) = inner("Blight<") {
            AddCost::Blight(b.parse().map_err(|_| format!("cost {s}"))?)
        } else if let Some(b) = inner("Behold<") {
            let (n, t) = n_type(b)?;
            if n != 1 {
                return Err(format!("cost {s}"));
            }
            AddCost::Behold(self.filter(&t)?)
        } else if let Some(b) = inner("CollectEvidence<") {
            AddCost::CollectEvidence(b.parse().map_err(|_| format!("cost {s}"))?)
        } else if !s.contains('<') {
            AddCost::Mana(ManaCost::parse_forge(s)?)
        } else {
            return Err(format!("cost {s}"));
        })
    }

    /// Keywords for alternative/additional costs (`altcast`). `None`: not one of these.
    fn alt_keyword(&mut self, parts: &[&str], alt: &mut crate::altcast::AltDef, synth: &mut Vec<TriggerDef>) -> Option<Res<()>> {
        use crate::altcast::{flag, AltEffect, AltExpr};
        let self_filter = Filter { alts: vec![crate::filter::Alt { base: crate::filter::Base::Card, props: vec![crate::filter::Prop::SelfCard] }] };
        let cast_flag = |f: u16| Condition::Compare(Expr::Alt(AltExpr::CastFlag(f, 1, 0)), crate::filter::CmpOp::GE, Expr::Num(1));
        let etb = TrigMode::ChangesZone { origin: ZoneSet::ANY, dest: ZoneSet::of(Zone::Battlefield), valid: self_filter.clone() };
        let mc = |c: &str| ManaCost::parse_forge(c);
        let mut r: Res<()> = Ok(());
        match parts {
            ["Warp", c] => match mc(c) {
                Ok(m) => {
                    alt.warp = Some(m);
                    alt.warp_trigger = Some(synth.len() as u8);
                    let t = self.alt_trigger(TrigMode::Delayed, vec![], AltEffect::WarpExile, "Warp: exile this at the beginning of the next end step");
                    synth.push(t);
                }
                Err(e) => r = Err(e),
            },
            ["Evoke", c] => match mc(c) {
                Ok(m) => {
                    alt.evoke = Some(m);
                    let t = self.alt_trigger(etb, vec![cast_flag(flag::EVOKE)], AltEffect::SacrificeSelf, "Evoke: when this enters, if its evoke cost was paid, sacrifice it");
                    synth.push(t);
                }
                Err(e) => r = Err(e),
            },
            ["Impending", n, c] => match (n.parse::<i32>(), mc(c)) {
                (Ok(n), Ok(m)) => {
                    let k = self.counter_kind("TIME");
                    alt.impending = Some((n, m, k));
                    let conds = vec![cast_flag(flag::IMPENDING), Condition::Compare(Expr::Alt(AltExpr::SelfCounters(k)), crate::filter::CmpOp::GE, Expr::Num(1))];
                    let t = self.alt_trigger(TrigMode::Phase { step: Step::End, player: PlayerSel::You }, conds, AltEffect::ImpendingTick(k), "Impending: at the beginning of your end step, remove a time counter");
                    synth.push(t);
                }
                _ => r = Err(format!("Impending {n}:{c}")),
            },
            ["Harmonize", c] => r = mc(c).map(|m| alt.harmonize = Some(m)),
            // CR 702.190a (returning the unblocked attacker: `altcast::announce_with`).
            ["Sneak", c] => r = mc(c).map(|m| alt.sneak = Some(m)),
            // CR 702.188a (the returned creature: `altcast::web_candidates`).
            ["Web-slinging", c] => r = mc(c).map(|m| alt.web_slinging = Some(m)),
            // CR 702.187b.
            ["Mayhem", c] => r = mc(c).map(|m| alt.mayhem = Some(m)),
            ["Plot", c] => r = mc(c).map(|m| alt.plot = Some(m)),
            ["Offspring", c] => match mc(c) {
                Ok(m) => {
                    alt.offspring = Some(m);
                    let t = self.alt_trigger(etb, vec![cast_flag(flag::OFFSPRING)], AltEffect::OffspringCopy, "Offspring: when this enters, if its offspring cost was paid, create a 1/1 token copy of it");
                    synth.push(t);
                }
                Err(e) => r = Err(e),
            },
            ["Affinity", t] => r = self.filter(t).map(|f| alt.affinity = Some(f)),
            // A single cost whose `;` lists alternatives of one kind ("sacrifice an artifact or
            // creature", Dusk Rose Reliquary): a mandatory additional cost (CR 601.2b).
            ["AlternateAdditionalCost", a] if a.starts_with("Sac<") && alt.extra_cost.is_none() => {
                r = (|| {
                    alt.extra_cost = Some(self.alt_add_cost(a)?);
                    Ok(())
                })()
            }
            ["AlternateAdditionalCost", a, b] => {
                r = (|| {
                    alt.either = Some((self.alt_add_cost(a)?, self.alt_add_cost(b)?));
                    Ok(())
                })()
            }
            ["Teamwork", n] => r = n.parse().map(|n| alt.teamwork = Some(n)).map_err(|_| format!("Teamwork {n}")),
            ["Spree"] | ["Tiered"] => alt.mode_costs = true,
            // CR 702.166a: "As an additional cost to cast this spell, you may sacrifice an
            // artifact, enchantment, or token."
            ["Bargain"] => {
                let c = crate::altcast::AddCost::Sac(1, self.filter("Artifact,Enchantment,Permanent.token").unwrap());
                self.pending_statics.push(StaticDef { kind: StaticKind::Alt(crate::altcast::AltStatic::OptionalCost(c)), zones: ZoneSet::of(Zone::Stack), conditions: vec![], text: "Bargain".into() });
            }
            ["Mobilize", n] => {
                r = (|| {
                    let n: u8 = n.parse().map_err(|_| format!("Mobilize {n}"))?;
                    let token = self.token("r_1_1_warrior")?;
                    // "Sacrifice them at the beginning of the next end step": one delayed
                    // trigger for all the tokens (CR 702.181a, 603.7a).
                    let sac = match self.cards[token.0 as usize].faces[0].alt.mobilize_sac {
                        Some(a) => a,
                        None => {
                            let a = self.alt_ability(AltEffect::SacrificeRemembered, "Sacrifice them at the beginning of the next end step (mobilize)");
                            self.cards[token.0 as usize].faces[0].alt.mobilize_sac = Some(a);
                            a
                        }
                    };
                    let t = self.alt_trigger(TrigMode::Attacks { valid: self_filter.clone() }, vec![], AltEffect::Mobilize { token, n, sac }, &format!("Mobilize {n}"));
                    synth.push(t);
                    Ok(())
                })()
            }
            _ => return None,
        }
        Some(r)
    }

    /// Casting-related statics: `OptionalCost` (CR 118.8b), `CantBeCast`.
    fn alt_static(&mut self, p: &P, mode: &str) -> Res<crate::altcast::AltStatic> {
        use crate::altcast::AltStatic;
        match mode {
            "OptionalCost" => {
                if p.get("ValidCard") != Some("Card.Self") || p.get("ValidSA") != Some("Spell") {
                    return Err("OptionalCost on other spells".into());
                }
                let c = p.get("Cost").ok_or("OptionalCost without Cost")?;
                Ok(AltStatic::OptionalCost(self.alt_add_cost(c)?))
            }
            _ => {
                let your_turn_only = match p.get("Condition") {
                    None => false,
                    Some("PlayerTurn") => true,
                    Some(c) => return Err(format!("CantBeCast Condition {c}")),
                };
                Ok(AltStatic::CantBeCast { valid: self.filter(p.get("ValidCard").unwrap_or("Card"))?, caster: PlayerSel::parse(p.get("Caster"))?, your_turn_only })
            }
        }
    }
}
