# Bundled Firecrawl self-host support

GnomeAI-RS starts a rootless local Firecrawl deployment only after Web Search
is enabled and a web request is made. The deployment launcher is
`scripts/gnomeai-firecrawl`; it uses official public container images and
binds the API only to `127.0.0.1:3002`.

Upstream source:

- Project: <https://github.com/firecrawl/firecrawl>
- Version: `v2.11.134`
- Commit: `4f8c82f0762ccd9614ef45de80c74457b21b24f8`
- License: GNU Affero General Public License 3.0
- Source archive: `firecrawl-v2.11.134.tar.gz`
- Source SHA-256:
  `62c3fd48e01c766b17eb437eb53c3edef4f17eba277c61c4f9984658aaaeeee5`

The source archive is an unmodified snapshot of the upstream tag. GnomeAI-RS
does not modify Firecrawl itself; its own launcher and integration code remain
part of the GnomeAI-RS source tree.

The first Web Search use downloads the pinned images into the current user's
rootless Podman storage. Image layers are not embedded in the `.deb`, because
doing so would add several gigabytes to the package. No privileged daemon and
no `sudo` are used at application runtime.
