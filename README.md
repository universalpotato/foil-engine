# Foil engine

A Rust rules engine for Magic: The Gathering (Standard).

- `mtg-core/`: game state, rules, card compilation.
- `forge-script/`: parser for Forge card scripts.

## Usage

The following snippet plays one game between two decks, printing the options at each decision and taking the first.

    use mtg_core::deck::DeckList;
    use mtg_core::testutil::describe;
    use mtg_core::{CardDb, Game};
    use std::{path::Path, sync::Arc};

    let a = DeckList::load(Path::new("a.txt"))?;    // lines of "4 Card Name"
    let b = DeckList::load(Path::new("b.txt"))?;

    let mut names = a.card_names();
    names.extend(b.card_names());
    let db = Arc::new(CardDb::load_forge(Path::new("forge/forge-gui/res/cardsfolder"), &names)?);

    let mut game = Game::new(db.clone(), [&a.resolve(&db)?, &b.resolve(&db)?], 7);    // 7 is the seed
    while let Some(d) = game.pending() {
        for o in &d.options {
            println!("{}", describe(&game, o));
        }
        game.submit(0)?;
    }
    println!("{:?}", game.result());

Games are deterministic by seed.

## Performance

About 30,000 decisions per second per core. CPU time per decision:

| Step | Time |
| :---- | :---- |
| Advance the game to the next decision | 33 µs |
| Build the player's view | 6.4 µs |
| Encode the view into arrays | 2.9 µs |
| Copy a game for search | 1.4 µs |
| Deal out hidden cards for search | 40 µs |

## License

GPL-3.

## Attribution

Card behaviour is sourced from Forge's card scripts.
