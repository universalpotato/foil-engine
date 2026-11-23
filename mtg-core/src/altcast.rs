//! Alternative and additional casting costs, and casting permissions (CR 118.8–9, 601.2b/f).
//!
//! Alternative methods (one per spell, CR 118.9a): warp (702.185), evoke (702.74), impending
//! (702.176), harmonize (702.180), plot (702.170, a special action plus a free cast later).
//! Additional costs (118.8): "[A] or [B]" costs, optional costs (blight 701.68), offspring
//! (702.175), teamwork (702.194), spree/tiered mode costs (702.172/702.183, 700.2h).
//! Cost reductions: affinity (702.41). Permissions/restrictions: can't-be-cast statics.
//!
//! How a spell was cast is recorded in `State::cast_info` (a [`CastInfo`] per object); the permanent
//! a spell becomes keeps the relevant part (CR 702.185a "the permanent this spell becomes"), so
//! linked abilities ("if its evoke cost was paid") and `CastSA>` expressions read it there.
//! Keyword-implied delayed triggers ("at the beginning of the next end step") are put on the
//! stack by [`end_step_begins`] from per-object flags, so they still happen if the permanent
//! loses its abilities (they are not abilities of the permanent).
//!
//! Every option offered is completable: castability and each announcement choice check that
//! the remaining mandatory costs stay payable ([`feasible`]).

use crate::basics::*;
use crate::carddb::*;
use crate::cast::{cost_reduction, mana_sources, mode_sets, payable, requirement, Req, Src};
use crate::engine::*;
use crate::expr::{EvalCtx, Expr};
use crate::filter::Filter;
use crate::query::*;
use crate::state::*;
use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

/// Bits of [`CastInfo::flags`].
pub mod flag {
    /// Cast for its warp cost (CR 702.185a).
    pub const WARP: u16 = 1 << 0;
    pub const EVOKE: u16 = 1 << 1;
    pub const IMPENDING: u16 = 1 << 2;
    pub const OFFSPRING: u16 = 1 << 3;
    /// An optional additional cost (`OptionalCost` static: blight, ...) was paid.
    pub const OPTCOST: u16 = 1 << 4;
    pub const TEAMWORK: u16 = 1 << 5;
    pub const HARMONIZE: u16 = 1 << 6;
    /// In exile: exiled by warp's delayed trigger (CR 702.185b); castable on a later turn.
    pub const WARPED: u16 = 1 << 7;
    /// In exile: plotted (CR 702.170a); castable for free on a later turn.
    pub const PLOTTED: u16 = 1 << 8;
    /// An offspring token (a marker the observation shows; no rules effect): its "except it's
    /// 1/1" is its copy effect's exception (`zones::CopyExc`, CR 707.9b), so copies
    /// of it inherit the 1/1 (not this marker).
    pub const OFFSPRING_TOKEN: u16 = 1 << 9;
    /// A mobilize token: sacrificed at the beginning of the next end step (CR 702.181a).
    pub const MOBILIZED: u16 = 1 << 10;
    /// A spell cast from a graveyard (flashback, harmonize, ...; Forge `wasCastFromGraveyard`).
    pub const FROM_GRAVE: u16 = 1 << 11;
    /// In exile: airbent (CR 701.65a); its owner may cast it for {2} while it stays exiled.
    pub const AIRBENT: u16 = 1 << 12;
    /// A spell cast from exile (`wasCastFromExile`); recorded only when a loaded card asks
    /// (`ext9::X9Tables::any_from_exile`).
    pub const FROM_EXILE: u16 = 1 << 13;
    /// Flags the permanent a spell becomes keeps.
    pub const PERMANENT: u16 = WARP | EVOKE | IMPENDING | OFFSPRING | OPTCOST | TEAMWORK;
}

/// How an object was cast, plus casting-related designations (`State::cast_info`).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CastInfo {
    pub flags: u16,
    /// Mana spent to cast it, 2 bits per color (W U B R G), saturating at 3 (for adamant).
    pub spent: u16,
}
impl CastInfo {
    pub fn has(self, f: u16) -> bool {
        self.flags & f != 0
    }
    /// What the permanent a resolving spell becomes keeps.
    pub fn on_battlefield(self) -> CastInfo {
        CastInfo { flags: self.flags & flag::PERMANENT, spent: self.spent }
    }
    pub fn spent_of(self, c: Color) -> u8 {
        ((self.spent >> (2 * c as u16)) & 3) as u8
    }
}

/// A way of casting a spell, or a special action that takes a card from a hidden zone.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum Method {
    Normal = 0,
    Flashback = 1,
    Warp = 2,
    Evoke = 3,
    Impending = 4,
    Harmonize = 5,
    /// Special action: exile from hand, plotted (CR 702.170a–b).
    Plot = 6,
    /// Cast a plotted card without paying its mana cost (CR 702.170d).
    Plotted = 7,
    /// Cast without paying its mana cost while an effect resolves (CR 118.9, 608.2g), e.g. a
    /// copy of a card (Uldaros Theorix). Never offered at priority.
    Free = 8,
    /// Sneak (CR 702.190a): during your declare blockers step, for the sneak cost plus returning
    /// an unblocked creature you control to its owner's hand; enters tapped and attacking.
    Sneak = 9,
    /// Special actions (CR 709.5e): pay the mana cost of a Room's locked left / right half to
    /// unlock it (`ext5::unlock_door`). Not casting.
    UnlockLeft = 10,
    UnlockRight = 11,
    /// Special action: turn a manifested permanent face up by paying its card's mana cost (CR
    /// 701.40b; `ext6::turn_face_up`). Not casting.
    TurnFaceUp = 12,
    /// Cast from the graveyard for its mayhem cost, the turn it was discarded (CR 702.187b).
    Mayhem = 13,
    /// Web-slinging (CR 702.188a): for the web-slinging cost plus returning a tapped creature you
    /// control to its owner's hand (`ext8`; `web_candidates`).
    WebSlinging = 14,
    /// Cast an airbent card from exile for {2} rather than its mana cost (CR 701.65a, 118.9;
    /// `ext9`).
    Airbend = 15,
    /// Flashback with the flashback ability an effect granted (Flashback, Stingcaster Mage: "The
    /// flashback cost is equal to its mana cost") to a card that has its own flashback with a
    /// different cost: a second instance, another alternative cost to choose (CR 113.2c,
    /// 702.34a, 118.9a; Flashback ruling 2026-03-20). A card with only the granted
    /// one is cast with `Flashback`.
    FlashbackGranted = 16,
    /// An airbent card cast as its other face (an Adventure, an Omen, a split card's or Room's
    /// second half) for {2}: an option code only, cast as face 1 by `Airbend` (CR 701.65a,
    /// 715.3d, 720.3, 709.3).
    AirbendFace = 17,
}
impl Method {
    pub fn from_u8(m: u8) -> Option<Method> {
        use Method::*;
        [Normal, Flashback, Warp, Evoke, Impending, Harmonize, Plot, Plotted, Free, Sneak, UnlockLeft, UnlockRight, TurnFaceUp, Mayhem, WebSlinging, Airbend, FlashbackGranted, AirbendFace].get(m as usize).copied()
    }
    pub fn name(self) -> &'static str {
        match self {
            Method::Normal => "normally",
            Method::Flashback => "with flashback",
            Method::Warp => "for its warp cost",
            Method::Evoke => "for its evoke cost",
            Method::Impending => "for its impending cost",
            Method::Harmonize => "with harmonize",
            Method::Plot => "plot",
            Method::Plotted => "plotted, without paying its mana cost",
            Method::Free => "without paying its mana cost",
            Method::Sneak => "for its sneak cost",
            Method::UnlockLeft => "unlock its left door",
            Method::UnlockRight => "unlock its right door",
            Method::TurnFaceUp => "turn it face up for its mana cost",
            Method::Mayhem => "for its mayhem cost",
            Method::WebSlinging => "for its web-slinging cost",
            Method::Airbend => "airbent, for {2}",
            Method::FlashbackGranted => "with its granted flashback (its mana cost)",
            Method::AirbendFace => "as its other face, airbent, for {2}",
        }
    }
    /// The face whose spell an `Opt::Alt(_, m)` casts, as `take_action` casts it (`AirbendFace`:
    /// face 1, CR 715.3d / 709.3; every other casting method: the card's normal face 0, CR
    /// 118.9 / 601.2b: an alternative cost changes what is paid, not the spell), or None for the
    /// special actions that cast nothing (plot, CR 702.170b; unlocking a door, 709.5e; turning a
    /// permanent face up, 701.40b / 116.2b). Exhaustive on purpose: a new method doesn't compile
    /// until it says which spell it casts (`observe::option_targets`).
    pub fn cast_face(self) -> Option<u8> {
        match self {
            Method::Normal | Method::Flashback | Method::Warp | Method::Evoke | Method::Impending | Method::Harmonize | Method::Plotted | Method::Free | Method::Sneak | Method::Mayhem | Method::WebSlinging | Method::Airbend | Method::FlashbackGranted => Some(0),
            Method::AirbendFace => Some(1),
            Method::Plot | Method::UnlockLeft | Method::UnlockRight | Method::TurnFaceUp => None,
        }
    }
}

/// An additional cost component.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AddCost {
    Mana(ManaCost),
    Life(i32),
    Discard(u8, Filter),
    Sac(u8, Filter),
    /// Put N -1/-1 counters on a creature you control (CR 701.68a).
    Blight(u8),
    /// Reveal a matching card from hand or choose a matching permanent you control (CR 701.4a).
    Behold(Filter),
    /// Exile cards with total mana value N or greater from your graveyard (CR 701.59a).
    CollectEvidence(u8),
    /// Exile N matching cards from your graveyard (Abhorrent Oculus; `ext6`).
    ExileFromGrave(u8, Filter),
}
impl AddCost {
    pub fn describe(&self) -> String {
        match self {
            AddCost::Mana(m) => format!("pay {m}"),
            AddCost::Life(n) => format!("pay {n} life"),
            AddCost::Discard(n, _) => format!("discard {n} card(s)"),
            AddCost::Sac(n, _) => format!("sacrifice {n} permanent(s)"),
            AddCost::Blight(n) => format!("blight {n}"),
            AddCost::Behold(_) => "behold".into(),
            AddCost::CollectEvidence(n) => format!("collect evidence {n}"),
            AddCost::ExileFromGrave(n, _) => format!("exile {n} card(s) from your graveyard"),
        }
    }
}

