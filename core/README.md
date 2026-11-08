# steam-vent-core

This contains a set of traits and types extracted from `steam-vent`.

The goal of having these is a separate crate is to allow high-level API crates, like [steam-vent-chat](https://codeberg.org/steam-vent/chat),
to be implemented against a much smaller API service than the full `steam-vent` public API.

This in turn allow bumping the `steam-vent` version without requiring all high-level API crates to also create new releases for it.
