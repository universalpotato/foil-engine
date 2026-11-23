//! Compiler for effect APIs and parameters added after the core set (see `effect_ext`).

use crate::carddb::*;
use crate::compile::P;
use crate::effect_ext::{ExtEffect, TypeChoice};
use crate::expr::Expr;
use crate::state::Zone;
use forge_script::Face;

type Res<T> = Result<T, String>;

/// A bare sub-ability node.
pub(crate) fn node(effect: Effect, defined: Option<Defined>) -> AbilityDef {
    AbilityDef {
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
    }
}

fn zone(s: &str) -> Res<Zone> {
    Ok(match s {
        "Battlefield" => Zone::Battlefield,
        "Graveyard" => Zone::Graveyard,
        "Hand" => Zone::Hand,
        "Exile" => Zone::Exile,
        "Library" => Zone::Library,
        d => return Err(format!("zone {d}")),
    })
}

impl CardDb {
    pub(crate) fn ext_api(&mut self, api: &str, p: &P, f: &Face, _uns: &mut Vec<String>, _depth: u32, _extra: &mut Vec<AbilityDef>) -> Res<Option<Effect>> {
        let num = |db: &CardDb, k: &str, default: i32| -> Res<Expr> {
            match p.get(k) {
                Some(v) => db.value(f, v),
                None => Ok(Expr::Num(default)),
            }
        };
        Ok(Some(match api {
            "MultiplyCounter" => {
                let kind = self.counter_kind(p.get("CounterType").ok_or("MultiplyCounter without CounterType")?);
                let factor = p.get("Multiplier").unwrap_or("2").parse().map_err(|_| "MultiplyCounter Multiplier")?;
                Effect::Ext(ExtEffect::MultiplyCounter { kind, factor })
            }
            "ChooseType" => {
                let kind = match p.get("Type") {
                    Some("Creature") => TypeChoice::Creature,
                    Some("Basic Land") => TypeChoice::BasicLand,
                    t => return Err(format!("ChooseType Type {t:?}")),
                };
                let chooser = self.defined(p.get("Defined").unwrap_or("You"))?;
                Effect::Ext(ExtEffect::ChooseType { kind, chooser })
            }
            "ChooseCard" => {
                let zone = match p.get("ChoiceZone") {
                    None | Some("Battlefield") => Zone::Battlefield,
                    Some("Exile") => Zone::Exile,
                    Some(z) => return Err(format!("ChooseCard ChoiceZone {z}")),
                };
                let choices = self.filter_f(f, p.get("Choices").ok_or("ChooseCard without Choices")?)?;
                let chooser = if p.has("ValidTgts") { Defined::Targeted } else { self.defined(p.get("Defined").unwrap_or("You"))? };
                let up_to = !p.flag("Mandatory");
                Effect::Ext(ExtEffect::ChooseCard { choices, chooser, n: num(self, "Amount", 1)?, up_to, zone })
            }
            "RevealHand" => Effect::Ext(ExtEffect::RevealHand),
            "Repeat" => self.punisher(p, f)?,
            // CR 701.14a: the defined creature and the target fight.
            "Fight" if p.peek("Defined").is_some() && p.peek("ValidTgts").is_some() => Effect::Ext(ExtEffect::Fight { parent: p.peek("Defined") == Some("ParentTarget") }),
            "CopySpellAbility" => {
                // "copy this spell" from a cast trigger (CR 707.10): same choices, no new targets.
                if p.get("Defined") != Some("TriggeredSpellAbility") {
                    return Err("CopySpellAbility of other spells".into());
                }
                Effect::Ext(ExtEffect::CopyTriggeredSpell)
            }
            "AnimateAll" if p.flag("RemoveAllAbilities") => {
                // "Each [matching] creature loses all abilities until end of turn."
                if p.has("Duration") {
                    return Err("AnimateAll Duration".into());
                }
                let valid = crate::compile::targeted_players_only(p, api, self.filter_f(f, p.get("ValidCards").ok_or("AnimateAll without ValidCards")?)?)?;
                let affected = self.filter("Card.IsRemembered")?;
                let si = self.statics_pool.len() as u32;
                self.statics_pool.push(StaticDef { kind: StaticKind::RemoveAllAbilities { affected }, zones: ZoneSet::ANY, conditions: vec![], text: "Loses all abilities until end of turn.".into() });
                Effect::Ext(ExtEffect::LoseAllAbilitiesAll { valid, static_idx: si })
            }
            "Connive" => Effect::Ext(ExtEffect::Connive { n: num(self, "ConniveNum", 1)? }),
            "RepeatEach" => {
                let players = match p.get("RepeatPlayers") {
                    Some("Player") => None,
                    Some("Targeted") | Some("TargetedPlayer") => Some(Defined::Targeted),
                    r => return Err(format!("RepeatEach {r:?}")),
                };
                // Damage dealt by the repetitions is one simultaneous event in Forge; with one
                // damage source per repetition the result is the same.
                p.ack("DamageMap", &["True"], "one damage event per repetition");
                let name = p.get("RepeatSubAbility").ok_or("RepeatEach without RepeatSubAbility")?;
                let sv = f.svar(name).and_then(|s| s.as_ability()).ok_or_else(|| format!("RepeatSubAbility {name} missing"))?;
                let sub = self.ability_inner(&sv, f, _uns, _depth + 1, false)?;
                Effect::Ext(ExtEffect::RepeatEachPlayer { players, sub })
            }
            "Branch" => {
                let cond = {
                    let (intern, face) = (|s: &str| self.subtype(s), f);
                    let ec = crate::expr::ExprCompiler { face, intern: &intern, counters: &self.counter_names };
                    ec.check_svar(p.get("BranchConditionSVar").ok_or("Branch without condition")?, p.get("BranchConditionSVarCompare"))?
                };
                let mut sub = |db: &mut CardDb, k: &str| -> Res<Option<AbilityId>> {
                    match p.get(k) {
                        None => Ok(None),
                        Some(n) => {
                            let sv = f.svar(n).and_then(|s| s.as_ability()).ok_or_else(|| format!("Branch {n} missing"))?;
                            Ok(Some(db.ability_inner(&sv, f, _uns, _depth + 1, false)?))
                        }
                    }
                };
                let then = sub(self, "TrueSubAbility")?;
                let otherwise = sub(self, "FalseSubAbility")?;
                Effect::Ext(ExtEffect::Branch { cond, then, otherwise })
            }
            "Earthbend" => {
                // CR 701.66a: "target land you control".
                if !p.has("ValidTgts") {
                    self.pending_target = None;
                }
                let n = num(self, "Num", 1)?;
                let mut ret = node(
                    Effect::ChangeZone {
                        origin: ZoneSet(ZoneSet::of(Zone::Graveyard).0 | ZoneSet::of(Zone::Exile).0),
                        dest: Zone::Battlefield,
                        hidden: false,
                        change_type: None,
                        num: Expr::Num(1),
                        tapped: true,
                        optional: false,
                        shuffle: false,
                        searcher: Defined::You,
                    },
                    Some(Defined::Triggered),
                );
                // "... under your control" (CR 701.66a): the controller of the delayed trigger,
                // i.e. of the earthbend (603.7e), not the land's owner.
                ret.text = "When that land dies or is put into exile, return it to the battlefield tapped under your control.".into();
                let ret = self.push_ability(ret);
                self.bf_under_you.push(ret);
                let filter = self.filter("Land.YouCtrl")?;
                self.pending_target = Some(TargetSpec { filter, kind: TargetKind::Normal, min: 1, max: 1, unique: false, max_expr: None, by_promise: None });
                Effect::Ext(ExtEffect::Earthbend { n, ret })
            }
            "NameCard" => {
                let valid = self.filter_f(f, p.get("ValidCards").unwrap_or("Card"))?;
                // Names are chosen from a names-only universe: only card-type / basic / legendary
                // restrictions can be decided from a name (`name_pool::filter_supported`).
                if !crate::name_pool::filter_supported(&valid) {
                    return Err("NameCard ValidCards beyond card types".into());
                }
                let chooser = self.defined(p.get("Defined").unwrap_or("You"))?;
                Effect::Ext(ExtEffect::NameCard { valid, chooser })
            }
            "ManaReflected" => {
                if p.get("Valid") != Some("Defined.ExiledWith") || p.get("ColorOrType") != Some("Color") || p.get("ReflectProperty") != Some("Is") {
                    return Err("ManaReflected form".into());
                }
                Effect::Ext(ExtEffect::ManaExiledColors)
            }
            "ImmediateTrigger" => {
                let exec = p.get("Execute").ok_or("ImmediateTrigger without Execute")?;
                // `TriggerAmount$ Count$RememberedNumber` only feeds `TriggerCount$`, which no
                // supported ability reads; anything else is unsupported.
                match p.get("TriggerAmount") {
                    None | Some("Count$RememberedNumber") => {}
                    Some(a) => return Err(format!("ImmediateTrigger TriggerAmount {a}")),
                }
                // `RememberObjects$ RememberedCard`: the reflexive ability refers to the objects
                // remembered so far ("that card", `DelayTriggerRememberedLKI`; "that many",
                // `TriggerRemembered$Valid <filter>` = how many of them match).
                // "That creature deals damage ..." (Thorin): the reflexive trigger remembers the
                // previous node's target as its trigger object.
                if let Some(d) = p.peek("RememberObjects").filter(|d| matches!(*d, "ParentTarget" | "Targeted")) {
                    // (`TriggerAmount$` was checked above.)
                    p.ack("RememberObjects", &["ParentTarget", "Targeted"], "checked above");
                    let sv = f.svar(exec).and_then(|s| s.as_ability()).ok_or_else(|| format!("ImmediateTrigger execute {exec} missing"))?;
                    let execute = self.ability_inner(&sv, f, _uns, _depth + 1, false)?;
                    return Ok(Some(Effect::Ext(ExtEffect::ReflexiveWith { execute, remember: self.defined(d)? })));
                }
                let remember = match p.get("RememberObjects") {
                    None => false,
                    Some("RememberedCard") | Some("Remembered") => true,
                    Some(r) => return Err(format!("ImmediateTrigger RememberObjects {r}")),
                };
                let mut count = None;
                if remember {
                    let mut fs: Vec<&str> = f.svars.iter().filter_map(|s| s.value.trim().strip_prefix("TriggerRemembered$Valid ")).collect();
                    fs.dedup();
                    match fs.as_slice() {
                        [] => {}
                        [one] => count = Some(self.filter_f(f, one.trim())?),
                        _ => return Err("several TriggerRemembered counts".into()),
                    }
                }
                let sv = f.svar(exec).and_then(|s| s.as_ability()).ok_or_else(|| format!("ImmediateTrigger execute {exec} missing"))?;
                let execute = self.ability_inner(&sv, f, _uns, _depth + 1, false)?;
                Effect::Ext(ExtEffect::Reflexive { execute, count, remember })
            }
            _ => return self.more_api(api, p, f),
        }))
    }

