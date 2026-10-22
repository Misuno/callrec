{
  description = "callrec — TUI call recorder (Rust + PipeWire + flac)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            # Rust toolchain
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer

            # pipewire-sys builds bindings via bindgen; the hook sets
            # LIBCLANG_PATH and BINDGEN_EXTRA_CLANG_ARGS
            pkg-config
            rustPlatform.bindgenHook
          ];

          buildInputs = with pkgs; [ pipewire ];

          packages = with pkgs; [
            # Runtime: FLAC encoder fed via stdin
            flac
            # Manual acceptance checks: ffprobe / ffplay, pw-record / pw-dump / pw-link
            ffmpeg
            # JSON parsing in Claude Code hooks (.claude/hooks)
            jq
            pipewire
            wireplumber
          ];

          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
          RUST_BACKTRACE = "1";
        };
      });

      packages = forAllSystems (pkgs: {
        default = pkgs.callPackage ./default.nix { };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
