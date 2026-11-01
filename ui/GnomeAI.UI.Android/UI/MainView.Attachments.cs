using Avalonia;
using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.Media.Imaging;
using Avalonia.Platform.Storage;

using GnomeAI.Client;

namespace GnomeAI.Android.UI;

public sealed partial class MainView
{
    private readonly Func<Task<string?>>? _capturePhoto;
    private readonly Border _attachmentCard=new() {IsVisible=false};
    private readonly Image _attachmentImage=new() {Width=76,Height=76,Stretch=Stretch.UniformToFill};
    private readonly TextBlock _attachmentName=new() {FontWeight=FontWeight.SemiBold,TextWrapping=TextWrapping.Wrap,MaxLines=2};
    private readonly TextBlock _attachmentInfo=new() {FontSize=11,Opacity=.7,Text="Ready to send · add a question below",TextWrapping=TextWrapping.Wrap};
    /// <summary>
    /// Files staged for the next message. Any type, any number: the picker no
    /// longer stops at one and the core reads each one in order.
    /// </summary>
    private readonly List<string> _attachmentPaths=[];
    private void BuildAttachmentCard()
    {
        var row=new Grid {ColumnDefinitions=new ColumnDefinitions("Auto,*,Auto"),ColumnSpacing=12};
        row.Children.Add(_attachmentImage);
        var label=new StackPanel {Spacing=4,VerticalAlignment=VerticalAlignment.Center};label.Children.Add(_attachmentName);label.Children.Add(_attachmentInfo);
        Grid.SetColumn(label,1);row.Children.Add(label);
        var remove=IconButton("close","Remove attachment",()=>{ClearAttachment();return Task.CompletedTask;});Grid.SetColumn(remove,2);row.Children.Add(remove);
        _attachmentCard.Child=row;_attachmentCard.Padding=new Thickness(10);_attachmentCard.CornerRadius=new CornerRadius(16);
        _attachmentCard.Background=Mobile?Ink("#263C34"):new SolidColorBrush(Color.FromArgb(24,100,150,130));
    }
    private void ClearAttachment()
    {
        if(_attachmentImage.Source is IDisposable image)image.Dispose();_attachmentImage.Source=null;
        var staged=_attachmentPaths.ToArray();_attachmentPaths.Clear();_attachmentCard.IsVisible=false;
        foreach(var path in staged)try{File.Delete(path);}catch(IOException){}catch(UnauthorizedAccessException){}
    }
    private async Task CaptureAsync()
    {
        if(_capturePhoto is null)throw new IOException("Camera capture is available in the Android app.");
        if(_session.Length==0)await NewAsync();
        var session=_session;var source=Selected;
        var path=await _capturePhoto();
        if(path is null)return;
        if(_disposed || session!=_session || source!=Selected){File.Delete(path);return;}
        StageAttachment(path,"Camera photo");
    }
    private async Task AttachAsync()
    {
        // In a messenger conversation, an attachment belongs to that chat and
        // skips the agent's 20 MiB draft limit: files are sealed and chunked by
        // the core, with the account's own tier limit (100 MiB / 2 GiB).
        if(_account.Length>0 && _conversation.Length>0) { await AttachToChatAsync(); return; }
        if(_session.Length==0)await NewAsync();
        var session=_session;var source=Selected;
        var storage=TopLevel.GetTopLevel(this)?.StorageProvider??throw new IOException("File picker is unavailable.");
        // Any type and any number: the core extracts what it can and the rest is
        // still handed to the model as a named file.
        var files=await storage.OpenFilePickerAsync(new FilePickerOpenOptions {Title="Choose files",AllowMultiple=true});
        if(files.Count==0)return;
        var root=Path.Combine(_home,"store","attachment-drafts");Directory.CreateDirectory(root);
        var staged=new List<(string Path,string Name)>();
        try {
            foreach(var file in files) {
                var path=Path.Combine(root,Guid.NewGuid().ToString("N")+Path.GetExtension(file.Name));
                try {
                    await using(var input=await file.OpenReadAsync())
                    await using(var output=File.Create(path)) {
                        var bytes=new byte[65536];long total=0;int count;
                        while((count=await input.ReadAsync(bytes))!=0) {
                            total+=count;if(total>20*1024*1024)throw new IOException($"{file.Name} exceeds 20 MiB.");
                            await output.WriteAsync(bytes.AsMemory(0,count));
                        }
                        if(total==0)throw new IOException($"{file.Name} is empty.");
                    }
                    staged.Add((path,file.Name));
                } catch { try{File.Delete(path);}catch(IOException){}catch(UnauthorizedAccessException){} throw; }
                if(_disposed || session!=_session || source!=Selected){foreach(var item in staged)File.Delete(item.Path);return;}
            }
            StageAttachments(staged);
        } catch { foreach(var item in staged)try{File.Delete(item.Path);}catch(IOException){}catch(UnauthorizedAccessException){} throw; }
    }
    private void StageAttachment(string path,string name) =>
        StageAttachments([(path,name)]);
    private void StageAttachments(IReadOnlyList<(string Path,string Name)> files)
    {
        // A camera photo replaces what is staged; picked files add to it.
        if (files.Count == 1 && _attachmentPaths.Count > 0 && files[0].Name == "Camera photo") ClearAttachment();
        foreach(var file in files)
            if(!_attachmentPaths.Any(staged=>string.Equals(staged,file.Path,StringComparison.Ordinal)))
                _attachmentPaths.Add(file.Path);
        var first=files[0];
        _attachmentName.Text=files.Count==1
            ? first.Name
            : $"{files.Count} files · {string.Join(", ",files.Take(2).Select(file=>file.Name))}"
              + (files.Count>2?"…":"");
        // Decode a bounded thumbnail, not the full camera resolution.
        try {
            using var input=File.OpenRead(first.Path);
            _attachmentImage.Source=Bitmap.DecodeToWidth(input,240);
        }catch(Exception error) when(error is not OutOfMemoryException) {_attachmentImage.Source=null;}
        _attachmentImage.IsVisible=_attachmentImage.Source is not null;
        _attachmentCard.IsVisible=true;ShowTranscript();
        _status.Text=files.Count==1?"Attachment ready. Add a question, then tap Send."
            :$"{files.Count} attachments ready. Add a question, then tap Send.";
    }
    private async Task SubmitAttachmentAsync(string session,string text)
    {
        var drafts=_attachmentPaths.ToArray();
        if(drafts.Length==0)throw new IOException("Attachment is missing.");
        var staged=drafts.ToList();
        if (Selected is { } peer) {
            // The remote device takes one text payload; every file is folded in.
            var parts=new List<string>();
            if(text.Trim().Length>0)parts.Add(text);
            foreach(var draft in drafts)parts.Add(await PhotoMessage.FromFileAsync(draft,""));
            if (Selected != peer || _session != session) throw new IOException("Device selection changed; files were not sent.");
            await _hub.RequestAsync(peer, "submit", new { session_id=session, text=string.Join("\n",parts) });
            if (_attachmentPaths.SequenceEqual(staged)) ClearAttachment();
            return;
        }
        var root=Path.Combine(await _hub.LocalWorkspaceAsync(session),"attachments");Directory.CreateDirectory(root);
        var copies=new List<string>();
        try {
            foreach(var draft in drafts) {
                var path=Path.Combine(root,Guid.NewGuid().ToString("N")+Path.GetExtension(draft));
                File.Copy(draft,path);copies.Add(path);
            }
            if(copies.Count==1)
                await _hub.Bridge.SendAsync(new Dictionary<string,object?> {
                    ["op"]="submit_attachment_to",["session_id"]=session,["path"]=copies[0],["text"]=text });
            else
                await _hub.Bridge.SendAsync(new Dictionary<string,object?> {
                    ["op"]="submit_attachments_to",["session_id"]=session,["paths"]=copies.ToArray(),["text"]=text });
        }catch{foreach(var copy in copies)File.Delete(copy);throw;}
        if(_attachmentPaths.SequenceEqual(staged))ClearAttachment();
    }
}
