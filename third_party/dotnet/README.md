Microsoft .NET SDK 10.0.400
===========================

The Debian package builder downloads the official Linux x64 SDK archive from:

https://builds.dotnet.microsoft.com/dotnet/Sdk/10.0.400/dotnet-sdk-10.0.400-linux-x64.tar.gz

Expected SHA-512:

1033977dd837150e0814cf0c5d5b17ceb63925fda7ba2158b47258a4bd7c048cf82eac3bc1166f3146f53124a3f5fba09db1de1260d2ce96399860303b404b48

The verified archive is extracted into `/usr/lib/gnomeai-rs/dotnet` in the
generated package. Upstream `LICENSE.txt` and `ThirdPartyNotices.txt` remain
alongside the SDK. GnomeAI-RS targets `net10.0`, configures `DOTNET_ROOT`
privately, and does not add a Microsoft APT source or replace a system-wide
.NET installation.
