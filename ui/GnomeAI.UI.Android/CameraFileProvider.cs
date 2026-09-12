using Android.Content;
using AndroidX.Core.Content;

namespace GnomeAI.Android;

[ContentProvider(new[] {"io.github.adrgu372.gnomeai.camera"},
    Name="io.github.adrgu372.gnomeai.CameraFileProvider",Exported=false,GrantUriPermissions=true)]
public sealed class CameraFileProvider : FileProvider { }
