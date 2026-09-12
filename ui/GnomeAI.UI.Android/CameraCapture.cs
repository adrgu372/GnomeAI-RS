using Path = System.IO.Path;
using Android.App;
using Android.Content;
using Android.Graphics;
using Android.Media;
using Android.Provider;
using AndroidX.Core.Content;

namespace GnomeAI.Android;

/// Delegates capture to the installed camera app. Only the single output URI is
/// granted; the agent cannot invoke this service or capture in the background.
internal static class CameraCapture
{
    private const int RequestCode=4107;
    private static WeakReference<MainActivity>? _activity;
    private static TaskCompletionSource<string?>? _pending;
    private static string? _output;
    private static global::Android.Net.Uri? _uri;
    public static void Attach(MainActivity activity)=>_activity=new(activity);
    public static void Detach(MainActivity activity) {
        if(!activity.IsFinishing)return;
        var completion=_pending;Clean(completion);completion?.TrySetResult(null);
    }
    public static async Task<string?> TakePhotoAsync()
    {
        if(_pending is not null)throw new IOException("A camera capture is already open.");
        if(_activity is null || !_activity.TryGetTarget(out var activity) || activity.IsFinishing)
            throw new IOException("Open GnomeAI to take a photo.");
        if(!await CameraPermission.ForPhotoAsync(activity))throw new IOException("Camera permission was denied. Choose an image from the gallery instead.");
        // Permission completion may resume off the UI thread.
        return await Avalonia.Threading.Dispatcher.UIThread.InvokeAsync(()=>StartCapture(activity));
    }
    private static Task<string?> StartCapture(MainActivity activity) {
        if(_pending is not null)throw new IOException("A camera capture is already open.");
        var root=Path.Combine(activity.CacheDir!.AbsolutePath,"camera-captures");Directory.CreateDirectory(root);
        var path=Path.Combine(root,Guid.NewGuid().ToString("N")+".jpg");
        using(File.Create(path)){}
        var completion=new TaskCompletionSource<string?>(TaskCreationOptions.RunContinuationsAsynchronously);
        _pending=completion;_output=path;
        try {
            using var file=new Java.IO.File(path);
            _uri=FileProvider.GetUriForFile(activity,activity.PackageName+".camera",file);
            using var intent=new Intent(MediaStore.ActionImageCapture);
            intent.PutExtra(MediaStore.ExtraOutput,_uri);
            intent.ClipData=ClipData.NewRawUri("Photo",_uri);
            intent.AddFlags(ActivityFlags.GrantReadUriPermission|ActivityFlags.GrantWriteUriPermission);
#pragma warning disable CS0618 // Activity result is forwarded by Avalonia's host activity.
            activity.StartActivityForResult(intent,RequestCode);
#pragma warning restore CS0618
        } catch(ActivityNotFoundException) {
            Clean(completion);completion.TrySetException(new IOException("No camera app is installed. Choose an image from the gallery instead."));
        } catch(Exception error) {Clean(completion);completion.TrySetException(new IOException("Cannot open the camera: "+error.Message,error));}
        return completion.Task;
    }
    public static bool OnResult(MainActivity activity,int requestCode,Result result)
    {
        if(requestCode!=RequestCode)return false;
        _=CompleteAsync(activity,result);return true;
    }
    private static async Task CompleteAsync(MainActivity activity,Result result)
    {
        var completion=_pending;var path=_output;
        if(completion is null || path is null)return;
        string? prepared=null;Exception? failure=null;
        try {
            if(result==Result.Ok) {
                if(!File.Exists(path) || new FileInfo(path).Length==0)throw new IOException("The camera did not save a photo. Try again.");
                prepared=await Task.Run(()=>PreparePhoto(path));
            }
        } catch(Exception error) {failure=new IOException("Cannot read the captured photo: "+error.Message,error);}
        finally {Clean(completion);}
        if(failure is not null)completion.TrySetException(failure);
        else if(!completion.TrySetResult(prepared) && prepared is not null)File.Delete(prepared);
    }
    private static string PreparePhoto(string path)
    {
        using var bounds=new BitmapFactory.Options {InJustDecodeBounds=true};BitmapFactory.DecodeFile(path,bounds);
        if(bounds.OutWidth<=0 || bounds.OutHeight<=0)throw new IOException("Invalid camera image.");
        var sample=1;while(Math.Max(bounds.OutWidth,bounds.OutHeight)/sample>2560)sample*=2;
        using var options=new BitmapFactory.Options {InSampleSize=sample};
        using var bitmap=BitmapFactory.DecodeFile(path,options)??throw new IOException("Image decoding failed.");
        using var exif=new ExifInterface(path);var orientation=exif.GetAttributeInt(ExifInterface.TagOrientation,1);
        using var matrix=new Matrix();
        switch(orientation) {
            case 2:matrix.SetScale(-1,1);break;
            case 3:matrix.SetRotate(180);break;
            case 4:matrix.SetScale(1,-1);break;
            case 5:matrix.SetRotate(90);matrix.PostScale(-1,1);break;
            case 6:matrix.SetRotate(90);break;
            case 7:matrix.SetRotate(-90);matrix.PostScale(-1,1);break;
            case 8:matrix.SetRotate(-90);break;
        }
        Bitmap? rotated=null;var target=path+".ready.jpg";
        try {
            if(orientation is >=2 and <=8)rotated=Bitmap.CreateBitmap(bitmap,0,0,bitmap.Width,bitmap.Height,matrix,true);
            using var output=File.Create(target);
            if(!(rotated??bitmap).Compress(Bitmap.CompressFormat.Jpeg!,90,output))throw new IOException("Could not prepare the photo.");
            output.Flush(true);return target;
        } catch {File.Delete(target);throw;}
        finally {if(rotated is not null && !ReferenceEquals(rotated,bitmap))rotated.Dispose();}
    }
    private static void Clean(TaskCompletionSource<string?>? expected)
    {
        if(!ReferenceEquals(_pending,expected))return;
        try {if(_uri is not null)global::Android.App.Application.Context.RevokeUriPermission(_uri,ActivityFlags.GrantReadUriPermission|ActivityFlags.GrantWriteUriPermission);}catch(Java.Lang.SecurityException){}
        try {if(_output is not null)File.Delete(_output);}catch(IOException){}
        _uri?.Dispose();_uri=null;_output=null;_pending=null;
    }
}