    /// `Repeat | MaxRepeat$ N | RepeatSubAbility$ C` where C is `GenericChoice | Defined$ Opponent |
    /// TempRemember$ Chooser` among "lose L life unless you sacrifice a [filter]" / "... unless you
    /// discard a card" with a plain "lose L life" fallback: N times, each opponent loses L life
    /// unless they sacrifice a [filter] or discard a card (Rottenmouth Viper). One three-way
    /// choice per repetition (Forge's two-step choice would reach "lose life" twice).
    fn punisher(&mut self, p: &P, f: &Face) -> Res<Effect> {
        let times = self.value(f, p.get("MaxRepeat").ok_or("Repeat without MaxRepeat")?)?;
        let name = p.get("RepeatSubAbility").ok_or("Repeat without RepeatSubAbility")?;
        let sv = f.svar(name).and_then(|s| s.as_ability()).ok_or_else(|| format!("RepeatSubAbility {name} missing"))?;
        let q = P::new(&sv);
        if q.get("DB") != Some("GenericChoice") || q.get("Defined") != Some("Opponent") || q.get("TempRemember") != Some("Chooser") {
            return Err("API Repeat".into());
        }
        let (mut life, mut sac, mut discard) = (None::<i32>, None, false);
        let lose = |db: &mut CardDb, n: &str, unless: bool| -> Res<(i32, Option<String>)> {
            let a = f.svar(n).and_then(|s| s.as_ability()).ok_or_else(|| format!("GenericChoice {n} missing"))?;
            let r = P::new(&a);
            let _ = db;
            if r.get("DB") != Some("LoseLife") || r.get("Defined") != Some("Remembered") {
                return Err("API Repeat".into());
            }
            let l = r.get("LifeAmount").and_then(|v| v.parse::<i32>().ok()).ok_or("API Repeat")?;
            let c = if unless {
                if r.get("UnlessPayer") != Some("Remembered") {
                    return Err("API Repeat".into());
                }
                Some(r.get("UnlessCost").ok_or("API Repeat")?.to_string())
            } else {
                None
            };
            if !r.unused().is_empty() {
                return Err("API Repeat".into());
            }
            Ok((l, c))
        };
        for c in q.get("Choices").ok_or("GenericChoice without Choices")?.split(',') {
            let (l, cost) = lose(self, c.trim(), true)?;
            if life.is_some_and(|x| x != l) {
                return Err("API Repeat".into());
            }
            life = Some(l);
            match cost.as_deref() {
                Some("Discard<1/Card>") => discard = true,
                Some(k) if k.starts_with("Sac<1/") && k.ends_with('>') => sac = Some(self.filter_f(f, k[6..k.len() - 1].split('/').next().unwrap())?),
                _ => return Err("API Repeat".into()),
            }
        }
        let (fl, _) = lose(self, q.get("FallbackAbility").ok_or("API Repeat")?, false)?;
        if Some(fl) != life || !q.unused().is_empty() {
            return Err("API Repeat".into());
        }
        Ok(Effect::Ext(ExtEffect::Punisher { times, life: life.unwrap(), sac, discard }))
    }

