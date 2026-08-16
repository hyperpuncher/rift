# rift

lightweight multi-format clipboard manager for wayland.

features:

- preserves every mime format in a clipboard offer
- restores files, images, text, and mixed offers correctly
- keeps the current clipboard alive after its source exits
- content-addressed filesystem history with payload deduplication
- file metadata, image dimensions, and masked sensitive entries
- private json api for custom frontends
- one native process with no gui toolkit or database

## compatibility

rift requires `wlr-data-control-unstable-v1`. supported compositors include:

- hyprland
- sway
- niri
- river
- labwc
- other wlroots-based compositors

## replaces

one rift daemon replaces this common clipboard stack:

- `wl-clip-persist` for current clipboard persistence
- `cliphist` for clipboard history
- text and image `wl-paste --watch` processes

rift does not replace `wl-clipboard`, which provides the general-purpose `wl-copy` and `wl-paste` commands.

## install

```sh
cargo install --path . --locked
rift daemon
```

## usage

| command | action |
| --- | --- |
| `rift daemon` | run the clipboard daemon |
| `rift list [--json]` | list history from newest to oldest |
| `rift show <id>` | print an item manifest |
| `rift read <id> --mime <type>` | write one raw payload to stdout |
| `rift use <id>` | restore an item with all its formats |
| `rift delete <id>` | remove one history item |
| `rift clear` | remove all history |
| `rift status` | show daemon, storage, and limit information |

unique id prefixes are accepted.

## storage

history is stored under `$XDG_STATE_HOME/rift`, or `~/.local/state/rift`:

```text
rift/
├── index.json
└── items/<id>/
    ├── manifest.json
    └── payload-*
```

defaults:

- 300 items
- 256 mib maximum per grouped item
- 2 gib maximum history
- 3 second stream timeout with one retry
- 10 minute lifetime for restored sensitive items

the private newline-delimited json api listens on `$XDG_RUNTIME_DIR/rift.sock`.

## license

mit
