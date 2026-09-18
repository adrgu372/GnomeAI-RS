using Android.Views;
using Android.Widget;
using Color=global::Android.Graphics.Color;

namespace GnomeAI.Android;

internal static class NativeTextSelection
{
    private static WeakReference<MainActivity>? _activity;
    public static bool IsOpen {get;private set;}
    public static void Attach(MainActivity activity)=>_activity=new(activity);
    public static void Detach(MainActivity activity){if(_activity?.TryGetTarget(out var current)==true && current==activity)_activity=null;}
    public static Action? Show(string text,bool dark,Action closed)
    {
        if(IsOpen || _activity?.TryGetTarget(out var activity)!=true || activity is null || activity.IsFinishing)return null;
        var style=dark?global::GnomeAI.UI.Android.Resource.Style.GnomeAISelectionDark:global::GnomeAI.UI.Android.Resource.Style.GnomeAISelectionLight;
        var builder=new global::AndroidX.AppCompat.App.AlertDialog.Builder(activity,style);
        var label=new TextView(builder.Context){Text=text,TextSize=16};
        label.SetTextColor(Color.ParseColor(dark?"#EEF4F0":"#172B23"));
        label.SetBackgroundColor(Color.Transparent);label.SetPadding(24,20,24,32);
        label.SetTextIsSelectable(true);label.SetSingleLine(false);
        var scroll=new ScrollView(builder.Context);scroll.AddView(label);
        builder.SetTitle("Response");builder.SetView(scroll);builder.SetNegativeButton("Close",(_,_)=>{});
        var dialog=builder.Create();
        var finished=false;
        dialog.DismissEvent+=(_,_)=>{if(finished)return;finished=true;IsOpen=false;closed();};
        dialog.Show();IsOpen=true;
        dialog.Window?.SetSoftInputMode(SoftInput.StateAlwaysHidden);
        // Start the standard Android selection toolbar after the text is laid out.
        label.Post(()=>{if(!finished){label.RequestFocus();label.PerformLongClick();}});
        return ()=>{if(!finished)dialog.Dismiss();};
    }
}
