# Home Manager module: `programs.typst-livepreview`.
self:
{ config, lib, pkgs, ... }:
let
  cfg = config.programs.typst-livepreview;
  flakePkgs = self.packages.${pkgs.stdenv.hostPlatform.system};

  zedDataDir =
    if pkgs.stdenv.hostPlatform.isDarwin
    then "${config.home.homeDirectory}/Library/Application Support/Zed"
    else "${config.xdg.dataHome}/zed";
in
{
  options.programs.typst-livepreview = {
    enable = lib.mkEnableOption "Typst live preview for Zed (language server and Zed extension)";

    package = lib.mkOption {
      type = lib.types.package;
      default = flakePkgs.typst-livepreview;
      defaultText = lib.literalExpression "zed-typst-livepreview.packages.\${system}.typst-livepreview";
      description = "The `typst-livepreview` language server, added to `home.packages`.";
    };

    extensionPackage = lib.mkOption {
      type = lib.types.package;
      default = flakePkgs.zed-extension;
      defaultText = lib.literalExpression "zed-typst-livepreview.packages.\${system}.zed-extension";
      description = "The Zed extension (`extension.toml` + `extension.wasm`) copied into Zed's extensions directory.";
    };

    zedPackage = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = flakePkgs.zed-editor;
      defaultText = lib.literalExpression "zed-typst-livepreview.packages.\${system}.zed-editor";
      description = ''
        Zed with the Typst preview pane, used as the default of
        `programs.zed-editor.package` (it does not enable `programs.zed-editor`).
        Set to `null` to leave `programs.zed-editor.package` alone.
      '';
    };
  };

  config = lib.mkIf cfg.enable (lib.mkMerge [
    {
      home.packages = [ cfg.package ];

      # Zed treats a symlinked extension as a dev extension (with a "Rebuild"
      # button that would build into the read-only store), so install a real
      # copy. The `.home-manager-source` symlink records which store path it is.
      # Recreating the directory bumps the mtime of `installed/`, which makes
      # Zed rebuild its extension index on the next start.
      home.activation.typstLivepreviewZedExtension = lib.hm.dag.entryAfter [ "writeBoundary" ] ''
        tlpExt=${lib.escapeShellArg "${zedDataDir}/extensions/installed/typst-livepreview"}
        tlpSrc=${cfg.extensionPackage}
        if [[ -L $tlpExt || ( -e $tlpExt && ! -L $tlpExt/.home-manager-source ) ]]; then
          warnEcho "typst-livepreview: $tlpExt is not managed by Home Manager (e.g. a dev extension), leaving it alone. Remove it and switch again to install the extension."
        elif [[ $(readlink "$tlpExt/.home-manager-source" 2>/dev/null) != "$tlpSrc" ]]; then
          run rm -rf "$tlpExt"
          run mkdir -p "$tlpExt"
          run cp -R "$tlpSrc/." "$tlpExt/"
          run chmod -R u+w "$tlpExt"
          run ln -s "$tlpSrc" "$tlpExt/.home-manager-source"
        fi
        unset tlpExt tlpSrc
      '';
    }
    (lib.mkIf (cfg.zedPackage != null) {
      programs.zed-editor.package = lib.mkDefault cfg.zedPackage;
    })
  ]);
}
