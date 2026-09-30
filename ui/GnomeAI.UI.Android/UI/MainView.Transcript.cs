using System.Text.Json;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.Threading;
using Avalonia.VisualTree;
using Orientation = Avalonia.Layout.Orientation;

namespace GnomeAI.Android.UI;

public sealed partial class MainView
{
    private sealed record HistoryRow(string Role,string Text,Control Bubble,Expander? Thinking,SelectableTextView? ThinkingText);
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
        var dark=ActualThemeVariant==Avalonia.Styling.ThemeVariant.Dark;
        _thinking.SelectionBrush=Ink(dark?"#3F6E8C":"#B9D9F0");
        _thinking.CaretBrush=Ink(dark?"#EAF4EF":"#172B23");
        _thinking.SelectionChanged+=(_,_)=>OnSelectionChanged(_thinking);
        _scroll.BringIntoViewOnFocusChange=false;
        _scroll.ScrollChanged+=(_,e)=>{
            if(_applyingScroll)return;
            if(e.ExtentDelta.Y!=0 || e.ViewportDelta.Y!=0){QueueTranscriptScroll();return;}
            if(e.OffsetDelta.Y!=0){_followEnd=_scroll.Extent.Height-_scroll.Viewport.Height-_scroll.Offset.Y<64;_readingOffset=_scroll.Offset.Y;}
        };
        _messages.LayoutUpdated+=(_,_)=>ApplyTranscriptScroll();
        _live.ContentUpdated+=(_,_)=>QueueTranscriptScroll();
    }
    /// A transcript text block with touch selection and a visible caret. Every
    /// place the transcript shows model or user text uses this, so text behaves
    /// the same everywhere on the phone.
    private SelectableTextView Selectable(string? text,double fontSize=14)
    {
        var dark=ActualThemeVariant==Avalonia.Styling.ThemeVariant.Dark;
        var block=new SelectableTextView {
            Text=text??"",TextWrapping=TextWrapping.Wrap,FontSize=fontSize,
            SelectionBrush=Ink(dark?"#3F6E8C":"#B9D9F0"),
            CaretBrush=Ink(dark?"#EAF4EF":"#172B23"),
        };
        block.SelectionChanged+=(_,_)=>{OnSelectionChanged(block);};
        return block;
    }
    /// A selection anywhere in the conversation raises the floating copy bar in
    /// place, so the user never leaves the chat to copy text.
    private void OnSelectionChanged(SelectableTextView block)
    {
        if(!block.HasSelection) {
            SelectionModeChanged(this,EventArgs.Empty);
            QueueTranscriptScroll();
            RefreshCopyBar();
            return;
        }
        var captured=block;
        _selectionSource=()=>captured.SelectedText;
        SelectionModeChanged(this,EventArgs.Empty);
        RefreshCopyBar();
    }
    /// Selection inside a reply. `MarkdownView` keeps its text in several blocks,
    /// so the whole reply is asked for the union instead of one fragment.
    private void OnReplySelectionChanged(object? sender,EventArgs args)
    {
        var view=sender as MarkdownView;
        if(view is null)return;
        if(view.IsSelectingText) {
            var captured=view;
            _selectionSource=()=>captured.SelectedText;
        }
        SelectionModeChanged(this,EventArgs.Empty);
        RefreshCopyBar();
    }
    /// Shows the bar only while something is selected, and never over the composer.
    private void RefreshCopyBar()
    {
        if(_copyBar is null)return;
        var hasSelection=SelectingResponse();
        _copyBar.IsVisible=hasSelection && _scroll.IsVisible && !_disposed;
        if(!hasSelection)return;
        // Once a selection exists the whole conversation can be copied too, even
        // though only one block is highlighted.
        if(_copyButton is not null)_copyButton.Content="Copy";
        if(_copyAllButton is not null)_copyAllButton.Content="Copy everything";
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

    /// The in-conversation copy bar. It overlays the transcript and only appears
    /// while text is selected, so copying never opens a separate screen.
    private Control BuildCopyBar()
    {
        var row=new StackPanel {Orientation=Orientation.Horizontal,Spacing=8};
        _copyButton=Button("Copy",async()=>{
            var text=_selectionSource?.Invoke()??"";
            if(text.Length==0)return;
            await CopyAsync(text);
            _copyButton!.Content="Copied";
            await Task.Delay(1100);
            if(!_disposed)_copyButton!.Content="Copy";
        });
        _copyAllButton=Button("Copy everything",async()=>{
            var text=ConversationText();
            if(text.Length==0)return;
            await CopyAsync(text);
            _copyAllButton!.Content="Copied";
            await Task.Delay(1100);
            if(!_disposed)_copyAllButton!.Content="Copy everything";
        });
        row.Children.Add(_copyButton);
        row.Children.Add(_copyAllButton);
        row.Children.Add(Button("Done",()=>{EndResponseSelection();return Task.CompletedTask;}));
        _copyBar=new Border {
            Child=row,IsVisible=false,
            Background=Ink("#1B282E"),BorderBrush=Ink("#31433B"),BorderThickness=new Thickness(1),
            CornerRadius=new CornerRadius(18),Padding=new Thickness(10,8),
            HorizontalAlignment=HorizontalAlignment.Center,VerticalAlignment=VerticalAlignment.Bottom,
            Margin=new Thickness(16,0,16,10),
        };
        return _copyBar;
    }
    /// Everything said in the conversation, in reading order, as plain text.
    /// Reasoning and approval panels are deliberately excluded: they are model
    /// working notes, not the conversation the user is reading.
    private string ConversationText()
    {
        var blocks=new List<string>();
        // Reading order: history, live reasoning, then the reply being streamed.
        foreach(var root in new Control[] {_historyPanel,_reasoning,_live})Collect(root,blocks);
        return blocks.Count==0?"":string.Join("\n\n",blocks);
    }
    /// Depth-first walk, so the copy reads in the same order as the transcript.
    /// A node that owns its own text is collected and not descended into, which
    /// both preserves order and avoids emitting the same reply twice. The roots
    /// themselves are checked because the live reply is a reply, not a container.
    private static void Collect(Visual node,List<string> blocks)
    {
        if(node is MarkdownView reply) {
            // Replies keep their text in several blocks, so the view is asked for
            // the whole thing rather than the fragments underneath.
            if(reply.IsEffectivelyVisible && reply.PlainText.Length>0)blocks.Add(reply.PlainText);
            return;
        }
        if(node is SelectableTextView text) {
            // Only what the user can see: a collapsed "Thinking" expander must not
            // leak its text into the copied conversation.
            if(text.IsEffectivelyVisible && !string.IsNullOrEmpty(text.Text))blocks.Add(text.Text!);
            return;
        }
        foreach(var child in node.GetVisualChildren()) {
            Collect(child,blocks);
        }
    }
    private async Task CopyAsync(string text)
    {
        if(TopLevel.GetTopLevel(this)?.Clipboard is not {} clipboard)return;
        await clipboard.SetTextAsync(text);
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
        // The blocks that held the selection are gone, so the bar must go with them
        // instead of offering to copy text that no longer exists.
        _selectionSource=null;
        RefreshCopyBar();
    }
    private HistoryRow BuildHistoryRow(string role,string text,string thinking)
    {
        var bubble=new StackPanel {Spacing=6};
        bubble.Children.Add(new TextBlock {Text=role=="user"?"You":"GnomeAI",FontSize=11,Opacity=.6});
        Expander? expander=null;SelectableTextView? thought=null;
        if(role=="assistant") {
            thought=Selectable(thinking,13);
            expander=new Expander {Header="Thinking",Content=thought,IsVisible=thinking.Length>0,IsExpanded=false};bubble.Children.Add(expander);
        }
        if(role=="assistant")bubble.Children.Add(ReplyView(text));
        else {
            var content=GnomeAI.Client.MessageContent.Read(text);
            foreach(var image in content.Images)bubble.Children.Add(new GnomeAI.UI.ConversationPhoto {DataUri=image});
            bubble.Children.Add(Selectable(DisplayUserText(content.Text)));
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
