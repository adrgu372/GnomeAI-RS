# Bundled Firecrawl self-host support

GnomeAI-RS starts a rootless local Firecrawl deployment only after Web Search
is enabled and a web request is made. The deployment launcher is
`scripts/gnomeai-firecrawl`; it uses official public container images and
binds the API only to `127.0.0.1:3002`.

Upstream source:

- Project: <https://github.com/firecrawl/firecrawl>
- Version: `v2.11.302`
- Commit: `e3b72346c28224dd8f0673224cf90f692b0c3964`
- License: GNU Affero General Public License 3.0
- Canonical source tag: <https://github.com/firecrawl/firecrawl/tree/v2.11.302>

GnomeAI-RS does not modify Firecrawl itself. `SOURCE.txt` records the exact
upstream tag and commit corresponding to the pinned API build. The previous
v2.11.134 source tarball was removed rather than leaving a stale vendor copy in
the source tree.

The first Web Search use downloads the pinned images into the current user's
rootless Podman storage. Image layers are not embedded in the `.deb`, because
doing so would add several gigabytes to the package. No privileged daemon and
no `sudo` are used at application runtime.
