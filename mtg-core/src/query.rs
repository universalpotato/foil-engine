//! Derived information: characteristics after continuous effects (CR 613), filter matching,
//! targeting legality.

use crate::basics::*;
use crate::carddb::{CardDb, Kw, KwSet};
use crate::filter::{Alt, Base, Filter, Prop, Stat};
use crate::state::*;
use smallvec::SmallVec;

/// `Clone` is hand-written (below): memo hits clone these, and the derived clone takes
/// `SmallVec`'s generic iterator path.
#[derive(Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Chars {
    pub types: TypeSet,
    pub supertypes: Supertypes,
    pub subtypes: SmallVec<[Subtype; 4]>,
    pub colors: ColorSet,
    pub power: i32,
    pub toughness: i32,
    pub kws: KwSet,
    pub mv: u32,
    /// Activated abilities (printed + gained), in order.
    pub abilities: SmallVec<[crate::carddb::AbilityId; 4]>,
    /// Lost all abilities (printed triggers/statics don't function).
    pub lost_abilities: bool,
    /// Has all creature types (CR 205.3m "all creature types", e.g. Soulstone Sanctuary).
    pub all_creature_types: bool,
}

impl Clone for Chars {
    #[inline]
    fn clone(&self) -> Chars {
        Chars { subtypes: SmallVec::from_slice(&self.subtypes), abilities: SmallVec::from_slice(&self.abilities), ..*self }
    }
}

/// Characteristics from layers 1–5 only (printed values; copy/control/text/type/color effects
/// would go here). Used to evaluate which objects static abilities affect without recursion.
pub fn chars_base(s: &State, db: &CardDb, id: ObjId) -> Chars {
    if s.battlefield.len() >= BIG_BOARD && memo_idle() {
        return frozen(s, db, || chars_base(s, db, id)); // see `chars`
    }
    if memo_active(s) {
        if let Some(c) = memo_get(s, db, id, false) {
            return c;
        }
        let c = chars_base_defined(s, db, id).0;
        memo_put(s, db, id, false, &c);
        return c;
    }
    chars_base_defined(s, db, id).0
}

/// `AffectedDefined$` statics (the object's own, or of Equipment/Auras attached to it) that
/// apply to `id`, with their timestamps and sources.
pub type DefinedList<'a> = SmallVec<[(u32, &'a crate::carddb::DefinedCont, ObjId); 2]>;

/// Does this `AffectedDefined$` static have a layer-4 (type-changing) part?
fn defined_l4(dc: &crate::carddb::DefinedCont) -> bool {
    dc.add_types != TypeSet::default() || !dc.add_subtypes.is_empty() || dc.remove_card_types
}

/// Does `dc` (a static of `src`) apply to `id`, whose characteristics are `c` at the layer where
/// the effect starts to apply (its `Affected$` filter; CR 611.3a, 613.6)? Evaluated under the
/// `IN_DEFINED` recursion guard (see `chars_base_defined`).
fn defined_affects(s: &State, db: &CardDb, dc: &crate::carddb::DefinedCont, src: ObjId, id: ObjId, c: &Chars) -> bool {
    let Some(f) = &dc.affected else { return true };
    let so = s.obj(src);
    let fcx = FilterCtx { you: so.controller, source: Some(src), x: 0, rem: &[] };
    let was = IN_DEFINED.with(|g| g.replace(true));
    let ok = f.alts.iter().any(|a| alt_matches_obj(s, db, a, id, c, fcx));
    IN_DEFINED.with(|g| g.set(was));
    ok
}

/// The `AffectedDefined$` statics that may apply to `id` (their conditions hold and their source
/// has them), sorted by timestamp; whether each applies (its `Affected$` filter) is decided in
/// the layer where it starts to apply (`chars_base_defined`).
fn defined_statics<'a>(s: &State, db: &'a CardDb, id: ObjId) -> DefinedList<'a> {
    let mut out: DefinedList<'a> = SmallVec::new();
    let mut consider = |src: ObjId, want: crate::carddb::StaticWho| {
        let so = s.obj(src);
        for (si, st) in db.get(so.card).face(so.face).statics.iter().enumerate() {
            let crate::carddb::StaticKind::DefinedContinuous(dc) = &st.kind else { continue };
            if dc.who != want || !st.zones.has(Zone::Battlefield) {
                continue;
            }
            // The conditions depend on the source and the state only (not on `id`): memoized
            // per (source, static) in a `frozen` scope (`memo_defined_cond`).
            let cond = || {
                let ecx = crate::expr::EvalCtx { s, db, you: so.controller, source: src, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &[], imprinted: &[], base_only: true };
                st.conditions.iter().all(|cd| crate::expr::holds(cd, &ecx))
            };
            if !st.conditions.is_empty() && !memo_defined_cond(s, db, src, si, cond) {
                continue;
            }
            // A source that lost all abilities (layer 6) no longer has this static: its effect
            // depends on the loss (613.8a(b): the loss changes its existence) and doesn't apply,
            // unless it has a layer-4 part, which applied before the loss and keeps applying
            // in the later layers (613.6). E.g. Basilisk Collar under Azure Beastbinder's
            // trigger; a creature's own "has flying as long as ..." under The Wondrous Wasp's.
            if !defined_l4(dc) && may_lose_abilities(s, db) && lost_all_abilities(s, db, src) {
                continue;
            }
            out.push((so.ts, &**dc, src));
        }
    };
    consider(id, crate::carddb::StaticWho::SelfObj);
    for src in attached_to(s, db, id) {
        consider(src, crate::carddb::StaticWho::Attached);
    }
    out.sort_by_key(|x| x.0);
    out
}

/// Layer 1–5 characteristics plus the `AffectedDefined$` statics applying to the object, sorted
/// by timestamp. Their layer-4 type changes are applied in timestamp order with the other
/// layer-4 effects (CR 613.1d, 613.7, 613.7a: the timestamp of the static's object; an Aura's or
/// Equipment's is when it became attached, 613.7e) and they keep applying in later layers
/// (613.6); one without a layer-4 part applies if its `Affected$` filter matches the object as
/// layers 1–5 leave it (an Equipment's "equipped creature gets +2/+0" on a permanent that is a
/// creature only by its own static).
pub fn chars_base_defined<'a>(s: &State, db: &'a CardDb, id: ObjId) -> (Chars, DefinedList<'a>) {
    COMPUTED.with(|n| n.set(n.get() + 1));
    if let Some(l) = crate::lki::of(s, id) {
        return (l.chars.clone(), SmallVec::new());
    }
    // A condition of an `AffectedDefined$` static ("as long as you control another Dwarf") is
    // evaluated on layer 1–5 characteristics *without* such statics, so it can't recurse into
    // itself (CR 613.8 dependencies among these don't arise in the supported cards).
    if s.obj(id).zone != Zone::Battlefield || IN_DEFINED.with(|f| f.get()) {
        return (chars_base_l15(s, db, id, &[], &mut SmallVec::new()), SmallVec::new());
    }
    IN_DEFINED.with(|f| f.set(true));
    let cands = defined_statics(s, db, id);
    IN_DEFINED.with(|f| f.set(false));
    let mut list: DefinedList<'a> = SmallVec::new();
    let c = chars_base_l15(s, db, id, &cands, &mut list);
    for &(ts, dc, src) in &cands {
        if !defined_l4(dc) && defined_affects(s, db, dc, src, id, &c) {
            list.push((ts, dc, src));
        }
    }
    list.sort_by_key(|x| x.0);
    (c, list)
}