    /// `Token` extras: `TokenAttacking$` (CR 508.4) and `AtEOT$ Sacrifice` (delayed trigger).
    pub(crate) fn token_ext(&mut self, p: &P, extra: &mut Vec<AbilityDef>) -> Res<()> {
        // Any other value (a defined player or planeswalker) is unsupported.
        match p.peek("TokenAttacking") {
            None => {}
            Some(v) if v.eq_ignore_ascii_case("true") => {
                p.matched("TokenAttacking", |v| v.eq_ignore_ascii_case("true"), "checked above");
                extra.push(node(Effect::Ext(ExtEffect::MakeAttacking), None));
            }
            Some("TriggeredDefender") => {
                p.ack("TokenAttacking", &["TriggeredDefender"], "checked above");
                extra.push(node(Effect::Ext(ExtEffect::MakeAttackingTriggeredDefender), None));
            }
            Some(v) => return Err(format!("Token TokenAttacking$ {v}")),
        }
        match p.get("AtEOT") {
            None => {}
            Some("Sacrifice") => {
                // `Remembered`: the delayed trigger's objects, every token the node created
                // (`ExtEffect::AtNextEnd`, `trig_ext::fire_extra`).
                let mut sac = node(Effect::Ext(ExtEffect::SacrificeDefined), Some(Defined::Remembered));
                sac.text = "Sacrifice them at the beginning of the next end step.".into();
                let execute = self.push_ability(sac);
                extra.push(node(Effect::Ext(ExtEffect::AtNextEnd { execute }), None));
            }
            Some(x) => return Err(format!("Token AtEOT {x}")),
        }
        // "It gains haste until end of turn" (Stingerquill Charm's Cadet).
        if let Some(k) = p.get("PumpKeywords") {
            if p.get("PumpDuration") != Some("EOT") {
                return Err("Token PumpDuration".into());
            }
            let mut kws = KwSet::default();
            for w in k.split('&') {
                kws = kws.with(crate::compile_audit::granted_kw(w, "Token PumpKeywords")?);
            }
            extra.push(node(Effect::Ext(ExtEffect::PumpMoved { kws }), None));
        }
        Ok(())
    }

    /// `Effect | Triggers$ A,B`: an effect object with these triggered abilities.
    pub(crate) fn effect_triggers(&mut self, names: &str, f: &Face, uns: &mut Vec<String>, duration: EffDuration, extra: &mut Vec<AbilityDef>) -> Res<()> {
        let mut triggers = vec![];
        for name in names.split(',') {
            let sv = f.svar(name.trim()).ok_or_else(|| format!("Effect trigger {name} missing"))?;
            let tp = forge_script::Params::parse(&sv.value);
            let td = self.compile_trigger(&tp, f, uns)?.ok_or("Effect trigger")?;
            triggers.push(self.triggers_pool.len() as u32);
            self.triggers_pool.push(td);
        }
        extra.push(node(Effect::Ext(ExtEffect::TriggerEffect { triggers, duration }), None));
        Ok(())
    }

