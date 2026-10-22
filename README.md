# restic-gtk

GTK4/libadwaita browser for multiple restic repositories, written in Rust.
Rewrite of the Python prototype in `~/repos/restic-gtk`.

Repositories are read natively with [rustic_core](https://github.com/rustic-rs/rustic_core);
no `restic` binary is needed.

## Features

- **Repository list** in the sidebar: add, edit, remove. Local paths, rest-servers
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
- **Disk images** (`.img`, `.raw`, `.dd`, and the VM disk formats `.qcow2`, `.vhd`, `.vhdx`,
  `.vmdk`) open like folders, without downloading them: first
  the partitions (GPT or MBR), then the files of NTFS and FAT volumes. Opening a file reads only
  its blocks; files Windows stores compressed (CompactOS/WOF with XPRESS or LZX, NTFS
  compression) are decompressed. qcow2 (also compressed), VHD and VHDX (fixed and dynamic) and
  VMDK (sparse, flat and split files next to their descriptor) are read in place; disks that need
  a parent image (backing files, differencing disks, VMware snapshots) or a VHDX with a
  log to replay show why they can't be opened. Drive letters come from the `system.json` that
  `backup-windows` writes next to its images. Files, folders and whole partitions inside images
  can be restored as well, with their content and modification time; links are left out, and
  so are files that can't be read (EFS-encrypted, deduplicated, WIMBoot).
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

`tests/real_image.rs` browses a real Windows image backup and is ignored by default; its
module docs say how to run it. The fixtures in `tests/fixtures/` come with the scripts that
generate them.

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
- Disk images: ext2/3/4 and LVM, BitLocker and LUKS and compressed images (`.img.gz`) can't be
  browsed yet. Of the VM disk formats, qcow2 with a backing file, VHD/VHDX differencing disks,
  VMDK snapshots, encrypted or extended-L2 qcow2 and VHDX files with a log to replay are
  refused. The extent files of a VMDK (`-flat`, `-s001`, ...) show up as images of their own.
  Restoring from images keeps no permissions, owners or
  alternate data streams. Changes still in the NTFS journal of an image of a running
  system are not applied.
- Fragmented files in disk images are slow to open: every scattered piece fetches a whole blob
  (about 1 MiB) from the repository. Logs with NTFS compression are typical, a 79 MB Windows
  CBS log took about 90 s from a rest-server over the internet.