thread_local! {
    /// Set while `defined_statics` evaluates conditions (recursion guard, see above).
    static IN_DEFINED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static MEMO: std::cell::RefCell<Memo> = std::cell::RefCell::new(Memo::default());
    /// The memo `frozen_aside` swaps in while it runs (kept between calls, like `MEMO`).
    static ASIDE: std::cell::RefCell<Memo> = std::cell::RefCell::new(Memo::default());
    /// Characteristic computations (`chars_base_defined` + `chars_uncached` calls, i.e. memo
    /// misses) on this thread: a deterministic, host-independent measure of engine work for
    /// cost regression tests.
    static COMPUTED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Characteristic computations so far on this thread (see `COMPUTED`): differences between two
/// reads measure the work of the code in between.
pub fn chars_computations() -> u64 {
    COMPUTED.with(|n| n.get())
}

// ------------------------------------------------------------------ characteristics memo
//
// `chars`/`chars_base` are pure functions of (`&State`, `&CardDb`, id) (plus `IN_DEFINED`), and
// costly: each scans every permanent's static abilities, whose filters and conditions call
// `chars_base` of other objects. Read-only queries (option enumeration, mana sources, SBA scans)
// ask for the same objects many times. `frozen(s, db, f)` memoizes them while `f` runs: `s` is
// borrowed shared for the whole call and `State` has no interior mutability, so it cannot change
// while the memo is installed, and the memo is keyed by the addresses of `s` and `db` (calls on
// any other state, e.g. a scratch copy, bypass it). Values computed while `IN_DEFINED` is set (a
// different, reduced function, also pure) live in their own tables (`full_red`, `base_red`).
//
// Across scopes: each memo epoch keeps a snapshot of the state content its values were computed
// for (cheap: `ObjVec` chunks are shared). A new outermost scope whose state is
// `State::eq_but_priority` to the snapshot, with the same `CardDb` instance, continues the epoch;
// otherwise a new epoch starts. Every memoized value is a function of exactly that content, so
// this is exact; it lets a chain of units in which players pass priority without anything else
// changing compute characteristics once instead of once per unit.

#[derive(Default)]
struct Memo {
    state: usize,
    db: usize,
    /// Scope counter: an entry is valid iff its stamp equals the current scope's (so entries
    /// never need clearing and the tables are reused across scopes).
    epoch: u64,
    full: Vec<(u64, Chars)>,
    base: Vec<(u64, Chars)>,
    /// The same two functions while `IN_DEFINED` is set (the reduced ones `defined_statics`
    /// conditions see: layers 1–5 without `AffectedDefined$` statics): pure functions of the
    /// state too, so memoized in their own tables. Without them, a static whose condition counts
    /// permanents ("as long as you control eight or more permanents named Phoenix Fleet
    /// Airship") recomputed every counted permanent's characteristics for every such permanent:
    /// O(N²) characteristic computations per scope.
    full_red: Vec<(u64, Chars)>,
    base_red: Vec<(u64, Chars)>,
    /// `defined_statics` conditions of each source: (epoch, evaluated bits, value bits) by
    /// static index (< 64; others aren't memoized). Evaluated with `IN_DEFINED` set, always.
    dcond: Vec<(u64, u64, u64)>,
    /// `memo_count` results: (epoch, (zone, you, base_only, IN_DEFINED), filter address, filter,
    /// count): matched on the address and the value (an address can be reused within an epoch).
    /// Entries of older epochs are dropped when a new one is added.
    #[allow(clippy::type_complexity)]
    counts: Vec<(u64, (u8, PlayerId, bool, bool), usize, crate::filter::Filter, i32)>,
    /// Facts that don't depend on the object asked about, built on first use in a scope.
    /// Battlefield static abilities `chars` applies to other objects (layer-6 grants and
    /// `Continuous`): (source, index in its front face's statics), in battlefield then ability
    /// order, without sources that lost all abilities. Valid iff `srcs_epoch == epoch`.
    srcs_epoch: u64,
    srcs_building: bool,
    srcs: Vec<(ObjId, u16)>,
    /// `conditions` of `srcs[i]`: 0 = not evaluated yet, 1 = false, 2 = true.
    cond: Vec<u8>,
    /// (attachment, permanent it's attached to) for the battlefield. Valid iff `att_epoch == epoch`.
    att_epoch: u64,
    att: Vec<(ObjId, ObjId)>,
    /// `ext4::lands_become_active` of the state (a scan of the battlefield, asked for every
    /// nonbasic land's characteristics). Valid iff `lb_epoch == epoch`.
    lb_epoch: u64,
    lb: Option<u8>,
    /// `memo_bool` results of this scope: (epoch, key, value).
    bools: Vec<(u64, (u8, u32, u32, u8), bool)>,
    /// The state content the current epoch's entries were computed for, and its DB (instance
    /// id and sizes): a later scope on a state `eq_but_priority` to it with the same DB keeps
    /// the epoch (see `frozen`).
    snap: Option<Box<State>>,
    snap_db: (u64, usize, usize, usize),
}

/// Memoize a pure predicate of `s` under a caller-chosen key while a `frozen` scope for `s` is
/// active (otherwise just evaluates it). `key.0` names the predicate kind; the rest must
/// identify its inputs fully. `f` must be a function of `s`, `db` and the key only.
pub fn memo_bool(s: &State, db: &CardDb, key: (u8, u32, u32, u8), f: impl FnOnce() -> bool) -> bool {
    if IN_DEFINED.with(|f| f.get()) {
        return f();
    }
    let hit = MEMO.with(|m| {
        let m = m.borrow();
        if m.state != s as *const State as usize || m.db != db as *const CardDb as usize {
            return Err(());
        }
        Ok(m.bools.iter().rev().find(|e| e.0 == m.epoch && e.1 == key).map(|e| e.2))
    });
    match hit {
        Err(()) => f(),
        Ok(Some(v)) => v,
        Ok(None) => {
            let v = f();
            MEMO.with(|m| {
                let mut m = m.borrow_mut();
                if m.state == s as *const State as usize {
                    let ep = m.epoch;
                    m.bools.push((ep, key, v));
                }
            });
            v
        }
    }
}

/// `f` is built only from bases and from properties that read the object and
/// `FilterCtx::you` (never the source, X or remembered objects): a count of the objects it
/// matches in a zone depends on the state, `you` and the characteristics function in use
/// (`IN_DEFINED`, `base_only`) only, not on which source asks. Conservative: any other property
/// is evaluated per call.
pub(crate) fn source_free(f: &crate::filter::Filter) -> bool {
    f.alts.iter().all(|a| {
        a.props.iter().all(|p| matches!(p, Prop::YouCtrl | Prop::OppCtrl | Prop::YouOwn | Prop::Token | Prop::NonToken | Prop::Tapped | Prop::Untapped | Prop::Ext(crate::filter_ext::ExtProp::Named(_))))
    })
}

/// `expr::Expr::CountValid` of a `source_free` filter (`zone`:
/// the zone selector) for `you`, memoized for the epoch while a `frozen` scope for `s` is active
/// (otherwise just evaluated). "For each artifact you control" on N tokens, or N copies of
/// Phoenix Fleet Airship each counting the Airships, is then one count per state instead of N.
pub(crate) fn memo_count(s: &State, db: &CardDb, filter: &crate::filter::Filter, zone: u8, you: PlayerId, base_only: bool, f: impl FnOnce() -> i32) -> i32 {
    let (sp, dp) = (s as *const State as usize, db as *const CardDb as usize);
    let k = (zone, you, base_only, IN_DEFINED.with(|f| f.get()));
    // Matched on the address (fast) and the value (an address can be reused within an epoch).
    let addr = filter as *const crate::filter::Filter as usize;
    let hit = MEMO.with(|m| {
        let m = m.borrow();
        if m.state != sp || m.db != dp {
            return Err(());
        }
        Ok(m.counts.iter().find(|e| e.0 == m.epoch && e.1 == k && e.2 == addr && e.3 == *filter).map(|e| e.4))
    });
    match hit {
        Err(()) => f(),
        Ok(Some(v)) => v,
        Ok(None) => {
            let v = f();
            MEMO.with(|m| {
                let mut m = m.borrow_mut();
                if m.state == sp && m.db == dp {
                    let ep = m.epoch;
                    m.counts.retain(|e| e.0 == ep);
                    m.counts.push((ep, k, addr, filter.clone(), v));
                }
            });
            v
        }
    }
}

/// `defined_statics`' conditions of static `si` of `src` (`f`, a pure function of the state:
/// it doesn't depend on the object whose characteristics are computed), memoized for the epoch
/// while a `frozen` scope for `s` is active (otherwise just evaluated).
fn memo_defined_cond(s: &State, db: &CardDb, src: ObjId, si: usize, f: impl FnOnce() -> bool) -> bool {
    let (sp, dp) = (s as *const State as usize, db as *const CardDb as usize);
    if si >= 64 {
        return f();
    }
    let bit = 1u64 << si;
    let hit = MEMO.with(|m| {
        let m = m.borrow();
        if m.state != sp || m.db != dp {
            return Err(());
        }
        Ok(m.dcond.get(src.idx()).filter(|e| e.0 == m.epoch && e.1 & bit != 0).map(|e| e.2 & bit != 0))
    });
    match hit {
        Err(()) => f(),
        Ok(Some(v)) => v,
        Ok(None) => {
            let v = f();
            MEMO.with(|m| {
                let mut m = m.borrow_mut();
                if m.state == sp && m.db == dp {
                    let ep = m.epoch;
                    if let Some(e) = m.dcond.get_mut(src.idx()) {
                        if e.0 != ep {
                            *e = (ep, 0, 0);
                        }
                        e.1 |= bit;
                        if v {
                            e.2 |= bit;
                        }
                    }
                }
            });
            v
        }
    }
}

/// `f()` (a pure function of `s` and `db`: `ext4::lands_become_active`), memoized for the epoch
/// while a `frozen` scope for `s` is active (otherwise just evaluated).
pub(crate) fn memo_lands_become(s: &State, db: &CardDb, f: impl FnOnce() -> Option<u8>) -> Option<u8> {
    let (sp, dp) = (s as *const State as usize, db as *const CardDb as usize);
    let hit = MEMO.with(|m| {
        let m = m.borrow();
        (m.state == sp && m.db == dp && m.lb_epoch == m.epoch).then_some(m.lb)
    });
    if let Some(v) = hit {
        return v;
    }
    let v = f();
    MEMO.with(|m| {
        let mut m = m.borrow_mut();
        if m.state == sp && m.db == dp {
            m.lb_epoch = m.epoch;
            m.lb = v;
        }
    });
    v
}

/// `srcs` of the active memo for `s` (building it if needed), or None when no memo applies.
fn memo_static_sources(s: &State, db: &CardDb) -> Option<SmallVec<[(ObjId, u16); 16]>> {
    if IN_DEFINED.with(|f| f.get()) {
        return None;
    }
    let ready = MEMO.with(|m| {
        let mut m = m.borrow_mut();
        if m.state != s as *const State as usize || m.db != db as *const CardDb as usize {
            return None;
        }
        if m.srcs_epoch == m.epoch {
            return Some(true);
        }
        if m.srcs_building {
            return None; // re-entered while building: compute directly
        }
        m.srcs_building = true;
        Some(false)
    })?;
    if !ready {
        let v = static_sources(s, db);
        MEMO.with(|m| {
            let mut m = m.borrow_mut();
            m.srcs_building = false;
            m.srcs.clear();
            m.srcs.extend(v.iter().copied());
            m.cond.clear();
            m.cond.resize(v.len(), 0);
            m.srcs_epoch = m.epoch;
        });
        return Some(v);
    }
    MEMO.with(|m| Some(m.borrow().srcs.iter().copied().collect()))
}

/// Battlefield static abilities that `chars` applies to other objects (see `Memo::srcs`).
fn static_sources(s: &State, db: &CardDb) -> SmallVec<[(ObjId, u16); 16]> {
    let mut v = SmallVec::new();
    for &src in &s.battlefield {
        let so = s.obj(src);
        let statics = &db.get(so.card).front().statics;
        if statics.is_empty() || (crate::query::may_lose_abilities(s, db) && lost_all_abilities(s, db, src)) {
            continue;
        }
        for (i, st) in statics.iter().enumerate() {
            let relevant = match &st.kind {
                crate::carddb::StaticKind::Ext(crate::static_ext::StaticExt::GrantAbility { .. }) | crate::carddb::StaticKind::Continuous { .. } => st.zones.has(Zone::Battlefield),
                _ => false,
            };
            if relevant {
                v.push((src, i as u16));
            }
        }
    }
    v
}

/// Do the conditions of static source `i` (index into the memo's `srcs`, or None without a
/// memo) hold? Memoized per scope.
fn source_conditions_hold(s: &State, db: &CardDb, i: Option<usize>, src: ObjId, st: &crate::carddb::StaticDef) -> bool {
    if st.conditions.is_empty() {
        return true;
    }
    if let Some(i) = i {
        if let Some(v) = MEMO.with(|m| m.borrow().cond.get(i).copied()).filter(|&v| v != 0) {
            return v == 2;
        }
    }
    let so = s.obj(src);
    let ecx = crate::expr::EvalCtx { s, db, you: so.controller, source: src, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &[], imprinted: &[], base_only: true };
    let r = st.conditions.iter().all(|cd| crate::expr::holds(cd, &ecx));
    if let Some(i) = i {
        MEMO.with(|m| {
            if let Some(slot) = m.borrow_mut().cond.get_mut(i) {
                *slot = 1 + r as u8;
            }
        });
    }
    r
}

/// Battlefield permanents attached to `id` (Equipment, Auras), in battlefield order.
fn attached_to(s: &State, db: &CardDb, id: ObjId) -> SmallVec<[ObjId; 2]> {
    let hit = MEMO.with(|m| {
        let mut m = m.borrow_mut();
        if m.state != s as *const State as usize || m.db != db as *const CardDb as usize {
            return None;
        }
        if m.att_epoch != m.epoch {
            m.att.clear();
            for &src in &s.battlefield {
                if let Some(t) = s.obj(src).attached_to {
                    m.att.push((src, t));
                }
            }
            m.att_epoch = m.epoch;
        }
        Some(m.att.iter().filter(|e| e.1 == id).map(|e| e.0).collect())
    });
    hit.unwrap_or_else(|| s.battlefield.iter().copied().filter(|&src| s.obj(src).attached_to == Some(id)).collect())
}

/// Battlefield size from which `frozen_if_big` installs a memo scope.
pub const BIG_BOARD: usize = 64;

/// `frozen(s, db, f)` when the battlefield has at least `BIG_BOARD` permanents, else just `f()`:
/// for read-only work in mutating code (trigger matching per event) where installing a scope
/// on every call costs more than it saves on normal boards, while on token-explosion boards
/// characteristics that count the battlefield make every uncached query O(N) or worse. The
/// memo is exact, so the result is the same either way.
pub fn frozen_if_big<R>(s: &State, db: &CardDb, f: impl FnOnce() -> R) -> R {
    if s.battlefield.len() >= BIG_BOARD {
        frozen(s, db, f)
    } else {
        f()
    }
}

/// Run `f` with `chars`/`chars_base` of `s` memoized (see above). Nested scopes on the same
/// state share the memo; a scope on another state while one is active just runs `f`.
pub fn frozen<R>(s: &State, db: &CardDb, f: impl FnOnce() -> R) -> R {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            MEMO.with(|m| {
                let mut m = m.borrow_mut();
                m.state = 0;
                m.db = 0;
            });
        }
    }
    let (sp, dp) = (s as *const State as usize, db as *const CardDb as usize);
    let install = MEMO.with(|m| {
        let mut m = m.borrow_mut();
        if m.state != 0 {
            return false;
        }
        m.state = sp;
        m.db = dp;
        m.srcs_building = false;
        let dbk = (db.id.get(), db.cards.len(), db.abilities.len(), db.statics_pool.len());
        let m = &mut *m;
        let same = m.snap_db == dbk && m.snap.as_deref().is_some_and(|sn| sn.eq_but_priority(s));
        if !same {
            m.epoch += 1; // u64: never wraps, so a stale stamp never matches
            m.bools.clear();
            match &mut m.snap {
                Some(sn) => sn.copy_from(s),
                None => m.snap = Some(Box::new(s.clone())),
            }
            m.snap_db = dbk;
        }
        let n = s.objs.len();
        if m.full.len() < n {
            m.full.resize(n, (0, Chars::default()));
            m.base.resize(n, (0, Chars::default()));
            m.full_red.resize(n, (0, Chars::default()));
            m.base_red.resize(n, (0, Chars::default()));
            m.dcond.resize(n, (0, 0, 0));
        }
        true
    });
    // `bool::then_some(Guard)` would build the Guard eagerly and, for a nested scope (install
    // false), drop it at once: that cleared the OUTER scope's memo, so every later query in it
    // re-installed (an O(state) comparison) and recomputed characteristics.
    let _g = if install { Some(Guard) } else { None };
    f()
}

