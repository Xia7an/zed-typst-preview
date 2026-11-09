{
  description = "Native live preview for Typst in the Zed editor";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    let
      # The language server / renderer.
      mkServer = pkgs: rustPlatform:
        let
          lib = pkgs.lib;
          isLinux = pkgs.stdenv.hostPlatform.isLinux;
          isDarwin = pkgs.stdenv.hostPlatform.isDarwin;
          linuxRuntimeLibs = with pkgs; [ libxkbcommon wayland xorg.libX11 xorg.libXcursor xorg.libXi xorg.libXrandr ];
        in
        rustPlatform.buildRustPackage {
          pname = "typst-livepreview";
          version = "0.1.0";
          src = ./server;
          cargoLock.lockFile = ./server/Cargo.lock;
          nativeBuildInputs = [ pkgs.pkg-config ] ++ lib.optionals isLinux [ pkgs.makeWrapper ];
          buildInputs = lib.optionals isDarwin [ pkgs.libiconv ]
            ++ lib.optionals isLinux ([ pkgs.openssl ] ++ linuxRuntimeLibs);
          # The tests need system fonts and a temp dir; run them with `cargo test` in the dev shell.
          doCheck = false;
          postFixup = lib.optionalString isLinux ''
            wrapProgram $out/bin/typst-livepreview \
              --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath linuxRuntimeLibs}
          '';
          meta = {
            description = "Native (non-web) live preview server for Typst, for the Zed editor";
            mainProgram = "typst-livepreview";
            license = lib.licenses.asl20;
          };
        };

      # The Zed extension (src/, extension.toml): only extension.toml and the
      # wasm component, like Zed's own `zed: install dev extension` produces.
      mkExtension = pkgs: rust:
        let
          lib = pkgs.lib;
          rustPlatform = pkgs.makeRustPlatform { cargo = rust; rustc = rust; };
        in
        pkgs.stdenv.mkDerivation {
          pname = "zed-extension-typst-livepreview";
          version = (lib.importTOML ./extension.toml).version;
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./src ./extension.toml ];
          };
          cargoDeps = rustPlatform.importCargoLock { lockFile = ./Cargo.lock; };
          nativeBuildInputs = [ rustPlatform.cargoSetupHook rust pkgs.wasm-tools ];
          buildPhase = ''
            runHook preBuild
            cargo build --release --offline --target wasm32-wasip2
            runHook postBuild
          '';
          # Strip custom sections like Zed does (it keeps only `name`, `component-type:*`,
          # `dylink.0` and the required `zed:api-version`).
          installPhase = ''
            runHook preInstall
            mkdir -p $out
            cp extension.toml $out/
            wasm-tools strip -d '^(\.debug_.*|producers|target_features|component-name)$' \
              -o $out/extension.wasm target/wasm32-wasip2/release/zed_typst_livepreview.wasm
            runHook postInstall
          '';
        };

      # Zed with the built-in Typst preview pane (zed-patch/).
      mkZed = zed-editor: zed-editor.overrideAttrs (old: {
        patches = (old.patches or [ ]) ++ [ ./zed-patch/zed-integration.patch ];
        postPatch = (old.postPatch or "") + ''
          for crate in typst_preview extension_toolbar; do
            cp -r ${./zed-patch}/$crate crates/$crate
            chmod -R u+w crates/$crate
            ln -sf ../../LICENSE-GPL crates/$crate/LICENSE-GPL
          done
        '';
        # Applied after the vendored-dependencies consistency check: the new
        # crates only use dependencies Zed already has.
        preConfigure = (old.preConfigure or "") + ''
          patch -p1 < ${./zed-patch/cargo-lock.patch}
        '';
        doCheck = false;
      });
    in
    {
      overlays.default = final: prev: {
        typst-livepreview = mkServer final final.rustPlatform;
        zed-editor = mkZed prev.zed-editor;
        # nixpkgs' rustc has no wasm32-wasip2 std; the wasm is platform independent.
        zed-extension-typst-livepreview = self.packages.${final.stdenv.hostPlatform.system}.zed-extension;
      };

      homeManagerModules.default = import ./nix/home-manager.nix self;
      homeModules.default = self.homeManagerModules.default;
    }
    // flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };
        lib = pkgs.lib;
        isLinux = pkgs.stdenv.hostPlatform.isLinux;
        isDarwin = pkgs.stdenv.hostPlatform.isDarwin;

        # Stable Rust with the target Zed uses to build extensions.
        rust = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
          targets = [ "wasm32-wasip2" ];
        };
        rustPlatform = pkgs.makeRustPlatform { cargo = rust; rustc = rust; };
        server = mkServer pkgs rustPlatform;
        linuxRuntimeLibs = with pkgs; [ libxkbcommon wayland xorg.libX11 xorg.libXcursor xorg.libXi xorg.libXrandr ];
      in
      {
        packages = {
          default = server;
          typst-livepreview = server;
          zed-editor = mkZed pkgs.zed-editor;
          zed-extension = mkExtension pkgs rust;
        };

        apps.default = flake-utils.lib.mkApp { drv = server; };

        devShells.default = pkgs.mkShell {
          packages = [ rust pkgs.pkg-config pkgs.typst ]
            ++ lib.optionals isDarwin [ pkgs.libiconv ]
            ++ lib.optionals isLinux ([ pkgs.openssl ] ++ linuxRuntimeLibs);

          LD_LIBRARY_PATH = lib.optionalString isLinux (lib.makeLibraryPath linuxRuntimeLibs);

          shellHook = ''
            echo "typst-livepreview dev shell: cargo $(cargo --version | cut -d' ' -f2), targets: host + wasm32-wasip2"
          '';
        };
      });
}