    /// Trigger modes handled in `trig_ext`.
    pub(crate) fn ext_trigger_mode(&self, m: &str, p: &P) -> Res<Option<crate::trig_ext::TrigExt>> {
        use crate::trig_ext::TrigExt as T;
        Ok(Some(match m {
            "AttackersDeclared" => T::AttackersDeclared { player: PlayerSel::parse(p.get("AttackingPlayer"))?, valid: p.get("ValidAttackers").map(|v| self.filter(v)).transpose()? },
            "ChangesZoneAll" => {
                let origin = ZoneSet::parse(p.get("Origin").unwrap_or("Any"))?;
                let dest = ZoneSet::parse(p.get("Destination").unwrap_or("Any"))?;
                // Matched by look-back before the objects move when leaving the battlefield;
                // otherwise after the batch has moved (`ext5`).
                if origin != ZoneSet::of(Zone::Battlefield) {
                    return Ok(Some(T::ZoneBatchAfter { origin, dest, valid: self.filter(p.get("ValidCards").unwrap_or("Card"))? }));
                }
                T::ZoneBatch { origin, dest, valid: self.filter(p.get("ValidCards").unwrap_or("Card"))? }
            }
            "Sacrificed" => {
                // Forge's TriggerSacrificed doesn't read Origin/Destination (The Sackville-Bagginses
                // has `Origin$ Any | Destination$ Battlefield` anyway): they don't restrict the
                // trigger, whatever their value.
                p.forge_ignores("Origin", "TriggerSacrificed");
                p.forge_ignores("Destination", "TriggerSacrificed");
                T::Sacrificed { valid: self.filter(p.get("ValidCard").unwrap_or("Card"))?, player: PlayerSel::parse(p.get("ValidPlayer"))? }
            }
            "CommitCrime" => T::CommitCrime { player: PlayerSel::parse(p.get("ValidPlayer"))? },
            "DamageDealtOnce" => {
                if p.peek("ValidTarget").is_some() || p.peek("CombatDamage").is_some() {
                    return Err("DamageDealtOnce with target/combat restrictions".into());
                }
                T::DamageDealtOnce { source: self.filter(p.get("ValidSource").unwrap_or("Card"))? }
            }
            "SearchedLibrary" => {
                if !p.flag("SearchOwnLibrary") {
                    return Err("SearchedLibrary of other libraries".into());
                }
                T::SearchedLibrary { player: PlayerSel::parse(p.get("ValidPlayer"))? }
            }
            "DamageAll" => {
                let player = match p.get("ValidTarget") {
                    Some("Opponent") | Some("Player.Opponent") => PlayerSel::Opponent,
                    Some("You") => PlayerSel::You,
                    Some("Player") => PlayerSel::Any,
                    v => return Err(format!("DamageAll ValidTarget {v:?}")),
                };
                let combat = p.bool_val("CombatDamage");
                T::PlayerDealtDamage { player, combat }
            }
            _ => return Ok(None),
        }))
    }

    /// `ChangeZone` parameters handled here: `Duration$` (exile until the source leaves),
    /// `ThisDefinedAndTgts$ Self`, and choosing from another player's hidden zone (`Chooser$`).
    /// Returns a replacement effect, or None to compile a plain `ChangeZone`.
    pub(crate) fn change_zone_ext(&mut self, p: &P, f: &Face, origin: ZoneSet, dest: Zone, hidden_origin: bool, extra: &mut Vec<AbilityDef>) -> Res<Option<Effect>> {
        if p.has("Duration") {
            self.duration_link(p, origin, extra)?;
        }
        if let Some(t) = p.get("ThisDefinedAndTgts") {
            // "Shuffle this and target creature into their owners' libraries": the plain
            // `ChangeZone` moves the source with its targets (`x9.this_and_targets`, compile.rs).
            if t != "Self" {
                return Err(format!("ThisDefinedAndTgts {t}"));
            }
        }
        // The hand was revealed by the previous node (RevealHand): nothing more to reveal.
        p.ack("AlreadyRevealed", &["True"], "revealed by an earlier node");
        let tgt_players = p.get("ValidTgts").is_some_and(|v| matches!(v.split('.').next(), Some("Player" | "Opponent")));
        if tgt_players && hidden_origin {
            // "Search target opponent's ...": the target is whose zones; it's a hidden choice.
            if p.peek("DefinedPlayer").is_some_and(|d| d != "Targeted") {
                return Err("ChangeZone DefinedPlayer with player targets".into());
            }
            p.ack("DefinedPlayer", &["Targeted"], "the target player's zone");
            p.ack("Hidden", &["True"], "a search of a hidden zone");
        }
        let chooser = p.get("Chooser");
        if hidden_origin && (tgt_players || chooser.is_some_and(|c| Some(c) != p.get("DefinedPlayer").or(Some("You")))) {
            let change_type = self.filter_f(f, p.get("ChangeType").unwrap_or("Card"))?;
            let num = match p.get("ChangeNum") {
                Some(v) => self.value(f, v)?,
                None => Expr::Num(1),
            };
            let owner = if tgt_players { Defined::Targeted } else { self.defined(p.get("DefinedPlayer").unwrap_or("You"))? };
            let chooser = self.defined(chooser.unwrap_or("You"))?;
            let up_to = !p.flag("Mandatory");
            return Ok(Some(Effect::Ext(ExtEffect::ChooseFromZone { origin, dest, change_type, num, owner, chooser, up_to })));
        }
        Ok(None)
    }

