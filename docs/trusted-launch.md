# 信頼する起動

lazygtd のエージェントの起動は 2 つに分かれる。

| | 読むだけの起動（既定） | 信頼する起動 |
| --- | --- | --- |
| 選び方 | 何も書かない（`"mode": "readonly"`） | `"mode": "trusted"` を明示する |
| ユーザーの設定・hooks・MCP・スキル | 読まない | CLI の既定のとおり読む |
| ツール | 読むだけに絞るよう求める | CLI の権限の設定で決まる |
| 環境変数 | 絞って渡す | 既定は絞る。`"inherit_env": true` で引き継ぐ |
| 書く・push する・外に出す操作 | 求めない | CLI の権限の設定とユーザーの hooks で決まる |

## 責任の分け方

- lazygtd は OS の隔離をしない。どちらの起動でも、公式 CLI は起動した人の権限で動く。
- 信頼する起動では、承認が要る操作と外に出す操作の守りは、ユーザーの設定（Claude Code の hooks や権限の設定、Codex の sandbox や config）が持つ。守りを持つ設定でだけ使う。
- lazygtd は、起動の前に信頼する起動であることを毎回はっきり出す。TUI の送信の確認と `agent-preview` に、信頼する起動であること、読む設定、権限、決まりのファイル、環境変数の扱いを出す。
- `--demo` では、どちらの起動も使えない（エージェントの設定を読まない）。

## 設定

```json
{
  "provider": "claude",
  "executable": "/absolute/path/to/official/claude",
  "workspace": "/absolute/path/to/trusted/project",
  "mode": "trusted",
  "permission_mode": "acceptEdits",
  "rules_file": "/absolute/path/to/rules.md"
}
```

- `mode`: `readonly`（既定）か `trusted`。
- `permission_mode`（trusted だけ）: Claude Code では `--permission-mode` に渡す。`default`（既定）、`acceptEdits`、`plan`、`auto`、`dontAsk` から選ぶ。Codex では `--sandbox` に渡し、`read-only` か `workspace-write` から選ぶ。書かなければ Codex の設定のまま。権限の確認をまるごと飛ばすモード（`bypassPermissions`、`danger-full-access`）は選べない。
- `rules_file`（trusted、Claude Code だけ）: エージェントに守らせる決まりのファイル。`--append-system-prompt-file` で渡す。絶対パスの普通のファイルで、持ち主が自分であり、グループやほかの人が書けないこと（`chmod go-w`）を、読み込むときと起動の直前に確かめる。Codex の決まりは `AGENTS.md` か Codex の設定に書く。
- `inherit_env`（trusted だけ）: `true` なら、起動した端末の環境変数をすべて引き継ぐ。hooks が `PATH` やツールの設定を要るときに使う。保存型のセッションでは、API キーの変数だけは今までどおり渡さない。
- `allow_file_reads` は読むだけの起動のための設定で、trusted では使えない。
- 信頼する起動は、[対話確認つきのレビュー](approvals.md)には使えない。

信頼する起動で外す引数は次のとおり。

- Claude Code: `--safe-mode`、`--setting-sources ""`、`--tools`、`--strict-mcp-config`、`--mcp-config`、`--disallowedTools`。`-p --verbose --output-format stream-json` と `--permission-mode` は残す。
- Codex: `--ignore-user-config`、`--disable`、`-c` での上書き、`-a never`、`--sandbox read-only`、プロジェクトの `.codex/config.toml` を断る確かめ。`exec --json --skip-git-repo-check --color never` は残す。

セッションの再開（[agent-sessions.md](agent-sessions.md)）は、どちらの起動でも同じ決まりで使える。
