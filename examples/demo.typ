#set page(paper: "a4", numbering: "1")
#set text(lang: "ja", font: ("New Computer Modern", "Hiragino Mincho ProN"))
#set heading(numbering: "1.1")

= Typst Live Preview のデモ

この文書は Zed 用のネイティブなライブプレビューを試すためのものです。
プレビューの文字をクリックすると、対応するソースの位置にジャンプします。

This is a paragraph written in English to check how quickly the preview reacts when you type into it.

== 数式

$ integral_0^infinity e^(-x^2) dif x = sqrt(pi) / 2 $

#for i in range(1, 40) [
  == 節 #i
  #lorem(80)

  $ sum_(k=1)^#i k = #(i * (i + 1) / 2) $
]
