{
  lib,
  rustPlatform,
  pkg-config,
  wrapGAppsHook4,
  blueprint-compiler,
  gtk4,
  libadwaita,
}:

let
  cargoToml = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = cargoToml.package.name;
  inherit (cargoToml.package) version;

  # Only the files cargo needs, so edits to docs or .claude/ don't trigger rebuilds.
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../build.rs
      ../src
      ../data
    ];
  };

  cargoLock.lockFile = ../Cargo.lock;

  strictDeps = true;

  nativeBuildInputs = [
    pkg-config
    wrapGAppsHook4
    blueprint-compiler
  ];

  buildInputs = [
    gtk4
    libadwaita
  ];

  postInstall = ''
    install -Dm644 -t $out/share/applications data/de.gg_itsystemhaus.ResticGtk.desktop
  '';

  meta = {
    description = "GTK4/libadwaita browser for multiple restic repositories";
    mainProgram = "restic-gtk";
    platforms = lib.platforms.linux;
  };
}
