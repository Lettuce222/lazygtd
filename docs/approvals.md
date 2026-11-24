# Codex app-server review (experimental)

This optional mode is separate from the usual exec/resume adapters. It uses a
private local JSONL pipe, a fresh private Codex home and an ephemeral
read-only thread. It requires `OPENAI_API_KEY` already supplied to the process
through the environment: no copying an existing login profile, borrowing
runtime credentials, or putting keys in lazygtd config. Keep using the normal
adapter if you want the existing Codex login behavior.

```sh
lazygtd --agent-config ./agent.json --codex-review --approved-note /absolute/path/to/nonsensitive-note.txt
```

The note flag is optional. Use the usual local task message and send preview
to start the selected task. The footer indicates a live request. Press `!` to open it, then `y` to
send the complete visible note, `n` or Esc to decline, or `c` to stop
the current turn. The note is never included in the initial task prompt.
Omit the note flag for a deny/cancel-only review.

For a supported server-originated command/file/permission request, the
reviewer may decline the exact request or cancel that exact turn. Neither
choice grants shell execution, filesystem writes, network permission, or
persistent access. Unrecognized, unrelated and duplicate requests never
produce any permission. The request identity is tied to its JSON-RPC ID,
thread, turn and item/call ID. Provider-generated command strings are not
executed by lazygtd. Claude Code's existing print-mode permission behavior
does not change.

The optional `read_approved_note` dynamic tool is narrow, not a filesystem
grant. It takes no arguments. The operator selects an absolute local UTF-8
file (maximum 8192 bytes), then reviews the exact frozen contents and
destination before choosing to send it. After approval, only the frozen
snapshot is passed to Codex as a single tool response in the active turn.
The file is not reopened or looked up on a provider request. A second
attempt, a different tool, nonempty arguments or a different run is denied.
The review deadline is at most 180 seconds; the host must stop waiting then.
The provider can receive and retain the approved text and could include it
in its reply, even though the local tool session is ephemeral. Use only
nonsensitive material you are allowed to send.

The adapter disables workspace-shell tools and requests no MCP, search,
plugins or hooks. It does not implement an operating-system sandbox of its
own; use only the trusted official CLI. System-managed provider policies
can still apply. If the installed provider does not accept the documented
protocol, the run fails without falling back to a less restricted adapter.

Protocol: [OpenAI Codex app-server](https://learn.chatgpt.com/docs/app-server#approvals)
and [dynamic tool schema](https://github.com/openai/codex/blob/main/codex-rs/app-server-protocol/schema/typescript/v2/DynamicToolCallParams.ts).
