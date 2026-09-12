using Android.Content.PM;

namespace GnomeAI.Android;

// Declaring CAMERA for the integrated scanner also makes Android require a
// grant before ACTION_IMAGE_CAPTURE. Ask only after the explicit photo action.
internal static class CameraPermission
{
    private const int RequestCode=4110;
    private static TaskCompletionSource<bool>? _pending;
    public static Task<bool> ForPhotoAsync(MainActivity activity) {
        if(activity.CheckSelfPermission(global::Android.Manifest.Permission.Camera)==Permission.Granted)return Task.FromResult(true);
        if(_pending is not null)return _pending.Task;
        var completion=new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);_pending=completion;
        activity.RequestPermissions(new[]{global::Android.Manifest.Permission.Camera},RequestCode);return completion.Task;
    }
    public static void OnResult(int code,Permission[] results) {
        if(code!=RequestCode)return;
        var completion=_pending;_pending=null;completion?.TrySetResult(results.Length>0 && results[0]==Permission.Granted);
    }
    public static void Cancel(){_pending?.TrySetResult(false);_pending=null;}
}
