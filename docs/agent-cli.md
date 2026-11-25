# エージェントから CLI で項目を扱う

Codex や Claude Code などのエージェントが、lazygtd の項目を作る・読む・直す・閉じるための使い方。
TUI を開かずに、1 回のコマンドで 1 つの書き込みをし、結果を JSON で受け取る。

## 決まり

- 書き込みのコマンドは、`add` を除いて `ID REV` を取る。REV が今の revision と違えば、
  何も変えずに exit 3 で終わる。`show ID` で読み直してから、やり直すかを決める。
- 書き込みが成功すると、結果の項目を 1 行の JSON で標準出力に書く。次の書き込みには、
  その `revision` を使う。中身が変わらない書き込み（同じ題名への `retitle` など）は、
  revision を上げずに今の項目を返す。
- 失敗は exit 1（revision の食い違いだけ exit 3）で、理由は標準エラーに出る。
- 項目の変更と履歴（event）は、1 つのトランザクションでまとめて書く。途中で失敗したら何も残らない。
- 書き込みに `--by NAME` を付けると、event の actor に `agent:NAME` として残る。付けなければ `user`
  （`reply` は `agent`、`watch-resolve` は `system:watch`）。NAME は 1〜64 文字の ASCII 英数字と `-` `_` `.`。
- `--` のあとの引数は、`--` で始まってもオプションとして読まない。
- `--db PATH` はコマンドの前に置く。エージェントごとに別の DB を使うときも同じ。

## 作る

```sh
lazygtd add "レビューの返事を待つ" \
  --box waiting --assignee me \
  --meta who=レビュアー --meta follow_up=2026-12-01 \
  --meta since=2026-11-20 --meta source=https://example.invalid/pr/1 \
  --body-file notes.md --by importer
```

- 箱・担当・planning の欄・本文を一度に渡し、1 つのトランザクションで作る。
  どれかが誤っていれば、項目を作らずに失敗する。
- `--box` を省くと `inbox`、`--assignee` を省くと `me`。
- `--meta KEY=VALUE` は欄ごとに 1 回。`links` だけは繰り返すと 1 行ずつ足す。
  `who`・`follow_up`・`then`・`watch` は `--box waiting` のときだけ、`outcome`・`short` は `--box projects` のときだけ受け付ける。
- `since` は箱に入った日（YYYY-MM-DD）。`inbox` 以外に作るときに省くと今日になる。
  連絡待ちを始めた日など、今日でない日を残すときに渡す。`move` で箱を変えると、その日に書き換わる。
- `source` は項目の出どころ（チャットのスレッド、PR、ほかの仕組みでの id など）。URL か短い文で、どの箱でも使える。

## 本文の渡し方

長い Markdown は、引数ではなくファイルか標準入力で渡す。シェルの引用や長さの上限で中身が変わらないようにするため。

| 書き方 | 使えるコマンド |
| --- | --- |
| `--body-file PATH` | `add`、`edit-body`、`step-add`、`reply` |
| `--body -`（標準入力） | `add`、`edit-body`、`step-add`、`reply` |
| 位置の `-`（標準入力） | `edit-body ID REV -`、`reply ID REV --as agent -` |
| 引数の文字列 | 今までどおり |

本文は 32000 バイトまでの UTF-8。どの渡し方でも同じに入る（改行の CRLF は LF にそろえる）。
1 つのコマンドに本文の渡し方は 1 つだけ。`edit-body` と `reply` では、`-` だけの本文は引数で渡せない。

## 直す

```sh
lazygtd retitle 12 3 "新しい題名"
lazygtd edit-body 12 4 - < body.md
lazygtd move 12 5 next
lazygtd meta 12 6 source https://example.invalid/issue/7
lazygtd assign 12 7 agent
lazygtd instruct 12 8 "再現手順を整理する"
```

`meta ID REV FIELD VALUE` の FIELD は `due` `follow_up` `who` `then` `watch` `outcome` `short` `links` `since` `source`。
空の VALUE で欄を消す。

## 閉じる・開き直す

```sh
lazygtd done 12 9      # 済んだ
lazygtd archive 12 9   # 済まさずに閉じる
lazygtd reopen 12 10   # done と archive のどちらも開き直す
```

