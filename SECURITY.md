# Security

Only the latest published version is supported. Reports should include the
release or commit, platform, and a concise description, without sharing real
task data, API credentials, or access tokens. Use this repository's private
vulnerability report form under **Security** when available. Do not publish
exploit details in open issues.

lazygtd is local by default, but does not encrypt its database. Use a private
filesystem directory and secure backups. Agent dispatch is opt-in and transmits
the confirmed text to the selected provider. Only run against a workspace and
CLI you trust. Demo mode is local and does not invoke a real provider.

Agent launches come in two modes. The default read-only mode requests the
provider's read-only restrictions and skips your hooks, MCP servers and
skills. The trusted mode, chosen only by an explicit `"mode": "trusted"` in
the agent configuration, starts the official CLI with your normal settings.
Trusted launches are in scope for reports about lazygtd itself, for example
a launch that is not shown as trusted before it runs, a rules file that is
accepted although others can write to it, or arguments that differ from the
preview. What a trusted agent may write, push or post is decided by the
CLI's permission settings and your hooks; lazygtd does not isolate the
process, so weaknesses in those settings are outside lazygtd's scope.

Download from GitHub Releases, then verify the archive against SHA256SUMS. For
signed GitHub build provenance, run:

    gh attestation verify ARCHIVE --repo Lettuce222/lazygtd

This checks provenance, not that the application is free of vulnerabilities.
Binaries do not have Windows Authenticode or macOS signing/notarization.
