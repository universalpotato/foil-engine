//! Rules invariants for fuzz and regression games.
//!
//! Two kinds:
//! - **State checks** ([`check_state`]): pure functions over a state, called by test drivers
//!   after every decision. The engine never calls them.
//! - **Probes** inside the engine (one move event per instruction, resolution costs that change
//!   the game, tokens a node created). They exist only with the `invariants` cargo feature, which
//!   only mtg-core's own test builds turn on (the self dev-dependency in `Cargo.toml`), and they
//!   run only on a thread that called [`enable_probes`]. Without the feature every probe is an
//!   empty `#[inline(always)]` function and `NodeFrame` is a zero-sized type: release builds
//!   carry nothing. With the feature but not enabled, a probe is
//!   one thread-local flag read. A probe never changes the game: it only records violations,
//!   which the driver collects with [`take_violations`].
use crate::basics::*;
#[cfg(feature = "invariants")]
use crate::carddb::Effect;
use crate::carddb::{AbilityId, CardDb};
use crate::state::*;

/// Whether the engine-internal probes are compiled in (the `invariants` feature).
pub const PROBES: bool = cfg!(feature = "invariants");

/// Invariants of a state (committed, or paused at a decision: `Game::view_state`). Err: every
/// violation, one line each.
///
/// - **Spell controller (CR 601.2a "that player becomes its controller", 108.4a, 110.2):** a
///   spell's stack entry and the spell object have the same controller (a spell
///   cast from another player's card had the caster on its `StackItem` but its owner on the
///   object, which every "its controller" reader saw).
pub fn check_state(s: &State, db: &CardDb) -> Result<(), String> {
    let mut errs: Vec<String> = vec![];
    for it in s.stack_items() {
        if it.kind != StackKind::Spell {
            continue;
        }
        let Some(o) = s.objs.get(it.source.idx()) else {
            errs.push(format!("stack spell #{} has no object", it.source.0));
            continue;
        };
        if o.controller != it.controller {
            errs.push(format!(
                "spell controller: {} (#{}) on the stack is controlled by {:?} but its object by {:?} (owner {:?}; CR 601.2a, 108.4a)",
                db.get(o.card).name,
                it.source.0,
                it.controller,
                o.controller,
                o.owner
            ));
        }
    }
    if errs.is_empty() {
        Ok(())
    } else {
        Err(errs.join("; "))
    }
}

/// `a` and `b` are the same game apart from objects that had ceased to exist in both (`Gone`
/// before and after: their last known information may be read, but changing it changes nothing
/// in the game) and the event list.
pub fn same_live_state(a: &State, b: &State) -> bool {
    if a.objs.len() != b.objs.len() {
        return false;
    }
    let mut b2 = b.clone();
    for (i, o) in b2.objs.iter_mut().enumerate() {
        if o.zone == Zone::Gone && a.objs[i].zone == Zone::Gone {
            *o = a.objs[i].clone();
        }
    }
    b2.events.clone_from(&a.events);
    *a == b2
}

/// Turn the engine probes on or off for the current thread (no-op without the feature).
pub fn enable_probes(on: bool) {
    #[cfg(feature = "invariants")]
    imp::ON.with(|c| c.set(on));
    #[cfg(not(feature = "invariants"))]
    let _ = on;
}

/// Violations the probes recorded on this thread since the last call.
pub fn take_violations() -> Vec<String> {
    #[cfg(feature = "invariants")]
    {
        imp::LOG.with(|l| std::mem::take(&mut *l.borrow_mut()))
    }
    #[cfg(not(feature = "invariants"))]
    {
        vec![]
    }
}

#[cfg(feature = "invariants")]
mod imp {
    use std::cell::{Cell, RefCell};
    pub struct Frame {
        /// One entry per move event (`zones::move_batch`, `move_special`): (origin zones, destination).
        pub moves: Vec<(u16, crate::state::Zone)>,
        pub objs_len: usize,
    }
    thread_local! {
        pub static ON: Cell<bool> = const { Cell::new(false) };
        pub static LOG: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
        pub static FRAMES: RefCell<Vec<Frame>> = const { RefCell::new(Vec::new()) };
    }
    pub fn on() -> bool {
        ON.with(|c| c.get())
    }
    pub fn record(msg: String) {
        LOG.with(|l| {
            let mut l = l.borrow_mut();
            if l.len() < 10_000 {
                l.push(msg);
            }
        });
    }
}

/// An ability node being applied (`resolve::apply_effect`); see [`node_begin`].
#[must_use]
pub struct NodeFrame {
    #[cfg(feature = "invariants")]
    depth: Option<usize>,
}

/// A node starts applying its effect: its move events and created objects are counted until
/// [`node_end`].
#[inline(always)]
pub(crate) fn node_begin(s: &State) -> NodeFrame {
    #[cfg(feature = "invariants")]
    {
        if !imp::on() {
            return NodeFrame { depth: None };
        }
        let d = imp::FRAMES.with(|f| {
            let mut f = f.borrow_mut();
            f.push(imp::Frame { moves: vec![], objs_len: s.objs.len() });
            f.len()
        });
        NodeFrame { depth: Some(d) }
    }
    #[cfg(not(feature = "invariants"))]
    {
        let _ = s;
        NodeFrame {}
    }
}