/// Alternative/additional-cost abilities of a card face.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AltDef {
    pub warp: Option<ManaCost>,
    pub evoke: Option<ManaCost>,
    /// (time counters, cost, the TIME counter kind)
    pub impending: Option<(i32, ManaCost, CounterKind)>,
    pub harmonize: Option<ManaCost>,
    pub plot: Option<ManaCost>,
    pub offspring: Option<ManaCost>,
    /// Affinity for permanents you control matching this filter.
    pub affinity: Option<Filter>,
    /// "As an additional cost to cast this spell, [A] or [B]."
    pub either: Option<(AddCost, AddCost)>,
    /// Teamwork N.
    pub teamwork: Option<i32>,
    /// Index in the face's triggers of warp's delayed exile trigger.
    pub warp_trigger: Option<u8>,
    /// On the Warrior token face: mobilize's shared "sacrifice them at the beginning of the next
    /// end step" ability (`AltEffect::SacrificeRemembered`), compiled once.
    pub mobilize_sac: Option<AbilityId>,
    /// Spree/tiered: modes carry additional costs (`CardDb::mode_costs`).
    pub mode_costs: bool,
    /// "As an additional cost to cast this spell, sacrifice ..." (mandatory, CR 601.2b; Forge
    /// `SP$ .. | Cost$ <mana> Sac<1/..>`).
    #[serde(default)]
    pub extra_cost: Option<AddCost>,
    /// Sneak cost (CR 702.190a).
    #[serde(default)]
    pub sneak: Option<ManaCost>,
    /// Mayhem cost (CR 702.187b): castable from the graveyard the turn it was discarded.
    #[serde(default)]
    pub mayhem: Option<ManaCost>,
    /// Web-slinging cost (CR 702.188a).
    #[serde(default)]
    pub web_slinging: Option<ManaCost>,
}

/// Casting-related static abilities.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AltStatic {
    /// "As an additional cost to cast this spell, you may [cost]." (functions on the stack)
    OptionalCost(AddCost),
    /// Matching spells can't be cast by `caster` (only during the controller's turn if set).
    CantBeCast { valid: Filter, caster: PlayerSel, your_turn_only: bool },
}

/// Effects implied by the keywords.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AltEffect {
    /// Exile the warped permanent; it may be cast on a later turn (CR 702.185a).
    WarpExile,
    /// Its controller sacrifices it (evoke, mobilize tokens).
    SacrificeSelf,
    /// Remove a time counter (impending, CR 702.176a).
    ImpendingTick(CounterKind),
    /// Create a 1/1 token copy of it (offspring, CR 702.175a).
    OffspringCopy,
    /// Create N tapped and attacking tokens and one delayed trigger (`sac`, a
    /// `SacrificeRemembered` ability) that sacrifices them at the next end step (CR 702.181a,
    /// 603.7a, 603.7e: its source is the mobilize creature).
    Mobilize { token: CardId, n: u8, sac: AbilityId },
    /// Its controller sacrifices the delayed trigger's remembered objects it still controls, in
    /// one event (mobilize's "Sacrifice them", CR 702.181a, 701.21a).
    SacrificeRemembered,
}

/// Expressions about how the source was cast.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AltExpr {
    /// A if the flag is set, else B (`Count$OptionalGenericCostPaid.A.B`, `Count$Teamwork.A.B`).
    CastFlag(u16, i32, i32),
    /// A if at least N mana of the color was spent to cast it, else B (`Count$Adamant_N.Color.A.B`).
    Adamant(Color, u8, i32, i32),
    /// Number of counters of a kind on the source.
    SelfCounters(CounterKind),
}

/// `Count$...` forms handled here.
pub fn count_expr(body: &str) -> Result<Option<Expr>, String> {
    let ab = |rest: &str| -> Result<(i32, i32), String> {
        let mut it = rest.split('.');
        let a = it.next().and_then(|x| x.parse().ok()).ok_or_else(|| format!("expr Count${body}"))?;
        let b = it.next().and_then(|x| x.parse().ok()).ok_or_else(|| format!("expr Count${body}"))?;
        Ok((a, b))
    };
    if let Some(rest) = body.strip_prefix("OptionalGenericCostPaid.") {
        let (a, b) = ab(rest)?;
        return Ok(Some(Expr::Alt(AltExpr::CastFlag(flag::OPTCOST, a, b))));
    }
    // CR 702.174k: "if the gift was promised" (gift is an optional additional cost, `ext7`).
    if let Some(rest) = body.strip_prefix("PromisedGift.") {
        let (a, b) = ab(rest)?;
        return Ok(Some(Expr::Alt(AltExpr::CastFlag(flag::OPTCOST, a, b))));
    }
    // CR 702.166b: "if this spell was bargained" (bargain is an optional additional cost).
    if let Some(rest) = body.strip_prefix("Bargain.") {
        let (a, b) = ab(rest)?;
        return Ok(Some(Expr::Alt(AltExpr::CastFlag(flag::OPTCOST, a, b))));
    }
    if let Some(rest) = body.strip_prefix("wasCastFromGraveyard.") {
        let (a, b) = ab(rest)?;
        return Ok(Some(Expr::Alt(AltExpr::CastFlag(flag::FROM_GRAVE, a, b))));
    }
    if let Some(rest) = body.strip_prefix("Teamwork.") {
        let (a, b) = ab(rest)?;
        return Ok(Some(Expr::Alt(AltExpr::CastFlag(flag::TEAMWORK, a, b))));
    }
    if let Some(rest) = body.strip_prefix("Adamant_") {
        // `Adamant_N.Color.A.B`
        let mut it = rest.splitn(3, '.');
        let n: u8 = it.next().and_then(|x| x.parse().ok()).ok_or_else(|| format!("expr Count${body}"))?;
        let c = it.next().and_then(Color::from_word).ok_or_else(|| format!("expr Count${body}"))?;
        let (a, b) = ab(it.next().unwrap_or(""))?;
        if n > 3 {
            return Err(format!("expr Count${body}"));
        }
        return Ok(Some(Expr::Alt(AltExpr::Adamant(c, n, a, b))));
    }
    Ok(None)
}

pub fn eval_expr(e: &AltExpr, c: &EvalCtx) -> i32 {
    let ci = c.s.cast_of(c.source);
    match *e {
        AltExpr::CastFlag(f, a, b) => {
            if ci.has(f) {
                a
            } else {
                b
            }
        }
        AltExpr::Adamant(col, n, a, b) => {
            if ci.spent_of(col) >= n {
                a
            } else {
                b
            }
        }
        AltExpr::SelfCounters(k) => c.s.obj(c.source).counter(k),
    }
}

/// Record mana of type `t` spent on the spell `subject` (if it is being cast).
pub fn note_spent(s: &mut State, subject: ObjId, t: ManaType, n: u8) {
    if s.obj(subject).zone != Zone::Stack || n == 0 {
        return;
    }
    if let Some(c) = t.color() {
        let ci = s.cast_mut(subject);
        let sh = 2 * c as u16;
        let cur = (ci.spent >> sh) & 3;
        let new = (cur + n as u16).min(3);
        ci.spent = (ci.spent & !(3 << sh)) | (new << sh);
    }
}

// ---------------------------------------------------------------- characteristics

/// Layer 4 (impending: not a creature while it has a time counter, CR 702.176a) adjustment of
/// printed values. (An offspring token's 1/1 is its copy effect's exception, `zones::CopyExc`.)
pub fn adjust_chars(s: &State, id: ObjId, f: &FaceDef, c: &mut Chars) {
    if s.cast_info.is_empty() {
        return;
    }
    let ci = s.cast_of(id);
    let o = s.obj(id);
    if o.zone == Zone::Battlefield && ci.has(flag::IMPENDING) {
        if let Some((_, _, k)) = &f.alt.impending {
            if o.counter(*k) > 0 {
                c.types = TypeSet(c.types.0 & !TypeSet::default().with(CardType::Creature).0);
            }
        }
    }
}

/// "Enters with" parts of the keywords (impending's time counters, CR 702.176a).
pub fn as_enters(s: &mut State, db: &CardDb, nid: ObjId) {
    if s.cast_of(nid).has(flag::IMPENDING) {
        if let Some((n, _, k)) = db.get(s.obj(nid).card).front().alt.impending.clone() {
            crate::ext6::put_counters(s, db, nid, k, n);
        }
    }
}

// ---------------------------------------------------------------- castability

/// The mana cost paid under a method (alternative costs replace the mana cost, CR 118.9),
/// or None if the face doesn't have that method.
pub fn base_cost(f: &FaceDef, m: Method) -> Option<ManaCost> {
    match m {
        Method::Normal => Some(f.cost.clone()),
        Method::Flashback => f.flashback.clone(),
        Method::Warp => f.alt.warp.clone(),
        Method::Evoke => f.alt.evoke.clone(),
        Method::Impending => f.alt.impending.as_ref().map(|i| i.1.clone()),
        Method::Harmonize => f.alt.harmonize.clone(),
        Method::Plotted | Method::Free => Some(ManaCost::default()),
        Method::Plot | Method::UnlockLeft | Method::UnlockRight | Method::TurnFaceUp => None,
        Method::Sneak => f.alt.sneak.clone(),
        Method::Mayhem => f.alt.mayhem.clone(),
        Method::WebSlinging => f.alt.web_slinging.clone(),
        Method::Airbend | Method::AirbendFace => Some(ManaCost { syms: smallvec::smallvec![ManaSym::Generic(2)], none: false }),
        // The granted instance's cost depends on the object (`base_cost_of`).
        Method::FlashbackGranted => None,
    }
}

/// `base_cost` for the object `o`: a card that was given flashback by an effect (Flashback, the
/// card) has flashback with a cost equal to its mana cost. `Flashback` is the card's first
/// instance (its printed one, else the granted one); `FlashbackGranted` the granted one.
pub fn base_cost_of(s: &State, o: ObjId, f: &FaceDef, m: Method) -> Option<ManaCost> {
    let granted = || !s.ext.granted_flashback.is_empty() && s.ext.granted_flashback.contains(&o);
    match m {
        Method::FlashbackGranted => granted().then(|| f.cost.clone()),
        _ => base_cost(f, m).or_else(|| (m == Method::Flashback && granted()).then(|| f.cost.clone())),
    }
}