    /// `Duration$ UntilHostLeavesPlay`: link the moved objects to the source; they return to
    /// their previous zone when it leaves the battlefield (CR 610.3).
    pub(crate) fn duration_link(&mut self, p: &P, origin: ZoneSet, extra: &mut Vec<AbilityDef>) -> Res<()> {
        match p.get("Duration") {
            Some("UntilHostLeavesPlay") => {}
            d => return Err(format!("ChangeZone Duration {d:?}")),
        }
        let back = match p.get("Origin") {
            Some(o) if !o.contains(',') => zone(o)?,
            o => return Err(format!("Duration with origin {o:?}")),
        };
        let _ = origin;
        extra.push(node(Effect::Ext(ExtEffect::LinkMoved { until: Some(back) }), None));
        Ok(())
    }

    /// `ChangeZone | StaticEffect$ X` where X is `Mode$ Continuous | Affected$ Card.IsRemembered
    /// | AddType$ T | RemoveCardTypes$ True`: the moved object gets that type change.
    pub(crate) fn static_effect_on_moved(&mut self, f: &Face, name: &str, extra: &mut Vec<AbilityDef>) -> Res<()> {
        let sv = f.svar(name).ok_or_else(|| format!("StaticEffect {name} missing"))?;
        let sp = forge_script::Params::parse(&sv.value);
        let q = P::new(&sp);
        if q.get("Mode") != Some("Continuous") || q.get("Affected") != Some("Card.IsRemembered") {
            return Err("StaticEffect form".into());
        }
        let mut add_types = crate::basics::TypeSet::default();
        let mut add_subtypes = vec![];
        for w in q.get("AddType").unwrap_or("").split(&[',', '&'][..]).filter(|w| !w.is_empty()) {
            match crate::basics::CardType::parse(w.trim()) {
                Some(t) => add_types = add_types.with(t),
                None => add_subtypes.push(self.subtype(w.trim()).ok_or_else(|| format!("StaticEffect type {w}"))?),
            }
        }
        let remove_card_types = q.flag("RemoveCardTypes");
        let set_pt = match (q.get("SetPower"), q.get("SetToughness")) {
            (None, None) => None,
            (Some(a), Some(b)) => Some((a.parse::<i32>().map_err(|_| "StaticEffect SetPower")?, b.parse::<i32>().map_err(|_| "StaticEffect SetToughness")?)),
            _ => return Err("StaticEffect with only one of power/toughness".into()),
        };
        let unused = q.unused();
        if !unused.is_empty() {
            return Err(format!("StaticEffect param {}", unused.join(",")));
        }
        extra.push(node(Effect::Ext(ExtEffect::AnimateMoved { add_types, remove_card_types, add_subtypes, set_pt }), None));
        Ok(())
    }

    /// `Activation$` restrictions on activated abilities: `Solved` (Cases, CR 719.3c).
    pub(crate) fn activation_ext(&self, p: &P) -> Res<Option<crate::expr::Condition>> {
        if p.peek("PlayerTurn").is_some() {
            // "Activate only during your turn."
            return Ok(Some(crate::expr::Condition::YourTurn(p.flag("PlayerTurn"))));
        }
        match p.get("Activation") {
            None => Ok(None),
            Some("Solved") => Ok(Some(crate::expr::Condition::Compare(
                Expr::CountValid(crate::expr::ZoneSel::Battlefield, self.filter("Card.Self+IsSolved")?),
                crate::filter::CmpOp::GE,
                Expr::Num(1),
            ))),
            // Delirium (CR 207.2c ability word): "Activate only if there are four or more card
            // types among cards in your graveyard" (`ext8`).
            Some("Delirium") => Ok(Some(crate::ext8::delirium(self.filter("Card.YouOwn")?))),
            Some(a) => Err(format!("Activation {a}")),
        }
    }

    /// `Animate | Defined$ ValidGraveyard <filter> | staticAbilities$ Play` where Play is
    /// "you may play this card from your graveyard": the matching cards may be cast from the
    /// graveyard until end of turn (Case of the Uneaten Feast).
    pub(crate) fn graveyard_may_cast(&mut self, p: &P, f: &Face) -> Res<Effect> {
        let d = p.get("Defined").unwrap();
        let cards = self.filter_f(f, d.trim_start_matches("ValidGraveyard ").trim())?;
        let name = p.get("staticAbilities").unwrap();
        let sv = f.svar(name).ok_or_else(|| format!("static {name} missing"))?;
        let sp = forge_script::Params::parse(&sv.value);
        let q = P::new(&sp);
        if q.get("Mode") != Some("Continuous") || !q.flag("MayPlay") || q.get("AffectedZone") != Some("Graveyard") || q.get("EffectZone") != Some("Graveyard") {
            return Err("graveyard play grant form".into());
        }
        // "Card.Self+X" on the granted card = the remembered card with X.
        let aff = q.get("Affected").unwrap_or("Card.Self").replace("Self", "IsRemembered");
        let affected = self.filter_f(f, &aff)?;
        if !q.unused().is_empty() || p.has("Duration") {
            return Err("graveyard play grant params".into());
        }
        let si = self.statics_pool.len() as u32;
        self.statics_pool.push(StaticDef { kind: StaticKind::MayPlay { affected, zones: ZoneSet::of(Zone::Graveyard), any_type: false }, zones: ZoneSet::ANY, conditions: vec![], text: "You may cast this card from your graveyard.".into() });
        Ok(Effect::Ext(ExtEffect::GraveyardMayCast { cards, static_idx: si }))
    }

