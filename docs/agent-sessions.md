# 既存の Codex / Claude Code セッションを指定して再開する

通常の設定では、lazygtd からの実行は一時セッションです。再開する場合は、
信頼するプロジェクトに対応した **既知のローカルセッション UUID** を明示します。
名前、直近のセッション、クラウドセッション、履歴ファイルのパスは使いません。

```json
{
  "provider": "codex",
  "executable": "/absolute/path/to/official/codex",
  "workspace": "/absolute/path/to/trusted/project",
  "session_mode": "persist",
  "resume_session": "01892af6-8270-7ced-86c1-929b50dadcb5"
}
```

Claude Code の場合は provider を `claude`、executable を公式 Claude 実行ファイル
に置き換えます。UUID の例も、自分で公式 CLI から確認した値に置き換えます。
既にあるセッションは、最初に選ぶときは UUID とプロジェクトの対応を確認してください。
公式 CLI の既定ローカルプロフィールから、該当するプロバイダーへのログインが必要です。
このモードでは、lazygtd は環境の API キーやカスタムプロフィール先を引き渡しません。
アカウントが変わった場合は、公式 CLI で該当するセッションを確認し直してください。

- `lazygtd --agent-config config.json` で項目の返信欄から進めます。最終確認に
  プロバイダー、実行ファイル、作業場所、再開する UUID、今回の入力が出ます。
- CLI なら `lazygtd agent-preview ITEM config.json` で確認し、
  `lazygtd agent-run ITEM REVISION config.json --send` を明示実行します。
  失敗時に別のセッションで自動再送することはありません。
- lazygtd 内で新しい保存型セッションを作りたい場合は `session_mode` を
  `persist` にし、`resume_session` は指定しません。成功後の `agent-status ITEM` の
  `session_id` を、次回の設定の `resume_session` に設定します。
- この DB に記録済みの UUID は、同じプロバイダー・項目・正規化した作業場所に
  固定されます。外で作った UUID の初回指定は、本人が確認した対応を登録する方式です。
  既存の CLI 履歴の作成者・アカウント・内容を独自に探したり検証したりはしません。
- UUID 自体ではなく **以前の会話やツール出力の内容** に注意してください。プロバイダーの
  再開機能は、新しく入力した部分だけでなく以前の履歴も使います。確認画面は新しい送信文
  を表示しますが、以前の履歴を監査するものではありません。秘密を含む履歴は使わず、
  必要なら新しい一時セッションを選んでください。lazygtd DB には関連付けと
  実行の記録を持ち、公式 CLI 側にも会話履歴が保存されます。

実行は既存の読み取り用途の制限を保ち、Codex には作業場所と read-only 設定を
明示します。ただし公式 CLI、管理設定、既存プロファイル自体を隔離するものでは
ありません。信頼できない作業場所、CLI、過去のセッションでは使わないでください。

参照: [Codex 非対話モード (resume)](https://learn.chatgpt.com/docs/non-interactive-mode#resume-a-non-interactive-session)、
[Claude Code CLI リファレンス (`--resume`)](https://code.claude.com/docs/en/cli-reference)。
