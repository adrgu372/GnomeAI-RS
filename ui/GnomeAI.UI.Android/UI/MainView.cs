using CheckBox = Avalonia.Controls.CheckBox;
using Button = Avalonia.Controls.Button;
using Orientation = Avalonia.Layout.Orientation;
using System.Text;
using System.Text.Json;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.Platform.Storage;
using Avalonia.Threading;
using Avalonia.VisualTree;

using GnomeAI.Client;

namespace GnomeAI.Android.UI;

/// Android shell. Device protocol and state live in GnomeAI.Client.
public sealed partial class MainView : UserControl, IDisposable
{
    private sealed record Source(string Label, PeerLink? Link) { public override string ToString()=>Label; }
    private readonly DeviceHub _hub;
    private readonly string _home;
    private readonly ComboBox _source=new() { MinWidth=140, HorizontalAlignment=HorizontalAlignment.Stretch };
    private readonly TextBlock _status=new() { Text="Connecting…",TextWrapping=TextWrapping.Wrap,FontSize=12 };
    private readonly TextBox _composer=new() { AcceptsReturn=true,MinLines=1,MaxLines=6,Watermark="Message",TextWrapping=TextWrapping.Wrap };
    private readonly StackPanel _messages=new() { Spacing=12,Margin=new Thickness(12) };
    private readonly ScrollViewer _scroll=new();
    private readonly StackPanel _panel=new() { Spacing=10,Margin=new Thickness(12) };
    private readonly ScrollViewer _panelScroll=new() { IsVisible=false };
    private readonly MarkdownView _live=new();
    private readonly Expander _reasoning=new() {Header="Reasoning"};
    private readonly StackPanel _peerCards=new() {Spacing=12};
    private bool _devicesVisible, _running;
    private PeerLink? _observedPeer;
    private bool _observedOnline;
    private readonly TextBlock _thinking=new() { TextWrapping=TextWrapping.Wrap,Opacity=.65,FontSize=12 };
    private readonly Button _send=new() { Content="Send" };
    private string _session="", _epoch="";
    private long _revision;
    private int _viewGeneration;
    private readonly List<(PeerLink? Source,JsonElement Message)> _bufferedEvents=[];
    private bool _chatsVisible, _openedOnce;
    private bool _refreshing, _refreshAgain, _disposed, _selecting;
    private JsonElement _config;
    private readonly DispatcherTimer _refreshTimer=new() { Interval=TimeSpan.FromMilliseconds(200) };
    private PeerLink? Selected => (_source.SelectedItem as Source)?.Link;
    private readonly Func<Task<string?>>? _scanQr;
    public MainView(DeviceHub hub,string home,Func<Task<string?>>? capturePhoto=null,Func<Task<string?>>? scanQr=null)
    {
        _hub=hub; _home=home;_reasoning.Content=_thinking;_capturePhoto=capturePhoto;BuildAttachmentCard();
        _live.SelectionModeChanged+=SelectionModeChanged;
        _scanQr=scanQr;Content=BuildMobileLayout();InitializeTranscript();
        TopLevel.SetAutoSafeAreaPadding(this,false);
        MobileHost.InsetsChanged+=ApplyWindowInsets;ApplyWindowInsets(MobileHost.Insets,MobileHost.KeyboardVisible);
        _source.SelectionChanged+=async (_,_)=>
        {
            if (_selecting) return;
            EndResponseSelection();
            _observedPeer=Selected;_observedOnline=Selected?.Online??true;
            ClearAttachment();
            _viewGeneration++;_batchText.Clear();_batchReasoning.Clear();_session="";_revision=0;_epoch="";_messages.Children.Clear();
            await Guard(ShowChatsAsync);
        };
        hub.Changed+=HubChanged;hub.SessionEvent+=OnSessionEvent;hub.Bridge.EventReceived+=OnCore;
        _refreshTimer.Tick+=async (_,_)=>{ _refreshTimer.Stop();if(_devicesVisible){RefreshPeerCards();return;}if(_panelScroll.IsVisible && !_chatsVisible)return;await Guard(_chatsVisible?ShowChatsAsync:RefreshCurrentAsync); };
        AttachedToVisualTree+=async (_,_)=>{if(_openedOnce)return;_openedOnce=true;RebuildSources();await Guard(ShowChatsAsync);};
    }
    private Button Button(string text,Func<Task> action)
    {
        var button=new Button {Content=Mobile?new TextBlock {Text=text,TextWrapping=TextWrapping.Wrap}:text,MinHeight=Mobile?48:38};
        if(Mobile)button.HorizontalAlignment=HorizontalAlignment.Stretch;
        button.Click+=async (_,_)=>{button.IsEnabled=false;try{await Guard(action);}finally{if(!_disposed)button.IsEnabled=true;}};return button;
    }
    private async Task Guard(Func<Task> action)
    {
        try { await action(); } catch(Exception error) { if (!_disposed) {_status.Text=error.Message;if(Mobile){_status.TextWrapping=TextWrapping.Wrap;_status.TextTrimming=TextTrimming.None;}} }
    }
    private void RebuildSources()
    {
        var selected=Selected;
        _selecting=true;
        var items=new List<Source> { new(OperatingSystem.IsAndroid()?"This phone":"This PC",null) };
        items.AddRange(_hub.Links.Where(l=>l.Peer.Trusted).Select(l=>new Source(l.Peer.Name+" · "+l.ConnectionLabel,l)));
        _source.ItemsSource=items;_source.SelectedItem=items.FirstOrDefault(i=>i.Link==selected)??items[0];
        _selecting=false;
    }
    private void HubChanged()=>Dispatcher.UIThread.Post(()=>
    {
        if (_disposed) return;
        var previous=Selected;
        RebuildSources();
        if(previous is not null && Selected is null) {
            EndResponseSelection();_viewGeneration++;_session="";_revision=0;_epoch="";
            _batchText.Clear();_batchReasoning.Clear();_messages.Children.Clear();
            _=Guard(ShowChatsAsync);
        }
        _status.Text=Selected is { } link ? link.ConnectionLabel : "Local device";
        var peer=Selected;var online=peer?.Online??true;
        var reconnected=peer is not null && peer==_observedPeer && online && !_observedOnline;
        _observedPeer=peer;_observedOnline=online;
        // Heartbeats update connection labels, not the entire transcript.
        if(_devicesVisible || reconnected)_refreshTimer.Start();
    });
    private Task OnCore(JsonElement message)
    {
        // Transcript/transport traffic is handled by the batched session path.
        var coreKind=message.GetProperty("event").GetString();
        if(coreKind is not ("ui_config" or "ready" or "provider_changed" or "error" or "notice"))return Task.CompletedTask;
        if(!Dispatcher.UIThread.CheckAccess()){Dispatcher.UIThread.Post(()=>{if(!_disposed)_=OnCore(message);});return Task.CompletedTask;}
        var kind=message.GetProperty("event").GetString();
        if(kind=="ui_config") _config=message.Clone();
        if(kind is "ready" or "provider_changed")AcceptModelUpdate(message);
        if(kind=="ready" && Selected is null && _session.Length==0)
        { _session=message.GetProperty("session_id").GetString()!;_refreshTimer.Start(); }
        if(kind is "error" or "notice") _status.Text=message.GetProperty("message").GetString();
        return Task.CompletedTask;
    }
    private readonly object _eventGate=new();
    private readonly Queue<(PeerLink? Source,JsonElement Message,int Generation)> _eventQueue=new();
    private readonly CancellationTokenSource _eventStop=new();
    private bool _pumping;
    private int _eventOverflow;
    private readonly StringBuilder _batchText=new(),_batchReasoning=new();
    private void OnSessionEvent(PeerLink? source,JsonElement message)
    {
        if(Volatile.Read(ref _disposed) || source!=Volatile.Read(ref _observedPeer))return;
        lock(_eventGate) {
            if(_disposed)return;
            if(_eventQueue.Count>=2048){_eventQueue.Clear();Interlocked.Exchange(ref _eventOverflow,1);}
            _eventQueue.Enqueue((source,message,Volatile.Read(ref _viewGeneration)));
            if(_pumping)return;
            _pumping=true;_ = Task.Run(PumpEventsAsync);
        }
    }
    private async Task PumpEventsAsync()
    {
        try {
            while(!_eventStop.IsCancellationRequested) {
                await Task.Delay(40,_eventStop.Token).ConfigureAwait(false);
                var batch=new List<(PeerLink? Source,JsonElement Message,int Generation)>();
                lock(_eventGate)while(batch.Count<256 && _eventQueue.TryDequeue(out var item))batch.Add(item);
                await Dispatcher.UIThread.InvokeAsync(()=>{
                    if(_disposed)return;
                    foreach(var item in batch)if(item.Generation==_viewGeneration)ProcessSessionEvent(item.Source,item.Message);
                    FlushStreamBatch();
                    if(Interlocked.Exchange(ref _eventOverflow,0)!=0){_refreshAgain=true;_refreshTimer.Start();}
                });
                lock(_eventGate)if(_eventQueue.Count==0){_pumping=false;return;}
            }
        }catch(OperationCanceledException) when(_eventStop.IsCancellationRequested){}
        catch(Exception) {
            lock(_eventGate){_eventQueue.Clear();_pumping=false;}
            if(!_disposed)await Dispatcher.UIThread.InvokeAsync(()=>{if(!_disposed){_status.Text="Stream interrupted; refreshing conversation…";_refreshAgain=true;_refreshTimer.Start();}});
        }
        finally{lock(_eventGate){if(_disposed){_eventQueue.Clear();_pumping=false;}}}
    }
    private void FlushStreamBatch()
    {
        var changed=_batchText.Length>0 || _batchReasoning.Length>0;
        if(_batchText.Length>0){_live.Markdown+=_batchText.ToString();_batchText.Clear();}
        if(_batchReasoning.Length>0){_thinking.Text+=_batchReasoning.ToString();_batchReasoning.Clear();_reasoning.IsVisible=true;}
        if(changed)QueueTranscriptScroll();
    }
    private void ProcessSessionEvent(PeerLink? source,JsonElement message)
    {
        if(_disposed || source!=Selected) return;
        if(_refreshing) { if(_bufferedEvents.Count<4096) _bufferedEvents.Add((source,message));else _refreshAgain=true;return; }
        var revision=message.GetProperty("revision").GetInt64();var epoch=message.GetProperty("epoch").GetString()!;
        if(_epoch==epoch && revision<=_revision) return;
        var gap=_epoch!=epoch || (_revision!=0 && revision!=_revision+1);
        _epoch=epoch;_revision=revision;
        var payload=message.GetProperty("payload");
        var kind=payload.GetProperty("event").GetString();
        if(kind=="session_event" && payload.GetProperty("session_id").GetString()==_session)
        {
            var e=payload.GetProperty("payload");var eventKind=e.GetProperty("event").GetString();
            if(eventKind=="token") {_live.IsStreaming=true;_batchText.Append(e.GetProperty("text").GetString());}
            else if(eventKind=="reasoning") {_batchReasoning.Append(e.GetProperty("text").GetString());}
            else if(eventKind=="approval_request")
            {
                ShowApproval(e,_session,Selected);
            }
            else if(eventKind is "turn_started" or "turn_completed" or "interrupted" or "error" or "tool_call_ended") {
                FlushStreamBatch();
                if(eventKind=="turn_started"){_thinkingStartIndex=_historyRows.Count;_live.IsStreaming=true;_thinking.Text="";_reasoning.IsVisible=false;}
                else if(eventKind!="tool_call_ended"){RememberCompletedThinking();_live.IsStreaming=false;}
                _refreshTimer.Start();
            }
            else if(eventKind=="tool_call_started") _status.Text="Using "+e.GetProperty("name").GetString();
        }
        else if(kind=="session_list" && _panelScroll.IsVisible) _refreshTimer.Start();
        if(gap) _refreshTimer.Start();
    }
    private async Task Request(string action,object payload) { await _hub.RequestAsync(Selected,action,payload); }
    private void ShowApproval(JsonElement approval,string sessionId,PeerLink? link)
    {
        var id=approval.GetProperty("call_id").GetString();
        var card=new StackPanel {Spacing=6};
        card.Children.Add(new TextBlock {Text=approval.GetProperty("command").GetString(),TextWrapping=TextWrapping.Wrap});
        async Task Decide(string decision) {
            await _hub.RequestAsync(link,"approve",new {session_id=sessionId,call_id=id,decision});
            card.Children.Clear();card.Children.Add(new TextBlock {Text=decision=="allow"?"Allowed":"Denied"});
        }
        card.Children.Add(Button("Allow this action",()=>Decide("allow")));
        card.Children.Add(Button("Deny",()=>Decide("deny")));
        _approvalPanel.Children.Add(card);
    }
    private void ShowPanel() { _settingsGeneration++; if(_composeSurface is not null)_composeSurface.IsVisible=false; _devicesVisible=false;_chatsVisible=false;_panel.Children.Clear();_panelScroll.IsVisible=true;_scroll.IsVisible=false; }
    private void ShowTranscript() { _settingsGeneration++; if(_composeSurface is not null)_composeSurface.IsVisible=true;SelectTab("Chat"); _devicesVisible=false;_chatsVisible=false;_panelScroll.IsVisible=false;_scroll.IsVisible=true; }
    private async Task ShowChatsAsync()
    {
        var link=Selected;var generation=_viewGeneration;
        _status.Text=link is null?"Loading conversations…":link.Online?"Connected · loading conversations…":link.ConnectionLabel;
        var data=await _hub.RequestAsync(link,"list",new {});
        if(generation!=_viewGeneration || _disposed) return;
        ShowPanel();_chatsVisible=true;SelectTab("Chats");
        _panel.Children.Add(new TextBlock {Text="Conversations",FontSize=28,FontWeight=FontWeight.SemiBold});
        _panel.Children.Add(Button("New conversation",NewAsync));
        foreach(var item in data.GetProperty("sessions").EnumerateArray())
        {
            var id=item.GetProperty("id").GetString()!;
            var title=item.GetProperty("title").ValueKind==JsonValueKind.String?item.GetProperty("title").GetString():"New conversation";
            var state=item.GetProperty("status").GetString();
            var row=new Grid {ColumnDefinitions=new ColumnDefinitions("*,Auto"),ColumnSpacing=8};
            var open=Button(title??"New conversation",()=>OpenAsync(id));open.Content=new TextBlock {Text=title,TextTrimming=TextTrimming.CharacterEllipsis,MaxLines=1};row.Children.Add(open);
            var actions=new StackPanel {Orientation=Orientation.Horizontal,Spacing=6};
            actions.Children.Add(Button("Rename",()=>{
                ShowPanel();var name=new TextBox {Text=title};_panel.Children.Add(name);
                _panel.Children.Add(Button("Save name",async()=>{await _hub.RequestAsync(link,"rename",new {session_id=id,title=name.Text??""});await ShowChatsAsync();}));
                _panel.Children.Add(Button("Cancel",ShowChatsAsync));return Task.CompletedTask;
            }));
            actions.Children.Add(Button("Delete",()=>{
                ShowPanel();_panel.Children.Add(new TextBlock {Text="Delete this conversation and its history?",TextWrapping=TextWrapping.Wrap});
                _panel.Children.Add(Button("Delete conversation",async()=>{await _hub.RequestAsync(link,"delete",new {session_id=id});if(_session==id)_session="";await ShowChatsAsync();}));
                _panel.Children.Add(Button("Cancel",ShowChatsAsync));return Task.CompletedTask;
            }));
            var more=Button("⋯",()=>Task.CompletedTask);more.Flyout=new Flyout {Content=actions};Grid.SetColumn(more,1);row.Children.Add(more);_panel.Children.Add(row);
        }
        if(data.GetProperty("sessions").GetArrayLength()==0) _panel.Children.Add(Mobile?Welcome():new TextBlock {Text="No conversations yet. Start a new conversation."});
        _status.Text=link is null?"Local conversations":(link.Online?"Live · ":"Offline · cached · ")+link.Peer.Name;
    }
    private async Task OpenAsync(string id) {
        EndResponseSelection();
        if(_session!=id)ClearAttachment();_session=id;_viewGeneration++;_followEnd=true;ShowTranscript();
        if (Selected is null) await _hub.Bridge.SendAsync(new Dictionary<string,object?> { ["op"]="resume_session",["id"]=id });
        await RefreshCurrentAsync();
    }
    private void EndResponseSelection() {
        _refreshAgain=false;_refreshTimer.Stop();
        foreach(var view in _messages.GetVisualDescendants().OfType<MarkdownView>().ToArray())view.EndSelection();
        _live.EndSelection();
    }
    private bool SelectingResponse()=>NativeTextSelection.IsOpen || _messages.GetVisualDescendants().OfType<MarkdownView>().Any(view=>view.IsSelectingText);
    private void SelectionModeChanged(object? sender,EventArgs args) {if(_refreshAgain && !SelectingResponse())_refreshTimer.Start();}
    private MarkdownView ReplyView(string text) {
        var view=new MarkdownView {Markdown=text};view.SelectionModeChanged+=SelectionModeChanged;view.ContentUpdated+=(_,_)=>QueueTranscriptScroll();return view;
    }
    private async Task RefreshCurrentAsync()
    {
        if(SelectingResponse()){_refreshAgain=true;return;}
        if(_refreshing) { _refreshAgain=true;return; }
        _refreshing=true;_refreshAgain=false;
        try
        {
            if(_session.Length==0) { await ShowChatsAsync();return; }
            var generation=_viewGeneration;var link=Selected;
            var data=await _hub.RequestAsync(link,"snapshot",new { session_id=_session });
            if(generation!=_viewGeneration || _disposed) return;
            if(SelectingResponse()){_refreshAgain=true;return;}
            CaptureSnapshotThinking(data);
            if(data.TryGetProperty("revision",out var snapshotRevision)) {
                _revision=snapshotRevision.GetInt64();_epoch=data.GetProperty("epoch").GetString()!;
            }
            await RenderSnapshotAsync(data,generation);
            if(generation!=_viewGeneration || _disposed)return;
            _status.Text=(link is null?"":link.ConnectionLabel+(link.Online?" · ":" · cached · "))+(link?.Peer.Name??"Local")+(data.GetProperty("busy").GetBoolean()?" · Running":" · Ready");
            QueueTranscriptScroll();
        }
        finally {
            _refreshing=false;var buffered=_bufferedEvents.ToArray();_bufferedEvents.Clear();
            foreach(var item in buffered) ProcessSessionEvent(item.Source,item.Message);
            FlushStreamBatch();
            if(_refreshAgain) {_refreshAgain=false;_refreshTimer.Start();}
        }
    }
    private async Task NewAsync()
    {
        var data=await _hub.RequestAsync(Selected,"new",new {});await OpenAsync(data.GetProperty("session_id").GetString()!);
    }
    private async Task SendAsync()
    {
        if(_running){await Request("interrupt",new {session_id=_session});return;}
        var text=_composer.Text??"";if(string.IsNullOrWhiteSpace(text) && _attachmentPath is null) return;
        _send.IsEnabled=false;
        try
        {
            if(_session.Length==0) await NewAsync();
            if(_attachmentPath is not null) {await SubmitAttachmentAsync(_session,text);}
            else await _hub.RequestAsync(Selected,"submit",new { session_id=_session,text });
            if(_composer.Text==text) _composer.Text="";
            ShowTranscript();QueueTranscriptScroll(true);await RefreshCurrentAsync();
        }
        finally {_send.IsEnabled=true;}
    }
    private async Task MoveAsync()
    {
        if(_hub.HasPendingTransfer) {await _hub.ResumeMoveAsync();await RefreshCurrentAsync();return;}
        if(_session.Length==0) throw new IOException("Open a conversation first.");
        if(Selected is { } source)
        {
            _status.Text="Moving session to this device…";await _hub.MoveAsync(source,_session,true);
            var id=_session;_source.SelectedIndex=0;await OpenAsync(id);
        }
        else
        {
            ShowPanel();_panel.Children.Add(new TextBlock { Text="Move execution to…" });
            foreach(var link in _hub.Links.Where(l=>l.Online))
                _panel.Children.Add(Button(link.Peer.Name,async()=>{await _hub.MoveAsync(link,_session,false);_status.Text="Session moved to "+link.Peer.Name;await ShowChatsAsync();}));
        }
    }
    public async Task SelectDeviceAsync(PeerLink? link) {
        RebuildSources();_selecting=true;
        _source.SelectedItem=((IEnumerable<Source>)_source.ItemsSource!).First(i=>i.Link==link);_selecting=false;
        EndResponseSelection();ClearAttachment();_session="";_viewGeneration++;await ShowChatsAsync();
    }
    public async Task MoveSessionAsync(string id) {await SelectDeviceAsync(null);await OpenAsync(id);await MoveAsync();}
    public void OpenDevices()=>ShowDevices();
    private Task ShowNavigationAsync() {
        ShowPanel();_panel.Children.Add(new TextBlock {Text="GnomeAI",FontSize=28,FontWeight=FontWeight.SemiBold});
        _panel.Children.Add(Button("＋ New conversation",NewAsync));_panel.Children.Add(Button("Conversations",ShowChatsAsync));
        _panel.Children.Add(Button("Paired devices",()=>{ShowDevices();return Task.CompletedTask;}));
        _panel.Children.Add(Button("AI settings",ShowSettingsAsync));return Task.CompletedTask;
    }
    private Task ShowConversationMenuAsync() {
        ShowPanel();_panel.Children.Add(new TextBlock {Text="Conversation",FontSize=22});
        _panel.Children.Add(Button("Return to conversation",async()=>{ShowTranscript();QueueTranscriptScroll(true);await RefreshCurrentAsync();}));
        _panel.Children.Add(Button("Move execution and files…",MoveAsync));
        _panel.Children.Add(new TextBlock {Text="After a move, workspace files sync automatically while both agents are idle. Conflicts keep both versions.",TextWrapping=TextWrapping.Wrap});
        foreach(var link in _hub.Links.Where(l=>l.Peer.Trusted)) {
            _panel.Children.Add(Button("Sync files with "+link.Peer.Name,async()=>{await _hub.SyncWorkspaceAsync(link,_session);_status.Text=_hub.SyncStatus;}));
            _panel.Children.Add(Button("Pause file sync with "+link.Peer.Name,()=>{_hub.DisableSync(link,_session);_status.Text="File sync paused";return Task.CompletedTask;}));
        }
        return Task.CompletedTask;
    }
    private static string DisplayUserText(string text) {
        var attachment=text.IndexOf("<attached_file name=",StringComparison.Ordinal);
        if(attachment>=0)return text[..attachment].TrimEnd()+"\n📎 Document attached";
        if(text.StartsWith("[",StringComparison.Ordinal))try {
            using var doc=JsonDocument.Parse(text);
            if(doc.RootElement.ValueKind==JsonValueKind.Array && doc.RootElement.EnumerateArray().Any(e=>e.TryGetProperty("type",out var type)&&type.GetString()=="image_url"))
                return string.Join("\n",doc.RootElement.EnumerateArray().Where(e=>e.TryGetProperty("text",out _)).Select(e=>e.GetProperty("text").GetString()))+"\n📎 Image attached";
        }catch(JsonException){}
        return text;
    }
    private void ShowDevices()
    {
        _viewGeneration++;ShowPanel();_devicesVisible=true;SelectTab("Devices");
        _panel.Children.Add(new TextBlock { Text="Your devices",FontSize=26,FontWeight=FontWeight.SemiBold });
        _panel.Children.Add(new TextBlock { Text="Connect phone and desktop over Tor. Create an invitation on either device, open it on the other, then compare and confirm the six digits on both screens.",TextWrapping=TextWrapping.Wrap });
        var relay=new TextBox {Watermark="wss://optional-relay.example"};
        _panel.Children.Add(new Expander {Header="Advanced: use an existing WSS relay",Content=relay});
        var code=new TextBox { AcceptsReturn=true,TextWrapping=TextWrapping.Wrap,Watermark="Paste pairing code",MinHeight=80,MaxHeight=160 };
        var qr=new Image { MaxWidth=280,MaxHeight=280,HorizontalAlignment=HorizontalAlignment.Center };
        _panel.Children.Add(new TextBlock {Text="Pair new device",FontSize=20});
        if(_scanQr is not null)_panel.Children.Add(Button("Scan QR",async()=> {
            var scanned=await _scanQr();if(scanned is null)return;
            _hub.Pair(scanned);_status.Text="Connecting… Compare the six digits on both devices.";
        }));
        _panel.Children.Add(code);_panel.Children.Add(qr);
        var create=Button("Create invitation",async()=> {
            _status.Text="Connecting to Tor… The first connection can take a few minutes.";
            code.Text=await _hub.CreatePairingAsync(string.IsNullOrWhiteSpace(relay.Text)?"tor":relay.Text.Trim());
            using var generator=new QRCoder.QRCodeGenerator();
            using var data=generator.CreateQrCode(code.Text,QRCoder.QRCodeGenerator.ECCLevel.M);
            using var png=new QRCoder.PngByteQRCode(data);
            if(qr.Source is IDisposable previous)previous.Dispose();
            qr.Source=new Avalonia.Media.Imaging.Bitmap(new MemoryStream(png.GetGraphic(4)));
            _status.Text="Invitation expires in 10 minutes. Confirm matching digits on both devices.";
        });
        _panel.Children.Add(create);
        _panel.Children.Add(Button("Copy invitation",async()=>{if(TopLevel.GetTopLevel(this)?.Clipboard is {} clipboard) await clipboard.SetTextAsync(code.Text); }));
        _panel.Children.Add(Button("Connect using invitation",()=>{_hub.Pair(code.Text??"");_status.Text="Connecting… Keep both devices open for confirmation.";return Task.CompletedTask;}));
        if(_hub.HasPendingTransfer) _panel.Children.Add(Button("Pending session transfer · Resume or discard",()=>{ShowTransferRecovery();return Task.CompletedTask;}));
        _panel.Children.Add(_peerCards);RefreshPeerCards();
    }
    private void ShowTransferRecovery(PeerLink? link=null) {
        ShowPanel();_panel.Children.Add(new TextBlock {Text="Pending session transfer",FontSize=24});
        _panel.Children.Add(new TextBlock {Text="A session transfer is still pending. You can resume it, or discard the incomplete transfer and forget its device. A committed or activated transfer cannot be discarded safely.",TextWrapping=TextWrapping.Wrap});
        var error=new TextBlock {TextWrapping=TextWrapping.Wrap};_panel.Children.Add(error);
        var canResume=false;
        try {var pending=_hub.Links.FirstOrDefault(p=>p.Peer.Id==_hub.PendingTransferPeerId);canResume=pending is not null;
            _panel.Children.Add(new TextBlock {Text=pending is null?"The transfer's device no longer exists locally. Use discard to recover this checkpoint.":"Transfer with "+pending.Peer.Name,TextWrapping=TextWrapping.Wrap});
        }catch(Exception e){error.Text=e.Message;}
        var resume=Button("Resume transfer",async()=>{try{await _hub.ResumeMoveAsync();ShowDevices();}catch(Exception e){error.Text=e.Message;}});resume.IsEnabled=canResume;_panel.Children.Add(resume);
        var confirmation=new StackPanel {Spacing=8,IsVisible=false};
        confirmation.Children.Add(new TextBlock {Text="Confirm discarding the incomplete transfer and forgetting its device?",TextWrapping=TextWrapping.Wrap});
        confirmation.Children.Add(Button("Confirm discard and forget",async()=>{try{await _hub.DiscardTransferAndForgetAsync(link);ShowDevices();}catch(Exception e){error.Text=e.Message;}}));
        _panel.Children.Add(Button("Discard transfer and forget device",()=>{confirmation.IsVisible=true;return Task.CompletedTask;}));
        _panel.Children.Add(confirmation);
        _panel.Children.Add(Button("Cancel",()=>{ShowDevices();return Task.CompletedTask;}));
    }
    private void RefreshPeerCards() {
        _peerCards.Children.Clear();
        foreach(var link in _hub.Links) {
            var card=new StackPanel {Spacing=8};
            var expired=!link.Peer.Trusted && link.Peer.Expires<DateTimeOffset.UtcNow.ToUnixTimeSeconds();
            card.Children.Add(new TextBlock {Text=link.Peer.Name+" · "+link.ConnectionLabel,FontWeight=FontWeight.SemiBold,TextWrapping=TextWrapping.Wrap});
            card.Children.Add(new TextBlock {Text=link.Peer.Relay.StartsWith("tor://",StringComparison.Ordinal)?"Tor · encrypted device connection":"WSS · encrypted device connection",FontSize=12,Opacity=.65});
            if(link.LastConnectionError.Length>0)card.Children.Add(new TextBlock {Text=link.LastConnectionError,TextWrapping=TextWrapping.Wrap});
            if(link.AwaitingConfirmation) {
                card.Children.Add(new TextBlock {Text=link.SecurityCode,FontSize=32,FontWeight=FontWeight.SemiBold});
                card.Children.Add(new TextBlock {Text="Compare these digits directly on your other device before confirming.",TextWrapping=TextWrapping.Wrap});
                if(!link.Peer.LocalConfirmed)card.Children.Add(Button("The digits match · Confirm",()=>link.ConfirmAsync()));
                else card.Children.Add(new TextBlock {Text="Waiting for confirmation on the other device…"});
            }
            if(link.Peer.Trusted) {
                card.Children.Add(Button("Open conversations",()=>SelectDeviceAsync(link)));
                card.Children.Add(Button("Merge memories",async()=>{await _hub.MergeMemoriesAsync(link);_status.Text="Memories merged in both directions.";}));
            }
            card.Children.Add(Button(link.Peer.Trusted?"Forget device":"Cancel invitation",()=>{
                if(_hub.HasPendingTransfer){ShowTransferRecovery(link);return Task.CompletedTask;}
                card.Children.Clear();
                card.Children.Add(new TextBlock {Text=$"Forget {link.Peer.Name}? The other device will be notified if connected.",TextWrapping=TextWrapping.Wrap});
                card.Children.Add(Button("Confirm forget",async()=>{await _hub.ForgetAsync(link);RefreshPeerCards();_status.Text=_hub.SyncStatus;}));
                card.Children.Add(Button("Keep device",()=>{RefreshPeerCards();return Task.CompletedTask;}));
                return Task.CompletedTask;
            }));
            _peerCards.Children.Add(new Border {Child=card,Padding=new Thickness(14),CornerRadius=new CornerRadius(12),BorderThickness=new Thickness(1),BorderBrush=new SolidColorBrush(Color.FromArgb(65,128,128,128))});
        }
    }
    public void Dispose()
    {
        MobileHost.InsetsChanged-=ApplyWindowInsets;
        _disposed=true;_eventStop.Cancel();ClearAttachment();_refreshTimer.Stop();_hub.Changed-=HubChanged;_hub.SessionEvent-=OnSessionEvent;_hub.Bridge.EventReceived-=OnCore;
    }
}