/// Does graveyard card `o` (front face `f`) have a second flashback instance, a granted one whose
/// cost differs from its printed flashback cost (`Method::FlashbackGranted`)? One with the same
/// cost is the same choice (`#flashback:<n>`).
pub fn has_second_flashback(s: &State, o: ObjId, f: &FaceDef) -> bool {
    f.flashback.as_ref().is_some_and(|c| *c != f.cost) && !s.ext.granted_flashback.is_empty() && s.ext.granted_flashback.contains(&o)
}

pub fn add_req(a: &mut Req, b: &Req) {
    a.any_type |= b.any_type;
    for (i, n) in b.exact.iter().enumerate() {
        a.exact[i] += n;
    }
    a.hybrid.extend(b.hybrid.iter().copied());
    a.generic += b.generic;
}

fn reduced(mut r: Req, by: u32) -> Req {
    r.generic = r.generic.saturating_sub(by);
    r.settle_any_type(); // CR 609.4b, after reductions (`Req::any_type`)
    r
}

/// Additional mana of the chosen modes (spree/tiered, CR 700.2h).
pub fn mode_cost(db: &CardDb, a: AbilityId, modes: &[u8]) -> Req {
    let mut r = Req::default();
    if db.mode_costs.is_empty() {
        return r;
    }
    for n in crate::resolve::nodes(db, a, &[]) {
        if let Effect::Charm { modes: ms, .. } = &db.ability(n).effect {
            for &m in modes {
                if let Some((_, c)) = db.mode_costs.iter().find(|(id, _)| *id == ms[m as usize]) {
                    add_req(&mut r, &requirement(c, 0));
                }
            }
        }
    }
    r
}

/// Affinity (CR 702.41a): {1} less for each matching permanent the caster controls (`spell`
/// cast as face `face`).
pub fn affinity_reduction(cx: &Ctx, p: PlayerId, spell: ObjId, face: u8) -> u32 {
    let o = cx.s.obj(spell);
    let Some(f) = &cx.db.get(o.card).face(face).alt.affinity else { return 0 };
    let fcx = FilterCtx { you: p, source: Some(spell), x: 0, rem: &[] };
    cx.s.battlefield.iter().filter(|&&b| cx.s.obj(b).controller == p && obj_matches(cx.s, cx.db, f, b, fcx)).count() as u32
}

/// Can't-be-cast statics (e.g. "Your opponents can't cast spells during your turn"), for `h`
/// cast as face `face`: only the characteristics it will have as that spell are evaluated
/// (CR 601.3e, 715.3a, 720.3a, 709.3a; `SpellView`).
pub fn cant_cast(cx: &Ctx, p: PlayerId, h: ObjId, face: u8) -> bool {
    let s = &*cx.s;
    let mut view = SpellView::new(s, cx.db, h, face, p);
    for &src in &s.battlefield {
        let so = s.obj(src);
        for st in &cx.db.get(so.card).face(so.face).statics {
            let StaticKind::Alt(AltStatic::CantBeCast { valid, caster, your_turn_only }) = &st.kind else { continue };
            if !st.zones.has(Zone::Battlefield) || (*your_turn_only && s.turn.active != so.controller) || !caster.matches(p, so.controller) {
                continue;
            }
            if crate::query::may_lose_abilities(s, cx.db) && lost_all_abilities(s, cx.db, src) {
                continue;
            }
            if view.matches(valid, so.controller, Some(src)) {
                return true;
            }
        }
    }
    // Effects of resolved spells ("players can't cast spells this turn", Bilbo's Gambit; `ext8`).
    for eo in &s.effect_objs {
        for &si in &eo.statics {
            // effect-object static reader: CantBeCast (`static_ext::applies_from_effect_object`)
            let st = &cx.db.statics_pool[si as usize];
            let StaticKind::Alt(AltStatic::CantBeCast { valid, caster, your_turn_only }) = &st.kind else { continue };
            if (*your_turn_only && s.turn.active != eo.controller) || !caster.matches(p, eo.controller) {
                continue;
            }
            if view.matches(valid, eo.controller, Some(eo.source)) {
                return true;
            }
        }
    }
    crate::ext9::cant_cast(cx, p, h)
}

/// The spell an object will be as it is cast as face `face`, for effects that look at the spell
/// while it is being cast (cost reductions, "can't cast" prohibitions). CR 601.2a: the card is
/// moved to the stack first, and its total cost is determined there (601.2f); 601.3e: an
/// alternative set of characteristics replaces the object's for the legality check (Adventure
/// 715.3a-b, Omen 720.3a-b, split card / Room door 709.3a-b, MDFC face 712.11c). Where the
/// object's characteristics already are the spell's (its normal face in a hand, graveyard or
/// exile, not a Room: a split card outside the stack has both halves', 709.4), filters are
/// matched in place. Otherwise against a scratch state with the card moved to the
/// stack as that face (a prepared permanent's spell is a copy of it, CR 722.3c: the permanent
/// stays), built on first use: most positions have no spell-filtered static at all.
pub struct SpellView<'a> {
    s: &'a State,
    db: &'a CardDb,
    spell: ObjId,
    face: u8,
    /// The player casting it: the spell's controller (CR 601.2a, 112.2).
    caster: PlayerId,
    scratch: Option<(State, ObjId)>,
}

impl<'a> SpellView<'a> {
    pub fn new(s: &'a State, db: &'a CardDb, spell: ObjId, face: u8, caster: PlayerId) -> Self {
        SpellView { s, db, spell, face, caster, scratch: None }
    }

    /// The object's characteristics where it is now aren't the spell's (or its controller
    /// isn't: a card cast from another player's hand, library or exile, CR 601.2a).
    pub fn differs(&self) -> bool {
        let o = self.s.obj(self.spell);
        o.zone != Zone::Stack && (o.face != self.face || o.zone == Zone::Battlefield || o.controller != self.caster || matches!(self.db.get(o.card).layout, Layout::Room | Layout::Split))
    }

    /// Does the spell match `f`, evaluated for `you` from `source` (None: the spell itself, e.g.
    /// its own "this spell costs {1} less")?
    pub fn matches(&mut self, f: &Filter, you: PlayerId, source: Option<ObjId>) -> bool {
        if !self.differs() {
            return obj_matches(self.s, self.db, f, self.spell, FilterCtx { you, source: Some(source.unwrap_or(self.spell)), x: 0, rem: &[] });
        }
        let (s, spell, face, caster) = (self.s, self.spell, self.face, self.caster);
        let (s2, so) = self.scratch.get_or_insert_with(|| spell_on_stack(s, spell, face, caster));
        obj_matches(s2, self.db, f, *so, FilterCtx { you, source: Some(source.unwrap_or(*so)), x: 0, rem: &[] })
    }
}

/// A scratch copy of `s` with `spell` put on the stack by `caster` as face `face` (CR 601.2a)
/// and the id it has there. A permanent (a prepared one) stays where it is: the spell is a copy
/// of it in exile (CR 722.3c), as `cast::cast_prepared` makes it.
pub fn spell_on_stack(s: &State, spell: ObjId, face: u8, caster: PlayerId) -> (State, ObjId) {
    let mut s2 = s.clone();
    let src = if s2.obj(spell).zone == Zone::Battlefield {
        let (card, ctrl) = (s2.obj(spell).card, s2.obj(spell).controller);
        let c = s2.new_token(card, ctrl);
        s2.obj_mut(c).zone = Zone::Exile;
        c
    } else {
        spell
    };
    let so = put_on_stack_as_spell(&mut s2, src, face, caster);
    (s2, so)
}

/// CR 601.2a: move the card `h` to the stack as face `face`; the player casting it becomes the
/// spell's controller (112.2), also when it is another player's card (cast from an opponent's
/// library or exile: Outrageous Robbery, Null Summoner, Cruelclaw's Heist). Every
/// "you" of the spell (its cost reductions, cast triggers, "its controller") reads this. Once it
/// leaves the stack the new object is its owner's again (`State::move_obj_limbo`, 108.4a), or
/// the caster's permanent (`resolve`, 110.2a).
pub fn put_on_stack_as_spell(s: &mut State, h: ObjId, face: u8, caster: PlayerId) -> ObjId {
    let so = s.move_obj(h, Zone::Stack, false);
    let o = s.obj_mut(so);
    o.face = face;
    o.controller = caster;
    so
}

/// Mana sources, minus `excl`. `spell`: the spell being paid for, so mana with a spending
/// restriction (CR 106.6, Cavern of Souls) counts when it may pay for it.
fn sources_without(cx: &Ctx, p: PlayerId, excl: &[ObjId], spell: Option<ObjId>) -> Vec<Src> {
    crate::cast::mana_sources_excluding(cx, p, excl, spell)
}

/// Objects a cost may choose: your permanents (`bf`) or cards in your hand, matching `f`.
fn candidates(cx: &Ctx, p: PlayerId, src: ObjId, f: Option<&Filter>, zone: Zone, creature_only: bool) -> Vec<ObjId> {
    let fcx = FilterCtx { you: p, source: Some(src), x: 0, rem: &[] };
    let pool: Vec<ObjId> = match zone {
        Zone::Battlefield => cx.s.battlefield.iter().copied().filter(|&o| cx.s.obj(o).controller == p).collect(),
        Zone::Hand => cx.s.player(p).hand.iter().copied().filter(|&o| crate::query::is_card(cx.s, o)).collect(),
        Zone::Graveyard => cx.s.player(p).graveyard.iter().copied().filter(|&o| crate::query::is_card(cx.s, o)).collect(),
        _ => vec![],
    };
    pool.into_iter()
        .filter(|&o| o != src && (!creature_only || chars(cx.s, cx.db, o).types.has(CardType::Creature)) && f.is_none_or(|f| obj_matches(cx.s, cx.db, f, o, fcx)))
        .collect()
}

