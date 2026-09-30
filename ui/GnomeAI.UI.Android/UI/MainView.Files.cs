using System.Text;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.Platform.Storage;
using Button = Avalonia.Controls.Button;
using Orientation = Avalonia.Layout.Orientation;

namespace GnomeAI.Android.UI;

/// Local storage of this phone. The agent workspace lives in the application's
/// private directory, which no file manager or system picker can open, so this
/// panel browses it, previews text, exports a copy through the system save
/// picker and hands one file to another application through a read-only
/// content URI.
public sealed partial class MainView
{
    private const int MaxDirectoryEntries=400;
    private const long MaxPreviewBytes=256L*1024;
    private const long MaxShareBytes=64L*1024*1024;
    private static readonly HashSet<string> TextExtensions=new(StringComparer.OrdinalIgnoreCase) {
        ".md",".markdown",".txt",".text",".json",".jsonl",".yaml",".yml",".toml",".ini",".cfg",".conf",".log",".csv",".tsv",
        ".cs",".rs",".py",".js",".mjs",".ts",".tsx",".jsx",".html",".htm",".css",".scss",".xml",".svg",
        ".c",".h",".cpp",".hpp",".java",".kt",".kts",".go",".rb",".php",".sh",".bash",".zsh",".ps1",".sql",".gradle",".csproj",".props",".targets",
    };
    private sealed record StorageRoot(string Label,string Path);
    /// Workspace of the local session, reported by the core's `ready` event.
    private string? _conversationWorkspace;
    private static string DisplaySize(long bytes)=>bytes<1024?$"{bytes} B"
        :bytes<1024*1024?$"{bytes/1024.0:0.0} KB"
        :bytes<1024L*1024*1024?$"{bytes/(1024.0*1024):0.0} MB"
        :$"{bytes/(1024.0*1024*1024):0.0} GB";
    private static bool IsText(string name)=>TextExtensions.Contains(Path.GetExtension(name));
    /// Directories this panel may browse. Every visited path must stay inside
    /// the selected root, so the browser cannot escape into application state.
    private List<StorageRoot> StorageRoots()
    {
        var roots=new List<StorageRoot>();
        foreach(var (label,path) in new[] {
            ("Conversation workspace",_conversationWorkspace??""),
            ("Workspace",Path.Combine(_home,"workspace")),
            ("Moved workspaces",Path.Combine(_home,"workspaces")),
            ("Generated files",Path.Combine(_home,"generated")),
            ("Tool outputs",Path.Combine(_home,"store","tool_outputs")),
            ("Attachments",Path.Combine(_home,"uploads")),
        }) {
            if(path.Length==0)continue;
            string full;
            try {full=Path.GetFullPath(path);} catch(Exception error) when(error is ArgumentException or NotSupportedException or PathTooLongException) {continue;}
            if(!Directory.Exists(full))continue;
            if(roots.Any(root=>string.Equals(root.Path,full,StringComparison.Ordinal)))continue;
            roots.Add(new(label,full));
        }
        if(roots.Count==0) {
            var fallback=Path.Combine(_home,"workspace");
            Directory.CreateDirectory(fallback);roots.Add(new("Workspace",fallback));
        }
        return roots;
    }
    private static string Inside(string root,string candidate)
    {
        var full=Path.GetFullPath(candidate);var basePath=Path.GetFullPath(root).TrimEnd(Path.DirectorySeparatorChar);
        // The root itself is allowed; anything else must be strictly inside it.
        if(!string.Equals(full.TrimEnd(Path.DirectorySeparatorChar),basePath,StringComparison.Ordinal) &&
           !full.StartsWith(basePath+Path.DirectorySeparatorChar,StringComparison.Ordinal))
            throw new IOException("This file is outside the browsable folders.");
        return full;
    }
    private static List<(string Name,bool Directory,long Size,DateTime Modified)> ReadDirectory(string path)
    {
        var folders=new List<(string,bool,long,DateTime)>();var files=new List<(string,bool,long,DateTime)>();
        foreach(var entry in new DirectoryInfo(path).EnumerateFileSystemInfos()) {
            if((entry.Attributes&FileAttributes.ReparsePoint)!=0)continue;
            try {
                if(entry is DirectoryInfo)folders.Add((entry.Name,true,0,entry.LastWriteTime));
                else if(entry is FileInfo file)files.Add((entry.Name,false,file.Length,file.LastWriteTime));
            } catch(IOException){} catch(UnauthorizedAccessException){}
            if(folders.Count+files.Count>=MaxDirectoryEntries*4)break;
        }
        var compare=StringComparer.OrdinalIgnoreCase;
        folders.Sort((a,b)=>compare.Compare(a.Item1,b.Item1));files.Sort((a,b)=>compare.Compare(a.Item1,b.Item1));
        var rows=folders.Concat(files).Take(MaxDirectoryEntries).ToList();
        return rows;
    }
    private Button StorageRow(string icon,string title,string detail,Func<Task> action)
    {
        var row=new Grid {ColumnDefinitions=new ColumnDefinitions("Auto,*"),ColumnSpacing=12};
        row.Children.Add(Symbol(icon));
        var label=new StackPanel {Spacing=3};
        label.Children.Add(new TextBlock {Text=title,TextWrapping=TextWrapping.Wrap,FontWeight=FontWeight.SemiBold});
        label.Children.Add(new TextBlock {Text=detail,FontSize=11,Foreground=Ink("#9FB4A9"),TextWrapping=TextWrapping.Wrap});
        Grid.SetColumn(label,1);row.Children.Add(label);
        var button=Button(title,action);button.Content=row;button.HorizontalContentAlignment=HorizontalAlignment.Stretch;return button;
    }
    /// `root` selects a browsable folder, `relative` a path inside it.
    private Task ShowFilesAsync(string? root=null,string? relative=null)
    {
        ShowPanel();SelectTab("Files");
        var generation=_settingsGeneration;
        bool Current()=>!_disposed && generation==_settingsGeneration;
        var roots=StorageRoots();
        var active=roots.FirstOrDefault(item=>item.Label==root)??roots[0];
        var parts=(relative??"").Split('/',StringSplitOptions.RemoveEmptyEntries);
        var directory=Path.Combine(new[] {active.Path}.Concat(parts).ToArray());
        var inside=Inside(active.Path,directory);
        _panel.Children.Add(new TextBlock {Text="Files",FontSize=24,FontWeight=FontWeight.SemiBold});
        _panel.Children.Add(new TextBlock {
            Text="Everything the agent writes on this phone lives in the app's private storage. Export a copy to a folder you choose, or share one file with another app.",
            TextWrapping=TextWrapping.Wrap
        });
        var feedback=new TextBlock {TextWrapping=TextWrapping.Wrap};
        var chips=new WrapPanel {ItemWidth=190,Orientation=Orientation.Horizontal};
        foreach(var item in roots) {
            var chip=Button((item==active?"• ":"")+item.Label,()=>ShowFilesAsync(item.Label,""));
            chip.FontSize=12;chips.Children.Add(chip);
        }
        _panel.Children.Add(chips);
        _panel.Children.Add(new TextBlock {
            Text=active.Label+(parts.Length==0?Path.DirectorySeparatorChar.ToString():Path.DirectorySeparatorChar+string.Join(Path.DirectorySeparatorChar,parts)+Path.DirectorySeparatorChar),
            FontFamily=FontFamily.Parse("monospace"),FontSize=12,TextWrapping=TextWrapping.Wrap
        });
        var up=Button("Up one folder",()=>ShowFilesAsync(active.Label,string.Join('/',parts.Take(parts.Length-1))));
        up.IsEnabled=parts.Length>0;
        _panel.Children.Add(up);
        if(_hub.HasPendingTransfer)_panel.Children.Add(new TextBlock {Text="A session transfer is pending; workspace files may be frozen for handoff.",TextWrapping=TextWrapping.Wrap});
        if(Selected is not null)_panel.Children.Add(new TextBlock {Text="These folders belong to this phone, not to the paired device selected as source.",TextWrapping=TextWrapping.Wrap,FontSize=12});
        List<(string Name,bool Directory,long Size,DateTime Modified)> entries;
        try {entries=ReadDirectory(inside);}
        catch(Exception error) when(error is IOException or UnauthorizedAccessException or ArgumentException) {
            feedback.Text="Cannot read this folder: "+error.Message;entries=[];
        }
        foreach(var entry in entries) {
            var name=entry.Name;var child=(parts.Length==0?name:string.Join('/',parts.Append(name)));
            var detail=entry.Directory?"Folder":$"{DisplaySize(entry.Size)} · {entry.Modified.ToLocalTime():yyyy-MM-dd HH:mm}";
            Func<Task> open=entry.Directory
                ?()=>ShowFilesAsync(active.Label,child)
                :()=>ShowFileAsync(active,Path.Combine(inside,name));
            _panel.Children.Add(StorageRow(entry.Directory?"folder":"files",name,detail,open));
        }
        if(entries.Count==0 && (feedback.Text??"").Length==0)
            feedback.Text=parts.Length==0?"This folder is empty. Files the agent writes appear here after a turn.":"This folder is empty.";
        else if(entries.Count>=MaxDirectoryEntries)_panel.Children.Add(new TextBlock {Text=$"Only the first {MaxDirectoryEntries} entries are listed in this folder.",TextWrapping=TextWrapping.Wrap,FontSize=12});
        _panel.Children.Add(Button("Refresh",()=>ShowFilesAsync(active.Label,string.Join('/',parts))));
        _panel.Children.Add(feedback);
        if(Current())_status.Text=$"{active.Label} · {entries.Count} entries";
        return Task.CompletedTask;
    }
    private Task ShowFileAsync(StorageRoot root,string path)
    {
        ShowPanel();SelectTab("Files");
        var name=Path.GetFileName(path);
        var info=new FileInfo(path);
        var relative=Path.GetRelativePath(root.Path,path).Replace(Path.DirectorySeparatorChar,'/');
        _panel.Children.Add(new TextBlock {Text=name,FontSize=22,FontWeight=FontWeight.SemiBold,TextWrapping=TextWrapping.Wrap});
        _panel.Children.Add(new TextBlock {Text=$"{root.Label}/{relative}\n{DisplaySize(info.Length)} · {info.LastWriteTime:yyyy-MM-dd HH:mm}",FontSize=12,TextWrapping=TextWrapping.Wrap});
        var feedback=new TextBlock {TextWrapping=TextWrapping.Wrap};
        var segments=relative.Split('/');
        var back=string.Join('/',segments.Take(segments.Length-1));
        _panel.Children.Add(Button("Back to this folder",()=>ShowFilesAsync(root.Label,back)));
        if(IsText(name))_panel.Children.Add(Button("Preview text",()=>PreviewAsync(path,feedback)));
        else _panel.Children.Add(new TextBlock {Text="This file type has no text preview. Export or share it to open it elsewhere.",TextWrapping=TextWrapping.Wrap,FontSize=12});
        _panel.Children.Add(Button("Export a copy…",()=>ExportAsync(path,feedback)));
        _panel.Children.Add(Button("Share with another app…",()=>ShareAsync(path,feedback)));
        _panel.Children.Add(feedback);
        return Task.CompletedTask;
    }
    private async Task PreviewAsync(string path,TextBlock feedback)
    {
        var info=new FileInfo(path);
        if(info.Length>MaxPreviewBytes){feedback.Text=$"Preview is limited to {DisplaySize(MaxPreviewBytes)}. Use Export or Share for this file.";return;}
        string text;
        try {
            var bytes=await File.ReadAllBytesAsync(path);
            text=new UTF8Encoding(false,false).GetString(bytes);
        } catch(Exception error) when(error is IOException or UnauthorizedAccessException) {feedback.Text="Cannot read this file: "+error.Message;return;}
        if(_disposed)return;
        _panel.Children.Add(new TextBox {
            Text=text,IsReadOnly=true,AcceptsReturn=true,TextWrapping=TextWrapping.Wrap,MaxHeight=420,
            FontFamily=FontFamily.Parse("monospace"),FontSize=12
        });
        feedback.Text="Preview only. Nothing was copied outside the app.";
        _status.Text="Previewing "+Path.GetFileName(path);
    }
    private async Task ExportAsync(string path,TextBlock feedback)
    {
        var storage=TopLevel.GetTopLevel(this)?.StorageProvider??throw new IOException("File picker is unavailable.");
        if(!storage.CanSave)throw new IOException("Saving files is unavailable on this device.");
        var name=Path.GetFileName(path);var extension=Path.GetExtension(name);
        feedback.Text="Choose where to save the copy…";
        using var file=await storage.SaveFilePickerAsync(new FilePickerSaveOptions {
            Title="Export file",SuggestedFileName=name,
            DefaultExtension=extension.Length>1?extension[1..]:null,
            FileTypeChoices=[new FilePickerFileType("File") {Patterns=["*"+extension],MimeTypes=[MimeFor(name)]}]
        });
        if(file is null){feedback.Text="Export cancelled.";return;}
        await using(var input=File.OpenRead(path))
        await using(var output=await file.OpenWriteAsync()) {
            if(output.CanSeek)output.SetLength(0);
            await input.CopyToAsync(output);await output.FlushAsync();
        }
        feedback.Text=$"Exported {name} ({DisplaySize(new FileInfo(path).Length)}).";
        _status.Text="Exported "+name;
    }
    private Task ShareAsync(string path,TextBlock feedback)
    {
        var info=new FileInfo(path);
        if(info.Length>MaxShareBytes)throw new IOException($"Sharing is limited to {DisplaySize(MaxShareBytes)}. Export this file instead.");
        var context=global::Android.App.Application.Context;
        if(context?.CacheDir is null)throw new IOException("The share sheet is unavailable on this device.");
        var cache=Path.Combine(context.CacheDir.AbsolutePath,"share");
        Directory.CreateDirectory(cache);
        // One file is handed out at a time; earlier copies are no longer needed.
        foreach(var stale in Directory.EnumerateFiles(cache))try {File.Delete(stale);} catch(IOException){} catch(UnauthorizedAccessException){}
        var copy=Path.Combine(cache,Guid.NewGuid().ToString("N")+"-"+info.Name);
        File.Copy(path,copy,true);
        var authority=context.PackageName+".share";
        using var shared=new Java.IO.File(copy);
        var uri=AndroidX.Core.Content.FileProvider.GetUriForFile(context,authority,shared);
        using var send=new global::Android.Content.Intent(global::Android.Content.Intent.ActionSend);
        send.SetType(MimeFor(info.Name));
        send.PutExtra(global::Android.Content.Intent.ExtraStream,uri);
        send.ClipData=global::Android.Content.ClipData.NewRawUri(info.Name,uri);
        send.AddFlags(global::Android.Content.ActivityFlags.GrantReadUriPermission);
        using var chooser=global::Android.Content.Intent.CreateChooser(send,"Share file")!;
        chooser.AddFlags(global::Android.Content.ActivityFlags.GrantReadUriPermission|global::Android.Content.ActivityFlags.NewTask);
        context.StartActivity(chooser);
        feedback.Text=$"Opened the share sheet for {info.Name}. The copy is removed the next time you share.";
        return Task.CompletedTask;
    }
    private static string MimeFor(string name)=>Path.GetExtension(name).ToLowerInvariant() switch {
        ".md" or ".markdown" or ".txt" or ".text" or ".log" or ".csv" or ".tsv" or ".ini" or ".cfg" or ".conf" =>"text/plain",
        ".json" or ".jsonl" =>"application/json",".xml" =>"application/xml",".yaml" or ".yml" =>"text/yaml",
        ".html" or ".htm" =>"text/html",".svg" =>"image/svg+xml",".pdf" =>"application/pdf",
        ".png" =>"image/png",".jpg" or ".jpeg" =>"image/jpeg",".webp" =>"image/webp",".gif" =>"image/gif",
        ".mp3" =>"audio/mpeg",".m4a" =>"audio/mp4",".wav" =>"audio/wav",".ogg" =>"audio/ogg",
        ".mp4" =>"video/mp4",".webm" =>"video/webm",".zip" =>"application/zip",
        ".apk" =>"application/vnd.android.package-archive",
        ".cs" or ".rs" or ".py" or ".js" or ".mjs" or ".ts" or ".tsx" or ".jsx" or ".css" or ".scss" or ".sh" or ".bash" or ".zsh" or ".sql" or ".toml" or ".c" or ".h" or ".cpp" or ".hpp" or ".java" or ".kt" or ".kts" or ".go" or ".rb" or ".php" =>"text/plain",
        _ =>"application/octet-stream"
    };
}
