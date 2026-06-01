# Othello Full Search Database Builder

8x8 Othello/Reversi の合法手を枝切りなしで全展開し、SQLite データベースに保存する研究用CLIです。中断した場合も、DB内の `frontier` と `reach_count` から再開できます。

重要: 8x8 Othello の完全なゲーム木は非常に巨大です。このプログラムは「正確に全展開し、途中で止めても続きから実行できる」ためのものです。普通のPCで全完了するサイズにはなりません。

## 使い方

```powershell
cd E:\Chess\Othello
cargo build --release
```

DBを初期化します。

```powershell
.\target\release\othello_full_search.exe init --reset
```

探索を進めます。`Ctrl+C` で止めても、最後にコミット済みの位置から再開できます。

```powershell
.\target\release\othello_full_search.exe run --batch 100 --max-seconds 3600
```

進捗確認:

```powershell
.\target\release\othello_full_search.exe status
```

再開:

```powershell
.\target\release\othello_full_search.exe run --batch 100 --max-seconds 3600
```

DBパスを変える場合:

```powershell
.\target\release\othello_full_search.exe --db E:\Chess\Othello\data\search.sqlite3 init
.\target\release\othello_full_search.exe --db E:\Chess\Othello\data\search.sqlite3 run --batch 1000
```

## DB内容

- `positions`: 局面、手番、空きマス数、合法手数、到達回数、伝播済み到達回数、完全値 `value_black`
- `edges`: `parent_key -> child_key` の合法手グラフ。`move = 64` はパス
- `frontier`: まだ子へ伝播する必要がある局面
- `terminal_counts`: 終局スコア別のゲーム経路数

`reach_count` は同じ局面に到達するゲーム経路数です。64bitを超えるため、10進文字列の任意精度整数として保存します。

## AI用の値

探索済みの終局局面には `value_black = 黒石数 - 白石数` が入ります。全子局面の値がそろった局面は、次で後退解析できます。

```powershell
.\target\release\othello_full_search.exe retrograde --batch 1000
```

黒番は `value_black` を最大化、白番は最小化します。途中までのDBでも、子がすべて解決済みの局面から順に値が入ります。

## 検証

合法手生成の基本検証:

```powershell
cargo test
.\target\release\othello_full_search.exe perft --depth 6
```

初期局面の perft は深さ6で `8200` です。

## CSV出力

```powershell
.\target\release\othello_full_search.exe export-csv --out positions.csv
```

