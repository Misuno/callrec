{
  lib,
  rustPlatform,
  pkg-config,
  pipewire,
  flac,
  makeWrapper,
}:

rustPlatform.buildRustPackage {
  pname = "callrec";
  version = "0.1.0";

  src = lib.fileset.toSource {
    root = ./.;
    fileset = lib.fileset.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./clippy.toml
      ./src
    ];
  };
  cargoLock.lockFile = ./Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    # The pipewire crate generates bindings via bindgen
    rustPlatform.bindgenHook
    makeWrapper
  ];
  buildInputs = [ pipewire ];

  # Tests spawn the real `flac` encoder
  nativeCheckInputs = [ flac ];

  postInstall = ''
    wrapProgram $out/bin/callrec --prefix PATH : ${lib.makeBinPath [ flac ]}
  '';

  meta = {
    description = "TUI call recorder: default mic and speaker into one stereo FLAC";
    homepage = "https://github.com/Misuno/callrec";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
    mainProgram = "callrec";
  };
}
