# BulkReName
<img src="BulkReName.png" width="10%">
ファイル名を一括変更

## 機能
- `Literal` : 任意の文字列
- `Number` : 数字（ファイルリストの上から数えられる）
  - `min digits`：最小桁数
  - `init`：初期値
  - `gain`：増加量

  例：`min digits：3, init：4, gain：2`
  → 004, 006, 008, 010, 012, ...

  例：`min digits：1, init：11, gain：-3`
  → 11, 8, 5, 2, -1, ...

- `Date fmt`：日付
  - %Y：年
  - %y：年（下2桁）
  - %f：月
  - %m：月（必ず2桁）
  - %B：月（英語）
  - %b：月（略英語）
  - %e：日
  - %d：日（必ず2桁）
  - %H：時（24時間）
  - %I：時（12時間）
  - %p：AM, PM
  - %M：分（必ず2桁）
  - %S：秒（必ず2桁）
  - %s：UNIX時間

- `Orig. Name`：元のファイル名

- Save/Load Template
  テンプレートを呼び出す機能

## 備考
サポートされている形式
"png", "jpg", "jpeg", "webp", "gif", "bmp", "ico"

## 既知の不具合
- ~~インポートしたファイルのDelボタンを押すと落ちる~~（修正済）
- ファイルを追加した状態でDateフォーマットを変更すると落ちる
