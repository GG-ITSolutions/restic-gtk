# restic-gtk

GTK4/libadwaita browser for multiple restic repositories, written in Rust.
Rewrite of the Python prototype in `~/repos/restic-gtk`.

Repositories are read natively with [rustic_core](https://github.com/rustic-rs/rustic_core);
no `restic` binary is needed.

## Features

- **Repository list** in the sidebar: add, edit, remove, reorder by drag and drop or with
  "Move Up" / "Move Down" in the context menu. Local paths, rest-servers
  (`rest:https://user:password@host/repository/`) and S3 or S3-compatible storage (AWS, Hetzner,
  Wasabi, MinIO, …) with endpoint, bucket, path, region and access key ID. Pasting a restic URL
  such as `s3:https://host/bucket/path` fills in the S3 fields.
- **Unlock per repository**: the password, and for S3 the secret key, is asked for and never
  stored. After unlocking only the decrypted key, and the secret key, stay in memory, until you
  lock the repository or quit.
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
  `<folder>/<name>`. Before existing files are replaced, their number is shown and must be
  confirmed, then the restore shows its progress. A folder, file or link of another type in
  the way stops the restore before anything is written.

## Files

| Path | Content |
|---|---|
| `~/.config/restic-gtk/repos.json` | Repository names, locations and S3 settings, mode `0600` (locations may contain rest-server credentials). Never repository passwords or S3 secret keys. An invalid file is moved to `repos.json.broken`; an unreadable one is kept and the list is not saved until restart. |
| `~/.cache/restic-gtk/` | rustic's cache of encrypted index and snapshot files |
| `$XDG_RUNTIME_DIR/restic-gtk/` | Files opened from snapshots, removed on quit. Without `XDG_RUNTIME_DIR` (`sudo`, SSH) `~/.cache/restic-gtk/opened/`, never the shared `/tmp`. |

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

- S3 settings come only from the dialog, never from `AWS_*` variables or `~/.aws`. AWS needs the
  region of the bucket unless the endpoint names it (`s3.eu-central-1.amazonaws.com`); other
  services work with an empty region.
- An unreachable endpoint fails only after about a minute, rustic retries every request.
- A running restore can't be cancelled; rustic_core offers no way to abort it.
- Restoring to non-UTF-8 target paths is not supported.
