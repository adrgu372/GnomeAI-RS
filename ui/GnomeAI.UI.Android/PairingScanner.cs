using Environment = System.Environment;
using Android.App;
using Android.Content;
using Android.Content.PM;
using Android.OS;
using Android.Views;
using Android.Widget;
using GnomeAI.Client;
using ZXing;
using Camera = global::Android.Hardware.Camera;
using Color = global::Android.Graphics.Color;

namespace GnomeAI.Android;

internal static class PairingScanner
{
    private const int RequestCode=4108;
    private static WeakReference<MainActivity>? _activity;
    private static TaskCompletionSource<string?>? _pending;
    public static void Attach(MainActivity activity)=>_activity=new(activity);
    public static void Detach(MainActivity activity) {if(activity.IsFinishing){_pending?.TrySetResult(null);_pending=null;}}
    public static Task<string?> ScanAsync() {
        if(_pending is not null)throw new IOException("The scanner is already open.");
        if(_activity is null || !_activity.TryGetTarget(out var activity) || activity.IsFinishing)throw new IOException("Open GnomeAI to scan an invitation.");
        var completion=new TaskCompletionSource<string?>(TaskCreationOptions.RunContinuationsAsynchronously);_pending=completion;
        try {
#pragma warning disable CS0618
            activity.StartActivityForResult(new Intent(activity,typeof(PairingScannerActivity)),RequestCode);
#pragma warning restore CS0618
        }catch(Exception error){_pending=null;completion.TrySetException(error);}
        return completion.Task;
    }
    public static bool OnResult(int requestCode,global::Android.App.Result result,Intent? data) {
        if(requestCode!=RequestCode)return false;
        var completion=_pending;_pending=null;
        if(data?.GetStringExtra("error") is {} error)completion?.TrySetException(new IOException(error));
        else completion?.TrySetResult(result==global::Android.App.Result.Ok?data?.GetStringExtra("pairing_code"):null);
        return true;
    }
}

/// Native preview and local QR decoding. Images remain in memory and are never uploaded.
#pragma warning disable CS0618
[Activity(Label="Scan GnomeAI invitation",Theme="@style/GnomeAITheme",Exported=false,ScreenOrientation=ScreenOrientation.Portrait)]
public sealed class PairingScannerActivity : Activity, ISurfaceHolderCallback, Camera.IAutoFocusCallback
{
    private SurfaceView? _preview;
    private TextView? _status, _details;
    private Button? _capture;
    private Camera? _camera;
    private PreviewFrames? _frames;
    private PictureFrame? _picture;
    private bool _surfaceReady, _resumed, _done, _canFocus;
    private bool _awaitingFrame, _captureRequested, _takingPicture, _awaitingPicture;
    private int _width, _height, _decoding, _generation, _previewRotation, _frameCount, _checks, _points;
    private long _requestedAt, _nextScanAt, _pictureStartedAt, _statusUntil;
    private string _lastError="none", _lastInput="none";
    private System.Threading.Timer? _watchdog;

    // Each registration carries a generation. Do not compare managed Camera wrappers
    // returned through JNI; a callback from a closed camera must not touch a new one.
    private sealed class PreviewFrames : Java.Lang.Object, Camera.IPreviewCallback
    {
        private readonly PairingScannerActivity _owner;
        private readonly int _generation;
        public PreviewFrames(PairingScannerActivity owner,int generation){_owner=owner;_generation=generation;}
        public void OnPreviewFrame(byte[]? data,Camera? camera)=>_owner.PreviewFrame(data,_generation);
    }
    private sealed class PictureFrame : Java.Lang.Object, Camera.IPictureCallback
    {
        private readonly PairingScannerActivity _owner;
        private readonly int _generation;
        public PictureFrame(PairingScannerActivity owner,int generation){_owner=owner;_generation=generation;}
        public void OnPictureTaken(byte[]? data,Camera? camera)=>_owner.PictureTaken(data,_generation);
    }

