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
    private string? _attachmentPath;
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
        var path=_attachmentPath;_attachmentPath=null;_attachmentCard.IsVisible=false;
        if(path is not null)try{File.Delete(path);}catch(IOException){}catch(UnauthorizedAccessException){}
    }
    private async Task CaptureAsync()
    {
        if(_capturePhoto is null)throw new IOException("Camera capture is available in the Android app.");
        if(Selected is not null)throw new IOException("Switch to This phone to take a photo for a local conversation.");
        if(_session.Length==0)await NewAsync();
        var session=_session;var source=Selected;
        var path=await _capturePhoto();
        if(path is null)return;
        if(_disposed || session!=_session || source!=Selected){File.Delete(path);return;}
        StageAttachment(path,"Camera photo");
    }
    private async Task AttachAsync()
    {
        if(Selected is not null)throw new IOException("Switch to this device to attach a photo or document.");
        if(_session.Length==0)await NewAsync();
        var session=_session;var source=Selected;
        var storage=TopLevel.GetTopLevel(this)?.StorageProvider??throw new IOException("File picker is unavailable.");
        var files=await storage.OpenFilePickerAsync(new FilePickerOpenOptions {Title="Choose image or document",AllowMultiple=false});
        if(files.Count==0)return;
        var root=Path.Combine(_home,"store","attachment-drafts");Directory.CreateDirectory(root);
        var path=Path.Combine(root,Guid.NewGuid().ToString("N")+Path.GetExtension(files[0].Name));
        try {
            await using(var input=await files[0].OpenReadAsync())
            await using(var output=File.Create(path)) {
                var bytes=new byte[65536];long total=0;int count;
                while((count=await input.ReadAsync(bytes))!=0) {
                    total+=count;if(total>20*1024*1024)throw new IOException("Attachment exceeds 20 MiB.");
                    await output.WriteAsync(bytes.AsMemory(0,count));
                }
                if(total==0)throw new IOException("The selected file is empty.");
            }
            if(_disposed || session!=_session || source!=Selected){File.Delete(path);return;}
            StageAttachment(path,files[0].Name);
        }catch{File.Delete(path);throw;}
    }
    private void StageAttachment(string path,string name)
    {
        ClearAttachment();_attachmentPath=path;_attachmentName.Text=name;
        // Decode a bounded thumbnail, not the full camera resolution.
        try {
            using var input=File.OpenRead(path);
            _attachmentImage.Source=Bitmap.DecodeToWidth(input,240);
        }catch(Exception error) when(error is not OutOfMemoryException) {_attachmentImage.Source=null;}
        _attachmentImage.IsVisible=_attachmentImage.Source is not null;
        _attachmentCard.IsVisible=true;ShowTranscript();_status.Text="Attachment ready. Add a question, then tap Send.";
    }
    private async Task SubmitAttachmentAsync(string session,string text)
    {
        var draft=_attachmentPath??throw new IOException("Attachment is missing.");
        var root=Path.Combine(await _hub.LocalWorkspaceAsync(session),"attachments");Directory.CreateDirectory(root);
        var path=Path.Combine(root,Guid.NewGuid().ToString("N")+Path.GetExtension(draft));
        File.Copy(draft,path);
        try {
            await _hub.Bridge.SendAsync(new Dictionary<string,object?> {
                ["op"]="submit_attachment_to",["session_id"]=session,["path"]=path,["text"]=text });
        }catch{File.Delete(path);throw;}
        if(_attachmentPath==draft)ClearAttachment();
    }
}
