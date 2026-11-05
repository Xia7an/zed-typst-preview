# Typst Live Preview for Zed

Zed で動く Typst のライブプレビューです。[tinymist](https://github.com/Myriad-Dreamin/tinymist) のプレビュー(VS Code)と同じ操作感を、Web 技術を一切使わずに実現します。

- **入力が即座に反映**: キー入力ごとにメモリ上でインクリメンタルコンパイルし、変更のあったページだけを再ラスタライズ(保存不要・デバウンスなし)
- **クリックでジャンプ**: プレビューをクリックすると、対応する `.typ` の位置にカーソルが移動
- **カーソル追従**: エディタのカーソル位置をプレビュー上に表示し、見えていなければスクロール
- **Zed の中に表示**: Zed のペイン(タブ/分割)としてプレビューを表示。描画は Zed 自身の GPU レンダラ(GPUI/Metal)

レンダリングは `typst-render`(tiny-skia による CPU ラスタライズ)→ BGRA ビットマップ → GPUI の画像描画です。HTML/SVG/WebView/WebSocket は使っていません。

## 速度

`typst-livepreview bench examples/demo.typ`(M シリーズ Mac、10 ページ、Retina 解像度相当 2.6 px/pt):

| 項目 | 中央値 |
| --- | --- |
| インクリメンタルコンパイル | 約 2 ms |
| 変更ページの再ラスタライズ | 約 1.8 ms |
| キー入力 → ビットマップ完成 | **約 5 ms** |
| ソケット経由で Zed のペインに届くまで(`tests/embed_smoke.py`) | 1 ms 未満の追加 |

画面に出るのはこの後の次のフレーム(VSync)です。

## 構成

```
Zed (パッチ適用済み)                         typst-livepreview (言語サーバー)
┌──────────────────────────────┐  LSP (stdio)  ┌─────────────────────────────────┐
│ エディタ ── didChange/codeAction ───────────▶│ core: typst の World に編集を適用 │
│                              │               │       → インクリメンタルコンパイル │
│ Typst Preview ペイン (GPUI)  │  Unix socket  │ render: 変更ページを並列ラスタライズ│
│  表示/スクロール/ズーム ◀── bitmap/cursor ──│                                 │
│  クリック/カーソル ── click/cursor ────────▶│ jump: typst-ide でソース位置を解決 │
└──────────────────────────────┘               └─────────────────────────────────┘
```

| ディレクトリ | 内容 |
| --- | --- |
| `server/` | 言語サーバー兼レンダラ `typst-livepreview`(Rust, ネイティブ) |
| `zed-patch/` | Zed に Typst プレビューペインを追加するパッチ(`typst_preview` クレート) |
| `src/`, `extension.toml` | Zed 拡張機能(言語サーバーを起動するだけの WASM) |
| `flake.nix` | ビルドに必要なツール一式・サーバー・パッチ適用済み Zed |
| `docs/PROTOCOL.md` | Zed のペインとサーバー間のプロトコル |

Zed の拡張機能 API では独自の描画ペインを作れないため、Zed 本体に小さなパッチを当てています。パッチなしの Zed でも動きますが、その場合プレビューは別ウィンドウ(winit + softbuffer)になります。

## インストール (Nix)

### home-manager モジュール

```nix
# flake.nix の inputs
inputs.zed-typst-livepreview.url = "git+ssh://git@github.com/Xia7an/zed-typst-preview";  # private なので SSH で取得

# home.nix
{ inputs, ... }:
{
  imports = [ inputs.zed-typst-livepreview.homeManagerModules.default ];

  programs.typst-livepreview.enable = true;
  programs.zed-editor.enable = true;  # Zed 自体の設定はこれまで通り
}
```

`programs.typst-livepreview.enable = true` で次のことが行われます。

- 言語サーバー `typst-livepreview` を `home.packages` に追加(PATH に置く)
- Zed 拡張機能(`extension.toml` とビルド済みの `extension.wasm`)を Zed の拡張機能ディレクトリにコピー
  - macOS: `~/Library/Application Support/Zed/extensions/installed/typst-livepreview`
  - Linux: `~/.local/share/zed/extensions/installed/typst-livepreview`
  - 中身が変わったときだけ置き換えます。Zed は起動中なら即座に、そうでなければ次回起動時に読み込みます
- `programs.zed-editor.package` の既定値をパッチ適用済み Zed にする(`lib.mkDefault` なので、自分で指定した値が優先されます。`programs.zed-editor` 自体は有効化しません)

| オプション(`programs.typst-livepreview.*`) | 既定値 | 内容 |
| --- | --- | --- |
| `enable` | `false` | 有効化 |
| `package` | この flake の `typst-livepreview` | 言語サーバー |
| `extensionPackage` | この flake の `zed-extension` | Zed 拡張機能 |
| `zedPackage` | この flake の `zed-editor` | `programs.zed-editor.package` に設定する Zed。`null` なら設定しない |

> **以前 `zed: install dev extension` でインストールしていた場合**: `installed/typst-livepreview` がこのリポジトリへのシンボリックリンクになっています。モジュールはこれを上書きせず警告を出すだけなので、一度だけ削除してから `home-manager switch` し直してください(シンボリックリンクだけが消え、リポジトリは消えません)。
>
> ```sh
> rm ~/Library/Application\ Support/Zed/extensions/installed/typst-livepreview   # macOS
> rm ~/.local/share/zed/extensions/installed/typst-livepreview                   # Linux
> ```

> パッチ適用済み Zed はバイナリキャッシュにないため、手元でソースからビルドされます。初回は 1 時間前後かかり、ビルド中に **40 GB 程度の空きディスク** が必要です。

既存の Typst 拡張機能(tinymist)と併用する前提です(補完・診断は tinymist、プレビューはこの拡張機能)。

### パッケージを個別に使う場合

モジュールを使わずに、パッケージだけを参照することもできます(この場合、拡張機能は下の dev extension の手順で入れます)。

```nix
# home.nix
{ inputs, pkgs, ... }:
let tlp = inputs.zed-typst-livepreview.packages.${pkgs.stdenv.hostPlatform.system}; in
{
  programs.zed-editor.package = tlp.zed-editor;   # Zed + Typst プレビューペイン
  home.packages = [ tlp.typst-livepreview ];      # 言語サーバー (PATH に置く)
}
```

または overlay(`overlays.default`)で `zed-editor` と `typst-livepreview` を差し替えられます(拡張機能は `zed-extension-typst-livepreview`)。

単体で試す場合:

```sh
nix build .#zed-editor        # パッチ適用済み Zed → ./result
nix build .#typst-livepreview # サーバー → ./result/bin/typst-livepreview
nix build .#zed-extension     # 拡張機能 → ./result/{extension.toml,extension.wasm}
```

### 開発用: dev extension としてインストール

拡張機能(`src/`)を手元で変更しながら試す場合は、Zed のコマンドパレットで `zed: install dev extension` を実行し、このリポジトリのディレクトリを選びます。home-manager モジュールがコピーした `installed/typst-livepreview` がある場合は、先にそれを削除してください(dev extension の間、モジュールは警告を出すだけで何もしません)。

Zed は拡張機能を `wasm32-wasip2` 向けにビルドするので、そのターゲット入りの Rust が必要です。開発シェルに入っているので、**Zed を一度終了してから**開発シェル経由で起動してインストールしてください:

```sh
nix develop -c zeditor .
```

## 使い方

| 操作 | 方法 |
| --- | --- |
| プレビューを開く | `.typ` を開いてツールバーの 👁 ボタン(分割で開く / Alt+クリックで同じペイン)、コマンドパレットの `typst preview: open preview to the side`、またはコードアクション(`Cmd+.`)の **Open Typst Live Preview** |
| ソースへジャンプ | プレビューをクリック |
| スクロール | ホイール / トラックパッド / ドラッグ / 矢印・PageUp/Down・Space |
| ズーム | `Cmd` + ホイール、ピンチ、`Cmd +` / `Cmd -`、`Cmd 0`(幅に合わせる) |

複数ファイルのプロジェクトでは、`#include` されたファイルを編集している間もメインファイルのプレビューが維持されます。依存関係にないファイルに移ると、プレビューはそのファイルに切り替わります(固定したい場合は `main` を設定)。

コンパイルエラーの間は最後に成功した結果を表示したまま、ペイン下部にエラーを表示します。

## 設定

`settings.json`:

```jsonc
{
  "lsp": {
    "typst-livepreview": {
      "settings": {
        "main": "main.typ",          // 常にこのファイルをプレビュー(既定: 編集中のファイルに追従)
        "root": null,                // プロジェクトルート(既定: ワークスペースフォルダ)
        "follow_cursor": true,       // カーソル追従
        "invert_colors": false,      // ダークモード風(色相を保ったまま明度を反転)
        "font_paths": [],            // 追加のフォントディレクトリ
        "ignore_system_fonts": false,
        "auto_open": false,          // Typst ファイルを開いたら自動でプレビュー
        // 以下はパッチなし Zed(別ウィンドウ)のときのみ
        "always_on_top": false,
        "focus_editor_on_jump": true,
        "background": "#525659"
      },
      // PATH 以外の場所にあるサーバーを使う場合
      "binary": { "path": "/path/to/typst-livepreview" }
    }
  }
}
```

## 開発

```sh
nix develop                      # Rust (wasm32-wasip2 込み), libiconv, typst
cd server
cargo test                       # 単体テスト(クリック⇔カーソルの対応、差分編集など)
cargo build --release
./target/release/typst-livepreview bench ../examples/demo.typ    # レイテンシ計測
./target/release/typst-livepreview preview ../examples/demo.typ  # 別ウィンドウで単体起動
python3 -I tests/embed_smoke.py target/release/typst-livepreview # Zed ペインのプロトコル試験
CLICK=600,130 python3 -I tests/lsp_smoke.py target/release/typst-livepreview  # LSP 経由の通し試験
```

デバッグ用の環境変数: `RUST_LOG=typst_livepreview=debug`(「edit → on screen」の遅延を記録)、`TYPST_LIVEPREVIEW_NO_EMBED=1`(常に別ウィンドウ)。

Zed のパッチを更新するときは、Zed v1.26.2 のソースの `crates/` に `zed-patch/typst_preview` をコピーし、`zed-integration.patch` と `cargo-lock.patch` を当ててから、nix のビルド環境で差分ビルドすると速いです(初回 20〜30 分、以降は 1 分程度):

```sh
nix develop /path/to/zed-typst-livepreview#zed-editor -c bash -c \
  'unset LK_CUSTOM_WEBRTC; CARGO_PROFILE_DEV_DEBUG=0 cargo build -p zed --features gpui_platform/runtime_shaders'
python3 -I zed-patch/tests/pane_test.py target/debug/zed /tmp/tlp-pane      # 偽サーバーでペインを試験
python3 -I zed-patch/tests/e2e_test.py target/debug/zed <server> <extension.wasm> /tmp/tlp-e2e  # 実サーバーで通し試験
```

ペインのログは `ZED_LOG=typst_preview=debug` で `~/Library/Logs/Zed/Zed.log` に出ます。
