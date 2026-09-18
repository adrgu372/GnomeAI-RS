using System.Text.Json;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.Threading;

namespace GnomeAI.Android.UI;

public sealed partial class MainView
{
    private sealed record HistoryRow(string Role,string Text,Control Bubble,Expander? Thinking,TextBlock? ThinkingText);
    private readonly List<HistoryRow> _historyRows=[];
    private readonly StackPanel _historyPanel=new(){Spacing=20};
    private readonly StackPanel _approvalPanel=new(){Spacing=12};
    private MobileThinkingStore _thinkingStore=null!;
    private string _historyScope="",_pendingThinking="",_pendingThinkingScope="";
    private int _thinkingStartIndex;
    private bool _followEnd=true,_scrollPending,_applyingScroll;
    private double _readingOffset;
    private string CurrentScope=>(Selected?.Peer.Channel??"local")+"/"+_session;
    private void InitializeTranscript()
    {
        _thinkingStore=new MobileThinkingStore(_home);_reasoning.Header="Thinking";
        _scroll.BringIntoViewOnFocusChange=false;
        _scroll.ScrollChanged+=(_,e)=>{
            if(_applyingScroll)return;
            if(e.ExtentDelta.Y!=0 || e.ViewportDelta.Y!=0){QueueTranscriptScroll();return;}
            if(e.OffsetDelta.Y!=0){_followEnd=_scroll.Extent.Height-_scroll.Viewport.Height-_scroll.Offset.Y<64;_readingOffset=_scroll.Offset.Y;}
        };
        _messages.LayoutUpdated+=(_,_)=>ApplyTranscriptScroll();
        _live.ContentUpdated+=(_,_)=>QueueTranscriptScroll();
        AttachTextSelection(_thinking,()=>_thinking.Text??"");
    }
    private void AttachTextSelection(Control target,Func<string> read)
    {
        Gestures.SetIsHoldingEnabled(target,true);
        target.AddHandler(Gestures.HoldingEvent,(_,e)=>{
            if(e.HoldingState!=HoldingState.Started || string.IsNullOrEmpty(read()))return;
            e.Handled=true;NativeTextSelection.Show(read(),ActualThemeVariant==Avalonia.Styling.ThemeVariant.Dark,()=>{SelectionModeChanged(this,EventArgs.Empty);QueueTranscriptScroll();});
        },Avalonia.Interactivity.RoutingStrategies.Bubble,true);
    }
    private void QueueTranscriptScroll(bool forceEnd=false)
    {
        if(_disposed)return;
        if(forceEnd)_followEnd=true;
        _scrollPending=true;
        // Background priority runs after pending measure/arrange; LayoutUpdated
        // also reapplies when the final Markdown has acquired its real height.
        Dispatcher.UIThread.Post(ApplyTranscriptScroll,DispatcherPriority.Background);
    }
    private void ApplyTranscriptScroll()
    {
        if(!_scrollPending || _disposed || SelectingResponse())return;
        _scrollPending=false;_applyingScroll=true;
        try {
            var end=Math.Max(0,_scroll.Extent.Height-_scroll.Viewport.Height);
            _scroll.Offset=new Vector(_scroll.Offset.X,_followEnd?end:Math.Min(_readingOffset,end));
        } finally {_applyingScroll=false;}
    }
    private void RememberCompletedThinking()
    {
        _pendingThinking=_thinking.Text??"";_pendingThinkingScope=CurrentScope;
    }
    private void CaptureSnapshotThinking(JsonElement data)
    {
        if(data.GetProperty("busy").GetBoolean())return;
        var text=new System.Text.StringBuilder(_historyScope==CurrentScope?_thinking.Text??"":"");
        var end=data.TryGetProperty("revision",out var revision)?revision.GetInt64():long.MaxValue;
        var epoch=data.TryGetProperty("epoch",out var epochValue)?epochValue.GetString():_epoch;
        foreach(var frame in _bufferedEvents) {
            if(frame.Source!=Selected || frame.Message.GetProperty("epoch").GetString()!=epoch)continue;
            var number=frame.Message.GetProperty("revision").GetInt64();
            if(number>end || (epoch==_epoch && number<=_revision))continue;
            var payload=frame.Message.GetProperty("payload");
            if(payload.GetProperty("event").GetString()!="session_event" || payload.GetProperty("session_id").GetString()!=_session)continue;
            var detail=payload.GetProperty("payload");
            if(detail.GetProperty("event").GetString()=="turn_started")text.Clear();
            if(detail.GetProperty("event").GetString()=="reasoning")text.Append(detail.GetProperty("text").GetString());
        }
        if(text.Length>0){_pendingThinking=text.ToString();_pendingThinkingScope=CurrentScope;}
    }