/// `frozen` on a scratch state (e.g. a modified copy of the state being observed) from inside
/// another `frozen` scope, or outside one: `f` runs with a memo of its own (a second thread-local
/// memo, swapped in and back), so the current scope's memo and its cross-scope epoch (the
/// snapshot of the state it was computed for) are left exactly as they were. Same exactness
/// argument as `frozen`: the aside memo keeps its own snapshot and epochs. Values computed for `s`
/// never reach the other memo, nor the other way round. Used by `observe` for the characteristics
/// a permanent will have once this turn's "until end of turn" effects end (`observe::EotView`).
pub fn frozen_aside<R>(s: &State, db: &CardDb, f: impl FnOnce() -> R) -> R {
    fn swap() {
        MEMO.with(|m| ASIDE.with(|a| std::mem::swap(&mut *m.borrow_mut(), &mut *a.borrow_mut())));
    }
    struct Back;
    impl Drop for Back {
        fn drop(&mut self) {
            swap();
        }
    }
    // The aside memo is never installed between calls (its scope's guard uninstalled it).
    debug_assert!(ASIDE.with(|a| a.borrow().state == 0));
    swap();
    let _b = Back;
    frozen(s, db, f)
}

#[inline]
fn memo_get(s: &State, db: &CardDb, id: ObjId, full: bool) -> Option<Chars> {
    MEMO.with(|m| {
        let m = m.borrow();
        if m.state != s as *const State as usize || m.db != db as *const CardDb as usize {
            return None;
        }
        let red = IN_DEFINED.with(|f| f.get());
        if red && s.battlefield.len() < BIG_BOARD {
            return None; // reduced values on normal boards: recomputing is cheaper than storing
        }
        let v = match (full, red) {
            (true, false) => &m.full,
            (false, false) => &m.base,
            (true, true) => &m.full_red,
            (false, true) => &m.base_red,
        };
        v.get(id.idx()).filter(|e| e.0 == m.epoch).map(|e| e.1.clone())
    })
}

#[inline]
fn memo_put(s: &State, db: &CardDb, id: ObjId, full: bool, c: &Chars) {
    MEMO.with(|m| {
        let mut m = m.borrow_mut();
        if m.state != s as *const State as usize || m.db != db as *const CardDb as usize {
            return;
        }
        let red = IN_DEFINED.with(|f| f.get());
        if red && s.battlefield.len() < BIG_BOARD {
            return;
        }
        let ep = m.epoch;
        let v = match (full, red) {
            (true, false) => &mut m.full,
            (false, false) => &mut m.base,
            (true, true) => &mut m.full_red,
            (false, true) => &mut m.base_red,
        };
        if let Some(slot) = v.get_mut(id.idx()) {
            *slot = (ep, c.clone());
        }
    })
}

#[inline]
/// No memo scope is installed (on any state).
fn memo_idle() -> bool {
    MEMO.with(|m| m.borrow().state == 0)
}

fn memo_active(s: &State) -> bool {
    MEMO.with(|m| m.borrow().state == s as *const State as usize)
}

/// Layers 1–5. `defined`: candidate `AffectedDefined$` statics (`defined_statics`, timestamp
/// order); the layer-4 parts of those that apply are applied among the layer-4 effects in
/// timestamp order, and those statics are pushed to `applied`.
fn chars_base_l15<'a>(s: &State, db: &CardDb, id: ObjId, defined: &[(u32, &'a crate::carddb::DefinedCont, ObjId)], applied: &mut DefinedList<'a>) -> Chars {
    let o = s.obj(id);
    // CR 406.3a: a card exiled face down has no characteristics. (A face-down permanent has
    // its face-down card's, CR 708.2a: `ext6`.)
    if o.face_down() && o.zone == Zone::Exile {
        return Chars::default();
    }
    // A Room outside the battlefield and stack has both halves' characteristics (CR 709.4).
    let cd = db.get(o.card);
    let f = if matches!(cd.layout, crate::carddb::Layout::Room | crate::carddb::Layout::Split) && !matches!(o.zone, Zone::Battlefield | Zone::Stack) { cd.face(2) } else { cd.face(o.face) };
    let mut c = Chars {
        types: f.types,
        supertypes: f.supertypes,
        subtypes: f.subtypes.iter().copied().collect(),
        colors: f.colors,
        power: f.power.unwrap_or(0),
        toughness: f.toughness.unwrap_or(0),
        kws: f.kws,
        mv: f.cost.mana_value(),
        abilities: f.activated.iter().copied().collect(),
        lost_abilities: false,
        // Changeling is a characteristic-defining ability (CR 702.73a, 604.3): layer 4 first
        // (613.3), so later effects that remove creature types override it.
        all_creature_types: f.extra.changeling,
    };
    // Layer 1: a copy effect's exceptions (CR 707.9b); the copied values come from `o.card`.
    if !s.copy_effects.is_empty() {
        if let Some(ce) = s.copy_effects.iter().find(|ce| ce.obj == id) {
            c.types = TypeSet(c.types.0 | ce.add_types.0);
            for &st in &ce.add_subtypes {
                if !c.subtypes.contains(&st) {
                    c.subtypes.push(st);
                }
            }
            c.kws = c.kws.union(ce.kws);
            c.supertypes.0 |= ce.add_super;
            if let Some((p, t)) = ce.set_pt {
                c.power = p;
                c.toughness = t;
            }
            if ce.transformed {
                c.mv = db.get(ce.printed).front().cost.mana_value(); // CR 712.8e
            }
        }
    }
    crate::altcast::adjust_chars(s, id, f, &mut c);
    if o.zone == Zone::Battlefield {
        if !s.ext.chosen_type.is_empty() {
            crate::static_ext::self_land_type(s, db, id, &mut c);
        }
        // "Nonbasic lands are Mountains" (Zhao; CR 305.7, layers 4 and 6).
        if db.mr.lands_become.is_some() {
            crate::ext4::land_type_override(s, db, id, &mut c);
        }
        // Layers 4–5 (CR 613.1d-e) from one timestamp-ordered list (`Layered`, 613.7): the
        // resolved effects on the object and the layer-4 parts of its own and its attachments'
        // `AffectedDefined$` statics: Kaito's "he's a 3/4 Ninja creature" (his timestamp,
        // 613.7a) before a later "becomes a Hero" (205.1a: the later subtype-setting effect
        // wins). Whether such a static applies is checked as it starts to apply
        // (its `Affected$` filter on the object as the earlier effects left it, 613.6).
        let parts = layered_l45(s, id, defined);
        for p in &parts {
            match p.part {
                Part::Effect(e) => {
                    if e.remove_card_types {
                        c.types = TypeSet::default();
                        // CR 205.3d: an object can't have subtypes of types it doesn't have, so
                        // becoming a noncreature (Enduring Innocence: "It's an enchantment")
                        // loses creature types.
                        if !e.add_types.has(CardType::Creature) && !e.add_types.has(CardType::Kindred) {
                            c.subtypes.retain(|st| !db.creature_types.contains(st));
                            c.all_creature_types = false;
                        }
                    }
                    if e.remove_creature_types {
                        let creature_types = &db.creature_types;
                        c.subtypes.retain(|st| !creature_types.contains(st));
                        c.all_creature_types = false;
                    }
                    c.types = TypeSet(c.types.0 | e.add_types.0);
                    c.all_creature_types |= e.all_creature_types;
                    for &st in &e.add_subtypes {
                        if !c.subtypes.contains(&st) {
                            c.subtypes.push(st);
                        }
                    }
                }
                Part::Defined(dc, src) => {
                    if !defined_affects(s, db, dc, src, id, &c) {
                        continue;
                    }
                    if dc.remove_card_types {
                        c.types = TypeSet::default();
                    }
                    c.types = TypeSet(c.types.0 | dc.add_types.0);
                    for &st in &dc.add_subtypes {
                        if !c.subtypes.contains(&st) {
                            c.subtypes.push(st);
                        }
                    }
                    applied.push((p.ts, dc, src));
                }
                Part::Static(..) => unreachable!("no layer 4-5 part"),
            }
        }
        // "Creatures you control but don't own are [type]" (ext8) after the other layer-4
        // effects: it depends on any of them that makes the object a creature (613.8a(c)), and a
        // later creature-type removal still removes it (its own timestamp check).
        if db.x8.stolen_subtype {
            crate::ext8::stolen_subtype(s, db, id, &mut c);
        }
        // Layer 5, same list.
        for p in &parts {
            if let Part::Effect(ContEffect { colors: Some(cs), overwrite_colors, .. }) = p.part {
                c.colors = if *overwrite_colors { *cs } else { ColorSet(c.colors.0 | cs.0) };
            }
        }
    }
    c
}

// ------------------------------------------------------------------ the layer list
//
// Every continuous effect applying to one object, from every source, as one list of
// (timestamp, part) sorted by timestamp (CR 613.7), which each layer walks in order. Nothing
// depends on the order of the vectors the parts come from (`State::effects`,
// `State::effect_objs`, the battlefield): a test shuffles them and checks that
// characteristics don't change. The few adjustments outside this list apply at fixed points,
// each for a stated reason: the copy effect's exceptions (layer 1, 707.9b), the object's own
// impending / chosen land type / "nonbasic lands are [type]" (`adjust_chars`,
// `static_ext::self_land_type`, `ext4::land_type_override`: the first layer-4 effects),
// the stolen subtype (dependency, above), the crafted abilities and keyword
// counters (layer 6, their own rules), characteristic-defining P/T (7a) and counters (7c).