/// Can the non-mana part of an additional cost be paid?
pub fn nonmana_payable(cx: &Ctx, p: PlayerId, src: ObjId, c: &AddCost) -> bool {
    match c {
        AddCost::Mana(_) => true,
        AddCost::Life(n) => cx.s.player(p).life >= *n, // CR 119.4
        AddCost::Discard(n, f) => candidates(cx, p, src, Some(f), Zone::Hand, false).len() >= *n as usize,
        AddCost::Sac(n, f) => candidates(cx, p, src, Some(f), Zone::Battlefield, false).len() >= *n as usize,
        AddCost::Blight(_) => !candidates(cx, p, src, None, Zone::Battlefield, true).is_empty(), // CR 701.68b
        AddCost::Behold(f) => !candidates(cx, p, src, Some(f), Zone::Battlefield, false).is_empty() || !candidates(cx, p, src, Some(f), Zone::Hand, false).is_empty(),
        // CR 701.59b: can't choose to collect evidence without enough mana value.
        AddCost::CollectEvidence(n) => crate::ext2::graveyard_mv(cx, p) >= *n as i32,
        AddCost::ExileFromGrave(n, f) => candidates(cx, p, src, Some(f), Zone::Graveyard, false).len() >= *n as usize,
    }
}

fn mana_part(c: &AddCost) -> Req {
    match c {
        AddCost::Mana(m) => requirement(m, 0),
        _ => Req::default(),
    }
}

/// Untapped creatures you control that could be tapped for harmonize/teamwork, with power.
fn tappable_creatures(cx: &Ctx, p: PlayerId, excl: &[ObjId]) -> Vec<(ObjId, i32)> {
    cx.s.battlefield
        .iter()
        .copied()
        .filter(|&o| cx.s.obj(o).controller == p && !cx.s.obj(o).tapped && !excl.contains(&o))
        .filter_map(|o| {
            let c = chars(cx.s, cx.db, o);
            c.types.has(CardType::Creature).then_some((o, c.power))
        })
        .collect()
}

/// Is a cast with mana requirement `req` (before generic `reduction`) completable, given
/// still-unannounced mandatory choices: a harmonize creature (`harmonize`), an "A or B"
/// additional cost (`either`)? Optional costs can always be declined and X can be 0.
#[allow(clippy::too_many_arguments)]
pub fn feasible(cx: &Ctx, p: PlayerId, src: ObjId, f: &FaceDef, req: &Req, reduction: u32, excluded: &[ObjId], harmonize: bool, either: bool) -> bool {
    // Restrictions are checked on the object as it is; when casting another face of it
    // (Adventure, from hand) restricted mana is conservatively not counted.
    let so = cx.s.obj(src);
    let spell = std::ptr::eq(f, cx.db.get(so.card).face(so.face)).then_some(src);
    let mut eithers: SmallVec<[Option<&AddCost>; 2]> = SmallVec::new();
    match (&f.alt.either, &f.alt.extra_cost, either) {
        (Some((a, b)), _, true) => {
            for c in [a, b] {
                if nonmana_payable(cx, p, src, c) {
                    eithers.push(Some(c));
                }
            }
        }
        // A mandatory additional cost (never together with "A or B", see compile).
        (None, Some(c), true) => {
            if nonmana_payable(cx, p, src, c) {
                eithers.push(Some(c));
            }
        }
        _ => eithers.push(None),
    }
    for e in eithers {
        let mut r = req.clone();
        if let Some(c) = e {
            add_req(&mut r, &mana_part(c));
            if needs_battlefield_pick(c) {
                // The permanent paid can't also be a mana source that sacrifices itself.
                if battlefield_picks(cx, p, src, c, &reduced(r.clone(), reduction), excluded, &[]).is_empty() {
                    continue;
                }
                return true;
            }
        }
        if payable(&reduced(r.clone(), reduction), &sources_without(cx, p, excluded, spell)) {
            return true;
        }
        // Another face (an Omen, an Adventure) cast from hand: mana with a spending restriction
        // counts when it may pay for the spell as cast, i.e. with only that face's
        // characteristics on the stack (CR 601.2a, 106.6, 720.3a-b, 715.3a-b:
        // Maelstrom of the Spirit Dragon's mana pays for an Omen spell). Checked on a scratch
        // copy (as `SpellView`) only when the unrestricted sources fall short.
        if spell.is_none() && !cx.db.mana_restrict.is_empty() && may_have_restricted_mana(cx, p) {
            if let Some(face) = cx.db.get(so.card).faces.iter().position(|x| std::ptr::eq(x, f)) {
                let (mut s2, so2) = spell_on_stack(cx.s, src, face as u8, p);
                let cx2 = Ctx::scratch(&mut s2, cx.db);
                if payable(&reduced(r.clone(), reduction), &sources_without(&cx2, p, excluded, Some(so2))) {
                    return true;
                }
            }
        }
        if harmonize {
            for (c, pw) in tappable_creatures(cx, p, excluded) {
                let mut ex: SmallVec<[ObjId; 4]> = excluded.iter().copied().collect();
                ex.push(c);
                if payable(&reduced(r.clone(), reduction.saturating_add(pw.max(0) as u32)), &sources_without(cx, p, &ex, spell)) {
                    return true;
                }
            }
        }
    }
    false
}

/// Could `p` have mana with a spending restriction (an untapped permanent with a restricted mana
/// ability, abilities as it has them, or restricted mana in the pool)? A cheap pre-check for
/// `feasible`'s scratch copy.
fn may_have_restricted_mana(cx: &Ctx, p: PlayerId) -> bool {
    let s = &*cx.s;
    s.ext.x6.rpool.iter().any(|x| x.0 == p)
        || s.battlefield.iter().any(|&o| {
            let ob = s.obj(o);
            ob.controller == p && !ob.tapped && chars(s, cx.db, o).abilities.iter().any(|a| cx.db.mana_restrict.iter().any(|r| r.0 == *a))
        })
}

/// Additional costs paid with one permanent you control that is chosen before mana is paid.
fn needs_battlefield_pick(c: &AddCost) -> bool {
    matches!(c, AddCost::Sac(1, _) | AddCost::Blight(_))
}

/// Permanents that can pay `c` (sacrifice / blight) while `req` stays payable with them and the
/// permanents `picked` for earlier costs reserved (`cast::Reserve::picked`, the same rule the
/// payment follows): a permanent whose mana ability sacrifices it (Treasure) can't both produce
/// mana that way and be the one paid with, but it may still be tapped, e.g. for Gene
/// Pollinator's "tap an untapped permanent" (CR 601.2g before 601.2h). Distinct options only
/// (identical permanents are interchangeable); one permanent pays one cost (CR 118.3).
fn battlefield_picks(cx: &Ctx, p: PlayerId, src: ObjId, c: &AddCost, req: &Req, excluded: &[ObjId], picked: &[ObjId]) -> Vec<ObjId> {
    let cands: Vec<ObjId> = match c {
        AddCost::Sac(_, f) => candidates(cx, p, src, Some(f), Zone::Battlefield, false),
        AddCost::Blight(_) => candidates(cx, p, src, None, Zone::Battlefield, true),
        _ => return vec![],
    };
    let cands: Vec<ObjId> = cands.into_iter().filter(|o| !picked.contains(o)).collect();
    let srcs = sources_without(cx, p, excluded, None);
    distinct(cx, &cands)
        .into_iter()
        .filter(|&o| {
            let mut with: SmallVec<[ObjId; 2]> = picked.iter().copied().collect();
            with.push(o);
            crate::cast::payable_reserving(req, &srcs, crate::cast::Reserve::picked(cx, &with).as_ref())
        })
        .collect()
}

/// CR 601.2b–h: choose the permanents paid for sacrifice/blight costs before mana abilities are
/// activated; the mana payment then reserves them (`cast::ReserveFor::Picked`): it may tap them
/// but not sacrifice them. Returns the chosen permanents.
pub fn choose_before_mana(cx: &mut Ctx, p: PlayerId, so: ObjId, ann: &mut Announced, req: &Req) -> R<SmallVec<[ObjId; 1]>> {
    let mut picked: SmallVec<[ObjId; 1]> = SmallVec::new();
    for i in 0..ann.pay.len() {
        let c = ann.pay[i].clone();
        if !needs_battlefield_pick(&c) {
            continue;
        }
        let opts: Vec<Opt> = battlefield_picks(cx, p, so, &c, req, &[], &picked).into_iter().map(Opt::Card).collect();
        if opts.is_empty() {
            return cx.unsupported("additional cost: no permanent keeps the mana payable");
        }
        let Opt::Card(o) = cx.choose(Decision { player: p, kind: DecisionKind::PayCost, options: opts, subject: Some(so) })? else { unreachable!() };
        picked.push(o);
        ann.picked.push((i as u8, o));
    }
    Ok(picked)
}

/// Can additional cost `c` be paid together with mana `req` (mana part of `c` included)?
fn addcost_ok(cx: &Ctx, p: PlayerId, so: ObjId, f: &FaceDef, c: &AddCost, req: &Req, reduction: u32) -> bool {
    if !nonmana_payable(cx, p, so, c) {
        return false;
    }
    if needs_battlefield_pick(c) {
        return !battlefield_picks(cx, p, so, c, &reduced(req.clone(), reduction), &[], &[]).is_empty();
    }
    feasible(cx, p, so, f, req, reduction, &[], false, false)
}

fn optional_cost(cx: &Ctx, f: &FaceDef) -> Option<AddCost> {
    let _ = cx;
    f.statics.iter().find_map(|st| match &st.kind {
        StaticKind::Alt(AltStatic::OptionalCost(c)) => Some(c.clone()),
        _ => None,
    })
}

