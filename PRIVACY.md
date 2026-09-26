# Privacy Policy

Kurisu has no telemetry, analytics, or crash reporting and sends no data to its author.

The program connects to:

- **AniList** for account and list sync using your OAuth token.
- **Your local Discord client**, if Rich Presence is enabled, to share the current show and episode. Your client publishes it under your Discord settings.
- **GitHub** for update checks and downloads. GitHub receives these requests like website visits.
- **Configured torrent feeds** for RSS, and **nyaa.si** for searches, including your search text. Remote feeds and searches use HTTPS.

These services apply their own privacy policies.

Your token, list cache, and settings are stored locally in `kurisu.db`. The token is plaintext. Data leaves the machine only through the connections above. Delete the program's data directory to remove its local database and settings.