/// One continuous effect's contribution to an object's characteristics.
#[derive(Clone, Copy)]
enum Part<'s, 'd> {
    /// A resolved continuous effect (`State::effects`).
    Effect(&'s ContEffect),
    /// A `Continuous` static of a battlefield permanent or of an effect object (its source).
    Static(&'d crate::carddb::StaticKind, ObjId),
    /// An `AffectedDefined$` static of the object or of a permanent attached to it (source).
    Defined(&'d crate::carddb::DefinedCont, ObjId),
}

impl Part<'_, '_> {
    /// Order among parts with the same timestamp (only parts of one source share one: the
    /// layer-6 and 7c halves of one pump, two statics of one object, which keep their order).
    fn rank(&self) -> u8 {
        match self {
            Part::Effect(_) => 0,
            Part::Static(..) => 1,
            Part::Defined(..) => 2,
        }
    }
}

#[derive(Clone, Copy)]
struct Layered<'s, 'd> {
    ts: u32,
    part: Part<'s, 'd>,
}

type LayerList<'s, 'd> = SmallVec<[Layered<'s, 'd>; 6]>;

/// Timestamp order (CR 613.7), stable within a timestamp.
fn sort_layered(v: &mut LayerList<'_, '_>) {
    v.sort_by_key(|x| (x.ts, x.part.rank()));
}

/// Does a resolved effect have a layer-4 or layer-5 part?
fn effect_l45(e: &ContEffect) -> bool {
    e.remove_card_types || e.remove_creature_types || e.all_creature_types || e.add_types != TypeSet::default() || !e.add_subtypes.is_empty() || e.colors.is_some()
}

/// The layer 4–5 list of `id`: its resolved effects with a layer 4–5 part and the candidate
/// `AffectedDefined$` statics with a layer-4 part.
/// Hot: layers 1–5 run once per static source whose filter looks at the object, so the common
/// case (nothing in layers 4–5) returns before building a list.
#[inline]
fn layered_l45<'s, 'd>(s: &'s State, id: ObjId, defined: &[(u32, &'d crate::carddb::DefinedCont, ObjId)]) -> LayerList<'s, 'd> {
    if !defined.iter().any(|d| defined_l4(d.1)) && !s.effects.iter().any(|e| e.affected == id && effect_l45(e)) {
        return SmallVec::new();
    }
    let mut v: LayerList<'s, 'd> = s.effects.iter().filter(|e| e.affected == id && effect_l45(e)).map(|e| Layered { ts: e.ts, part: Part::Effect(e) }).collect();
    v.extend(defined.iter().filter(|d| defined_l4(d.1)).map(|&(ts, dc, src)| Layered { ts, part: Part::Defined(dc, src) }));
    sort_layered(&mut v);
    v
}

/// Characteristics of an object, applying layers in order (CR 613.1), each layer's effects in
/// timestamp order from one list (`Layered`): layers 1–5 (`chars_base`), 6 (abilities, keywords,
/// ability loss), 7a–7c (P/T: defining, setting, modifications and counters). Static abilities'
/// affected-sets and conditions are evaluated with layer 1–5 characteristics (613.8
/// dependencies on later layers don't arise among the statics we support yet).
pub fn chars(s: &State, db: &CardDb, id: ObjId) -> Chars {
    // Outside any memo scope on a big board: one scope for this query (`frozen`; exact). Its
    // epoch continues across calls on unchanged state, so scans in mutating code (an effect's
    // "all permanents that ..." on a board whose characteristics count the battlefield)
    // share their values instead of recounting per object.
    if s.battlefield.len() >= BIG_BOARD && memo_idle() {
        return frozen(s, db, || chars(s, db, id));
    }
    if memo_active(s) {
        if let Some(c) = memo_get(s, db, id, true) {
            return c;
        }
        let c = chars_uncached(s, db, id);
        memo_put(s, db, id, true, &c);
        return c;
    }
    chars_uncached(s, db, id)
}

fn chars_uncached(s: &State, db: &CardDb, id: ObjId) -> Chars {
    COMPUTED.with(|n| n.set(n.get() + 1));
    // A permanent that left the battlefield: as it last existed there (CR 608.2h, `lki`).
    if let Some(l) = crate::lki::of(s, id) {
        return l.chars.clone();
    }
    let (mut c, defined) = chars_base_defined(s, db, id);
    let o = s.obj(id);
    if o.face_down() && o.zone == Zone::Exile {
        return c; // CR 406.3a
    }
    // Layer 7a: characteristic-defining P/T (CR 604.3: in all zones; 613.4a). Evaluated with
    // layer 1–5 characteristics (no recursion through other objects' P/T). A copy effect's own
    // P/T exception replaces it (CR 707.9d). Every compiled CDA defines only P/T (`cda_pt`), so
    // it starts applying in 7a: if the object lost all abilities in layer 6, it doesn't apply at
    // all (613.6) and the printed */.. values stand with * = 0 (Merfolk Trickster ruling,
    // 2018-04-27). Applied below, after layer 6, on the battlefield.
    let cda: Option<(i32, i32)> = db.get(o.card).face(o.face).cda_pt.as_ref().filter(|_| !s.copy_effects.iter().any(|ce| ce.obj == id && ce.set_pt.is_some())).map(|(pe, te)| {
        let ecx = crate::expr::EvalCtx { s, db, you: o.controller, source: id, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &[], imprinted: &[], base_only: true };
        (crate::expr::eval(pe, &ecx), crate::expr::eval(te, &ecx))
    });
    if o.zone != Zone::Battlefield {
        if let Some((p, t)) = cda {
            c.power = p;
            c.toughness = t;
        }
    }
    if o.zone == Zone::Stack {
        // CR 202.3e: on the stack, X is the number chosen for it, counted in the mana cost of the
        // face it was cast as (715.3b, 709.3a, 722.3: an Adventure's / split half's / prepare
        // spell's own cost, e.g. Elusive Otter's Grove's Bounty {X}{G}), not the front face's.
        // A resolving spell is still on the stack (608.2n: `stack_item_of` covers it).
        if let Some(it) = s.stack_item_of(id) {
            let nx = db.get(o.card).face(o.face).cost.syms.iter().filter(|x| **x == ManaSym::X).count() as i32;
            c.mv = crate::num::add(c.mv as i32, crate::num::mul(nx, it.x)).max(0) as u32;
        }
    }
    if o.zone != Zone::Battlefield {
        return c;
    }
    // Every continuous effect applying to this object in layers 6–7 (`Layered`): resolved
    // effects, other permanents' and effect objects' `Continuous` statics, and the
    // `AffectedDefined$` statics that apply (`defined`); sorted by timestamp below.
    let mut parts: LayerList = s.effects.iter().filter(|e| e.affected == id).map(|e| Layered { ts: e.ts, part: Part::Effect(e) }).collect();
    // (timestamp of the granting effect, ability)
    let mut granted: SmallVec<[(u32, crate::carddb::AbilityId); 1]> = SmallVec::new();
    // Sources come from the scope memo when there is one (same list, same order).
    let (srcs, memoized) = match memo_static_sources(s, db) {
        Some(v) => (v, true),
        None => (static_sources(s, db), false),
    };
    for (i, &(src, si)) in srcs.iter().enumerate() {
        let so = s.obj(src);
        let st = &db.get(so.card).front().statics[si as usize];
        let fcx = FilterCtx { you: so.controller, source: Some(src), x: 0, rem: &[] };
        match &st.kind {
            crate::carddb::StaticKind::Ext(crate::static_ext::StaticExt::GrantAbility { affected, ability }) => {
                // Layer 6: "Lands with the chosen name have '{T}: Add {C}.'" (Petrified Hamlet).
                if obj_matches_base(s, db, affected, id, fcx) {
                    granted.push((so.ts, *ability));
                }
            }
            crate::carddb::StaticKind::Continuous { affected, .. } => {
                if obj_matches_base(s, db, affected, id, fcx) && source_conditions_hold(s, db, memoized.then_some(i), src, st) {
                    parts.push(Layered { ts: so.ts, part: Part::Static(&st.kind, src) });
                }
            }
            _ => unreachable!("static_sources keeps grants and Continuous only"),
        }
    }
    // Layer 6 (CR 613.1f): ability-adding effects, keyword counters and ability-removing effects
    // apply in timestamp order (613.3, 613.7), so a "loses all abilities" effect removes the
    // printed abilities and every ability granted with an earlier timestamp, and abilities granted
    // later apply (613.9 example; rulings on Tishana's Tidebinder / Unable to Scream / Abigale:
    // "If the affected permanent gains an ability after ... it will keep that ability"). No
    // dependency (613.8a) exists between a grant from another object and the loss: neither
    // changes what the other applies to or does. Only the latest loss matters: `loss` is its
    // timestamp, and a grant applies iff its timestamp is later (`kept`).
    let mut loss: Option<u32> = None;
    for eo in &s.effect_objs {
        for &si in &eo.statics {
            // effect-object static reader: Continuous, RemoveAllAbilities (`static_ext::applies_from_effect_object`)
            let st = &db.statics_pool[si as usize];
            let fcx = eo_fcx!(eo);
            match &st.kind {
                crate::carddb::StaticKind::Continuous { affected, .. } if obj_matches_base(s, db, affected, id, fcx) => parts.push(Layered { ts: eo.ts, part: Part::Static(&st.kind, eo.source) }),
                crate::carddb::StaticKind::RemoveAllAbilities { affected } if obj_matches_base(s, db, affected, id, fcx) => loss = loss.max(Some(eo.ts)),
                _ => {}
            }
        }
    }
    parts.extend(defined.iter().map(|&(ts, dc, src)| Layered { ts, part: Part::Defined(dc, src) }));
    sort_layered(&mut parts);
    let kept = |ts: u32| loss.is_none_or(|l| ts > l);
    // The crafted-card abilities are the object's own (its static ability, CR 702.167): a loss
    // removes them by dependency (613.8a(b)), whatever the timestamps.
    if !s.ext.x6.crafted.is_empty() {
        crate::ext6::gained_from_crafted(s, db, id, &mut c.abilities);
    }
    if loss.is_some() {
        c.kws = KwSet::default();
        c.abilities.clear();
        c.lost_abilities = true;
    }
    let ev = |e: &crate::expr::Expr, src: ObjId| {
        let ecx = crate::expr::EvalCtx { s, db, you: s.obj(src).controller, source: src, x: 0, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: &[], imprinted: &[], base_only: true };
        crate::expr::eval(e, &ecx)
    };
    // `AffectedDefined$` statics: an Equipment/Aura's has the timestamp it got when it became
    // attached (613.7a, 613.7e). The object's own is one of the abilities the loss removes: once
    // lost it is in `defined` only for a layer 1–5 part (613.6, `defined_statics`), and its
    // layer-6 part doesn't survive the loss.
    let defined_kept = |ts: u32, src: ObjId| if src == id { loss.is_none() } else { kept(ts) };
    // Keywords: a set, so only which parts survive the loss matters.
    for p in &parts {
        match p.part {
            Part::Effect(e) if e.layer == Layer::L6 && kept(p.ts) => c.kws = c.kws.union(e.kws),
            Part::Static(crate::carddb::StaticKind::Continuous { kws, .. }, _) if kept(p.ts) => c.kws = c.kws.union(*kws),
            Part::Defined(dc, src) if defined_kept(p.ts, src) => c.kws = c.kws.union(dc.kws),
            _ => {}
        }
    }
    // Activated abilities, listed as printed, crafted, other permanents' grants (battlefield
    // order), then the `AffectedDefined$` statics' and the resolved effects' (each in timestamp
    // order). The list order is only the order options are offered in (rules-wise a set).
    c.abilities.extend(granted.iter().filter(|g| kept(g.0)).map(|g| g.1));
    for p in &parts {
        if let Part::Defined(dc, src) = p.part {
            if defined_kept(p.ts, src) {
                c.abilities.extend(dc.abilities.iter().copied());
            }
        }
    }
    for p in &parts {
        if let Part::Effect(e) = p.part {
            if kept(p.ts) {
                c.abilities.extend(e.abilities.iter().copied());
            }
        }
    }
    // CR 122.1b: keyword counters (e.g. a flying counter) grant that keyword; each kind has the
    // timestamp of the last counter of that kind put on the object (613.7c, `kw_counter_ts`).
    if !o.counters.is_empty() {
        for &(k, _) in &o.counters {
            if let CounterKind::Named(i) = k {
                if let Some(kw) = db.counter_names.get(i as usize).and_then(|n| crate::carddb::Kw::parse(n)) {
                    if loss.is_none() || kept(crate::ext6::kw_counter_ts(s, id, k)) {
                        c.kws = c.kws.with(kw);
                    }
                }
            }
        }
    }
    // Layer 7a (see above): only if the CDA survived layer 6.
    if let Some((p, t)) = cda.filter(|_| !c.lost_abilities) {
        c.power = p;
        c.toughness = t;
    }
    // Layer 7b, in timestamp order (CR 613.4b, 613.7): resolved effects, other permanents'
    // statics and the `AffectedDefined$` statics together, so a later "has base power and
    // toughness 1/1" (Secret Identity) wins over Kaito's own earlier "he's a 3/4"
    // (it isn't a characteristic-defining ability, 604.3a). `None`: that value isn't set ("base
    // power becomes X", Animate with only Power$).
    for p in &parts {
        let (pw, th) = match p.part {
            Part::Effect(e) => match e.set_pt {
                Some((pw, th)) => (Some(pw).filter(|&v| v != i32::MIN), Some(th).filter(|&v| v != i32::MIN)),
                None => continue,
            },
            Part::Static(crate::carddb::StaticKind::Continuous { set_power, set_toughness, .. }, src) => (set_power.as_ref().map(|x| ev(x, src)), set_toughness.as_ref().map(|x| ev(x, src))),
            Part::Static(..) => continue,
            Part::Defined(dc, src) => (dc.set_power.as_ref().map(|x| ev(x, src)), dc.set_toughness.as_ref().map(|x| ev(x, src))),
        };
        if let Some(pw) = pw {
            c.power = pw;
        }
        if let Some(th) = th {
            c.toughness = th;
        }
    }
    // Layer 7c, in timestamp order (the additions commute while values stay within the engine's
    // number bound, `num`; past it the unit fails anyway).
    for p in &parts {
        let (pw, th) = match p.part {
            Part::Effect(e) if e.layer == Layer::L7c => (Some(e.pt.0), Some(e.pt.1)),
            Part::Effect(_) => continue,
            Part::Static(crate::carddb::StaticKind::Continuous { power, toughness, .. }, src) => (power.as_ref().map(|x| ev(x, src)), toughness.as_ref().map(|x| ev(x, src))),
            Part::Static(..) => continue,
            Part::Defined(dc, src) => (dc.power.as_ref().map(|x| ev(x, src)), dc.toughness.as_ref().map(|x| ev(x, src))),
        };
        if let Some(pw) = pw {
            c.power = crate::num::add(c.power, pw);
        }
        if let Some(th) = th {
            c.toughness = crate::num::add(c.toughness, th);
        }
    }
    let d = crate::num::sub(o.counter(CounterKind::P1P1), o.counter(CounterKind::M1M1));
    c.power = crate::num::add(c.power, d);
    c.toughness = crate::num::add(c.toughness, d);
    // CR 122.1j: each hone counter on an Equipment attached to it gives +1/+0.
    if let Some(i) = db.counter_names.iter().position(|n| n == "HONE") {
        for src in attached_to(s, db, id) {
            c.power = crate::num::add(c.power, s.obj(src).counter(CounterKind::Named(i as u16)));
        }
    }
    c
}

/// Is blocking `attacker` with `blocker` forbidden by a static ability (`CantBlockBy`)?
pub fn block_forbidden(s: &State, db: &CardDb, attacker: ObjId, blocker: ObjId) -> bool {
    for eo in &s.effect_objs {
        for &si in &eo.statics {
            // effect-object static reader: CantBlockBy (`static_ext::applies_from_effect_object`)
            if let crate::carddb::StaticKind::CantBlockBy { attacker: af, blocker: bf } = &db.statics_pool[si as usize].kind {
                let fcx = eo_fcx!(eo);
                if obj_matches(s, db, af, attacker, fcx) && bf.as_ref().is_none_or(|f| obj_matches(s, db, f, blocker, fcx)) {
                    return true;
                }
            }
        }
    }
    for &src in &s.battlefield {
        let so = s.obj(src);
        for st in &db.get(so.card).front().statics {
            let crate::carddb::StaticKind::CantBlockBy { attacker: af, blocker: bf } = &st.kind else { continue };
            // CR 613.1f / 113.6: a permanent that lost all abilities has no static abilities.
            if may_lose_abilities(s, db) && lost_all_abilities(s, db, src) {
                continue;
            }
            let fcx = FilterCtx { you: so.controller, source: Some(src), x: 0, rem: &[] };
            if obj_matches(s, db, af, attacker, fcx) && bf.as_ref().is_none_or(|f| obj_matches(s, db, f, blocker, fcx)) {
                return true;
            }
        }
    }
    false
}

/// Is `blocker` forbidden to block every creature by a `CantBlockBy` static (an effect object's or a
/// permanent's) whose attacker filter matches any creature ("that creature can't block this turn")?
/// Then `block_forbidden(a, blocker)` holds for every creature `a`. Same readers as `block_forbidden`.
pub fn cant_block_any(s: &State, db: &CardDb, blocker: ObjId) -> bool {
    use crate::filter::Base;
    let any = |f: &crate::filter::Filter| f.alts.iter().any(|a| a.props.is_empty() && matches!(a.base, Base::Card | Base::Permanent | Base::Type(CardType::Creature)));
    for eo in &s.effect_objs {
        for &si in &eo.statics {
            // effect-object static reader: CantBlockBy (`static_ext::applies_from_effect_object`)
            if let crate::carddb::StaticKind::CantBlockBy { attacker: af, blocker: Some(bf) } = &db.statics_pool[si as usize].kind {
                if any(af) && obj_matches(s, db, bf, blocker, eo_fcx!(eo)) {
                    return true;
                }
            }
        }
    }
    for &src in &s.battlefield {
        let so = s.obj(src);
        for st in &db.get(so.card).front().statics {
            let crate::carddb::StaticKind::CantBlockBy { attacker: af, blocker: Some(bf) } = &st.kind else { continue };
            if !any(af) || (may_lose_abilities(s, db) && lost_all_abilities(s, db, src)) {
                continue;
            }
            if obj_matches(s, db, bf, blocker, FilterCtx { you: so.controller, source: Some(src), x: 0, rem: &[] }) {
                return true;
            }
        }
    }
    false
}

/// Lore counters on an object (CR 714.3).
pub fn lore(s: &State, db: &CardDb, id: ObjId) -> i32 {
    match db.counter_names.iter().position(|n| n == "LORE") {
        Some(i) => s.obj(id).counter(CounterKind::Named(i as u16)),
        None => 0,
    }
}

/// A Saga card's final chapter number (CR 714.2d) from its printed chapter abilities; 0 if it
/// has none. For a permanent use `final_chapter_of`.
pub fn final_chapter(db: &CardDb, card: CardId) -> u8 {
    db.get(card).front().triggers.iter().filter_map(|t| if let crate::carddb::TrigMode::Chapter { n } = t.mode { Some(n) } else { None }).max().unwrap_or(0)
}

/// The final chapter number of the Saga permanent `o` as it currently is (CR 714.2d): 0 once it
/// has no chapter abilities, e.g. it lost all abilities (613.1f; The Wondrous Wasp, Tishana's
/// Tidebinder). The sacrifice SBA (704.5s, 714.4) and the precombat-main lore counter (714.3c)
/// apply only to a Saga "with one or more chapter abilities".
pub fn final_chapter_of(s: &State, db: &CardDb, o: ObjId) -> u8 {
    let fin = final_chapter(db, s.obj(o).card);
    if fin > 0 && may_lose_abilities(s, db) && lost_all_abilities(s, db, o) {
        return 0;
    }
    fin
}

/// The card whose name an object has (a copy effect may give it another name, CR 707.9b).
pub fn name_card(s: &State, id: ObjId) -> CardId {
    name_override(s, id).unwrap_or(s.obj(id).card)
}

/// The card whose name a copy has instead of the copied card's (a copy effect's name exception,
/// CR 707.9b), if any.
pub fn name_override(s: &State, id: ObjId) -> Option<CardId> {
    if s.copy_effects.is_empty() {
        return None;
    }
    s.copy_effects.iter().find(|c| c.obj == id).and_then(|c| c.name).filter(|&n| n != s.obj(id).card)
}

/// The names an object has (CR 201.2), for "same name" / "the chosen name" / "named X" checks:
/// - a copy's name exception (CR 707.9b, part of its copiable values; 707.2), also for the old
///   object of a permanent that left the battlefield (its last known information, 608.2h);
/// - a face-down permanent (or its last known information) has no name (708.2a);
/// - on the stack / battlefield (or as last known information) only the face that's up (712.8d-f,
///   715.3b, 715.4); a Room permanent has the names of its unlocked halves, none if both are
///   locked (709.5); elsewhere a split card has both halves' names (709.4, 709.4a).
pub fn names<'a>(s: &State, db: &'a CardDb, id: ObjId) -> SmallVec<[&'a str; 2]> {
    let o = s.obj(id);
    let mut out: SmallVec<[&'a str; 2]> = SmallVec::new();
    if (o.zone == Zone::Battlefield && o.face_down()) || db.x6.face_down_card == Some(o.card) {
        return out;
    }
    let lki = crate::lki::of(s, id);
    let over = name_override(s, id).or_else(|| lki.and_then(|l| l.copy.as_ref()).and_then(|c| c.name).filter(|&n| n != o.card));
    if let Some(n) = over {
        out.push(db.get(n).name.as_str());
        return out;
    }
    let cd = db.get(o.card);
    let up = matches!(o.zone, Zone::Stack | Zone::Battlefield) || lki.is_some();
    if !up && matches!(cd.layout, crate::carddb::Layout::Split | crate::carddb::Layout::Room) && cd.faces.len() >= 2 {
        out.push(cd.faces[0].name.as_str());
        out.push(cd.faces[1].name.as_str());
        return out;
    }
    if crate::ext5::room_of(db, o.card).is_some_and(|r| r[3] == o.card) {
        return out; // CR 709.5: both doors locked, no name
    }
    let full = if up && o.face != 0 { cd.face(o.face).name.as_str() } else { cd.name.as_str() };
    // Without a '/' the split is `full` itself (skips building a substring searcher per call).
    if !full.as_bytes().contains(&b'/') {
        if !full.is_empty() {
            out.push(full);
        }
        return out;
    }
    out.extend(full.split(" // ").filter(|n| !n.is_empty()));
    out
}

/// CR 201.2a: two objects have the same name if they have at least one name in common; an
/// object with no name doesn't have the same name as any other object.
pub fn same_name(s: &State, db: &CardDb, a: ObjId, b: ObjId) -> bool {
    let na = names(s, db, a);
    !na.is_empty() && names(s, db, b).iter().any(|n| na.contains(n))
}

/// The object has the name `name` (one of its names, CR 709.4a; "the chosen name", 201.3).
pub fn has_name(s: &State, db: &CardDb, id: ObjId, name: &str) -> bool {
    names(s, db, id).iter().any(|n| crate::carddb::name_key_eq(name, n))
}

pub fn has_kw(s: &State, db: &CardDb, id: ObjId, k: Kw) -> bool {
    chars(s, db, id).kws.has(k)
}

/// Can `o`, an object in a graveyard, hand, library or exile, be chosen as "a card" there? A
/// token isn't a card (CR 108.2, 111.1): one that left the battlefield stays in its new zone only
/// until state-based actions are next checked (111.7, 704.5d), e.g. while the spell that
/// destroyed it finishes resolving, and choices of cards from that zone never offer it
/// (Deadly Cover-Up offered a Soldier token it had just destroyed). Moving it would do
/// nothing anyway (111.8, `zones::move_batch`).
#[inline]
pub fn is_card(s: &State, o: ObjId) -> bool {
    !s.obj(o).token
}

/// Context for evaluating a filter: who "you" is and which object is "self".
#[derive(Copy, Clone, Debug)]
pub struct FilterCtx<'a> {
    pub you: PlayerId,
    pub source: Option<ObjId>,
    /// X of the resolving spell/ability, for filters like `cmcLEX`.
    pub x: i32,
    /// Objects remembered by the evaluating effect/ability (`IsRemembered`).
    pub rem: &'a [ObjId],
}