- `archive` は項目を消さない。event とステップのつながりは残り、`list` と TUI の箱から外れる。
  `list --all` と `show ID` で読める。物理的に消すコマンドは無い。
- プロジェクトのステップを `done` か `archive` で閉じると、並びの次のステップが「次の行動」に繰り上がる。
  まだ開いているステップのあるプロジェクトは、どちらでも閉じられない。
- `reopen` は閉じたときの箱に戻す。ステップは、プロジェクトが閉じていれば開き直せない（先にプロジェクトを開き直す）。
- 閉じた項目は、`reopen` のほかは書き換えられない（済んだステップをプロジェクトにつなぐ・外すのは除く）。

## 読む

```sh
lazygtd list --json --box next --assignee agent --open
lazygtd list --json --all
lazygtd show 12 --with conversation,steps
```

- `list --json` は、閉じていない（archive していない）項目を ID の順に返す。済んだものは含む。
  - `--box BOX`: その箱だけ（`review` と `desk` は TUI では「次の行動」に出るが、ここでは元の ID で絞る）
  - `--assignee me|agent`: その担当だけ
  - `--done` / `--open`: 済んだものだけ / 済んでいないものだけ
  - `--all`: archive したものも含める
- `show ID --with conversation,steps` は、項目の欄に `conversation` と `steps` の配列を足して返す。
  `--with` を省けば、今までどおり項目だけ。会話は新しいほうから 40 件まで。

## エージェントとして返事を書く

```sh
lazygtd reply 12 11 --as agent --body-file report.md --by researcher
```

- 会話に role `agent`、`run_id` 無し、未読の返事として入る。TUI では項目が「次の行動」に未読の印付きで出て、
  ユーザーが読むと既読になる。
- 次にその項目を lazygtd から実行するとき、この返事は「これまでの会話」としてプロンプトに入る。
- `message ID REV TEXT` はユーザーからの指示で、次の実行に渡す待ちの行に入る。エージェントの返事には使わない。

## 出力の JSON

`add`・`show`・書き込みの結果と、`list --json` の要素は、次の形の項目になる。

```json
{
  "id": 12, "revision": 4, "title": "レビューの返事を待つ",
  "box_id": "waiting", "done": false, "archived": false,
  "assignee": "me", "instructions": "", "body": "…",
  "pending_message": false, "unread_reply": false,
  "planning": {
    "due": null, "calendar_action": false, "since": "2026-11-20",
    "follow_up": "2026-12-01", "who": "レビュアー", "then": null, "watch": null,
    "outcome": null, "short": null, "links": [], "source": "https://example.invalid/pr/1"
  },
  "project_id": null, "agent_review": false, "agent_active": false
}
```

| 欄 | 意味 |
| --- | --- |
| `revision` | 次の書き込みに渡す値。書き込みのたびに上がる |
| `box_id` | `inbox` `next` `projects` `waiting` `someday` `reference`（古い `review` `desk` もある） |
| `done` / `archived` | 済んだ / 済まさずに閉じた |
| `pending_message` | まだ実行に渡していないユーザーの指示がある |
| `unread_reply` | ユーザーが読んでいないエージェントの返事がある |
| `project_id` | ステップなら属するプロジェクトの ID |
| `agent_review` / `agent_active` | 実行の結果がレビュー待ち / 実行中（実行中の項目は書き換えられない） |

`show --with` の `conversation` の要素は `{"id", "role": "user"|"agent", "body", "run_id", "read", "written_at"}`、
`steps` の要素は `{"id", "revision", "title", "state"}` で、`state` は `next` `waiting` `planned` `review` `done` `archived` など。

## 履歴

どの書き込みも event の表に 1 行を残す（kind は `add` `move` `done` `archive` `reopen` `retitle` `edit-body`
`assign` `instruct` `message` `reply` `plan-meta` `step-attach` `step-detach` `step-reorder`）。
ステップの繰り上げなどの規則による変更は、actor `system:planning` で残る。
DB の版は v7。v1〜v6 の DB は開いたときに移行し、古いアプリでは開けなくなる。
