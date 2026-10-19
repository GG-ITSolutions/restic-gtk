# restic-gtk

GTK4/libadwaita browser for multiple restic repositories, written in Rust.
Rewrite of the Python prototype in `~/repos/restic-gtk`.

Repositories are read natively with [rustic_core](https://github.com/rustic-rs/rustic_core);
no `restic` binary is needed.

## Features

- **Repository list** in the sidebar: add, edit, remove. Local paths and rest-servers
  (`rest:https://user:password@host/repository/`).
- **Unlock per repository**: the password is asked for and never stored. After unlocking only
  the decrypted key stays in memory, until you lock the repository or quit.
- **Snapshots** grouped by date (Today, Yesterday, This Week, by month) with host, paths,
  tags, size and id.
- **Repository details**: size on storage, unique data after deduplication, compression,
  number of snapshots and pack files.
- **Snapshot details**: host, user, paths, tags, full id, creating program and, if the
  snapshot records it (restic ≥ 0.17, rustic), the backup summary: size, new/changed/unchanged
  files, data added to the repository and duration.
- **File browser** per snapshot with path bar and keyboard navigation (Enter, Alt+↑, Backspace).
- **Open** a file with its default app. It is extracted read-only to
  `$XDG_RUNTIME_DIR/restic-gtk/` and removed when the app quits.
- **Restore** selected files and folders into a folder of your choice, as
  `<folder>/<name>`. Existing files that would be replaced are listed first, then the
  restore shows its progress.

## Files

| Path | Content |
|---|---|
| `~/.config/restic-gtk/repos.json` | Repository names and locations, mode `0600` (locations may contain rest-server credentials). Never repository passwords. |
| `~/.cache/restic-gtk/` | rustic's cache of encrypted index and snapshot files |
| `$XDG_RUNTIME_DIR/restic-gtk/` | Files opened from snapshots, removed on quit |

## Usage

```bash
nix run              # build and start the app
nix build            # result/bin/restic-gtk
nix develop          # dev shell: cargo, clippy, rustfmt, rust-analyzer, blueprint-compiler
cargo run            # inside the dev shell
cargo test           # backend tests against real repositories created in temp folders
```

New files must be known to git (`git add`), otherwise the flake doesn't see them.

## Layout

- `src/backend/`: repository access on top of rustic_core. Blocking and free of GTK, so it runs on
  worker threads and is tested headless (`tests/`). Exposed as library together with `src/config.rs`.
- `src/ui/`: the GTK interface. Each widget has a `mod.rs`/`imp.rs` pair and a Blueprint file in
  `data/resources/ui/`; GObject models live in `src/ui/objects/`.
- `src/application.rs`, `src/main.rs`: application setup.
- `build.rs`: compiles the Blueprint files and bundles the gresource. New `.blp` files must be
  listed there and in `data/resources/resources.gresource.xml`.
- `flake.nix`, `nix/package.nix`: Nix package (`buildRustPackage` + `wrapGAppsHook4`) and dev shell.

## Known limitations

- A running restore can't be cancelled; rustic_core offers no way to abort it.
- Restoring to non-UTF-8 target paths is not supported.