pub fn obj_matches(s: &State, db: &CardDb, f: &Filter, id: ObjId, cx: FilterCtx) -> bool {
    if all_alts_rejected_early(s, db, f, id, cx) {
        return false;
    }
    let c = chars(s, db, id);
    f.alts.iter().any(|a| alt_matches_obj(s, db, a, id, &c, cx))
}

/// Like `obj_matches` but with layer 1–5 characteristics (see `chars`).
pub fn obj_matches_base(s: &State, db: &CardDb, f: &Filter, id: ObjId, cx: FilterCtx) -> bool {
    if all_alts_rejected_early(s, db, f, id, cx) {
        return false;
    }
    let c = chars_base(s, db, id);
    f.alts.iter().any(|a| alt_matches_obj(s, db, a, id, &c, cx))
}

/// Every alternative of `f` already fails on its base or on a property that doesn't depend on
/// characteristics (controller, owner, token, tapped, source), so computing the object's
/// characteristics can be skipped. Exactly `alt_matches_obj`'s verdict for those parts (all
/// parts are pure, so evaluation order doesn't matter).
fn all_alts_rejected_early(s: &State, db: &CardDb, f: &Filter, id: ObjId, cx: FilterCtx) -> bool {
    let o = s.obj(id);
    f.alts.iter().all(|a| {
        matches!(a.base, Base::Player | Base::Opponent | Base::You)
            || a.props.iter().any(|p| match p {
                Prop::YouCtrl => o.controller != cx.you,
                Prop::OppCtrl => o.controller == cx.you,
                Prop::YouOwn => o.owner != cx.you,
                Prop::Other => Some(id) == cx.source,
                Prop::SelfCard => Some(id) != cx.source,
                Prop::Token => !o.token,
                Prop::NonToken => o.token,
                Prop::Tapped => !o.tapped,
                Prop::Untapped => o.tapped,
                // Names don't depend on characteristics (`names`: card, face, copy and name
                // overrides): "permanents named X" counts skip the other permanents' chars.
                Prop::Ext(crate::filter_ext::ExtProp::Named(n)) => !has_name(s, db, id, n),
                _ => false,
            })
    })
}

