# VideoScope_H（ビデオスコープ）

今のフレームの輝度の波形・RGB パレード・RGB の重ね・ベクトルスコープ・ヒストグラムをウィンドウに描く AviUtl2 の汎用プラグイン。
プロジェクトは書き換えない。YMM4 の MuchaMonitor・スコープモニターに当たる。

- 仕様: `AI/specifications/20261009_VideoScope_H_spec.md`
- 本体 2.1.11 以降（aviutl2-rs 0.48）

## ビルドと配置

```bash
python AI/tools/au2_build.py VideoScope_H                 # 本番へ
python AI/tools/au2_build.py VideoScope_H --no-deploy     # 本番に書かず、違いだけ出す
```

## 作りの要点

| ファイル | 中身 |
|---|---|
| `src/scope.rs` | 背景への合成と間引き、BT.709 の輝度・色差、波形・パレード・重ね・ベクトルスコープ・ヒストグラムの描画と目盛り（16 / 235・レンジ警告_H のしきい値の線を含む。cargo test の中心） |
| `src/worker.rs` | 集計のスレッド。受け取った画素とオプションから絵を作る。プラグインの `Drop` で止めて終わりを待つ |
| `src/request.rs` | 止まっているとき: 本体への描画の頼み方（`rendering_scene_video`）。間隔 100ms 以上・重ねない・2 秒で諦める。再生中: 受け口へ「欲しい」の期限（600ms 先）を書き、届いた 1 枚を集計へ渡す。再生が終わるたびにログへ枚数と読み出しの時間を 1 行 |
| `src/share.rs` | 再生中のフレームの受け渡し（名前付きの共有メモリ。番号の偶奇で書きかけを捨てる）。**`AI/plugins/FullSizePreview_H/src/share.rs` と同じ内容**（公開リポジトリが別なので共通のクレートにしていない。直すときは両方） |
| `src/gui.rs` | ウィンドウ（egui） |

- 本体を呼ぶのは `rendering_scene_video` だけ（`call_edit_section` は使わない）
- 本体のイベント（フレームの移動・オブジェクトの更新・シーンの変更）では印を立てるだけで、頼むのは UI の側
- ウィンドウが見えていない間・「有効にする」がオフの間は頼まない
- 再生中は、FullSizePreview_H（v0.6.0 以降）の受け口が読んだフレームバッファを受け取る（受け口を共有するのはユーザーの決定）。受け口が無ければ下の行に案内を出す