    /// Intern counter names mentioned in `counters_<cmp>_<NAME>` filters anywhere on the face,
    /// so filters compiled without mutable access (conditions) can resolve them.
    pub(crate) fn preintern_counters(&mut self, f: &Face) {
        let mut texts: Vec<&str> = f.svars.iter().map(|s| s.value.as_str()).collect();
        for ps in f.abilities.iter().chain(&f.triggers).chain(&f.statics).chain(&f.replacements) {
            texts.extend(ps.pairs.iter().map(|(_, v)| v.as_str()));
        }
        let mut names: Vec<String> = vec![];
        for t in texts {
            for (i, _) in t.match_indices("counters_") {
                let rest = &t[i + 9..];
                let tok: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                if let Some((_, name)) = tok.split_once('_') {
                    if !name.is_empty() && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
                        names.push(name.to_string());
                    }
                }
            }
        }
        for n in names {
            self.counter_kind(&n);
        }
    }

    /// Make sure the intrinsic basic land mana abilities exist (CR 305.6).
    pub(crate) fn ensure_basic_land_mana(&mut self) {
        if !self.basic_land_mana.is_empty() {
            return;
        }
        for mt in [crate::basics::ManaType::W, crate::basics::ManaType::U, crate::basics::ManaType::B, crate::basics::ManaType::R, crate::basics::ManaType::G] {
            let mut a = node(Effect::Mana { produced: Produced::One(mt), amount: 1 }, None);
            a.kind = AbilityKind::Activated;
            a.cost = Some(Cost { tap: true, ..Default::default() });
            a.is_mana = true;
            a.text = format!("{{T}}: Add {{{}}}.", mt.sym());
            let id = self.push_ability(a);
            self.basic_land_mana.push(id);
        }
    }

    /// Static modes handled in `static_ext` (None: not one of them; keys untouched).
    pub(crate) fn ext_static(&mut self, mode: &str, p: &P, f: &Face) -> Res<Option<StaticKind>> {
        use crate::static_ext::StaticExt as X;
        let valid = |db: &mut CardDb, k: &str| -> Res<crate::filter::Filter> { db.filter_f(f, p.get(k).unwrap_or("Card")) };
        Ok(Some(StaticKind::Ext(match mode {
            // "Creatures you control but don't own are Mercenaries in addition to their other
            // types" (`ext8`).
            "Continuous" if p.peek("Affected") == Some("Creature.YouCtrl+YouDontOwn") && p.peek("AddType").is_some_and(|t| !t.contains('&')) => {
                p.ack("Affected", &["Creature.YouCtrl+YouDontOwn"], "checked above");
                let t = p.get("AddType").unwrap();
                if crate::basics::CardType::parse(t).is_some() {
                    return Err(format!("static AddType {t} for stolen creatures"));
                }
                let st = self.subtype(t).ok_or_else(|| format!("static AddType {t}"))?;
                if !self.creature_types.contains(&st) {
                    return Err(format!("static AddType {t}: not a creature type"));
                }
                self.x8.stolen_subtype = true;
                X::StolenSubtype(st)
            }
            "Continuous" if p.peek("AddType") == Some("ChosenType") && p.peek("AffectedDefined") == Some("Self") => {
                p.ack("AddType", &["ChosenType"], "checked above");
                p.ack("AffectedDefined", &["Self"], "checked above");
                // `SelfChosenLandType` replaces land types (CR 305.7). "In addition to its other
                // types" (no `RemoveLandTypes$`) is the same only for a non-land (Adaptive
                // Automaton); a land keeps its other land types then (Thran Portal).
                if p.ack("RemoveLandTypes", &["True"], "the chosen type replaces land types").is_none() && f.types.as_deref().is_some_and(|t| t.split_whitespace().any(|w| w == "Land")) {
                    return Err("chosen land type in addition to other land types".into());
                }
                self.ensure_basic_land_mana();
                X::SelfChosenLandType
            }
            "Continuous" if p.peek("AdjustLandPlays").is_some() => {
                if p.get("Affected") != Some("You") {
                    return Err("AdjustLandPlays for others".into());
                }
                X::ExtraLandPlays(p.get("AdjustLandPlays").unwrap().parse().map_err(|_| "AdjustLandPlays")?)
            }
            "CantBlock" => X::CantBlock { valid: valid(self, "ValidCard")? },
            "CantBeActivated" => {
                let zone = p.get("AffectedZone");
                zone.filter(|z| *z != "Battlefield").map_or(Ok(()), |z| Err(format!("CantBeActivated zone {z}")))?;
                let nonmana_only = match p.get("ValidSA") {
                    Some("Activated") => false,
                    Some("Activated.!ManaAbility") => true,
                    v => return Err(format!("CantBeActivated ValidSA {v:?}")),
                };
                let mut valid = valid(self, "ValidCard")?;
                // "Activated abilities of artifacts, creatures, and planeswalkers" (Clarion
                // Conqueror): permanents only, not cards in hand or graveyard (CR 109.2).
                // Forge `StaticAbilityCantBeCast`: `AffectedZone$` limits the card's zone.
                if zone.is_some() {
                    for a in &mut valid.alts {
                        a.props.push(crate::filter::Prop::InZone(crate::state::Zone::Battlefield));
                    }
                }
                X::CantBeActivated { valid, nonmana_only }
            }

            "DisableTriggers" if p.peek("ValidTrigger").is_none() => {
                if p.get("ValidMode") != Some("ChangesZone,ChangesZoneAll") || p.get("Destination") != Some("Battlefield") {
                    return Err("DisableTriggers other than entering".into());
                }
                X::DisableEtbTriggers { cause: valid(self, "ValidCause")? }
            }
            "CantGainLife" => X::CantGainLife { players: PlayerSel::parse(p.get("ValidPlayer"))? },
            "Continuous" if p.peek("AddAbility").is_some() && p.peek("Affected").is_some() && p.peek("AffectedDefined").is_none() && ["AddPower", "AddToughness", "AddKeyword", "SetPower", "SetToughness", "AffectedZone", "MayPlay"].iter().all(|k| p.peek(k).is_none()) => {
                let name = p.get("AddAbility").unwrap();
                if name.contains('&') {
                    return Err("AddAbility with several abilities".into());
                }
                let affected = self.filter_f(f, p.get("Affected").unwrap())?;
                let sv = f.svar(name).and_then(|s| s.as_ability()).ok_or_else(|| format!("AddAbility {name} missing"))?;
                let mut uns = vec![];
                let ability = self.ability_inner(&sv, f, &mut uns, 1, false)?;
                if !uns.is_empty() {
                    return Err(uns.join("; "));
                }
                X::GrantAbility { affected, ability }
            }
            _ => match self.lt_static(mode, p, f)? {
                Some(l) => X::Lt(l),
                None => match self.more_static(mode, p, f)? {
                    Some(m) => X::More(m),
                    None => return Ok(None),
                },
            },
        })))
    }

    /// Replacement events other than `Moved` that are compiled as statics (pushed to
    /// `pending_statics`): "can't be countered" (CR 101.2) and damage increases (CR 614.1a).
    pub(crate) fn ext_replacement(&mut self, ev: &str, p: &P, f: &Face) -> Res<()> {
        use crate::static_ext::StaticExt as X;
        let spell_filter = |db: &mut CardDb, v: &str| -> Res<crate::filter::Filter> {
            let v = v.split(',').map(|alt| alt.strip_prefix("Spell.").map(|r| format!("Card.{r}")).unwrap_or_else(|| if alt == "Spell" { "Card".into() } else { alt.to_string() })).collect::<Vec<_>>().join(",");
            db.filter_f(f, &v)
        };
        let mr = self.mr_replacement(ev, p, f)?;
        let (kind, zones) = match ev {
            _ if mr.is_some() => {
                let (m, z) = mr.unwrap();
                (X::Mr(m), z)
            }
            "Counter" => {
                if p.get("Layer") != Some("CantHappen") {
                    return Err("replacement Counter".into());
                }
                let zones = match p.get("ActiveZones") {
                    None => ZoneSet::of(Zone::Stack),
                    Some(z) => ZoneSet::parse(z)?,
                };
                let valid = match (p.get("ValidCard"), p.get("ValidSA")) {
                    (Some("Card.Self"), Some("Spell")) if zones == ZoneSet::of(Zone::Stack) => self.filter("Card.Self")?,
                    (None, Some(sa)) => spell_filter(self, sa)?,
                    (c, sa) => return Err(format!("replacement Counter {c:?} {sa:?}")),
                };
                (X::CantBeCountered { valid }, zones)
            }
            "DamageDone" if p.peek("ValidTarget") == Some("Card.Self") => {
                // "If damage would be dealt to [this], instead that damage is dealt, but all other
                // damage already dealt to him is healed" (Wolverine, Fierce Fighter).
                let zones = ZoneSet::parse(p.get("ActiveZones").unwrap_or("Battlefield"))?;
                p.ack("ValidTarget", &["Card.Self"], "checked above");
                p.ack("ReplacementResult", &["Updated"], "the damage is still dealt");
                let with = p.get("ReplaceWith").ok_or("replacement without ReplaceWith")?;
                let sv = f.svar(with).and_then(|s| s.as_ability()).ok_or("ReplaceWith missing")?;
                if sv.api() != Some(("DB", "HealDamage")) || sv.get("Defined") != Some("ReplacedTarget") || sv.pairs.len() != 2 {
                    return Err("replacement DamageDone to self".into());
                }
                (X::HealBeforeDamage, zones)
            }
            "DamageDone" => {
                let zones = ZoneSet::parse(p.get("ActiveZones").unwrap_or("Battlefield"))?;
                if p.get("ValidTarget") != Some("Permanent,Player") {
                    return Err("replacement DamageDone target".into());
                }
                let source = self.filter_f(f, p.get("ValidSource").ok_or("DamageDone without ValidSource")?)?;
                let with = p.get("ReplaceWith").ok_or("replacement without ReplaceWith")?;
                let sv = f.svar(with).and_then(|s| s.as_ability()).ok_or("ReplaceWith missing")?;
                let plus = match (sv.api(), sv.get("VarName"), sv.get("VarValue").and_then(|v| f.svar(v)).map(|v| v.value.clone())) {
                    (Some(("DB", "ReplaceEffect")), Some("DamageAmount"), Some(v)) => v.strip_prefix("ReplaceCount$DamageAmount/Plus.").and_then(|n| n.parse::<i32>().ok()).ok_or("DamageDone amount")?,
                    _ => return Err("replacement DamageDone".into()),
                };
                (X::DamagePlus { source, plus }, zones)
            }
            "DrawCards" => {
                // "As long as [condition], if you would draw one or more cards, you draw that
                // many cards plus one instead" (Quantum Riddler).
                let zones = ZoneSet::parse(p.get("ActiveZones").unwrap_or("Battlefield"))?;
                if p.get("ValidPlayer") != Some("You") || zones != ZoneSet::of(Zone::Battlefield) {
                    return Err("replacement DrawCards form".into());
                }
                let with = p.get("ReplaceWith").ok_or("replacement without ReplaceWith")?;
                let sv = f.svar(with).and_then(|s| s.as_ability()).ok_or("ReplaceWith missing")?;
                let plus = match (sv.api(), sv.get("NumCards"), sv.pairs.len()) {
                    (Some(("DB", "Draw")), Some(v), 2) => v.strip_prefix("ReplaceCount$Number/Plus.").and_then(|n| n.parse::<i32>().ok()).ok_or("DrawCards amount")?,
                    _ => return Err("replacement DrawCards".into()),
                };
                (X::DrawPlus { plus }, zones)
            }
            "CreateToken" => {
                // "If one or more tokens would be created under your control, twice that many of
                // those tokens are created instead." (Elspeth, Storm Slayer; CR 614.1a)
                let zones = ZoneSet::parse(p.get("ActiveZones").unwrap_or("Battlefield"))?;
                let with = p.get("ReplaceWith").ok_or("replacement without ReplaceWith")?;
                let sv = f.svar(with).and_then(|s| s.as_ability()).ok_or("ReplaceWith missing")?;
                if p.get("ValidToken") != Some("Card.YouCtrl") || zones != ZoneSet::of(Zone::Battlefield) || sv.api() != Some(("DB", "ReplaceToken")) || sv.get("Type") != Some("Amount") || sv.pairs.len() != 2 {
                    return Err("replacement CreateToken form".into());
                }
                (X::Lt(crate::ext3::LtStatic::DoubleTokens), zones)
            }
            _ => return Err(format!("replacement {ev}")),
        };
        let conditions = self.conditions(p, f, "")?;
        let text = p.get("Description").unwrap_or("").to_string();
        let unused = p.unused();
        if !unused.is_empty() {
            return Err(format!("replacement {ev} param {}", unused.join(",")));
        }
        self.pending_statics.push(StaticDef { kind: StaticKind::Ext(kind), zones, conditions, text });
        Ok(())
    }
}