pub fn player_matches(f: &Filter, p: PlayerId, cx: FilterCtx) -> bool {
    f.alts.iter().any(|a| {
        let base_ok = match a.base {
            Base::Any | Base::Player => true,
            Base::Opponent => p != cx.you,
            Base::You => p == cx.you,
            _ => false,
        };
        base_ok
            && a.props.iter().all(|pr| match pr {
                Prop::Other => p != cx.you, // "Player.Other"
                _ => false,
            })
    })
}

fn alt_matches_obj(s: &State, db: &CardDb, a: &Alt, id: ObjId, c: &Chars, cx: FilterCtx) -> bool {
    let o = s.obj(id);
    let base_ok = match a.base {
        Base::Card => true,
        // Forge's `Permanent` type: a permanent, or a permanent card/spell in another zone (CR
        // 110.4a: permanent cards; e.g. "if it's a permanent card" in a library or graveyard).
        Base::Permanent => o.zone == Zone::Battlefield || (o.zone != Zone::Gone && c.types.is_permanent()),
        Base::Type(t) => c.types.has(t),
        Base::Any => {
            o.zone == Zone::Battlefield
                && (c.types.has(CardType::Creature) || c.types.has(CardType::Planeswalker) || c.types.has(CardType::Battle))
        }
        Base::Subtype(st) => c.subtypes.contains(&st) || (c.all_creature_types && db.creature_types.contains(&st)),
        Base::Player | Base::Opponent | Base::You => false,
    };
    base_ok && a.props.iter().all(|p| prop_matches(s, db, p, id, o, c, cx))
}

fn prop_matches(s: &State, db: &CardDb, p: &Prop, id: ObjId, o: &Obj, c: &Chars, cx: FilterCtx) -> bool {
    match p {
        Prop::YouCtrl => o.controller == cx.you,
        Prop::OppCtrl => o.controller != cx.you,
        Prop::YouOwn => o.owner == cx.you,
        Prop::Other => Some(id) != cx.source,
        Prop::SelfCard => Some(id) == cx.source,
        Prop::Token => o.token,
        Prop::NonToken => !o.token,
        Prop::Tapped => o.tapped,
        Prop::Untapped => !o.tapped,
        Prop::Attacking => s.combat.attackers.iter().any(|a| a.0 == id),
        Prop::Blocking => s.combat.blocks.iter().any(|b| b.0 == id),
        Prop::Not(inner) => !prop_matches(s, db, inner, id, o, c, cx),
        Prop::NonType(t) => !c.types.has(*t),
        Prop::NonSubtype(st) => !(c.subtypes.contains(st) || (c.all_creature_types && db.creature_types.contains(st))),
        Prop::Legendary => c.supertypes.legendary(),
        Prop::NonLegendary => !c.supertypes.legendary(),
        Prop::Basic => c.supertypes.basic(),
        Prop::NonBasic => !c.supertypes.basic(),
        Prop::WithKeyword(k) => c.kws.has(*k),
        Prop::WithoutKeyword(k) => !c.kws.has(*k),
        Prop::Color(col) => c.colors.has(*col),
        Prop::NonColor(col) => !c.colors.has(*col),
        Prop::Multicolor => c.colors.count() > 1,
        Prop::Colorless => c.colors.is_empty(),
        Prop::YouDontCtrl => o.controller != cx.you,
        Prop::YouDontOwn => o.owner != cx.you,
        Prop::IsType(t) => c.types.has(*t),
        Prop::IsPermanent => c.types.is_permanent(),
        Prop::ThisTurnEntered => o.entered == s.turn.number,
        Prop::WasCast => o.cast_by.is_some(),
        Prop::WasCastByYou => o.cast_by == Some(cx.you),
        Prop::Counters(k, op, n) => op.eval(o.counter(*k), *n),
        Prop::InZone(z) => o.zone == *z,
        Prop::IsRemembered => cx.rem.contains(&id),
        // A face-down card or permanent has no Adventure (CR 406.3a, 708.2; Memory Theft ruling
        // 2019-10-04: "any face-up exiled card with an Adventure").
        Prop::AdventureCard => !o.face_down() && db.get(o.card).layout == crate::carddb::Layout::Adventure,
        Prop::AnySubtype(v) => v.iter().any(|st| c.subtypes.contains(st)) || (c.all_creature_types && v.iter().any(|st| db.creature_types.contains(st))),
        Prop::Ext(x) => crate::filter_ext::ext_prop_matches(s, db, x, id, o, c, cx),
        Prop::AttachedBySource => cx.source.is_some_and(|src| s.obj(src).attached_to == Some(id)),
        Prop::CmpExpr(stat, op, e) => {
            let v = match stat {
                Stat::Cmc => c.mv as i32,
                Stat::Power => c.power,
                Stat::Toughness => c.toughness,
            };
            let ecx = crate::expr::EvalCtx { s, db, you: cx.you, source: cx.source.unwrap_or(id), x: cx.x, kicked: false, triggered: None, trigger_amount: 0, targets: &[], rem: cx.rem, imprinted: &[], base_only: true };
            op.eval(v, crate::expr::eval(e, &ecx))
        }
        Prop::Cmp(stat, op, n) => {
            let v = match stat {
                Stat::Cmc => c.mv as i32,
                Stat::Power => c.power,
                Stat::Toughness => c.toughness,
            };
            op.eval(v, *n)
        }
    }
}

/// Can `id` be targeted by an ability controlled by `by`? (hexproof, CR 702.11)
pub fn targetable_by(s: &State, db: &CardDb, id: ObjId, by: PlayerId) -> bool {
    let o = s.obj(id);
    !(o.controller != by && has_kw(s, db, id, Kw::Hexproof) && !crate::ext3::ignores_hexproof(s, db, id, by))
}

/// Is any permanent matching `f` present (for `IsPresent$`)?
pub fn is_present(s: &State, db: &CardDb, f: &Filter, cx: FilterCtx) -> bool {
    s.battlefield.iter().any(|&id| obj_matches(s, db, f, id, cx))
}

/// Interchangeability signature of an object (canonical set choices):
/// (card, flags, damage, unique, extra, chosen). Two objects with equal signatures are
/// interchangeable, so a choice among them is one option. Anything about an object that can
/// change what a choice does must therefore be in it: tapped, token, prepared, face, controller,
/// owner and damage directly; summoning sickness, combat role, hand knowledge, entering this turn
/// (for cards that ask, `SIG_ENTERED_NOW`) and (off the
/// battlefield) cast designations and "discarded this turn" in `extra`; the
/// creature type and card name chosen for it as it entered in `chosen` (two Caverns of Souls
/// naming different types differ, naming the same one they don't); everything rarer (counters,
/// effects, attachments, copy effects, face-down, adventure, links and per-object history:
/// `refs_of`) makes the signature unique (`o + 1`).
pub type ObjSig = (u32, u8, i32, u32, u32, u32);

/// `ObjSig.4` bits. A creature that can't attack or {T} this turn (CR 302.6: summoning sick, no
/// haste): a sick and a ready copy are different choices (which one to sacrifice, bounce, ...).
const SIG_SICK: u32 = 1 << 31;
/// An attacking creature (CR 506.3) not blocked (blocked attackers are unique: they differ by
/// their blockers); the low bits are the attacked planeswalker + 1 (0: the player).
const SIG_ATTACKING: u32 = 1 << 30;
/// A creature blocking exactly one attacker (CR 509.1); the low bits are that attacker + 1.
/// Blocking another attacker, or none, makes it a different object to choose (a removal target
/// during combat: killing the blocker leaves the attacker blocked, CR 509.1h).
const SIG_BLOCKING: u32 = 1 << 29;
/// A card in hand known to some player (revealed): bits = which players know it.
const SIG_KNOWN_SHIFT: u32 = 27;
const SIG_KNOWN: u32 = 3 << SIG_KNOWN_SHIFT;
/// A card with mayhem discarded this turn (CR 702.187b): a designation of that card, not a
/// reference to it, so two such twins stay interchangeable; other cards'
/// "discarded this turn" is read by nothing and not in the signature.
const SIG_DISCARDED: u32 = 1 << 26;
/// Combat bits on the battlefield; off the battlefield the low 16 bits are the object's cast
/// designations (`altcast::flag`: warped, plotted, airbent in exile).
const SIG_LOW: u32 = SIG_DISCARDED - 1;
/// On the battlefield (`SIG_DISCARDED`'s bit, used only off it): a permanent that entered this
/// turn, for cards whose own abilities ask (`ext9::X9Tables::self_entered`: Hidden Lair taps for
/// {U} or {B} only the turn it entered; the canonicalisation soundness test merged it with an
/// older Hidden Lair for mana payment). For every other card entering this turn is read only as
/// counts of zone changes (`moved_this_turn`), so a land played this turn stays interchangeable
/// with an older one.
const SIG_ENTERED_NOW: u32 = SIG_DISCARDED;
/// Off the battlefield (above the 16 designation bits): a warped or plotted card exiled this turn.
/// Both are cast "on a later turn" (CR 702.185a, 702.170d), so a twin exiled this turn isn't
/// interchangeable with one exiled earlier (canonicalisation soundness test: the cast offer
/// merged only castable ones, but every other choice among exiled cards merged both).
const SIG_EXILED_NOW: u32 = 1 << 25;

pub fn obj_sig(s: &State, db: &CardDb, o: ObjId) -> ObjSig {
    let mut referenced = false;
    refs_of(s, db, |x| referenced |= x == o);
    obj_sig_with(s, db, o, |o| s.battlefield.iter().any(|&x| s.obj(x).attached_to == Some(o)), |_| referenced)
}

