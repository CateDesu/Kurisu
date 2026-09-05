# Privacy Policy

Kurisu collects nothing. There is no telemetry, no analytics, no crash reporting, and no data is ever sent to the program's author.

## What the program talks to

- **AniList** - your OAuth token and list data go to anilist.co to sync your library. Governed by AniList's own privacy policy.
- **Discord** - if Rich Presence is enabled, the program hands your current show and episode to your local Discord client over its IPC socket. Nothing goes to Discord directly from the program; your client broadcasts the presence under your own Discord settings and Discord's privacy policy.
- **GitHub** - the update checker makes a plain HTTPS request to this repository's releases. GitHub sees the request like any website visit.
- **Torrent feeds** - enabled feeds are fetched over HTTPS from their respective sites, and the torrent search sends your search text to nyaa.si over HTTPS.

## What is stored locally

Your AniList token, list cache, and settings live in a local SQLite database (`kurisu.db`) in the program's config directory. The token is stored in plaintext. None of it leaves your machine except through the connections listed above.

Delete the config directory and everything the program knows about you is gone.
