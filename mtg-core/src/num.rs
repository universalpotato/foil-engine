//! Bounds on game numbers.
//!
//! CR 107.1: "The only numbers the Magic game uses are integers." The CR sets no upper bound, but
//! the engine stores life, damage, counters and P/T modifications as `i32` (release builds don't
//! check overflow), and exponential effects (Ouroboroid's counters, counter/token doublers)
//! reach 2^31 in a few turns. Rather than wrap (or saturate) into a wrong but
//! legal-looking state, every game number is bounded by `NUM_MAX` and a game that would exceed it
//! fails loudly:
//!
//! - **Stored numbers** (life, life gained/lost this turn, damage marked, counters, P/T
//!   modifications of continuous effects, effect amounts) are written through
//!   `State::num_add` / `State::num_check`: a value or amount beyond `NUM_MAX` records
//!   `State::overflow`. `Ctx::choose` and `Game::advance` turn that into `Halt::Unsupported`
//!   (an engine error: the game is aborted, nothing after the overflow is committed or shown).
//! - **Derived numbers** (expressions, characteristics) are computed exactly in `i64` and narrowed
//!   with the operations below: they never wrap; a result past the `i32` range saturates and
//!   stays saturated through later operations (so it can't come back into range as a wrong but
//!   plausible value), hence an in-range result is always exact. Out-of-range power/toughness is caught at the next
//!   state-based-action check (CR 704.3, before any player gets priority, `sba::check`); any
//!   out-of-range amount that an effect uses is caught where it is evaluated or stored.
//!
//! `NUM_MAX` = 10^9 leaves `i32` headroom: the sum of two in-range values never overflows.
//! Comparisons against in-range values stay exact under narrowing (it is monotone).

/// Largest magnitude of any game number (see module docs).
pub const NUM_MAX: i32 = 1_000_000_000;

/// Not a number: "all of them" in a compiled effect (`ChangeNum$ All`); never bounded.
pub const ALL: i32 = i32::MAX;

/// Is `x` beyond the bound?
#[inline]
pub fn big(x: i32) -> bool {
    x.unsigned_abs() > NUM_MAX as u32
}

/// An exact value as `i32`, saturating (never wrapping) past the `i32` range.
#[inline]
pub fn narrow(v: i64) -> i32 {
    v.clamp(-(i32::MAX as i64), i32::MAX as i64) as i32
}

/// Saturated by `narrow`, so possibly inexact (`±i32::MAX`, or `i32::MIN` from elsewhere).
#[inline]
fn inexact(x: i32) -> bool {
    x.unsigned_abs() >= i32::MAX as u32
}

/// The `i32` result of an operation whose exact value is `v`: exact unless an operand was
/// already saturated, in which case it stays saturated (sign of `v`).
#[inline]
fn op(a: i32, b: i32, v: i64) -> i32 {
    if inexact(a) || inexact(b) {
        if v < 0 {
            -i32::MAX
        } else {
            i32::MAX
        }
    } else {
        narrow(v)
    }
}

/// `a + b` (sticky, see module docs).
#[inline]
pub fn add(a: i32, b: i32) -> i32 {
    op(a, b, a as i64 + b as i64)
}

/// `a - b` (sticky).
#[inline]
pub fn sub(a: i32, b: i32) -> i32 {
    op(a, b, a as i64 - b as i64)
}

/// `a * b` (sticky).
#[inline]
pub fn mul(a: i32, b: i32) -> i32 {
    op(a, b, a as i64 * b as i64)
}

/// `a` divided by `d > 0`, rounded down (sticky).
#[inline]
pub fn div_floor(a: i32, d: i32) -> i32 {
    op(a, 0, (a as i64).div_euclid(d.max(1) as i64))
}

/// Sum (exact in `i64`; sticky).
pub fn sum(it: impl IntoIterator<Item = i32>) -> i32 {
    let (mut v, mut sat) = (0i64, false);
    for x in it {
        sat |= inexact(x);
        v += x as i64;
    }
    op(if sat { i32::MAX } else { 0 }, 0, v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_in_range_sticky_out_of_range() {
        assert_eq!(add(NUM_MAX - 1, 1), NUM_MAX);
        assert!(big(add(NUM_MAX, 1)));
        assert_eq!(add(i32::MAX, i32::MAX), i32::MAX, "saturates, never wraps");
        assert_eq!(mul(1 << 20, 1 << 20), i32::MAX);
        assert_eq!(sub(-NUM_MAX, NUM_MAX), -2 * NUM_MAX);
        // Values past NUM_MAX are still exact up to the i32 range ...
        assert_eq!(sub(2 * NUM_MAX, NUM_MAX + 5), NUM_MAX - 5);
        assert_eq!(sum([NUM_MAX, NUM_MAX, -NUM_MAX]), NUM_MAX);
        // ... and a saturated operand keeps the result saturated (never back in range).
        assert_eq!(sub(i32::MAX, NUM_MAX), i32::MAX);
        assert_eq!(div_floor(-i32::MAX, 2), -i32::MAX);
        assert_eq!(sum([mul(NUM_MAX, 3), -2 * NUM_MAX]), i32::MAX);
        assert_eq!(div_floor(-7, 2), -4);
    }
}