/// Interchangeability key of a card outside the battlefield (hand, graveyard, exile, library,
/// sideboard) for "identical cards are one option" merges: `obj_sig` without hand knowledge.
/// Two cards merge only when nothing refers to one of them specifically (a target of a spell
/// or ability on the stack, the source being paid for, a link, an effect: `refs_of`) and they
/// carry the same designations. Hand knowledge is left out: which copy of
/// a revealed card is played isn't a different choice the engine distinguishes for casting or
/// hand set choices (and the observation doesn't carry the opponent's knowledge of one's own
/// hand); `scenario::interchangeable` uses the same key.
pub fn card_sig(s: &State, db: &CardDb, o: ObjId) -> ObjSig {
    let mut k = obj_sig(s, db, o);
    k.4 &= !SIG_KNOWN;
    k
}

/// `card_sig` of each of `objs` (one `refs_of` scan).
pub fn card_sigs(s: &State, db: &CardDb, objs: &[ObjId]) -> Vec<ObjSig> {
    let mut v = obj_sigs(s, db, objs);
    v.iter_mut().for_each(|k| k.4 &= !SIG_KNOWN);
    v
}

/// Set-choice items for cards outside the battlefield, keyed by `card_sig` (identical,
/// unreferenced cards are one option; `Ctx::choose_set`).
pub fn card_items(s: &State, db: &CardDb, objs: &[ObjId]) -> Vec<(ObjSig, crate::engine::Opt)> {
    card_sigs(s, db, objs).into_iter().zip(objs).map(|(k, &o)| (k, crate::engine::Opt::Card(o))).collect()
}

/// `(obj_sig(o), Opt::Card(o))` for each of `objs`, in order (the items of a set choice among
/// permanents): one `obj_sigs` scan, memoized characteristics on a big board, instead of an
/// `obj_sig` (a whole-state scan) per object. Same values.
pub fn sig_items(s: &State, db: &CardDb, objs: &[ObjId]) -> Vec<(ObjSig, crate::engine::Opt)> {
    frozen_if_big(s, db, || obj_sigs(s, db, objs)).into_iter().zip(objs).map(|(k, &o)| (k, crate::engine::Opt::Card(o))).collect()
}

/// Sort `objs` in canonical (obj_sig, id) order: one `obj_sigs` scan instead of an `obj_sig` (a
/// whole-state scan) per comparison. The same order as `sort_by_key(|&o| (obj_sig(o), o))`.
pub fn sort_by_sig(s: &State, db: &CardDb, objs: &mut Vec<ObjId>) {
    let sigs = frozen_if_big(s, db, || obj_sigs(s, db, objs));
    let mut keyed: Vec<(ObjSig, ObjId)> = sigs.into_iter().zip(objs.iter().copied()).collect();
    keyed.sort_unstable();
    objs.clear();
    objs.extend(keyed.into_iter().map(|k| k.1));
}

/// Indices of `objs` that are the first of their `card_sig` class (in order): the cards a
/// "one option per identical card" loop offers.
pub fn first_of_each_card(s: &State, db: &CardDb, objs: &[ObjId]) -> Vec<bool> {
    // Cheap path: no two share a card, nothing to merge (no `refs_of` scan).
    let dup = objs.iter().enumerate().any(|(i, &x)| objs[..i].iter().any(|&y| s.obj(y).card == s.obj(x).card));
    if !dup {
        return vec![true; objs.len()];
    }
    let sigs = card_sigs(s, db, objs);
    (0..objs.len()).map(|i| !sigs[..i].contains(&sigs[i])).collect()
}

/// `obj_sig` of each of `objs` (same values), with the battlefield's attachments and the
/// per-object references collected once instead of scanned per object: O(battlefield + objs)
/// instead of O(battlefield x objs). Choices over large boards (declaring 1,000+ attackers)
/// call this once per unit run.
pub fn obj_sigs(s: &State, db: &CardDb, objs: &[ObjId]) -> Vec<ObjSig> {
    if objs.len() <= 3 {
        // A few objects: one scan each is cheaper than collecting and sorting the references.
        return objs.iter().map(|&o| obj_sig(s, db, o)).collect();
    }
    let mut hosts: Vec<ObjId> = s.battlefield.iter().filter_map(|&x| s.obj(x).attached_to).collect();
    hosts.sort_unstable();
    let mut refs: Vec<ObjId> = Vec::new();
    refs_of(s, db, |x| refs.push(x));
    refs.sort_unstable();
    objs.iter().map(|&o| obj_sig_with(s, db, o, |o| hosts.binary_search(&o).is_ok(), |o| refs.binary_search(&o).is_ok())).collect()
}

/// Calls `f` with every object that state outside its `Obj` refers to in a way that makes it
/// differ from an otherwise identical object: links (exiled with, X paid),
/// designations (solved, harnessed, class level), per-object history (activations, exhausted
/// abilities, loyalty abilities and triggers used this turn, targeted this turn),
/// pending game objects (stack targets, triggers, delayed triggers, effects' remembered
/// objects, sources of abilities on the stack), and the targets of the spell or ability whose
/// costs are being paid and the source of an ability being activated (its own cost choices must
/// not merge a target or the source with an identical object). Exhaustive
/// destructuring: a new field must be classified here to compile. Left out on purpose: other
/// state of the resolution or choice in progress (it refers to the objects being chosen among), objects that no longer exist (last known information, spells as they
/// were cast), and zone-change history (`moved_this_turn`: a land played this turn is
/// interchangeable with an older one).
/// The objects a trigger's event context refers to (`refs_of`).
fn trig_refs(t: &TrigCtx, f: &mut impl FnMut(ObjId)) {
    let TrigCtx {
        obj,
        new_obj,
        player: _,
        amount: _,
        stack_item: _, // a stack uid, not an object
        // "other than that creature": read only while the trigger is put on the stack, when
        // `sba::put_triggers_on_stack` refers to it (`ext.x6.paying_for`)
        target: _,
        gain_source: _, // names the instance only (see `TrigCtx::gain_source`)
    } = t;
    obj.iter().chain(new_obj).for_each(|&x| f(x));
}

fn refs_of(s: &State, db: &CardDb, mut f: impl FnMut(ObjId)) {
    let crate::ext::Ext {
        chosen_type: _, // in the signature (`ObjSig.5`)
        named: _,       // in the signature (`ObjSig.5`)
        chosen_cards: _,
        uncounterable,
        exiled_with,
        res_targets: _,
        delayed,
        cast_this_turn: _,
        moved_this_turn: _,
        spent_colors: _,
        activations,
        sacrificed_this_turn: _,
        x_paid,
        rep_player: _,
        solved,
        searched: _,
        pool: _,
        speed: _,
        speed_turn: _,
        enduring_story: _,
        harnessed,
        equips_this_turn: _,
        free_activations,
        enter_with: _,
        enter_with_node: _, // only set while a node resolves
        cost_sacrificed: _,
        res_remembered: _,
        batch: _,
        exhausted,
        granted_flashback,
        divided: _,
        crimes: _,
        lt,
        ordering: _,
        mr,
        le,
        x6,
        x7,
        x9: _, // firebending mana amounts: no objects
        lki: _, // objects that no longer exist (last known information)
        lifelink_held: _, // only set while damage is being dealt (`resolve::hold_lifelink`)
        stack_uids: _,    // a counter
        adventure_player: _, // cards on an adventure are never merged (`ObjSig`: `on_adventure`)
        unless_due: _,       // only set while a PayUnless decision is pending (no objects)
    } = &s.ext;
    let crate::ext3::LtState { dies_exile, face_down_next: _, next_uncounterable: _, sneak: _ } = lt;
    let crate::ext4::MrState { targeted, warped: _, attacked: _, must_attack, warped_by: _ } = mr;
    let crate::ext5::LeState { cleanup_again: _, attached_lki: _, draws: _, batch: _, delayed_rem, cost_tapped } = le;
    let crate::ext6::XState {
        storm: _,
        // restricted pool mana remembers the mana ability's source (`ext6::Restr::Ability`): its
        // restriction is evaluated with that source (Cavern of Souls' chosen type), which may leave
        // or change; the source is not its twin while the mana is in the pool (soundness test:
        // targeting the Tablet of Discovery whose mana is floating vs the other one)
        rpool,
        face_down: _,
        face_down_next: _,
        // per (source, ability) count read only by `Count$ResolvedThisTurn` as the ability
        // resolves: every triggered ability is recorded, so only the sources of cards that read
        // it are referred to (`ext9::X9Tables::resolved_readers`: South Pole Voyager's "if this
        // is the second time this ability has resolved this turn"; canonicalisation soundness
        // test). `resolving_count` is set as each one resolves, before it is read.
        resolved,
        resolving_count: _,
        counter_seen: _,
        levels,
        discards: _,
        discarded_this_turn: _, // in the signature (`SIG_DISCARDED`)
        must_attack: must_attack6,
        granted,
        // only objects with counters are listed, and those are never interchangeable (`modified`)
        kw_counter_ts: _,
        paying_for,
        craft_pending,
        crafted,
        gained_used,
        activated: _,
        next_activated: _,
        skip_turns: _,
        // the targets chosen for the earlier targeted parts of the spell or ability whose
        // targets are being chosen (CR 601.2c, 115.3: one object may be the target of several
        // target words): targeting it again differs from targeting its twin (Jeskai Revelation
        // returning a Monk token and dealing 4 to it or to the other token; soundness test).
        // `pick.chosen` is the set choice in progress, in canonical order already.
        pick,
        full_mana: _,
        mana_unfiltered: _,
        no_priority_mana: _,
    } = x6;
    uncounterable.iter().for_each(|&x| f(x));
    for e in exiled_with.all_links() {
        f(e.obj);
        f(e.source);
    }
    // Exhaustive (here and for stack items, pending triggers and trigger contexts below): a new
    // field naming an object must be classified to compile.
    for d in delayed {
        let crate::ext::DelayedTrig { when, controller: _, source, source_card: _, execute: _, remembered } = d;
        f(*source);
        remembered.iter().for_each(|&x| f(x));
        match *when {
            crate::ext::DelayedWhen::Leaves { obj, to: _ } | crate::ext::DelayedWhen::DiesWithin { obj, turn: _ } => f(obj),
            crate::ext::DelayedWhen::NextStep { step: _ } | crate::ext::DelayedWhen::PlayersNextStep { step: _, player: _ } => {}
        }
    }
    activations.iter().for_each(|x| f(x.0));
    // `ext7`: modes a source chose this turn ("not chosen this turn"); `paradigm` names only
    // spell objects that no longer exist.
    let crate::ext7::X7State { paradigm: _, modes_used, player_hexproof: _ } = x7;
    modes_used.iter().for_each(|x| f(x.0));
    x_paid.iter().for_each(|x| f(x.0));
    solved.iter().for_each(|&x| f(x));
    harnessed.iter().for_each(|&x| f(x));
    free_activations.iter().for_each(|x| f(x.0));
    exhausted.iter().for_each(|x| f(x.0));
    granted_flashback.iter().for_each(|&x| f(x));
    dies_exile.iter().for_each(|&x| f(x));
    // Valiant (first time each turn, `ext4`): only while some permanent has such a trigger;
    // every targeted object is recorded.
    let valiant = || {
        s.battlefield.iter().any(|&x| {
            let ob = s.obj(x);
            db.get(ob.card).face(ob.face).triggers.iter().any(|t| matches!(t.mode, crate::carddb::TrigMode::Ext(crate::trig_ext::TrigExt::Valiant { .. })))
        })
    };
    if !targeted.is_empty() && valiant() {
        targeted.iter().for_each(|x| f(x.0));
    }
    must_attack.iter().for_each(|x| f(x.0));
    for (src, _, rem) in delayed_rem {
        f(*src);
        rem.iter().for_each(|&x| f(x));
    }
    for &(src, c) in cost_tapped {
        f(src);
        f(c);
    }
    levels.iter().for_each(|x| f(x.0));
    must_attack6.iter().for_each(|x| f(x.0));
    granted.iter().for_each(|x| f(x.0));
    paying_for.iter().for_each(|&x| f(x));
    for r in rpool {
        if let crate::ext6::Restr::Ability(_, src) = r.3 {
            f(src);
        }
    }
    pick.earlier.iter().for_each(|t| {
        if let crate::state::Target::Obj(x) = t {
            f(*x)
        }
    });
    if !db.x9.resolved_readers.is_empty() {
        resolved.iter().filter(|r| s.objs.get(r.0.idx()).is_some_and(|ob| db.x9.resolved_readers.contains(&ob.card))).for_each(|r| f(r.0));
    }
    for (c, mats) in craft_pending.iter().chain(crafted.iter()) {
        f(*c);
        mats.iter().for_each(|&x| f(x));
    }
    gained_used.iter().for_each(|x| f(x.0));
    s.turn_stats.loyalty_activated.iter().for_each(|&x| f(x));
    s.turn_stats.trigger_counts.iter().for_each(|x| f(x.0));
    // Pending game objects: a spell or ability on the stack targeting one copy (CR 115.1), a
    // trigger about one copy, an effect remembering one copy, and the source of an ability on the
    // stack or waiting to go there ("this creature gets +2/+2" is about that copy, CR 113.7).
    // A spell's source is the spell itself.
    // The resolving item too (still on the stack, 608.2n: its later parts may refer to them).
    for si in s.stack_items() {
        let StackItem { kind, controller: _, source, source_card: _, ability: _, targets, x: _, kicked: _, modes: _, trig, trigger_idx: _, flashback: _, uid: _ } = si;
        for &(_, t) in targets {
            match t {
                Target::Obj(x) | Target::Ability { src: x, uid: _ } => f(x),
                Target::Player(_) => {}
            }
        }
        trig_refs(trig, &mut f);
        if *kind != StackKind::Spell {
            f(*source);
        }
    }
    for pt in &s.pending_triggers {
        let crate::state::PendingTrigger { source, card: _, controller: _, trigger_idx: _, trig, adhoc: _ } = pt;
        trig_refs(trig, &mut f);
        f(*source);
    }
    for eo in &s.effect_objs {
        // Exhaustive: a new field naming an object must be classified here to compile.
        let crate::state::EffectObj { source, controller: _, statics: _, remembered, duration, created_turn: _, forget_on_moved: _, exile_on_moved: _, had_remembered: _, ts: _, triggers: _ } = eo;
        remembered.iter().for_each(|&x| f(x));
        // "For as long as [this] remains on the battlefield" (CR 611.2b; `State::move_obj` ends
        // the effect when its source leaves, 400.7): the source hosts the effect, so it isn't
        // interchangeable with an otherwise identical permanent (attacking with,
        // blocking with or targeting Tishana's Tidebinder whose ability loss is running, or its
        // twin, differ). Other durations don't depend on the source object; statics naming it
        // (`EffectSource`) aren't compiled (a test guards that).
        if *duration == crate::carddb::EffDuration::AsLongAsInPlay {
            f(*source);
        }
    }
}