/// Mode selections whose costs (plus everything mandatory) can be paid.
pub fn affordable_mode_sets(cx: &Ctx, p: PlayerId, h: ObjId, face: u8, m: Method) -> Vec<SmallVec<[u8; 3]>> {
    let f = cx.db.get(cx.s.obj(h).card).face(face);
    let Some(base) = base_cost_of(cx.s, h, f, m) else { return vec![] };
    let sets = match f.spell {
        Some(a) => mode_sets(cx, p, a, Some(h)),
        None => vec![SmallVec::new()],
    };
    let reduction = cost_reduction(cx, p, h, face) + f.extra.sac_any_reduce.as_ref().map_or(0, |fl| sac_any_candidates(cx, p, h, fl).len() as u32);
    let mut base_req = requirement(&base, 0);
    base_req.any_type = crate::ext7::any_type_permission(cx, p, h);
    sets.into_iter()
        .filter(|ms| {
            let mut r = base_req.clone();
            if let Some(a) = f.spell {
                add_req(&mut r, &mode_cost(cx.db, a, ms));
                // Modes needing the optional additional cost: it must be payable too.
                if crate::ext2::charm_optcost_paid(cx.db, a, ms) == Some(true) {
                    let Some(c) = optional_cost(cx, f) else { return false };
                    add_req(&mut r, &mana_part(&c));
                    return addcost_ok(cx, p, h, f, &c, &r, reduction);
                }
            }
            if m == Method::WebSlinging {
                return !web_candidates(cx, p, h, f, &r, reduction).is_empty();
            }
            feasible(cx, p, h, f, &r, reduction, &[], m == Method::Harmonize, true)
        })
        .collect()
}

/// Web-slinging (CR 702.188a): the creatures `p` may return to pay for spell `so` with total
/// mana requirement `req`, as options: a tapped creature `p` controls (`Opt::Card`, distinct
/// ones), or an untapped one with a mana ability costing only {T}, tapped for mana now (CR
/// 601.2g before 601.2h; Spider-Sense ruling 2025-09-19: "if you tap a creature with a mana
/// ability to pay part of the spell's web-slinging cost, you could also return that
/// creature"): `Opt::Mana(o, a, t)`, one per mana type that keeps the spell payable.
pub fn web_candidates(cx: &Ctx, p: PlayerId, so: ObjId, f: &FaceDef, req: &Req, reduction: u32) -> Vec<Opt> {
    let s = &*cx.s;
    let mut out = vec![];
    let creature = |o: ObjId| s.obj(o).controller == p && chars(s, cx.db, o).types.has(CardType::Creature);
    let tapped: Vec<ObjId> = s.battlefield.iter().copied().filter(|&o| s.obj(o).tapped && creature(o)).collect();
    if !tapped.is_empty() && feasible(cx, p, so, f, req, reduction, &[], false, true) {
        out.extend(distinct(cx, &tapped).into_iter().map(Opt::Card));
    }
    for (o, a) in crate::cast::activatable_mana_abilities(cx, p) {
        let tap_only = cx.db.ability(a).cost.as_ref().is_none_or(|c| !c.sac_self && c.pay_life == 0 && c.tap_x.is_none());
        if !creature(o) || !tap_only || crate::ext6::ability_restr(cx.db, a, o).is_some() {
            continue;
        }
        let Some((mask, amount)) = crate::cast::mana_output(s, cx.db, o, p, a) else { continue };
        for t in ManaType::all() {
            if mask & (1 << t as u8) == 0 {
                continue;
            }
            let mut s2 = s.clone();
            s2.obj_mut(o).tapped = true;
            crate::ext6::add_pool(&mut s2, p, t, amount, None);
            let cx2 = Ctx::scratch(&mut s2, cx.db);
            if feasible(&cx2, p, so, f, req, reduction, &[], false, true) {
                out.push(Opt::Mana(o, a, t));
            }
        }
    }
    out
}

/// Can `p` cast `h` (as face `face`) by method `m` now? Covers timing, restrictions, modes and
/// targets (CR 601.2b–c), and payability of the total cost including mandatory additional costs.
pub fn castable(cx: &Ctx, p: PlayerId, h: ObjId, face: u8, sorcery_ok: bool, m: Method) -> bool {
    let f = cx.db.get(cx.s.obj(h).card).face(face);
    let Some(base) = base_cost_of(cx.s, h, f, m) else { return false };
    if base.none {
        return false;
    }
    let instant = f.types.has(CardType::Instant) || f.kws.has(Kw::Flash);
    // CR 702.170d: plotted cards are cast during your main phase while the stack is empty.
    // CR 702.190a: sneak any time you could cast an instant during your declare blockers step,
    // returning an unblocked creature you control.
    let timing = match m {
        Method::Plotted => sorcery_ok,
        Method::Sneak => cx.s.turn.active == p && cx.s.turn.step == Step::DeclareBlockers && !crate::cast::unblocked_attackers(cx, p).is_empty(),
        _ => instant || sorcery_ok,
    };
    if !timing || cant_cast(cx, p, h, face) {
        return false;
    }
    // CR 601.2a: the card is on the stack while its costs are determined and paid. From the
    // graveyard (harmonize) that matters for the creature tapped: its power may count the
    // graveyard (Enigma Drake), so check on a copy with the card moved to the stack.
    if m == Method::Harmonize && cx.s.obj(h).zone == Zone::Graveyard {
        let mut s2 = cx.s.clone();
        let so = put_on_stack_as_spell(&mut s2, h, face, p);
        let cx2 = Ctx::scratch(&mut s2, cx.db);
        return !affordable_mode_sets(&cx2, p, so, face, m).is_empty();
    }
    !affordable_mode_sets(cx, p, h, face, m).is_empty()
}

/// Alternative actions at priority: alternative casting methods from hand, harmonize from the
/// graveyard, casting warped/plotted cards from exile, and the plot special action.
pub fn legal_actions(cx: &Ctx, p: PlayerId, sorcery_ok: bool, out: &mut Vec<Opt>) {
    let s = &*cx.s;
    // Identical cards are one option unless something refers to one of them (`query::card_sig`).
    let hand = &s.player(p).hand;
    let hand_first = crate::query::first_of_each_card(s, cx.db, hand);
    for (i, &h) in hand.iter().enumerate() {
        if !hand_first[i] {
            continue;
        }
        let f = cx.db.get(s.obj(h).card).front();
        for m in [Method::Warp, Method::Evoke, Method::Impending, Method::Sneak, Method::WebSlinging] {
            if base_cost(f, m).is_some() && castable(cx, p, h, 0, sorcery_ok, m) {
                out.push(Opt::Alt(h, m as u8));
            }
        }
        // CR 702.170a: plot any time you have priority during your main phase, stack empty.
        if let Some(c) = &f.alt.plot {
            if sorcery_ok && payable(&requirement(c, 0), &mana_sources(cx, p, None)) {
                out.push(Opt::Alt(h, Method::Plot as u8));
            }
        }
    }
    // One per identical card among `objs` (`query::card_sig`).
    let first_among = |objs: Vec<ObjId>| -> Vec<ObjId> {
        let first = crate::query::first_of_each_card(s, cx.db, &objs);
        objs.into_iter().zip(first).filter(|x| x.1).map(|x| x.0).collect()
    };
    // CR 702.187b: mayhem, the turn the card was discarded ("discarded this turn" is a
    // designation in the card's signature: two such twins are one option).
    let gy = &s.player(p).graveyard;
    let mayhem: Vec<ObjId> = if s.ext.x6.discarded_this_turn.is_empty() { vec![] } else { gy.iter().copied().filter(|&o| cx.db.get(s.obj(o).card).front().alt.mayhem.is_some() && s.ext.x6.discarded_this_turn.contains(&o)).collect() };
    for o in first_among(mayhem) {
        if castable(cx, p, o, 0, sorcery_ok, Method::Mayhem) {
            out.push(Opt::Alt(o, Method::Mayhem as u8));
        }
    }
    let harmonize: Vec<ObjId> = gy.iter().copied().filter(|&o| cx.db.get(s.obj(o).card).front().alt.harmonize.is_some()).collect();
    for o in first_among(harmonize) {
        if castable(cx, p, o, 0, sorcery_ok, Method::Harmonize) {
            out.push(Opt::Alt(o, Method::Harmonize as u8));
        }
    }
    // Warped / plotted / airbent cards in exile: the designations are in the card's signature,
    // so twins with the same ones are one option; merged among the cards castable now only (a
    // plotted twin exiled this turn isn't).
    let ready: Vec<ObjId> = if s.cast_info.is_empty() {
        vec![]
    } else {
        s.exile.iter().copied().filter(|&o| s.obj(o).owner == p && s.cast_of(o).flags & (flag::WARPED | flag::PLOTTED) != 0 && s.turn.number > s.obj(o).entered).collect()
    };
    for o in first_among(ready) {
        let ci = s.cast_of(o);
        // CR 702.185a: a warped card may be cast after the turn it was exiled ended.
        if ci.has(flag::WARPED) && castable(cx, p, o, 0, sorcery_ok, Method::Normal) {
            out.push(Opt::Cast(o));
        }
        // CR 702.170d: a plotted card, on a later turn, without paying its mana cost.
        if ci.has(flag::PLOTTED) && castable(cx, p, o, 0, sorcery_ok, Method::Plotted) {
            out.push(Opt::Alt(o, Method::Plotted as u8));
        }
    }
    // CR 701.65a: an airbent card, by its owner, for {2} rather than its mana cost (any time
    // its timing allows; a land can't be cast: ruling 2025-10-02).
    let airbent: Vec<ObjId> = if s.cast_info.is_empty() { vec![] } else { s.exile.iter().copied().filter(|&o| s.obj(o).owner == p && s.cast_of(o).has(flag::AIRBENT)).collect() };
    for o in first_among(airbent) {
        // "Its owner may cast it" isn't limited to its normal characteristics: an adventurer or
        // omen card may be cast as its Adventure / Omen for {2} (CR 715.3, 715.3d: "other effects
        // that allow a player to cast it may allow a player to cast it as an Adventure"; 720.3;
        // adventurer rulings: alternative costs may apply to the Adventure), a split card or
        // Room as its other half (709.3); a land adventurer only as its Adventure.
        for face in crate::faces::spell_faces(s, cx.db, o, crate::faces::FacePerm::Any) {
            if castable(cx, p, o, face, sorcery_ok, Method::Airbend) {
                out.push(Opt::Alt(o, if face == 0 { Method::Airbend } else { Method::AirbendFace } as u8));
            }
        }
    }
}