/// One move event: the simultaneous zone changes (origin, destination) of `zones::move_batch` /
/// `move_special`, counted for the innermost node applying (one entry per destination).
#[inline(always)]
pub(crate) fn move_event(moves: impl Iterator<Item = (Zone, Zone)>) {
    #[cfg(feature = "invariants")]
    {
        if !imp::on() {
            return;
        }
        let mut per_to: Vec<(u16, Zone)> = vec![];
        for (from, to) in moves {
            match per_to.iter_mut().find(|x| x.1 == to) {
                Some(x) => x.0 |= 1 << from as u16,
                None => per_to.push((1 << from as u16, to)),
            }
        }
        imp::FRAMES.with(|f| {
            if let Some(top) = f.borrow_mut().last_mut() {
                top.moves.extend(per_to);
            }
        });
    }
    #[cfg(not(feature = "invariants"))]
    {
        let _ = moves;
    }
}

/// The node `n` finished applying (`ok`: without halting; a paused unit re-runs it).
///
/// - **One instruction, one event (CR 608.2c, 603.10a, 101.4):** a node moving several objects
///   from the same zones to the same zone does so in one move event, so each object's
///   leaves-the-battlefield abilities see the others leave (several players'
///   sacrifices were one event per player).
/// - **Created tokens are what the node affected (CR 614.1a, 111.1; Kiki-Jiki ruling
///   2021-03-19):** every token a token-creating node made (doubled ones included) is in the
///   node's moved set, which the next nodes act on ("sacrifice them", "they gain haste";
///   only the first copy was).
#[inline(always)]
pub(crate) fn node_end(f: NodeFrame, ok: bool, s: &State, db: &CardDb, n: AbilityId, source_card: CardId, moved: &[ObjId]) {
    #[cfg(feature = "invariants")]
    {
        let Some(depth) = f.depth else { return };
        let fr = imp::FRAMES.with(|fs| {
            let mut fs = fs.borrow_mut();
            debug_assert_eq!(fs.len(), depth, "node frames out of order");
            fs.truncate(depth);
            fs.pop()
        });
        let Some(fr) = fr else { return };
        if !ok {
            return;
        }
        let ad = db.ability(n);
        let name = || db.get(source_card).name.clone();
        // The effect's variant name ("Sacrifice", "Ext(Lt(TokenAttached", ...).
        let kind = || {
            let e = format!("{:?}", ad.effect);
            e[..e.find([' ', '{']).unwrap_or(e.len())].to_string()
        };
        for (i, a) in fr.moves.iter().enumerate() {
            if let Some(b) = fr.moves[i + 1..].iter().find(|b| b.1 == a.1 && b.0 & a.0 != 0) {
                imp::record(format!(
                    "one-event: {} node {} ({}) moved objects {:#06b}->{:?} in {} events ({:?} and {:?}) (CR 608.2c, 603.10a)",
                    name(),
                    n.0,
                    kind(),
                    a.0,
                    a.1,
                    fr.moves.iter().filter(|m| m.1 == a.1).count(),
                    a,
                    b
                ));
                break;
            }
        }
        let made: Vec<ObjId> = (fr.objs_len..s.objs.len()).map(|i| ObjId(i as u32)).filter(|&o| s.obj(o).token && s.obj(o).zone == Zone::Battlefield).collect();
        if !made.is_empty() {
            let creator = matches!(ad.effect, Effect::Token { .. } | Effect::CopyPermanent | Effect::Ext(crate::effect_ext::ExtEffect::TokenFor { .. }) | Effect::Ext(crate::effect_ext::ExtEffect::Lt(crate::ext3::LtEffect::TokenAttached { .. })));
            let missing = made.iter().filter(|o| !moved.contains(o)).count();
            if missing > 0 && (creator || missing < made.len()) {
                imp::record(format!("created-tokens: {} node {} ({}) created {} tokens but {missing} are not in its moved set (CR 614.1a, 111.1)", name(), n.0, kind(), made.len()));
            }
        }
    }
    #[cfg(not(feature = "invariants"))]
    {
        let _ = (f, ok, s, db, n, source_card, moved);
    }
}

/// A snapshot before a resolution-time cost is paid ([`cost_paid`]); `None` when probes are off
/// or the cost is `free` (evaluated only with probes on: "{0}" is paid by acknowledging it, CR
/// 118.5, and changes nothing).
#[inline(always)]
pub(crate) fn cost_begin(s: &State, free: impl FnOnce() -> bool) -> Option<Box<State>> {
    #[cfg(feature = "invariants")]
    {
        (imp::on() && !free()).then(|| Box::new(s.clone()))
    }
    #[cfg(not(feature = "invariants"))]
    {
        let _ = (s, free);
        None
    }
}

/// No mana symbol to pay (`{0}` or nothing).
pub fn zero_mana(m: &ManaCost) -> bool {
    m.syms.iter().all(|s| matches!(s, ManaSym::Generic(0)))
}

/// **Paying changes the game (CR 118.3, 400.7, 608.2d):** answering "yes" to a
/// resolution-time cost ("you may [cost]. If you do, ...", "unless [player] pays") pays something
/// real. A cost paid by changing only an object that had already ceased to exist (Guiding Hydra's
/// counters removed from its last known information after it left the battlefield) is no payment.
#[inline(always)]
pub(crate) fn cost_paid(before: Option<Box<State>>, s: &State, db: &CardDb, source_card: CardId, what: &str) {
    #[cfg(feature = "invariants")]
    {
        let Some(b) = before else { return };
        if same_live_state(&b, s) {
            imp::record(format!("cost-paid: {} paid {what} without changing the game (CR 118.3, 400.7)", db.get(source_card).name));
        }
    }
    #[cfg(not(feature = "invariants"))]
    {
        let _ = (before, s, db, source_card, what);
    }
}