/// Does `o` have an activated ability with {T} in its cost (CR 302.6: summoning sickness can
/// stop it)? Its current abilities (`c = chars(s, db, o)`: printed and gained, CR 613.1f; Great
/// Divide Guide's "Each land and Ally you control has '{T}: Add one mana of any color'")
/// or a printed one (kept even while the abilities are lost: the loss may end this turn,
/// and splitting more never merges a different choice).
pub fn has_tap_ability(s: &State, db: &CardDb, o: ObjId, c: &Chars) -> bool {
    let ob = s.obj(o);
    let tap = |a: &crate::carddb::AbilityId| db.ability(*a).cost.as_ref().is_some_and(|c| c.tap);
    c.abilities.iter().any(tap) || db.get(ob.card).face(ob.face).activated.iter().any(tap)
}

fn obj_sig_with(s: &State, db: &CardDb, o: ObjId, has_attachment: impl Fn(ObjId) -> bool, referenced: impl Fn(ObjId) -> bool) -> ObjSig {
    let ob = s.obj(o);
    let on_bf = ob.zone == Zone::Battlefield;
    let mut extra = 0u32;
    let mut combat_unique = false;
    if on_bf {
        // CR 302.6, where it can matter: on its controller's turn (attacking, {T} costs), or
        // for a creature with a {T} ability (activatable on any turn). A creature the opponent
        // cast last turn is otherwise interchangeable with an older copy.
        if s.is_sick(o) {
            let c = chars(s, db, o);
            if c.types.has(CardType::Creature) && !c.kws.has(Kw::Haste) && (ob.controller == s.turn.active || has_tap_ability(s, db, o, &c)) {
                extra |= SIG_SICK;
            }
        }
        if s.combat.attackers.iter().any(|a| a.0 == o) {
            if s.combat.blocked.contains(&o) {
                combat_unique = true;
            } else {
                let pw = s.combat.attacking_pw.iter().find(|x| x.0 == o).map_or(0, |x| x.1 .0 + 1);
                extra |= SIG_ATTACKING | (pw & SIG_LOW);
            }
        }
        if ob.entered == s.turn.number && !db.x9.self_entered.is_empty() && db.x9.self_entered.contains(&ob.card) {
            extra |= SIG_ENTERED_NOW;
        }
        let mut blocks = s.combat.blocks.iter().filter(|b| b.0 == o);
        if let Some(first) = blocks.next() {
            if blocks.next().is_some() {
                combat_unique = true;
            } else {
                extra |= SIG_BLOCKING | ((first.1 .0 + 1) & SIG_LOW);
            }
        }
    }
    if ob.zone == Zone::Hand {
        extra |= (ob.known_to as u32 & 3) << SIG_KNOWN_SHIFT;
    }
    // Cast designations: on the battlefield and the stack they make the object unique (how
    // the permanent was cast, its links); in the other zones they are per-card designations
    // (warped, plotted, airbent in exile; CR 702.185b, 702.170a, 701.65a) that twins can share.
    let cast_flags = if s.cast_info.is_empty() { 0 } else { s.cast_of(o).flags };
    let designations_in_sig = !on_bf && ob.zone != Zone::Stack;
    if designations_in_sig {
        extra |= cast_flags as u32 & 0xffff;
        if cast_flags & (crate::altcast::flag::WARPED | crate::altcast::flag::PLOTTED) != 0 && ob.entered == s.turn.number {
            extra |= SIG_EXILED_NOW;
        }
        // Read only by mayhem (CR 702.187b: castable the turn it was discarded): a designation
        // of a mayhem card, nothing for any other card.
        if !s.ext.x6.discarded_this_turn.is_empty() && s.ext.x6.discarded_this_turn.contains(&o) && db.get(ob.card).front().alt.mayhem.is_some() {
            extra |= SIG_DISCARDED;
        }
    }
    let chosen = s.ext.chosen_type_of(o).map_or(0, |t| t as u32 + 1) | s.ext.named_of(o).map_or(0, |n| (n as u32 + 1) << 16);
    // Face-down permanents are never interchangeable (their real cards differ, CR 708).
    let modified = combat_unique
        || (on_bf && ob.face_down())
        || !ob.counters.is_empty()
        || ob.on_adventure
        || (!designations_in_sig && cast_flags != 0)
        || s.effects.iter().any(|e| e.affected == o)
        || s.copy_effects.iter().any(|c| c.obj == o)
        || ob.attached_to.is_some()
        || (on_bf && has_attachment(o))
        || referenced(o);
    // Controller and owner are part of the signature: identical permanents of different players
    // are different choices (a removal spell's target, a copy source, an exile that returns to
    // its owner). Requiting Hex's targets {own Dream Beavers, opponent's Dream Beavers}
    // collapsed into one forced option.
    (ob.card.0, ob.tapped as u8 | (ob.token as u8) << 1 | (ob.prepared as u8) << 2 | (ob.face & 7) << 3 | (ob.controller.0 & 1) << 6 | (ob.owner.0 & 1) << 7, ob.damage, if modified { o.0 + 1 } else { 0 }, extra, chosen)
}

/// Might some object have lost its abilities (cheap guard for `lost_all_abilities`)?
pub fn may_lose_abilities(s: &State, db: &CardDb) -> bool {
    !s.effect_objs.is_empty() || db.mr.lands_become.is_some()
}

/// Has `o` lost all abilities (CR 613.1f layer 6, e.g. Tishana's Tidebinder)? Cheap when no
/// effect objects exist. Uses layer 1–5 characteristics for the affected filter.
pub fn lost_all_abilities(s: &State, db: &CardDb, o: ObjId) -> bool {
    // CR 305.7: a land whose subtype an effect sets to a basic land type loses the abilities
    // from its rules text (`ext4`, Zhao).
    if db.mr.lands_become.is_some() && crate::ext4::rules_text_lost(s, db, o) {
        return true;
    }
    ability_loss_ts(s, db, o).is_some()
}

/// Timestamp of the latest "loses all abilities" effect applying to `o` (CR 613.1f, 613.7), if
/// any. Not CR 305.7's loss of rules-text abilities (it doesn't remove granted abilities).
pub fn ability_loss_ts(s: &State, db: &CardDb, o: ObjId) -> Option<u32> {
    let mut loss = None;
    for eo in &s.effect_objs {
        for &si in &eo.statics {
            // effect-object static reader: RemoveAllAbilities (`static_ext::applies_from_effect_object`)
            if let crate::carddb::StaticKind::RemoveAllAbilities { affected } = &db.statics_pool[si as usize].kind {
                if loss.is_none_or(|l| eo.ts > l) && obj_matches_base(s, db, affected, o, eo_fcx!(eo)) {
                    loss = Some(eo.ts);
                }
            }
        }
    }
    loss
}

/// Has an ability granted to `o` by an effect with timestamp `ts` (an Equipment / Aura / other
/// static's, or a resolved effect's) been removed by a later "loses all abilities" effect (CR
/// 613.1f, 613.7: layer 6 in timestamp order)? Abilities granted later are kept.
pub fn grant_removed(s: &State, db: &CardDb, o: ObjId, ts: u32) -> bool {
    !s.effect_objs.is_empty() && ability_loss_ts(s, db, o).is_some_and(|l| l > ts)
}

#[cfg(test)]
mod tests {
    /// `query::frozen` reuses memoized values across states that differ only in
    /// `turn.priority` / `turn.passes` (`State::eq_but_priority`). That is exact only while no
    /// query reads them: only the priority loop (`flow.rs`) and state construction may
    /// (`scenario.rs` builds a position's `Turn` and dumps `priority` in the god-view state; it is
    /// never called by a query).
    #[test]
    fn priority_fields_read_only_by_the_priority_loop() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut bad = vec![];
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            if !name.ends_with(".rs") || ["flow.rs", "state.rs", "builder.rs", "query.rs", "scenario.rs"].contains(&name.as_str()) {
                continue;
            }
            let src = std::fs::read_to_string(&p).unwrap();
            for (i, line) in src.lines().enumerate() {
                let l = line.split("//").next().unwrap();
                // A state's `s.turn` not followed by `.field`: the whole `Turn` is used.
                let whole_turn = l.match_indices("s.turn").any(|(i, _)| {
                    let state_var = i == 0 || !l[..i].ends_with(|c: char| c == '_' || c.is_alphanumeric());
                    state_var && !l[i + 6..].starts_with(|c: char| c == '.' || c == '_' || c.is_alphanumeric())
                });
                if l.contains(".passes") || l.contains(".priority") || l.contains("Turn {") || whole_turn {
                    bad.push(format!("{name}:{}: {}", i + 1, line.trim()));
                }
            }
        }
        assert!(bad.is_empty(), "turn.priority/passes (or the whole Turn) read outside flow.rs; update State::eq_but_priority: {bad:#?}");
    }
}