    protected override void OnCreate(Bundle? state)
    {
        base.OnCreate(state);Window?.AddFlags(WindowManagerFlags.KeepScreenOn);
        var root=new LinearLayout(this){Orientation=Orientation.Vertical};root.SetBackgroundColor(Color.Rgb(16,25,30));
        var hint=new TextView(this){Text="Scan the invitation in GnomeAI → Devices",TextSize=18,Gravity=GravityFlags.Center};
        hint.SetTextColor(Color.White);hint.SetPadding(20,20,20,12);root.AddView(hint);
        _status=new TextView(this){Text="Waiting for the camera…",TextSize=14,Gravity=GravityFlags.Center};
        _status.SetTextColor(Color.White);_status.SetPadding(16,0,16,12);root.AddView(_status);
        var frame=new FrameLayout(this);_preview=new SurfaceView(this);
        frame.AddView(_preview,new FrameLayout.LayoutParams(-1,-1,GravityFlags.Center));
        root.AddView(frame,new LinearLayout.LayoutParams(-1,0,1));
        _preview.Click+=(_,_)=>FocusCamera();
        var actions=new LinearLayout(this){Orientation=Orientation.Horizontal};
        var focus=new Button(this){Text="Focus"};focus.Click+=(_,_)=>FocusCamera();
        _capture=new Button(this){Text="Scan sharp capture"};_capture.Click+=(_,_)=>RequestCapture();
        actions.AddView(focus,new LinearLayout.LayoutParams(0,-2,1));actions.AddView(_capture,new LinearLayout.LayoutParams(0,-2,2));root.AddView(actions);
        var diagnostics=new Button(this){Text="Scanner details · preview7"};
        _details=new TextView(this){TextSize=12,Visibility=ViewStates.Gone};_details.SetTextColor(Color.White);_details.SetPadding(16,0,16,8);
        diagnostics.Click+=(_,_)=>{_details.Visibility=_details.Visibility==ViewStates.Visible?ViewStates.Gone:ViewStates.Visible;UpdateDetails();};
        root.AddView(diagnostics);root.AddView(_details);
        var cancel=new Button(this){Text="Cancel · paste a pairing code instead"};cancel.Click+=(_,_)=>Finish();root.AddView(cancel);
        root.SetFitsSystemWindows(true);SetContentView(root);_preview.Holder!.AddCallback(this);
        // CAMERA is requested only after the user explicitly opens Scan QR.
        if(CheckSelfPermission(global::Android.Manifest.Permission.Camera)!=Permission.Granted)
            RequestPermissions(new[]{global::Android.Manifest.Permission.Camera},4109);
    }
    protected override void OnResume(){base.OnResume();_resumed=true;OpenCamera();}
    protected override void OnPause(){_resumed=false;CloseCamera();base.OnPause();}
    public override void OnRequestPermissionsResult(int requestCode,string[] permissions,Permission[] grantResults)
    {
        base.OnRequestPermissionsResult(requestCode,permissions,grantResults);
        if(requestCode!=4109)return;
        if(grantResults.Length>0 && grantResults[0]==Permission.Granted)OpenCamera();
        else Fail("Camera permission was denied. Use Paste pairing code in Devices.");
    }
    public void SurfaceCreated(ISurfaceHolder holder){_surfaceReady=true;OpenCamera();}
    public void SurfaceChanged(ISurfaceHolder holder,global::Android.Graphics.Format format,int width,int height)=>FitPreview();
    public void SurfaceDestroyed(ISurfaceHolder holder){_surfaceReady=false;CloseCamera();}
    private bool IsCurrent(int generation)=>_resumed && !_done && _camera is not null && generation==_generation;
    private void Post(int generation,Action action)
    {
        try{RunOnUiThread(()=>{if(IsCurrent(generation))action();});}catch(ObjectDisposedException){}
    }
    private void Status(string text,int holdMilliseconds=0)
    {
        _status!.Text=text;_statusUntil=Environment.TickCount64+holdMilliseconds;
    }
    private void Error(Exception error,string stage)
    {
        // Never record payloads, frame contents, or exception messages that could contain an invitation.
        _lastError=stage+": "+error.GetType().Name;UpdateDetails();
    }
    private void UpdateDetails()
    {
        if(_details is null || _details.Visibility!=ViewStates.Visible)return;
        _details.Text=$"preview7 · {_width}×{_height} · rotation {_previewRotation}°\nFrames: {_frameCount} · checks: {_checks} · finder points: {_points}\nInput: {_lastInput} · last error: {_lastError}";
    }
    private void OpenCamera()
    {
        if(!_surfaceReady || !_resumed || _camera is not null || _done || CheckSelfPermission(global::Android.Manifest.Permission.Camera)!=Permission.Granted)return;
        try
        {
            var index=-1;using var info=new Camera.CameraInfo();
            for(var i=0;i<Camera.NumberOfCameras;i++){Camera.GetCameraInfo(i,info);if(info.Facing==global::Android.Hardware.CameraFacing.Back){index=i;break;}}
            if(index<0)throw new IOException("No rear camera is available.");
            _camera=Camera.Open(index)??throw new IOException("The camera is unavailable.");
            using var parameters=_camera.GetParameters()!;
            var size=parameters.SupportedPreviewSizes!.Where(s=>s.Width<=1920 && s.Height<=1080).OrderByDescending(s=>s.Width*s.Height).FirstOrDefault()
                ??parameters.SupportedPreviewSizes!.OrderBy(s=>s.Width*s.Height).First();
            parameters.SetPreviewSize(size.Width,size.Height);parameters.PreviewFormat=global::Android.Graphics.ImageFormatType.Nv21;
            var picture=parameters.SupportedPictureSizes?.Where(s=>(long)s.Width*s.Height<=6_000_000).OrderByDescending(s=>s.Width*s.Height).FirstOrDefault()
                ??parameters.SupportedPictureSizes?.OrderBy(s=>s.Width*s.Height).FirstOrDefault();
            if(picture is not null)parameters.SetPictureSize(picture.Width,picture.Height);
            parameters.PictureFormat=global::Android.Graphics.ImageFormatType.Jpeg;parameters.JpegQuality=95;
            var modes=parameters.SupportedFocusModes;_canFocus=modes?.Contains(Camera.Parameters.FocusModeAuto)==true;
            if(modes?.Contains(Camera.Parameters.FocusModeContinuousPicture)==true)parameters.FocusMode=Camera.Parameters.FocusModeContinuousPicture;
            else if(modes?.Contains(Camera.Parameters.FocusModeContinuousVideo)==true)parameters.FocusMode=Camera.Parameters.FocusModeContinuousVideo;
            else if(_canFocus)parameters.FocusMode=Camera.Parameters.FocusModeAuto;
            _camera.SetParameters(parameters);
            using var actual=_camera.GetParameters()!;
            _width=actual.PreviewSize!.Width;_height=actual.PreviewSize.Height;
            if(actual.PreviewFormat!=global::Android.Graphics.ImageFormatType.Nv21)throw new IOException("NV21 preview is unavailable.");
            var degrees=WindowManager!.DefaultDisplay!.Rotation switch{SurfaceOrientation.Rotation90=>90,SurfaceOrientation.Rotation180=>180,SurfaceOrientation.Rotation270=>270,_=>0};
            _previewRotation=(info.Orientation-degrees+360)%360;_camera.SetDisplayOrientation(_previewRotation);
            _generation++;_frames=new PreviewFrames(this,_generation);
            _camera.SetPreviewDisplay(_preview!.Holder);_camera.StartPreview();FitPreview();
            _capture!.Enabled=true;_nextScanAt=0;Status("Waiting for the first camera image…");ArmFrame();
            if(actual.FocusMode==Camera.Parameters.FocusModeAuto)FocusCamera();
            _watchdog=new System.Threading.Timer(_=>{try{RunOnUiThread(CheckCamera);}catch(ObjectDisposedException){}},null,250,250);
        }
        catch(Exception error){CloseCamera();Fail("Cannot open the camera: "+error.Message);}
    }
    private void FitPreview()
    {
        if(_preview?.Parent is not FrameLayout frame || frame.Width==0 || frame.Height==0 || _width==0)return;
        var ratio=_previewRotation is 90 or 270?(double)_height/_width:(double)_width/_height;
        var width=Math.Min(frame.Width,(int)(frame.Height*ratio));var height=(int)(width/ratio);
        if(_preview.LayoutParameters?.Width==width && _preview.LayoutParameters.Height==height)return;
        _preview.LayoutParameters=new FrameLayout.LayoutParams(width,height,GravityFlags.Center);
    }
    private void ArmFrame()
    {
        if(!IsCurrent(_generation) || _awaitingFrame || _takingPicture || _captureRequested || Volatile.Read(ref _decoding)!=0 || Environment.TickCount64<_nextScanAt)return;
        try
        {
            _awaitingFrame=true;_requestedAt=Environment.TickCount64;
            // One shot avoids NV21 buffer ownership/recycling differences across camera drivers.
            // The next frame is requested only after the current decoder releases its input.
            _camera!.SetOneShotPreviewCallback(_frames);
        }
        catch(Exception error){_awaitingFrame=false;Error(error,"request frame");Status("Camera image unavailable. Try Scan sharp capture.",3000);}
    }
    private void PreviewFrame(byte[]? data,int generation)
    {
        if(!IsCurrent(generation))return;
        _awaitingFrame=false;_frameCount++;
        if(_takingPicture || _captureRequested)return;
        var width=_width;var height=_height;var count=checked(width*height);
        if(data is null || count==0 || data.Length<count)
        {
            _lastError="incomplete preview frame";Status("Camera returned an incomplete image. Try Scan sharp capture.",3000);UpdateDetails();return;
        }
        if(Interlocked.CompareExchange(ref _decoding,1,0)!=0)return;
        try
        {
            var luma=data.AsSpan(0,count).ToArray();_lastInput="preview";_nextScanAt=Environment.TickCount64+250;
            _=DecodeAsync(()=>new RGBLuminanceSource(luma,width,height,RGBLuminanceSource.BitmapFormat.Gray8),generation,false);
        }
        catch(Exception error){Interlocked.Exchange(ref _decoding,0);Error(error,"read frame");Status("Could not read the camera image. Try Scan sharp capture.",3000);}
    }
    private Task DecodeAsync(Func<LuminanceSource> createSource,int generation,bool picture)=>Task.Run(()=>
    {
        var points=0;
        try
        {
            var source=createSource();
            var reader=new BarcodeReaderGeneric{AutoRotate=true,Options=new ZXing.Common.DecodingOptions{TryHarder=true,PossibleFormats=new[]{BarcodeFormat.QR_CODE}}};
            reader.ResultPointFound+=_=>points++;
            var result=reader.Decode(source)??reader.Decode(source.invert());
            // A centered crop removes unrelated high contrast screen/window edges.
            if(result is null && source.Width!=source.Height)
            {
                var side=Math.Min(source.Width,source.Height);
                var crop=source.crop((source.Width-side)/2,(source.Height-side)/2,side,side);
                result=reader.Decode(crop)??reader.Decode(crop.invert());
            }
            var text=result?.Text?.Trim();
            string? rejection=null;
            if(text is not null)
            {
                try{DeviceIdentity.ReadInvitation(text);}
                catch(Exception){rejection=text.StartsWith("gnomeai://pair/",StringComparison.Ordinal)
                    ?"GnomeAI QR read, but the invitation is invalid or expired. Create a new invitation."
                    :"QR read, but it is not a GnomeAI invitation. Open Devices on the other device.";}
            }
            Post(generation,()=>
            {
                _checks++;_points=points;UpdateDetails();
                if(text is not null && rejection is null)
                {
                    _done=true;SetResult(global::Android.App.Result.Ok,new Intent().PutExtra("pairing_code",text));Finish();return;
                }
                if(rejection is not null)Status(rejection,6000);
                else if(picture)Status("Capture checked: no readable QR. Enlarge the QR on the PC and keep its white border visible.",6000);
                else if(Environment.TickCount64>=_statusUntil)Status(points>0
                    ?"Scanning… Pattern found, but the QR is not readable yet. Try Scan sharp capture."
                    :"Scanning camera images… Keep the complete QR visible, or try Scan sharp capture.");
            });
        }
        catch(Exception error){Post(generation,()=>{Error(error,"decode");Status("The QR reader could not process this image. Open Scanner details.",6000);});}
        finally
        {
            Interlocked.Exchange(ref _decoding,0);
            Post(generation,()=>{if(picture)ResumePreview();else ArmFrame();});
        }
    });
    private void RequestCapture()
    {
        if(!IsCurrent(_generation) || _takingPicture || _captureRequested)return;
        _captureRequested=true;_capture!.Enabled=false;Status("Hold steady — reading a sharp capture…",6000);CheckCamera();
    }
    private void TakeCapture()
    {
        _captureRequested=false;_takingPicture=true;_awaitingPicture=true;_awaitingFrame=false;_pictureStartedAt=Environment.TickCount64;
        try
        {
            _camera!.SetOneShotPreviewCallback(null);
            _picture=new PictureFrame(this,_generation);
            // The JPEG path bypasses preview callbacks entirely; no file or external camera app.
            _camera.TakePicture(null,null,_picture);
        }
        catch(Exception error){Error(error,"capture");Status("Could not capture the QR. Try again or paste the invitation.",5000);ResumePreview();}
    }
    private void PictureTaken(byte[]? jpeg,int generation)
    {
        if(!IsCurrent(generation) || !_takingPicture)return;
        _awaitingPicture=false;
        if(jpeg is null || jpeg.Length==0){Status("Camera returned an empty capture. Try again.",5000);ResumePreview();return;}
        if(Interlocked.CompareExchange(ref _decoding,1,0)!=0){ResumePreview();return;}
        _lastInput="JPEG capture";
        _=DecodeAsync(()=>
        {
            using var options=new global::Android.Graphics.BitmapFactory.Options{InJustDecodeBounds=true};
            using(var bounds=global::Android.Graphics.BitmapFactory.DecodeByteArray(jpeg,0,jpeg.Length,options)){}
            if(options.OutWidth<=0 || options.OutHeight<=0)throw new IOException("Invalid JPEG dimensions.");
            options.InJustDecodeBounds=false;options.InSampleSize=1;
            while((long)options.OutWidth*options.OutHeight/options.InSampleSize/options.InSampleSize>6_000_000)options.InSampleSize*=2;
            using var bitmap=global::Android.Graphics.BitmapFactory.DecodeByteArray(jpeg,0,jpeg.Length,options)??throw new IOException("Invalid JPEG.");
            var width=bitmap.Width;var height=bitmap.Height;var pixels=new int[checked(width*height)];
            bitmap.GetPixels(pixels,0,width,0,0,width,height);var luma=new byte[pixels.Length];
            for(var i=0;i<pixels.Length;i++){var p=pixels[i];luma[i]=(byte)((306*((p>>16)&255)+601*((p>>8)&255)+117*(p&255))>>10);}
            return new RGBLuminanceSource(luma,width,height,RGBLuminanceSource.BitmapFormat.Gray8);
        },generation,true);
    }
    private void ResumePreview()
    {
        _takingPicture=false;_awaitingPicture=false;_captureRequested=false;_awaitingFrame=false;
        if(!IsCurrent(_generation))return;
        try{_camera!.StartPreview();_capture!.Enabled=true;ArmFrame();}
        catch(Exception error){Error(error,"resume");Fail("Camera preview could not resume. Reopen Scan QR.");}
    }
    private void CheckCamera()
    {
        if(!IsCurrent(_generation))return;
        UpdateDetails();
        if(_awaitingPicture && Environment.TickCount64-_pictureStartedAt>12000)
        {
            // Release the entire capture session so a late JPEG cannot affect a new request.
            CloseCamera();OpenCamera();Status("The capture timed out; camera reopened. Try again or paste the invitation.",6000);return;
        }
        if(_takingPicture)return;
        if(_captureRequested){if(Volatile.Read(ref _decoding)==0)TakeCapture();return;}
        if(_awaitingFrame && Environment.TickCount64-_requestedAt>5000)
        {
            // A visible SurfaceView alone does not prove the decoder receives frames.
            _lastError="preview callback timeout";
            CloseCamera();OpenCamera();Status("Camera images are not reaching the scanner. Try Scan sharp capture.",6000);return;
        }
        ArmFrame();
    }
    private void FocusCamera()
    {
        if(!IsCurrent(_generation) || _takingPicture || _captureRequested)return;
        if(!_canFocus){Status("This camera focuses automatically. Scanning continues.",2000);return;}
        try{_camera!.CancelAutoFocus();_camera.AutoFocus(this);}
        catch(Java.Lang.RuntimeException error){Error(error,"focus");}
    }
    public void OnAutoFocus(bool success,Camera? camera)
    {
        // Java equality identifies the native object; do not depend on CLR wrapper identity.
        if(camera is null || _camera is null || !camera.Equals(_camera) || !IsCurrent(_generation) || _takingPicture)return;
        if(Environment.TickCount64>=_statusUntil)Status(success?"Focused. Scanning continues…":"Focus could not lock. Move slightly away from the screen.",1500);
        // AutoFocus locks continuous focus until cancelled on Camera1 devices.
        try{_camera.CancelAutoFocus();}catch(Java.Lang.RuntimeException){}
    }
    private void CloseCamera()
    {
        _watchdog?.Dispose();_watchdog=null;_generation++;
        _awaitingFrame=false;_takingPicture=false;_awaitingPicture=false;_captureRequested=false;
        var camera=_camera;_camera=null;
        if(camera is not null)
        {
            try{camera.SetOneShotPreviewCallback(null);camera.StopPreview();}catch(Java.Lang.RuntimeException){}
            finally{camera.Release();camera.Dispose();}
        }
        // Keep Java callback peers alive until the camera has been released.
        _frames=null;_picture=null;
    }
    private void Fail(string error){if(_done)return;_done=true;SetResult(global::Android.App.Result.Canceled,new Intent().PutExtra("error",error));Finish();}
}
#pragma warning restore CS0618
