//! Which faces of a card a permission lets a player cast or play: one answer for every path.
//!
//! The rules question: a card with two castable faces, an adventurer (CR 715.3), an omen card
//! (720.3), a split card or a Room (709.3, 709.5), is cast as the face the player chooses; "only
//! the chosen half is evaluated to see if it can be cast" (709.3a, 715.3a, 720.3a, 601.3e). A
//! permission that names no characteristic ("you may cast that card", airbend's "its owner may
//! cast it", "cast a copy") allows either face (715.3d: "other effects that allow a player to cast
//! it may allow a player to cast it as an Adventure"; adventurer rulings: alternative costs and
//! "without paying its mana cost" may apply to the Adventure; Room rulings: "To cast a Room spell,
//! choose a half (or "door") to cast", "If an effect allows you to cast a spell with certain
//! characteristics, consider only the characteristics of the door you're casting"). A land face
//! is never cast (playing a land is a special action, 116.2a, 305.1; 305.9: "It can't be cast as
//! a spell"); a play permission plays it as a land.
//!
//! Every path that offers a card to cast or play asks this module. The paths and the permission
//! each one is (tests check them all with each layout):
//! - a card in its owner's hand: `FacePerm::Any` (715.3, 720.3, 709.3);
//! - `MayPlay` statics and effects (impulse draws, Outrageous Robbery, Null Summoner, Icetill
//!   Explorer): `FacePerm::Filtered` (601.3e);
//! - a card exiled on an adventure: `FacePerm::NormalOnly` (715.3d: "It can't be cast as an
//!   Adventure this way");
//! - airbend (701.65a), "you may cast that card" (`ext4::MayCastImprinted`), "you
//!   may cast it without paying" (`ext9::MayCastDefinedFree`), casting copies (`ext2::CastCopies`,
//!   707.12, 709.3c; `ext7::ParadigmCopy`): `FacePerm::Any`;
//! - keyword permissions printed on the card's normal face (flashback, mayhem, harmonize, warp,
//!   plot): that face only (`FacePerm::NormalOnly`); no Standard card has one of them on an
//!   adventurer / omen / split card / Room.
//!
//! Kept approximation: a split card or Room outside the stack has
//! both halves' characteristics (709.4), and a `Filtered` permission matches its halves with
//! those; a filter that tells the halves apart (by colour, mana value or type) would over-allow
//! one half. No pool permission does.
use crate::basics::*;
use crate::carddb::{CardDb, Layout};
use crate::engine::Opt;
use crate::filter::Filter;
use crate::query::{obj_matches, FilterCtx};
use crate::state::*;
use smallvec::SmallVec;

/// The card's normal characteristics: a land card played as a land (CR 305.9), any other card
/// cast normally (for a Room: its left door; a split card: its left half).
pub const FACE_NORMAL: u8 = 1;
/// The card's other castable face: an Adventure, Omen, split card's right half or Room's right
/// door (face 1).
pub const FACE_ALT: u8 = 2;

/// Does a card of this layout have a second face a player may choose to cast (CR 715.3, 720.3,
/// 709.3)? A preparation card's prepare spell isn't (722.3: "can't be cast using the alternative
/// characteristics"; its copies are, `cast::cast_prepared`), nor a transforming card's back face
/// (712.11: cast with its front face up; "transformed" casting isn't compiled).
pub fn has_alt_face(l: Layout) -> bool {
    matches!(l, Layout::Adventure | Layout::Omen | Layout::Split | Layout::Room)
}

/// What a permission says about faces.
#[derive(Clone, Copy)]
pub enum FacePerm<'a> {
    /// Any face (a card in hand; permissions that name no characteristic).
    Any,
    /// The normal face only (715.3d's own permission; keyword permissions of the normal face).
    NormalOnly,
    /// The faces whose characteristics the permission's filter matches (CR 601.3e).
    Filtered(&'a Filter, FilterCtx<'a>),
}

/// The faces (`FACE_NORMAL` / `FACE_ALT`) of `o` that `perm` allows, before castability (timing,
/// costs, targets) and before the land rule: a land face in the mask can be played as a land by a
/// play permission, never cast (`spell_faces`).
pub fn permitted(s: &State, db: &CardDb, o: ObjId, perm: FacePerm) -> u8 {
    let alt = has_alt_face(db.get(s.obj(o).card).layout);
    match perm {
        FacePerm::Any => FACE_NORMAL | if alt { FACE_ALT } else { 0 },
        FacePerm::NormalOnly => FACE_NORMAL,
        FacePerm::Filtered(f, fcx) => {
            let mut m = 0;
            if filter_allows_face(s, db, f, o, 0, fcx) {
                m |= FACE_NORMAL;
            }
            if alt && filter_allows_face(s, db, f, o, 1, fcx) {
                m |= FACE_ALT;
            }
            m
        }
    }
}

/// The faces of `o` that `perm` lets a player cast as a spell, in face order: the permitted
/// faces that aren't land faces (CR 116.2a, 305.9). The caller still checks castability.
pub fn spell_faces(s: &State, db: &CardDb, o: ObjId, perm: FacePerm) -> SmallVec<[u8; 2]> {
    faces_of_mask(s, db, o, permitted(s, db, o, perm))
}

/// The castable (non-land) faces among `mask`.
pub fn faces_of_mask(s: &State, db: &CardDb, o: ObjId, mask: u8) -> SmallVec<[u8; 2]> {
    let cd = db.get(s.obj(o).card);
    let mut v = SmallVec::new();
    for (bit, face) in [(FACE_NORMAL, 0u8), (FACE_ALT, 1u8)] {
        if mask & bit != 0 && !cd.face(face).types.has(CardType::Land) {
            v.push(face);
        }
    }
    v
}

/// Does a permission's filter allow face `face` of `o` (CR 601.3e: "an alternative set of
/// characteristics ... [is] considered to determine if a card ... is legal to cast"; 715.3a,
/// 720.3a)? An Adventure / Omen is matched with its own characteristics, so "you may cast instant
/// and sorcery spells from among them" allows an instant Adventure and not its creature card, and
/// "you may cast that card" (`nonLand`) an adventurer land's sorcery Adventure (Midgar's Reactor
/// Raid) but not the land. A split card or Room outside the stack has both halves'
/// characteristics (709.4); its halves are matched with those (module doc: kept approximation).
fn filter_allows_face(s: &State, db: &CardDb, f: &Filter, o: ObjId, face: u8, fcx: FilterCtx) -> bool {
    if face == 0 || !matches!(db.get(s.obj(o).card).layout, Layout::Adventure | Layout::Omen) {
        return obj_matches(s, db, f, o, fcx);
    }
    // The same object (remembered by the permission) with the face's characteristics, where it is.
    let mut s2 = s.clone();
    s2.obj_mut(o).face = face;
    obj_matches(&s2, db, f, o, fcx)
}

/// The option for casting face `face` of `o` in a "you may cast one of these" choice made while
/// an effect resolves (`ChooseCard`): the card for its normal face, `CastFace` for the other.
pub fn choice_opt(o: ObjId, face: u8) -> Opt {
    if face == 0 {
        Opt::Card(o)
    } else {
        Opt::CastFace(o, face)
    }
}

/// The card and face a `choice_opt` names (None: `Done` or another option).
pub fn chosen_face(opt: &Opt) -> Option<(ObjId, u8)> {
    match *opt {
        Opt::Card(o) => Some((o, 0)),
        Opt::CastFace(o, f) => Some((o, f)),
        _ => None,
    }
}
