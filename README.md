# restic-gtk

GTK4/libadwaita browser for multiple restic repositories, written in Rust.
Rewrite of the Python prototype in `~/repos/restic-gtk`.

## Usage

```bash
nix run              # build and start the app
nix build            # result/bin/restic-gtk
nix develop          # dev shell: cargo, clippy, rustfmt, rust-analyzer, blueprint-compiler
cargo run            # inside the dev shell
```

New files must be known to git (`git add`), otherwise the flake doesn't see them.

## Layout

- `flake.nix`, `nix/package.nix`: Nix package (`buildRustPackage` + `wrapGAppsHook4`) and dev shell
- `build.rs`: compiles `data/resources/ui/*.blp` with blueprint-compiler and bundles the gresource
- `src/`: application code; `src/window/` is the main window (composite template)
- `data/`: resources, `.desktop` file