/// `IsRemembered` read by a node that nothing before it in its chain remembered for: Forge's
/// card-level memory, filled by the card's other abilities (Endless Horizons: the enters
/// ability's `ChangeZone | Destination$ Exile | RememberChanged$ True`, read by the upkeep
/// ability's `ChangeType$ Card.IsRemembered+YouOwn`). The engine has no card-level memory: when
/// every remembering node of the face is such an exile (no other `Remember*$` / `Imprint$`
/// anywhere) and `node` can't be reached from one of them (so its chain remembered nothing),
/// that memory is exactly the cards exiled with this object (CR 607.2a, 406.6; links are kept by
/// `effect_ext::link_exiled` and end with the object, 400.7): `ExiledWithSource`. Returns the
/// `ChangeType` to compile. Other shapes are returned unchanged.
pub(crate) fn card_memory_change_type(f: &Face, node: &forge_script::Params, origin_exile_only: bool, change_type: &str) -> String {
    if !origin_exile_only || !change_type.split([',', '+', '.', '!']).any(|t| t == "IsRemembered") {
        return change_type.to_string();
    }
    // Every ability-shaped line of the face, with its SVar name (abilities / triggers: none).
    let mut all: Vec<(Option<&str>, forge_script::Params)> = vec![];
    all.extend(f.abilities.iter().chain(&f.triggers).chain(&f.statics).chain(&f.replacements).map(|p| (None, p.clone())));
    all.extend(f.svars.iter().filter_map(|s| s.as_ability().map(|p| (Some(s.name.as_str()), p))));
    let remembers = |p: &forge_script::Params| p.keys().any(|k| k.starts_with("Remember") || k.starts_with("Imprint"));
    let exile_memory = |p: &forge_script::Params| {
        let api = p.api().map(|a| a.1);
        matches!(api, Some("ChangeZone") | Some("ChangeZoneAll"))
            && p.get("Destination") == Some("Exile")
            && p.pairs.iter().filter(|(k, _)| k.starts_with("Remember") || k.starts_with("Imprint")).all(|(k, v)| k == "RememberChanged" && v.eq_ignore_ascii_case("true"))
    };
    let mut any = false;
    for (_, p) in &all {
        if remembers(p) {
            if !exile_memory(p) {
                return change_type.to_string();
            }
            any = true;
        }
    }
    if !any {
        return change_type.to_string();
    }
    // SVars reachable (any parameter naming an SVar) from a remembering node.
    let mut reach: Vec<&str> = vec![];
    let mut todo: Vec<&forge_script::Params> = all.iter().filter(|(_, p)| remembers(p)).map(|(_, p)| p).collect();
    while let Some(p) = todo.pop() {
        for (_, v) in &p.pairs {
            if let Some((name, q)) = all.iter().find_map(|(n, q)| n.filter(|n| n == v).map(|n| (n, q))) {
                if !reach.contains(&name) {
                    reach.push(name);
                    todo.push(q);
                }
            }
        }
    }
    if all.iter().any(|(n, q)| n.is_some_and(|n| reach.contains(&n)) && q == node) {
        return change_type.to_string();
    }
    // Token-wise (`Card.IsRemembered+YouOwn`, `Creature.!IsRemembered`): only the whole property.
    let mut out = String::new();
    let mut tok = String::new();
    for ch in change_type.chars().chain(std::iter::once('\0')) {
        if matches!(ch, ',' | '+' | '.' | '!' | '\0') {
            out.push_str(if tok == "IsRemembered" { "ExiledWithSource" } else { &tok });
            tok.clear();
            if ch != '\0' {
                out.push(ch);
            }
        } else {
            tok.push(ch);
        }
    }
    out
}
