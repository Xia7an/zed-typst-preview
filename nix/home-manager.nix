# Home Manager module: `programs.typst-livepreview` and `programs.tex-livepreview`.
self:
{ config, lib, pkgs, ... }:
let
  flakePkgs = self.packages.${pkgs.stdenv.hostPlatform.system};

  zedDataDir =
    if pkgs.stdenv.hostPlatform.isDarwin
    then "${config.home.homeDirectory}/Library/Application Support/Zed"
    else "${config.xdg.dataHome}/zed";

  # Zed treats a symlinked extension as a dev extension (with a "Rebuild"
  # button that would build into the read-only store), so install a real
  # copy. The `.home-manager-source` symlink records which store path it is.
  # Recreating the directory bumps the mtime of `installed/`, which makes
  # Zed rebuild its extension index on the next start.
  installExtension = id: src: ''
    tlpExt=${lib.escapeShellArg "${zedDataDir}/extensions/installed/${id}"}
    tlpSrc=${src}
    if [[ -L $tlpExt || ( -e $tlpExt && ! -L $tlpExt/.home-manager-source ) ]]; then
      warnEcho "${id}: $tlpExt is not managed by Home Manager (e.g. a dev extension), leaving it alone. Remove it and switch again to install the extension."
    elif [[ $(readlink "$tlpExt/.home-manager-source" 2>/dev/null) != "$tlpSrc" ]]; then
      run rm -rf "$tlpExt"
      run mkdir -p "$tlpExt"
      run cp -R "$tlpSrc/." "$tlpExt/"
      run chmod -R u+w "$tlpExt"
      run ln -s "$tlpSrc" "$tlpExt/.home-manager-source"
    fi
    unset tlpExt tlpSrc
  '';

  # Options shared by both previews.
  previewOptions = { what, server, extension }: {
    enable = lib.mkEnableOption "${what} live preview for Zed (language server and Zed extension)";

    package = lib.mkOption {
      type = lib.types.package;
      default = flakePkgs.${server};
      defaultText = lib.literalExpression "zed-typst-livepreview.packages.\${system}.${server}";
      description = "The `${server}` language server, added to `home.packages`.";
    };

    extensionPackage = lib.mkOption {
      type = lib.types.package;
      default = flakePkgs.${extension};
      defaultText = lib.literalExpression "zed-typst-livepreview.packages.\${system}.${extension}";
      description = "The Zed extension (`extension.toml` + `extension.wasm`) copied into Zed's extensions directory.";
    };

    zedPackage = lib.mkOption {
      type = lib.types.nullOr lib.types.package;
      default = flakePkgs.zed-editor;
      defaultText = lib.literalExpression "zed-typst-livepreview.packages.\${system}.zed-editor";
      description = ''
        Zed with the preview pane, used as the default of
        `programs.zed-editor.package` (it does not enable `programs.zed-editor`).
        Set to `null` to leave `programs.zed-editor.package` alone.
      '';
    };
  };

  typst = config.programs.typst-livepreview;
  tex = config.programs.tex-livepreview;
  # The patched Zed both previews ask for (the Typst one wins if both set one).
  zedPackage =
    if typst.enable && typst.zedPackage != null then typst.zedPackage
    else if tex.enable && tex.zedPackage != null then tex.zedPackage
    else null;
in
{
  options.programs.typst-livepreview = previewOptions {
    what = "Typst";
    server = "typst-livepreview";
    extension = "zed-extension";
  };

  options.programs.tex-livepreview = previewOptions {
    what = "LaTeX";
    server = "tex-livepreview";
    extension = "zed-extension-tex";
  };

  config = lib.mkMerge [
    (lib.mkIf typst.enable {
      home.packages = [ typst.package ];
      home.activation.typstLivepreviewZedExtension =
        lib.hm.dag.entryAfter [ "writeBoundary" ] (installExtension "typst-livepreview" typst.extensionPackage);
    })
    (lib.mkIf tex.enable {
      home.packages = [ tex.package ];
      home.activation.texLivepreviewZedExtension =
        lib.hm.dag.entryAfter [ "writeBoundary" ] (installExtension "tex-livepreview" tex.extensionPackage);
    })
    (lib.mkIf (zedPackage != null) {
      programs.zed-editor.package = lib.mkDefault zedPackage;
    })
  ];
}
