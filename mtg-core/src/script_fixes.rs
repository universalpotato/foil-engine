//! Corrections to Forge card scripts whose rules deviate from the card's Oracle text (the
//! Oracle text is authoritative). Each fix replaces one exact fragment of one card's script as
//! it is loaded; the file stays untouched. If the fragment isn't in the script (Forge
//! fixed or changed it), loading fails loudly so the fix gets re-checked rather than silently
//! dropped.

/// (card name, exact script fragment, replacement, why).
pub const FIXES: &[(&str, &str, &str, &str)] = &[
    (
        "Konstrari Charm",
        "SVar:DBPutCounter:DB$ PutCounter | ValidTgts$ Creature.YouCtrl | ValidTgtsDesc$ creature you control |",
        "SVar:DBPutCounter:DB$ PutCounter | ValidTgts$ Creature | ValidTgtsDesc$ creature |",
        "Oracle: \"Put two +1/+1 counters on target creature. It gains trample until end of turn.\" (any creature, \
         CR 115.1); the script restricts it to creatures you control",
    ),
    (
        "Giada, Font of Hope",
        "K:ETBReplacement:Other:AddExtraCounter:Mandatory:Battlefield:Creature.Angel+YouCtrl+Other",
        "K:ETBReplacement:Other:AddExtraCounter:Mandatory:Battlefield:Angel.YouCtrl+Other",
        "Oracle: \"Each other Angel you control enters with an additional +1/+1 counter on it ...\": any Angel \
         permanent (CR 109.2), e.g. a changeling Kindred artifact (205.3m, 702.73a; Firdoch Core); the script \
         limits it to Angel creatures. XMage: `FilterPermanent(SubType.ANGEL)`",
    ),
    (
        "Dragonstorm Globe",
        "K:ETBReplacement:Other:AddExtraCounter:Mandatory:Battlefield:Creature.Dragon+YouCtrl",
        "K:ETBReplacement:Other:AddExtraCounter:Mandatory:Battlefield:Dragon.YouCtrl",
        "Oracle: \"Each Dragon you control enters with an additional +1/+1 counter on it.\": any Dragon permanent \
         (CR 109.2), as Giada. XMage: `FilterPermanent(SubType.DRAGON, \"Dragon\")`",
    ),
];

/// The script text with this card's fixes applied (`name` = its `Name:` line).
pub fn apply(name: &str, src: String) -> Result<String, String> {
    let mut src = src;
    for &(card, from, to, _) in FIXES {
        if !crate::carddb::name_key_eq(card, name) {
            continue;
        }
        if !src.contains(from) {
            return Err(format!("script fix for {card:?} no longer applies (fragment not found): {from:?}"));
        }
        src = src.replacen(from, to, 1);
    }
    Ok(src)
}
