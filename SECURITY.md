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

Download from GitHub Releases, then verify the archive against SHA256SUMS. For
signed GitHub build provenance, run:

    gh attestation verify ARCHIVE --repo Lettuce222/lazygtd

This checks provenance, not that the application is free of vulnerabilities.
Binaries do not have Windows Authenticode or macOS signing/notarization.
