# Typst Live Preview for Zed

Zed で動く Typst のライブプレビューです。[tinymist](https://github.com/Myriad-Dreamin/tinymist) のプレビュー(VS Code)と同じ操作感を、Web 技術を一切使わずに実現します。

- **入力が即座に反映**: キー入力ごとにメモリ上でインクリメンタルコンパイルし、レイアウトの差分から見た目が変わった帯(数行分)だけを再ラスタライズ・転送(保存不要・デバウンスなし)
- **クリックでジャンプ**: プレビューをクリックすると、対応する `.typ` の位置にカーソルが移動
- **カーソル追従**: エディタのカーソル位置をプレビュー上に表示し、見えていなければスクロール
- **Zed の中に表示**: Zed のペイン(タブ/分割)としてプレビューを表示。描画は Zed 自身の GPU レンダラ(GPUI: macOS は Metal、Linux は wgpu/Vulkan)
- **macOS / Linux 対応**: どちらでもパッチ適用済み Zed のペインで動きます(Linux は X11 / Wayland)

レンダリングは `typst-render`(tiny-skia による CPU ラスタライズ)→ BGRA ビットマップ → GPUI の画像描画です。HTML/SVG/WebView/WebSocket は使っていません。

## 速度

`typst-livepreview bench examples/demo.typ`(M シリーズ Mac、10 ページ、Retina 解像度相当 2.6 px/pt):

| 項目 | 中央値 |
| --- | --- |
| インクリメンタルコンパイル | 約 2 ms |
| レイアウト差分(変わった帯の検出) | 約 0.1 ms |
| 変わったストリップの再ラスタライズ | 約 0.3 ms(ページ全体なら約 1.8 ms) |
| 1 文字入力で描き直す画素 | 約 1 MB(ページ全体なら約 13 MB) |
| キー入力 → ビットマップ完成 | **約 3.4 ms**(ページ全体なら約 4.8 ms) |
| キー入力 → 変わったストリップがソケットで届くまで(`tests/embed_smoke.py`、ペインからの要求なし) | 約 1.2 ms |

画面に出るのはこの後の次のフレーム(VSync)です。

### 差分コンパイル・差分レンダリング

キー入力 1 回あたりの処理は次の通りです。

1. **差分コンパイル**: 編集をソースに適用して部分的に再パースし、typst の comemo キャッシュで変わっていない部分の評価・レイアウトを再利用します。
2. **レイアウト差分** (`server/src/diff.rs`): ページのハッシュが変わったページについて、前回と今回のフレームを要素単位で比較します(ソース位置などの見た目に影響しない情報は無視し、同じ位置の段落などのグループは再帰的に比較)。見た目が変わった要素の外接矩形から、ページ内の「変わった帯」を求めます。
3. **ストリップ単位のレンダリング**: プレビューは各ページを高さ 128 デバイスピクセルの横長ストリップ(GPU テクスチャ)の並びとして持ちます。サーバーは変わった帯にかかるストリップだけを、その帯にかかる要素だけでラスタライズします。
4. **先回りの送信**: 変わったストリップは、ペインが直前に表示していた解像度でサーバーがすぐに描いて送ります(ペインからの要求を待たない)。ペインは変わっていないストリップを前のバージョンからコピーなしで使い回します。

結果として、1 文字の入力でラスタライズ・ソケット転送・GPU アップロードされるのはページ全体(A4 で約 13 MB)ではなく、通常 1〜2 本のストリップ(約 1 MB)です。`bench` は毎回、ストリップの画素がページ全体を描いた場合と一致すること、ストリップ以外の画素が前のバージョンと一致することも検証します。

## 構成

```
Zed (パッチ適用済み)                         typst-livepreview (言語サーバー)
┌──────────────────────────────┐  LSP (stdio)  ┌─────────────────────────────────┐
│ エディタ ── didChange/codeAction ───────────▶│ core: typst の World に編集を適用 │
│                              │               │       → インクリメンタルコンパイル │
│ プレビューペイン (GPUI)      │  Unix socket  │ diff/render: 変わった帯だけを描画 │
│  表示/スクロール/ズーム ◀── bitmap/cursor ──│                                 │
│  クリック/カーソル ── click/cursor ────────▶│ jump: typst-ide でソース位置を解決 │
└──────────────────────────────┘               └─────────────────────────────────┘
```

| ディレクトリ | 内容 |
| --- | --- |
| `server/` | 言語サーバー兼レンダラ `typst-livepreview`(Rust, ネイティブ) |
| `zed-patch/` | Zed へのパッチ: 外部の描画サーバー用のプレビューペイン(`external_preview`)と、拡張機能が宣言するツールバーボタン(`extension_toolbar`) |
| `src/`, `extension.toml` | Zed 拡張機能(言語サーバーを起動するだけの WASM) |
| `flake.nix` | ビルドに必要なツール一式・サーバー・パッチ適用済み Zed |
| `docs/PROTOCOL.md` | Zed のペインとサーバー間のプロトコル |

Zed の拡張機能 API では独自の描画ペインを作れないため、Zed 本体に小さなパッチを当てています。パッチなしの Zed でも動きますが、その場合プレビューは別ウィンドウ(winit + softbuffer)になります。ウィンドウを開けない環境(Linux で `WAYLAND_DISPLAY` も `DISPLAY` も無いなど)では、プレビューなしで言語サーバーとして動き続けます(PDF 出力は使えます)。

パッチのペインとボタンは Typst 専用ではありません。描画サーバーは `hello` で「何を描くか(`kind`)・どの拡張子か・レイアウト(固定 / ペイン幅で組み直す)」を名乗り、ページのビットマップを送ります。Markdown など別の形式のプレビューも、Zed を変更せずに拡張機能(サーバーとボタンの宣言)と描画サーバーだけで作れます。書き方は `docs/PROTOCOL.md` の「Writing a preview server」を参照してください。

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
  programs.zed-editor.package = tlp.zed-editor;   # Zed + プレビューペイン
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
| プレビューを開く | `.typ` を開いてツールバーの 👁 ボタン(分割で開く / Alt+クリックで同じペイン)、コマンドパレットの `external preview: open preview to the side`、またはコードアクション(`Cmd+.`、Linux は `Ctrl+.`)の **Open Typst Live Preview** |
| PDF を書き出す | ツールバーの PDF ボタン(👁 の左)。メインファイルの隣に `<メインファイル名>.pdf` を書き出します(保存前の編集も反映)。パッチなしの Zed ではコードアクションの **Export PDF** |
| ソースへジャンプ | プレビューをクリック |
| スクロール | ホイール / トラックパッド / ドラッグ / 矢印・PageUp/Down・Space |
| ズーム | `Cmd` + ホイール、ピンチ、`Cmd +` / `Cmd -`、`Cmd 0`(幅に合わせる)。Linux では `Cmd` の代わりに `Ctrl` |

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
# Linux(拡大率 1)では CLICK=300,65。ディスプレイが無ければ xvfb-run -a を前に付ける
```

デバッグ用の環境変数: `RUST_LOG=typst_livepreview=debug`(「edit → on screen」の遅延を記録)、`TYPST_LIVEPREVIEW_NO_EMBED=1`(常に別ウィンドウ)。

Zed のパッチを更新するときは、Zed v1.26.2 のソースの `crates/` に `zed-patch/external_preview` と `zed-patch/extension_toolbar` をコピーし、`zed-integration.patch` と `cargo-lock.patch` を当ててから、nix のビルド環境で差分ビルドすると速いです(初回 20〜30 分、以降は 1 分程度):

```sh
nix develop /path/to/zed-typst-livepreview#zed-editor -c bash -c \
  'unset LK_CUSTOM_WEBRTC; CARGO_PROFILE_DEV_DEBUG=0 cargo build -p zed --features gpui_platform/runtime_shaders'
# Linux では --features gpui_platform/runtime_shaders(macOS の Metal 用)は不要
python3 -I zed-patch/tests/pane_test.py target/debug/zed /tmp/tlp-pane      # 偽サーバー(Typst v2 互換 + Markdown 風 reflow)でペインを試験
python3 -I zed-patch/tests/e2e_test.py target/debug/zed <server> <extension.wasm> /tmp/tlp-e2e  # 実サーバーで通し試験
```

ペインのログは `ZED_LOG=external_preview=debug` で `~/Library/Logs/Zed/Zed.log`(Linux: `~/.local/share/zed/logs/Zed.log`、`--user-data-dir` 指定時はその下の `logs/Zed.log`)に出ます。
