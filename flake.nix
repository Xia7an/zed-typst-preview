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

      # Zed with the built-in Typst preview pane (zed-patch/).
      mkZed = zed-editor: zed-editor.overrideAttrs (old: {
        patches = (old.patches or [ ]) ++ [ ./zed-patch/zed-integration.patch ];
        postPatch = (old.postPatch or "") + ''
          cp -r ${./zed-patch/typst_preview} crates/typst_preview
          chmod -R u+w crates/typst_preview
          ln -sf ../../LICENSE-GPL crates/typst_preview/LICENSE-GPL
        '';
        # Applied after the vendored-dependencies consistency check: the new
        # crate only uses dependencies Zed already has.
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
      };
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
