using Android.Content;
using AndroidX.Core.Content;

namespace GnomeAI.Android;

/// Read-only cache handoff for the Files panel. Only single files copied into
/// the app cache are exposed, and only to the application the user picks.
[ContentProvider(new[] {"io.github.adrgu372.gnomeai.share"},
    Name="io.github.adrgu372.gnomeai.ShareFileProvider",Exported=false,GrantUriPermissions=true)]
public sealed class ShareFileProvider : FileProvider { }