    private void EnsureTranscriptScaffold()
    {
        if(_messages.Children.Contains(_historyPanel))return;
        _messages.Children.Clear();_historyPanel.Children.Clear();_historyRows.Clear();_historyScope="";
        _messages.Children.Add(_historyPanel);_messages.Children.Add(_reasoning);_messages.Children.Add(_live);_messages.Children.Add(_approvalPanel);
    }
    private HistoryRow BuildHistoryRow(string role,string text,string thinking)
    {
        var bubble=new StackPanel {Spacing=6};
        bubble.Children.Add(new TextBlock {Text=role=="user"?"You":"GnomeAI",FontSize=11,Opacity=.6});
        Expander? expander=null;TextBlock? thought=null;
        if(role=="assistant") {
            thought=new TextBlock {Text=thinking,TextWrapping=TextWrapping.Wrap,FontSize=13};
            var captured=thought;AttachTextSelection(thought,()=>captured.Text??"");
            expander=new Expander {Header="Thinking",Content=thought,IsVisible=thinking.Length>0,IsExpanded=false};bubble.Children.Add(expander);
        }
        if(role=="assistant")bubble.Children.Add(ReplyView(text));
        else {
            var content=GnomeAI.Client.MessageContent.Read(text);
            foreach(var image in content.Images)bubble.Children.Add(new GnomeAI.UI.ConversationPhoto {DataUri=image});
            var body=new TextBlock {Text=DisplayUserText(content.Text),TextWrapping=TextWrapping.Wrap};AttachTextSelection(body,()=>content.Text);bubble.Children.Add(body);
        }
        var border=new Border {Child=bubble,Padding=new Thickness(14),CornerRadius=new CornerRadius(14),Background=role=="user"?Ink("#254037"):Brushes.Transparent,HorizontalAlignment=role=="user"?HorizontalAlignment.Right:HorizontalAlignment.Stretch,MaxWidth=960};
        return new(role,text,border,expander,thought);
    }
    private async Task RenderSnapshotAsync(JsonElement data,int generation)
    {
        var scope=CurrentScope;var turns=data.GetProperty("turns").EnumerateArray().ToArray();
        var busy=data.GetProperty("busy").GetBoolean();
        var saved=await _thinkingStore.LoadAsync(scope);
        if(generation!=_viewGeneration || _disposed)return;
        if(!busy && _pendingThinkingScope==scope && _pendingThinking.Length>0) {
            var last=Array.FindLastIndex(turns,t=>t.GetProperty("role").GetString()=="assistant");
            if(last>=_thinkingStartIndex){saved[MobileThinkingStore.TurnKey(last,turns[last].GetProperty("text").GetString()??"")]=_pendingThinking;try { await _thinkingStore.SaveAsync(scope,saved);_pendingThinking=""; }
                catch(IOException) { /* Keep the pending reasoning for a retry; render the answer now. */ }
                catch(UnauthorizedAccessException) { /* Display remains usable if local storage is unavailable. */ }}
        }
        if(generation!=_viewGeneration || _disposed)return;
        EnsureTranscriptScaffold();
        if(_historyScope!=scope){_historyPanel.Children.Clear();_historyRows.Clear();_historyScope=scope;_followEnd=true;}
        var common=0;
        while(common<_historyRows.Count && common<turns.Length && _historyRows[common].Role==turns[common].GetProperty("role").GetString() && _historyRows[common].Text==(turns[common].GetProperty("text").GetString()??""))common++;
        for(var i=_historyRows.Count-1;i>=common;i--){_historyPanel.Children.Remove(_historyRows[i].Bubble);_historyRows.RemoveAt(i);}
        for(var i=0;i<turns.Length;i++) {
            var role=turns[i].GetProperty("role").GetString()??"assistant";var text=turns[i].GetProperty("text").GetString()??"";
            var thought=saved.GetValueOrDefault(MobileThinkingStore.TurnKey(i,text),"");
            if(i>=common){var row=BuildHistoryRow(role,text,thought);_historyRows.Add(row);_historyPanel.Children.Add(row.Bubble);}
            else if(_historyRows[i] is {Thinking:{} expander,ThinkingText:{} body}){body.Text=thought;expander.IsVisible=thought.Length>0;}
        }
        _batchText.Clear();_batchReasoning.Clear();_live.IsStreaming=busy;
        var liveText="";var liveThought="";
        if(data.TryGetProperty("live",out var live) && live.ValueKind==JsonValueKind.Object){liveText=live.GetProperty("text").GetString()??"";liveThought=live.GetProperty("reasoning").GetString()??"";}
        _live.Markdown=liveText;_thinking.Text=liveThought;_reasoning.IsVisible=liveThought.Length>0;
        _running=busy;UpdateSendAppearance();_approvalPanel.Children.Clear();
        if(data.TryGetProperty("approvals",out var approvals))foreach(var approval in approvals.EnumerateArray())ShowApproval(approval,_session,Selected);
        QueueTranscriptScroll();
    }
}