/// Take an `Opt::Alt` action at priority.
pub fn take_action(cx: &mut Ctx, p: PlayerId, o: ObjId, m: u8) -> R<()> {
    match Method::from_u8(m) {
        Some(Method::Plot) => plot(cx, p, o),
        Some(Method::UnlockLeft) => crate::ext5::unlock_door(cx, p, o, 0),
        Some(Method::UnlockRight) => crate::ext5::unlock_door(cx, p, o, 1),
        Some(Method::TurnFaceUp) => crate::ext6::turn_face_up(cx, p, o),
        Some(Method::AirbendFace) => crate::cast::cast_spell_method(cx, p, o, 1, Method::Airbend),
        Some(m) => crate::cast::cast_spell_method(cx, p, o, 0, m),
        None => cx.unsupported(format!("cast method {m}")),
    }
}

/// CR 702.170a–b: exile the card from your hand and pay the plot cost (special action). The
/// card is exiled face up: it becomes public.
fn plot(cx: &mut Ctx, p: PlayerId, o: ObjId) -> R<()> {
    let c = cx.db.get(cx.s.obj(o).card).front().alt.plot.clone().unwrap();
    crate::cast::pay_mana(cx, p, requirement(&c, 0), None, o)?;
    if let Some(nid) = crate::zones::move_to(cx, o, Zone::Exile)? {
        cx.s.cast_mut(nid).flags |= flag::PLOTTED;
    }
    Ok(())
}

// ---------------------------------------------------------------- casting

/// Option codes of `DecisionKind::AdditionalCost` decisions (`Opt::CostOption`).
pub mod code {
    pub const DECLINE: u8 = 0;
    pub const EITHER_A: u8 = 1;
    pub const EITHER_B: u8 = 2;
    pub const OPTIONAL: u8 = 3;
    pub const OFFSPRING: u8 = 4;
    pub const TEAMWORK: u8 = 5;
}

/// Readable text of an `Opt::CostOption` for the spell `card`.
pub fn describe_cost_option(db: &CardDb, card: CardId, k: u8) -> String {
    let f = db.get(card).front();
    match k {
        code::DECLINE => "Don't pay the optional cost".into(),
        code::EITHER_A => f.alt.either.as_ref().map(|e| format!("Additional cost: {}", e.0.describe())).unwrap_or_default(),
        code::EITHER_B => f.alt.either.as_ref().map(|e| format!("Additional cost: {}", e.1.describe())).unwrap_or_default(),
        code::OPTIONAL if f.statics.iter().any(|st| matches!(st.kind, StaticKind::Alt(AltStatic::OptionalCost(_))) && st.text == "Gift") => "Promise a gift to your opponent (gift)".into(),
        code::OPTIONAL => f.statics.iter().find_map(|st| if let StaticKind::Alt(AltStatic::OptionalCost(c)) = &st.kind { Some(format!("Optional additional cost: {}", c.describe())) } else { None }).unwrap_or_default(),
        code::OFFSPRING => f.alt.offspring.as_ref().map(|c| format!("Pay offspring {c}")).unwrap_or_default(),
        code::TEAMWORK => format!("Use teamwork {} (tap creatures with that much total power)", f.alt.teamwork.unwrap_or(0)),
        _ => format!("cost option {k}"),
    }
}

/// Choices made while announcing a spell (CR 601.2b) that are paid later (601.2h).
#[derive(Clone, Debug, Default)]
pub struct Announced {
    pub pay: SmallVec<[AddCost; 2]>,
    /// Extra generic reduction (harmonize: the tapped creature's power).
    pub reduction: u32,
    /// Permanents chosen before mana payment for `pay[i]` (sacrifice/blight).
    pub picked: SmallVec<[(u8, ObjId); 1]>,
    /// Sneak: the unblocked attacker to return to its owner's hand (CR 702.190a).
    pub sneak_return: Option<ObjId>,
    /// Web-slinging: the tapped creature to return to its owner's hand (CR 702.188a).
    pub web_return: Option<ObjId>,
}

fn sig_items(cx: &Ctx, objs: &[ObjId]) -> Vec<(crate::query::ObjSig, Opt)> {
    crate::query::sig_items(cx.s, cx.db, objs)
}

/// Distinct options (identical objects are interchangeable), in canonical order.
fn distinct(cx: &Ctx, objs: &[ObjId]) -> Vec<ObjId> {
    let mut items = sig_items(cx, objs);
    items.sort_by_key(|x| (x.0, opt_key(&x.1)));
    items.dedup_by_key(|x| x.0);
    items.into_iter().filter_map(|(_, o)| if let Opt::Card(x) = o { Some(x) } else { None }).collect()
}

/// CR 601.2b: announce the alternative/additional-cost choices for spell `so` being cast by
/// method `m`: the harmonize creature, "A or B" costs, optional costs, offspring, teamwork.
/// Creatures tapped for harmonize/teamwork are tapped now (they can't also produce mana).
/// Mana parts are added to `req`; `reduction` is the generic reduction already known.
pub fn announce(cx: &mut Ctx, p: PlayerId, so: ObjId, face: u8, m: Method, req: &mut Req, reduction: u32) -> R<Announced> {
    announce_with(cx, p, so, face, m, req, reduction, None)
}

/// `announce`; `forced_optional`: whether the optional additional cost is paid was already
/// decided by the number of modes chosen (`CardDb::charm_optcost`).
#[allow(clippy::too_many_arguments)]
pub fn announce_with(cx: &mut Ctx, p: PlayerId, so: ObjId, face: u8, m: Method, req: &mut Req, reduction: u32, forced_optional: Option<bool>) -> R<Announced> {
    let card = cx.s.obj(so).card;
    let f = cx.db.get(card).face(face);
    let mut ann = Announced::default();
    cx.s.cast_mut(so).flags |= match m {
        Method::Warp => flag::WARP,
        Method::Evoke => flag::EVOKE,
        Method::Impending => flag::IMPENDING,
        Method::Harmonize => flag::HARMONIZE,
        _ => 0,
    };
    // Harmonize (CR 702.180a–b): tap up to one untapped creature you control.
    if m == Method::Harmonize {
        let mut opts = vec![];
        if feasible(cx, p, so, f, req, reduction, &[], false, true) {
            opts.push(Opt::Done);
        }
        let cands: Vec<ObjId> = tappable_creatures(cx, p, &[]).into_iter().map(|c| c.0).collect();
        for c in distinct(cx, &cands) {
            let pw = chars(cx.s, cx.db, c).power.max(0) as u32;
            if feasible(cx, p, so, f, req, reduction + pw, &[c], false, true) {
                opts.push(Opt::Card(c));
            }
        }
        if let Opt::Card(c) = cx.choose(Decision { player: p, kind: DecisionKind::PayCost, options: opts, subject: Some(so) })? {
            ann.reduction = ann.reduction.saturating_add(chars(cx.s, cx.db, c).power.max(0) as u32);
            cx.s.obj_mut(c).tapped = true;
        }
    }
    let red = |a: &Announced| reduction + a.reduction;
    // Sneak (CR 702.190a): which unblocked creature you control is returned (distinct options).
    if m == Method::Sneak {
        let cands = distinct(cx, &crate::cast::unblocked_attackers(cx, p));
        let opts: Vec<Opt> = cands.into_iter().map(Opt::Card).collect();
        let Opt::Card(o) = cx.choose(Decision { player: p, kind: DecisionKind::PayCost, options: opts, subject: Some(so) })? else { unreachable!() };
        ann.sneak_return = Some(o);
    }
    // Web-slinging (CR 702.188a): which tapped creature you control is returned; one tapped
    // for mana now is tapped as it's chosen (`web_candidates`).
    if m == Method::WebSlinging {
        let opts = web_candidates(cx, p, so, f, req, red(&ann));
        if opts.is_empty() {
            return cx.unsupported("web-slinging without a creature to return");
        }
        ann.web_return = Some(match cx.choose(Decision { player: p, kind: DecisionKind::PayCost, options: opts, subject: Some(so) })? {
            Opt::Card(o) => o,
            Opt::Mana(o, a, t) => {
                crate::cast::activate_mana_ability_as(cx, p, o, a, Some(t))?;
                o
            }
            _ => unreachable!(),
        });
    }
    // "As an additional cost, [A] or [B]" (mandatory).
    if let Some((a, b)) = f.alt.either.clone() {
        let mut opts = vec![];
        for (k, c) in [(code::EITHER_A, &a), (code::EITHER_B, &b)] {
            let mut r = req.clone();
            add_req(&mut r, &mana_part(c));
            if addcost_ok(cx, p, so, f, c, &r, red(&ann)) {
                opts.push(Opt::CostOption(k));
            }
        }
        let c = cx.choose(Decision { player: p, kind: DecisionKind::AdditionalCost, options: opts, subject: Some(so) })?;
        let chosen = if c == Opt::CostOption(code::EITHER_A) { a } else { b };
        add_req(req, &mana_part(&chosen));
        ann.pay.push(chosen);
    }
    // A mandatory additional cost: nothing to announce (castability checked it is payable).
    if let Some(c) = f.alt.extra_cost.clone() {
        add_req(req, &mana_part(&c));
        ann.pay.push(c);
    }
    // Optional additional costs.
    if let Some(c) = optional_cost(cx, f) {
        let mut r = req.clone();
        add_req(&mut r, &mana_part(&c));
        if forced_optional == Some(true) && !addcost_ok(cx, p, so, f, &c, &r, red(&ann)) {
            return cx.unsupported("modes chosen need an optional cost that can't be paid");
        }
        // A gift whose promise changes the targets (Into the Flood Maw, `ext8`): only the
        // choices that leave legal targets (CR 601.2c).
        let gate = f.spell.map_or((true, true), |a| crate::ext8::promise_gate(cx, p, a, so, false));
        if forced_optional != Some(false) && gate.1 && addcost_ok(cx, p, so, f, &c, &r, red(&ann)) {
            let opts = if forced_optional == Some(true) || !gate.0 { vec![Opt::CostOption(code::OPTIONAL)] } else { vec![Opt::CostOption(code::OPTIONAL), Opt::CostOption(code::DECLINE)] };
            if cx.choose(Decision { player: p, kind: DecisionKind::AdditionalCost, options: opts, subject: Some(so) })? == Opt::CostOption(code::OPTIONAL) {
                *req = r;
                ann.pay.push(c);
                cx.s.cast_mut(so).flags |= flag::OPTCOST;
            }
        }
    }
    // Offspring (CR 702.175a).
    if let Some(c) = f.alt.offspring.clone() {
        let mut r = req.clone();
        add_req(&mut r, &requirement(&c, 0));
        if feasible(cx, p, so, f, &r, red(&ann), &[], false, false) {
            let opts = vec![Opt::CostOption(code::OFFSPRING), Opt::CostOption(code::DECLINE)];
            if cx.choose(Decision { player: p, kind: DecisionKind::AdditionalCost, options: opts, subject: Some(so) })? == Opt::CostOption(code::OFFSPRING) {
                *req = r;
                cx.s.cast_mut(so).flags |= flag::OFFSPRING;
            }
        }
    }
    // Teamwork N (CR 702.194a): tap any number of creatures with total power N or more.
    if let Some(n) = f.alt.teamwork {
        if teamwork_completable(cx, p, so, f, req, red(&ann), n) {
            let opts = vec![Opt::CostOption(code::TEAMWORK), Opt::CostOption(code::DECLINE)];
            if cx.choose(Decision { player: p, kind: DecisionKind::AdditionalCost, options: opts, subject: Some(so) })? == Opt::CostOption(code::TEAMWORK) {
                choose_teamwork(cx, p, so, f, req, red(&ann), n)?;
                cx.s.cast_mut(so).flags |= flag::TEAMWORK;
            }
        }
    }
    Ok(ann)
}

