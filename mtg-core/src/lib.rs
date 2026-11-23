//! Deterministic, step-based Magic: The Gathering rules engine.
//!
//! Entry point: [`engine::Game`].

/// The filter context of the resolving ability `$ex` (`resolve::Exec`): its controller, its
/// source, its X and the objects it remembered so far. Every resolver builds its filter context
/// with this, overriding a field with struct-update syntax where it means something else
/// (`FilterCtx { you: p, x: 0, ..exec_fcx!(ex) }`), so no resolver evaluates `IsRemembered` with
/// an empty memory (a test checks no literal bypasses it).
macro_rules! exec_fcx {
    ($ex:ident) => {
        crate::query::FilterCtx { you: $ex.you, source: Some($ex.source), x: $ex.x, rem: &$ex.remembered }
    };
}
/// The filter context of an expression being evaluated (`expr::EvalCtx` `$c`): as
/// [`exec_fcx`], with the evaluating ability's remembered objects (`Count$Valid
/// Card.IsRemembered...`).
macro_rules! eval_fcx {
    ($c:ident) => {
        crate::query::FilterCtx { you: $c.you, source: Some($c.source), x: $c.x, rem: $c.rem }
    };
}
/// The filter context of an effect object's static `$eo` (`state::EffectObj`): its controller,
/// its source and the objects it remembers ("that creature can't block", Braided Net).
macro_rules! eo_fcx {
    ($eo:ident) => {
        crate::query::FilterCtx { you: $eo.controller, source: Some($eo.source), x: 0, rem: &$eo.remembered }
    };
}

pub mod altcast;
pub mod aux;
pub mod basics;
pub mod bo3;
pub mod builder;
pub mod carddb;
pub mod cast;
#[deny(unused_must_use)] // `compile::P` readers: a discarded read is a dropped parameter
pub mod compile;
#[deny(unused_must_use)] // `compile::P` readers: a discarded read is a dropped parameter
pub mod compile_ext;
pub mod compile_audit;
pub mod combat;
pub mod deck;
pub mod determinize;
pub mod effect_ext;
pub mod encode;
pub mod engine;
pub mod expr;
pub mod expr_ext;
pub mod ext;
#[deny(unused_must_use)] // `compile::P` readers: a discarded read is a dropped parameter
pub mod ext2;
#[deny(unused_must_use)] // `compile::P` readers: a discarded read is a dropped parameter
pub mod ext3;
#[deny(unused_must_use)] // `compile::P` readers: a discarded read is a dropped parameter
pub mod ext4;
#[deny(unused_must_use)] // `compile::P` readers: a discarded read is a dropped parameter
pub mod ext5;
#[deny(unused_must_use)] // `compile::P` readers: a discarded read is a dropped parameter
pub mod ext6;
#[deny(unused_must_use)] // `compile::P` readers: a discarded read is a dropped parameter
pub mod ext7;
#[deny(unused_must_use)] // `compile::P` readers: a discarded read is a dropped parameter
pub mod ext8;
#[deny(unused_must_use)] // `compile::P` readers: a discarded read is a dropped parameter
pub mod ext9;
pub mod faces;
pub mod filter;
pub mod filter_ext;
pub mod flow;
pub mod hist;
pub mod invariants;
pub mod lki;
pub mod loops;
pub mod name_pool;
pub mod num;
pub mod observe;
pub mod query;
pub mod resolve;
pub mod sba;
pub mod scenario;
pub mod scenario_stack;
pub mod script_fixes;
pub mod state;
pub mod static_ext;
pub mod testutil;
pub mod triggers;
pub mod trig_ext;
pub mod zones;

pub use basics::*;
pub use carddb::CardDb;
pub use engine::{Deck, Decision, DecisionKind, Fidelity, Game, Opt};
