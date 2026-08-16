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

### arch linux

```sh
paru -S rift-clipboard-bin
systemctl --user enable --now rift.service
```

### other linux distributions

download the archive for your architecture from the [latest release](https://github.com/hyperpuncher/rift/releases/latest), then install the binary and user service:

```sh
tar -xzf rift-linux-x64.tar.gz
sudo install -Dm755 rift /usr/local/bin/rift
install -Dm644 rift.service ~/.config/systemd/user/rift.service
systemctl --user daemon-reload
systemctl --user enable --now rift.service
```

systems without systemd can start `rift daemon` from the compositor or graphical session autostart.

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

## configuration

rift creates `$XDG_CONFIG_HOME/rift/config.json`, or `~/.config/rift/config.json`, with these defaults:

```json
{
  "max_items": 300,
  "max_item_mib": 256,
  "max_history_mib": 2048,
  "stream_timeout_seconds": 3,
  "mime_retries": 1,
  "sensitive_timeout_seconds": 600
}
```

restart the daemon after changing the file.

## storage

history is stored under `$XDG_STATE_HOME/rift`, or `~/.local/state/rift`:

```text
rift/
├── index.json
└── items/<id>/
    ├── manifest.json
    └── payload-*
```

the private newline-delimited json api listens on `$XDG_RUNTIME_DIR/rift.sock`.

## license

mit