type Keyed = (crate::query::ObjSig, ObjId, i32);

/// Untapped creatures, in canonical order (signature, id), with power (CR 702.194a: "any
/// number of creatures", a 0-power one too).
fn teamwork_candidates(cx: &Ctx, p: PlayerId) -> Vec<Keyed> {
    let cands: Vec<(ObjId, i32)> = tappable_creatures(cx, p, &[]);
    let objs: Vec<ObjId> = cands.iter().map(|c| c.0).collect();
    let sigs = crate::query::frozen_if_big(cx.s, cx.db, || crate::query::obj_sigs(cx.s, cx.db, &objs));
    let mut keyed: Vec<Keyed> = sigs.into_iter().zip(cands).map(|(k, (o, pw))| (k, o, pw)).collect();
    keyed.sort_by_key(|k| (k.0, k.1));
    keyed
}

/// Can creatures from `rest` (picked in order) bring power `have` up to `n` with the mana
/// still payable without the tapped creatures? Bounded search.
#[allow(clippy::too_many_arguments)]
fn reachable(cx: &Ctx, p: PlayerId, so: ObjId, f: &FaceDef, req: &Req, reduction: u32, rest: &[Keyed], chosen: &mut SmallVec<[ObjId; 8]>, have: i32, n: i32, budget: &mut u32) -> bool {
    if have >= n {
        return feasible(cx, p, so, f, req, reduction, chosen, false, false);
    }
    if *budget == 0 || have + rest.iter().map(|r| r.2.max(0)).sum::<i32>() < n {
        return false;
    }
    *budget -= 1;
    for j in 0..rest.len() {
        chosen.push(rest[j].1);
        let ok = reachable(cx, p, so, f, req, reduction, &rest[j + 1..], chosen, have + rest[j].2, n, budget);
        chosen.pop();
        if ok {
            return true;
        }
    }
    false
}

/// Can teamwork N be paid (some creatures with total power N, mana still payable)?
fn teamwork_completable(cx: &Ctx, p: PlayerId, so: ObjId, f: &FaceDef, req: &Req, reduction: u32, n: i32) -> bool {
    let keyed = teamwork_candidates(cx, p);
    let mut budget = 4096;
    reachable(cx, p, so, f, req, reduction, &keyed, &mut SmallVec::new(), 0, n, &mut budget)
}

/// Choose creatures to tap for teamwork one at a time in canonical order; a pick is offered only
/// if the total `n` stays reachable with creatures later in the order (and the mana payable).
/// Once the total is `n` or more, `Done` is offered beside further creatures (CR 702.194a: "any
/// number of creatures"; tapping more than needed is legal as crew). Taps them.
fn choose_teamwork(cx: &mut Ctx, p: PlayerId, so: ObjId, f: &FaceDef, req: &Req, reduction: u32, n: i32) -> R<()> {
    let mut keyed = teamwork_candidates(cx, p);
    let mut chosen: SmallVec<[ObjId; 8]> = SmallVec::new();
    let mut have = 0;
    let mut next = 0usize;
    loop {
        let mut opts = vec![];
        let mut pos = vec![];
        let mut seen_sig = None;
        for i in next..keyed.len() {
            let (sig, o, pw) = keyed[i];
            if seen_sig == Some(sig) {
                continue;
            }
            let mut ch2 = chosen.clone();
            ch2.push(o);
            let mut budget = 4096;
            if reachable(cx, p, so, f, req, reduction, &keyed[i + 1..], &mut ch2, have + pw, n, &mut budget) {
                seen_sig = Some(sig);
                opts.push(Opt::Card(o));
                pos.push(i);
            }
        }
        if opts.is_empty() {
            if have >= n {
                break;
            }
            return cx.unsupported("teamwork: no completable choice");
        }
        if have >= n {
            opts.push(Opt::Done);
        }
        let c = cx.choose(Decision { player: p, kind: DecisionKind::PayCost, options: opts.clone(), subject: Some(so) })?;
        if c == Opt::Done {
            break;
        }
        let i = pos[cx.offered_ix(c, &opts)];
        cx.swap_alt(c, i, &mut keyed, |k| Opt::Card(k.1) == c)?;
        next = i + 1;
        have += keyed[i].2;
        chosen.push(keyed[i].1);
    }
    for c in chosen {
        cx.s.obj_mut(c).tapped = true;
    }
    Ok(())
}

/// Pay the non-mana additional costs announced for `so` (CR 601.2h), choosing objects.
pub fn pay_announced(cx: &mut Ctx, p: PlayerId, so: ObjId, ann: &Announced) -> R<()> {
    if let Some(o) = ann.sneak_return {
        // CR 702.190a–b: return it; the permanent this spell becomes attacks what it attacked.
        let Some(&(_, def)) = cx.s.combat.attackers.iter().find(|a| a.0 == o) else { return cx.unsupported("sneak: the returned creature isn't attacking") };
        let pw = cx.s.combat.attacking_pw.iter().find(|a| a.0 == o).map(|a| a.1);
        cx.s.ext.lt.sneak.push((so, def, pw));
        crate::zones::move_batch(cx, &[crate::zones::Move::new(o, Zone::Hand)])?;
    }
    if let Some(o) = ann.web_return {
        // CR 702.188a, 601.2h: still a tapped creature you control as costs are paid.
        let ok = cx.s.obj(o).zone == Zone::Battlefield && cx.s.obj(o).controller == p && cx.s.obj(o).tapped && chars(cx.s, cx.db, o).types.has(CardType::Creature);
        if !ok {
            return cx.unsupported("web-slinging: the creature to return is no longer a tapped creature you control");
        }
        crate::zones::move_batch(cx, &[crate::zones::Move::new(o, Zone::Hand)])?;
    }
    for (i, c) in ann.pay.iter().enumerate() {
        match ann.picked.iter().find(|x| x.0 as usize == i) {
            Some(&(_, o)) => pay_picked(cx, so, c, o)?,
            None => pay_add_cost(cx, p, so, c)?,
        }
    }
    Ok(())
}

fn pick_objs(cx: &mut Ctx, p: PlayerId, so: ObjId, cands: Vec<ObjId>, n: usize, kind: DecisionKind) -> R<Vec<ObjId>> {
    if cands.len() < n {
        // E.g. a Treasure was sacrificed for mana that was also needed for a sacrifice cost.
        return cx.unsupported("additional cost no longer payable after mana payment");
    }
    let items = sig_items(cx, &cands);
    Ok(cx.choose_set(p, kind, items, n, n, Some(so))?.into_iter().filter_map(|o| if let Opt::Card(x) = o { Some(x) } else { None }).collect())
}

/// Pay a sacrifice/blight cost with the permanent chosen before mana payment.
fn pay_picked(cx: &mut Ctx, so: ObjId, c: &AddCost, o: ObjId) -> R<()> {
    let _ = so;
    if cx.s.obj(o).zone != Zone::Battlefield {
        return cx.unsupported("additional cost no longer payable after mana payment");
    }
    match c {
        AddCost::Sac(..) => {
            let p = cx.s.obj(o).controller;
            crate::trig_ext::fire_sacrificed(cx, &[o], p);
            crate::zones::move_batch(cx, &[crate::zones::Move::new(o, Zone::Graveyard)])?;
        }
        AddCost::Blight(n) => crate::ext6::put_counters(cx.s, cx.db, o, CounterKind::M1M1, *n as i32),
        _ => unreachable!(),
    }
    Ok(())
}

fn pay_add_cost(cx: &mut Ctx, p: PlayerId, so: ObjId, c: &AddCost) -> R<()> {
    match c {
        AddCost::Mana(_) => {}
        AddCost::Life(n) => crate::resolve::lose_life(cx.s, cx.db, p, *n),
        AddCost::Discard(n, f) => {
            let cands = candidates(cx, p, so, Some(f), Zone::Hand, false);
            let chosen = pick_objs(cx, p, so, cands, *n as usize, DecisionKind::PayCost)?;
            let mut moves = vec![];
            for o in chosen {
                let card = cx.s.obj(o).card;
                cx.s.emit(Event::Discard { player: p, card });
                moves.push(crate::zones::Move::new(o, Zone::Graveyard));
            }
            crate::zones::move_batch(cx, &moves)?;
        }
        AddCost::Sac(n, f) => {
            let cands = candidates(cx, p, so, Some(f), Zone::Battlefield, false);
            let chosen = pick_objs(cx, p, so, cands, *n as usize, DecisionKind::PayCost)?;
            crate::trig_ext::fire_sacrificed(cx, &chosen, p);
            let moves: Vec<_> = chosen.into_iter().map(|o| crate::zones::Move::new(o, Zone::Graveyard)).collect();
            crate::zones::move_batch(cx, &moves)?;
        }
        AddCost::Blight(n) => {
            let cands = candidates(cx, p, so, None, Zone::Battlefield, true);
            if cands.is_empty() {
                return cx.unsupported("blight: no creature");
            }
            let o = pick_objs(cx, p, so, cands, 1, DecisionKind::PayCost)?[0];
            crate::ext6::put_counters(cx.s, cx.db, o, CounterKind::M1M1, *n as i32);
        }
        AddCost::Behold(f) => {
            behold(cx, p, so, f)?;
        }
        AddCost::CollectEvidence(n) => crate::ext2::collect_evidence(cx, p, so, *n as i32)?,
        AddCost::ExileFromGrave(n, f) => {
            let cands = candidates(cx, p, so, Some(f), Zone::Graveyard, false);
            let chosen = pick_objs(cx, p, so, cands, *n as usize, DecisionKind::PayCost)?;
            let moves: Vec<_> = chosen.into_iter().map(|o| crate::zones::Move::new(o, Zone::Exile)).collect();
            crate::zones::move_batch(cx, &moves)?;
        }
    }
    Ok(())
}

