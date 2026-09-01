Microsoft .NET SDK 8.0.424
==========================

The Debian package builder downloads the official Linux x64 SDK archive from:

https://builds.dotnet.microsoft.com/dotnet/Sdk/8.0.424/dotnet-sdk-8.0.424-linux-x64.tar.gz

Expected SHA-512:

6503fd9f464d5e3a4f43a881d2b74afc6a2c46ceda74d027f1565b7239f4b3ec884857c03c0dcd49eb52f384d5ae1fa5aaf135f0a6aabc5518103aceed643c74

The verified archive is extracted into `/usr/lib/gnomeai-rs/dotnet` in the
generated package. Upstream `LICENSE.txt` and `ThirdPartyNotices.txt` remain
alongside the SDK. GnomeAI-RS configures `DOTNET_ROOT` privately and does not
add a Microsoft APT source or replace a system-wide .NET installation.
