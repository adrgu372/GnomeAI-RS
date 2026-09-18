using Avalonia;
using Avalonia.Controls;
using Button = Avalonia.Controls.Button;
using Avalonia.Media;
using Avalonia.Media.Imaging;
using Avalonia.Platform.Storage;
namespace GnomeAI.UI;

public sealed class ConversationPhoto : UserControl
{
    public static readonly StyledProperty<string> DataUriProperty = AvaloniaProperty.Register<ConversationPhoto,string>(nameof(DataUri), "");
    public string DataUri { get => GetValue(DataUriProperty); set => SetValue(DataUriProperty,value); }
    private readonly Image _image = new() { MaxWidth=640, MaxHeight=360, Stretch=Stretch.Uniform, HorizontalAlignment=Avalonia.Layout.HorizontalAlignment.Left };
    private readonly Button _save = new() { Content="Save photo…", IsEnabled=false };
    private readonly TextBlock _status = new() { TextWrapping=TextWrapping.Wrap, IsVisible=false };
    private Bitmap? _bitmap;
    static ConversationPhoto() { DataUriProperty.Changed.AddClassHandler<ConversationPhoto>((view,_)=>view.Reload()); }
    public ConversationPhoto()
    {
        var panel = new StackPanel { Spacing=6 };
        panel.Children.Add(_image); panel.Children.Add(_save); panel.Children.Add(_status); Content=panel;
        _save.Click += async (_,_) => await SaveAsync();
    }
    private (byte[] Bytes, string Extension, string Mime) Decode()
    {
        var value=DataUri; var comma=value.IndexOf(',');
        if(comma<0 || value.Length>24*1024*1024) throw new IOException("Invalid photo data.");
        var (ext,mime) = value[..comma] switch {
            "data:image/jpeg;base64" => ("jpg","image/jpeg"),
            "data:image/png;base64" => ("png","image/png"),
            "data:image/webp;base64" => ("webp","image/webp"),
            "data:image/gif;base64" => ("gif","image/gif"),
            _ => throw new IOException("Unsupported photo format.")
        };
        return (Convert.FromBase64String(value[(comma+1)..]),ext,mime);
    }
    private void Reload()
    {
        _image.Source=null; _bitmap?.Dispose(); _bitmap=null; _save.IsEnabled=false; _status.IsVisible=false;
        try {
            var photo=Decode();
            using var stream=new MemoryStream(photo.Bytes);
            _bitmap=Bitmap.DecodeToWidth(stream,640); _image.Source=_bitmap; _save.IsEnabled=true;
        } catch(Exception error) when(error is not OutOfMemoryException) { Status("Photo unavailable: "+error.Message); }
    }
    private void Status(string text) { _status.Text=text; _status.IsVisible=true; }
    private async Task SaveAsync()
    {
        _save.IsEnabled=false;
        try {
            var photo=Decode();
            var storage=TopLevel.GetTopLevel(this)?.StorageProvider;
            if(storage is null || !storage.CanSave) throw new IOException("Saving files is unavailable on this device.");
            using var file=await storage.SaveFilePickerAsync(new FilePickerSaveOptions {
                Title="Save photo", SuggestedFileName=$"GnomeAI-{DateTime.Now:yyyyMMdd-HHmmss}.{photo.Extension}",
                DefaultExtension=photo.Extension,
                FileTypeChoices=new[] { new FilePickerFileType("Photo") { Patterns=new[] { "*."+photo.Extension }, MimeTypes=new[] { photo.Mime } } }
            });
            if(file is null) return;
            await using(var output=await file.OpenWriteAsync()) {
                if(output.CanSeek) output.SetLength(0);
                await output.WriteAsync(photo.Bytes); await output.FlushAsync();
            }
            Status("Photo saved.");
        } catch(Exception error) when(error is not OutOfMemoryException) { Status("Could not save photo: "+error.Message); }
        finally { _save.IsEnabled=_bitmap is not null; }
    }
    protected override void OnAttachedToVisualTree(VisualTreeAttachmentEventArgs e) { base.OnAttachedToVisualTree(e); if(_bitmap is null)Reload(); }
    protected override void OnDetachedFromVisualTree(VisualTreeAttachmentEventArgs e) { _image.Source=null; _bitmap?.Dispose(); _bitmap=null; base.OnDetachedFromVisualTree(e); }
}