/// Behold (CR 701.4a): choose a matching permanent you control or reveal a matching card from
/// your hand (revealing makes it known to everyone). Returns the chosen object.
pub fn behold(cx: &mut Ctx, p: PlayerId, src: ObjId, f: &Filter) -> R<ObjId> {
    let mut cands = distinct(cx, &candidates(cx, p, src, Some(f), Zone::Battlefield, false));
    let hand = candidates(cx, p, src, Some(f), Zone::Hand, false);
    let first = crate::query::first_of_each_card(cx.s, cx.db, &hand);
    cands.extend(hand.iter().zip(first).filter(|x| x.1).map(|x| *x.0));
    let opts: Vec<Opt> = cands.into_iter().map(Opt::Card).collect();
    let Opt::Card(o) = cx.choose(Decision { player: p, kind: DecisionKind::PayCost, options: opts, subject: Some(src) })? else { unreachable!() };
    if cx.s.obj(o).zone == Zone::Hand {
        cx.s.obj_mut(o).known_to = 0b11;
    }
    Ok(o)
}

/// "Unless"-cost forms handled here: can `p` behold?
pub fn can_behold(cx: &Ctx, p: PlayerId, src: ObjId, f: &Filter) -> bool {
    nonmana_payable(cx, p, src, &AddCost::Behold(f.clone()))
}

// ---------------------------------------------------------------- triggers and effects

/// CR 702.185a: warp's "at the beginning of the next end step" delayed trigger. Called as the
/// end step begins. Mobilize's tokens (CR 702.181a) lose their "sacrificed at the next end step"
/// marker here; their one delayed trigger is a `DelayedWhen::NextStep` (`AltEffect::Mobilize`).
pub fn end_step_begins(s: &mut State, db: &CardDb) {
    if s.cast_info.is_empty() {
        return;
    }
    // Battlefield order (timestamps of entering), as the triggers would be created; then warped
    // permanents that already left the battlefield (old objects kept in `cast_info` as last known
    // information): the trigger was created when the spell resolved, so it still triggers and
    // does nothing as it resolves (CR 603.7c).
    let mut due: Vec<ObjId> = s.battlefield.iter().copied().filter(|&o| s.cast_of(o).flags & (flag::WARP | flag::MOBILIZED) != 0).collect();
    due.extend(s.cast_info.iter().filter(|e| e.1.has(flag::WARP) && s.obj(e.0).zone == Zone::Gone).map(|e| e.0));
    for o in due {
        let ob = s.obj(o);
        let (card, controller) = (ob.card, ob.controller);
        let alt = &db.get(card).front().alt;
        if s.cast_of(o).has(flag::MOBILIZED) {
            s.cast_mut(o).flags &= !flag::MOBILIZED;
        }
        if s.cast_of(o).has(flag::WARP) {
            s.cast_mut(o).flags &= !flag::WARP; // the delayed trigger happens once
            if let Some(ti) = alt.warp_trigger {
                s.pending_triggers.push(PendingTrigger { source: o, card, controller, trigger_idx: ti, trig: TrigCtx::default(), adhoc: None });
            }
        }
    }
}

pub fn apply(cx: &mut Ctx, you: PlayerId, source: ObjId, e: &AltEffect, remembered: &[ObjId]) -> R<()> {
    let on_bf = cx.s.obj(source).zone == Zone::Battlefield;
    match e {
        AltEffect::WarpExile => {
            if on_bf {
                if let Some(nid) = crate::zones::move_to(cx, source, Zone::Exile)? {
                    cx.s.cast_mut(nid).flags |= flag::WARPED;
                }
            }
        }
        AltEffect::SacrificeSelf => {
            if on_bf && cx.s.obj(source).controller == you {
                // A sacrifice (CR 701.21a; evoke 702.74a, mobilize 702.181a): "whenever you
                // sacrifice" triggers and "sacrificed this turn" counts see it.
                crate::trig_ext::fire_sacrificed(cx, &[source], you);
                crate::zones::move_to(cx, source, Zone::Graveyard)?;
            }
        }
        AltEffect::ImpendingTick(k) => {
            if on_bf {
                crate::ext6::put_counters(cx.s, cx.db, source, *k, -1);
            }
        }
        AltEffect::OffspringCopy => {
            // CR 702.175a / 707.2: copiable values of the permanent (last known if it left; a
            // copy of a copy, ruling 2024-07-26), except it's 1/1: an exception of the copy
            // effect, so part of the token's copiable values (707.9b) and copied by a copy of
            // the token (Three Steps Ahead ruling 2024-04-12).
            crate::zones::create_token_copy_except(cx, source, you, crate::zones::CopyExc { set_pt: Some((1, 1)), flags: flag::OFFSPRING_TOKEN, add_super: 0 })?;
        }
        AltEffect::SacrificeRemembered => {
            let mine: Vec<ObjId> = remembered.iter().copied().filter(|&o| cx.s.obj(o).zone == Zone::Battlefield && cx.s.obj(o).controller == you).collect();
            if !mine.is_empty() {
                crate::trig_ext::fire_sacrificed(cx, &mine, you);
                let moves: Vec<crate::zones::Move> = mine.iter().map(|&o| crate::zones::Move::new(o, Zone::Graveyard)).collect();
                crate::zones::move_batch(cx, &moves)?;
            }
        }
        AltEffect::Mobilize { token, n, sac } => {
            let ids = crate::zones::create_tokens_with(cx, *token, you, *n as i32, true, CastInfo { flags: flag::MOBILIZED, spent: 0 })?;
            // One delayed triggered ability for all of them (CR 603.7a), created
            // whether or not they could attack.
            crate::trig_ext::add_delayed_over(cx, crate::ext::DelayedWhen::NextStep { step: Step::End }, you, source, *sac, &ids)?;
            // CR 508.4: they enter attacking (never "attacked"); for each token on its own, its
            // controller chooses the defending player or a planeswalker that player controls
            // (Voice of Victory ruling 2025-04-04).
            if !(cx.s.turn.step >= Step::DeclareAttackers && cx.s.turn.step <= Step::EndCombat) {
                return Ok(());
            }
            for t in ids {
                if cx.s.obj(t).zone == Zone::Battlefield {
                    crate::combat::enter_attacking(cx, you, t)?;
                }
            }
        }
    }
    Ok(())
}


/// Candidates for "you may sacrifice any number of [filter] permanents; this spell costs {1}
/// less for each" (Rottenmouth Viper): your matching permanents, except mana sources that
/// sacrifice themselves (using them for mana pays the same {1}).
pub fn sac_any_candidates(cx: &Ctx, p: PlayerId, h: ObjId, f: &Filter) -> Vec<ObjId> {
    let srcs = mana_sources(cx, p, None);
    let fcx = FilterCtx { you: p, source: Some(h), x: 0, rem: &[] };
    let mut v: Vec<ObjId> = cx
        .s
        .battlefield
        .iter()
        .copied()
        .filter(|&o| o != h && cx.s.obj(o).controller == p && obj_matches(cx.s, cx.db, f, o, fcx) && !srcs.iter().any(|x| x.obj == o && x.sac))
        .collect();
    crate::query::sort_by_sig(cx.s, cx.db, &mut v);
    v
}

/// Choose the permanents to sacrifice for `sac_any_reduce` (CR 601.2b): one at a time in
/// canonical order, identical ones interchangeable; `Done` when the rest of the cost (reduced by
/// {1} per permanent) is payable, a pick only if the cost stays payable with every later
/// candidate added. The chosen permanents may still be tapped for mana before they're
/// sacrificed (601.2g–h).
pub fn choose_sac_any(cx: &mut Ctx, p: PlayerId, so: ObjId, f: &Filter, req: &Req, reduction: u32) -> R<Vec<ObjId>> {
    let mut cands = sac_any_candidates(cx, p, so, f);
    let srcs = crate::cast::mana_sources_for(cx, p, None, Some(so));
    let ok = |k: usize| payable(&reduced(req.clone(), reduction + k as u32), &srcs);
    let (mut chosen, mut next): (Vec<ObjId>, usize) = (vec![], 0);
    loop {
        let mut opts = vec![];
        let mut pos = vec![];
        if ok(chosen.len()) {
            opts.push(Opt::Done);
            pos.push(usize::MAX);
        }
        let mut last = None;
        let sigs = crate::query::frozen_if_big(cx.s, cx.db, || crate::query::obj_sigs(cx.s, cx.db, &cands[next..]));
        for i in next..cands.len() {
            let sig = sigs[i - next];
            if last == Some(sig) || !ok(chosen.len() + 1 + (cands.len() - i - 1)) {
                continue;
            }
            last = Some(sig);
            opts.push(Opt::Card(cands[i]));
            pos.push(i);
        }
        if opts.is_empty() {
            return cx.unsupported("sacrifice-to-reduce dead end");
        }
        let c = cx.choose(Decision { player: p, kind: DecisionKind::Sacrifice, options: opts.clone(), subject: Some(so) })?;
        let i = pos[cx.offered_ix(c, &opts)];
        if i == usize::MAX {
            return Ok(chosen);
        }
        cx.swap_alt(c, i, &mut cands, |&o| Opt::Card(o) == c)?;
        chosen.push(cands[i]);
        next = i + 1;
    }
}
